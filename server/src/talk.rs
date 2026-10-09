//! Talking to agents: unsend a queued message, Claude's own next-prompt
//! suggestion as a chip, a TL;DR of long answers for the away user, "Catch me
//! up" digests, `/phone` commands from the desk, and permission prompts /
//! auto-mode denials answered from the phone.
//!
//! The mod (mod/sheprd) reports through `/api/talk/*` and acts on inbox
//! messages (`unsend`, `digest`, `allow_once`). Hooks from main.rs: `decorate`
//! (per-agent `talk` field, chips, held steers in `queued`, perm → blocked),
//! `routes`; from modbridge.rs: `await_tldr` (push body); from v3.rs:
//! `forced_away` (`/phone away` turns off desk suppression).

use crate::transcript::{cut, now_ms, tool_summary};
use crate::{App, Shared, herdr, json_err, send_push};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;

/// A done push waits this long for the TL;DR the mod is writing.
const TLDR_WAIT_MS: u64 = 12_000;
/// A "TL;DR coming" mark older than this is ignored.
const TLDR_PENDING_TTL_MS: i64 = 60_000;
/// Catch-up cards are dropped after this.
const DIGEST_TTL_MS: i64 = 2 * 3600_000;
/// A digest the mod never answered turns into an error after this.
const DIGEST_TIMEOUT_MS: i64 = 120_000;
/// Longest a permission wait poll is held open.
const PERM_HOLD_MS: u64 = 25_000;
/// Permission questions and auto-mode denials are dropped after this.
const PERM_TTL_MS: i64 = 30 * 60_000;
/// How long `/unsend` waits for the mod's ack.
const UNSEND_ACK_MS: u64 = 4_000;
/// `/phone away` lasts at most this long.
const AWAY_MAX_MS: i64 = 12 * 3600_000;
/// Claude's input-box hint while messages are queued (not text the user typed).
const QUEUE_HINT: &str = "Press up to edit queued messages";

pub struct Talk {
    inner: Mutex<Inner>,
    tick: watch::Sender<u64>,
}

impl Default for Talk {
    fn default() -> Self {
        Talk { inner: Mutex::new(Inner::default()), tick: watch::channel(0).0 }
    }
}

#[derive(Default)]
struct Inner {
    seq: u64,
    /// session -> Claude's own next-prompt guess (text, ts)
    suggest: HashMap<String, (String, i64)>,
    /// session -> latest TL;DR
    tldr: HashMap<String, Tldr>,
    /// session -> (turnId, ts) of a TL;DR the mod is writing
    tldr_pending: HashMap<String, (String, i64)>,
    /// pane -> latest catch-up digest
    digests: HashMap<String, Digest>,
    /// session -> (steers the mod holds until the turn's next step, snapshot ts)
    held: HashMap<String, (Vec<Value>, i64)>,
    /// id -> permission question / auto-mode denial
    perms: HashMap<String, Perm>,
    /// `/phone away` until (ms); 0 = presence is detected
    away_until: i64,
}

#[derive(Clone)]
struct Tldr {
    turn: String,
    text: String,
    i: Option<usize>,
    ts: i64,
}

#[derive(Clone)]
struct Digest {
    id: String,
    state: &'static str,
    text: String,
    ts: i64,
    since: Option<i64>,
}

#[derive(Clone)]
struct Perm {
    session: String,
    pane: Option<String>,
    /// "ask" (the agent waits on it) | "denied" (auto mode blocked it)
    kind: &'static str,
    tool: String,
    summary: String,
    input: Value,
    reason: Option<String>,
    /// "Allow always" is offered (the request carried permission suggestions)
    always: bool,
    ts: i64,
    decision: Option<String>,
}

impl Talk {
    fn bump(&self) {
        self.tick.send_modify(|v| *v += 1);
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        format!("{prefix}{:x}{:x}", now_ms() & 0xffffff, inner.seq)
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
        .route("/api/talk", get(api_talk))
        .route("/api/talk/suggest", post(api_suggest))
        .route("/api/talk/tldr", post(api_tldr))
        .route("/api/talk/held", post(api_held))
        .route("/api/talk/digest", post(api_digest))
        .route("/api/talk/cmd", post(api_cmd))
        .route("/api/talk/perm", post(api_perm))
        .route("/api/talk/perm/{id}/wait", get(api_perm_wait))
        .route("/api/talk/perm/{id}/cancel", post(api_perm_cancel))
        .route("/api/a/{pane}/unsend", post(api_unsend))
        .route("/api/a/{pane}/catchup", post(api_catchup))
        .route("/api/a/{pane}/perm", post(api_perm_decide))
        .route("/api/a/{pane}/askanswer", post(api_askanswer))
}

// ---------- presence ----------

/// `/phone away` is on: pushes are not held back for the desk.
pub fn forced_away(app: &App) -> bool {
    app.talk.inner.lock().unwrap().away_until > now_ms()
}

/// The user sits at this machine (a local herdr client, screen unlocked and awake),
/// whichever pane is focused. Permission prompts then show in the terminal as usual.
async fn desk_present(app: &App) -> bool {
    !forced_away(app) && crate::v3::desktop_client() && crate::v3::hypr_present().await
}

/// A visible client is showing this pane right now.
fn watching(app: &App, pane: &str) -> bool {
    let inner = app.inner.lock().unwrap();
    matches!(&inner.viewing, Some((Some(p), true, t)) if p == pane && now_ms() - t < 45_000)
}

/// Nobody is reading this agent: not on the phone screen, not focused at the desk.
async fn away(app: &App, pane: Option<&str>) -> bool {
    if forced_away(app) {
        return true;
    }
    let Some(pane) = pane else { return true };
    !watching(app, pane) && !crate::v3::at_desk(app, pane).await
}

fn pane_of(app: &App, b: &Value) -> Option<String> {
    if let Some(p) = b["pane"].as_str().filter(|p| !p.is_empty()) {
        return Some(p.to_string());
    }
    let s = b["session"].as_str()?;
    app.inner.lock().unwrap().pane_sessions.iter().find(|(_, v)| v.as_str() == s).map(|(p, _)| p.clone())
}

fn title_of(app: &App, pane: &str) -> String {
    app.inner.lock().unwrap().pane_titles.get(pane).cloned().unwrap_or_else(|| "agent".into())
}

fn session_of(b: &Value) -> Option<String> {
    b["session"].as_str().filter(|s| !s.is_empty() && s.len() <= 100).map(str::to_string)
}

// ---------- state ----------

/// Adds the per-agent `talk` field — `{suggest?, tldr?, digest?, perm?, denied?}` —,
/// puts Claude's own suggestion first in `chips`, lists the steers the mod holds in
/// `queued` (`{text, ts, id:"m<seq>", mod:true}`) and shows a pending permission
/// question as `status: "blocked"`.
pub fn decorate(app: &App, agents: &mut [Value]) {
    let now = now_ms();
    let mut t = app.talk.inner.lock().unwrap();
    t.perms.retain(|_, p| now - p.ts < PERM_TTL_MS && (p.kind == "ask" || p.decision.is_none()));
    t.digests.retain(|_, d| now - d.ts < DIGEST_TTL_MS);
    t.held.retain(|_, (_, ts)| now - *ts < 6 * 3600_000);
    for d in t.digests.values_mut() {
        if d.state == "pending" && now - d.ts > DIGEST_TIMEOUT_MS {
            d.state = "error";
            d.text = "No answer from the agent.".into();
        }
    }
    for a in agents.iter_mut() {
        let sid = a["session"].as_str().unwrap_or("").to_string();
        let pane = a["pane"].as_str().unwrap_or("").to_string();
        let status = a["status"].as_str().unwrap_or("").to_string();
        let since = a["since"].as_i64().unwrap_or(0);
        let mut talk = json!({});
        if let Some((text, ts)) = t.suggest.get(&sid).cloned() {
            if status == "working" && since > ts {
                t.suggest.remove(&sid);
            } else if status == "idle" || status == "done" {
                let mut chips: Vec<Value> = a["chips"].as_array().cloned().unwrap_or_default();
                chips.retain(|c| !c.as_str().is_some_and(|c| c.eq_ignore_ascii_case(&text)));
                chips.insert(0, json!(text));
                chips.truncate(4);
                a["chips"] = Value::Array(chips);
                talk["suggest"] = json!(text);
            }
        }
        if let Some(x) = t.tldr.get(&sid) {
            talk["tldr"] = json!({"turnId": x.turn, "text": x.text, "i": x.i, "ts": x.ts});
        }
        if let Some(d) = t.digests.get(&pane) {
            talk["digest"] = json!({"id": d.id, "state": d.state, "text": d.text, "ts": d.ts, "since": d.since});
        }
        if status == "working" || status == "blocked" {
            if let Some((held, _)) = t.held.get(&sid).filter(|(h, _)| !h.is_empty()) {
                let mut q = a["queued"].as_array().cloned().unwrap_or_default();
                for h in held {
                    q.push(json!({"text": h["text"], "ts": h["ts"], "id": format!("m{}", h["seq"]), "mod": true}));
                }
                a["queued"] = Value::Array(q);
            }
        }
        let mine = |p: &Perm| p.session == sid || (!pane.is_empty() && p.pane.as_deref() == Some(pane.as_str()));
        let card = |id: &str, p: &Perm| {
            json!({"id": id, "kind": p.kind, "tool": p.tool, "summary": p.summary, "reason": p.reason,
                   "always": p.always, "ts": p.ts})
        };
        // answered in the terminal: herdr no longer sees the dialog
        if status != "blocked" {
            t.perms.retain(|_, p| !(p.kind == "ask" && p.decision.is_none() && mine(p) && now - p.ts > 6_000));
        }
        if let Some((id, p)) =
            t.perms.iter().filter(|(_, p)| p.kind == "ask" && p.decision.is_none() && mine(p)).max_by_key(|(_, p)| p.ts)
        {
            talk["perm"] = card(id, p);
            a["status"] = json!("blocked");
            a["chips"] = json!([]);
        }
        let mut denied: Vec<(&String, &Perm)> =
            t.perms.iter().filter(|(_, p)| p.kind == "denied" && p.decision.is_none() && mine(p)).collect();
        denied.sort_by_key(|(_, p)| -p.ts);
        if !denied.is_empty() {
            talk["denied"] = denied.iter().take(3).map(|(id, p)| card(id, p)).collect();
        }
        a["talk"] = talk;
    }
}

/// `GET /api/talk` → `{away}`: whether `/phone away` is on (until when).
async fn api_talk(State(app): State<Shared>) -> Response {
    let until = app.talk.inner.lock().unwrap().away_until;
    Json(json!({"away": until > now_ms(), "awayUntil": if until > now_ms() { json!(until) } else { Value::Null }})).into_response()
}

// ---------- suggestion chip ----------

/// The mod's `prompt.suggest` (origin `suggestion`): `{session, text}`.
async fn api_suggest(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let Some(session) = session_of(&b) else { return json_err(StatusCode::BAD_REQUEST, "session required") };
    let text = b["text"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    {
        let mut t = app.talk.inner.lock().unwrap();
        if text.is_empty() {
            t.suggest.remove(&session);
        } else {
            t.suggest.insert(session, (cut(&text, 300).0, now_ms()));
        }
    }
    app.refresh.notify_one();
    Json(json!({"ok": true})).into_response()
}

// ---------- TL;DR ----------

/// From the mod's `turn.complete`, for answers over 1500 chars:
/// `{session, pane, turnId, pending:true}` → `{ok, away}` (write one only when away),
/// then `{session, turnId, text, answer?}` (answer = its tail, to find the item) or
/// `{session, turnId, error}`.
async fn api_tldr(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let Some(session) = session_of(&b) else { return json_err(StatusCode::BAD_REQUEST, "session required") };
    let turn = b["turnId"].as_str().unwrap_or("").to_string();
    if b["pending"].as_bool() == Some(true) {
        let pane = pane_of(&app, &b);
        let away = away(&app, pane.as_deref()).await;
        if away {
            app.talk.inner.lock().unwrap().tldr_pending.insert(session, (turn, now_ms()));
        }
        return Json(json!({"ok": true, "away": away})).into_response();
    }
    let text = b["text"].as_str().map(str::trim).unwrap_or("").to_string();
    if text.is_empty() {
        app.talk.inner.lock().unwrap().tldr_pending.remove(&session);
        app.talk.bump();
        return Json(json!({"ok": true})).into_response();
    }
    let answer = b["answer"].as_str().unwrap_or("");
    let i = crate::modbridge::answer_index(&app, &session, answer).await;
    {
        let mut t = app.talk.inner.lock().unwrap();
        t.tldr_pending.remove(&session);
        t.tldr.insert(session, Tldr { turn, text: cut(&text, 600).0, i, ts: now_ms() });
    }
    app.talk.bump();
    app.refresh.notify_one();
    Json(json!({"ok": true, "i": i})).into_response()
}

/// A done push about to go out: the TL;DR the mod is writing for this session's
/// last turn (waits ≤ 12 s), else None.
pub async fn await_tldr(app: &App, session: &str) -> Option<String> {
    let since = {
        let t = app.talk.inner.lock().unwrap();
        let (_, ts) = t.tldr_pending.get(session).filter(|(_, ts)| now_ms() - ts < TLDR_PENDING_TTL_MS)?;
        *ts
    };
    let talk = &app.talk;
    let got = talk
        .wait_for(TLDR_WAIT_MS, || {
            let t = talk.inner.lock().unwrap();
            match t.tldr.get(session).filter(|x| x.ts >= since) {
                Some(x) => Some(Some(x.text.clone())),
                None if !t.tldr_pending.contains_key(session) => Some(None),
                None => None,
            }
        })
        .await;
    got.flatten()
}

// ---------- held steers + unsend ----------

/// The mod's snapshot of the phone steers it holds until the turn's next step:
/// `{session, held:[{seq, text, ts}]}`.
async fn api_held(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let Some(session) = session_of(&b) else { return json_err(StatusCode::BAD_REQUEST, "session required") };
    let held: Vec<Value> = b["held"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|h| Some(json!({"seq": h["seq"].as_u64()?, "text": cut(h["text"].as_str()?, 4000).0, "ts": h["ts"].as_i64().unwrap_or(0)})))
                .take(20)
                .collect()
        })
        .unwrap_or_default();
    app.talk.inner.lock().unwrap().held.insert(session, (held, now_ms()));
    app.refresh.notify_one();
    Json(json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct UnsendBody {
    text: Option<String>,
    id: Option<String>,
}

/// Takes back a queued message before the agent reads it → `{ok, how:"mod"|"claude", text}`.
/// - `id: "m<seq>"`: a steer the mod holds (inbox `unsend`).
/// - else `text`: a message in Claude's own queue: ↑ pulls the queue into the input box,
///   which is emptied, then the other messages are typed back in.
///
/// 409 `{error}`: `already_sent`, `desk_draft` (the desk's input box has text), `no_mod`.
async fn api_unsend(State(app): State<Shared>, AxPath(pane): AxPath<String>, Json(b): Json<UnsendBody>) -> Response {
    if let Some(seq) = b.id.as_deref().and_then(|i| i.strip_prefix('m')).and_then(|s| s.parse::<u64>().ok()) {
        let bridge = &app.bridge;
        let Some(session) = bridge.session_for(&app, &pane).filter(|s| bridge.live(s)) else {
            return json_err(StatusCode::CONFLICT, "no_mod");
        };
        let held_text = {
            let t = app.talk.inner.lock().unwrap();
            t.held.get(&session).and_then(|(h, _)| h.iter().find(|x| x["seq"] == seq).and_then(|x| x["text"].as_str().map(str::to_string)))
        };
        let msg = bridge.enqueue(&session, json!({"kind": "unsend", "ref": seq}));
        let ack = bridge.wait_ack(msg, UNSEND_ACK_MS).await;
        return match ack {
            Some(a) if a["ok"].as_bool() == Some(true) => {
                if let Some((h, _)) = app.talk.inner.lock().unwrap().held.get_mut(&session) {
                    h.retain(|x| x["seq"] != seq);
                }
                app.refresh.notify_one();
                Json(json!({"ok": true, "how": "mod", "text": held_text})).into_response()
            }
            Some(a) => json_err(StatusCode::CONFLICT, a["error"].as_str().unwrap_or("already_sent")),
            None => {
                bridge.withdraw_pub(&session, msg);
                json_err(StatusCode::CONFLICT, "no answer from the agent")
            }
        };
    }
    let Some(text) = b.text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) else {
        return json_err(StatusCode::BAD_REQUEST, "text or id required");
    };
    match unsend_claude(&app, &pane, &text).await {
        Ok(()) => {
            app.refresh.notify_one();
            Json(json!({"ok": true, "how": "claude", "text": text})).into_response()
        }
        Err(e) => json_err(StatusCode::CONFLICT, e),
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The input box's text, with Claude's hints counted as empty.
async fn box_text(pane: &str) -> Option<String> {
    let t = crate::input_box(&crate::read_screen(pane).await)?;
    Some(if t.starts_with(QUEUE_HINT) { String::new() } else { t })
}

async fn keys(pane: &str, k: Value) {
    let _ = herdr::quick("pane.send_keys", json!({"pane_id": pane, "keys": k})).await;
}

/// Empties the input box (ctrl+u per line, backspace joins lines).
async fn clear_box(pane: &str) {
    let mut prev: Option<String> = None;
    for _ in 0..12 {
        match box_text(pane).await {
            Some(t) if !t.is_empty() && prev.as_ref() != Some(&t) => {
                keys(pane, if t.contains('\n') { json!(["ctrl+u", "backspace"]) } else { json!(["ctrl+u"]) }).await;
                tokio::time::sleep(Duration::from_millis(120)).await;
                prev = Some(t);
            }
            _ => break,
        }
    }
}

/// Claude's queue: ↑ (with an empty input box) moves every queued message into the
/// box (`popAll`); the box is emptied and the others are typed back (re-queued in order).
async fn unsend_claude(app: &App, pane: &str, text: &str) -> Result<(), String> {
    let queued: Vec<String> = {
        let s = app.inner.lock().unwrap().state.clone();
        let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
        v["agents"]
            .as_array()
            .and_then(|l| l.iter().find(|a| a["pane"] == pane))
            .and_then(|a| a["queued"].as_array())
            .map(|q| q.iter().filter(|x| x["mod"] != true).filter_map(|x| x["text"].as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let target = norm(text);
    if !queued.iter().any(|q| norm(q) == target) {
        return Err("already_sent".into());
    }
    match box_text(pane).await {
        Some(t) if t.is_empty() => {}
        Some(_) => return Err("desk_draft".into()),
        None => return Err("no input box on screen".into()),
    }
    keys(pane, json!(["up"])).await;
    let mut pulled = String::new();
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        pulled = box_text(pane).await.unwrap_or_default();
        if !pulled.is_empty() {
            break;
        }
    }
    if pulled.is_empty() {
        return Err("already_sent".into());
    }
    clear_box(pane).await;
    // Type the rest back in: one message each when the box held exactly the queue.
    let joined = norm(&queued.join("\n"));
    let rest: Vec<String> = if norm(&pulled) == joined {
        let mut skipped = false;
        queued
            .into_iter()
            .filter(|q| {
                if !skipped && norm(q) == target {
                    skipped = true;
                    return false;
                }
                true
            })
            .collect()
    } else {
        let lines: Vec<&str> = pulled.lines().collect();
        let k = lines.iter().position(|l| norm(l) == target);
        let left: Vec<&str> = lines.iter().enumerate().filter(|(i, _)| Some(*i) != k).map(|(_, l)| *l).collect();
        let left = left.join("\n").trim().to_string();
        if k.is_none() {
            // Not where we expected it: put everything back.
            if !pulled.trim().is_empty() {
                let _ = herdr::quick("agent.prompt", json!({"target": pane, "text": pulled})).await;
            }
            return Err("not found in Claude's queue".into());
        }
        if left.is_empty() { vec![] } else { vec![left] }
    };
    for r in rest {
        if let Err(e) = herdr::quick("agent.prompt", json!({"target": pane, "text": r})).await {
            eprintln!("unsend {pane}: re-queue failed: {e}");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    eprintln!("unsend {pane}: took back {:?}", cut(text, 60).0);
    Ok(())
}

// ---------- catch me up ----------

#[derive(Deserialize, Default)]
struct CatchupBody {
    /// ms of the last item the user saw
    since: Option<i64>,
    /// the last thing the user saw (a few words, to anchor the digest)
    after: Option<String>,
}

/// "Catch me up": the mod forks the session (`$.model.fork`) for a short digest of what
/// changed since `since` → `{ok, id}`; the card is `agents[].talk.digest`. 409 `no_mod`.
async fn api_catchup(State(app): State<Shared>, AxPath(pane): AxPath<String>, body: Option<Json<CatchupBody>>) -> Response {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let bridge = &app.bridge;
    let Some(session) = bridge.session_for(&app, &pane).filter(|s| bridge.live(s)) else {
        return json_err(StatusCode::CONFLICT, "no_mod");
    };
    let id = app.talk.next_id("d");
    let mut msg = json!({"kind": "digest", "id": id});
    if let Some(s) = b.since.filter(|s| *s > 0) {
        msg["since"] = json!(s);
    }
    if let Some(a) = b.after.map(|a| norm(&a)).filter(|a| !a.is_empty()) {
        msg["after"] = json!(cut(&a, 300).0);
    }
    app.talk.inner.lock().unwrap().digests.insert(
        pane.clone(),
        Digest { id: id.clone(), state: "pending", text: String::new(), ts: now_ms(), since: b.since },
    );
    bridge.enqueue(&session, msg);
    app.refresh.notify_one();
    Json(json!({"ok": true, "id": id})).into_response()
}

/// The mod's digest: `{id, text}` or `{id, error}`.
async fn api_digest(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let Some(id) = b["id"].as_str() else { return json_err(StatusCode::BAD_REQUEST, "id required") };
    let found = {
        let mut t = app.talk.inner.lock().unwrap();
        match t.digests.values_mut().find(|d| d.id == id) {
            Some(d) => {
                match b["text"].as_str().map(str::trim).filter(|x| !x.is_empty()) {
                    Some(text) => {
                        d.state = "done";
                        d.text = cut(text, 3000).0;
                    }
                    None => {
                        d.state = "error";
                        d.text = cut(b["error"].as_str().unwrap_or("no reply"), 300).0;
                    }
                }
                d.ts = now_ms();
                true
            }
            None => false,
        }
    };
    if !found {
        return json_err(StatusCode::NOT_FOUND, "unknown digest");
    }
    app.refresh.notify_one();
    Json(json!({"ok": true})).into_response()
}

// ---------- /phone commands ----------

/// `/phone <op> [text]` typed at the desk: `{session, pane, op, text?}` → `{ok, text}` (shown in the terminal).
/// ops: `send <text>` (a surface on the phone), `mute|unmute|pin|unpin` (this agent),
/// `away|back` (presence override: away = push everything, even for the focused pane).
async fn api_cmd(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let session = session_of(&b);
    let pane = pane_of(&app, &b);
    let op = b["op"].as_str().unwrap_or("").to_lowercase();
    let text = b["text"].as_str().unwrap_or("").trim().to_string();
    let reply = |ok: bool, msg: String| Json(json!({"ok": ok, "text": msg})).into_response();
    match op.as_str() {
        "send" => {
            if text.is_empty() {
                return reply(false, "Usage: /phone send <text>".into());
            }
            let first = text.lines().next().unwrap_or("").trim();
            let (title, long) = cut(first, 80);
            let mut s = json!({"title": if long { format!("{title}…") } else { title }, "level": "attention"});
            if long || text.contains('\n') {
                s["body"] = json!(text);
            }
            if let Some(x) = &session {
                s["session"] = json!(x);
            }
            if let Some(p) = &pane {
                s["pane"] = json!(p);
            }
            let r = crate::api_surface(State(app.clone()), Json(s)).await;
            let ok = r.status().is_success();
            reply(ok, if ok { "Sent to your phone.".into() } else { format!("Could not surface it (HTTP {}).", r.status().as_u16()) })
        }
        "mute" | "unmute" | "pin" | "unpin" => {
            let Some(sid) = session else { return reply(false, "No session id.".into()) };
            let on = !op.starts_with("un");
            let (pinned, muted) = if op.ends_with("pin") { (Some(on), None) } else { (None, Some(on)) };
            app.prefs.lock().unwrap().set_flags(&sid, pinned, muted);
            app.refresh.notify_one();
            let what = match op.as_str() {
                "mute" => "Muted: no done pushes for this agent.",
                "unmute" => "Unmuted.",
                "pin" => "Pinned to the top of the phone's list.",
                _ => "Unpinned.",
            };
            reply(true, what.into())
        }
        "away" => {
            let until = now_ms() + AWAY_MAX_MS;
            app.talk.inner.lock().unwrap().away_until = until;
            reply(true, "Away: the phone gets every push (also for the focused pane), long answers get a TL;DR and permission prompts go to the phone. `/phone back` ends it (12 h max).".into())
        }
        "back" => {
            app.talk.inner.lock().unwrap().away_until = 0;
            reply(true, "Back: pushes are held again while you're at the desk.".into())
        }
        _ => {
            let away = forced_away(&app);
            let (pinned, muted) = session.as_deref().map(|s| app.prefs.lock().unwrap().flags(s)).unwrap_or((false, false));
            reply(
                op.is_empty() || op == "status" || op == "help",
                format!(
                    "/phone send <text> · mute · unmute · pin · unpin · away · back\nThis agent: {}{} · presence: {}",
                    if pinned { "pinned" } else { "not pinned" },
                    if muted { ", muted" } else { "" },
                    if away { "away (forced)" } else { "auto" }
                ),
            )
        }
    }
}

// ---------- permissions ----------

/// From the mod: `{session, pane, kind:"ask"|"denied", tool, input, always?, reason?}`.
/// - `ask` (a permission dialog opened; the terminal shows it too): `{ok, id}`; the agent shows as
///   blocked with Allow / Deny (/ Allow always) until either side answers.
/// - `denied` (auto mode refused a call): `{ok, id}`; a card offers "Allow once".
async fn api_perm(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let Some(session) = session_of(&b) else { return json_err(StatusCode::BAD_REQUEST, "session required") };
    let kind: &'static str = if b["kind"] == "denied" { "denied" } else { "ask" };
    let tool = b["tool"].as_str().unwrap_or("").to_string();
    if tool.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "tool required");
    }
    // their permission request is their own dialog (the question / plan cards answer it)
    if matches!(tool.as_str(), "AskUserQuestion" | "ExitPlanMode" | "EnterPlanMode") {
        return Json(json!({"ok": true, "skipped": true})).into_response();
    }
    let pane = pane_of(&app, &b);
    let input = b["input"].clone();
    let mut summary = tool_summary(&tool, &input);
    if summary.is_empty() {
        summary = cut(&input.to_string(), 300).0;
    }
    let reason = b["reason"].as_str().map(str::trim).filter(|r| !r.is_empty()).map(|r| cut(r, 400).0);
    let id = app.talk.next_id("p");
    let perm = Perm {
        session: session.clone(),
        pane: pane.clone(),
        kind,
        tool: tool.clone(),
        summary: summary.clone(),
        input: input.clone(),
        reason,
        always: b["always"].as_bool().unwrap_or(false),
        ts: now_ms(),
        decision: None,
    };
    {
        let mut t = app.talk.inner.lock().unwrap();
        if kind == "denied" {
            // the same call denied again replaces the older card
            t.perms.retain(|_, p| !(p.kind == "denied" && p.session == session && p.tool == tool && p.input == input));
        }
        t.perms.insert(id.clone(), perm);
    }
    app.talk.bump();
    app.refresh.notify_one();
    // An `ask` needs no push of its own: the terminal dialog opens at the same time, herdr reports the
    // agent blocked and the usual "Needs you" push goes out (the phone card answers it either way).
    if let Some(pane) = pane.filter(|p| kind == "denied" && !watching(&app, p)) {
        let muted = app.prefs.lock().unwrap().flags(&session).1;
        if !muted && !desk_present(&app).await {
            let who = title_of(&app, &pane);
            let payload = json!({"title": format!("Blocked {tool} · {who}"), "body": format!("{} — allow once?", cut(&summary, 240).0),
                   "url": format!("/#/a/{pane}"), "tag": format!("deny-{pane}")});
            send_push(&app, &payload, true).await;
        }
    }
    Json(json!({"ok": true, "id": id})).into_response()
}

#[derive(Deserialize)]
struct WaitQ {
    wait: Option<u64>,
}

/// The mod waits for the phone's answer to an `ask`: `{state:"decided", decision:"allow"|"always"|"deny"}`
/// | `{state:"pending"}`; 404 `{state:"gone"}` (dropped: answered at the desk, expired, restarted).
async fn api_perm_wait(State(app): State<Shared>, AxPath(id): AxPath<String>, Query(q): Query<WaitQ>) -> Response {
    let talk = &app.talk;
    let check = || {
        let t = talk.inner.lock().unwrap();
        match t.perms.get(&id) {
            None => Some(None),
            Some(p) => p.decision.clone().map(Some),
        }
    };
    match talk.wait_for(q.wait.unwrap_or(PERM_HOLD_MS).min(PERM_HOLD_MS), check).await {
        None => Json(json!({"state": "pending"})).into_response(),
        Some(None) => (StatusCode::NOT_FOUND, Json(json!({"state": "gone"}))).into_response(),
        Some(Some(d)) => {
            talk.inner.lock().unwrap().perms.remove(&id);
            app.refresh.notify_one();
            Json(json!({"state": "decided", "decision": d})).into_response()
        }
    }
}

/// The mod stopped waiting (timeout, interrupt, the dialog was answered elsewhere).
async fn api_perm_cancel(State(app): State<Shared>, AxPath(id): AxPath<String>) -> Response {
    app.talk.inner.lock().unwrap().perms.remove(&id);
    app.talk.bump();
    app.refresh.notify_one();
    Json(json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct DecideBody {
    id: String,
    /// ask: allow | always | deny; denied: allow (once) | dismiss
    decision: String,
}

/// The phone answers: `{id, decision}` → `{ok}`. For an auto-mode denial, `allow` lets that exact
/// call through once (inbox `allow_once`: the mod allows the matching call and tells the agent to
/// retry). 404 unknown/answered, 409 `no_mod`.
async fn api_perm_decide(State(app): State<Shared>, AxPath(pane): AxPath<String>, Json(b): Json<DecideBody>) -> Response {
    let p = app.talk.inner.lock().unwrap().perms.get(&b.id).filter(|p| p.decision.is_none()).cloned();
    let Some(p) = p else { return json_err(StatusCode::NOT_FOUND, "no_such_prompt") };
    let d = b.decision.as_str();
    let ok = match p.kind {
        "ask" => d == "allow" || d == "deny" || (d == "always" && p.always),
        _ => d == "allow" || d == "dismiss",
    };
    if !ok {
        return json_err(StatusCode::BAD_REQUEST, "bad decision");
    }
    if p.kind == "denied" && d == "allow" {
        let bridge = &app.bridge;
        let session = bridge.session_for(&app, &pane).filter(|s| bridge.live(s)).or_else(|| Some(p.session.clone()).filter(|s| bridge.live(s)));
        let Some(session) = session else { return json_err(StatusCode::CONFLICT, "no_mod") };
        bridge.enqueue(&session, json!({"kind": "allow_once", "tool": p.tool, "input": p.input, "summary": p.summary}));
    }
    if let Some(x) = app.talk.inner.lock().unwrap().perms.get_mut(&b.id) {
        x.decision = Some(d.to_string());
    }
    app.talk.bump();
    app.refresh.notify_one();
    Json(json!({"ok": true})).into_response()
}

// ---------- AskUserQuestion answered from the phone ----------

#[derive(Deserialize)]
struct AskAnswer {
    /// option labels (single-select: the first one; multiSelect: every one to tick)
    #[serde(default)]
    labels: Vec<String>,
    /// a free-text answer ("Type something"); with multiSelect it is ticked as well
    text: Option<String>,
}

#[derive(Deserialize)]
struct AskAnswerBody {
    answers: Vec<AskAnswer>,
}

/// A menu row's label without its checkbox, trailing dot and the preview box drawn to its right
/// ("[✔] red" → "red", "Type something." → "Type something", "1. A      ┌──┐" → "A").
fn opt_label(l: &str) -> String {
    let t = l.trim();
    let t = t.find(['│', '┌', '└', '┐', '┘']).map_or(t, |i| &t[..i]).trim();
    let t = ["[ ]", "[✔]", "[✓]", "[x]", "[X]"].iter().find_map(|p| t.strip_prefix(p)).unwrap_or(t).trim();
    t.trim_end_matches('.').trim().to_string()
}

fn is_checked(l: &str) -> bool {
    let t = l.trim();
    ["[✔]", "[✓]", "[x]", "[X]"].iter().any(|p| t.starts_with(p))
}

/// A screen row shows option `want`: the same text, or its first line when the label wraps (preview layout).
fn label_is(screen: &str, want: &str) -> bool {
    let (s, w) = (norm(screen).to_lowercase(), norm(want).to_lowercase());
    !s.is_empty() && (s == w || (w.starts_with(&s) && s.chars().count() >= 3))
}

/// What Claude's AskUserQuestion dialog shows right now.
#[derive(Debug, Default, PartialEq)]
struct AskView {
    /// `(header, answered)` per question tab ("☐ Input" / "☒ Input"), the closing "✔ Submit" tab left out;
    /// empty for a single question (no tab bar)
    tabs: Vec<(String, bool)>,
    /// the "Review your answers" / "Submit answers" step of several questions
    review: bool,
    /// "⚠ You have not answered all questions" on the review step
    incomplete: bool,
    /// the question text above the menu, wrapped lines joined
    question: String,
    /// the side-by-side preview layout (single-select with `preview`): no "Type something" row, free text
    /// goes in the option's notes ("n")
    preview: bool,
}

fn parse_tabs(l: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(char, String)> = Vec::new();
    for c in l.trim().trim_start_matches('←').trim_end_matches('→').chars() {
        match c {
            '☐' | '☒' | '✔' => out.push((c, String::new())),
            _ => {
                if let Some(x) = out.last_mut() {
                    x.1.push(c)
                }
            }
        }
    }
    out.into_iter().filter(|(m, h)| !(*m == '✔' && h.trim() == "Submit")).map(|(m, h)| (h.trim().to_string(), m != '☐')).collect()
}

fn ask_view(screen: &str) -> AskView {
    let lines: Vec<&str> = screen.lines().collect();
    let mut v = AskView::default();
    let bar = lines.iter().rposition(|l| {
        let t = l.trim();
        t.starts_with('←') && t.ends_with('→') && (t.contains('☐') || t.contains('☒'))
    });
    let from = match bar {
        Some(b) => {
            v.tabs = parse_tabs(lines[b]);
            b + 1
        }
        // one question: the dialog fits in the bottom of the screen
        None => lines.len().saturating_sub(60),
    };
    let rest = &lines[from..];
    v.review = bar.is_some() && rest.iter().take(4).any(|l| l.contains("Review your answers"));
    v.incomplete = v.review && rest.iter().any(|l| l.contains("You have not answered all questions"));
    v.preview = rest.iter().any(|l| l.contains("n to add notes") || l.contains("Notes: "));
    let menu_row = |l: &str| {
        let t = l.trim_start().trim_start_matches('❯').trim_start();
        l.trim_start().starts_with('❯') || t.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    };
    // several questions: the lines between the tab bar and the menu; one question: the bottom of the screen
    let q: Vec<&str> = if bar.is_some() { rest.iter().take_while(|l| !menu_row(l)).copied().collect() } else { rest.to_vec() };
    v.question = norm(&q.iter().map(|l| l.replace('│', " ")).collect::<Vec<_>>().join(" "));
    v
}

/// Which question the dialog is on: `Some(n)` = the review step, `None` = no dialog of these questions.
fn current_q(v: &AskView, questions: &[Value]) -> Option<usize> {
    if v.review {
        return Some(questions.len());
    }
    if v.question.is_empty() {
        return None;
    }
    let on = v.question.to_lowercase();
    questions.iter().position(|q| {
        let want = norm(q["question"].as_str().unwrap_or("")).to_lowercase();
        let head: String = want.chars().take(50).collect();
        !head.is_empty() && (on.contains(&head) || (on.chars().count() >= 20 && want.starts_with(&on)))
    })
}

const DIALOG_GONE: &str = "the question is no longer open in the terminal";

/// Selects menu row `t` (up/down against the live screen) once `ok` accepts that row's label; returns the label.
async fn select_row(pane: &str, t: usize, ok: &(dyn Fn(&str) -> bool + Sync)) -> Result<String, String> {
    for _ in 0..24 {
        let menu = crate::parse_menu(&crate::read_screen(pane).await);
        if menu.len() < 2 {
            return Err(DIALOG_GONE.into());
        }
        let Some(row) = menu.get(t) else { return Err("the options on screen do not match the question".into()) };
        if !ok(&opt_label(&row.label)) {
            return Err(format!("the options on screen do not match the question (row {} is “{}”)", t + 1, opt_label(&row.label)));
        }
        let cur = menu.iter().position(|o| o.selected).unwrap_or(0);
        if cur == t {
            return Ok(row.label.clone());
        }
        keys(pane, json!([if t > cur { "down" } else { "up" }])).await;
        tokio::time::sleep(Duration::from_millis(160)).await;
    }
    Err("the menu did not respond".into())
}

/// Waits (≤ 3 s) until the screen no longer shows `prev`.
async fn screen_moves(pane: &str, prev: &str) -> String {
    for _ in 0..15 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let s = crate::read_screen(pane).await;
        if s != prev {
            return s;
        }
    }
    crate::read_screen(pane).await
}

/// Brings the dialog to question `target` (`questions.len()` = the review step) with ← / →, wherever it is now:
/// the terminal may have been left on any question (a half-done earlier attempt, a tap at the desk). The arrows
/// switch tabs only from a plain option row (on the text row they move the cursor, on Next nothing happens), so
/// the selection goes to the first row first.
async fn goto_q(pane: &str, questions: &[Value], target: usize) -> Result<String, String> {
    let n = questions.len();
    for _ in 0..(3 * n + 8) {
        let s = crate::read_screen(pane).await;
        let v = ask_view(&s);
        let Some(cur) = current_q(&v, questions) else { return Err(DIALOG_GONE.into()) };
        if cur == target {
            return Ok(s);
        }
        if n == 1 || v.tabs.is_empty() {
            return Err(DIALOG_GONE.into());
        }
        let menu = crate::parse_menu(&s);
        if cur < n && menu.iter().position(|o| o.selected).unwrap_or(0) != 0 {
            select_row(pane, 0, &|_| true).await?;
            continue;
        }
        keys(pane, json!([if target > cur { "right" } else { "left" }])).await;
        screen_moves(pane, &s).await;
    }
    Err(format!("could not reach question {} in the terminal", target + 1))
}

/// Types `t` into the free-text field that is open now (the "Type something" row or the notes), replacing any
/// text an earlier attempt left there.
async fn type_text(pane: &str, t: &str) {
    keys(pane, json!(["ctrl+u"])).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let _ = herdr::quick("pane.send_text", json!({"pane_id": pane, "text": t})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Answers question `k` (the dialog is on it). Single-select: the option (or "Type something" + text) and Enter;
/// the preview layout has no text row, so text goes in the notes ("n", text, Enter: "(no option selected) notes: …").
/// multiSelect: every option ticked or unticked to match (a retry starts from whatever is ticked), the text row
/// filled or emptied, then its "Next" / "Submit" row. Enter goes only to rows checked against the question, never
/// to "Chat about this".
async fn answer_one(pane: &str, q: &Value, a: &AskAnswer, k: usize, preview: bool) -> Result<(), String> {
    let opts: Vec<String> = q["options"].as_array().map(|o| o.iter().map(|o| o["label"].as_str().unwrap_or("").to_string()).collect()).unwrap_or_default();
    let no = opts.len();
    // one line: a newline would submit the inline input early
    let flat = a.text.as_deref().map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "));
    let text = flat.as_deref().filter(|t| !t.is_empty());
    let idx = |l: &str| opts.iter().position(|o| norm(o).eq_ignore_ascii_case(&norm(l)));
    if q["multiSelect"].as_bool() == Some(true) {
        for (i, o) in opts.iter().enumerate() {
            let want = a.labels.iter().any(|l| idx(l) == Some(i));
            let mut done = false;
            for _ in 0..3 {
                let menu = crate::parse_menu(&crate::read_screen(pane).await);
                let Some(row) = menu.get(i) else { return Err(DIALOG_GONE.into()) };
                if is_checked(&row.label) == want {
                    done = true;
                    break;
                }
                select_row(pane, i, &|l| label_is(l, o)).await?;
                let before = crate::read_screen(pane).await;
                keys(pane, json!(["enter"])).await;
                screen_moves(pane, &before).await;
            }
            if !done {
                return Err(format!("could not {} “{o}”", if want { "tick" } else { "untick" }));
            }
        }
        let menu = crate::parse_menu(&crate::read_screen(pane).await);
        let row = menu.get(no).map(|r| r.label.clone()).unwrap_or_default();
        let has = is_checked(&row).then(|| opt_label(&row));
        if has.as_deref().map(norm) != text.map(norm) {
            select_row(pane, no, &|_| true).await?;
            match text {
                Some(t) => type_text(pane, t).await,
                None => {
                    keys(pane, json!(["ctrl+u"])).await;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
        // "Next" while more questions follow, "Submit" on the last one
        select_row(pane, no + 1, &|l| l == "Next" || l == "Submit").await?;
    } else if let Some(t) = text {
        if preview {
            select_row(pane, 0, &|l| label_is(l, &opts[0])).await?;
            let before = crate::read_screen(pane).await;
            keys(pane, json!(["n"])).await;
            let s = screen_moves(pane, &before).await;
            if s.contains("press n to add notes") {
                return Err(format!("could not open the notes of question {}", k + 1));
            }
            type_text(pane, t).await;
        } else {
            select_row(pane, no, &|l| l.eq_ignore_ascii_case("type something") || norm(l) == norm(t) || !opts.iter().any(|o| label_is(l, o))).await?;
            type_text(pane, t).await;
        }
    } else {
        let Some(i) = a.labels.first().and_then(|l| idx(l)) else {
            return Err(format!("no answer for question {}", k + 1));
        };
        select_row(pane, i, &|l| label_is(l, &opts[i])).await?;
    }
    let before = crate::read_screen(pane).await;
    keys(pane, json!(["enter"])).await;
    screen_moves(pane, &before).await;
    Ok(())
}

/// Answers the AskUserQuestion dialog on screen the way a person would, checking the screen after every step:
/// each question is reached by its tab (← / →) wherever the dialog was left, answered, and confirmed answered in
/// the tab bar (☒), with one retry; several questions end on the review step → "Submit answers". On any mismatch
/// it stops with a reason and leaves the dialog open: never Esc, never "Chat about this" (both end the tool).
async fn answer_ask(pane: &str, questions: &[Value], answers: &[AskAnswer]) -> Result<(), String> {
    let n = questions.len();
    for (k, (q, a)) in questions.iter().zip(answers).enumerate() {
        let mut tries = 0;
        loop {
            tries += 1;
            let s = goto_q(pane, questions, k).await?;
            answer_one(pane, q, a, k, ask_view(&s).preview).await?;
            if n == 1 {
                return Ok(());
            }
            let v = ask_view(&crate::read_screen(pane).await);
            if v.tabs.get(k).is_some_and(|t| t.1) {
                break;
            }
            if v.tabs.is_empty() && current_q(&v, questions).is_none() {
                return Err(DIALOG_GONE.into());
            }
            if tries >= 2 {
                return Err(format!("question {} did not take the answer", k + 1));
            }
        }
    }
    let s = goto_q(pane, questions, n).await?;
    let v = ask_view(&s);
    if v.incomplete {
        let left: Vec<String> = v.tabs.iter().filter(|t| !t.1).map(|t| t.0.clone()).collect();
        return Err(format!("the terminal still lists unanswered questions ({})", left.join(", ")));
    }
    let menu = crate::parse_menu(&s);
    let Some(i) = menu.iter().position(|o| opt_label(&o.label) == "Submit answers") else {
        return Err("no “Submit answers” in the terminal".into());
    };
    select_row(pane, i, &|l| l == "Submit answers").await?;
    let before = crate::read_screen(pane).await;
    keys(pane, json!(["enter"])).await;
    let after = screen_moves(pane, &before).await;
    if ask_view(&after).review {
        return Err("“Submit answers” did not go through".into());
    }
    Ok(())
}

/// `POST /api/a/:pane/askanswer {answers:[{labels:[…], text?}]}` — one entry per question of the open
/// AskUserQuestion (`agents[].ask.questions`, in order) → `{ok}`. 409 `{error}` if no question is open
/// or the dialog does not match.
async fn api_askanswer(State(app): State<Shared>, AxPath(pane): AxPath<String>, Json(b): Json<AskAnswerBody>) -> Response {
    let questions: Vec<Value> = {
        let s = app.inner.lock().unwrap().state.clone();
        let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
        v["agents"]
            .as_array()
            .and_then(|l| l.iter().find(|a| a["pane"] == pane))
            .filter(|a| a["ask"]["phone"] != true)
            .and_then(|a| a["ask"]["questions"].as_array().cloned())
            .unwrap_or_default()
    };
    if questions.is_empty() {
        return json_err(StatusCode::CONFLICT, "no_question");
    }
    if b.answers.len() != questions.len() {
        return json_err(StatusCode::BAD_REQUEST, format!("{} answers for {} questions", b.answers.len(), questions.len()));
    }
    let lock = app.bridge.pane_lock(&pane);
    let _one = lock.lock().await;
    match answer_ask(&pane, &questions, &b.answers).await {
        Ok(()) => {
            app.refresh.notify_one();
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => json_err(StatusCode::CONFLICT, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_labels() {
        assert_eq!(opt_label("[✔] red"), "red");
        assert_eq!(opt_label("[ ] Type something"), "Type something");
        assert_eq!(opt_label("Type something."), "Type something");
        assert_eq!(opt_label("  Submit"), "Submit");
        assert!(is_checked("[✔] teal") && !is_checked("[ ] blue"));
    }

    const BAR: &str = "←  ☒ Input  ☒ Panel  ☐ Format  ☐ Delivery  ✔ Submit  →";

    fn sample_qs() -> Vec<Value> {
        let o = |l: &[&str]| l.iter().map(|l| json!({"label": l})).collect::<Vec<_>>();
        vec![
            json!({"header": "Input", "multiSelect": true, "question": "What should the review cover? The old script read your \"weekly report\" files, but the last one is from June.",
                   "options": o(&["My last 7 days of commits", "Whatever I paste or point at", "Open issues / milestones", "Old reports"])}),
            json!({"header": "Panel", "multiSelect": false, "question": "Which reviewers should weigh in?", "options": o(&["Keep the usual four", "Swap in specialists", "I'll name them"])}),
            json!({"header": "Format", "multiSelect": false, "question": "What should the bullet output look like?", "options": o(&["3 bullets each + shared", "One flat list", "Bullets + a short monologue each"])}),
            json!({"header": "Delivery", "multiSelect": true, "question": "Where does it run and get saved?", "options": o(&["Save to the repo", "Card on my phone", "Weekly automatic", "Install as a Claude skill here"])}),
        ]
    }

    // screens captured from Claude Code 2.1.289 (herdr pane read), the 2026-10-07 4-question ask
    fn q1_screen(width_wrap: bool) -> String {
        let q = if width_wrap {
            "│ What should the review cover? The old script read your \"weekly report\"\n│ files, but the last one is from June."
        } else {
            "│ What should the review cover? The old script read your \"weekly report\" files, but the last one is from June."
        };
        format!("❯ earlier prompt\n────\n←  ☐ Input  ☐ Panel  ☐ Format  ☐ Delivery  ✔ Submit  →\n\n{q}\n\n  1. [ ] My last 7 days of commits\n         Git commits, agent sessions.\n  2. [✔] Whatever I paste or point at\n         A plan, an idea.\n  3. [ ] Open issues / milestones\n         Judge against milestones.\n  4. [ ] Old reports\n         Keep reading old reports like before.\n❯ 5. [✔] my own idea\n     Next\n──────────────\n  6. Chat about this\n\nEnter to select · Tab/Arrow keys to navigate · Esc to cancel\n")
    }

    fn q3_preview_screen() -> String {
        format!("{BAR}\n\nWhat should the bullet output look like?\n\n❯ 1. 3 bullets each + shared      ┌────────────────┐\n  2. One flat list                │ Alice 2/5      │\n  3. Bullets + a short            │ - Shipped: x   │\n    monologue each                │                │\n                                  └────────────────┘\n\n                                  Notes: press n to add notes\n\n──────────────\n  Chat about this\n\nEnter to select · ↑/↓ to navigate · n to add notes · Tab to switch questions · Esc to cancel\n")
    }

    #[test]
    fn ask_tabs_and_preview_layout() {
        let v = ask_view(&q3_preview_screen());
        let tabs: Vec<(&str, bool)> = v.tabs.iter().map(|(h, a)| (h.as_str(), *a)).collect();
        assert_eq!(tabs, [("Input", true), ("Panel", true), ("Format", false), ("Delivery", false)]);
        assert!(v.preview && !v.review);
        assert_eq!(v.question, "What should the bullet output look like?");
        assert_eq!(current_q(&v, &sample_qs()), Some(2)); // the dialog was left on question 3: found, not "question 1 is not on screen"
        // the preview box is not part of the labels; a wrapped label matches its option by its first line
        let menu = crate::parse_menu(&q3_preview_screen());
        let labels: Vec<String> = menu.iter().map(|o| opt_label(&o.label)).collect();
        assert_eq!(labels, ["3 bullets each + shared", "One flat list", "Bullets + a short"]);
        assert!(label_is(&labels[2], "Bullets + a short monologue each") && !label_is(&labels[1], "Bullets + a short monologue each"));
        assert!(menu[0].selected);
    }

    #[test]
    fn ask_multiselect_rows() {
        let qs = sample_qs();
        for wrap in [false, true] {
            let s = q1_screen(wrap);
            let v = ask_view(&s);
            assert!(!v.preview && !v.review);
            assert_eq!(current_q(&v, &qs), Some(0), "wrapped question: {wrap}");
            let menu = crate::parse_menu(&s);
            // options 0..4, the text row at 4 (ticked: it holds text), Next at 5, then "Chat about this" (never pressed)
            assert_eq!(menu.len(), 7);
            assert!(is_checked(&menu[1].label) && !is_checked(&menu[0].label));
            assert_eq!(opt_label(&menu[4].label), "my own idea");
            assert!(menu[4].selected && is_checked(&menu[4].label));
            assert_eq!(opt_label(&menu[5].label), "Next");
            assert_eq!(opt_label(&menu[6].label), "Chat about this");
        }
    }

    #[test]
    fn ask_single_select_and_review() {
        let s = format!("{BAR}\n\nWhich reviewers should weigh in?\n\n❯ 1. Keep the usual four\n     Same four.\n  2. Swap in specialists\n     Mix in people.\n  3. I'll name them\n     Tell me who.\n  4. Type something.\n──────────────\n  5. Chat about this\n\nEnter to select · Tab/Arrow keys to navigate · Esc to cancel\n");
        let v = ask_view(&s);
        assert_eq!(current_q(&v, &sample_qs()), Some(1));
        let menu = crate::parse_menu(&s);
        assert_eq!(opt_label(&menu[3].label), "Type something");
        let r = format!("{BAR}\n\nReview your answers\n\n⚠ You have not answered all questions\n\n ● What should the review cover?\n   → My last 7 days of commits\n\nReady to submit your answers?\n\n❯ 1. Submit answers\n  2. Cancel\n");
        let v = ask_view(&r);
        assert!(v.review && v.incomplete);
        assert_eq!(current_q(&v, &sample_qs()), Some(4));
        assert_eq!(opt_label(&crate::parse_menu(&r)[0].label), "Submit answers");
        // no dialog: nothing matches
        assert_eq!(current_q(&ask_view("❯ hello\n  ⏸ manual mode on"), &sample_qs()), None);
    }

    #[test]
    fn ask_single_question_has_no_tabs() {
        let s = "❯ Test: call AskUserQuestion\n\n● Reading\n────\n Fruit\n\nPick fruits\n\n❯ 1. [ ] apple\n  2. [ ] banana\n  3. [ ] Type something\n     Submit\n────\n  4. Chat about this\n";
        let qs = vec![json!({"question": "Pick fruits", "multiSelect": true, "options": [{"label": "apple"}, {"label": "banana"}]})];
        let v = ask_view(s);
        assert!(v.tabs.is_empty() && !v.review);
        assert_eq!(current_q(&v, &qs), Some(0));
    }

    #[test]
    fn norm_collapses_space() {
        assert_eq!(norm("  a \n b\tc "), "a b c");
    }

    #[tokio::test]
    async fn wait_for_wakes_on_bump() {
        let t = std::sync::Arc::new(Talk::default());
        let t2 = t.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            t2.inner.lock().unwrap().away_until = 5;
            t2.bump();
        });
        let t0 = std::time::Instant::now();
        let got = t.wait_for(2000, || (t.inner.lock().unwrap().away_until == 5).then_some(())).await;
        assert!(got.is_some());
        assert!(t0.elapsed() < Duration::from_millis(1000));
    }

    #[test]
    fn ids_are_unique() {
        let t = Talk::default();
        assert_ne!(t.next_id("p"), t.next_id("p"));
    }
}
