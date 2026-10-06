//! v3 features: plan cards, richer push bodies, presence-aware push
//! suppression, "open on desk", "tell me when…" watches, blocked re-pings,
//! "review this" reviewer agents, fork-a-session and worktree agents.
//!
//! Persistent bits (watches, settings, reviewer links) live in `<state dir>/v3.json`.
//! Hooks from main.rs: `decorate` (per-agent fields before each state publish),
//! `run` (the watch / re-ping loop), `plan_hint` (mod hints), `redirect_surface`,
//! `blocked_body` and `at_desk` (push text and suppression).

use crate::transcript::{cut, now_ms};
use crate::{App, Shared, herdr, herdr_err, json_err};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const WATCH_TTL_MS: i64 = 24 * 3600_000;
const PLAN_TTL_MS: i64 = 60 * 60_000;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Watch {
    pub id: String,
    pub pane: String,
    pub tid: Option<String>,
    /// "state" | "output" | "job"
    pub kind: String,
    /// state: any of "done", "blocked"
    pub until: Vec<String>,
    /// output: text or regex
    pub pattern: Option<String>,
    pub regex: bool,
    /// job: background job id
    pub job: Option<String>,
    pub label: String,
    pub title: String,
    pub created: i64,
    /// state: completionSeq when the watch was set
    pub cseq0: u64,
    /// output: matching lines already on screen when the watch was set
    pub baseline: Vec<String>,
    /// state: "blocked" only counts once the agent was seen not blocked
    pub armed: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    /// Re-ping a blocked agent after this many minutes (0 = off).
    #[serde(rename = "repingMin")]
    pub reping_min: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { reping_min: 30 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Review {
    /// the agent under review
    pub pane: String,
    pub session: Option<String>,
    pub title: String,
    pub ts: i64,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Disk {
    watches: Vec<Watch>,
    settings: Settings,
    /// reviewer pane -> reviewed agent
    reviews: HashMap<String, Review>,
}

#[derive(Clone)]
struct Plan {
    ts: i64,
    text: String,
    path: Option<String>,
    was_blocked: bool,
}

#[derive(Default)]
struct St {
    disk: Disk,
    /// pane -> plan the mod posted
    plans: HashMap<String, Plan>,
    /// pane -> plan shown in state (mod or transcript), for push bodies
    shown: HashMap<String, Value>,
    /// plan file path -> text (transcript fallback reads the full plan once)
    plan_files: HashMap<String, String>,
    /// pane -> blocked `since` that was already re-pinged
    repinged: HashMap<String, i64>,
}

pub struct V3 {
    st: Mutex<St>,
    path: PathBuf,
}

impl V3 {
    pub fn load(dir: &Path) -> V3 {
        let path = dir.join("v3.json");
        let disk: Disk = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        V3 { st: Mutex::new(St { disk, ..Default::default() }), path }
    }

    fn save(&self, d: &Disk) {
        if let Ok(b) = serde_json::to_vec(d) {
            let _ = std::fs::write(&self.path, b);
        }
    }

    /// The plan currently shown for `pane` (text), for the blocked push.
    pub fn plan_text(&self, pane: &str) -> Option<String> {
        self.st.lock().unwrap().shown.get(pane).and_then(|p| p["text"].as_str().map(str::to_string))
    }
}

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/a/{pane}/focus", post(api_focus))
        .route("/api/a/{pane}/review", post(api_review))
        .route("/api/a/{pane}/fork", post(api_fork))
        .route("/api/new/worktree", post(api_new_worktree))
        .route("/api/watches", get(api_watches).post(api_watch_add))
        .route("/api/watches/{id}/cancel", post(api_watch_cancel))
        .route("/api/settings", get(api_settings_get).post(api_settings_post))
}

// ---------------------------------------------------------------------------
// Plan cards

/// `/api/hint` kinds `plan` {plan, path?} and `plan_clear`.
pub fn plan_hint(app: &App, pane: &str, b: &Value) -> Result<(), &'static str> {
    let mut st = app.v3.st.lock().unwrap();
    match b["kind"].as_str() {
        Some("plan") => {
            let text = b["plan"].as_str().filter(|t| !t.trim().is_empty()).ok_or("plan required")?;
            let path = b["path"].as_str().map(str::to_string);
            st.plans.insert(pane.into(), Plan { ts: now_ms(), text: cut(text, 20_000).0, path, was_blocked: false });
        }
        _ => {
            st.plans.remove(pane);
        }
    }
    Ok(())
}

/// The ExitPlanMode call an agent is waiting on, read from its transcript:
/// (item index, plan text, plan file path).
fn transcript_plan(app: &App, sid: &str) -> Option<(usize, String, Option<String>)> {
    let tx = app.transcripts.lock().unwrap().get(sid).cloned()?;
    let t = tx.try_lock().ok()?;
    if !t.waiting_on("ExitPlanMode") {
        return None;
    }
    let it = t.items.iter().rev().find(|it| it.k == "tool" && it.name.as_deref() == Some("ExitPlanMode"))?;
    let input = it.input.as_ref()?;
    let text = input["plan"].as_str().unwrap_or("").to_string();
    let path = input["planFilePath"].as_str().map(str::to_string);
    Some((it.i, text, path))
}

/// Per-agent v3 fields, set on every refresh before the state is published:
/// `plan` ({text, path, ts} while an ExitPlanMode approval is open, else null),
/// `watches` ([{id, kind, label}]) and `reviewOf` (pane under review, or null).
pub fn decorate(app: &App, agents: &mut [Value]) {
    let now = now_ms();
    // Transcript fallback first (no v3 lock held while touching transcripts).
    let mut fallback: HashMap<String, (usize, String, Option<String>)> = HashMap::new();
    for a in agents.iter() {
        if a["status"] == "blocked" {
            if let (Some(pane), Some(sid)) = (a["pane"].as_str(), a["session"].as_str()) {
                if let Some(p) = transcript_plan(app, sid) {
                    fallback.insert(pane.to_string(), p);
                }
            }
        }
    }
    let mut st = app.v3.st.lock().unwrap();
    let live: HashSet<String> = agents.iter().filter_map(|a| a["pane"].as_str().map(str::to_string)).collect();
    st.plans.retain(|p, _| live.contains(p));
    st.shown.clear();
    for a in agents.iter_mut() {
        let pane = a["pane"].as_str().unwrap_or("").to_string();
        let blocked = a["status"] == "blocked";
        let mut plan = Value::Null;
        let mut drop_plan = false;
        if let Some(p) = st.plans.get_mut(&pane) {
            if blocked {
                p.was_blocked = true;
            }
            if (p.was_blocked && !blocked) || now - p.ts > PLAN_TTL_MS {
                drop_plan = true;
            } else if blocked {
                plan = json!({"text": p.text, "path": p.path, "ts": p.ts});
            }
        }
        if drop_plan {
            st.plans.remove(&pane);
        }
        if plan.is_null() {
            if let Some((i, text, path)) = fallback.remove(&pane) {
                // The transcript input is cut at 2000 chars: read the plan file once for the whole plan.
                let full = path.as_ref().and_then(|p| {
                    if let Some(t) = st.plan_files.get(p) {
                        return Some(t.clone());
                    }
                    let t = std::fs::read_to_string(p).ok().filter(|t| t.len() < 200_000)?;
                    if st.plan_files.len() > 20 {
                        st.plan_files.clear();
                    }
                    st.plan_files.insert(p.clone(), t.clone());
                    Some(t)
                });
                let text = full.filter(|f| f.len() >= text.len()).unwrap_or(text);
                plan = json!({"text": text, "path": path, "ts": a["since"], "i": i});
            }
        }
        if !plan.is_null() {
            st.shown.insert(pane.clone(), plan.clone());
        }
        a["plan"] = plan;
        let ws: Vec<Value> = st
            .disk
            .watches
            .iter()
            .filter(|w| w.pane == pane)
            .map(|w| json!({"id": w.id, "kind": w.kind, "label": w.label}))
            .collect();
        a["watches"] = json!(ws);
        a["reviewOf"] = st.disk.reviews.get(&pane).map(|r| json!(r.pane)).unwrap_or(Value::Null);
    }
    // Reviewer links die with the reviewer pane.
    let before = st.disk.reviews.len();
    // (A reviewer pane is a plain shell until claude starts in it: keep fresh links.)
    st.disk.reviews.retain(|p, r| live.contains(p) || now - r.ts < 10 * 60_000);
    if st.disk.reviews.len() != before {
        app.v3.save(&st.disk);
    }
}

// ---------------------------------------------------------------------------
// Push bodies

/// Body of a "needs you" push: the plan, the AskUserQuestion question or the
/// screen's prompt, followed by the menu's option labels.
pub fn blocked_body(plan: Option<&str>, question: Option<&str>, screen: &str) -> String {
    let menu = crate::parse_menu(screen);
    let opts: Vec<String> =
        menu.iter().map(|o| match o.number { Some(n) => format!("{n}. {}", o.label), None => o.label.clone() }).collect();
    let opts = opts.join(" · ");
    let head = if let Some(p) = plan {
        let lines: Vec<&str> = p.lines().map(|l| l.trim().trim_start_matches('#').trim()).filter(|l| !l.is_empty()).take(4).collect();
        let text = lines.join("\n");
        if text.to_lowercase().starts_with("plan") { text } else { format!("Plan: {text}") }
    } else if let Some(q) = question.filter(|q| !q.trim().is_empty()) {
        q.to_string()
    } else {
        menu_prompt(screen, &menu)
    };
    if opts.is_empty() || head.contains(&opts) { head } else { format!("{head}\n{opts}") }
}

/// The text above a menu (its question), else the screen's last lines.
fn menu_prompt(screen: &str, menu: &[crate::MenuOption]) -> String {
    let useful = |l: &&&str| l.len() > 2 && !l.chars().all(|c| "─━-═╌▔ ".contains(c));
    let lines: Vec<&str> = screen.lines().map(str::trim).collect();
    let first = menu.first().and_then(|o| lines.iter().position(|l| l.contains(o.label.as_str())));
    match first {
        Some(ix) => {
            let above: Vec<&str> = lines[..ix].iter().rev().filter(&useful).take(2).copied().collect();
            above.into_iter().rev().collect::<Vec<_>>().join("\n")
        }
        None => {
            let l: Vec<&str> = lines.iter().filter(&useful).copied().collect();
            l[l.len().saturating_sub(6)..].join("\n")
        }
    }
}

/// Body of a surface push: its body, else the agent; plus the command to run.
pub fn surface_body(body: Option<&str>, command: Option<&str>, who: &str) -> String {
    let b = body.filter(|s| !s.trim().is_empty());
    match (b, command) {
        (Some(b), Some(c)) => format!("{b}\n$ {c}"),
        (None, Some(c)) => format!("$ {c}"),
        (Some(b), None) => b.to_string(),
        (None, None) => who.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Presence: is the user at the desk looking at this agent?

/// True when `pane` is the focused pane of herdr, a herdr client runs on the
/// desktop (not over mosh/ssh), and Hyprland is unlocked, its screens on and
/// no screensaver up. Evaluated only when a push is about to go out.
pub async fn at_desk(app: &App, pane: &str) -> bool {
    if crate::talk::forced_away(app) {
        return false; // `/phone away`
    }
    let focused = {
        let s = app.inner.lock().unwrap().state.clone();
        let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
        v["agents"].as_array().is_some_and(|l| l.iter().any(|a| a["pane"] == pane && a["focused"] == true))
    };
    if !focused || !desktop_client() {
        return false;
    }
    let desk = hypr_present().await;
    if desk {
        eprintln!("push suppressed: {pane} is focused on the desk");
    }
    desk
}

/// A herdr client process whose ancestry has no mosh-server / sshd.
pub(crate) fn desktop_client() -> bool {
    let Ok(rd) = std::fs::read_dir("/proc") else { return false };
    let read = |pid: &str, f: &str| std::fs::read_to_string(format!("/proc/{pid}/{f}")).unwrap_or_default();
    for e in rd.flatten() {
        let pid = e.file_name().to_string_lossy().to_string();
        if !pid.bytes().all(|b| b.is_ascii_digit()) || read(&pid, "comm").trim() != "herdr" {
            continue;
        }
        let cmd = read(&pid, "cmdline").replace('\0', " ");
        if cmd.split_whitespace().nth(1).is_some_and(|a| a == "server" || a == "api" || a == "agent" || a == "pane") {
            continue;
        }
        let mut cur = pid.clone();
        let mut remote = false;
        for _ in 0..30 {
            let stat = read(&cur, "stat");
            let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r.to_string()) else { break };
            let ppid = rest.split_whitespace().nth(1).unwrap_or("0").to_string();
            if ppid == "0" || ppid == "1" {
                break;
            }
            let comm = read(&ppid, "comm");
            let comm = comm.trim();
            if comm.starts_with("mosh-server") || comm == "sshd" || comm.starts_with("sshd-") {
                remote = true;
                break;
            }
            cur = ppid;
        }
        if !remote {
            return true;
        }
    }
    false
}

fn hypr_socket() -> Option<PathBuf> {
    let rt = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| "/run/user/1000".into());
    if let Some(sig) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        let p = rt.join("hypr").join(sig).join(".socket.sock");
        if p.exists() {
            return Some(p);
        }
    }
    // Under systemd the signature is not in the environment: newest instance.
    std::fs::read_dir(rt.join("hypr"))
        .ok()?
        .flatten()
        .map(|e| e.path().join(".socket.sock"))
        .filter(|p| p.exists())
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

async fn hypr(cmd: &str) -> Option<Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let sock = hypr_socket()?;
    let fut = async {
        let mut s = tokio::net::UnixStream::connect(sock).await.ok()?;
        s.write_all(cmd.as_bytes()).await.ok()?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.ok()?;
        serde_json::from_slice(&buf).ok()
    };
    tokio::time::timeout(Duration::from_millis(800), fut).await.ok().flatten()
}

pub(crate) async fn hypr_present() -> bool {
    let Some(mons) = hypr("j/monitors").await else { return false };
    let Some(clients) = hypr("j/clients").await else { return false };
    desk_state(&mons, &clients)
}

/// Unlocked, some screen on, no screensaver window.
fn desk_state(mons: &Value, clients: &Value) -> bool {
    let Some(ms) = mons.as_array().filter(|m| !m.is_empty()) else { return false };
    let locked = ms.iter().any(|m| m["solitaryBlockedBy"].as_array().is_some_and(|b| b.iter().any(|x| x == "LOCK")));
    let on = ms.iter().any(|m| m["dpmsStatus"].as_bool() != Some(false));
    let saver = clients.as_array().is_some_and(|c| {
        c.iter().any(|w| w["class"].as_str().is_some_and(|k| k.contains("screensaver")))
    });
    !locked && on && !saver
}

// ---------------------------------------------------------------------------
// Open on desk

async fn api_focus(State(app): State<Shared>, AxPath(pane): AxPath<String>) -> Response {
    let r = match herdr::quick("agent.focus", json!({"target": pane})).await {
        Ok(v) => Ok(v),
        Err(_) => herdr::quick("pane.focus", json!({"pane_id": pane})).await,
    };
    app.refresh.notify_one();
    match r {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => herdr_err(e),
    }
}

// ---------------------------------------------------------------------------
// Starting agents (fork, review, worktree)

fn agent_json(app: &App, pane: &str) -> Option<Value> {
    let s = app.inner.lock().unwrap().state.clone();
    let mut v: Value = serde_json::from_str(&s).ok()?;
    let list = v["agents"].as_array_mut()?;
    let ix = list.iter().position(|a| a["pane"] == pane)?;
    Some(list.swap_remove(ix))
}

/// Starts claude in `pane` and sends `prompt` once it is ready (as /api/new does).
fn launch(app: &Shared, pane: String, label: &str, args: Vec<String>, prompt: Option<String>) {
    let name = crate::agent_slug(label);
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::start_claude(&pane, &name, args).await {
            eprintln!("agent.start {pane}: {e}");
        }
        app.refresh.notify_one();
        let Some(prompt) = prompt else { return };
        let deadline = now_ms() + 15 * 60_000;
        while now_ms() < deadline {
            if let Ok(a) = herdr::quick("agent.get", json!({"target": pane})).await {
                let st = a["agent"]["agent_status"].as_str().unwrap_or("");
                if (st == "idle" || st == "done")
                    && herdr::quick("agent.prompt", json!({"target": pane, "text": prompt})).await.is_ok()
                {
                    app.refresh.notify_one();
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
    });
}

async fn new_tab(ws: &str, cwd: &str, label: &str) -> Result<(String, String, String), Response> {
    let created = herdr::quick("tab.create", json!({"workspace_id": ws, "cwd": cwd, "label": label, "focus": false}))
        .await
        .map_err(herdr_err)?;
    let pane = created["root_pane"]["pane_id"].as_str().unwrap_or("").to_string();
    let tab = created["tab"]["tab_id"].as_str().or(created["root_pane"]["tab_id"].as_str()).unwrap_or("").to_string();
    let wsid = created["root_pane"]["workspace_id"].as_str().unwrap_or(ws).to_string();
    if pane.is_empty() {
        return Err(json_err(StatusCode::BAD_GATEWAY, "herdr did not return a pane"));
    }
    Ok((pane, tab, wsid))
}

fn short_label(prefix: &str, title: &str) -> String {
    let t: String = title.chars().filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-').collect();
    let t = t.split_whitespace().take(3).collect::<Vec<_>>().join("-").to_lowercase();
    let t: String = t.chars().take(18).collect();
    if t.is_empty() { prefix.to_string() } else { format!("{prefix}-{t}") }
}

/// "Fork from here": `claude --resume <sid> --fork-session` in a new tab, same cwd.
async fn api_fork(State(app): State<Shared>, AxPath(pane): AxPath<String>) -> Response {
    let Some(a) = agent_json(&app, &pane) else { return json_err(StatusCode::NOT_FOUND, "unknown pane") };
    let Some(sid) = a["session"].as_str().map(str::to_string) else { return json_err(StatusCode::CONFLICT, "agent has no session") };
    if crate::find_session(&app, &sid).is_none() {
        return json_err(StatusCode::CONFLICT, "no transcript yet: nothing to fork");
    }
    let cwd = a["cwd"].as_str().unwrap_or("").to_string();
    let ws = a["ws"].as_str().unwrap_or("").to_string();
    let label = short_label("fork", a["title"].as_str().unwrap_or(""));
    let (np, tab, wsid) = match new_tab(&ws, &cwd, &label).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let args = vec!["--permission-mode".into(), "auto".into(), "--resume".into(), sid, "--fork-session".into()];
    launch(&app, np.clone(), &label, args, None);
    app.refresh.notify_one();
    Json(json!({"pane": np, "tab": tab, "ws": wsid})).into_response()
}

/// Files touched by a session and its subagents (paths, newest first).
fn touched_files(app: &App, tid: &str) -> Vec<String> {
    let mut files: HashMap<String, i64> = HashMap::new();
    crate::each_session_transcript(app, tid, |_, t| {
        for (p, touch) in &t.touched {
            let e = files.entry(p.clone()).or_insert(0);
            *e = (*e).max(touch.last_ts);
        }
    });
    let mut v: Vec<(String, i64)> = files.into_iter().collect();
    v.sort_by_key(|(_, ts)| -ts);
    v.into_iter().map(|(p, _)| p).collect()
}

fn review_prompt(title: &str, pane: &str, cwd: &str, files: &[String], git: &Value) -> String {
    let mut s = format!(
        "Review the recent changes another Claude agent made: \"{title}\" (herdr pane {pane}), working in {cwd}.\n\n"
    );
    if files.is_empty() {
        s.push_str("It has no recorded file edits; review the uncommitted changes in the repo (`git status`, `git diff`).\n");
    } else {
        s.push_str("Files it touched (newest first):\n");
        for f in files.iter().take(40) {
            s.push_str(&format!("- {f}\n"));
        }
        if files.len() > 40 {
            s.push_str(&format!("- …and {} more\n", files.len() - 40));
        }
    }
    if let Some(ch) = git["changed"].as_array().filter(|c| !c.is_empty()) {
        s.push_str(&format!(
            "\nThe repo is on branch {} with {} uncommitted change(s); `git diff` shows them.\n",
            git["branch"].as_str().unwrap_or("?"),
            ch.len()
        ));
    }
    s.push_str(
        "\nRead the changed code and look for bugs, regressions, unhandled edge cases, security problems and leftover \
         debug code. Do not edit any files and do not message the other agent.\n\
         When you are done, call mcp__sheprd__surface exactly once with level \"done\", title \"Review: <one-line verdict>\" \
         and body = your findings as a short markdown list, most important first, each with file:line. \
         The card is shown in the reviewed agent's chat on the user's phone, where they can send the findings to it. \
         Then end your turn.",
    );
    s
}

/// "Review this": a reviewer agent in a new tab (same workspace and cwd) whose
/// surfaced result lands on the reviewed agent's chat.
async fn api_review(State(app): State<Shared>, AxPath(pane): AxPath<String>) -> Response {
    let Some(a) = agent_json(&app, &pane) else { return json_err(StatusCode::NOT_FOUND, "unknown pane") };
    let sid = a["session"].as_str().map(str::to_string);
    let cwd = a["cwd"].as_str().unwrap_or("").to_string();
    let ws = a["ws"].as_str().unwrap_or("").to_string();
    let title = a["title"].as_str().unwrap_or("agent").to_string();
    let files = match &sid {
        Some(s) => {
            let (app2, s2) = (app.clone(), s.clone());
            let f = tokio::task::spawn_blocking(move || touched_files(&app2, &s2)).await.unwrap_or_default();
            crate::trim_heap();
            f
        }
        None => Vec::new(),
    };
    let git = crate::git_status(&cwd).await;
    let prompt = review_prompt(&title, &pane, &cwd, &files, &git);
    let label = short_label("review", &title);
    let (np, tab, wsid) = match new_tab(&ws, &cwd, &label).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    {
        let mut st = app.v3.st.lock().unwrap();
        st.disk.reviews.insert(np.clone(), Review { pane: pane.clone(), session: sid, title, ts: now_ms() });
        app.v3.save(&st.disk);
    }
    launch(&app, np.clone(), &label, vec!["--permission-mode".into(), "auto".into()], Some(prompt));
    app.refresh.notify_one();
    Json(json!({"pane": np, "tab": tab, "ws": wsid, "files": files.len()})).into_response()
}

/// A surface from a reviewer agent goes to the reviewed agent's chat:
/// rewrites `pane`/`session` and adds `review: {pane, title}` (the reviewer).
pub fn redirect_surface(app: &App, b: &mut Value) {
    let pane = b["pane"].as_str().map(str::to_string).or_else(|| {
        let s = b["session"].as_str()?;
        crate::pane_for_session(app, s)
    });
    let Some(pane) = pane else { return };
    let r = app.v3.st.lock().unwrap().disk.reviews.get(&pane).cloned();
    let Some(r) = r else { return };
    let reviewer_title = app.inner.lock().unwrap().pane_titles.get(&pane).cloned();
    b["review"] = json!({"pane": pane, "title": reviewer_title});
    b["pane"] = json!(r.pane);
    b["session"] = r.session.map(Value::String).unwrap_or(Value::Null);
}

#[derive(Deserialize)]
struct WorktreeBody {
    cwd: String,
    branch: Option<String>,
    label: Option<String>,
    prompt: Option<String>,
}

fn branch_name(label: &str) -> String {
    let s: String = label
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '/' || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let s = s.split('-').filter(|x| !x.is_empty()).collect::<Vec<_>>().join("-");
    let s = s.trim_matches(|c| c == '/' || c == '.').to_string();
    if s.is_empty() { format!("agent-{:x}", now_ms() & 0xffff) } else { s.chars().take(40).collect() }
}

/// New agent in a fresh git worktree: herdr `worktree.create` (its own
/// workspace), else `git worktree add` next to the repo plus a new workspace.
async fn api_new_worktree(State(app): State<Shared>, Json(b): Json<WorktreeBody>) -> Response {
    if !Path::new(&b.cwd).is_dir() {
        return json_err(StatusCode::BAD_REQUEST, "cwd is not a directory");
    }
    let root = tokio::process::Command::new("git").args(["-C", &b.cwd, "rev-parse", "--show-toplevel"]).output().await;
    let Some(root) = root.ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()) else {
        return json_err(StatusCode::BAD_REQUEST, "not a git repository");
    };
    let label = b.label.clone().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| {
        format!("{}-{:x}", Path::new(&root).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or("wt".into()), now_ms() & 0xfff)
    });
    let branch = branch_name(b.branch.as_deref().filter(|x| !x.trim().is_empty()).unwrap_or(&label));
    // (herdr groups worktree workspaces under one for the source checkout, which
    // it opens too when none is open yet.)
    let created = herdr::quick("worktree.create", json!({"cwd": root, "branch": branch, "label": label, "focus": false})).await;
    let (pane, ws, path) = match created {
        Ok(c) => (
            c["root_pane"]["pane_id"].as_str().unwrap_or("").to_string(),
            c["workspace"]["workspace_id"].as_str().unwrap_or("").to_string(),
            c["worktree"]["path"].as_str().unwrap_or("").to_string(),
        ),
        Err(e) => {
            eprintln!("worktree.create failed ({e}); falling back to git worktree add");
            let parent = Path::new(&root).parent().unwrap_or(Path::new("/tmp"));
            let name = Path::new(&root).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or("repo".into());
            let path = parent.join(format!("{name}-wt-{}", branch.replace('/', "-"))).to_string_lossy().to_string();
            let out = tokio::process::Command::new("git").args(["-C", &root, "worktree", "add", "-b", &branch, &path]).output().await;
            match out {
                Ok(o) if o.status.success() => {}
                Ok(o) => return json_err(StatusCode::CONFLICT, String::from_utf8_lossy(&o.stderr).trim().to_string()),
                Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, e),
            }
            match herdr::quick("workspace.create", json!({"cwd": path, "label": label, "focus": false})).await {
                Ok(c) => (
                    c["root_pane"]["pane_id"].as_str().unwrap_or("").to_string(),
                    c["root_pane"]["workspace_id"].as_str().unwrap_or("").to_string(),
                    path,
                ),
                Err(e) => return herdr_err(e),
            }
        }
    };
    if pane.is_empty() {
        return json_err(StatusCode::BAD_GATEWAY, "herdr did not return a pane");
    }
    let prompt = b.prompt.clone().filter(|p| !p.trim().is_empty());
    launch(&app, pane.clone(), &label, vec!["--permission-mode".into(), "auto".into()], prompt);
    app.refresh.notify_one();
    Json(json!({"pane": pane, "ws": ws, "path": path, "branch": branch})).into_response()
}

// ---------------------------------------------------------------------------
// Watches

#[derive(Deserialize)]
struct WatchBody {
    pane: String,
    kind: String,
    until: Option<Vec<String>>,
    pattern: Option<String>,
    regex: Option<bool>,
    job: Option<String>,
}

fn matcher(pattern: &str, regex: bool) -> Result<Box<dyn Fn(&str) -> bool + Send>, String> {
    if regex {
        let re = regex_lite::Regex::new(pattern).map_err(|e| format!("bad regex: {e}"))?;
        Ok(Box::new(move |l: &str| re.is_match(l)))
    } else {
        let p = pattern.to_lowercase();
        Ok(Box::new(move |l: &str| l.to_lowercase().contains(&p)))
    }
}

/// Lines of the pane's recent output (ANSI stripped, trimmed, non-empty).
async fn recent_lines(pane: &str) -> Option<Vec<String>> {
    let r = herdr::quick("pane.read", json!({"pane_id": pane, "source": "recent", "lines": 400, "format": "text", "strip_ansi": true}))
        .await
        .ok()?;
    Some(r["read"]["text"].as_str()?.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
}

async fn api_watches(State(app): State<Shared>) -> Response {
    let list = app.v3.st.lock().unwrap().disk.watches.clone();
    let out: Vec<Value> = list.iter().map(watch_json).collect();
    Json(json!({"watches": out})).into_response()
}

fn watch_json(w: &Watch) -> Value {
    json!({"id": w.id, "pane": w.pane, "tid": w.tid, "kind": w.kind, "until": w.until, "pattern": w.pattern,
           "regex": w.regex, "job": w.job, "label": w.label, "title": w.title, "created": w.created})
}

async fn api_watch_add(State(app): State<Shared>, Json(b): Json<WatchBody>) -> Response {
    let Some(a) = agent_json(&app, &b.pane) else { return json_err(StatusCode::NOT_FOUND, "unknown pane") };
    let title = if a["home"] == true { "Master".to_string() } else { a["title"].as_str().unwrap_or(&b.pane).to_string() };
    let mut w = Watch {
        id: format!("w{:x}", now_ms()),
        pane: b.pane.clone(),
        tid: a["tid"].as_str().map(str::to_string),
        kind: b.kind.clone(),
        title,
        created: now_ms(),
        cseq0: a["completionSeq"].as_u64().unwrap_or(0),
        armed: a["status"] != "blocked",
        ..Default::default()
    };
    match b.kind.as_str() {
        "state" => {
            let until: Vec<String> =
                b.until.unwrap_or_else(|| vec!["done".into(), "blocked".into()]).into_iter().filter(|u| u == "done" || u == "blocked").collect();
            if until.is_empty() {
                return json_err(StatusCode::BAD_REQUEST, "until must include done and/or blocked");
            }
            w.label = match (until.contains(&"done".into()), until.contains(&"blocked".into())) {
                (true, true) => "done or needs you".into(),
                (true, false) => "done".into(),
                _ => "needs you".into(),
            };
            w.until = until;
        }
        "output" => {
            let Some(p) = b.pattern.filter(|p| !p.trim().is_empty() && p.len() <= 200) else {
                return json_err(StatusCode::BAD_REQUEST, "pattern (1-200 chars) required");
            };
            let regex = b.regex.unwrap_or(false);
            let m = match matcher(&p, regex) {
                Ok(m) => m,
                Err(e) => return json_err(StatusCode::BAD_REQUEST, e),
            };
            // Only new output counts: remember the matching lines already there.
            let lines = recent_lines(&b.pane).await.unwrap_or_default();
            let mut base: Vec<String> = lines.into_iter().filter(|l| m(l)).collect();
            base.dedup();
            base.truncate(200);
            w.baseline = base;
            w.label = if regex { format!("output ~ /{p}/") } else { format!("output has \"{p}\"") };
            w.pattern = Some(p);
            w.regex = regex;
        }
        "job" => {
            let Some(j) = b.job.filter(|j| !j.is_empty()) else { return json_err(StatusCode::BAD_REQUEST, "job id required") };
            let known = w.tid.as_deref().and_then(|t| app.transcripts.lock().unwrap().get(t).cloned()).and_then(|tx| {
                let t = tx.lock().ok()?;
                t.jobs.iter().find(|x| x.id == j).map(|x| (x.desc.clone().unwrap_or_else(|| cut(&x.cmd, 40).0), x.ended.is_some()))
            });
            let Some((desc, ended)) = known else { return json_err(StatusCode::NOT_FOUND, "no such job for this agent") };
            if ended {
                return json_err(StatusCode::CONFLICT, "job already finished");
            }
            w.label = format!("job ends: {desc}");
            w.job = Some(j);
        }
        _ => return json_err(StatusCode::BAD_REQUEST, "kind must be state, output or job"),
    }
    let id = w.id.clone();
    {
        let mut st = app.v3.st.lock().unwrap();
        st.disk.watches.push(w.clone());
        app.v3.save(&st.disk);
    }
    app.refresh.notify_one();
    Json(json!({"ok": true, "id": id, "watch": watch_json(&w)})).into_response()
}

async fn api_watch_cancel(State(app): State<Shared>, AxPath(id): AxPath<String>) -> Response {
    let found = {
        let mut st = app.v3.st.lock().unwrap();
        let n = st.disk.watches.len();
        st.disk.watches.retain(|w| w.id != id);
        let found = st.disk.watches.len() != n;
        if found {
            app.v3.save(&st.disk);
        }
        found
    };
    app.refresh.notify_one();
    if found { Json(json!({"ok": true})).into_response() } else { json_err(StatusCode::NOT_FOUND, "no such watch") }
}

async fn api_settings_get(State(app): State<Shared>) -> Response {
    let s = app.v3.st.lock().unwrap().disk.settings.clone();
    Json(json!(s)).into_response()
}

async fn api_settings_post(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let mut st = app.v3.st.lock().unwrap();
    if let Some(m) = b.get("repingMin") {
        let Some(m) = m.as_u64().filter(|m| *m <= 24 * 60) else { return json_err(StatusCode::BAD_REQUEST, "repingMin must be 0-1440") };
        st.disk.settings.reping_min = m as u32;
    }
    app.v3.save(&st.disk);
    Json(json!(st.disk.settings)).into_response()
}

/// What a state watch says about an agent now: Some(reason) when it fires.
fn state_fires(w: &Watch, status: &str, cseq: u64) -> Option<&'static str> {
    if w.until.iter().any(|u| u == "blocked") && status == "blocked" && w.armed {
        return Some("needs you");
    }
    if w.until.iter().any(|u| u == "done") && cseq > w.cseq0 && (status == "idle" || status == "done") {
        return Some("done");
    }
    None
}

/// The watch / re-ping loop: every 2 s, only does work while there are
/// watches or blocked agents.
pub async fn run(app: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        tick(&app).await;
    }
}

async fn tick(app: &Shared) {
    let (watches, reping) = {
        let st = app.v3.st.lock().unwrap();
        (st.disk.watches.clone(), st.disk.settings.reping_min)
    };
    let state = app.inner.lock().unwrap().state.clone();
    if watches.is_empty() && (reping == 0 || !state.contains("\"blocked\"")) {
        return;
    }
    let v: Value = serde_json::from_str(&state).unwrap_or(Value::Null);
    let Some(agents) = v["agents"].as_array() else { return };
    if v["herdr"] != true {
        return;
    }
    let by: HashMap<&str, &Value> = agents.iter().filter_map(|a| Some((a["pane"].as_str()?, a))).collect();
    let now = now_ms();
    let mut fired: Vec<(String, Value)> = Vec::new();
    let mut arm: Vec<String> = Vec::new();
    for w in &watches {
        let url = format!("/#/a/{}", w.pane);
        let Some(a) = by.get(w.pane.as_str()) else {
            fired.push((w.id.clone(), json!({"title": format!("Watch ended · {}", w.title), "body": "The agent was closed.", "url": "/#/agents"})));
            continue;
        };
        if now - w.created > WATCH_TTL_MS {
            fired.push((w.id.clone(), Value::Null)); // expired quietly
            continue;
        }
        match w.kind.as_str() {
            "state" => {
                let status = a["status"].as_str().unwrap_or("");
                if !w.armed && status != "blocked" {
                    arm.push(w.id.clone());
                }
                if let Some(why) = state_fires(w, status, a["completionSeq"].as_u64().unwrap_or(0)) {
                    let body = if why == "done" {
                        a["last"]["text"].as_str().map(|t| cut(t, 300).0).unwrap_or_else(|| "Finished.".into())
                    } else {
                        a["ask"]["questions"][0]["question"]
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| a["plan"]["text"].as_str().map(|p| format!("Plan ready: {}", p.lines().next().unwrap_or("").trim_start_matches('#').trim())))
                            .unwrap_or_else(|| "Waiting for you.".into())
                    };
                    let url = match (why, a["answerI"].as_u64()) {
                        ("done", Some(i)) => format!("{url}?i={i}"),
                        _ => url,
                    };
                    fired.push((w.id.clone(), json!({"title": format!("{} · {}", if why == "done" { "Done" } else { "Needs you" }, w.title), "body": body, "url": url})));
                }
            }
            "job" => {
                let tx = w.tid.as_deref().and_then(|t| app.transcripts.lock().unwrap().get(t).cloned());
                let ended = tx.and_then(|tx| {
                    let t = tx.try_lock().ok()?;
                    let j = t.jobs.iter().find(|x| Some(&x.id) == w.job.as_ref())?;
                    j.ended.map(|_| (j.status, j.desc.clone().unwrap_or_else(|| cut(&j.cmd, 80).0), j.i))
                });
                if let Some((status, desc, i)) = ended {
                    fired.push((w.id.clone(), json!({"title": format!("Job {status} · {}", w.title), "body": desc, "url": format!("{url}?i={i}")})));
                }
            }
            "output" => {
                let Ok(m) = matcher(w.pattern.as_deref().unwrap_or(""), w.regex) else {
                    fired.push((w.id.clone(), Value::Null));
                    continue;
                };
                if let Some(lines) = recent_lines(&w.pane).await {
                    if let Some(hit) = lines.iter().rev().find(|l| m(l) && !w.baseline.contains(l)) {
                        fired.push((w.id.clone(), json!({"title": format!("Output matched · {}", w.title), "body": cut(hit, 300).0, "url": url})));
                    }
                }
            }
            _ => fired.push((w.id.clone(), Value::Null)),
        }
    }
    if !arm.is_empty() {
        let mut st = app.v3.st.lock().unwrap();
        for w in st.disk.watches.iter_mut().filter(|w| arm.contains(&w.id)) {
            w.armed = true;
        }
        app.v3.save(&st.disk);
    }
    if !fired.is_empty() {
        {
            let mut st = app.v3.st.lock().unwrap();
            st.disk.watches.retain(|w| !fired.iter().any(|(id, _)| *id == w.id));
            app.v3.save(&st.disk);
        }
        app.refresh.notify_one();
        for (id, mut p) in fired {
            if p.is_null() {
                continue;
            }
            p["tag"] = json!(id);
            eprintln!("watch {id} fired: {}", p["title"]);
            crate::send_push(app, &p, true).await;
        }
    }
    // Re-ping agents blocked for longer than the setting, once per blocked episode.
    if reping > 0 {
        let mut due = Vec::new();
        {
            let mut st = app.v3.st.lock().unwrap();
            st.repinged.retain(|p, _| by.get(p.as_str()).is_some_and(|a| a["status"] == "blocked"));
            for a in agents {
                let (Some(pane), Some(since)) = (a["pane"].as_str(), a["since"].as_i64()) else { continue };
                if a["status"] != "blocked" || a["muted"] == true || now - since < reping as i64 * 60_000 || st.repinged.get(pane) == Some(&since) {
                    continue;
                }
                st.repinged.insert(pane.to_string(), since);
                due.push((*a).clone());
            }
        }
        for a in due {
            let pane = a["pane"].as_str().unwrap_or("").to_string();
            if at_desk(app, &pane).await {
                continue;
            }
            let mins = (now - a["since"].as_i64().unwrap_or(now)) / 60_000;
            let title = if a["home"] == true { "Master".to_string() } else { a["title"].as_str().unwrap_or(&pane).to_string() };
            let what = a["plan"]["text"]
                .as_str()
                .map(|p| format!("Plan waiting for approval: {}", p.lines().map(|l| l.trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("")))
                .or_else(|| a["ask"]["questions"][0]["question"].as_str().map(str::to_string))
                .unwrap_or_else(|| "Still waiting for you.".into());
            let p = json!({"title": format!("Still waiting {mins} min · {title}"), "body": cut(&what, 300).0,
                           "url": format!("/#/a/{pane}"), "tag": pane});
            eprintln!("re-ping {pane} after {mins} min");
            crate::send_push(app, &p, false).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies() {
        let screen = " Do you want to run this?\n  rm -rf build\n\n ❯ 1. Yes\n   2. No, tell Claude\n\n Esc to cancel";
        assert_eq!(blocked_body(None, None, screen), "Do you want to run this?\nrm -rf build\n1. Yes · 2. No, tell Claude");
        assert_eq!(blocked_body(None, Some("Tea or coffee?"), ""), "Tea or coffee?");
        let plan = "# Plan: create hello.txt\n\n- write it\n- check it\n- done\n- extra";
        let s = " Would you like to proceed?\n ❯ 1. Yes, and use auto mode\n   2. Yes, manually approve edits\n   3. Tell Claude what to change";
        assert_eq!(
            blocked_body(Some(plan), None, s),
            "Plan: create hello.txt\n- write it\n- check it\n- done\n1. Yes, and use auto mode · 2. Yes, manually approve edits · 3. Tell Claude what to change"
        );
        assert_eq!(surface_body(Some("hi"), Some("sudo x"), "a"), "hi\n$ sudo x");
        assert_eq!(surface_body(Some(" "), None, "agent"), "agent");
        assert_eq!(surface_body(None, Some("ls"), "agent"), "$ ls");
    }

    #[test]
    fn desk() {
        let mon = |lock: bool, dpms: bool| json!([{"dpmsStatus": dpms, "solitaryBlockedBy": if lock { json!(["LOCK", "WINDOWED"]) } else { json!(["WINDOWED"]) }}]);
        let none = json!([{"class": "Alacritty"}]);
        assert!(desk_state(&mon(false, true), &none));
        assert!(!desk_state(&mon(true, true), &none));
        assert!(!desk_state(&mon(false, false), &none));
        assert!(!desk_state(&mon(false, true), &json!([{"class": "org.omarchy.screensaver"}])));
        assert!(!desk_state(&json!([]), &none));
    }

    #[test]
    fn watches() {
        let w = Watch { until: vec!["done".into()], cseq0: 3, ..Default::default() };
        assert_eq!(state_fires(&w, "idle", 3), None);
        assert_eq!(state_fires(&w, "working", 4), None);
        assert_eq!(state_fires(&w, "done", 4), Some("done"));
        assert_eq!(state_fires(&w, "blocked", 4), None);
        let mut b = Watch { until: vec!["done".into(), "blocked".into()], cseq0: 3, ..Default::default() };
        assert_eq!(state_fires(&b, "blocked", 3), None); // was already blocked when set
        b.armed = true;
        assert_eq!(state_fires(&b, "blocked", 3), Some("needs you"));
        let m = matcher("err(or)?\\b", true).unwrap();
        assert!(m("an error here") && !m("errand"));
        let m = matcher("Build OK", false).unwrap();
        assert!(m("build ok!"));
        assert!(matcher("(", true).is_err());
    }

    #[test]
    fn names() {
        assert_eq!(branch_name("Fix the Login bug!"), "fix-the-login-bug");
        assert_eq!(short_label("review", "Clone personal repos to ~/Work"), "review-clone-personal-rep");
        assert_eq!(short_label("fork", ""), "fork");
        let p = review_prompt("T", "w6:p1", "/w", &["/w/a.rs".into()], &Value::Null);
        assert!(p.contains("- /w/a.rs") && p.contains("mcp__sheprd__surface") && p.contains("w6:p1"));
    }
}

#[cfg(test)]
mod live {
    /// Manual check against this machine: `cargo test --release -- --ignored presence --nocapture`.
    #[test]
    #[ignore]
    fn presence() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let hypr = rt.block_on(super::hypr_present());
        let mons = rt.block_on(super::hypr("j/monitors"));
        eprintln!("monitors: {}", mons.map(|m| m.to_string().chars().take(200).collect::<String>()).unwrap_or_default());
        eprintln!("desktop herdr client: {} · hyprland present: {hypr}", super::desktop_client());
    }
}
