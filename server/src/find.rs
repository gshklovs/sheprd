//! Finding and continuing agents (API.md "Find and continue"): every agent, awake (a live herdr pane) or asleep
//! (its pane closed), with a stable short name and a one-line job ("Runs the nightly backup and reports failures").
//!
//! - `<state>/agents.json` keeps, per session: where it last lived (pane, workspace, tab name, folder) so Continue
//!   brings it back in place (`claude --resume` in its old workspace and tab, as `/api/home/ensure` does for the home agent),
//!   Haiku's short name + job line, the user's own rename / job line for an asleep agent, and "hidden from Recent".
//! - Pins are the per-session `pinned` pref (prefs.rs, the same flag the swipe Pin sets), ≤ 5 from the phone;
//!   The home agent is pinned by default once.
//! - The job line is written by Haiku (thinking off, no tools) while the phone is in use, at most `DAY_CAP` calls a
//!   day; it is asked again only after the agent's prompt count doubled (≥ 6 more), and keeps its line unless the
//!   work clearly changed. A line the user typed is never replaced.
//!
//! Hooks from main.rs: `routes`, `decorate` (records places; adds `job` / `short` to live agents), `run` (job writer).
//! fleet.rs `api_resume` calls `place`, `resumed`, `after_resume`.

use crate::fleet::{self, Sess};
use crate::transcript::now_ms;
use crate::{App, Shared, json_err};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

/// Haiku job lines a day, at most.
const DAY_CAP: u32 = 60;
/// Phone pins, at most (Pin to Home).
pub const MAX_PINS: usize = 5;
/// Asleep agents older than this are left to History.
const ASLEEP_DAYS: i64 = 30;
/// The home agent's job line.
const HOME_JOB: &str = "Your main agent: hands out work and keeps track of it";

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/agents", get(api_agents))
        .route("/api/agents/resolve", get(api_resolve))
        .route("/api/agents/{sid}/meta", post(api_meta))
}

// ---------------------------------------------------------------------------
// Store

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Info {
    /// one-line job ("Runs the nightly backup and reports failures")
    pub job: Option<String>,
    /// "haiku" | "user"
    pub job_by: Option<String>,
    /// prompt count the job line was written (or last confirmed) at
    pub job_n: u32,
    pub job_at: i64,
    /// last Haiku attempt (ok or not), ms
    pub tried: i64,
    /// Haiku's short name, set once
    pub nick: Option<String>,
    /// the user's rename of an asleep agent (typed in as `/rename` when it is continued)
    pub name: Option<String>,
    /// where it last lived
    pub pane: Option<String>,
    pub ws: Option<String>,
    pub label: Option<String>,
    pub cwd: Option<String>,
    /// last time it was seen live, ms
    pub at: i64,
    /// hidden from Recent
    pub hidden: bool,
    /// branched from this session (branch.rs), with the parent's name then and when
    pub branch_of: Option<String>,
    pub branch_name: Option<String>,
    pub branch_at: i64,
}

#[derive(Serialize, Deserialize, Default, Debug)]
#[serde(default)]
pub struct Store {
    pub info: HashMap<String, Info>,
    /// sessions that got their default pin (once each)
    pub defaults: Vec<String>,
    pub day: String,
    pub calls: u32,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    saved: i64,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn store_path() -> PathBuf {
    crate::state_dir().join("agents.json")
}

fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut g = STORE.lock().unwrap();
    let s = g.get_or_insert_with(|| std::fs::read(store_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default());
    f(s)
}

/// Writes the store when it changed (at once with `now`, else at most every 10 s).
fn flush(now: bool) {
    let out = with(|s| {
        if !s.dirty || (!now && now_ms() - s.saved < 10_000) {
            return None;
        }
        s.dirty = false;
        s.saved = now_ms();
        serde_json::to_vec(s).ok()
    });
    if let Some(b) = out {
        let tmp = store_path().with_extension("json.tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(&tmp, store_path());
        }
    }
}

/// Where a session last lived, for Continue.
pub fn place(sid: &str) -> Option<Info> {
    with(|s| s.info.get(sid).cloned())
}

/// `sid` was just branched off `parent` (branch.rs).
pub fn branched(sid: &str, parent: &str, parent_name: &str, _i: usize) {
    with(|s| {
        let i = s.info.entry(sid.to_string()).or_default();
        i.branch_of = Some(parent.to_string());
        i.branch_name = Some(parent_name.to_string()).filter(|n| !n.is_empty());
        i.branch_at = now_ms();
        s.dirty = true;
    });
    flush(true);
}

/// The session `sid` was branched from, if any.
pub fn branch_of(sid: &str) -> Option<String> {
    with(|s| s.info.get(sid).and_then(|i| i.branch_of.clone()))
}

/// `branchOf: {sid, name, pane}` for a branch: its parent's name now (live title, else asleep name, else the name it
/// had at the branch) and its pane when awake.
fn branch_json(i: Option<&Info>, info: &HashMap<String, Info>, live: &[Value], sess: &HashMap<String, Sess>) -> Value {
    let Some(p) = i.and_then(|i| i.branch_of.clone()) else { return Value::Null };
    let a = live.iter().find(|a| a["session"].as_str() == Some(p.as_str()));
    let name = match a {
        Some(a) if a["home"] == true => Some(crate::home_name()),
        Some(a) => a["short"].as_str().or(a["title"].as_str()).map(str::to_string),
        None => sess.get(&p).map(|s| asleep_name(info.get(&p), s)),
    }
    .or_else(|| i.and_then(|i| i.branch_name.clone()))
    .unwrap_or_else(|| "another chat".into());
    json!({"sid": p, "name": name, "pane": a.map(|a| a["pane"].clone()).unwrap_or(Value::Null)})
}

/// A resume just opened `pane` for `sid`.
pub fn resumed(sid: &str, pane: &str) {
    with(|s| {
        let i = s.info.entry(sid.to_string()).or_default();
        i.pane = Some(pane.to_string());
        i.at = now_ms();
        s.dirty = true;
    });
    flush(true);
}

/// The resumed agent is up: a rename the user made while it slept is typed in (`/rename`) once it is idle.
pub fn after_resume(app: &App, sid: &str, pane: &str) {
    let Some(name) = with(|s| s.info.get_mut(sid).and_then(|i| i.name.take())) else { return };
    with(|s| s.dirty = true);
    flush(true);
    {
        let mut i = app.inner.lock().unwrap();
        i.meta.insert(pane.to_string(), crate::MetaReq { title: Some(name.clone()), color: None, applied: 0 });
    }
    crate::patch_agent(app, pane, |a| a["title"] = json!(name));
    app.refresh.notify_one();
}

// ---------------------------------------------------------------------------
// Names

/// Cleans a one-line string: whitespace collapsed, quotes and a final period dropped, ≤ `max` chars.
fn tidy(s: &str, max: usize) -> Option<String> {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '“' || c == '”').trim_end_matches('.').trim().to_string();
    if t.is_empty() || t.chars().count() > max { None } else { Some(t) }
}

fn short(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut c: String = flat.chars().take(max.saturating_sub(1)).collect();
    c.push('…');
    c
}

/// Display name of an asleep agent: the user's rename > custom title > Haiku's short name > Claude's title > first prompt.
pub fn asleep_name(i: Option<&Info>, s: &Sess) -> String {
    i.and_then(|i| i.name.clone())
        .or_else(|| s.custom.clone())
        .or_else(|| i.and_then(|i| i.nick.clone()))
        .or_else(|| s.ai.clone())
        .or_else(|| s.first.as_deref().map(|f| short(f, 40)))
        .unwrap_or_else(|| "Untitled".into())
}

/// A live agent's short name when it is better than its title: only when the agent has no custom title (Claude's
/// long ai-title or a fallback) and Haiku gave it a short name; else null.
fn live_short(i: Option<&Info>, s: Option<&Sess>, title: &str, pending: bool) -> Option<String> {
    if pending || s.is_some_and(|s| s.custom.is_some()) {
        return None;
    }
    i.and_then(|i| i.nick.clone()).filter(|n| n != title)
}

/// Folders that only hold test runs and scratch work: never listed as asleep agents.
fn scratch(cwd: &str) -> bool {
    let home = crate::home().to_string_lossy().to_string();
    let tmp = (cwd.starts_with("/tmp/") || cwd == "/tmp") && !cwd.starts_with(&format!("{home}/"));
    // test runs: the bare ~/Work/tries folder and folders named like tests (follow-test2, mm-tests)
    let leaf = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_lowercase();
    let test = leaf == "tries" || leaf.split(['-', '_', '.']).any(|w| w.trim_end_matches(|c: char| c.is_ascii_digit()).trim_end_matches('s') == "test");
    tmp || test || cwd.contains("/scratchpad") || cwd.starts_with(&format!("{home}/.cache/"))
}

// ---------------------------------------------------------------------------
// State decoration

/// Records where each live agent lives, adds `job` and `short` (display name when better than the title), and gives
/// the home agent its default pin once.
pub fn decorate(app: &App, agents: &mut [Value]) {
    let now = now_ms();
    let sess: HashMap<String, Sess> = fleet::snapshot().into_iter().map(|s| (s.sid.clone(), s)).collect();
    let pending: Vec<String> = app.inner.lock().unwrap().meta.keys().cloned().collect();
    let mut pin = Vec::new();
    let mut branches: Vec<(String, Info)> = Vec::new();
    with(|st| {
        for a in agents.iter_mut() {
            let Some(sid) = a["session"].as_str().map(str::to_string) else {
                a["job"] = Value::Null;
                a["short"] = Value::Null;
                continue;
            };
            let home = a["home"] == true;
            let i = st.info.entry(sid.clone()).or_default();
            let place = (
                a["pane"].as_str().map(str::to_string),
                a["ws"].as_str().map(str::to_string),
                a["tabLabel"].as_str().map(str::to_string),
                a["cwd"].as_str().map(str::to_string),
            );
            if (i.pane.clone(), i.ws.clone(), i.label.clone(), i.cwd.clone()) != place || now - i.at > 60_000 {
                (i.pane, i.ws, i.label, i.cwd) = place;
                i.at = now;
                st.dirty = true;
            }
            if home && i.job.is_none() {
                i.job = Some(HOME_JOB.into());
                i.job_by = Some("default".into());
                st.dirty = true;
            }
            let pane = a["pane"].as_str().unwrap_or("").to_string();
            let title = a["title"].as_str().unwrap_or("").to_string();
            a["job"] = json!(i.job);
            let i2 = i.clone();
            branches.push((a["pane"].as_str().unwrap_or("").to_string(), i2));
            a["short"] = if home { Value::Null } else { json!(live_short(Some(i), sess.get(&sid), &title, pending.contains(&pane))) };
            let want = home;
            if want && !st.defaults.contains(&sid) {
                st.defaults.push(sid.clone());
                st.dirty = true;
                pin.push(sid.clone());
                if a["pinned"] != true {
                    a["pinned"] = json!(true);
                }
            }
        }
    });
    // `branchOf` on branches (branch.rs): their parent, awake or asleep
    let branches: Vec<(String, Info)> = branches.into_iter().filter(|(_, i)| i.branch_of.is_some()).collect();
    if !branches.is_empty() {
        let info = with(|st| st.info.clone());
        let live: Vec<Value> = agents.to_vec();
        for (pane, i) in &branches {
            let b = branch_json(Some(i), &info, &live, &sess);
            if let Some(a) = agents.iter_mut().find(|a| a["pane"].as_str() == Some(pane.as_str())) {
                a["branchOf"] = b;
            }
        }
    }
    if !pin.is_empty() {
        let mut p = app.prefs.lock().unwrap();
        let mut n = pinned_count(&p);
        for sid in pin {
            if p.v["sessions"][&sid].is_object() || n >= MAX_PINS {
                continue; // the user set this one himself (pinned or not), or the row is full
            }
            p.set_flags(&sid, Some(true), None);
            n += 1;
        }
    }
    flush(false);
}

/// Pin to Home / unpin; at most `MAX_PINS` pinned.
pub fn pin(p: &mut crate::prefs::Prefs, sid: &str, on: bool) -> Result<(), String> {
    if on && !p.flags(sid).0 && pinned_count(p) >= MAX_PINS {
        return Err(format!("{MAX_PINS} agents are pinned already"));
    }
    p.set_flags(sid, Some(on), None);
    Ok(())
}

fn pinned_count(p: &crate::prefs::Prefs) -> usize {
    p.v["sessions"].as_object().map(|m| m.values().filter(|s| s["pinned"] == true).count()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// GET /api/agents

#[derive(Deserialize)]
struct ListQ {
    /// asleep agents, at most (default 40)
    limit: Option<usize>,
}

fn state_of(app: &App) -> Value {
    let s = app.inner.lock().unwrap().state.clone();
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

/// One row: the same shape for awake and asleep agents.
fn row_live(a: &Value, i: Option<&Info>, s: Option<&Sess>) -> Value {
    let home = a["home"] == true;
    let name = if home { crate::home_name() } else { a["short"].as_str().or(a["title"].as_str()).unwrap_or("").to_string() };
    json!({
        "sid": a["session"], "name": name, "title": a["title"], "tab": a["tabLabel"], "job": a["job"], "jobBy": i.and_then(|i| i.job_by.clone()),
        "cwd": a["cwd"], "color": a["color"], "pane": a["pane"], "status": a["status"], "since": a["since"],
        "asleep": false, "home": home, "service": !a["service"].is_null(),
        "ts": s.map(|s| s.mtime).unwrap_or(0).max(a["last"]["ts"].as_i64().unwrap_or(0)),
        "lu": s.map(|s| s.last_user).unwrap_or(0), "last": a["last"],
        "pinned": a["pinned"] == true, "hidden": i.is_some_and(|i| i.hidden), "prompts": s.map(|s| s.prompts).unwrap_or(0),
    })
}

fn row_asleep(s: &Sess, i: Option<&Info>, pinned: bool) -> Value {
    let last = s.last_asst.as_ref().map(|t| json!({"role": "assistant", "text": t})).or_else(|| s.last.as_ref().map(|t| json!({"role": "user", "text": t})));
    json!({
        "sid": s.sid, "name": asleep_name(i, s), "title": s.custom.clone().or(s.ai.clone()), "tab": i.and_then(|i| i.label.clone()), "job": i.and_then(|i| i.job.clone()),
        "jobBy": i.and_then(|i| i.job_by.clone()), "cwd": s.cwd, "color": s.color, "pane": Value::Null, "status": Value::Null,
        "since": s.mtime, "created": s.created, "asleep": true, "home": false, "service": false, "ts": s.mtime, "lu": s.last_user, "last": last,
        "pinned": pinned, "hidden": i.is_some_and(|i| i.hidden), "prompts": s.prompts,
    })
}

/// Can this session be listed as an asleep agent?
fn listable(s: &Sess, now: i64) -> bool {
    !s.headless
        && s.prompts > 0
        && s.cont.is_none()
        && now - s.mtime < ASLEEP_DAYS * 86_400_000
        && s.cwd.as_deref().is_some_and(|c| !scratch(c) && std::path::Path::new(c).is_dir())
}

async fn api_agents(State(app): State<Shared>, Query(q): Query<ListQ>) -> Response {
    let all = fleet::sessions().await;
    let now = now_ms();
    let st = state_of(&app);
    let live: Vec<Value> = st["agents"].as_array().cloned().unwrap_or_default();
    let by: HashMap<&str, &Sess> = all.iter().map(|s| (s.sid.as_str(), s)).collect();
    let by_sid: HashMap<String, Sess> = all.iter().map(|s| (s.sid.clone(), s.clone())).collect();
    let (prefs, pins) = {
        let p = app.prefs.lock().unwrap();
        (p.v["sessions"].clone(), pinned_count(&p))
    };
    let mut out = Vec::new();
    let mut older = 0;
    with(|st| {
        for a in &live {
            let Some(sid) = a["session"].as_str() else { continue };
            let mut r = row_live(a, st.info.get(sid), by.get(sid).copied());
            r["branchOf"] = branch_json(st.info.get(sid), &st.info, &live, &by_sid);
            out.push(r);
        }
        let awake: Vec<&str> = live.iter().filter_map(|a| a["session"].as_str()).collect();
        let mut sleep: Vec<&Sess> = all.iter().filter(|s| !awake.contains(&s.sid.as_str()) && listable(s, now)).collect();
        sleep.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        let lim = q.limit.unwrap_or(40).clamp(1, 200);
        older = sleep.len().saturating_sub(lim);
        for s in sleep.into_iter().take(lim) {
            let mut r = row_asleep(s, st.info.get(&s.sid), prefs[&s.sid]["pinned"] == true);
            r["branchOf"] = branch_json(st.info.get(&s.sid), &st.info, &live, &by_sid);
            out.push(r);
        }
    });
    Json(json!({"now": now, "agents": out, "older": older, "pins": pins, "maxPins": MAX_PINS})).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/agents/resolve?sid=|pane=&name=  — one agent by session, or who sent this agent message (an `.amsg` card), awake or asleep

#[derive(Deserialize)]
struct ResolveQ {
    /// a session id straight away (the asleep chat's header)
    sid: Option<String>,
    pane: Option<String>,
    name: Option<String>,
}

/// The session behind a sender: the live pane, else the session that last lived in that pane, else a name match.
pub fn resolve_sid(live: &[Value], info: &HashMap<String, Info>, sess: &[Sess], pane: Option<&str>, name: Option<&str>) -> Option<String> {
    let pane = pane.filter(|p| !p.is_empty());
    if let Some(p) = pane {
        if let Some(sid) = live.iter().find(|a| a["pane"] == p).and_then(|a| a["session"].as_str()) {
            return Some(sid.to_string());
        }
        if let Some((sid, _)) = info.iter().filter(|(_, i)| i.pane.as_deref() == Some(p)).max_by_key(|(_, i)| i.at) {
            return Some(sid.clone());
        }
    }
    let n = name.map(|n| n.trim().to_lowercase()).filter(|n| !n.is_empty())?;
    if let Some(sid) = live.iter().find(|a| [&a["title"], &a["short"], &a["name"]].iter().any(|t| t.as_str().is_some_and(|t| t.to_lowercase() == n))).and_then(|a| a["session"].as_str()) {
        return Some(sid.to_string());
    }
    sess.iter()
        .filter(|s| {
            let i = info.get(&s.sid);
            [i.and_then(|i| i.name.clone()), s.custom.clone(), i.and_then(|i| i.nick.clone()), s.ai.clone()].iter().flatten().any(|t| t.to_lowercase() == n)
        })
        .max_by_key(|s| s.mtime)
        .map(|s| s.sid.clone())
}

async fn api_resolve(State(app): State<Shared>, Query(q): Query<ResolveQ>) -> Response {
    let all = fleet::sessions().await;
    let st = state_of(&app);
    let live = st["agents"].as_array().cloned().unwrap_or_default();
    let prefs = app.prefs.lock().unwrap().v["sessions"].clone();
    let row = with(|store| {
        let sid = q.sid.clone().filter(|s| all.iter().any(|x| &x.sid == s) || live.iter().any(|a| a["session"] == s.as_str()))
            .or_else(|| resolve_sid(&live, &store.info, &all, q.pane.as_deref(), q.name.as_deref()))?;
        let i = store.info.get(&sid);
        let s = all.iter().find(|s| s.sid == sid);
        match live.iter().find(|a| a["session"] == sid.as_str()) {
            Some(a) => Some(row_live(a, i, s)),
            None => s.map(|s| row_asleep(s, i, prefs[&sid]["pinned"] == true)),
        }
    });
    match row {
        Some(r) => Json(json!({"agent": r})).into_response(),
        None => json_err(StatusCode::NOT_FOUND, "unknown agent"),
    }
}

// ---------------------------------------------------------------------------
// POST /api/agents/{sid}/meta {pinned?, hidden?, name?, job?}

#[derive(Deserialize)]
struct MetaBody {
    pinned: Option<bool>,
    hidden: Option<bool>,
    /// asleep agents only (a live one is renamed through /api/a/{pane}/meta)
    name: Option<String>,
    /// "" clears the user's line (Haiku writes one again)
    job: Option<String>,
}

/// Applies a meta change to the store; `Err` = 400 message.
pub fn apply_meta(st: &mut Store, sid: &str, hidden: Option<bool>, name: Option<&str>, job: Option<&str>) -> Result<(), String> {
    let name = match name {
        Some(n) => Some(tidy(n, 100).ok_or("name must be 1-100 chars")?),
        None => None,
    };
    let job = match job {
        Some(j) if j.trim().is_empty() => Some(None),
        Some(j) => Some(Some(tidy(j, 120).ok_or("job line must be ≤ 120 chars")?)),
        None => None,
    };
    let i = st.info.entry(sid.to_string()).or_default();
    if let Some(h) = hidden {
        i.hidden = h;
    }
    if let Some(n) = name {
        i.name = Some(n);
    }
    match job {
        Some(Some(j)) => {
            i.job = Some(j);
            i.job_by = Some("user".into());
            i.job_at = now_ms();
        }
        Some(None) => {
            i.job = None;
            i.job_by = None;
            i.job_n = 0;
            i.tried = 0;
        }
        None => {}
    }
    st.dirty = true;
    Ok(())
}

async fn api_meta(State(app): State<Shared>, AxPath(sid): AxPath<String>, Json(b): Json<MetaBody>) -> Response {
    if !crate::valid_sid(&sid) {
        return json_err(StatusCode::BAD_REQUEST, "bad session id");
    }
    if crate::find_session(&app, &sid).is_none() && crate::pane_for_session(&app, &sid).is_none() {
        return json_err(StatusCode::NOT_FOUND, "no such session");
    }
    if b.name.is_some() && crate::pane_for_session(&app, &sid).is_some() {
        return json_err(StatusCode::CONFLICT, "agent is awake: rename it with /api/a/{pane}/meta");
    }
    if let Some(p) = b.pinned {
        if let Err(e) = pin(&mut app.prefs.lock().unwrap(), &sid, p) {
            return json_err(StatusCode::CONFLICT, e);
        }
    }
    if let Err(e) = with(|st| apply_meta(st, &sid, b.hidden, b.name.as_deref(), b.job.as_deref())) {
        return json_err(StatusCode::BAD_REQUEST, e);
    }
    flush(true);
    app.refresh.notify_one();
    Json(json!({"ok": true})).into_response()
}

// ---------------------------------------------------------------------------
// Job lines (Haiku)

/// Is this session due for a (re)written job line? `n` = its prompt count now.
pub fn job_due(i: Option<&Info>, n: u32, now: i64) -> bool {
    if n == 0 {
        return false;
    }
    let Some(i) = i else { return true };
    if matches!(i.job_by.as_deref(), Some("user") | Some("default")) || now - i.tried < 10 * 60_000 {
        return false;
    }
    match &i.job {
        None => true,
        // again only once the work could have moved on: twice the prompts, and at least 6 more
        Some(_) => n >= i.job_n + i.job_n.max(6),
    }
}

/// What Haiku sees about one agent.
pub struct Ask {
    pub sid: String,
    pub prompts: u32,
    pub title: String,
    pub cwd: String,
    pub first: String,
    pub last: String,
    pub reply: String,
    pub job: Option<String>,
}

fn job_prompt(a: &Ask) -> String {
    let cut = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let folder = a.cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let cur = match &a.job {
        Some(j) => format!(
            "Its current job line is: \"{j}\". Keep it (\"changed\": false, same text) unless the work has clearly moved to \
             something else.\n"
        ),
        None => String::new(),
    };
    format!(
        "Label a Claude coding agent for a phone list where the user finds it again later. Reply with JSON only, no prose: \
         {{\"name\": \"<1-3 word name, no emoji>\", \"job\": \"<one line under 60 chars: what this agent does for the user, present \
         tense, starting with a verb, e.g. Runs the nightly backup and reports failures>\", \"changed\": true|false}}.\n{cur}\
         Title: {}\nFolder: {folder}\nFirst request:\n<<<\n{}\n>>>\nLatest request:\n<<<\n{}\n>>>\nLatest reply:\n<<<\n{}\n>>>",
        cut(&a.title, 120),
        cut(&a.first, 500),
        cut(&a.last, 400),
        cut(&a.reply, 400)
    )
}

/// Pulls {name, job, changed} out of Haiku's reply.
pub fn parse_job(s: &str) -> Option<(Option<String>, String, bool)> {
    let a = s.find('{')?;
    let b = s.rfind('}')?;
    let v: Value = serde_json::from_str(s.get(a..=b)?).ok()?;
    let job = tidy(v["job"].as_str()?, 80).filter(|j| j.split_whitespace().count() >= 2)?;
    let name = v["name"].as_str().and_then(|n| tidy(n, 32)).filter(|n| (1..=4).contains(&n.split_whitespace().count()));
    Some((name, job, v["changed"].as_bool().unwrap_or(true)))
}

async fn haiku_job(a: &Ask) -> Result<(Option<String>, String, bool), String> {
    if let Ok(fake) = std::env::var("SHEPRD_FAKE_JOB") {
        // tests: "Name|Job line"
        let (n, j) = fake.split_once('|').unwrap_or(("", &fake));
        return Ok((Some(n.to_string()).filter(|n| !n.is_empty()), j.to_string(), true));
    }
    let v = crate::newagent::run_haiku(None, &job_prompt(a), Duration::from_secs(60)).await?;
    let text = v["result"].as_str().unwrap_or("");
    parse_job(text).ok_or_else(|| format!("unusable reply: {}", text.chars().take(120).collect::<String>()))
}

/// Stores Haiku's answer: the short name only once, the job line unless Haiku says it still fits.
pub fn store_job(st: &mut Store, sid: &str, n: u32, r: Result<(Option<String>, String, bool), String>) {
    let i = st.info.entry(sid.to_string()).or_default();
    i.tried = now_ms();
    st.dirty = true;
    let Ok((name, job, changed)) = r else { return };
    if i.nick.is_none() {
        i.nick = name;
    }
    if matches!(i.job_by.as_deref(), Some("user") | Some("default")) {
        return; // the user typed one meanwhile
    }
    if i.job.is_none() || changed {
        i.job = Some(job);
        i.job_by = Some("haiku".into());
        i.job_at = now_ms();
    }
    i.job_n = n;
}

/// The agents due for a job line, live ones first (most recently used), then recent asleep ones.
fn due_list(app: &App) -> Vec<Ask> {
    let now = now_ms();
    let st = state_of(app);
    let live = st["agents"].as_array().cloned().unwrap_or_default();
    let all = fleet::snapshot();
    let by: HashMap<&str, &Sess> = all.iter().map(|s| (s.sid.as_str(), s)).collect();
    let mut out = Vec::new();
    with(|store| {
        let ask = |s: &Sess, title: String, i: Option<&Info>| Ask {
            sid: s.sid.clone(), prompts: s.prompts, title, cwd: s.cwd.clone().unwrap_or_default(), first: s.first.clone().unwrap_or_default(),
            last: s.last.clone().unwrap_or_default(), reply: s.last_asst.clone().unwrap_or_default(), job: i.and_then(|i| i.job.clone()),
        };
        for a in &live {
            if a["home"] == true || a["kind"] != "claude" {
                continue;
            }
            let Some(s) = a["session"].as_str().and_then(|sid| by.get(sid)) else { continue };
            if s.cwd.as_deref().is_some_and(scratch) {
                continue;
            }
            let i = store.info.get(&s.sid);
            if job_due(i, s.prompts, now) {
                out.push(ask(s, a["title"].as_str().unwrap_or("").to_string(), i));
            }
        }
        let awake: Vec<&str> = live.iter().filter_map(|a| a["session"].as_str()).collect();
        let mut sleep: Vec<&Sess> = all.iter().filter(|s| !awake.contains(&s.sid.as_str()) && listable(s, now) && now - s.mtime < 14 * 86_400_000).collect();
        sleep.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        for s in sleep.into_iter().take(25) {
            let i = store.info.get(&s.sid);
            if job_due(i, s.prompts, now) {
                out.push(ask(s, s.custom.clone().or(s.ai.clone()).unwrap_or_default(), i));
            }
        }
    });
    out
}

/// Job writer: every 20 s while the phone is in use (seen in the last 15 min), up to 2 Haiku calls, ≤ DAY_CAP a day.
pub async fn run(app: Shared) {
    let off = std::env::var("SHEPRD_JOB_HAIKU").is_ok_and(|v| v == "0");
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        if off {
            continue;
        }
        let viewed = now_ms() - app.inner.lock().unwrap().last_visible < 15 * 60_000;
        if !viewed {
            continue;
        }
        let day = chrono_day();
        let left = with(|s| {
            if s.day != day {
                s.day = day.clone();
                s.calls = 0;
                s.dirty = true;
            }
            DAY_CAP.saturating_sub(s.calls)
        });
        if left == 0 {
            continue;
        }
        let _ = fleet::sessions().await; // index up to date
        let due = due_list(&app);
        for a in due.into_iter().take(left.min(2) as usize) {
            with(|s| {
                s.calls += 1;
                s.info.entry(a.sid.clone()).or_default().tried = now_ms();
            });
            let r = haiku_job(&a).await;
            if let Err(e) = &r {
                eprintln!("job line {}: {e}", a.sid);
            }
            with(|s| store_job(s, &a.sid, a.prompts, r));
            flush(true);
            app.refresh.notify_one();
        }
    }
}

/// Local day as "YYYY-MM-DD"-ish key (UTC days are fine for a daily cap).
fn chrono_day() -> String {
    format!("{}", now_ms() / 86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(sid: &str) -> Sess {
        Sess { sid: sid.into(), cwd: Some("/home/user/Work/x".into()), prompts: 3, mtime: now_ms(), ..Default::default() }
    }

    #[test]
    fn job_due_rules() {
        let now = now_ms();
        assert!(job_due(None, 1, now));
        assert!(!job_due(None, 0, now));
        let mut i = Info { job: Some("Builds x".into()), job_by: Some("haiku".into()), job_n: 4, ..Default::default() };
        assert!(!job_due(Some(&i), 9, now)); // 4 + max(4,6) = 10
        assert!(job_due(Some(&i), 10, now));
        i.tried = now - 60_000;
        assert!(!job_due(Some(&i), 50, now)); // tried a minute ago
        i.tried = 0;
        i.job_by = Some("user".into());
        assert!(!job_due(Some(&i), 50, now)); // the user's own line stays
    }

    #[test]
    fn haiku_reply_parsing_and_storage() {
        let r = parse_job("sure {\"name\": \"Nightly Backup\", \"job\": \"Runs the nightly backup and reports failures.\", \"changed\": true}");
        assert_eq!(r, Some((Some("Nightly Backup".into()), "Runs the nightly backup and reports failures".into(), true)));
        assert_eq!(parse_job("{\"job\": \"x\"}"), None); // one word
        assert_eq!(parse_job("no json"), None);
        let mut st = Store::default();
        store_job(&mut st, "s1", 3, Ok((Some("Backup".into()), "Runs the backup".into(), true)));
        // a later answer keeps the stable name; "changed": false keeps the line but moves job_n on
        store_job(&mut st, "s1", 12, Ok((Some("Other".into()), "Does something else".into(), false)));
        let i = &st.info["s1"];
        assert_eq!((i.nick.as_deref(), i.job.as_deref(), i.job_n), (Some("Backup"), Some("Runs the backup"), 12));
        store_job(&mut st, "s1", 30, Ok((None, "Trains the walker".into(), true)));
        assert_eq!(st.info["s1"].job.as_deref(), Some("Trains the walker"));
        // the user's line is never replaced
        apply_meta(&mut st, "s1", None, None, Some("My own line")).unwrap();
        store_job(&mut st, "s1", 80, Ok((None, "Haiku line".into(), true)));
        assert_eq!(st.info["s1"].job.as_deref(), Some("My own line"));
        // "" clears it so Haiku writes one again
        apply_meta(&mut st, "s1", None, None, Some(" ")).unwrap();
        assert!(st.info["s1"].job.is_none() && job_due(st.info.get("s1"), 1, now_ms()));
        assert!(apply_meta(&mut st, "s1", None, Some(&"x".repeat(101)), None).is_err());
    }

    #[test]
    fn names_and_listing() {
        let mut s = sess("a");
        s.ai = Some("Repo investigation: sysid bench, Darth Vader".into());
        s.first = Some("look at the repo".into());
        let mut i = Info { nick: Some("Sysid Bench".into()), ..Default::default() };
        assert_eq!(asleep_name(Some(&i), &s), "Sysid Bench");
        s.custom = Some("Robot Bench".into());
        assert_eq!(asleep_name(Some(&i), &s), "Robot Bench");
        i.name = Some("Bench".into());
        assert_eq!(asleep_name(Some(&i), &s), "Bench");
        assert_eq!(asleep_name(None, &sess("b")), "Untitled");
        // a live agent's short name only stands in for a non-custom title
        assert_eq!(live_short(Some(&i), Some(&sess("c")), "Long ai title", false), Some("Sysid Bench".into()));
        assert_eq!(live_short(Some(&i), Some(&s), "Robot Bench", false), None);
        assert_eq!(live_short(Some(&i), Some(&sess("c")), "Long ai title", true), None);
        let now = now_ms();
        let mut t = sess("t");
        t.cwd = Some("/tmp/acme/storefront".into());
        assert!(!listable(&t, now));
        t.cwd = Some(crate::home().to_string_lossy().to_string());
        assert!(listable(&t, now));
        t.cwd = Some(format!("{}/Work/tries/follow-test2", crate::home().display()));
        assert!(!listable(&t, now)); // a test folder
        t.cwd = Some(format!("{}/Work/tries/scratch-run", crate::home().display()));
        assert!(!scratch(t.cwd.as_deref().unwrap())); // real work in tries/ stays (the folder need not exist here)
        t.cwd = Some(crate::home().to_string_lossy().to_string());
        t.cont = Some("next".into());
        assert!(!listable(&t, now)); // continued in another session
        t.cont = None;
        t.headless = true;
        assert!(!listable(&t, now));
    }

    #[test]
    fn resolves_the_sender_of_an_agent_message() {
        let live = vec![json!({"pane": "w6:p1", "session": "live1", "title": "sheprd builder"})];
        let mut info = HashMap::new();
        info.insert("old".to_string(), Info { pane: Some("w6:p9".into()), at: 5, ..Default::default() });
        info.insert("older".to_string(), Info { pane: Some("w6:p9".into()), at: 1, ..Default::default() });
        let mut s = sess("backup");
        s.custom = Some("Nightly Backup".into());
        let all = vec![s];
        assert_eq!(resolve_sid(&live, &info, &all, Some("w6:p1"), None).as_deref(), Some("live1"));
        assert_eq!(resolve_sid(&live, &info, &all, Some("w6:p9"), Some("whatever")).as_deref(), Some("old")); // newest in that pane
        assert_eq!(resolve_sid(&live, &info, &all, Some("gone:x"), Some("nightly backup")).as_deref(), Some("backup"));
        assert_eq!(resolve_sid(&live, &info, &all, None, Some("Sheprd Builder")).as_deref(), Some("live1"));
        assert_eq!(resolve_sid(&live, &info, &all, None, Some("nobody")), None);
    }

    #[test]
    fn pins_persist_and_stop_at_five() {
        let dir = std::env::temp_dir().join(format!("mm-find-pins-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = crate::prefs::Prefs::load(&dir);
        for k in 0..MAX_PINS {
            pin(&mut p, &format!("s{k}"), true).unwrap();
        }
        assert!(pin(&mut p, "s9", true).is_err());
        pin(&mut p, "s0", true).unwrap(); // already pinned: fine
        pin(&mut p, "s0", false).unwrap();
        pin(&mut p, "s9", true).unwrap();
        let back = crate::prefs::Prefs::load(&dir); // persisted
        assert_eq!((back.flags("s9").0, back.flags("s0").0, pinned_count(&back)), (true, false, MAX_PINS));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hidden_and_rename_are_stored() {
        let mut st = Store::default();
        apply_meta(&mut st, "s", Some(true), Some("  Nightly   Backup "), None).unwrap();
        assert!(st.info["s"].hidden);
        assert_eq!(st.info["s"].name.as_deref(), Some("Nightly Backup"));
        apply_meta(&mut st, "s", Some(false), None, None).unwrap();
        assert!(!st.info["s"].hidden);
        let back: Store = serde_json::from_slice(&serde_json::to_vec(&st).unwrap()).unwrap();
        assert_eq!(back.info["s"], st.info["s"]);
    }
}
