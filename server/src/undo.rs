//! Undo last turn (API.md "Undo last turn"): `GET /api/a/{pane}/undo` (what would be undone) and
//! `POST /api/a/{pane}/undo {uuid, files}` (do it).
//!
//! The conversation goes back to before the last prompt and that prompt comes back to the phone's composer; files
//! the turn edited go back where Claude's checkpoints allow it. How, for a live agent in its pane:
//! 1. only while it is idle with an empty input box (inputbox.rs) and nothing held for it: `/exit` is typed in, which
//!    ends Claude cleanly (no signals) and leaves the pane's shell, so the pane id stays;
//! 2. `claude -p --resume <sid> --rewind-files <prompt uuid>` (a hidden, standalone flag of this Claude Code build;
//!    print mode needs `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING=1`) puts every file Claude tracked back to its
//!    checkpoint at that prompt: edits restored, files the turn created deleted. Shell commands, commits and anything
//!    else outside Claude's Edit/Write checkpoints is not undone;
//! 3. the transcript is cut, in place, at the prompt's line (a copy of the whole file is kept first under
//!    `<state>/undo/`); the session's latest name / colour / mode rows from after the cut are kept;
//! 4. `claude --resume <sid>` starts again in the same pane under the same herdr name (`master` stays `master`), its
//!    model and permission mode as before. Prompts sent meanwhile wait in the boot queue (newagent.rs).
//! If stopping fails nothing changed; if the rewind fails Claude is started again on the untouched transcript.

use crate::transcript::{Transcript, now_ms};
use crate::{Shared, herdr, json_err};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

pub fn routes() -> Router<Shared> {
    Router::new().route("/api/a/{pane}/undo", get(api_preview).post(api_undo))
}

/// Panes being undone right now.
static BUSY: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// Rows after the cut that describe the session, not the turn: the latest of each is kept.
const KEEP: [&str; 7] = ["custom-title", "agent-name", "agent-color", "ai-title", "mode", "permission-mode", "tag"];

/// Tools that only look: never listed as "already happened".
const QUIET: [&str; 13] = [
    "Read", "Grep", "Glob", "LS", "TodoWrite", "TaskCreate", "TaskUpdate", "TaskList", "TaskGet", "ToolSearch", "Edit", "Write", "MultiEdit",
];

/// The last turn, from the raw transcript.
#[derive(Debug, Default)]
pub struct Turn {
    pub uuid: String,
    /// byte offset of the prompt's line: the cut
    pub off: usize,
    pub ts: i64,
    /// last row of the turn (for "changed after the turn")
    pub end_ts: i64,
    /// the prompt as written (fallback when the parsed item is missing)
    pub raw: String,
    /// messages typed into the running turn (queued commands): they go back to the composer too
    pub also: Vec<String>,
    /// (tool name, input) of every call in the turn
    pub tools: Vec<(String, Value)>,
    /// files the turn's Edit / Write / MultiEdit / NotebookEdit calls touched (absolute)
    pub edited: Vec<String>,
    /// Claude's checkpoint at the prompt: absolute path → backup file name (None: the file did not exist)
    pub tracked: Option<BTreeMap<String, Option<String>>>,
    /// rows kept from after the cut (latest per KEEP type)
    pub keep: Vec<String>,
    pub model: Option<String>,
    pub perm: Option<String>,
    /// the session went on in another transcript (follow.rs): refuse
    pub continued: bool,
}

fn ts_of(v: &Value) -> i64 {
    v["timestamp"].as_str().and_then(|t| chrono_ms(t)).unwrap_or(0)
}

/// "2026-10-10T19:38:49.216Z" → ms (UTC), no crate.
fn chrono_ms(t: &str) -> Option<i64> {
    let b = t.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |a: usize, z: usize| t.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, s) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let ms = if b.get(19) == Some(&b'.') { t.get(20..23).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0) } else { 0 };
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + s * 1000 + ms)
}

/// A user row the user (or another agent) typed: not a tool result, a command, a meta row or a summary.
fn is_prompt(v: &Value) -> bool {
    if v["type"] != "user" || v["isSidechain"] == true || v["isMeta"] == true || v["isCompactSummary"] == true || v["uuid"].as_str().is_none() {
        return false;
    }
    let c = &v["message"]["content"];
    if let Some(s) = c.as_str() {
        let t = s.trim_start();
        return !t.is_empty()
            && !["<command-name>", "<command-message>", "<local-command", "<bash-", "<task-notification>"].iter().any(|p| t.starts_with(p));
    }
    let Some(blocks) = c.as_array() else { return false };
    !blocks.iter().any(|b| b["type"] == "tool_result") && blocks.iter().any(|b| b["type"] == "text" || b["type"] == "image")
}

fn prompt_text(v: &Value) -> String {
    let c = &v["message"]["content"];
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    c.as_array()
        .map(|bs| bs.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

/// Where a checkpoint entry lives: `realParentDir` + its file name, else relative to the session's folder.
fn abs_path(key: &str, e: &Value, cwd: Option<&str>) -> String {
    if Path::new(key).is_absolute() {
        return key.to_string();
    }
    if let Some(dir) = e["realParentDir"].as_str() {
        if let Some(name) = Path::new(key).file_name() {
            return Path::new(dir).join(name).to_string_lossy().to_string();
        }
    }
    match cwd {
        Some(c) => Path::new(c).join(key).to_string_lossy().to_string(),
        None => key.to_string(),
    }
}

/// Finds the last turn in a transcript's bytes.
pub fn analyze(bytes: &[u8]) -> Result<Turn, String> {
    let text = String::from_utf8_lossy(bytes);
    // (offset, line)
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut at = 0usize;
    for l in text.split_inclusive('\n') {
        if !l.trim().is_empty() {
            lines.push((at, l.trim_end_matches('\n')));
        }
        at += l.len();
    }
    let mut cut = None;
    for (k, (_, l)) in lines.iter().enumerate().rev() {
        if !l.contains("\"user\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(l) {
            if is_prompt(&v) {
                cut = Some((k, v));
                break;
            }
        }
    }
    let Some((k, pv)) = cut else { return Err("there is no message of yours to undo".into()) };
    let mut t = Turn {
        uuid: pv["uuid"].as_str().unwrap_or("").to_string(),
        off: lines[k].0,
        ts: ts_of(&pv),
        raw: prompt_text(&pv),
        ..Default::default()
    };
    t.end_ts = t.ts;
    let cwd = pv["cwd"].as_str().map(str::to_string);
    let mut keep: BTreeMap<&str, (usize, String)> = BTreeMap::new();
    let mut tracked: Option<BTreeMap<String, Option<String>>> = None;
    let mut seen_edit: HashSet<String> = HashSet::new();
    // the checkpoint and the session's state before the cut
    for (_, l) in &lines[..k] {
        if l.contains("\"permission-mode\"") {
            if let Ok(v) = serde_json::from_str::<Value>(l) {
                if let Some(m) = v["permissionMode"].as_str() {
                    t.perm = Some(m.to_string());
                }
            }
        }
        if l.contains("\"assistant\"") && l.contains("\"model\"") {
            if let Ok(v) = serde_json::from_str::<Value>(l) {
                if v["type"] == "assistant" {
                    if let Some(m) = v["message"]["model"].as_str().filter(|m| m.starts_with("claude-") && !m.contains("synthetic")) {
                        t.model = Some(m.to_string());
                    }
                }
            }
        }
    }
    let snap_of = |l: &str, tracked: &mut Option<BTreeMap<String, Option<String>>>, uuid: &str| {
        if !l.contains("file-history-") || !l.contains(uuid) {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(l) else { return };
        match v["type"].as_str() {
            Some("file-history-snapshot") if v["messageId"] == uuid => {
                let m = tracked.get_or_insert_with(BTreeMap::new);
                if let Some(fb) = v["snapshot"]["trackedFileBackups"].as_object() {
                    for (key, e) in fb {
                        m.insert(abs_path(key, e, cwd.as_deref()), e["backupFileName"].as_str().map(str::to_string));
                    }
                }
            }
            Some("file-history-delta") if v["snapshotMessageId"] == uuid => {
                // a file first tracked during the turn: its backup joins the checkpoint (null = it did not exist)
                if let Some(key) = v["trackingPath"].as_str() {
                    let m = tracked.get_or_insert_with(BTreeMap::new);
                    m.entry(abs_path(key, &v["backup"], cwd.as_deref())).or_insert(v["backup"]["backupFileName"].as_str().map(str::to_string));
                }
            }
            _ => {}
        }
    };
    // a snapshot may be written just before the prompt row
    for (_, l) in lines[k.saturating_sub(20)..k].iter() {
        snap_of(l, &mut tracked, &t.uuid);
    }
    for (_, l) in &lines[k + 1..] {
        snap_of(l, &mut tracked, &t.uuid);
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        let ty = v["type"].as_str().unwrap_or("");
        if let Some(kk) = KEEP.iter().find(|x| **x == ty) {
            keep.insert(kk, (keep.len(), l.to_string()));
            if ty == "permission-mode" {
                if let Some(m) = v["permissionMode"].as_str() {
                    t.perm = Some(m.to_string());
                }
            }
            continue;
        }
        if ty == "continued-in" {
            t.continued = true;
        }
        let ts = ts_of(&v);
        if ts > t.end_ts {
            t.end_ts = ts;
        }
        if ty == "attachment" && v["attachment"]["type"] == "queued_command" {
            let p = v["attachment"]["prompt"].as_str().unwrap_or("").trim();
            if !p.is_empty() && !p.starts_with('<') {
                t.also.push(p.to_string());
            }
        }
        if ty == "assistant" && v["isSidechain"] != true {
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                if b["type"] == "tool_use" || b["type"] == "server_tool_use" {
                    let name = b["name"].as_str().unwrap_or("").to_string();
                    if matches!(name.as_str(), "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
                        if let Some(p) = b["input"]["file_path"].as_str().or(b["input"]["notebook_path"].as_str()) {
                            let p = if Path::new(p).is_absolute() { p.to_string() } else { abs_path(p, &Value::Null, cwd.as_deref()) };
                            if seen_edit.insert(p.clone()) {
                                t.edited.push(p);
                            }
                        }
                    }
                    t.tools.push((name, b["input"].clone()));
                }
            }
        }
    }
    let mut kept: Vec<(usize, String)> = keep.into_values().collect();
    kept.sort();
    t.keep = kept.into_iter().map(|(_, l)| l).collect();
    t.tracked = tracked;
    Ok(t)
}

/// The cut transcript: everything before the prompt, then the kept session rows.
pub fn cut_jsonl(bytes: &[u8], t: &Turn) -> Vec<u8> {
    let mut out = bytes[..t.off].to_vec();
    if !out.is_empty() && *out.last().unwrap() != b'\n' {
        out.push(b'\n');
    }
    for l in &t.keep {
        out.extend_from_slice(l.as_bytes());
        out.push(b'\n');
    }
    out
}

/// One file in Claude's checkpoint, compared with the disk now.
#[derive(Debug, Clone, PartialEq)]
pub struct FileState {
    pub path: String,
    /// "restore" | "delete" | "recreate" | "same" | "missing" (backup gone: can't restore)
    pub act: &'static str,
    /// touched by this turn's own edits
    pub in_turn: bool,
    /// modified after the turn ended (the user or something else changed it since)
    pub later: bool,
}

fn file_states(sid: &str, t: &Turn) -> Vec<FileState> {
    let Some(tr) = &t.tracked else { return Vec::new() };
    let dir = crate::home().join(".claude/file-history").join(sid);
    let edited: HashSet<&String> = t.edited.iter().collect();
    let mut out = Vec::new();
    for (path, backup) in tr {
        let cur = std::fs::read(path).ok();
        let act = match backup {
            None if cur.is_none() => "same",
            None => "delete",
            Some(b) => match std::fs::read(dir.join(b)) {
                Err(_) => "missing",
                Ok(old) => match &cur {
                    None => "recreate",
                    Some(c) if *c == old => "same",
                    Some(_) => "restore",
                },
            },
        };
        let later = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| t.end_ts > 0 && d.as_millis() as i64 > t.end_ts + 2000);
        out.push(FileState { path: path.clone(), act, in_turn: edited.contains(path), later });
    }
    out
}

fn short(path: &str, cwd: &str) -> String {
    Path::new(path).strip_prefix(cwd).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| path.replace(&crate::home().to_string_lossy().to_string(), "~"))
}

/// Git's view: changed files Claude's checkpoint does not cover, and commits since the prompt.
async fn git_hint(cwd: &str, covered: &HashSet<String>, since_ms: i64) -> Value {
    let st = tokio::process::Command::new("git").args(["-C", cwd, "status", "--porcelain=v1", "-uall"]).kill_on_drop(true).output();
    let Ok(Ok(st)) = tokio::time::timeout(Duration::from_secs(5), st).await else { return Value::Null };
    if !st.status.success() {
        return Value::Null; // not a repo
    }
    let top = tokio::process::Command::new("git").args(["-C", cwd, "rev-parse", "--show-toplevel"]).output().await.ok();
    let top = top.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| cwd.to_string());
    let mut other = Vec::new();
    for l in String::from_utf8_lossy(&st.stdout).lines() {
        let p = l.get(3..).unwrap_or("").split(" -> ").last().unwrap_or("").trim_matches('"');
        let abs = Path::new(&top).join(p).to_string_lossy().to_string();
        if !p.is_empty() && !covered.contains(&abs) {
            other.push(short(&abs, cwd));
        }
    }
    let since = format!("--since=@{}", since_ms / 1000);
    let commits = tokio::process::Command::new("git").args(["-C", cwd, "log", "--oneline", &since]).output().await.ok();
    let commits: Vec<String> = commits.map(|o| String::from_utf8_lossy(&o.stdout).lines().take(5).map(str::to_string).collect()).unwrap_or_default();
    json!({"other": other.iter().take(12).collect::<Vec<_>>(), "otherN": other.len(), "commits": commits})
}

struct Ctx {
    sid: String,
    path: PathBuf,
    name: String,
    cwd: String,
    home: bool,
    status: String,
}

fn ctx(app: &Shared, pane: &str) -> Result<Ctx, Response> {
    let st: Value = serde_json::from_str(&app.inner.lock().unwrap().state).unwrap_or(Value::Null);
    let Some(a) = st["agents"].as_array().into_iter().flatten().find(|a| a["pane"] == pane).cloned() else {
        return Err(json_err(StatusCode::NOT_FOUND, "no such agent"));
    };
    if a["kind"].as_str().is_some_and(|k| k != "claude") {
        return Err(json_err(StatusCode::CONFLICT, "only a Claude agent can undo a turn"));
    }
    let Some(sid) = a["session"].as_str().filter(|s| crate::valid_sid(s)).map(str::to_string) else {
        return Err(json_err(StatusCode::CONFLICT, "it has no conversation yet"));
    };
    let Some(path) = crate::find_session(app, &sid) else { return Err(json_err(StatusCode::CONFLICT, "its transcript isn't there")) };
    Ok(Ctx {
        sid,
        path,
        name: restart_name(a["name"].as_str(), a["title"].as_str(), a["home"] == true),
        cwd: a["cwd"].as_str().unwrap_or("").to_string(),
        home: a["home"] == true,
        status: a["status"].as_str().unwrap_or("").to_string(),
    })
}

/// herdr's agent name for the restart: its own, else (an agent started by hand, `claude` typed in a pane, has none and
/// agent.start refuses an empty one) a slug of its title; never `master*` unless it is the home agent (that prefix
/// marks the home agent).
pub fn restart_name(name: Option<&str>, title: Option<&str>, home: bool) -> String {
    if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    let n = crate::agent_slug(title.map(str::trim).filter(|t| !t.is_empty()).unwrap_or("agent"));
    if !home && n.starts_with("master") { format!("a-{n}") } else { n }
}

/// Whether a (cut) transcript still holds a conversation: undoing the first prompt leaves only session rows, and
/// `claude --resume` on that says "No conversation found".
pub fn has_conversation(bytes: &[u8]) -> bool {
    bytes.split(|&b| b == b'\n').any(|l| {
        (l.windows(6).any(|w| w == b"\"user\"") || l.windows(11).any(|w| w == b"\"assistant\""))
            && serde_json::from_slice::<Value>(l).is_ok_and(|v| v["type"] == "user" || v["type"] == "assistant")
    })
}

/// Why it can't be undone right now (None = it can).
async fn blocker(pane: &str, c: &Ctx) -> Option<&'static str> {
    if BUSY.lock().unwrap().contains(pane) {
        return Some("busy");
    }
    if c.status == "working" || c.status == "blocked" {
        return Some(if c.status == "working" { "working" } else { "blocked" });
    }
    if !crate::inputbox::held_for(pane).is_empty() {
        return Some("held");
    }
    match crate::inputbox::read(pane).await {
        Some(crate::inputbox::Input::Draft(_)) => Some("draft"),
        Some(crate::inputbox::Input::Menu) => Some("menu"),
        None => Some("unreadable"),
        _ => None,
    }
}

/// The prompt as the chat shows it (reminders stripped, a steer's header gone), else as written.
fn shown_text(app: &Shared, sid: &str, t: &Turn) -> (String, usize, Option<String>) {
    if let Some(tx) = crate::transcript(app, sid) {
        let mut g = tx.lock().unwrap();
        g.refresh();
        if let Some(it) = g.items.iter().find(|x| x.off == t.off as u64 && x.k == "user") {
            return (it.text.clone().unwrap_or_default(), it.images.as_ref().map_or(0, |v| v.len()), it.from.clone().or(it.kind.map(str::to_string)));
        }
    }
    (crate::transcript::strip_reminders(&t.raw), 0, None)
}

fn summarize(t: &Turn, cwd: &str) -> (Vec<String>, Vec<String>) {
    let pre = format!("{}/", cwd.trim_end_matches('/'));
    let mut cmds = Vec::new();
    let mut other: Vec<String> = Vec::new();
    for (n, input) in &t.tools {
        if n == "Bash" {
            let c = input["command"].as_str().unwrap_or("").lines().next().unwrap_or("").to_string();
            let c = if cwd.is_empty() { c } else { c.replace(&pre, "") };
            cmds.push(c.chars().take(120).collect());
        } else if !QUIET.contains(&n.as_str()) && !other.contains(n) {
            other.push(n.clone());
        }
    }
    (cmds, other)
}

async fn api_preview(State(app): State<Shared>, AxPath(pane): AxPath<String>) -> Response {
    let c = match ctx(&app, &pane) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let path = c.path.clone();
    let t = match tokio::task::spawn_blocking(move || std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| analyze(&b))).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return json_err(StatusCode::CONFLICT, e),
        Err(_) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, "read failed"),
    };
    let (text, images, from) = shown_text(&app, &c.sid, &t);
    let files = {
        let (sid, tt) = (c.sid.clone(), Turn { tracked: t.tracked.clone(), edited: t.edited.clone(), end_ts: t.end_ts, ..Default::default() });
        tokio::task::spawn_blocking(move || file_states(&sid, &tt)).await.unwrap_or_default()
    };
    let covered: HashSet<String> = files.iter().map(|f| f.path.clone()).collect();
    let git = if Path::new(&c.cwd).is_dir() { git_hint(&c.cwd, &covered, t.ts).await } else { Value::Null };
    let (cmds, other) = summarize(&t, &c.cwd);
    let flist: Vec<Value> = files
        .iter()
        .filter(|f| f.act != "same" || f.in_turn)
        .map(|f| json!({"path": short(&f.path, &c.cwd), "act": f.act, "inTurn": f.in_turn, "later": f.later}))
        .collect();
    // edits the checkpoint doesn't cover (checkpoints off, or a file outside them)
    let unchecked: Vec<String> = t.edited.iter().filter(|p| !covered.contains(*p)).map(|p| short(p, &c.cwd)).collect();
    let why = if t.continued { Some("moved") } else { blocker(&pane, &c).await };
    Json(json!({
        "uuid": t.uuid,
        "ts": t.ts,
        "text": text,
        "also": t.also,
        "images": images,
        "from": from,
        "checkpoint": t.tracked.is_some(),
        "files": flist,
        "unchecked": unchecked,
        "commands": cmds,
        "other": other,
        "tools": t.tools.len(),
        "git": git,
        "home": c.home,
        "blocked": why,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct UndoBody {
    /// the prompt the preview showed: refused if a newer one came in since
    uuid: String,
    /// also put files back (default: yes, when there is a checkpoint)
    files: Option<bool>,
}

async fn api_undo(State(app): State<Shared>, AxPath(pane): AxPath<String>, Json(b): Json<UndoBody>) -> Response {
    let c = match ctx(&app, &pane) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Some(why) = blocker(&pane, &c).await {
        return json_err(StatusCode::CONFLICT, reason(why));
    }
    if !BUSY.lock().unwrap().insert(pane.clone()) {
        return json_err(StatusCode::CONFLICT, reason("busy"));
    }
    let r = undo(&app, &pane, &c, &b).await;
    BUSY.lock().unwrap().remove(&pane);
    app.refresh.notify_one();
    match r {
        Ok(v) => Json(v).into_response(),
        Err((code, msg)) => json_err(code, msg),
    }
}

fn reason(why: &str) -> &'static str {
    match why {
        "working" => "it's working: wait for the turn to end, or stop it first",
        "blocked" => "it's waiting on a question or permission",
        "held" => "a message to it is still waiting to be delivered",
        "draft" => "its input box on the PC holds an unsent draft",
        "menu" => "a menu is open in its terminal",
        "unreadable" => "its terminal can't be read right now",
        "moved" => "this conversation moved to another session",
        _ => "an undo is already running for it",
    }
}

type Fail = (StatusCode, String);

async fn undo(app: &Shared, pane: &str, c: &Ctx, b: &UndoBody) -> Result<Value, Fail> {
    let conflict = |m: &str| (StatusCode::CONFLICT, m.to_string());
    let bytes = std::fs::read(&c.path).map_err(|e| conflict(&format!("can't read the transcript: {e}")))?;
    let t = analyze(&bytes).map_err(|e| conflict(&e))?;
    if t.uuid != b.uuid {
        return Err(conflict("a newer message came in: look again"));
    }
    if t.continued {
        return Err(conflict(reason("moved")));
    }
    let (text, _, _) = shown_text(app, &c.sid, &t);
    let want_files = b.files.unwrap_or(true) && t.tracked.is_some();
    // a copy of the whole conversation first
    let keep_dir = crate::state_dir().join("undo");
    let _ = std::fs::create_dir_all(&keep_dir);
    let backup = keep_dir.join(format!("{}.{}.jsonl", c.sid, now_ms()));
    std::fs::write(&backup, &bytes).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("can't keep a copy: {e}")))?;
    prune(&keep_dir, 20);

    // 1. Claude stops: `/exit` into the empty box (agent.prompt is refused over a draft or a menu)
    crate::newagent::boot_begin(pane, None); // prompts sent from now on wait for the restart
    let restart = |args: Vec<String>| {
        let (app, pane, name) = (app.clone(), pane.to_string(), c.name.clone());
        async move {
            let r = crate::start_claude(&pane, &name, args).await;
            app.refresh.notify_one();
            let app2 = app.clone();
            let pane2 = pane.clone();
            tokio::spawn(async move { crate::newagent::finish_boot(&app2, &pane2).await });
            r
        }
    };
    if let Err(e) = herdr::quick("agent.prompt", json!({"target": pane, "text": "/exit"})).await {
        crate::newagent::finish_boot(app, pane).await;
        return Err(conflict(&format!("couldn't stop it ({e})")));
    }
    if !wait_gone(pane).await {
        tokio::spawn({
            let (app, pane) = (app.clone(), pane.to_string());
            async move { crate::newagent::finish_boot(&app, &pane).await }
        });
        return Err(conflict("it didn't stop: nothing was changed (look at its terminal)"));
    }
    let mut args = vec!["--resume".to_string(), c.sid.clone(), "--permission-mode".into()];
    args.push(t.perm.clone().filter(|m| ["default", "acceptEdits", "plan", "auto"].contains(&m.as_str())).unwrap_or_else(|| "auto".into()));
    if let Some(m) = &t.model {
        args.push("--model".into());
        args.push(m.clone());
    }
    if let Ok(extra) = std::env::var("SHEPRD_CLAUDE_ARGS") {
        args.extend(extra.split_whitespace().map(str::to_string)); // tests: settings for the isolated agents
    }

    // 2. files back to the checkpoint
    let mut files_note = Value::Null;
    if want_files {
        match rewind_files(&c.sid, &t.uuid, &c.cwd).await {
            Ok(warn) => files_note = json!({"ok": true, "warning": warn}),
            Err(e) => {
                let _ = restart(args).await;
                return Err(conflict(&format!("files couldn't be put back ({e}); nothing was changed and it's running again")));
            }
        }
    }

    // 3. the conversation, cut in place (the rewind appended a row or two: read again)
    let now = std::fs::read(&c.path).unwrap_or(bytes);
    let t2 = analyze(&now).ok().filter(|x| x.uuid == t.uuid);
    let Some(t2) = t2 else {
        let _ = restart(args).await;
        return Err(conflict("the transcript changed under the undo; files may be back, the conversation is as it was"));
    };
    let out = cut_jsonl(&now, &t2);
    let tmp = c.path.with_extension("jsonl.undo-tmp");
    if let Err(e) = std::fs::write(&tmp, &out).and_then(|_| std::fs::rename(&tmp, &c.path)) {
        let _ = restart(args).await;
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("couldn't write the transcript: {e}")));
    }
    if let Some(tx) = app.transcripts.lock().unwrap().get(&c.sid).cloned() {
        let mut g = tx.lock().unwrap();
        *g = Transcript::new(c.sid.clone(), c.path.clone());
    }
    eprintln!("undo {pane}: cut {} at {} ({} → {} bytes), files {}", c.sid, t.uuid, now.len(), out.len(), want_files);

    // 4. Claude again, same pane, same name. Undoing the first prompt leaves nothing to resume: a fresh session there
    // (Claude names it anew; the old file keeps only the session rows, the full copy is in <state>/undo/).
    let fresh = !has_conversation(&out);
    let mut args = args;
    if fresh {
        args.drain(0..2); // "--resume", sid
    }
    let started = restart(args).await;
    let back: Vec<String> = std::iter::once(text).chain(t.also.iter().cloned()).filter(|s| !s.trim().is_empty()).collect();
    Ok(json!({
        "ok": true,
        "text": back.join("\n\n"),
        "files": files_note,
        "restarted": started.is_ok(),
        "error": started.err().map(|e| e.to_string()),
        "backup": backup.to_string_lossy(),
        "tid": c.sid,
        "fresh": fresh,
    }))
}

/// Waits until the pane no longer runs an agent (Claude exited back to the shell).
async fn wait_gone(pane: &str) -> bool {
    let until = now_ms() + 20_000;
    while now_ms() < until {
        tokio::time::sleep(Duration::from_millis(400)).await;
        match herdr::call("agent.get", json!({"target": pane}), Duration::from_secs(3)).await {
            Err(e) if e.code == "agent_not_found" => return true,
            Ok(v) if v["agent"]["agent"].is_null() => return true,
            _ => {}
        }
    }
    false
}

/// `claude -p --resume <sid> --rewind-files <uuid>` in the session's folder, in its own cgroup like run_haiku.
async fn rewind_files(sid: &str, uuid: &str, cwd: &str) -> Result<Option<String>, String> {
    let bin = crate::cloud::claude_bin().ok_or("claude is not installed")?;
    let dir = if Path::new(cwd).is_dir() { PathBuf::from(cwd) } else { crate::home() };
    let args: Vec<String> = ["-p", "--resume", sid, "--rewind-files", uuid, "--setting-sources", "", "--strict-mcp-config"].iter().map(|s| s.to_string()).collect();
    let path = crate::cloud::child_path();
    let mut cmd = if crate::cloud::user_scope() {
        let mut c = tokio::process::Command::new("systemd-run");
        c.args(["--user", "--quiet", "--pipe", "--wait", "--collect", "-p", "MemoryMax=1000M", "--working-directory"])
            .arg(&dir)
            .arg(format!("--setenv=PATH={path}"))
            .arg("--setenv=CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING=1")
            .arg("--")
            .arg(&bin)
            .args(&args);
        c
    } else {
        let mut c = tokio::process::Command::new(&bin);
        c.args(&args).current_dir(&dir).env("PATH", &path).env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "1");
        c
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("could not start claude: {e}"))?;
    let out = tokio::time::timeout(Duration::from_secs(90), child.wait_with_output())
        .await
        .map_err(|_| "claude timed out".to_string())?
        .map_err(|e| format!("claude failed: {e}"))?;
    let so = String::from_utf8_lossy(&out.stdout);
    let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
    eprintln!("undo rewind {sid} {uuid}: {} | {}", so.trim(), se);
    if !out.status.success() || !so.contains("Files rewound") {
        let e = se.lines().find(|l| l.starts_with("Error")).unwrap_or(se.lines().next().unwrap_or("no answer")).trim_start_matches("Error: ");
        return Err(e.chars().take(200).collect());
    }
    Ok(se.lines().find(|l| l.starts_with("Warning")).map(str::to_string))
}

fn prune(dir: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = rd.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
    if v.len() <= keep {
        return;
    }
    v.sort();
    for (_, p) in &v[..v.len() - keep] {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(o: Value) -> String {
        o.to_string()
    }

    fn fixture() -> String {
        let s = "s1";
        [
            row(json!({"type":"permission-mode","permissionMode":"acceptEdits","sessionId":s})),
            row(json!({"type":"file-history-snapshot","messageId":"u1","snapshot":{"messageId":"u1","trackedFileBackups":{}}})),
            row(json!({"type":"user","uuid":"u1","parentUuid":null,"cwd":"/r","timestamp":"2026-10-10T19:38:40.000Z","message":{"role":"user","content":"first"}})),
            row(json!({"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-10-10T19:38:45.000Z","message":{"model":"claude-haiku-4-5","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/r/a.txt"}}]}})),
            row(json!({"type":"file-history-delta","messageId":"a1","snapshotMessageId":"u1","trackingPath":"a.txt","backup":{"backupFileName":"h@v1","version":1,"realParentDir":"/r"}})),
            row(json!({"type":"user","uuid":"r1","parentUuid":"a1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}})),
            row(json!({"type":"custom-title","customTitle":"Old","sessionId":s})),
            row(json!({"type":"user","uuid":"u2","parentUuid":"r1","cwd":"/r","timestamp":"2026-10-10T19:39:00.000Z","message":{"role":"user","content":[{"type":"text","text":"second please"}]}})),
            row(json!({"type":"file-history-snapshot","messageId":"u2","snapshot":{"messageId":"u2","trackedFileBackups":{"a.txt":{"backupFileName":"h@v2","version":2,"realParentDir":"/r"}}}})),
            row(json!({"type":"file-history-delta","messageId":"a2","snapshotMessageId":"u2","trackingPath":"c.txt","backup":{"backupFileName":null,"version":1,"realParentDir":"/r"}})),
            row(json!({"type":"assistant","uuid":"a2","parentUuid":"u2","timestamp":"2026-10-10T19:39:05.000Z","message":{"model":"claude-haiku-4-5","content":[{"type":"tool_use","id":"t2","name":"Write","input":{"file_path":"/r/c.txt"}},{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"echo hi > d.txt"}}]}})),
            row(json!({"type":"attachment","uuid":"q1","parentUuid":"a2","attachment":{"type":"queued_command","prompt":"and also this"}})),
            row(json!({"type":"custom-title","customTitle":"New Name","sessionId":s})),
            row(json!({"type":"last-prompt","lastPrompt":"second please","sessionId":s})),
            row(json!({"type":"user","uuid":"x1","parentUuid":"q1","message":{"role":"user","content":"<command-name>/exit</command-name>"}})),
            row(json!({"type":"permission-mode","permissionMode":"plan","sessionId":s})),
        ]
        .join("\n")
            + "\n"
    }

    #[test]
    fn finds_the_last_prompt_and_its_turn() {
        let f = fixture();
        let t = analyze(f.as_bytes()).unwrap();
        assert_eq!(t.uuid, "u2");
        assert_eq!(t.raw, "second please");
        assert!(f[t.off..].split('\n').next().unwrap().contains("\"uuid\":\"u2\""));
        assert_eq!(t.edited, ["/r/c.txt"]);
        assert_eq!(t.tools.len(), 2);
        assert_eq!(t.also, ["and also this"]);
        assert_eq!(t.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(t.perm.as_deref(), Some("plan"));
        let tr = t.tracked.as_ref().unwrap();
        assert_eq!(tr.get("/r/a.txt"), Some(&Some("h@v2".to_string())));
        assert_eq!(tr.get("/r/c.txt"), Some(&None));
        assert!(!t.continued);
        assert!(t.end_ts > t.ts);
    }

    #[test]
    fn cuts_before_the_prompt_and_keeps_session_rows() {
        let f = fixture();
        let t = analyze(f.as_bytes()).unwrap();
        let out = String::from_utf8(cut_jsonl(f.as_bytes(), &t)).unwrap();
        let rows: Vec<Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let types: Vec<&str> = rows.iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["permission-mode", "file-history-snapshot", "user", "assistant", "file-history-delta", "user", "custom-title", "custom-title", "permission-mode"]);
        assert_eq!(rows[7]["customTitle"], "New Name");
        assert!(!out.contains("second please") && !out.contains("and also this") && !out.contains("/exit"));
        // the cut transcript's last turn is the first one
        assert_eq!(analyze(out.as_bytes()).unwrap().uuid, "u1");
    }

    #[test]
    fn commands_and_results_are_not_prompts() {
        let only = [
            row(json!({"type":"user","uuid":"x","message":{"content":"<command-name>/exit</command-name>"}})),
            row(json!({"type":"user","uuid":"y","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"x"}]}})),
            row(json!({"type":"user","uuid":"z","isMeta":true,"message":{"content":"meta"}})),
        ]
        .join("\n");
        assert!(analyze(only.as_bytes()).is_err());
    }

    #[test]
    fn undoing_the_first_prompt_leaves_no_conversation() {
        let f = fixture();
        let t = analyze(f.as_bytes()).unwrap();
        assert!(has_conversation(&cut_jsonl(f.as_bytes(), &t)));
        // cut the cut once more: back before "first", only session rows are left
        let once = cut_jsonl(f.as_bytes(), &t);
        let t1 = analyze(&once).unwrap();
        assert_eq!(t1.uuid, "u1");
        let none = cut_jsonl(&once, &t1);
        assert!(!none.is_empty());
        assert!(!has_conversation(&none));
        assert!(!has_conversation(b""));
    }

    #[test]
    fn a_hand_started_agent_gets_a_name() {
        assert_eq!(restart_name(Some("undotest"), Some("Whatever"), false), "undotest");
        assert_eq!(restart_name(Some("master"), None, true), "master");
        // agent_slug adds a short hex suffix
        assert!(restart_name(None, Some("Fix the parser"), false).starts_with("fix-the-parser-"));
        assert!(restart_name(Some(""), Some(""), false).starts_with("agent-"));
        assert!(restart_name(None, Some("Master plan"), false).starts_with("a-master-plan-"));
        assert!(restart_name(None, Some("Master plan"), true).starts_with("master-plan-"));
        assert!(!restart_name(None, Some("42 things"), false).is_empty());
    }

    #[test]
    fn iso_times() {
        assert_eq!(chrono_ms("1970-01-01T00:00:01.500Z"), Some(1500));
        assert_eq!(chrono_ms("2026-10-10T19:38:49.216Z"), Some(1791661129216));
    }
}
