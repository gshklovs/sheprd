//! Catch up (API.md "Catch up"): the agents whose last message waits on the user, one card at a time, reachable from Home
//! without going through Tidy. Reuses Tidy's per-turn question verdict and its cache (tidy.rs `QInfo`, one Haiku call
//! per finished turn under Tidy's daily cap; the heuristic until it answers) and Tidy's sleep (undo window, tidy-log).
//!
//! **One place for a question**: an agent with a decision still waiting in the Decide deck (open, later, or stale
//! awaiting Clear / Keep) belongs to Decide and is left out here, until that decision closes.
//!
//! - `GET /api/catchup` → `{n, rows, decide, now}`: rows like Tidy's (`pane, sid, name, color, cwd, idleMs, last, full,
//!   ctx, reasons:[{kind:"q", text, fix:{kind:"answer", reply}}]`) plus `said` (the user's last prompt, `tidy::said_json`),
//!   `wait` (Haiku's "Waiting on you for" line) and `key`; `decide` = the agents left out for an open decision.
//! - `GET /api/catchup/said?sids=a,b` → `{said: {sid: said}, wait: {sid: line}}` for asleep agents (the wake card,
//!   "Just put to sleep").
//! - `POST /api/catchup/send {sid, pane?, text, kind: answer|continue}`: awake → the steer path (held on a draft or a
//!   menu); asleep → the duplicate-safe resume with that text. No commitment to sleep (that is Tidy's).
//! - `POST /api/catchup/keep {pane}`: leave it out until it asks something new.

use crate::transcript::now_ms;
use crate::{Shared, json_err, tidy};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/catchup", get(api_list))
        .route("/api/catchup/said", get(api_said))
        .route("/api/catchup/send", post(api_send))
        .route("/api/catchup/keep", post(api_keep))
}

static CACHE: Mutex<Option<(i64, Value)>> = Mutex::new(None);

fn forget() {
    *CACHE.lock().unwrap() = None;
}

fn s(a: &Value, k: &str) -> String {
    a[k].as_str().unwrap_or("").to_string()
}

/// Sessions and panes with a decision still waiting in the Decide deck → its id.
fn open_decisions(list: &Value) -> (HashMap<String, String>, HashMap<String, String>) {
    let (mut sids, mut panes) = (HashMap::new(), HashMap::new());
    for k in ["open", "later", "stale"] {
        for r in list[k].as_array().into_iter().flatten() {
            let id = s(r, "id");
            if let Some(x) = r["session"].as_str().filter(|x| !x.is_empty()) {
                sids.entry(x.to_string()).or_insert(id);
            } else if let Some(x) = r["pane"].as_str().filter(|x| !x.is_empty()) {
                panes.entry(x.to_string()).or_insert(id); // a card without a session: by its pane
            }
        }
    }
    (sids, panes)
}

/// Does this agent's open question belong to the Decide deck? A decision from its session, else (no session on the
/// card) from its pane.
pub fn decided_by(a: &Value, sids: &HashMap<String, String>, panes: &HashMap<String, String>) -> Option<String> {
    sids.get(&s(a, "session")).or_else(|| panes.get(&s(a, "pane"))).cloned()
}

async fn build(app: &Shared) -> Value {
    let agents = tidy::live(app);
    let now = now_ms();
    let busy = tidy::busy_panes();
    let dl = {
        let inner = app.inner.lock().unwrap();
        crate::decide::list(&inner.surfaces, now)
    };
    let (dsids, dpanes) = open_decisions(&dl);
    let (mut rows, mut decide) = (vec![], vec![]);
    for a in agents.iter().filter(|a| a["kind"] == "claude" && a["session"].is_string()) {
        let (pane, sid, cwd) = (s(a, "pane"), s(a, "session"), s(a, "cwd"));
        if busy.contains(&pane)
            || tidy::guard(a).is_some()
            || !matches!(a["status"].as_str(), Some("idle") | Some("done"))
            || a["last"]["role"] != "assistant"
            || a["queued"].as_array().is_some_and(|q| !q.is_empty())
            || !crate::inputbox::held_for(&pane).is_empty()
        {
            continue;
        }
        let key = tidy::q_key(a);
        let cached = tidy::with(|st| st.q.get(&sid).filter(|q| q.key == key).cloned());
        let (app2, sid2, cwd2) = (app.clone(), sid.clone(), cwd.clone());
        let sc = tokio::task::spawn_blocking(move || tidy::scan_for(&app2, &sid2, &cwd2)).await.unwrap_or_default();
        let full = sc.asst.last().cloned().or_else(|| a["last"]["text"].as_str().map(str::to_string));
        let q = match cached {
            Some(q) => q,
            None => {
                let guess = full.as_deref().is_some_and(tidy::asks_guess);
                if guess {
                    tidy::q_later(app, sid.clone(), key.clone(), full.clone().unwrap_or_default());
                }
                tidy::QInfo { key: key.clone(), asks: guess, by: "guess".into(), ..Default::default() }
            }
        };
        if !q.asks || tidy::with(|st| st.ckeep.get(&sid) == Some(&key)) {
            continue;
        }
        let name = tidy::name_of(a);
        if let Some(id) = decided_by(a, &dsids, &dpanes) {
            decide.push(json!({"sid": sid, "pane": pane, "name": name, "id": id}));
            continue;
        }
        let last_ts = a["last"]["ts"].as_i64().unwrap_or(0).max(a["since"].as_i64().unwrap_or(0));
        let reason = json!({"kind": "q", "text": q.chip.clone().unwrap_or_else(|| "Asks you a question".into()), "fix": {"kind": "answer", "reply": q.reply}});
        let mut r = tidy::row(a, if last_ts > 0 { now - last_ts } else { 0 }, vec![reason], full.as_deref());
        r["said"] = tidy::said_json(&sc);
        r["wait"] = json!(q.wait);
        r["key"] = json!(key);
        r["by"] = json!(q.by);
        rows.push(r);
    }
    rows.sort_by_key(|r| r["idleMs"].as_i64().unwrap_or(0));
    json!({"n": rows.len(), "rows": rows, "decide": decide, "now": now})
}

async fn api_list(State(app): State<Shared>) -> Response {
    if let Some((at, v)) = CACHE.lock().unwrap().clone() {
        if now_ms() - at < 4_000 {
            return Json(v).into_response();
        }
    }
    let v = build(&app).await;
    *CACHE.lock().unwrap() = Some((now_ms(), v.clone()));
    Json(v).into_response()
}

#[derive(Deserialize)]
struct SaidQ {
    sids: String,
}

/// Haiku's verdict still describes the session's last text: asked for it, or for something written after it.
fn verdict_current(key: &str, asst_ts: i64) -> bool {
    let ts: i64 = key.rsplit(':').next().and_then(|x| x.parse().ok()).unwrap_or(0);
    asst_ts > 0 && ts >= asst_ts - 2_000
}

async fn api_said(State(app): State<Shared>, Query(q): Query<SaidQ>) -> Response {
    let sids: Vec<String> = q.sids.split(',').map(str::trim).filter(|x| crate::valid_sid(x)).take(20).map(str::to_string).collect();
    let (mut said, mut wait) = (serde_json::Map::new(), serde_json::Map::new());
    for sid in sids {
        let cwd = crate::find::place(&sid).and_then(|i| i.cwd).unwrap_or_default();
        let (app2, sid2) = (app.clone(), sid.clone());
        let sc = tokio::task::spawn_blocking(move || tidy::scan_for(&app2, &sid2, &cwd)).await.unwrap_or_default();
        said.insert(sid.clone(), tidy::said_json(&sc));
        let w = tidy::with(|st| st.q.get(&sid).filter(|x| x.asks && verdict_current(&x.key, sc.asst_ts)).and_then(|x| x.wait.clone()));
        wait.insert(sid, json!(w));
    }
    Json(json!({"said": said, "wait": wait})).into_response()
}

#[derive(Deserialize)]
struct SendBody {
    sid: String,
    #[serde(default)]
    pane: Option<String>,
    text: String,
    /// answer | continue
    #[serde(default)]
    kind: Option<String>,
}

async fn api_send(State(app): State<Shared>, h: HeaderMap, Json(b): Json<SendBody>) -> Response {
    let text = b.text.trim().to_string();
    if text.is_empty() || text.len() > 20_000 {
        return json_err(StatusCode::BAD_REQUEST, "text required");
    }
    if !crate::valid_sid(&b.sid) {
        return json_err(StatusCode::BAD_REQUEST, "bad session id");
    }
    let kind = b.kind.as_deref().unwrap_or("answer").to_string();
    let src = tidy::origin(&h, "POST /api/catchup/send");
    // the pane its session runs in now (it may have moved), else asleep
    let pane = crate::pane_for_session(&app, &b.sid).or(b.pane.filter(|p| crate::pane_for_session_rev(&app, p).as_deref() == Some(b.sid.as_str())));
    let (r, how) = match &pane {
        Some(p) => (crate::modbridge::steer(app.clone(), p.clone(), text.clone()).await, "steer"),
        None => (crate::fleet::resume_from(app.clone(), b.sid.clone(), text.clone(), h.clone()).await, "resume"),
    };
    let status = r.status();
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap_or_default();
    let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let via = if v["via"] == "held" { format!("held:{}", v["how"].as_str().unwrap_or("draft")) } else { how.to_string() };
    tidy::log_event("catchup", json!({"kind": kind, "sid": b.sid, "pane": pane, "via": via, "status": status.as_u16(), "src": src}));
    if !status.is_success() {
        return (status, Json(v)).into_response();
    }
    if let (Some(p), "answer") = (&pane, kind.as_str()) {
        crate::asks::record(&app, &h, p, &text, "steer");
    }
    forget();
    tidy::invalidate();
    app.refresh.notify_one();
    Json(json!({"ok": true, "via": via, "held": v["via"] == "held", "pane": pane.or_else(|| v["pane"].as_str().map(str::to_string))})).into_response()
}

#[derive(Deserialize)]
struct PaneBody {
    pane: String,
}

async fn api_keep(State(app): State<Shared>, h: HeaderMap, Json(b): Json<PaneBody>) -> Response {
    let agents = tidy::live(&app);
    let Some(a) = agents.iter().find(|a| a["pane"].as_str() == Some(b.pane.as_str())) else {
        return json_err(StatusCode::NOT_FOUND, "unknown pane");
    };
    let (sid, key) = (s(a, "session"), tidy::q_key(a));
    tidy::with(|st| {
        if st.ckeep.len() > 200 {
            st.ckeep.clear();
        }
        st.ckeep.insert(sid.clone(), key.clone());
    });
    tidy::save();
    tidy::log_event("catchup", json!({"kind": "keep", "sid": sid, "pane": b.pane, "src": tidy::origin(&h, "POST /api/catchup/keep")}));
    forget();
    Json(json!({"ok": true})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_owns_its_agents() {
        let l = json!({"open": [{"id": "s1", "session": "abc", "pane": "w1:p1"}], "later": [{"id": "s2", "session": null, "pane": "w1:p2"}],
                       "stale": [{"id": "s3", "session": "def", "pane": "w1:p3"}], "done": [{"id": "s4", "session": "ghi", "pane": "w1:p4"}]});
        let (sids, panes) = open_decisions(&l);
        let a = |pane: &str, sid: &str| json!({"pane": pane, "session": sid});
        assert_eq!(decided_by(&a("w9:p9", "abc"), &sids, &panes).as_deref(), Some("s1")); // its session, wherever it is now
        assert_eq!(decided_by(&a("w1:p2", "zzz"), &sids, &panes).as_deref(), Some("s2")); // a card without a session: its pane
        assert_eq!(decided_by(&a("w1:p3", "def"), &sids, &panes).as_deref(), Some("s3")); // stale until Clear / Keep
        assert_eq!(decided_by(&a("w1:p4", "ghi"), &sids, &panes), None); // answered: back to catch-up
    }

    #[test]
    fn verdict_for_the_last_text() {
        assert!(verdict_current("4:1000000", 1000500));
        assert!(!verdict_current("4:1000000", 1100000));
        assert!(!verdict_current("4:1000000", 0));
    }
}
