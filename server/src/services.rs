//! Background services: agents that run unattended (pollers, watchers, nightly jobs) and say so.
//!
//! A service opts in by reporting itself from its mod, at no model cost:
//! `POST /api/mod/service {pane | session, name, icon?, verb?, status, lastRun?, everyMs?, lastActive?,
//! lastError?: {ts, msg}, error?}` (API.md "Background services"). The server keeps the latest report per pane
//! (persisted in `<state>/services.json`, so a restart does not turn a service back into a plain agent) and:
//! - adds it to the agent in `/api/state` as `service` (+ `stale` when reports stopped);
//! - pins the agent's `since` to its last real activity, so its own polling never counts as activity;
//! - never sends "Done" pushes for it; a new `error` (the service is failing, not a one-off hiccup) pushes once.

use crate::transcript::now_ms;
use crate::{Shared, json_err, send_push, state_dir};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;

/// Reports older than this are dropped when their pane is gone.
const KEEP_MS: i64 = 24 * 3600_000;
/// A service counts as stale after missing this many report intervals (at least `STALE_MIN_MS`).
const STALE_INTERVALS: i64 = 3;
const STALE_MIN_MS: i64 = 10 * 60_000;

pub struct Services {
    by_pane: Mutex<HashMap<String, Value>>,
    /// false in tests
    save: bool,
}

impl Default for Services {
    fn default() -> Self {
        let m = std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Services { by_pane: Mutex::new(m), save: true }
    }
}

fn path() -> std::path::PathBuf {
    state_dir().join("services.json")
}

impl Services {
    pub fn is_service(&self, pane: &str) -> bool {
        self.by_pane.lock().unwrap().contains_key(pane)
    }

    /// Stores a report; returns the error to push when it is new (none before, or a different one).
    fn report(&self, pane: &str, mut svc: Value, now: i64) -> Option<String> {
        let mut m = self.by_pane.lock().unwrap();
        let prev = m.get(pane);
        let prev_err = prev.and_then(|p| p["error"].as_str()).unwrap_or("").to_string();
        svc["ts"] = json!(now);
        svc["since"] = prev.and_then(|p| p["since"].as_i64()).map_or(json!(now), |s| json!(s));
        let err = svc["error"].as_str().unwrap_or("").to_string();
        // a push goes out when the service starts failing; it stays quiet while the same failure continues
        let pushed = prev.and_then(|p| p["errorPushed"].as_str()).unwrap_or("").to_string();
        let new_err = !err.is_empty() && prev_err.is_empty() && pushed != err;
        svc["errorPushed"] = json!(if err.is_empty() { "" } else if new_err { &err } else { &pushed });
        m.insert(pane.to_string(), svc);
        if self.save {
            let _ = std::fs::write(path(), serde_json::to_vec(&*m).unwrap_or_default());
        }
        new_err.then_some(err)
    }
}

pub fn routes() -> Router<Shared> {
    Router::new().route("/api/mod/service", post(api_service))
}

fn clip(v: &Value, n: usize) -> Value {
    match v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => json!(s.chars().take(n).collect::<String>()),
        None => Value::Null,
    }
}

/// The stored, sanitized form of a report (only known fields, strings clipped).
fn sanitize(b: &Value) -> Value {
    let ms = |k: &str| b[k].as_i64().filter(|t| *t > 0).map_or(Value::Null, |t| json!(t));
    let last_error = match (b["lastError"]["ts"].as_i64(), clip(&b["lastError"]["msg"], 160)) {
        (Some(ts), msg) if !msg.is_null() => json!({"ts": ts, "msg": msg}),
        _ => Value::Null,
    };
    json!({
        "name": clip(&b["name"], 40), "icon": clip(&b["icon"], 4), "verb": clip(&b["verb"], 16),
        "status": clip(&b["status"], 160), "error": clip(&b["error"], 200),
        "lastRun": ms("lastRun"), "lastActive": ms("lastActive"), "everyMs": ms("everyMs"),
        "lastError": last_error,
    })
}

async fn api_service(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    if b["name"].as_str().is_none_or(|s| s.trim().is_empty()) {
        return json_err(StatusCode::BAD_REQUEST, "name required");
    }
    let pane = match b["pane"].as_str().filter(|p| !p.is_empty()) {
        Some(p) => p.to_string(),
        None => {
            let Some(s) = b["session"].as_str() else { return json_err(StatusCode::BAD_REQUEST, "pane or session required") };
            let found = app.inner.lock().unwrap().pane_sessions.iter().find(|(_, v)| v.as_str() == s).map(|(p, _)| p.clone());
            match found {
                Some(p) => p,
                None => return json_err(StatusCode::NOT_FOUND, "no pane hosts that session"),
            }
        }
    };
    let svc = sanitize(&b);
    let name = svc["name"].as_str().unwrap_or("Service").to_string();
    let err = app.services.report(&pane, svc, now_ms());
    app.refresh.notify_one();
    if let Some(err) = err {
        let payload = json!({"title": format!("{name} · error"), "body": err, "url": format!("/#/a/{pane}"), "tag": format!("svc-{pane}")});
        send_push(&app, &payload, false).await;
    }
    Json(json!({"ok": true, "pane": pane})).into_response()
}

/// Adds `service` to each agent that reported as one, pins `since` to its last real activity,
/// and drops reports of panes that are gone.
pub fn decorate(app: &crate::App, agents: &mut [Value]) {
    let now = now_ms();
    let mut m = app.services.by_pane.lock().unwrap();
    let live: std::collections::HashSet<String> = agents.iter().filter_map(|a| a["pane"].as_str().map(str::to_string)).collect();
    m.retain(|p, s| live.contains(p) || now - s["ts"].as_i64().unwrap_or(0) < KEEP_MS);
    for a in agents.iter_mut() {
        let Some(s) = a["pane"].as_str().and_then(|p| m.get(p)) else { continue };
        let mut svc = s.clone();
        svc.as_object_mut().map(|o| o.remove("errorPushed"));
        let every = s["everyMs"].as_i64().unwrap_or(0);
        let stale = now - s["ts"].as_i64().unwrap_or(0) > (every * STALE_INTERVALS).max(STALE_MIN_MS);
        svc["stale"] = json!(stale);
        if a["status"] != "working" && a["status"] != "blocked" {
            let quiet_since = s["lastActive"].as_i64().or(s["since"].as_i64()).unwrap_or(0);
            if quiet_since > 0 {
                a["since"] = json!(quiet_since);
            }
        }
        a["service"] = svc;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_pushes_once_per_failure() {
        let s = Services { by_pane: Mutex::new(HashMap::new()), save: false };
        let r = |e: Option<&str>| sanitize(&json!({"name": "Backup", "status": "ok", "error": e}));
        assert_eq!(s.report("w1:p1", r(None), 1), None);
        assert_eq!(s.report("w1:p1", r(Some("NAS unreachable")), 2).as_deref(), Some("NAS unreachable"));
        assert_eq!(s.report("w1:p1", r(Some("NAS unreachable")), 3), None); // same failure: quiet
        assert_eq!(s.report("w1:p1", r(None), 4), None);
        assert_eq!(s.report("w1:p1", r(Some("NAS unreachable")), 5).as_deref(), Some("NAS unreachable")); // failing again
        assert!(s.is_service("w1:p1") && !s.is_service("w1:p2"));
        assert_eq!(s.by_pane.lock().unwrap()["w1:p1"]["since"], json!(1));
    }

    #[test]
    fn sanitize_clips_and_drops_unknown() {
        let v = sanitize(&json!({"name": "x".repeat(99), "body": "secret", "lastError": {"ts": 5, "msg": ""}, "lastRun": -3}));
        assert_eq!(v["name"].as_str().unwrap().len(), 40);
        assert!(v.get("body").is_none() && v["lastError"].is_null() && v["lastRun"].is_null());
    }
}
