//! Background services: agents that run unattended (pollers, watchers, nightly jobs) and say so.
//!
//! A service opts in by reporting itself from its mod, at no model cost:
//! `POST /api/mod/service {pane | session, name, icon?, verb?, status, lastRun?, everyMs?, lastActive?,
//! lastError?: {ts, msg}, error?}` (API.md "Background services"). The server keeps the latest report per pane
//! (persisted in `<state>/services.json`, so a restart does not turn a service back into a plain agent) and:
//! - adds it to the agent in `/api/state` as `service` (+ `stale` when reports stopped);
//! - pins the agent's `since` to its last real activity, so its own polling never counts as activity;
//! - never sends "Done" pushes for it; a new `error` (the service is failing, not a one-off hiccup) pushes once.
//!
//! **Pane-less services** (a systemd timer, a cron job: the nightly backup job posts pane `nightly-backup`): a report whose
//! pane is not a live agent pane when it arrives is `standalone`; `/api/state` lists it in top-level `services` (one row
//! of Home's Scheduled box) for as long as its pane stays out of the live set. Optional `nextRun` (ms of the next
//! expected report) makes `stale` precise (`nextRun` + 1 h); `link` (a same-origin `/#/…` path or an https URL) is
//! where tapping the row goes. A standalone row expires `max(24 h, 3 × everyMs)` after its last report.

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
/// With `nextRun`, a service is stale this long after the report it promised did not come (a run takes a while).
const NEXT_GRACE_MS: i64 = 3600_000;

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
    /// `live`: the pane is a live agent pane right now (else the report is a standalone, pane-less service).
    fn report(&self, pane: &str, mut svc: Value, live: bool, now: i64) -> Option<String> {
        let mut m = self.by_pane.lock().unwrap();
        let prev = m.get(pane);
        let prev_err = prev.and_then(|p| p["error"].as_str()).unwrap_or("").to_string();
        svc["ts"] = json!(now);
        svc["standalone"] = json!(!live);
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
    // a same-origin app path ("/#/fleet") or an https URL; anything else (javascript:, http:) is dropped
    let link = match b["link"].as_str().map(str::trim) {
        Some(l) if l.len() <= 300 && ((l.starts_with('/') && !l.starts_with("//")) || l.starts_with("https://")) => json!(l),
        _ => Value::Null,
    };
    let last_error = match (b["lastError"]["ts"].as_i64(), clip(&b["lastError"]["msg"], 160)) {
        (Some(ts), msg) if !msg.is_null() => json!({"ts": ts, "msg": msg}),
        _ => Value::Null,
    };
    json!({
        "name": clip(&b["name"], 40), "icon": clip(&b["icon"], 4), "verb": clip(&b["verb"], 16),
        "status": clip(&b["status"], 160), "error": clip(&b["error"], 200),
        "lastRun": ms("lastRun"), "lastActive": ms("lastActive"), "everyMs": ms("everyMs"),
        "nextRun": ms("nextRun"), "link": link, "lastError": last_error,
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
    let live = app.agent_panes.borrow().iter().any(|p| *p == pane);
    // a pane-less service has no chat: its push opens its link (same-origin only), else Home
    let url = if live {
        format!("/#/a/{pane}")
    } else {
        svc["link"].as_str().filter(|l| l.starts_with('/')).unwrap_or("/").to_string()
    };
    let err = app.services.report(&pane, svc, live, now_ms());
    app.refresh.notify_one();
    if let Some(err) = err {
        let payload = json!({"title": format!("{name} · error"), "body": err, "url": url, "tag": format!("svc-{pane}")});
        send_push(&app, &payload, false).await;
    }
    Json(json!({"ok": true, "pane": pane, "standalone": !live})).into_response()
}

/// No report for 3 intervals (≥ 10 min), or, with `nextRun`, an hour past the promised next report.
fn is_stale(s: &Value, now: i64) -> bool {
    let ts = s["ts"].as_i64().unwrap_or(0);
    if let Some(next) = s["nextRun"].as_i64().filter(|n| *n > ts) {
        return now > next + NEXT_GRACE_MS;
    }
    let every = s["everyMs"].as_i64().unwrap_or(0);
    now - ts > (every * STALE_INTERVALS).max(STALE_MIN_MS)
}

/// How long a report is kept after it stops coming once its pane is gone: a pane-less service stays (as a stale row)
/// for a few of its intervals, so a daily job that missed a day still shows up as late.
fn keep_ms(s: &Value) -> i64 {
    if s["standalone"].as_bool().unwrap_or(false) {
        (s["everyMs"].as_i64().unwrap_or(0) * STALE_INTERVALS).max(KEEP_MS)
    } else {
        KEEP_MS
    }
}

/// The public form of a stored report.
fn public(s: &Value, now: i64) -> Value {
    let mut svc = s.clone();
    if let Some(o) = svc.as_object_mut() {
        o.remove("errorPushed");
        o.remove("standalone");
    }
    svc["stale"] = json!(is_stale(s, now));
    svc
}

/// Pane-less services for `/api/state` `services`: standalone reports whose pane is not among `agents`, newest first.
/// Empty while herdr is unreachable (no live set to compare with).
pub fn standalone(app: &crate::App, agents: &Value, herdr_ok: bool) -> Value {
    if !herdr_ok {
        return json!([]);
    }
    let live: std::collections::HashSet<&str> =
        agents.as_array().map(|l| l.iter().filter_map(|a| a["pane"].as_str()).collect()).unwrap_or_default();
    list_standalone(&app.services.by_pane.lock().unwrap(), &live, now_ms())
}

fn list_standalone(m: &HashMap<String, Value>, live: &std::collections::HashSet<&str>, now: i64) -> Value {
    let mut out: Vec<Value> = m
        .iter()
        .filter(|(p, s)| s["standalone"].as_bool().unwrap_or(false) && !live.contains(p.as_str()))
        .filter(|(_, s)| now - s["ts"].as_i64().unwrap_or(0) < keep_ms(s))
        .map(|(p, s)| {
            let mut v = public(s, now);
            v["id"] = json!(p);
            v
        })
        .collect();
    out.sort_by_key(|v| std::cmp::Reverse(v["ts"].as_i64().unwrap_or(0)));
    json!(out)
}

/// Adds `service` to each agent that reported as one, pins `since` to its last real activity,
/// and drops reports of panes that are gone.
pub fn decorate(app: &crate::App, agents: &mut [Value]) {
    let now = now_ms();
    let mut m = app.services.by_pane.lock().unwrap();
    let live: std::collections::HashSet<String> = agents.iter().filter_map(|a| a["pane"].as_str().map(str::to_string)).collect();
    m.retain(|p, s| live.contains(p) || now - s["ts"].as_i64().unwrap_or(0) < keep_ms(s));
    for a in agents.iter_mut() {
        let Some(s) = a["pane"].as_str().and_then(|p| m.get(p)) else { continue };
        let svc = public(s, now);
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
        assert_eq!(s.report("w1:p1", r(None), true, 1), None);
        assert_eq!(s.report("w1:p1", r(Some("NAS unreachable")), true, 2).as_deref(), Some("NAS unreachable"));
        assert_eq!(s.report("w1:p1", r(Some("NAS unreachable")), true, 3), None); // same failure: quiet
        assert_eq!(s.report("w1:p1", r(None), true, 4), None);
        assert_eq!(s.report("w1:p1", r(Some("NAS unreachable")), true, 5).as_deref(), Some("NAS unreachable")); // failing again
        assert!(s.is_service("w1:p1") && !s.is_service("w1:p2"));
        assert_eq!(s.by_pane.lock().unwrap()["w1:p1"]["since"], json!(1));
    }

    #[test]
    fn sanitize_clips_and_drops_unknown() {
        let v = sanitize(&json!({"name": "x".repeat(99), "body": "secret", "lastError": {"ts": 5, "msg": ""}, "lastRun": -3}));
        assert_eq!(v["name"].as_str().unwrap().len(), 40);
        assert!(v.get("body").is_none() && v["lastError"].is_null() && v["lastRun"].is_null());
    }

    const H: i64 = 3600_000;

    fn backup(next: Option<i64>, now: i64) -> Value {
        sanitize(&json!({"pane": "nightly-backup", "name": "Nightly backup", "icon": "💾", "verb": "ran", "status": "12 GB saved",
            "lastRun": now, "everyMs": 24 * H, "nextRun": next, "link": "/#/fleet"}))
    }

    #[test]
    fn paneless_service_is_a_standalone_row() {
        let s = Services { by_pane: Mutex::new(HashMap::new()), save: false };
        let t0 = 100 * 24 * H;
        s.report("w1:p1", sanitize(&json!({"name": "Sync", "status": "ok", "everyMs": 120_000})), true, t0);
        s.report("nightly-backup", backup(Some(t0 + 23 * H), t0), false, t0);
        let live: std::collections::HashSet<&str> = ["w1:p1"].into();
        let m = s.by_pane.lock().unwrap();
        let l = list_standalone(&m, &live, t0 + 60_000);
        assert_eq!(l.as_array().unwrap().len(), 1, "only the pane-less one: {l}");
        let row = &l[0];
        assert_eq!(row["id"], "nightly-backup");
        assert_eq!(row["link"], "/#/fleet");
        assert_eq!(row["stale"], false);
        assert!(row.get("errorPushed").is_none() && row.get("standalone").is_none());
        // a pane-bound service whose pane died is not a standalone row (unchanged behaviour)
        assert_eq!(list_standalone(&m, &Default::default(), t0 + 60_000).as_array().unwrap().len(), 1);
        // a standalone id that turns into a live pane shows on that agent, not twice
        let live2: std::collections::HashSet<&str> = ["nightly-backup"].into();
        assert!(list_standalone(&m, &live2, t0 + 60_000).as_array().unwrap().is_empty());
    }

    #[test]
    fn paneless_staleness_and_expiry() {
        let t0 = 100 * 24 * H;
        let mut m = HashMap::new();
        let mut v = backup(Some(t0 + 23 * H), t0);
        v["ts"] = json!(t0);
        v["standalone"] = json!(true);
        m.insert("nightly-backup".to_string(), v.clone());
        let none = Default::default();
        let at = |dt: i64| list_standalone(&m, &none, t0 + dt);
        assert_eq!(at(23 * H + 30 * 60_000)[0]["stale"], false, "a run in progress is not late");
        assert_eq!(at(24 * H + 60_000)[0]["stale"], true, "an hour past nextRun: late");
        assert_eq!(at(71 * H)[0]["stale"], true, "still listed (as late) for 3 intervals");
        assert!(at(72 * H + 1).as_array().unwrap().is_empty(), "then it expires");
        // without nextRun: 3 × everyMs, like pane-bound services
        v["nextRun"] = Value::Null;
        m.insert("nightly-backup".to_string(), v);
        let at = |dt: i64| list_standalone(&m, &none, t0 + dt);
        assert_eq!(at(25 * H)[0]["stale"], false);
        assert_eq!(at(72 * H - 1)[0]["stale"], false);
        // a service without everyMs: 10 min stale, kept 24 h
        let mut q = sanitize(&json!({"name": "Ping", "status": "ok"}));
        q["ts"] = json!(t0);
        q["standalone"] = json!(true);
        assert!(is_stale(&q, t0 + 11 * 60_000) && keep_ms(&q) == KEEP_MS);
    }

    #[test]
    fn link_is_same_origin_or_https() {
        let l = |x: &str| sanitize(&json!({"name": "x", "link": x}))["link"].clone();
        assert_eq!(l("/#/fleet"), json!("/#/fleet"));
        assert_eq!(l("https://example.com/a"), json!("https://example.com/a"));
        for bad in ["javascript:alert(1)", "//evil.com", "http://x", "data:text/html,x"] {
            assert!(l(bad).is_null(), "{bad}");
        }
    }
}
