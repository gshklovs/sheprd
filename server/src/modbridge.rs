//! The phone → mod channel and what the mod reports back.
//!
//! The Claude mod (mod/sheprd) cannot receive HTTP, so it long-polls
//! `GET /api/mod/inbox` from every session; the server queues per-session
//! commands there (`steer`, `compact`) and the mod runs them with `$` calls.
//! The mod also reports exact turn records (`/api/mod/turn`), usage gauges
//! (`/api/mod/measure`) and blocking phone questions (`/api/mod/ask`).

use crate::transcript::{cut, now_ms};
use crate::{App, Shared, done_url, herdr, json_err, pane_session, seen_nonce, send_push, suppress_done};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;

/// Longest an inbox poll is held open.
const HOLD_MS: u64 = 25_000;
/// A session counts as connected while it polled this recently (a held poll refreshes it).
const LIVE_MS: i64 = HOLD_MS as i64 + 10_000;
/// Undelivered messages older than this are dropped.
const MSG_TTL_MS: i64 = 120_000;
/// How long `/steer` waits for the mod to say it appended the message.
const STEER_ACK_MS: u64 = 6_000;
/// How long a done push waits for the mod's turn record.
const TURN_WAIT_MS: u64 = 3_000;
/// A phone question nobody answered is dropped after this.
const ASK_TTL_MS: i64 = 2 * 3600_000;
/// Rate-limit windows at or above this % push once per window.
const RATE_ALERT_PCT: f64 = 90.0;

pub struct Bridge {
    inner: Mutex<Inner>,
    /// Bumped on every change a waiting request may care about.
    tick: watch::Sender<u64>,
    /// pane -> lock held while one phone message is being delivered, so messages
    /// to the same agent go out strictly in the order they arrived.
    pane_locks: Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// panes where a message this turn went into Claude's own input queue (typed
    /// via herdr): later messages must queue behind it, not jump ahead by append.
    typed_this_turn: Mutex<HashSet<String>>,
}

impl Default for Bridge {
    fn default() -> Self {
        Bridge {
            inner: Mutex::new(Inner { seq: now_ms() as u64, ..Default::default() }),
            tick: watch::channel(0).0,
            pane_locks: Mutex::new(HashMap::new()),
            typed_this_turn: Mutex::new(HashSet::new()),
        }
    }
}

impl Bridge {
    pub fn pane_lock(&self, pane: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.pane_locks.lock().unwrap().entry(pane.to_string()).or_default().clone()
    }
}

#[derive(Default)]
struct Inner {
    /// Message ids: start at the server's start time (ms) so they keep growing across restarts.
    seq: u64,
    /// session -> queued commands
    queues: HashMap<String, Vec<Msg>>,
    /// session -> last poll
    polls: HashMap<String, Poll>,
    /// message seq -> ack from the mod
    acks: HashMap<u64, Value>,
    /// session -> the last main-loop turn the mod reported
    turns: HashMap<String, TurnRec>,
    /// session -> usage gauges (the `ctx` state field)
    gauges: HashMap<String, Value>,
    /// session -> when the phone asked for a compaction (until the next measure)
    compacting: HashMap<String, i64>,
    /// model id -> context window the mod reported for a session on it (sizes transcript estimates)
    windows: HashMap<String, u64>,
    /// rate-limit window kind -> resetsAt it was pushed for
    rate_alerted: HashMap<String, String>,
    /// ask_phone questions by id
    asks: HashMap<String, PhoneAsk>,
}

#[derive(Clone)]
struct TurnRec {
    ts: i64,
    answer: String,
    duration_ms: i64,
    reason: String,
    answer_i: Option<usize>,
    used: bool,
}

#[derive(Clone)]
struct PhoneAsk {
    session: String,
    pane: Option<String>,
    ts: i64,
    question: String,
    options: Vec<String>,
    answer: Option<String>,
}

struct Msg {
    seq: u64,
    ts: i64,
    delivered: bool,
    body: Value,
}

struct Poll {
    pane: Option<String>,
    ts: i64,
}

impl Bridge {
    fn bump(&self) {
        self.tick.send_modify(|v| *v += 1);
    }

    /// Queues `body` (must carry `kind`) for `session`; returns its seq.
    pub fn enqueue(&self, session: &str, mut body: Value) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        let seq = inner.seq;
        body["seq"] = json!(seq);
        let now = now_ms();
        let q = inner.queues.entry(session.to_string()).or_default();
        q.retain(|m| now - m.ts < MSG_TTL_MS && !m.delivered);
        q.push(Msg { seq, ts: now, delivered: false, body });
        drop(inner);
        self.bump();
        seq
    }

    /// Takes back a message the mod has not picked up; true if it was still queued.
    fn withdraw(&self, session: &str, seq: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(q) = inner.queues.get_mut(session) else { return false };
        let before = q.len();
        q.retain(|m| m.seq != seq || m.delivered);
        q.len() != before
    }

    /// talk.rs: takes back an undelivered message (`withdraw`).
    pub fn withdraw_pub(&self, session: &str, seq: u64) -> bool {
        self.withdraw(session, seq)
    }

    /// talk.rs: waits ≤ `ms` for the mod's ack of message `seq`.
    pub async fn wait_ack(&self, seq: u64, ms: u64) -> Option<Value> {
        self.wait_for(ms, || self.inner.lock().unwrap().acks.get(&seq).cloned()).await
    }

    fn delivered(&self, session: &str, seq: u64) -> bool {
        let inner = self.inner.lock().unwrap();
        // Delivered messages are pruned lazily: absent from the queue = delivered or expired.
        inner.queues.get(session).is_none_or(|q| q.iter().all(|m| m.seq != seq || m.delivered))
    }

    /// The session whose mod polls for `pane` (most recent), else the pane's herdr session.
    pub fn session_for(&self, app: &App, pane: &str) -> Option<String> {
        let inner = self.inner.lock().unwrap();
        let now = now_ms();
        inner
            .polls
            .iter()
            .filter(|(_, p)| p.pane.as_deref() == Some(pane) && now - p.ts < LIVE_MS)
            .max_by_key(|(_, p)| p.ts)
            .map(|(s, _)| s.clone())
            .or_else(|| {
                drop(inner);
                pane_session(app, pane)
            })
    }

    /// True while the mod of `session` is polling.
    pub fn live(&self, session: &str) -> bool {
        self.inner.lock().unwrap().polls.get(session).is_some_and(|p| now_ms() - p.ts < LIVE_MS)
    }

    /// Waits until `f` yields a value or `ms` passes; `f` is re-checked on every bump.
    async fn wait_for<T>(&self, ms: u64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let mut rx = self.tick.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        loop {
            rx.borrow_and_update();
            if let Some(v) = f() {
                return Some(v);
            }
            if tokio::time::timeout_at(deadline, rx.changed()).await.is_err() {
                return f();
            }
        }
    }
}

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/mod/inbox", get(api_inbox))
        .route("/api/mod/send", post(api_send))
        .route("/api/mod/ack", post(api_ack))
        .route("/api/mod/turn", post(api_turn))
        .route("/api/mod/measure", post(api_measure))
        .route("/api/mod/ask", post(api_ask))
        .route("/api/mod/ask/{id}/wait", get(api_ask_wait))
        .route("/api/mod/ask/{id}/cancel", post(api_ask_cancel))
        .route("/api/a/{pane}/steer", post(api_steer))
        .route("/api/a/{pane}/compact", post(api_compact))
        .route("/api/a/{pane}/answer", post(api_answer))
}

/// The pane a mod request is about: its `pane`, else the pane hosting its `session`.
fn pane_of(app: &App, b: &Value) -> Option<String> {
    if let Some(p) = b["pane"].as_str().filter(|p| !p.is_empty()) {
        return Some(p.to_string());
    }
    let s = b["session"].as_str()?;
    app.inner.lock().unwrap().pane_sessions.iter().find(|(_, v)| v.as_str() == s).map(|(p, _)| p.clone())
}

/// A visible client is showing this pane right now (pushes for it are skipped).
fn watching(app: &App, pane: &str) -> bool {
    let inner = app.inner.lock().unwrap();
    matches!(&inner.viewing, Some((Some(p), true, t)) if p == pane && now_ms() - t < 45_000)
}

fn title_of(app: &App, pane: &str) -> String {
    app.inner.lock().unwrap().pane_titles.get(pane).cloned().unwrap_or_else(|| "agent".into())
}

fn is_home(app: &App, pane: &str) -> bool {
    app.inner.lock().unwrap().home.as_ref().is_some_and(|h| h["pane"] == pane)
}

/// Adds what the mod reported to each agent of the published state: `ctx` (usage gauges),
/// `mod` (its inbox is connected), an exact `answerI`, and a pending `ask_phone` question
/// as `ask` (with `phone: true`, status `blocked`).
pub fn decorate(app: &App, agents: &mut [Value]) {
    let b = &app.bridge;
    let now = now_ms();
    let mut inner = b.inner.lock().unwrap();
    inner.asks.retain(|_, a| now - a.ts < ASK_TTL_MS);
    for a in agents.iter_mut() {
        let sid = a["session"].as_str().unwrap_or("").to_string();
        let pane = a["pane"].as_str().unwrap_or("").to_string();
        let gauge = inner.gauges.get(&sid).cloned();
        let mut ctx = pick_ctx(gauge, a["ctx"].take(), &mut inner.windows);
        if let Some(t) = inner.compacting.get(&sid).filter(|t| now - **t < 10 * 60_000) {
            if ctx.is_null() {
                ctx = json!({});
            }
            ctx["compacting"] = json!(t);
        }
        a["ctx"] = ctx;
        a["mod"] = json!(inner.polls.get(&sid).is_some_and(|p| now - p.ts < LIVE_MS));
        if a["status"] != "working" {
            if let Some(i) = inner.turns.get(&sid).and_then(|t| t.answer_i) {
                a["answerI"] = json!(i);
            }
        }
        let ask = inner
            .asks
            .iter()
            .filter(|(_, q)| q.answer.is_none() && (q.session == sid || q.pane.as_deref() == Some(pane.as_str())))
            .max_by_key(|(_, q)| q.ts);
        if let (Some((id, q)), true) = (ask, a["ask"].is_null()) {
            let options: Vec<Value> = q.options.iter().map(|o| json!({"label": o, "description": ""})).collect();
            a["ask"] = json!({"ts": q.ts, "id": id, "phone": true,
                "questions": [{"header": "", "question": q.question, "multiSelect": false, "options": options}]});
            a["status"] = json!("blocked");
            a["chips"] = json!([]);
        }
    }
}

/// A done push about to be sent for `p.pane`: waits briefly for the mod's record of that
/// turn and uses its exact answer, answer index and duration. `None` = send nothing
/// (the turn was aborted, or failed and got its own push, or was short while watched).
pub async fn enrich_done(app: &App, mut p: Value) -> Option<Value> {
    let Some(pane) = p["pane"].as_str().map(str::to_string) else { return Some(p) };
    let b = &app.bridge;
    let Some(session) = b.session_for(app, &pane).filter(|s| b.live(s)) else { return Some(p) };
    let since = now_ms() - 30_000;
    let take = || {
        let mut inner = b.inner.lock().unwrap();
        let t = inner.turns.get_mut(&session).filter(|t| t.ts >= since && !t.used)?;
        t.used = true;
        Some(t.clone())
    };
    let Some(t) = b.wait_for(TURN_WAIT_MS, take).await else { return Some(p) };
    if t.reason != "answer" {
        return None;
    }
    let last_visible = app.inner.lock().unwrap().last_visible;
    if suppress_done(false, t.duration_ms, now_ms() - last_visible) {
        return None;
    }
    if !t.answer.trim().is_empty() {
        p["body"] = json!(t.answer);
    }
    // talk.rs: a long answer read while away gets the TL;DR the mod is writing as its body
    if let Some(tldr) = crate::talk::await_tldr(app, &session).await {
        p["body"] = json!(format!("TL;DR {tldr}"));
    }
    if t.answer_i.is_some() {
        p["url"] = json!(done_url(&pane, t.answer_i, is_home(app, &pane)));
    }
    Some(p)
}

/// Index of the transcript `text` item holding the end of `answer` (the turn's final text).
pub(crate) async fn answer_index(app: &App, session: &str, answer: &str) -> Option<usize> {
    let a = answer.trim().to_string();
    if a.is_empty() {
        return None;
    }
    let t = crate::transcript(app, session)?;
    tokio::task::spawn_blocking(move || {
        let mut t = t.lock().unwrap();
        t.refresh();
        let n = t.items.len();
        t.items[n.saturating_sub(400)..]
            .iter()
            .rev()
            .find(|it| {
                let x = it.text.as_deref().unwrap_or("").trim();
                let head: String = x.chars().take(200).collect();
                it.k == "text" && !x.is_empty() && (a.ends_with(x) || a.contains(&head))
            })
            .map(|it| it.i)
    })
    .await
    .ok()
    .flatten()
}

/// Exact turn record from the mod's `turn.complete` (main loop only):
/// `{session, pane, turnId, answer, durationMs, reason, error?, usage?}`.
async fn api_turn(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let Some(session) = b["session"].as_str().map(str::to_string) else {
        return json_err(StatusCode::BAD_REQUEST, "session required");
    };
    let answer = cut(b["answer"].as_str().unwrap_or(""), 20_000).0;
    let reason = match b["reason"].as_str() {
        Some(r @ ("answer" | "aborted" | "refusal" | "error")) => r.to_string(),
        _ => "answer".to_string(),
    };
    let duration_ms = b["durationMs"].as_i64().unwrap_or(0);
    let answer_i = answer_index(&app, &session, &answer).await;
    let rec = TurnRec { ts: now_ms(), answer: answer.clone(), duration_ms, reason: reason.clone(), answer_i, used: false };
    app.bridge.inner.lock().unwrap().turns.insert(session.clone(), rec);
    app.bridge.bump();
    app.refresh.notify_one();
    let pane = pane_of(&app, &b);
    if let (Some(pane), true) = (pane, reason == "error" || reason == "refusal") {
        let muted = app.prefs.lock().unwrap().flags(&session).1;
        let last_visible = app.inner.lock().unwrap().last_visible;
        if !watching(&app, &pane) && !suppress_done(muted, duration_ms, now_ms() - last_visible) {
            let detail = b["error"].as_str().filter(|e| !e.trim().is_empty()).map(str::to_string);
            let body = match (reason.as_str(), detail) {
                ("refusal", Some(d)) => format!("Refused: {d}"),
                ("refusal", None) => "The model refused to continue.".to_string(),
                (_, Some(d)) => d,
                _ if !answer.trim().is_empty() => answer.clone(),
                _ => "The turn ended on an API error.".to_string(),
            };
            let payload = json!({"title": format!("Failed · {}", title_of(&app, &pane)), "body": cut(&body, 300).0,
                "url": done_url(&pane, answer_i, is_home(&app, &pane)), "tag": pane});
            send_push(&app, &payload, true).await;
        }
    }
    Json(json!({"ok": true, "answerI": answer_i})).into_response()
}

/// The transcript estimate as carried from the transcript pass to `decorate`.
pub fn est_json(e: &crate::transcript::CtxEst) -> Value {
    json!({"model": e.model, "tokens": e.tokens, "max": e.max, "ts": e.ts, "cost": e.cost})
}

/// Context window when the mod has not said: what it reported for the same model, else 1M for the
/// models Claude Code runs with a 1M window here (Opus 4.6+/5.x, Sonnet 5.x, Fable/Mythos — opus-5-5
/// transcripts on this machine reach 730k without compacting) or when a prompt already exceeded 200k, else 200k.
fn window_for(model: &str, max: u64, learned: &HashMap<String, u64>) -> u64 {
    let m = model.trim_start_matches("us.").trim_start_matches("anthropic.");
    let big = ["claude-opus-4-6", "claude-opus-4-7", "claude-opus-4-8", "claude-opus-5", "claude-sonnet-5", "claude-fable", "claude-mythos"]
        .iter()
        .any(|p| m.starts_with(p))
        || m.ends_with("[1m]");
    let w = learned.get(model).copied().unwrap_or(if big { 1_000_000 } else { 200_000 });
    if max > w { 1_000_000.max(max) } else { w }
}

/// The mod's exact gauges when it reported (teaching `learned` the window of the transcript's model),
/// else the transcript estimate.
fn pick_ctx(gauge: Option<Value>, est: Value, learned: &mut HashMap<String, u64>) -> Value {
    match gauge {
        Some(g) => {
            if let (Some(m), Some(w)) = (est["model"].as_str().filter(|m| !m.is_empty()), g["window"].as_u64()) {
                learned.insert(m.to_string(), w);
            }
            g
        }
        None => estimate(&est, learned),
    }
}

/// `ctx` from the transcript estimate (`est: true`), or null when the transcript has no usage yet.
fn estimate(est: &Value, learned: &HashMap<String, u64>) -> Value {
    let Some(model) = est["model"].as_str() else { return Value::Null };
    let window = window_for(model, est["max"].as_u64().unwrap_or(0), learned);
    let tokens = est["tokens"].as_u64();
    let pct = tokens.map(|t| (t as f64 * 1000.0 / window as f64).round() / 10.0);
    let cost = est["cost"].as_f64().filter(|c| *c > 0.0).map(|c| (c * 100.0).round() / 100.0);
    json!({"pct": pct, "tokens": tokens, "window": window, "costUsd": cost, "rateLimits": [], "ts": est["ts"],
           "est": true, "model": model})
}

/// Usage gauges from the mod's `session.measure` (after every turn):
/// `{session, pane, context:{percent, tokens, window}, costUsd, rateLimits:[{kind, percentUsed, resetsAt}]}`.
async fn api_measure(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let Some(session) = b["session"].as_str().map(str::to_string) else {
        return json_err(StatusCode::BAD_REQUEST, "session required");
    };
    let rates: Vec<Value> = b["rateLimits"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|r| {
                    Some(json!({"kind": r["kind"].as_str()?, "pct": r["percentUsed"].as_f64()?, "resetsAt": r["resetsAt"]}))
                })
                .collect()
        })
        .unwrap_or_default();
    let c = &b["context"];
    let ctx = json!({
        "pct": c["percent"].as_f64(), "tokens": c["tokens"].as_u64(), "window": c["window"].as_u64(),
        "costUsd": b["costUsd"].as_f64(), "rateLimits": rates, "ts": now_ms(),
    });
    let mut alerts = Vec::new();
    {
        let mut inner = app.bridge.inner.lock().unwrap();
        inner.gauges.insert(session.clone(), ctx);
        inner.compacting.remove(&session);
        for r in &rates {
            let kind = r["kind"].as_str().unwrap_or("").to_string();
            let resets = r["resetsAt"].as_str().unwrap_or("").to_string();
            let pct = r["pct"].as_f64().unwrap_or(0.0);
            if pct >= RATE_ALERT_PCT && inner.rate_alerted.get(&kind) != Some(&resets) {
                inner.rate_alerted.insert(kind.clone(), resets.clone());
                alerts.push((kind, pct, resets));
            }
        }
    }
    app.refresh.notify_one();
    for (kind, pct, resets) in alerts {
        let name = match kind.as_str() {
            "five_hour" => "5-hour limit".to_string(),
            "seven_day" => "weekly limit".to_string(),
            k => k.replace('_', " "),
        };
        let left = iso_ms(&resets).map(|t| t - now_ms()).filter(|d| *d > 0);
        let body = match left {
            Some(d) => format!("Resets in {}.", human_ms(d)),
            None => "Close to the limit.".to_string(),
        };
        let payload = json!({"title": format!("{pct:.0}% of the {name} used"), "body": body, "url": "/#/agents", "tag": format!("rate-{kind}")});
        send_push(&app, &payload, false).await;
    }
    Json(json!({"ok": true})).into_response()
}

/// "2h 13m", "45m", "3d 4h".
fn human_ms(ms: i64) -> String {
    let m = ms / 60_000;
    let (d, h, m) = (m / 1440, (m / 60) % 24, m % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{}m", m.max(1))
    }
}

/// Parses an ISO 8601 timestamp (`2026-10-05T18:00:00Z`, fractional seconds and `±HH:MM` allowed) to unix ms.
fn iso_ms(s: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let rest = s.get(19..)?;
    let rest = rest.strip_prefix('.').map(|r| r.trim_start_matches(|c: char| c.is_ascii_digit())).unwrap_or(rest);
    let off = match rest {
        "" | "Z" | "z" => 0,
        r if r.len() == 6 && (r.starts_with('+') || r.starts_with('-')) => {
            let v = r[1..3].parse::<i64>().ok()? * 60 + r[4..6].parse::<i64>().ok()?;
            if r.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + se) - off * 60) * 1000)
}

#[derive(Deserialize, Default)]
struct CompactBody {
    instructions: Option<String>,
}

/// "Compact now" from the phone: through the mod (`$.session.compact`, run once the turn
/// ends), else typed in as `/compact` when the agent is idle.
async fn api_compact(State(app): State<Shared>, AxPath(pane): AxPath<String>, body: Option<Json<CompactBody>>) -> Response {
    let instructions = body.and_then(|b| b.0.instructions).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let status = app.inner.lock().unwrap().status.get(&pane).map(|s| s.0.clone()).unwrap_or_default();
    let b = &app.bridge;
    if let Some(session) = b.session_for(&app, &pane).filter(|s| b.live(s)) {
        let mut msg = json!({"kind": "compact"});
        if let Some(i) = &instructions {
            msg["instructions"] = json!(cut(i, 2000).0);
        }
        let seq = b.enqueue(&session, msg);
        b.inner.lock().unwrap().compacting.insert(session.clone(), now_ms());
        app.refresh.notify_one();
        let ack = b.wait_for(3_000, || b.inner.lock().unwrap().acks.get(&seq).cloned()).await;
        return match ack {
            Some(a) if a["ok"].as_bool() == Some(false) => {
                b.inner.lock().unwrap().compacting.remove(&session);
                app.refresh.notify_one();
                json_err(StatusCode::CONFLICT, a["error"].as_str().unwrap_or("compact failed"))
            }
            a => Json(json!({"ok": true, "via": "inbox", "how": a.map(|a| a["via"].clone()).unwrap_or(json!("unconfirmed"))}))
                .into_response(),
        };
    }
    if status != "idle" && status != "done" {
        return json_err(StatusCode::CONFLICT, "busy");
    }
    let text = match &instructions {
        Some(i) => format!("/compact {i}"),
        None => "/compact".to_string(),
    };
    match herdr::quick("agent.prompt", json!({"target": pane, "text": text})).await {
        Ok(_) => Json(json!({"ok": true, "via": "herdr"})).into_response(),
        Err(e) => crate::herdr_err(e),
    }
}

/// `ask_phone` from the mod: `{session, pane, question, options?}` → `{id}`. Pushes like a blocked agent.
async fn api_ask(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let Some(session) = b["session"].as_str().map(str::to_string) else {
        return json_err(StatusCode::BAD_REQUEST, "session required");
    };
    let question = b["question"].as_str().unwrap_or("").trim().to_string();
    if question.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "question required");
    }
    let options: Vec<String> = b["options"]
        .as_array()
        .map(|l| l.iter().filter_map(Value::as_str).map(str::trim).filter(|o| !o.is_empty()).take(6).map(|o| cut(o, 80).0).collect())
        .unwrap_or_default();
    let pane = pane_of(&app, &b);
    let id = {
        let mut inner = app.bridge.inner.lock().unwrap();
        inner.seq += 1;
        let id = format!("q{:x}", inner.seq);
        inner.asks.insert(id.clone(), PhoneAsk { session, pane: pane.clone(), ts: now_ms(), question: cut(&question, 2000).0, options, answer: None });
        id
    };
    app.refresh.notify_one();
    if let Some(pane) = pane.filter(|p| !watching(&app, p)) {
        let payload = json!({"title": format!("Needs you · {}", title_of(&app, &pane)), "body": cut(&question, 300).0,
            "url": format!("/#/a/{pane}"), "tag": pane});
        send_push(&app, &payload, true).await;
    }
    Json(json!({"ok": true, "id": id})).into_response()
}

#[derive(Deserialize)]
struct WaitQ {
    wait: Option<u64>,
}

/// Long-poll (≤25 s) for the phone's reply: `{state:"answered", text}` | `{state:"pending"}`; 404 `{state:"gone"}`.
async fn api_ask_wait(State(app): State<Shared>, AxPath(id): AxPath<String>, Query(q): Query<WaitQ>) -> Response {
    let b = &app.bridge;
    let check = || {
        let inner = b.inner.lock().unwrap();
        match inner.asks.get(&id) {
            None => Some(None),
            Some(a) => a.answer.clone().map(Some),
        }
    };
    match b.wait_for(q.wait.unwrap_or(HOLD_MS).min(HOLD_MS), check).await {
        None => Json(json!({"state": "pending"})).into_response(),
        Some(None) => (StatusCode::NOT_FOUND, Json(json!({"state": "gone"}))).into_response(),
        Some(Some(text)) => {
            b.inner.lock().unwrap().asks.remove(&id);
            app.refresh.notify_one();
            Json(json!({"state": "answered", "text": text})).into_response()
        }
    }
}

/// The mod gave up on a question (timeout, interrupted turn).
async fn api_ask_cancel(State(app): State<Shared>, AxPath(id): AxPath<String>) -> Response {
    app.bridge.inner.lock().unwrap().asks.remove(&id);
    app.bridge.bump();
    app.refresh.notify_one();
    Json(json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct AnswerBody {
    text: String,
    id: Option<String>,
}

/// The phone answers the agent's pending `ask_phone` question (`id`, else the newest for the pane).
async fn api_answer(State(app): State<Shared>, AxPath(pane): AxPath<String>, Json(body): Json<AnswerBody>) -> Response {
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "text required");
    }
    let session = pane_session(&app, &pane);
    let id = {
        let mut inner = app.bridge.inner.lock().unwrap();
        let id = body.id.filter(|i| inner.asks.get(i).is_some_and(|a| a.answer.is_none())).or_else(|| {
            inner
                .asks
                .iter()
                .filter(|(_, a)| a.answer.is_none() && (a.pane.as_deref() == Some(pane.as_str()) || Some(&a.session) == session.as_ref()))
                .max_by_key(|(_, a)| a.ts)
                .map(|(id, _)| id.clone())
        });
        if let Some(a) = id.as_ref().and_then(|i| inner.asks.get_mut(i)) {
            a.answer = Some(cut(&text, 4000).0);
        }
        id
    };
    let Some(id) = id else { return json_err(StatusCode::NOT_FOUND, "no_question") };
    app.bridge.bump();
    app.refresh.notify_one();
    Json(json!({"ok": true, "id": id})).into_response()
}

#[derive(Deserialize)]
struct InboxQ {
    session: String,
    pane: Option<String>,
    since: Option<u64>,
    /// Hold time in ms (default and max 25000; 0 = answer at once).
    wait: Option<u64>,
}

/// Long-poll: the queued messages for `session` newer than `since`, each delivered once.
async fn api_inbox(State(app): State<Shared>, Query(q): Query<InboxQ>) -> Response {
    if q.session.is_empty() || q.session.len() > 100 {
        return json_err(StatusCode::BAD_REQUEST, "session required");
    }
    let b = &app.bridge;
    let since = q.since.unwrap_or(0);
    let pane = q.pane.filter(|p| !p.is_empty() && p.len() < 40);
    let touch = || {
        let mut inner = b.inner.lock().unwrap();
        let now = now_ms();
        let first = !inner.polls.contains_key(&q.session);
        inner.polls.insert(q.session.clone(), Poll { pane: pane.clone(), ts: now });
        inner.polls.retain(|_, p| now - p.ts < 10 * 60_000);
        first
    };
    if touch() {
        app.refresh.notify_one();
    }
    let take = || {
        let mut inner = b.inner.lock().unwrap();
        let now = now_ms();
        let q = inner.queues.get_mut(&q.session)?;
        q.retain(|m| now - m.ts < MSG_TTL_MS && !m.delivered);
        let mut out = Vec::new();
        for m in q.iter_mut().filter(|m| m.seq > since) {
            m.delivered = true;
            out.push(m.body.clone());
        }
        (!out.is_empty()).then_some(out)
    };
    let msgs = b.wait_for(q.wait.unwrap_or(HOLD_MS).min(HOLD_MS), take).await.unwrap_or_default();
    touch();
    if !msgs.is_empty() {
        b.bump(); // wakes /steer waiting for delivery
    }
    let seq = b.inner.lock().unwrap().seq;
    Json(json!({"seq": seq, "messages": msgs})).into_response()
}

/// Internal: queue any command for a session (or the session of a pane).
async fn api_send(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let session = b["session"]
        .as_str()
        .map(str::to_string)
        .or_else(|| b["pane"].as_str().and_then(|p| app.bridge.session_for(&app, p)));
    let Some(session) = session else { return json_err(StatusCode::NOT_FOUND, "unknown agent") };
    let Some(kind) = b["kind"].as_str().filter(|k| !k.is_empty()) else {
        return json_err(StatusCode::BAD_REQUEST, "kind required");
    };
    let mut body = b.clone();
    if let Some(o) = body.as_object_mut() {
        o.remove("session");
        o.remove("pane");
        o.insert("kind".into(), json!(kind));
    }
    let seq = app.bridge.enqueue(&session, body);
    Json(json!({"ok": true, "seq": seq, "live": app.bridge.live(&session)})).into_response()
}

/// The mod reports the outcome of a message: `{seq, ok, via?, error?}`.
async fn api_ack(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let Some(seq) = b["seq"].as_u64() else { return json_err(StatusCode::BAD_REQUEST, "seq required") };
    {
        let mut inner = app.bridge.inner.lock().unwrap();
        inner.acks.insert(seq, b.clone());
        if inner.acks.len() > 200 {
            let min = *inner.acks.keys().min().unwrap_or(&0);
            inner.acks.remove(&min);
        }
    }
    app.bridge.bump();
    Json(json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct SteerBody {
    text: String,
    nonce: Option<String>,
    /// The sending agent's pane: the text gets a first line naming it (`transcript::steer_header`).
    #[serde(default)]
    from: Option<String>,
}


/// Steer a running turn: through the mod (`$.session.append`, read at the turn's
/// next step) when the agent is working and its mod is connected, else typed in
/// via herdr like `/prompt`.
async fn api_steer(State(app): State<Shared>, AxPath(pane): AxPath<String>, Json(b): Json<SteerBody>) -> Response {
    let mut text = b.text.trim().to_string();
    if text.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "text required");
    }
    if let Some(from) = b.from.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        if !crate::transcript::is_pane(from) || from == pane {
            return json_err(StatusCode::BAD_REQUEST, "from must be another agent's pane id");
        }
        text = format!("{}\n{text}", crate::transcript::steer_header(&title_of(&app, from), from));
    }
    if seen_nonce(&app, b.nonce.as_deref()) {
        return Json(json!({"ok": true, "dup": true})).into_response();
    }
    // One message at a time per agent, in arrival order.
    let lock = app.bridge.pane_lock(&pane);
    let _turn = lock.lock().await;
    let working = app.inner.lock().unwrap().status.get(&pane).is_some_and(|s| s.0 == "working");
    if !working {
        app.bridge.typed_this_turn.lock().unwrap().remove(&pane);
    }
    // Appending would overtake anything already waiting in Claude's input queue.
    let queue_waiting = app.bridge.typed_this_turn.lock().unwrap().contains(&pane) || pane_has_queue(&app, &pane);
    let session = app.bridge.session_for(&app, &pane).filter(|s| app.bridge.live(s));
    if let (true, false, Some(session)) = (working, queue_waiting, session) {
        let seq = app.bridge.enqueue(&session, json!({"kind": "steer", "text": text, "working": true}));
        let b2 = &app.bridge;
        let ack = b2.wait_for(STEER_ACK_MS, || b2.inner.lock().unwrap().acks.get(&seq).cloned()).await;
        match ack {
            Some(a) if a["ok"].as_bool() == Some(true) => {
                app.refresh.notify_one();
                return Json(json!({"ok": true, "via": "inbox", "how": a["via"]})).into_response();
            }
            Some(_) => {} // the mod could not append: type it in instead
            None => {
                // Picked up but unanswered: assume it landed rather than risk sending it twice.
                if !b2.withdraw(&session, seq) && b2.delivered(&session, seq) {
                    return Json(json!({"ok": true, "via": "inbox", "how": "unconfirmed"})).into_response();
                }
            }
        }
    }
    match herdr::quick("agent.prompt", json!({"target": pane, "text": text})).await {
        Ok(_) => {
            if working {
                app.bridge.typed_this_turn.lock().unwrap().insert(pane.clone());
            }
            app.refresh.notify_one();
            Json(json!({"ok": true, "via": "herdr"})).into_response()
        }
        Err(e) => {
            if let Some(n) = b.nonce.as_deref() {
                app.inner.lock().unwrap().nonces.retain(|(x, _)| x != n);
            }
            crate::herdr_err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_estimate_window() {
        let none = HashMap::new();
        assert_eq!(window_for("claude-opus-5-5", 40_000, &none), 1_000_000);
        assert_eq!(window_for("claude-haiku-4-5-20251001", 40_000, &none), 200_000);
        assert_eq!(window_for("claude-sonnet-4-5", 300_000, &none), 1_000_000);
        let learned = HashMap::from([("claude-opus-5-5".to_string(), 200_000u64)]);
        assert_eq!(window_for("claude-opus-5-5", 40_000, &learned), 200_000);
        let c = estimate(&json!({"model": "claude-opus-5-5", "tokens": 126_864, "max": 126_864, "ts": 5, "cost": 1.234}), &none);
        assert_eq!(c["pct"], json!(12.7));
        assert_eq!((c["window"].as_u64(), c["tokens"].as_u64(), c["est"].as_bool()), (Some(1_000_000), Some(126_864), Some(true)));
        assert_eq!(c["costUsd"], json!(1.23));
        let c = estimate(&json!({"model": "claude-haiku-4-5", "tokens": null, "max": 9, "ts": 5, "cost": 0.0}), &none);
        assert!(c["pct"].is_null() && c["costUsd"].is_null());
        assert!(estimate(&Value::Null, &none).is_null());
    }

    #[test]
    fn mod_gauges_win_and_teach_the_window() {
        let mut learned = HashMap::new();
        let est = json!({"model": "claude-opus-5-5", "tokens": 100_000, "max": 100_000, "ts": 1, "cost": 1.0});
        let g = json!({"pct": 50.0, "tokens": 100_000, "window": 200_000});
        assert_eq!(pick_ctx(Some(g.clone()), est.clone(), &mut learned), g);
        assert_eq!(learned.get("claude-opus-5-5"), Some(&200_000));
        assert_eq!(pick_ctx(None, est, &mut learned)["pct"], json!(50.0));
    }

    #[tokio::test]
    async fn enqueue_deliver_once_and_withdraw() {
        let b = Bridge::default();
        let s1 = b.enqueue("s", json!({"kind": "steer", "text": "a"}));
        let s2 = b.enqueue("s", json!({"kind": "steer", "text": "b"}));
        assert!(s2 > s1);
        assert!(b.withdraw("s", s2));
        assert!(!b.delivered("s", s1));
        let got = b
            .wait_for(10, || {
                let mut inner = b.inner.lock().unwrap();
                let q = inner.queues.get_mut("s")?;
                let out: Vec<Value> = q.iter_mut().filter(|m| !m.delivered).map(|m| { m.delivered = true; m.body.clone() }).collect();
                (!out.is_empty()).then_some(out)
            })
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["text"], "a");
        assert!(b.delivered("s", s1));
        assert!(!b.withdraw("s", s1));
    }

    #[test]
    fn iso_and_human() {
        assert_eq!(iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_ms("2026-10-05T18:00:00.123Z"), Some(1_791_223_200_000));
        assert_eq!(iso_ms("2026-10-05T11:00:00-07:00"), Some(1_791_223_200_000));
        assert_eq!(iso_ms("nope"), None);
        assert_eq!(human_ms(2 * 3600_000 + 13 * 60_000), "2h 13m");
        assert_eq!(human_ms(30_000), "1m");
        assert_eq!(human_ms(3 * 86_400_000 + 4 * 3600_000), "3d 4h");
    }

    #[tokio::test]
    async fn wait_for_wakes_on_bump() {
        let b = std::sync::Arc::new(Bridge::default());
        let b2 = b.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            b2.enqueue("x", json!({"kind": "compact"}));
        });
        let t0 = std::time::Instant::now();
        let got = b.wait_for(2000, || b.inner.lock().unwrap().queues.get("x").map(|q| q.len())).await;
        assert_eq!(got, Some(1));
        assert!(t0.elapsed() < Duration::from_millis(1000));
    }
}

/// Whether the agent has messages waiting in Claude's own input queue (from the
/// transcript's queue rows, as published in state).
fn pane_has_queue(app: &App, pane: &str) -> bool {
    let st = app.inner.lock().unwrap().state.clone();
    let v: Value = serde_json::from_str(&st).unwrap_or(Value::Null);
    v["agents"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["pane"].as_str() == Some(pane)))
        .and_then(|a| a["queued"].as_array())
        // steers the mod holds (talk.rs, `mod:true`) are not in Claude's queue: later ones are held behind them
        .is_some_and(|q| q.iter().any(|x| x["mod"] != true))
}
