//! Fleet features: the index of every past Claude session (Sessions list +
//! Resume), the parent/child agent tree, and "why is it blocked?" (herdr
//! `agent.explain`).
//!
//! The session index is incremental: each main transcript is read once from
//! byte 0, then only its appended bytes; the index is persisted in
//! `<state dir>/sessions-index.json` so a restart does not reparse anything.
//! Hooks from main.rs: `routes`, `decorate` (per-agent `parent`).

use crate::transcript::{cut, now_ms};
use crate::{App, Shared, herdr, herdr_err, json_err};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

const MAX_LINE: usize = 256 * 1024;
const RESCAN_MS: i64 = 2000;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/{sid}/resume", post(api_resume))
        .route("/api/a/{pane}/explain", get(api_explain))
        .route("/api/fleet/tree", get(api_tree))
}

// ---------------------------------------------------------------------------
// Session index

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default)]
struct Entry {
    sid: String,
    cwd: Option<String>,
    ai: Option<String>,
    custom: Option<String>,
    first: Option<String>,
    last: Option<String>,
    created: i64,
    mtime: i64,
    size: u64,
    off: u64,
    prompts: u32,
    /// uuid of the first user row: a `--fork-session` copy shares it.
    fu: Option<String>,
    headless: bool,
    branch: Option<String>,
    /// last prompt the user typed (not another agent's message), ms
    lu: i64,
    /// last assistant text, ≤ 200 chars
    la: Option<String>,
    color: Option<String>,
    /// the conversation went on in this session (Claude's `continued-in` record)
    cont: Option<String>,
    /// index format: entries older than `IDX_V` are read again from the start
    v: u8,
}

/// Bump to re-read every transcript once (new fields in `Entry`).
const IDX_V: u8 = 2;

impl Entry {
    fn title(&self) -> Option<String> {
        self.custom.clone().or_else(|| self.ai.clone()).or_else(|| self.first.as_deref().map(|f| short_chars(f, 80)))
    }
}

#[derive(Default)]
struct Index {
    by_path: HashMap<String, Entry>,
    scanned: i64,
    scan_ms: u64,
    loaded: bool,
}

static IDX: LazyLock<Mutex<Index>> = LazyLock::new(|| Mutex::new(Index::default()));

fn index_file() -> PathBuf {
    crate::state_dir().join("sessions-index.json")
}

fn short_chars(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut c: String = flat.chars().take(max.saturating_sub(1)).collect();
    c.push('…');
    c
}

fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Reads the bytes appended to `path` since `e.off` and folds them into `e`.
/// A trailing partial line is left for the next pass.
fn update_entry(e: &mut Entry, path: &Path, size: u64) {
    let Ok(mut f) = std::fs::File::open(path) else { return };
    if f.seek(SeekFrom::Start(e.off)).is_err() {
        return;
    }
    let mut r = BufReader::with_capacity(64 * 1024, f.by_ref().take(size - e.off));
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if buf.last() != Some(&b'\n') {
            break; // partial line: still being written
        }
        e.off += n as u64;
        if n <= MAX_LINE {
            fold_line(e, &buf);
        }
    }
}

fn fold_line(e: &mut Entry, line: &[u8]) {
    let user = has(line, b"\"type\":\"user\"") && !has(line, b"\"tool_use_id\"");
    let interesting = user
        || has(line, b"queued_command")
        || has(line, b"\"type\":\"ai-title\"")
        || has(line, b"\"type\":\"custom-title\"")
        || has(line, b"\"type\":\"agent-color\"")
        || has(line, b"\"type\":\"agent-name\"")
        || has(line, b"\"type\":\"continued-in\"")
        || (has(line, b"\"type\":\"assistant\"") && has(line, b"\"type\":\"text\"") && !has(line, b"\"isSidechain\":true"))
        || (e.cwd.is_none() && has(line, b"\"cwd\":\""))
        || (e.created == 0 && has(line, b"\"timestamp\":\""));
    if !interesting {
        return;
    }
    let Ok(v) = serde_json::from_slice::<Value>(line) else { return };
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    if e.cwd.is_none() {
        e.cwd = s("cwd");
    }
    if e.created == 0 {
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str) {
            e.created = crate::transcript::parse_ts(ts);
        }
    }
    match v.get("type").and_then(Value::as_str) {
        Some("ai-title") => e.ai = s("aiTitle").filter(|t| !t.is_empty()),
        Some("custom-title") => e.custom = s("customTitle").or_else(|| s("title")).filter(|t| !t.is_empty()),
        Some("agent-name") if e.custom.is_none() => e.custom = s("agentName").filter(|t| !t.is_empty()),
        Some("agent-color") => e.color = s("agentColor").filter(|c| !c.is_empty() && c != "default"),
        Some("continued-in") => e.cont = s("continuedInSessionId"),
        Some("assistant") => {
            let t: Vec<&str> = v["message"]["content"].as_array().into_iter().flatten()
                .filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
            let t = t.join(" ");
            if !t.trim().is_empty() {
                e.la = Some(short_chars(&t, 200));
                e.cont = None;
            }
        }
        Some("user") => {
            if e.fu.is_none() {
                e.fu = s("uuid");
            }
            if s("entrypoint").as_deref() == Some("sdk-cli") {
                e.headless = true;
            }
            if let Some(b) = s("gitBranch").filter(|b| !b.is_empty() && b != "HEAD") {
                e.branch = Some(b);
            }
        }
        _ => {}
    }
    if let Some((text, _)) = crate::scan::human_prompt(&v) {
        let t = short_chars(&text, 200);
        if e.first.is_none() {
            e.first = Some(t.clone());
        }
        if !from_agent(&text) {
            let ts = v.get("timestamp").and_then(Value::as_str).map(crate::transcript::parse_ts).unwrap_or(0);
            e.lu = e.lu.max(ts);
        }
        e.last = Some(t);
        e.prompts += 1;
        e.cont = None;
    }
}

/// A prompt another agent or a mod typed in (a steer with `from`), not the user.
fn from_agent(text: &str) -> bool {
    let t = text.trim_start();
    ["[Message from agent", "[from the ", "Another Claude session"].iter().any(|p| t.starts_with(p))
}

/// Brings the index up to date (stat every main transcript, read appended bytes).
fn scan_index() {
    let t0 = std::time::Instant::now();
    let mut idx = IDX.lock().unwrap();
    if !idx.loaded {
        idx.loaded = true;
        if let Ok(b) = std::fs::read(index_file()) {
            if let Ok(m) = serde_json::from_slice::<HashMap<String, Entry>>(&b) {
                idx.by_path = m;
            }
        }
    }
    let files = crate::scan::main_transcripts();
    let mut seen = HashSet::new();
    let mut dirty = false;
    for (p, mtime) in files {
        let key = p.to_string_lossy().to_string();
        seen.insert(key.clone());
        let Ok(meta) = std::fs::metadata(&p) else { continue };
        let size = meta.len();
        let sid = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let e = idx.by_path.entry(key).or_insert_with(|| Entry { sid: sid.clone(), v: IDX_V, ..Default::default() });
        if e.size == size && e.mtime == mtime && e.v >= IDX_V {
            continue;
        }
        if size < e.off || e.v < IDX_V {
            *e = Entry { sid, v: IDX_V, ..Default::default() }; // rewritten (or an older index format): start over
        }
        e.size = size;
        e.mtime = mtime;
        if size > e.off {
            update_entry(e, &p, size);
        }
        dirty = true;
    }
    let before = idx.by_path.len();
    idx.by_path.retain(|k, _| seen.contains(k));
    dirty |= idx.by_path.len() != before;
    idx.scanned = now_ms();
    idx.scan_ms = t0.elapsed().as_millis() as u64;
    if dirty {
        if let Ok(s) = serde_json::to_vec(&idx.by_path) {
            let tmp = index_file().with_extension("json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, index_file());
            }
        }
    }
}

async fn fresh_index() {
    let stale = now_ms() - IDX.lock().unwrap().scanned > RESCAN_MS;
    if stale {
        let _ = tokio::task::spawn_blocking(scan_index).await;
    }
}

/// One past or live session, as find.rs (agent list, Recent, search) sees it.
#[derive(Clone, Debug, Default)]
pub struct Sess {
    pub sid: String,
    pub cwd: Option<String>,
    /// custom title (`/rename`, Haiku's auto-name) or agent name
    pub custom: Option<String>,
    pub ai: Option<String>,
    pub first: Option<String>,
    pub last: Option<String>,
    pub last_asst: Option<String>,
    pub color: Option<String>,
    pub mtime: i64,
    pub created: i64,
    pub last_user: i64,
    pub prompts: u32,
    pub headless: bool,
    pub cont: Option<String>,
}

/// Every indexed main session (index brought up to date first, at most every 2 s).
pub async fn sessions() -> Vec<Sess> {
    fresh_index().await;
    snapshot()
}

/// The index as it is (no rescan): for state decoration.
pub fn snapshot() -> Vec<Sess> {
    let idx = IDX.lock().unwrap();
    idx.by_path
        .values()
        .map(|e| Sess {
            sid: e.sid.clone(), cwd: e.cwd.clone(), custom: e.custom.clone(), ai: e.ai.clone(), first: e.first.clone(),
            last: e.last.clone(), last_asst: e.la.clone(), color: e.color.clone(), mtime: e.mtime, created: e.created,
            last_user: e.lu, prompts: e.prompts, headless: e.headless, cont: e.cont.clone(),
        })
        .collect()
}

fn entry_for(sid: &str) -> Option<Entry> {
    IDX.lock().unwrap().by_path.values().find(|e| e.sid == sid).cloned()
}

#[derive(Deserialize)]
struct SessQ {
    q: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    /// 1 → include headless (`claude -p`) runs
    all: Option<u8>,
}

fn state_agents(app: &App) -> Vec<Value> {
    let s = app.inner.lock().unwrap().state.clone();
    let mut v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
    v["agents"].as_array_mut().map(std::mem::take).unwrap_or_default()
}

/// `true` when every whitespace-separated token of `q` (lowercase) is in `hay`.
fn matches(hay: &str, q: &str) -> bool {
    q.split_whitespace().all(|t| hay.contains(t))
}

async fn api_sessions(State(app): State<Shared>, Query(q): Query<SessQ>) -> Response {
    fresh_index().await;
    let live: HashMap<String, (String, String)> = state_agents(&app)
        .iter()
        .filter_map(|a| Some((a["session"].as_str()?.to_string(), (a["pane"].as_str()?.to_string(), a["status"].as_str().unwrap_or("").to_string()))))
        .collect();
    let query = q.q.as_deref().unwrap_or("").trim().to_lowercase();
    let all = q.all.unwrap_or(0) == 1;
    let (mut list, indexed, scan_ms) = {
        let idx = IDX.lock().unwrap();
        let list: Vec<Entry> = idx
            .by_path
            .values()
            .filter(|e| e.prompts > 0 || e.ai.is_some() || e.custom.is_some())
            .filter(|e| all || !e.headless || live.contains_key(&e.sid))
            .filter(|e| {
                query.is_empty() || {
                    let hay = format!(
                        "{} {} {} {} {} {}",
                        e.title().unwrap_or_default(),
                        e.first.as_deref().unwrap_or(""),
                        e.last.as_deref().unwrap_or(""),
                        e.cwd.as_deref().unwrap_or(""),
                        e.branch.as_deref().unwrap_or(""),
                        e.sid
                    )
                    .to_lowercase();
                    matches(&hay, &query)
                }
            })
            .cloned()
            .collect();
        (list, idx.by_path.len(), idx.scan_ms)
    };
    list.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    let total = list.len();
    let off = q.offset.unwrap_or(0).min(total);
    let lim = q.limit.unwrap_or(60).clamp(1, 500);
    let out: Vec<Value> = list[off..(off + lim).min(total)]
        .iter()
        .map(|e| {
            let l = live.get(&e.sid);
            json!({
                "sid": e.sid, "title": e.title(), "cwd": e.cwd, "first": e.first, "last": e.last,
                "created": e.created, "ts": e.mtime, "size": e.size, "prompts": e.prompts,
                "headless": e.headless, "branch": e.branch,
                "cwdOk": e.cwd.as_deref().is_some_and(|c| Path::new(c).is_dir()),
                "pane": l.map(|x| x.0.clone()), "status": l.map(|x| x.1.clone()),
            })
        })
        .collect();
    Json(json!({"sessions": out, "total": total, "indexed": indexed, "scanMs": scan_ms})).into_response()
}

// ---------------------------------------------------------------------------
// Starting agents (shared with sched.rs)

/// Workspace for a new tab: the one whose cwd equals `cwd`, else the deepest
/// one containing it, else the first.
pub fn pick_ws(app: &App, cwd: &str) -> Option<String> {
    let s = app.inner.lock().unwrap().state.clone();
    let st: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
    let list = st["workspaces"].as_array().cloned().unwrap_or_default();
    list.iter()
        .find(|w| w["cwd"].as_str() == Some(cwd))
        .or_else(|| {
            list.iter()
                .filter(|w| w["cwd"].as_str().is_some_and(|c| cwd.starts_with(c)))
                .max_by_key(|w| w["cwd"].as_str().map(str::len).unwrap_or(0))
        })
        .or(list.first())
        .and_then(|w| w["ws"].as_str().map(str::to_string))
}

/// Creates a tab (in `ws`, or a new workspace) and returns (pane, tab, ws).
pub async fn new_tab(ws: Option<&str>, cwd: &str, label: &str) -> Result<(String, String, String), Response> {
    let created = match ws {
        Some(w) => herdr::quick("tab.create", json!({"workspace_id": w, "cwd": cwd, "label": label, "focus": false})).await,
        None => herdr::quick("workspace.create", json!({"cwd": cwd, "label": label, "focus": false})).await,
    }
    .map_err(herdr_err)?;
    let pane = created["root_pane"]["pane_id"].as_str().unwrap_or("").to_string();
    let tab = created["tab"]["tab_id"].as_str().or(created["root_pane"]["tab_id"].as_str()).unwrap_or("").to_string();
    let wsid = created["root_pane"]["workspace_id"].as_str().unwrap_or(ws.unwrap_or("")).to_string();
    if pane.is_empty() {
        return Err(json_err(StatusCode::BAD_GATEWAY, "herdr did not return a pane"));
    }
    Ok((pane, tab, wsid))
}

/// Starts claude in `pane` (trust prompt auto-accepted) and, if given, sends
/// `prompt` once the agent is ready. Returns when the prompt was sent (or the
/// start failed / timed out).
pub async fn start_and_prompt(app: &Shared, pane: &str, label: &str, args: Vec<String>, prompt: Option<String>) -> bool {
    if let Err(e) = crate::start_claude(pane, &crate::agent_slug(label), args).await {
        eprintln!("agent.start {pane}: {e}");
    }
    app.refresh.notify_one();
    let Some(prompt) = prompt else { return true };
    let deadline = now_ms() + 15 * 60_000;
    while now_ms() < deadline {
        if let Ok(a) = herdr::quick("agent.get", json!({"target": pane})).await {
            let st = a["agent"]["agent_status"].as_str().unwrap_or("");
            if (st == "idle" || st == "done") && herdr::quick("agent.prompt", json!({"target": pane, "text": prompt})).await.is_ok() {
                app.refresh.notify_one();
                return true;
            }
        } else if herdr::quick("pane.get", json!({"pane_id": pane})).await.is_err() {
            return false; // pane closed
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    false
}

pub fn tab_label(prefix: &str, title: &str) -> String {
    let t: String = title.chars().filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-').collect();
    let t = t.split_whitespace().take(3).collect::<Vec<_>>().join("-").to_lowercase();
    let t: String = t.chars().take(18).collect();
    if t.is_empty() { prefix.to_string() } else { format!("{prefix}-{t}") }
}

/// sid → (pane, when): resumes started recently, so a double tap does not
/// open two tabs before the first one reports its session.
static RESUMING: LazyLock<Mutex<HashMap<String, (String, i64)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Deserialize, Default)]
struct ResumeBody {
    ws: Option<String>,
    /// sent once the agent is up (Continue sends "continue"); to the live pane at once when it is already running
    text: Option<String>,
}

/// Resume an asleep session with `text` as its first message (decide.rs: the user's pick wakes the agent that asked).
pub async fn resume_with(app: Shared, sid: String, text: String) -> Response {
    api_resume(State(app), AxPath(sid), Some(Json(ResumeBody { ws: None, text: Some(text) }))).await
}

async fn api_resume(State(app): State<Shared>, AxPath(sid): AxPath<String>, body: Option<Json<ResumeBody>>) -> Response {
    if !crate::valid_sid(&sid) {
        return json_err(StatusCode::BAD_REQUEST, "bad session id");
    }
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let text = body.text.as_deref().map(str::trim).filter(|t| !t.is_empty() && t.len() <= 20_000).map(str::to_string);
    if let Some(pane) = crate::pane_for_session(&app, &sid) {
        if let Some(t) = &text {
            if let Err(e) = herdr::quick("agent.prompt", json!({"target": pane, "text": t})).await {
                return herdr_err(e);
            }
        }
        return Json(json!({"pane": pane, "live": true})).into_response();
    }
    {
        let mut r = RESUMING.lock().unwrap();
        r.retain(|_, (_, t)| now_ms() - *t < 90_000);
        if let Some((pane, _)) = r.get(&sid) {
            return Json(json!({"pane": pane, "live": true, "starting": true})).into_response();
        }
    }
    let Some(path) = crate::find_session(&app, &sid) else { return json_err(StatusCode::NOT_FOUND, "no such session") };
    fresh_index().await;
    let e = entry_for(&sid).unwrap_or_default();
    let cwd = match e.cwd.clone().or_else(|| cwd_from_head(&path)) {
        Some(c) if Path::new(&c).is_dir() => c,
        Some(c) => return json_err(StatusCode::CONFLICT, format!("folder no longer exists: {c}")),
        None => return json_err(StatusCode::CONFLICT, "session has no working directory"),
    };
    // back where it lived (find.rs remembers each session's workspace and tab name, as home.json does for the home agent),
    // else the workspace that matches its folder
    let (ws, label) = resume_place(body.ws, crate::find::place(&sid).as_ref(), &state_ws(&app), pick_ws(&app, &cwd), &e.title().unwrap_or_default());
    let (pane, tab, wsid) = match new_tab(ws.as_deref().filter(|w| *w != "new"), &cwd, &label).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    RESUMING.lock().unwrap().insert(sid.clone(), (pane.clone(), now_ms()));
    crate::find::resumed(&sid, &pane);
    let args = vec!["--resume".to_string(), sid.clone(), "--permission-mode".into(), "auto".into()];
    let (app2, pane2, sid2) = (app.clone(), pane.clone(), sid.clone());
    tokio::spawn(async move {
        start_and_prompt(&app2, &pane2, &label, args, text).await;
        crate::find::after_resume(&app2, &sid2, &pane2);
    });
    app.refresh.notify_one();
    Json(json!({"pane": pane, "tab": tab, "ws": wsid, "live": false})).into_response()
}

/// Where a resumed session opens: the workspace asked for, else the one it lived in (if it still exists), else the
/// one matching its folder (`fallback`); in a tab named as before, else `re-<title>`.
fn resume_place(asked: Option<String>, place: Option<&crate::find::Info>, live_ws: &[String], fallback: Option<String>, title: &str) -> (Option<String>, String) {
    let ws = asked.filter(|w| !w.is_empty())
        .or_else(|| place.and_then(|p| p.ws.clone()).filter(|w| live_ws.contains(w)))
        .or(fallback);
    let label = place.and_then(|p| p.label.clone()).filter(|l| !l.trim().is_empty()).unwrap_or_else(|| tab_label("re", title));
    (ws, label)
}

/// Workspace ids in the current state.
fn state_ws(app: &App) -> Vec<String> {
    let s = app.inner.lock().unwrap().state.clone();
    let st: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
    st["workspaces"].as_array().into_iter().flatten().filter_map(|w| w["ws"].as_str().map(str::to_string)).collect()
}

fn cwd_from_head(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let r = BufReader::new(f.take(1 << 20));
    for line in r.lines().map_while(Result::ok) {
        if line.contains("\"cwd\":\"") {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if let Some(c) = v["cwd"].as_str() {
                    return Some(c.to_string());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Parent/child tree

/// What one transcript says about other panes: panes it created/started
/// (`spawn`) and panes it drove (`drive`: prompt / send-keys / run / wait).
#[derive(Default)]
struct Scan {
    n: usize,
    spawned: Vec<String>,
    driven: Vec<String>,
    /// tool_use id of a creating command whose result names the new pane
    pending: HashSet<String>,
}

#[derive(Default)]
struct TreeCache {
    scans: HashMap<String, Scan>,
    /// sid → (first user uuid, created) for fork detection
    heads: HashMap<String, (Option<String>, i64)>,
    head_tried: HashMap<String, i64>,
}

static TREE: LazyLock<Mutex<TreeCache>> = LazyLock::new(|| Mutex::new(TreeCache::default()));

static START_RE: LazyLock<regex_lite::Regex> =
    LazyLock::new(|| regex_lite::Regex::new(r"herdr\s+agent\s+start\b[^\n;|&]*?--pane[ =]+([A-Za-z0-9:_-]+)").unwrap());
static CREATE_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r"herdr\s+(pane\s+split|tab\s+create|workspace\s+create|worktree\s+(create|open))\b|/api/new\b|/api/a/[^/\s]+/(fork|review)\b").unwrap()
});
static DRIVE_RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r"herdr\s+(agent\s+(prompt|send-keys|wait|focus)|pane\s+(run|send-text|send-keys))\s+([A-Za-z0-9:_-]+)").unwrap()
});
static RESULT_PANE_RE: LazyLock<regex_lite::Regex> =
    LazyLock::new(|| regex_lite::Regex::new(r#""(pane_id|pane)"\s*:\s*"(w[0-9]+:p[0-9A-Za-z]+)""#).unwrap());

/// Pulls pane references out of one Bash command. Returns (spawned, driven,
/// creates) where `creates` means the new pane id is in the command's result.
fn command_refs(cmd: &str) -> (Vec<String>, Vec<String>, bool) {
    let mut spawned = Vec::new();
    let mut driven = Vec::new();
    for c in START_RE.captures_iter(cmd) {
        spawned.push(c[1].to_string());
    }
    for c in DRIVE_RE.captures_iter(cmd) {
        driven.push(c[4].to_string());
    }
    (spawned, driven, CREATE_RE.is_match(cmd))
}

fn result_panes(text: &str) -> Vec<String> {
    RESULT_PANE_RE.captures_iter(text).map(|c| c[2].to_string()).collect()
}

fn scan_items(s: &mut Scan, items: &[crate::transcript::Item]) {
    if items.len() < s.n {
        *s = Scan::default();
    }
    for it in &items[s.n..] {
        match it.k {
            "tool" if it.name.as_deref() == Some("Bash") => {
                let cmd = it.input.as_ref().and_then(|i| i["command"].as_str()).unwrap_or("");
                if !cmd.contains("herdr") && !cmd.contains("/api/") {
                    continue;
                }
                let (sp, dr, creates) = command_refs(cmd);
                s.spawned.extend(sp);
                s.driven.extend(dr);
                if creates {
                    if let Some(id) = &it.id {
                        s.pending.insert(id.clone());
                    }
                }
            }
            "result" => {
                if let Some(id) = &it.id {
                    if s.pending.remove(id) {
                        if let Some(p) = result_panes(it.text.as_deref().unwrap_or("")).into_iter().next() {
                            s.spawned.push(p);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    s.n = items.len();
    // keep it small: only the newest references matter
    for v in [&mut s.spawned, &mut s.driven] {
        if v.len() > 400 {
            v.drain(..v.len() - 400);
        }
    }
}

/// (first user uuid, created) from the head of a transcript.
fn head_of(path: &Path) -> Option<(Option<String>, i64)> {
    let f = std::fs::File::open(path).ok()?;
    let mut e = Entry::default();
    let mut r = BufReader::new(f.take(512 * 1024));
    let mut buf = Vec::new();
    while e.fu.is_none() {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => fold_line(&mut e, &buf),
        }
    }
    e.fu.as_ref()?;
    Some((e.fu, e.created))
}

/// Pane-number order (`w6:p1` < `w6:pB` < `w6:p16`): herdr numbers panes in a
/// positional base, so length then digits.
fn pane_ord(p: &str) -> (usize, String) {
    let n = p.rsplit_once(":p").map(|x| x.1).unwrap_or(p);
    (n.len(), n.to_string())
}

/// Infers each live agent's parent. Priority: branch = spawn > review > fork > drive > split (same tab).
fn parents(app: &App, agents: &[Value]) -> HashMap<String, (String, &'static str)> {
    let panes: HashSet<&str> = agents.iter().filter_map(|a| a["pane"].as_str()).collect();
    let names: HashMap<&str, &str> =
        agents.iter().filter_map(|a| Some((a["name"].as_str()?, a["pane"].as_str()?))).collect();
    let resolve = |r: &str| -> Option<String> {
        if panes.contains(r) {
            Some(r.to_string())
        } else {
            names.get(r).map(|p| p.to_string())
        }
    };
    let mut cand: HashMap<String, Vec<(u8, String, &'static str)>> = HashMap::new();
    let mut add = |child: String, parent: String, rank: u8, via: &'static str| {
        if child != parent {
            cand.entry(child).or_default().push((rank, parent, via));
        }
    };
    let mut tc = TREE.lock().unwrap();
    let now = now_ms();
    let mut heads: Vec<(String, Option<String>, i64)> = Vec::new();
    for a in agents {
        let Some(pane) = a["pane"].as_str() else { continue };
        // transcript references
        if let Some(tid) = a["tid"].as_str() {
            let tx = app.transcripts.lock().unwrap().get(tid).cloned();
            if let Some(tx) = tx {
                if let Ok(t) = tx.try_lock() {
                    let s = tc.scans.entry(tid.to_string()).or_default();
                    scan_items(s, &t.items);
                }
            }
            if let Some(s) = tc.scans.get(tid) {
                for r in &s.spawned {
                    if let Some(c) = resolve(r) {
                        add(c, pane.to_string(), 0, "spawn");
                    }
                }
                for r in &s.driven {
                    if let Some(c) = resolve(r) {
                        add(c, pane.to_string(), 3, "drive");
                    }
                }
            }
        }
        // reviewer → reviewed agent (v3)
        if let Some(of) = a["reviewOf"].as_str().filter(|p| panes.contains(p)) {
            add(pane.to_string(), of.to_string(), 1, "review");
        }
        // fork heads
        if let Some(sid) = a["session"].as_str() {
            if !tc.heads.contains_key(sid) && now - tc.head_tried.get(sid).copied().unwrap_or(0) > 30_000 {
                tc.head_tried.insert(sid.to_string(), now);
                if let Some(h) = crate::find_session(app, sid).as_deref().and_then(head_of) {
                    tc.heads.insert(sid.to_string(), h);
                }
            }
            if let Some((fu, created)) = tc.heads.get(sid) {
                heads.push((pane.to_string(), fu.clone(), *created));
            }
        }
    }
    let live_tids: HashSet<&str> = agents.iter().filter_map(|a| a["tid"].as_str()).collect();
    tc.scans.retain(|t, _| live_tids.contains(t.as_str()));
    let live_sids: HashSet<&str> = agents.iter().filter_map(|a| a["session"].as_str()).collect();
    tc.heads.retain(|s, _| live_sids.contains(s.as_str()));
    tc.head_tried.retain(|s, _| live_sids.contains(s.as_str()));
    drop(tc);
    // branches (branch.rs): recorded at the branch, so they win over the same-first-row fork guess
    let by_sid: HashMap<&str, &str> = agents.iter().filter_map(|a| Some((a["session"].as_str()?, a["pane"].as_str()?))).collect();
    for a in agents {
        let (Some(pane), Some(sid)) = (a["pane"].as_str(), a["session"].as_str()) else { continue };
        if let Some(pp) = crate::find::branch_of(sid).and_then(|p| by_sid.get(p.as_str()).copied()) {
            add(pane.to_string(), pp.to_string(), 0, "branch");
        }
    }
    // forks: same first user row; the older session is the parent
    for (p, fu, created) in &heads {
        let Some(fu) = fu else { continue };
        if let Some((q, _, _)) = heads
            .iter()
            .filter(|(q, f, c)| q != p && f.as_ref() == Some(fu) && (*c, pane_ord(q)) < (*created, pane_ord(p)))
            .min_by_key(|(_, _, c)| *c)
        {
            add(p.clone(), q.clone(), 2, "fork");
        }
    }
    // split: other panes in a tab hang under the tab's first agent pane
    let mut by_tab: HashMap<&str, Vec<&str>> = HashMap::new();
    for a in agents {
        if let (Some(t), Some(p)) = (a["tab"].as_str(), a["pane"].as_str()) {
            by_tab.entry(t).or_default().push(p);
        }
    }
    for list in by_tab.values_mut() {
        list.sort_by_key(|p| pane_ord(p));
        for p in &list[1..] {
            add(p.to_string(), list[0].to_string(), 4, "split");
        }
    }
    // choose, refusing cycles
    let mut out: HashMap<String, (String, &'static str)> = HashMap::new();
    let mut kids: Vec<(String, Vec<(u8, String, &'static str)>)> = cand.into_iter().collect();
    kids.sort_by(|a, b| a.0.cmp(&b.0));
    for (child, mut cs) in kids {
        cs.sort_by_key(|c| c.0);
        for (_, parent, via) in cs {
            let mut cur = parent.clone();
            let mut cyc = false;
            for _ in 0..64 {
                if cur == child {
                    cyc = true;
                    break;
                }
                match out.get(&cur) {
                    Some((p, _)) => cur = p.clone(),
                    None => break,
                }
            }
            if !cyc {
                out.insert(child.clone(), (parent, via));
                break;
            }
        }
    }
    out
}

/// Sets `parent: {pane, via} | null` on every agent (called before each state publish).
pub fn decorate(app: &App, agents: &mut [Value]) {
    let ps = parents(app, agents);
    for a in agents.iter_mut() {
        let p = a["pane"].as_str().and_then(|p| ps.get(p));
        a["parent"] = match p {
            Some((pane, via)) => json!({"pane": pane, "via": via}),
            None => Value::Null,
        };
    }
}

async fn api_tree(State(app): State<Shared>) -> Response {
    let agents = state_agents(&app);
    let list: Vec<Value> = agents.iter().map(|a| json!({"pane": a["pane"], "parent": a["parent"]})).collect();
    Json(json!({"tree": list})).into_response()
}

// ---------------------------------------------------------------------------
// Why is it blocked? (herdr agent.explain)

fn compact_rule(r: &Value) -> Value {
    let ev = &r["evidence"];
    json!({
        "id": r["id"], "state": r["state"], "matched": r["matched"], "region": r["region"], "priority": r["priority"],
        "preview": ev["region_preview"].as_str().map(|s| cut(s, 1500).0),
        "patterns": ev["line_regex"].as_array().into_iter().flatten()
            .chain(ev["regex"].as_array().into_iter().flatten())
            .chain(ev["contains"].as_array().into_iter().flatten())
            .take(4).cloned().collect::<Vec<_>>(),
    })
}

/// Why sheprd shows a status that herdr does not (transcript/mod overrides).
fn sheprd_reason(a: &Value, herdr_status: &str) -> Option<String> {
    let st = a["status"].as_str().unwrap_or("");
    if st != "blocked" || herdr_status == "blocked" {
        return None;
    }
    if a["ask"]["phone"] == true {
        return Some("The agent called ask_phone and is waiting for your reply (sheprd mod), so sheprd shows it as blocked.".into());
    }
    if !a["ask"].is_null() {
        return Some("An AskUserQuestion menu is open (reported by the sheprd mod); herdr does not detect that menu as blocked.".into());
    }
    if !a["plan"].is_null() {
        return Some("A plan approval (ExitPlanMode) is open.".into());
    }
    Some("The newest unresolved tool call in the transcript is AskUserQuestion; herdr does not detect that menu as blocked.".into())
}

async fn api_explain(State(app): State<Shared>, AxPath(pane): AxPath<String>) -> Response {
    let ex = match herdr::quick("agent.explain", json!({"target": pane})).await {
        Ok(v) => v,
        Err(e) => return herdr_err(e),
    };
    let ex = if ex.get("explain").is_some() { ex["explain"].clone() } else { ex };
    let rules = ex["evaluated_rules"].as_array().cloned().unwrap_or_default();
    let matched_id = ex["matched_rule"]["id"].clone();
    let matched = rules.iter().find(|r| r["id"] == matched_id).map(compact_rule);
    // Other rules that also matched, and the blocked-rules that did not (why not).
    let also: Vec<Value> = rules.iter().filter(|r| r["matched"] == true && r["id"] != matched_id).map(compact_rule).collect();
    let blockers: Vec<Value> =
        rules.iter().filter(|r| r["state"] == "blocked" && r["matched"] != true).take(6).map(compact_rule).collect();
    let screen = herdr::quick("pane.read", json!({"pane_id": pane, "source": "detection", "format": "text"}))
        .await
        .ok()
        .and_then(|r| r["read"]["text"].as_str().map(str::to_string))
        .unwrap_or_default();
    let mut lines: Vec<&str> = screen.lines().collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let excerpt = lines[lines.len().saturating_sub(24)..].join("\n");
    let herdr_state = ex["state"].as_str().unwrap_or("").to_string();
    let agent = state_agents(&app).into_iter().find(|a| a["pane"] == pane.as_str());
    let sheprd_status = agent.as_ref().and_then(|a| a["status"].as_str().map(str::to_string));
    let reason = agent.as_ref().and_then(|a| sheprd_reason(a, &herdr_state));
    Json(json!({
        "pane": pane, "agent": ex["agent"], "state": herdr_state, "status": sheprd_status, "sheprdReason": reason,
        "rule": matched, "also": also, "notMatched": blockers,
        "visible": {"blocker": ex["visible_blocker"], "idle": ex["visible_idle"], "working": ex["visible_working"]},
        "fallback": ex["fallback_reason"], "warning": ex["warning"], "skipped": ex["skipped_update_reason"],
        "manifest": {"source": ex["manifest_source"], "version": ex["manifest_version"]},
        "screen": excerpt,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_goes_back_where_it_lived() {
        let live = vec!["w6".to_string(), "wG".to_string()];
        let p = crate::find::Info { ws: Some("wG".into()), label: Some("nightly-backup".into()), ..Default::default() };
        assert_eq!(resume_place(None, Some(&p), &live, Some("w6".into()), "Nightly Backup"), (Some("wG".into()), "nightly-backup".into()));
        // its workspace is gone: the folder's one, same tab name
        let gone = crate::find::Info { ws: Some("wX".into()), ..p.clone() };
        assert_eq!(resume_place(None, Some(&gone), &live, Some("w6".into()), "Nightly Backup"), (Some("w6".into()), "nightly-backup".into()));
        // asked for a workspace: that one; never seen live: re-<title>
        assert_eq!(resume_place(Some("new".into()), Some(&p), &live, None, "x").0.as_deref(), Some("new"));
        assert_eq!(resume_place(None, None, &live, Some("w6".into()), "Backup Job build"), (Some("w6".into()), "re-backup-job-build".into()));
    }

    #[test]
    fn index_keeps_last_reply_color_and_user_prompt_time() {
        let mut e = Entry::default();
        fn l(e: &mut Entry, s: &str) {
            fold_line(e, s.as_bytes())
        }
        l(&mut e, r#"{"type":"user","message":{"role":"user","content":"run the backup"},"timestamp":"2026-10-10T06:00:00.000Z","cwd":"/home/user/Work/backup","uuid":"u1"}"#);
        l(&mut e, r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Backup 12 is done."}]},"timestamp":"2026-10-10T06:04:00.000Z"}"#);
        l(&mut e, r#"{"type":"user","message":{"role":"user","content":"[Message from agent "Master" (pane w6:pE) via sheprd]
ping"},"timestamp":"2026-10-10T07:00:00.000Z"}"#);
        l(&mut e, r#"{"type":"agent-color","agentColor":"orange","sessionId":"x"}"#);
        l(&mut e, r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"sub"}]}}"#);
        assert_eq!(e.lu, crate::transcript::parse_ts("2026-10-10T06:00:00.000Z")); // the agent's message is not the user's
        assert_eq!(e.la.as_deref(), Some("Backup 12 is done."));
        assert_eq!(e.color.as_deref(), Some("orange"));
        l(&mut e, r#"{"type":"continued-in","continuedInSessionId":"next"}"#);
        assert_eq!(e.cont.as_deref(), Some("next"));
        l(&mut e, r#"{"type":"assistant","message":{"content":[{"type":"text","text":"back here"}]}}"#);
        assert_eq!(e.cont, None); // the conversation went on in this file after all
    }

    #[test]
    fn refs() {
        let (s, d, c) = command_refs("herdr agent start fl-a --kind claude --pane w6:p1A -- --permission-mode auto");
        assert_eq!(s, ["w6:p1A"]);
        assert!(d.is_empty() && !c);
        let (s, d, c) = command_refs("herdr pane split --current --direction right --no-focus | jq .");
        assert!(s.is_empty() && d.is_empty() && c);
        let (_, d, _) = command_refs("herdr agent prompt reviewer \"go\" --wait; herdr pane run w6:p9 'ls'");
        assert_eq!(d, ["reviewer", "w6:p9"]);
        let (_, _, c) = command_refs("curl -s -XPOST localhost:7878/api/new -d '{}'");
        assert!(c);
        assert_eq!(result_panes(r#"{"result":{"root_pane":{"pane_id":"w6:pX","tab_id":"w6:t3"}}}"#), ["w6:pX"]);
        assert_eq!(result_panes(r#"{"pane": "w7:p2", "ws": "w7"}"#), ["w7:p2"]);
    }

    #[test]
    fn order() {
        assert!(pane_ord("w6:p1") < pane_ord("w6:pB"));
        assert!(pane_ord("w6:pV") < pane_ord("w6:p16"));
    }

    #[test]
    fn fold() {
        let mut e = Entry::default();
        for l in [
            r#"{"type":"mode","mode":"normal"}"#,
            r#"{"type":"user","uuid":"u1","cwd":"/tmp","entrypoint":"cli","gitBranch":"main","timestamp":"2026-10-05T09:00:00.000Z","message":{"content":"first   prompt"}}"#,
            r#"{"type":"user","uuid":"u2","message":{"content":[{"type":"tool_result","tool_use_id":"x","content":"zzz"}]}}"#,
            r#"{"type":"ai-title","aiTitle":"Ai title"}"#,
            r#"{"type":"user","uuid":"u3","message":{"content":"second"}}"#,
            r#"{"type":"custom-title","customTitle":"Mine"}"#,
        ] {
            fold_line(&mut e, l.as_bytes());
        }
        assert_eq!(e.first.as_deref(), Some("first prompt"));
        assert_eq!(e.last.as_deref(), Some("second"));
        assert_eq!(e.fu.as_deref(), Some("u1"));
        assert_eq!(e.cwd.as_deref(), Some("/tmp"));
        assert_eq!(e.branch.as_deref(), Some("main"));
        assert_eq!(e.title().as_deref(), Some("Mine"));
        assert!(e.created > 0 && !e.headless);
    }

    #[test]
    fn incremental() {
        let dir = std::env::temp_dir().join(format!("mm-fleet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let a = r#"{"type":"user","uuid":"u1","cwd":"/tmp","message":{"content":"one"}}"#;
        std::fs::write(&p, format!("{a}\n{{\"type\":\"user\",\"mess")).unwrap();
        let mut e = Entry::default();
        let size = std::fs::metadata(&p).unwrap().len();
        update_entry(&mut e, &p, size);
        assert_eq!(e.off as usize, a.len() + 1); // partial line left for later
        assert_eq!(e.prompts, 1);
        std::fs::write(&p, format!("{a}\n{{\"type\":\"user\",\"message\":{{\"content\":\"two\"}}}}\n")).unwrap();
        let size = std::fs::metadata(&p).unwrap().len();
        update_entry(&mut e, &p, size);
        assert_eq!(e.off, size);
        assert_eq!((e.prompts, e.last.as_deref()), (2, Some("two")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
