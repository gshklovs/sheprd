//! "My asks": every request you send an agent from the phone becomes a tracked item, so nothing slips
//! ("I asked … what happened", 2026-10-05 usage report). A phone prompt/steer (a browser sent it, no `from`) is
//! recorded unless it's a trivial reply. Its status follows the agent: `sent` → `working` (the agent's turn is
//! running) → `replied` (a turn ended after the ask; the activity outcome or the answer's first line is kept)
//! or `lost` (the agent closed first). You mark `done` / `dropped`, or nudges (re-asks the agent, back to `sent`).
//! Persisted in `<state dir>/asks.json` (500 newest). A watcher re-checks open items every 15 s.
//! Hooks from main.rs / modbridge.rs: `record` (api_prompt, api_steer route), `routes`, `run`.

use crate::transcript::{cut, now_ms};
use crate::{App, Shared, json_err};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

const KEEP: usize = 500;
const LOST_AFTER_MS: i64 = 120_000;
const SHOW_CLOSED_MS: i64 = 7 * 24 * 3600_000;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/asks", get(api_list))
        .route("/api/asks/{id}", post(api_update))
        .route("/api/asks/{id}/nudge", post(api_nudge))
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default)]
struct Ask {
    id: String,
    ts: i64,
    pane: String,
    agent: String,
    session: String,
    text: String,
    verb: String,
    /// sent | working | replied | lost | done | dropped
    status: String,
    status_ts: i64,
    outcome: String,
    answer_i: Option<u64>,
    nudges: u32,
}

static ASKS: LazyLock<Mutex<Vec<Ask>>> = LazyLock::new(|| Mutex::new(load()));

fn file() -> std::path::PathBuf {
    crate::state_dir().join("asks.json")
}
fn load() -> Vec<Ask> {
    std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}
fn save(v: &[Ask]) {
    if let Ok(b) = serde_json::to_vec_pretty(v) {
        let tmp = file().with_extension("json.tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(tmp, file());
        }
    }
}

/// A short acknowledgement, a lone reply chip or a server/agent-made message isn't an ask.
fn is_ask(text: &str) -> bool {
    let t = text.trim();
    if t.chars().count() < 12 || t.starts_with('[') || t.starts_with('/') {
        return false;
    }
    let low = t.to_lowercase();
    let w: Vec<&str> = low.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    const ACK: [&str; 22] = ["yes", "yeah", "yep", "no", "nope", "ok", "okay", "thanks", "thank", "you", "continue", "go", "ahead",
        "do", "it", "please", "sure", "sounds", "good", "great", "perfect", "done"];
    !(w.len() <= 4 && w.iter().all(|x| ACK.contains(x)))
}

fn from_browser(h: &HeaderMap) -> bool {
    h.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).is_some_and(|ua| ua.contains("Mozilla"))
}

fn agent_of(app: &App, pane: &str) -> Option<Value> {
    let s = app.inner.lock().unwrap().state.clone();
    let mut v: Value = serde_json::from_str(&s).ok()?;
    let list = v["agents"].as_array_mut()?;
    let i = list.iter().position(|a| a["pane"] == pane)?;
    Some(list.swap_remove(i))
}

/// Records a phone message to `pane` as an ask (call after it was sent).
pub fn record(app: &App, headers: &HeaderMap, pane: &str, text: &str, verb: &str) {
    if !from_browser(headers) || !is_ask(text) {
        return;
    }
    let a = agent_of(app, pane);
    let now = now_ms();
    let ask = Ask {
        id: format!("k{now:x}"),
        ts: now,
        pane: pane.to_string(),
        agent: a.as_ref().and_then(|a| a["title"].as_str().or(a["name"].as_str())).unwrap_or(pane).to_string(),
        session: a.as_ref().and_then(|a| a["session"].as_str()).unwrap_or("").to_string(),
        text: cut(text.trim(), 600).0,
        verb: verb.to_string(),
        status: "sent".into(),
        status_ts: now,
        ..Default::default()
    };
    let mut v = ASKS.lock().unwrap();
    // an outbox retry of the same message isn't a second ask
    if v.iter().rev().take(20).any(|k| k.pane == ask.pane && k.text == ask.text && now - k.ts < 600_000) {
        return;
    }
    v.push(ask);
    if v.len() > KEEP {
        let n = v.len() - KEEP;
        v.drain(..n);
    }
    save(&v);
}

/// One status step for an open ask given its agent's live entry (None = the pane is gone).
fn step(k: &Ask, a: Option<&Value>, now: i64) -> Option<(String, String, Option<u64>)> {
    let Some(a) = a else {
        return (now - k.status_ts > LOST_AFTER_MS && matches!(k.status.as_str(), "sent" | "working")).then(|| ("lost".into(), String::new(), None));
    };
    let st = a["status"].as_str().unwrap_or("");
    let since = a["since"].as_i64().unwrap_or(0);
    match st {
        "working" | "blocked" if k.status == "sent" => Some(("working".into(), String::new(), None)),
        // a turn ended after the ask (the agent's current idle/done state started after it)
        "idle" | "done" if matches!(k.status.as_str(), "sent" | "working") && since > k.status_ts => {
            let outcome = a["outcome"]["text"]
                .as_str()
                .filter(|_| a["outcome"]["ts"].as_i64().unwrap_or(0) >= k.status_ts)
                .map(str::to_string)
                .or_else(|| {
                    a["last"]["text"].as_str().map(|t| cut(t.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim(), 200).0)
                })
                .unwrap_or_default();
            Some(("replied".into(), outcome, a["answerI"].as_u64()))
        }
        _ => None,
    }
}

fn refresh(app: &App) -> bool {
    let open: Vec<(usize, String)> = ASKS
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, k)| matches!(k.status.as_str(), "sent" | "working"))
        .map(|(i, k)| (i, k.pane.clone()))
        .collect();
    if open.is_empty() {
        return false;
    }
    let s = app.inner.lock().unwrap().state.clone();
    let st: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
    if st["agents"].as_array().is_none() {
        return true; // no live state yet (herdr down): don't call everything lost
    }
    let now = now_ms();
    let mut v = ASKS.lock().unwrap();
    let mut changed = false;
    for (i, pane) in open {
        let a = st["agents"].as_array().and_then(|l| l.iter().find(|a| a["pane"] == pane.as_str()));
        if let Some(k) = v.get_mut(i).filter(|k| k.pane == pane) {
            if let Some((status, outcome, ai)) = step(k, a, now) {
                k.status = status;
                k.status_ts = now;
                if !outcome.is_empty() {
                    k.outcome = outcome;
                }
                k.answer_i = ai.or(k.answer_i);
                changed = true;
            }
        }
    }
    if changed {
        save(&v);
    }
    true
}

/// Watches open asks (15 s ticks, only while there are any).
pub async fn run(app: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        refresh(&app);
    }
}

#[derive(Deserialize)]
struct ListQ {
    all: Option<u8>,
}

async fn api_list(State(app): State<Shared>, Query(q): Query<ListQ>) -> Response {
    refresh(&app);
    let now = now_ms();
    let v = ASKS.lock().unwrap();
    let items: Vec<&Ask> = v
        .iter()
        .rev()
        .filter(|k| q.all == Some(1) || !matches!(k.status.as_str(), "done" | "dropped") || now - k.status_ts < SHOW_CLOSED_MS)
        .collect();
    let open = v.iter().filter(|k| !matches!(k.status.as_str(), "done" | "dropped")).count();
    let check = v.iter().filter(|k| matches!(k.status.as_str(), "replied" | "lost")).count();
    Json(json!({"asks": items, "open": open, "check": check})).into_response()
}

#[derive(Deserialize)]
struct UpdateBody {
    status: String,
}

async fn api_update(AxPath(id): AxPath<String>, Json(b): Json<UpdateBody>) -> Response {
    if !matches!(b.status.as_str(), "done" | "dropped" | "replied") {
        return json_err(StatusCode::BAD_REQUEST, "status: done | dropped | replied");
    }
    let mut v = ASKS.lock().unwrap();
    let Some(k) = v.iter_mut().find(|k| k.id == id) else { return json_err(StatusCode::NOT_FOUND, "no such ask") };
    k.status = b.status;
    k.status_ts = now_ms();
    let out = json!({"ok": true, "ask": k});
    save(&v);
    Json(out).into_response()
}

/// Re-asks the agent ("where is this?") and puts the ask back to `sent`.
async fn api_nudge(State(app): State<Shared>, AxPath(id): AxPath<String>) -> Response {
    let Some(k) = ASKS.lock().unwrap().iter().find(|k| k.id == id).cloned() else { return json_err(StatusCode::NOT_FOUND, "no such ask") };
    if agent_of(&app, &k.pane).is_none() {
        return json_err(StatusCode::CONFLICT, "that agent is closed — resume it or ask another agent");
    }
    let when = {
        let mins = (now_ms() - k.ts) / 60_000;
        if mins < 90 { format!("{mins} min ago") } else { format!("{} h ago", mins / 60) }
    };
    let text = format!(
        "Following up on what I asked {when}:\n\n> {}\n\nWhere is this? If it's done, say in one line what changed; if not, do it now or tell me what's blocking it.",
        cut(&k.text, 400).0.replace('\n', "\n> ")
    );
    let r = crate::modbridge::steer(app.clone(), k.pane.clone(), text).await;
    if !r.status().is_success() {
        return r;
    }
    let mut v = ASKS.lock().unwrap();
    if let Some(x) = v.iter_mut().find(|x| x.id == id) {
        x.status = "sent".into();
        x.status_ts = now_ms();
        x.nudges += 1;
    }
    save(&v);
    Json(json!({"ok": true})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivial_replies_are_not_asks() {
        for t in ["yes", "ok thanks", "Yes please do it", "continue", "[Note] queued", "/compact", "sounds good!"] {
            assert!(!is_ask(t), "{t}");
        }
        for t in ["Can you make the cart for me again?", "I dont like the gpu usage alert at the top", "yes but move it left please"] {
            assert!(is_ask(t), "{t}");
        }
    }

    #[test]
    fn status_follows_the_agent() {
        let k = Ask { status: "sent".into(), status_ts: 1000, ..Default::default() };
        assert_eq!(step(&k, Some(&json!({"status": "working", "since": 1500})), 2000).unwrap().0, "working");
        // still idle from before the ask: nothing happened yet
        assert!(step(&k, Some(&json!({"status": "idle", "since": 500})), 2000).is_none());
        let k2 = Ask { status: "working".into(), status_ts: 1000, ..Default::default() };
        let a = json!({"status": "done", "since": 3000, "answerI": 42, "outcome": {"text": "moved the alert below the header", "ts": 3000}, "last": {"text": "Done."}});
        assert_eq!(step(&k2, Some(&a), 4000), Some(("replied".into(), "moved the alert below the header".into(), Some(42))));
        let old_outcome = json!({"status": "idle", "since": 3000, "outcome": {"text": "earlier turn", "ts": 10}, "last": {"text": "\nFixed the cart link.\nMore"}});
        assert_eq!(step(&k2, Some(&old_outcome), 4000).unwrap().1, "Fixed the cart link.");
        assert!(step(&k2, None, 2000).is_none());
        assert_eq!(step(&k2, None, 1000 + LOST_AFTER_MS + 1).unwrap().0, "lost");
    }
}
