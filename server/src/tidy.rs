//! Clean up agents (API.md "Clean up agents"). Builds on Find and continue (find.rs): **sleep = close the pane**
//! (herdr `pane.close`, as `/api/a/{pane}/close`; the tab goes with its last pane) and **wake = Continue**
//! (`/api/sessions/{sid}/resume`). Nothing here kills a process.
//!
//! `GET /api/tidy` sorts the live Claude agents into
//! - **keep**: the home agent (herdr name `master*`), background services (a `service`), and any never-sleep pane
//!   (`SHEPRD_NEVER_SLEEP`) (`lock`, never offered: `/api/tidy/sleep` refuses them whatever the client sends), pinned,
//!   working / blocked, held messages, a timer that prompts the pane, kept ("Keep" = 24 h), active in the last
//!   `idleMin` minutes;
//! - **check** (look first): `q` the last message asks the user something (Haiku once per turn, cached; a heuristic until
//!   then), `draft` a real draft in the input box (inputbox.rs: Claude's faint suggestion is not one), `git` files this
//!   session wrote that are still uncommitted / commits it made that no remote has, `srv` / `shell` / `sub` servers,
//!   background shells and subagents that stop when the pane closes, or a detached server it started and mentions;
//! - **safe**: the rest, with `ok` chips for what keeps running after sleep (a systemd service or timer it mentions).
//!
//! `POST /api/tidy/sleep {panes, note}` closes them after an 8 s undo window (`/api/tidy/undo {id}`); with `note`,
//! Haiku writes a 5-line handoff note from the transcript (server side, the agent is not woken) into
//! `<state>/handoffs/<sid>.md`, shown above Continue in the asleep chat (`GET /api/tidy/note/{sid}`).
//! `POST /api/tidy/fix {pane, kind, text}` answers / asks it to push / stop a server / sends the draft **with a
//! commitment to sleep**: when that turn ends without a new question the pane gets the same deferred close.
//! `snooze {pane}` (Keep, 24 h), `hide {hours}` (Not now), `prefs {idleMin}`, `GET pending` (undo toasts).

use crate::transcript::now_ms;
use crate::{App, Shared, json_err};
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// The undo window before a pane closes.
pub const UNDO_MS: i64 = 8_000;
/// "Keep" leaves an agent out of cleanup this long.
const SNOOZE_MS: i64 = 24 * 3_600_000;
/// Haiku calls a day (questions + notes), at most.
const DAY_CAP: u32 = 150;
/// A commitment to sleep that never came due is dropped after this.
const COMMIT_TTL: i64 = 3 * 3_600_000;
/// Default idle threshold, minutes (pref `idleMin`).
const IDLE_MIN: u32 = 60;
/// Panes that never sleep besides the home agent and background services: `SHEPRD_NEVER_SLEEP`, comma-separated
/// herdr pane ids (none by default). The sessions seen in them are remembered too, so a renumbered pane stays
/// protected.
const NEVER_PANES: &str = "";

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/tidy", get(api_tidy))
        .route("/api/tidy/sleep", post(api_sleep))
        .route("/api/tidy/undo", post(api_undo))
        .route("/api/tidy/snooze", post(api_snooze))
        .route("/api/tidy/hide", post(api_hide))
        .route("/api/tidy/fix", post(api_fix))
        .route("/api/tidy/prefs", post(api_prefs))
        .route("/api/tidy/pending", get(api_pending))
        .route("/api/tidy/note/{sid}", get(api_note))
        .route("/api/tidy/just/hide", post(api_just_hide))
}

// ---------------------------------------------------------------------------
// Store (`<state>/tidy.json`) and runtime state

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct QInfo {
    /// completionSeq + last message ts it was asked for
    pub key: String,
    pub asks: bool,
    pub chip: Option<String>,
    pub reply: Option<String>,
    /// what it waits on the user for, one line ("your pick between the two layouts"; catch-up's "Waiting on you for")
    pub wait: Option<String>,
    /// "haiku" | "guess"
    pub by: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
#[serde(default)]
pub struct Store {
    pub q: HashMap<String, QInfo>,
    /// sid → kept until (ms)
    pub snooze: HashMap<String, i64>,
    pub hidden_until: i64,
    /// sid → put to sleep at (ms)
    pub slept: HashMap<String, i64>,
    /// the user hid the "Just put to sleep" group at (ms): sleeps before it are no longer listed there
    pub just_hidden: i64,
    /// sessions seen in a never-sleep pane
    pub never: Vec<String>,
    /// catch-up's Keep: sid → the question key it was kept at (it comes back when the agent asks something new)
    pub ckeep: HashMap<String, String>,
    pub idle_min: u32,
    pub day: i64,
    pub calls: u32,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

fn store_path() -> PathBuf {
    crate::state_dir().join("tidy.json")
}

pub(crate) fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut g = STORE.lock().unwrap();
    let s = g.get_or_insert_with(|| std::fs::read(store_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default());
    f(s)
}

pub(crate) fn save() {
    let b = with(|s| {
        let now = now_ms();
        s.snooze.retain(|_, t| *t > now);
        s.slept.retain(|_, t| now - *t < 30 * 86_400_000);
        if s.q.len() > 400 {
            s.q.clear();
        }
        serde_json::to_vec(s).ok()
    });
    if let Some(b) = b {
        let tmp = store_path().with_extension("json.tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(&tmp, store_path());
        }
    }
}

pub(crate) fn idle_ms() -> i64 {
    if let Some(ms) = std::env::var("SHEPRD_TIDY_IDLE_MS").ok().and_then(|v| v.parse().ok()) {
        return ms; // tests
    }
    let m = with(|s| s.idle_min);
    (if m == 0 { IDLE_MIN } else { m }) as i64 * 60_000
}

/// One Haiku call allowed today? Counts it.
fn take_call() -> bool {
    if std::env::var("SHEPRD_TIDY_HAIKU").is_ok_and(|v| v == "0") {
        return false;
    }
    let day = now_ms() / 86_400_000;
    with(|s| {
        if s.day != day {
            s.day = day;
            s.calls = 0;
        }
        if s.calls >= DAY_CAP {
            return false;
        }
        s.calls += 1;
        true
    })
}

struct Pending {
    id: String,
    /// (pane, sid, name)
    items: Vec<(String, String, String)>,
    at: i64,
    due: i64,
    note: bool,
    /// "user" | "commit"
    by: &'static str,
    cancelled: bool,
    done: bool,
    /// who asked (tidy-log.jsonl): `origin()` of the request, or the commitment that triggered it
    src: Value,
}

struct Commit {
    sid: String,
    name: String,
    seq0: u64,
    at: i64,
    saw_working: bool,
    note: bool,
}

#[derive(Default)]
struct Rt {
    pending: Vec<Pending>,
    commits: HashMap<String, Commit>,
    scans: HashMap<String, Scan>,
    asking: HashSet<String>,
    next: u64,
    cache: Option<(i64, Value)>,
}

static RT: Mutex<Option<Rt>> = Mutex::new(None);

fn rt<R>(f: impl FnOnce(&mut Rt) -> R) -> R {
    let mut g = RT.lock().unwrap();
    f(g.get_or_insert_with(Rt::default))
}

pub(crate) fn invalidate() {
    rt(|r| r.cache = None);
}

/// Panes going to sleep right now (inside an undo window) or committed to sleep after a fix.
pub(crate) fn busy_panes() -> HashSet<String> {
    rt(|r| r.pending.iter().filter(|p| !p.cancelled && !p.done).flat_map(|p| p.items.iter().map(|i| i.0.clone())).chain(r.commits.keys().cloned()).collect())
}

// ---------------------------------------------------------------------------
// Never sleep

fn never_panes() -> Vec<String> {
    std::env::var("SHEPRD_NEVER_SLEEP").unwrap_or_else(|_| NEVER_PANES.into()).split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn s(a: &Value, k: &str) -> String {
    a[k].as_str().unwrap_or("").to_string()
}

/// A background service (it reports itself through its mod, services.rs): it runs unattended, so it never sleeps.
fn is_service(a: &Value) -> bool {
    !a["service"].is_null()
}

/// Why this agent may never be put to sleep, if so. Server-side guard for every close this module makes.
pub fn never_why(a: &Value, never: &[String], never_sids: &[String]) -> Option<&'static str> {
    let master = |k: &str| s(a, k).to_lowercase().starts_with("master");
    if a["home"] == true || master("name") || master("tabLabel") {
        return Some("The home agent never sleeps");
    }
    if is_service(a) {
        return Some("Background services never sleep");
    }
    let sid = s(a, "session");
    if never.contains(&s(a, "pane")) || (!sid.is_empty() && never_sids.contains(&sid)) {
        return Some("never sleeps");
    }
    None
}

/// Live agents from the state, never-sleep sessions remembered.
pub(crate) fn live(app: &App) -> Vec<Value> {
    let st = app.inner.lock().unwrap().state.clone();
    let v: Value = serde_json::from_str(&st).unwrap_or(Value::Null);
    let agents = v["agents"].as_array().cloned().unwrap_or_default();
    let never = never_panes();
    let add: Vec<String> = agents
        .iter()
        .filter(|a| never.contains(&s(a, "pane")) || a["home"] == true || is_service(a))
        .filter_map(|a| a["session"].as_str().map(str::to_string))
        .collect();
    if with(|st| {
        let mut ch = false;
        for sid in add {
            if !st.never.contains(&sid) {
                st.never.push(sid);
                ch = true;
            }
        }
        if st.never.len() > 50 {
            let n = st.never.len() - 50;
            st.never.drain(..n);
        }
        ch
    }) {
        save();
    }
    agents
}

pub(crate) fn guard(a: &Value) -> Option<&'static str> {
    let ns = with(|s| s.never.clone());
    never_why(a, &never_panes(), &ns)
}

pub(crate) fn name_of(a: &Value) -> String {
    if a["home"] == true {
        return crate::home_name();
    }
    a["short"].as_str().or(a["title"].as_str()).filter(|t| !t.is_empty()).or(a["name"].as_str()).unwrap_or("Agent").to_string()
}

// ---------------------------------------------------------------------------
// Transcript scan: files written, commits made, the last messages (incremental, by file offset)

#[derive(Clone, Default, Debug)]
pub struct Scan {
    off: u64,
    pub files: Vec<String>,
    /// (repo dir, short hash)
    pub commits: Vec<(String, String)>,
    /// tool_use id → repo dir of a Bash command that ran `git commit`
    pending: HashMap<String, String>,
    /// last assistant texts (≤ 3), full
    pub asst: Vec<String>,
    last_id: String,
    /// recent conversation for the note: (role, text ≤ 600)
    pub convo: Vec<(String, String)>,
    pub first: String,
    /// the user's last real prompt (not an agent's steer, not a server note, not "continue"): (text ≤ 2000, ts)
    pub said: Option<(String, i64)>,
    /// the last message another agent sent after it: (sender, text ≤ 300, ts)
    pub then: Option<(String, String, i64)>,
    /// when its last text was written (ms)
    pub asst_ts: i64,
}

/// A server / mod note typed in as a user message ("[Decision: …] The user picked …", "[Message from agent …]", "[Send to
/// Master …]"), or a word the phone sends for them ("continue"): not something the user said.
pub fn system_note(t: &str) -> bool {
    let t = t.trim();
    let first = t.lines().next().unwrap_or("");
    let bracket = first.starts_with('[') && first.find(']').is_some_and(|e| e < 100 && !first[1..e].starts_with("Image") && !first[1..e].starts_with("Pasted"));
    bracket || t.starts_with('/') || t.starts_with('!') || t.trim_end_matches(['.', '!']).eq_ignore_ascii_case("continue")
}

/// `{text, ts, then: {from, text, ts} | null}` for the catch-up cards and the asleep wake card, or null.
pub fn said_json(sc: &Scan) -> Value {
    let then = sc.then.as_ref().map(|(f, t, ts)| json!({"from": f, "text": t, "ts": ts}));
    match &sc.said {
        Some((t, ts)) => json!({"text": t, "ts": ts, "then": then}),
        None if then.is_some() => json!({"text": null, "ts": null, "then": then}),
        None => Value::Null,
    }
}

fn repo_of_cmd(cmd: &str, cwd: &str) -> String {
    let re = regex_lite::Regex::new(r#"(?:git\s+-C\s+|cd\s+)("[^"]+"|'[^']+'|[^\s;&|]+)"#).unwrap();
    let mut dir = cwd.to_string();
    for c in re.captures_iter(cmd) {
        let p = c[1].trim_matches(|c| c == '"' || c == '\'').to_string();
        let p = if let Some(r) = p.strip_prefix("~/") { crate::home().join(r).to_string_lossy().to_string() } else { p };
        dir = if p.starts_with('/') { p } else { format!("{}/{p}", dir.trim_end_matches('/')) };
    }
    dir
}

fn text_of(c: &Value) -> String {
    match c {
        Value::String(t) => t.clone(),
        Value::Array(a) => a.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Reads the transcript from `sc.off` on. `cwd` = the session's folder.
pub fn scan_more(sc: &mut Scan, path: &Path, cwd: &str) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < sc.off {
        *sc = Scan::default();
    }
    if len == sc.off || f.seek(SeekFrom::Start(sc.off)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if f.take(len - sc.off).read_to_end(&mut buf).is_err() {
        return;
    }
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { return };
    sc.off += end as u64 + 1;
    let commit_re = regex_lite::Regex::new(r"(?m)^\[[^\]\s]+(?: \(root-commit\))? ([0-9a-f]{7,40})\]").unwrap();
    for line in buf[..end].split(|&b| b == b'\n') {
        let Ok(d) = serde_json::from_slice::<Value>(line) else { continue };
        let t = d["type"].as_str().unwrap_or("");
        let m = &d["message"];
        if t == "assistant" {
            let id = m["id"].as_str().unwrap_or("").to_string();
            for b in m["content"].as_array().into_iter().flatten() {
                match b["type"].as_str() {
                    Some("text") => {
                        let tx = b["text"].as_str().unwrap_or("").trim().to_string();
                        if tx.is_empty() {
                            continue;
                        }
                        if !id.is_empty() && id == sc.last_id && !sc.asst.is_empty() {
                            let l = sc.asst.last_mut().unwrap();
                            l.push_str("\n\n");
                            l.push_str(&tx);
                        } else {
                            sc.asst.push(tx.clone());
                            if sc.asst.len() > 3 {
                                sc.asst.remove(0);
                            }
                        }
                        sc.last_id = id.clone();
                        sc.asst_ts = crate::transcript::parse_ts(d["timestamp"].as_str().unwrap_or(""));
                        sc.convo.push(("assistant".into(), tx.chars().take(600).collect()));
                    }
                    Some("tool_use") => {
                        let name = b["name"].as_str().unwrap_or("");
                        let inp = &b["input"];
                        if matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
                            if let Some(p) = inp["file_path"].as_str().or(inp["notebook_path"].as_str()) {
                                if !sc.files.iter().any(|x| x == p) && sc.files.len() < 500 {
                                    sc.files.push(p.to_string());
                                }
                            }
                        } else if name == "Bash" {
                            let cmd = inp["command"].as_str().unwrap_or("");
                            if cmd.contains("git") && cmd.contains("commit") {
                                if let Some(id) = b["id"].as_str() {
                                    sc.pending.insert(id.to_string(), repo_of_cmd(cmd, cwd));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        } else if t == "user" {
            if d["isMeta"] == true || d["isCompactSummary"] == true {
                continue;
            }
            let c = &m["content"];
            if let Some(arr) = c.as_array() {
                for b in arr {
                    if b["type"] == "tool_result" {
                        let Some(repo) = b["tool_use_id"].as_str().and_then(|id| sc.pending.remove(id)) else { continue };
                        let out = text_of(&b["content"]);
                        for cap in commit_re.captures_iter(&out) {
                            let h = cap[1].to_string();
                            if !sc.commits.iter().any(|(_, x)| *x == h) {
                                sc.commits.push((repo.clone(), h));
                            }
                        }
                    }
                }
            }
            let tx = text_of(c);
            let tool = c.as_array().is_some_and(|a| a.iter().any(|b| b["type"] == "tool_result"));
            if !tool {
                let ts = crate::transcript::parse_ts(d["timestamp"].as_str().unwrap_or(""));
                match crate::transcript::user_msg(&tx) {
                    Some((None, t)) if !system_note(&t) => {
                        sc.said = Some((t.trim().chars().take(2000).collect(), ts));
                        sc.then = None;
                    }
                    Some((Some(from), t)) => sc.then = Some((from, t.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect(), ts)),
                    _ => {}
                }
            }
            let tx = tx.trim();
            if tx.is_empty() || tx.starts_with('<') || tool {
                continue;
            }
            if sc.first.is_empty() {
                sc.first = tx.chars().take(500).collect();
            }
            sc.convo.push(("user".into(), tx.chars().take(600).collect()));
        }
        if sc.convo.len() > 14 {
            let n = sc.convo.len() - 14;
            sc.convo.drain(..n);
        }
    }
    if sc.commits.len() > 60 {
        let n = sc.commits.len() - 60;
        sc.commits.drain(..n);
    }
}

pub(crate) fn scan_for(app: &App, sid: &str, cwd: &str) -> Scan {
    let Some(path) = crate::find_session(app, sid) else { return Scan::default() };
    let mut sc = rt(|r| r.scans.get(sid).cloned()).unwrap_or_default();
    scan_more(&mut sc, &path, cwd);
    rt(|r| {
        if r.scans.len() > 80 {
            r.scans.clear();
        }
        r.scans.insert(sid.to_string(), sc.clone());
    });
    sc
}

// ---------------------------------------------------------------------------
// git: what this session left behind

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("git").arg("-C").arg(dir).args(args).stderr(std::process::Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).to_string())
}

fn repo_root(p: &Path) -> Option<PathBuf> {
    let mut d = if p.is_dir() { Some(p) } else { p.parent() };
    while let Some(x) = d {
        if x.join(".git").exists() {
            return Some(x.to_path_buf());
        }
        d = x.parent();
    }
    None
}

/// (uncommitted files this session wrote, its commits that no remote has, repos).
fn git_left(sc: &Scan) -> (usize, usize, Vec<String>) {
    let mut by: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for f in &sc.files {
        if let Some(r) = repo_root(Path::new(f)) {
            by.entry(r).or_default().push(f.clone());
        }
    }
    let (mut dirty, mut unpushed, mut repos) = (0, 0, Vec::new());
    for (r, fs) in by {
        let rs = r.to_string_lossy().to_string();
        let mut args = vec!["status", "--porcelain", "--"];
        args.extend(fs.iter().take(200).map(String::as_str));
        let n = git(&rs, &args).map(|o| o.lines().filter(|l| !l.trim().is_empty()).count()).unwrap_or(0);
        if n > 0 {
            dirty += n;
            repos.push(rs);
        }
    }
    let mut seen: HashMap<String, bool> = HashMap::new();
    for (dir, h) in sc.commits.iter().rev().take(20) {
        let Some(root) = repo_root(Path::new(dir)) else { continue };
        let rs = root.to_string_lossy().to_string();
        let has_remote = *seen.entry(rs.clone()).or_insert_with(|| git(&rs, &["remote"]).is_some_and(|o| !o.trim().is_empty()));
        if !has_remote || git(&rs, &["cat-file", "-e", &format!("{h}^{{commit}}")]).is_none() {
            continue;
        }
        if git(&rs, &["branch", "-r", "--contains", h]).is_some_and(|o| o.trim().is_empty()) {
            unpushed += 1;
            if !repos.contains(&rs) {
                repos.push(rs);
            }
        }
    }
    (dirty, unpushed, repos)
}

// ---------------------------------------------------------------------------
// Processes: what stops with the pane, what keeps running

struct Procs {
    ppid: HashMap<u32, u32>,
    comm: HashMap<u32, String>,
}

fn procs() -> Procs {
    let mut p = Procs { ppid: HashMap::new(), comm: HashMap::new() };
    let Ok(rd) = std::fs::read_dir("/proc") else { return p };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(st) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        let (Some(a), Some(b)) = (st.find('('), st.rfind(')')) else { continue };
        let comm = st[a + 1..b].to_string();
        let pp = st[b + 2..].split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(0);
        p.ppid.insert(pid, pp);
        p.comm.insert(pid, comm);
    }
    p
}

fn descendants(p: &Procs, root: u32) -> Vec<u32> {
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&c, &pp) in &p.ppid {
        kids.entry(pp).or_default().push(c);
    }
    let (mut out, mut st) = (vec![], vec![root]);
    while let Some(x) = st.pop() {
        if out.len() > 400 {
            break;
        }
        out.push(x);
        st.extend(kids.get(&x).into_iter().flatten().copied().filter(|&c| c != x));
    }
    out
}

/// LISTEN sockets on the box: port → inode.
fn listen_table() -> HashMap<u16, String> {
    let mut out = HashMap::new();
    for (f, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        if let Ok(s) = std::fs::read_to_string(f) {
            for (_, port, inode) in crate::apps::parse_listen(&s, v6) {
                out.entry(port).or_insert(inode);
            }
        }
    }
    out
}

/// The process holding a socket inode (my own processes only).
fn owner_of(inode: &str) -> Option<u32> {
    let want = format!("socket:[{inode}]");
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for f in fds.flatten() {
            if std::fs::read_link(f.path()).is_ok_and(|l| l.to_str() == Some(want.as_str())) {
                return Some(pid);
            }
        }
    }
    None
}

/// The systemd user unit a process runs in (`grafana.service`), if any.
fn unit_of(pid: u32) -> Option<String> {
    let cg = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    // the leaf only: a terminal's scope sits under user@1000.service too
    let last = cg.lines().last()?.rsplit('/').next()?.to_string();
    // agents' own shells live in herdr's / the terminal's scope, never in a service of their own
    (last.ends_with(".service") && !last.starts_with("user@") && !last.starts_with("app-") && !last.contains("herdr")).then_some(last)
}

fn uptime_ms(pid: u32) -> Option<i64> {
    let st = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let b = st.rfind(')')?;
    let start: i64 = st[b + 2..].split_whitespace().nth(19)?.parse().ok()?;
    let btime: i64 = std::fs::read_to_string("/proc/stat").ok()?.lines().find_map(|l| l.strip_prefix("btime ").and_then(|x| x.trim().parse().ok()))?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as i64;
    Some(now_ms() - (btime * 1000 + start * 1000 / hz))
}

fn dur(ms: i64) -> String {
    let m = ms / 60_000;
    if m < 60 { format!("{}m", m.max(1)) } else if m < 48 * 60 { format!("{}h", m / 60) } else { format!("{}d", m / 1440) }
}

/// Ports an agent mentions in its last messages (":8790", "localhost:3417", "port 8790").
pub fn ports_in(text: &str) -> Vec<u16> {
    let re = regex_lite::Regex::new(r"(?i)(?:(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}|[a-z0-9-]+\.ts\.net|\s|\(|^)\*{0,2}:|\bport\s+)(\d{4,5})\b").unwrap();
    let mut out = vec![];
    for c in re.captures_iter(text) {
        if let Ok(p) = c[1].parse::<u16>() {
            if p >= 1024 && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

// unit → description, for "Grafana stays up on :3417"
static UNITS: Mutex<Option<(i64, HashMap<String, String>, Vec<Value>)>> = Mutex::new(None);

async fn units() -> (HashMap<String, String>, Vec<Value>) {
    if let Some((at, d, t)) = UNITS.lock().unwrap().clone() {
        if now_ms() - at < 120_000 {
            return (d, t);
        }
    }
    let mut desc = HashMap::new();
    if let Ok(o) = tokio::process::Command::new("systemctl")
        .args(["--user", "list-units", "--type=service", "--state=active", "--no-legend", "--plain", "--no-pager"])
        .output()
        .await
    {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() >= 5 {
                desc.insert(f[0].to_string(), f[4..].join(" "));
            }
        }
    }
    let timers = crate::sched::systemd_timers().await;
    *UNITS.lock().unwrap() = Some((now_ms(), desc.clone(), timers.clone()));
    (desc, timers)
}

/// "Grafana: dashboards (…)" → "Grafana"; else the unit name.
fn unit_name(unit: &str, desc: &str) -> String {
    let d = desc.split([':', '(', '—']).next().unwrap_or("").trim();
    if !d.is_empty() && d.len() <= 24 && !d.starts_with('[') { d.to_string() } else { unit.trim_end_matches(".service").to_string() }
}

// ---------------------------------------------------------------------------
// Questions: does the last message wait on the user?

/// Heuristic until Haiku answers (and when it is off): a question near the end, or "tell me / send me / should I …".
pub fn asks_guess(text: &str) -> bool {
    let t: String = text.chars().rev().take(500).collect::<Vec<_>>().into_iter().rev().collect();
    let no_urls = regex_lite::Regex::new(r"https?://\S+").unwrap().replace_all(&t, "");
    if no_urls.contains('?') {
        return true;
    }
    let re = regex_lite::Regex::new(r"(?i)\b(tell me (when|which|what|if)|let me know (when|which|what)|send me|should i\b|do you want|want me to|your call|waiting (on|for) (you|your)|once you('ve| have)?\b|which (one|option) )").unwrap();
    re.is_match(&no_urls)
}

fn q_prompt(text: &str) -> String {
    let cut: String = text.chars().take(2500).collect();
    format!(
        "The user manages many Claude coding agents from their phone. Below is the last message one agent sent before going idle. \
         Does it ask the user something or wait on their input, decision or action (a question, a choice, \"tell me when…\", \"send me…\", \
         \"should I…\")? A plain report, or \"done\" with an optional offer like \"let me know if you want more\", is NOT waiting.\n\
         Reply with JSON only: {{\"asks\": true|false, \"chip\": \"<if asks: 2-5 words saying what it waits on, e.g. Asks you a question, \
         Waiting on your measurements, Wants your pick>\", \"for\": \"<if asks: what exactly it waits on the user for, one line under 70 \
         characters, a lowercase noun phrase that completes 'Waiting on you for: ', e.g. your pick between the two layouts, the screen \
         sizes to test, a go-ahead to push; else empty>\", \"reply\": \"<if asks: a likely short reply the user could send, under 12 words, \
         first person; else empty>\"}}\n\nMessage:\n<<<\n{cut}\n>>>"
    )
}

/// (asks, chip, reply, wait)
pub fn parse_q(s: &str) -> Option<(bool, Option<String>, Option<String>, Option<String>)> {
    let a = s.find('{')?;
    let b = s.rfind('}')?;
    let v: Value = serde_json::from_str(s.get(a..=b)?).ok()?;
    let clean = |k: &str, max: usize| v[k].as_str().map(|x| x.split_whitespace().collect::<Vec<_>>().join(" ")).map(|x| x.trim_matches('"').trim_end_matches('.').to_string()).filter(|x| !x.is_empty() && x.chars().count() <= max);
    let wait = clean("for", 90).map(|w| w.trim_start_matches("Waiting on you for:").trim().to_string()).filter(|w| !w.is_empty());
    Some((v["asks"].as_bool()?, clean("chip", 40), clean("reply", 120), wait))
}

/// Haiku's verdict on `text` (fake in tests: `SHEPRD_FAKE_TIDY=asks` / `no`).
async fn ask_haiku(text: &str) -> Option<(bool, Option<String>, Option<String>, Option<String>)> {
    if let Ok(f) = std::env::var("SHEPRD_FAKE_TIDY") {
        let asks = text.contains('?') || f == "asks";
        return Some((asks, asks.then(|| "Asks you a question".into()), asks.then(|| "Yes, go ahead".into()), asks.then(|| "a yes or no on going ahead".into())));
    }
    if !take_call() {
        return None;
    }
    let v = crate::newagent::run_haiku(None, &q_prompt(text), Duration::from_secs(60)).await.ok()?;
    parse_q(v["result"].as_str().unwrap_or(""))
}

pub(crate) fn q_key(a: &Value) -> String {
    format!("{}:{}", a["completionSeq"].as_u64().unwrap_or(0), a["last"]["ts"].as_i64().unwrap_or(0))
}

/// Classifies `sid`'s last message in the background (once per key).
pub(crate) fn q_later(app: &Shared, sid: String, key: String, text: String) {
    if !rt(|r| r.asking.insert(sid.clone())) {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        let r = ask_haiku(&text).await;
        if let Some((asks, chip, reply, wait)) = r {
            with(|s| s.q.insert(sid.clone(), QInfo { key, asks, chip, reply, wait, by: "haiku".into() }));
            save();
            invalidate();
            app.refresh.notify_one();
        }
        rt(|r| r.asking.remove(&sid));
    });
}

// ---------------------------------------------------------------------------
// Classification (pure: unit-tested)

#[derive(Default, Debug, Clone)]
pub struct Facts {
    pub idle: i64,
    pub never: Option<&'static str>,
    pub held: usize,
    pub snoozed: bool,
    pub timer: Option<String>,
    pub q: Option<(Option<String>, Option<String>)>,
    pub draft: Option<String>,
    pub dirty: usize,
    pub unpushed: usize,
    /// (port, what, stops with the pane?, uptime)
    pub servers: Vec<(u16, String, bool, Option<i64>)>,
    pub shells: u64,
    pub subs: u64,
    pub ok: Vec<String>,
    /// Haiku's "Waiting on you for" line, with `q`
    pub wait: Option<String>,
    /// `said_json` of its scan (the user's last prompt), for the catch-up card
    pub said: Value,
}

fn reason(kind: &str, text: impl Into<String>) -> Value {
    json!({"kind": kind, "text": text.into()})
}

/// "keep" | "check" | "safe", and why.
pub fn classify(a: &Value, f: &Facts, threshold: i64) -> (&'static str, Vec<Value>) {
    let st = a["status"].as_str().unwrap_or("");
    if let Some(w) = f.never {
        return ("keep", vec![reason("lock", w)]);
    }
    if st == "working" {
        return ("keep", vec![reason("work", "working")]);
    }
    if st == "blocked" {
        return ("keep", vec![reason("work", "needs you now")]);
    }
    if a["pinned"] == true {
        return ("keep", vec![reason("pin", "pinned")]);
    }
    if f.held > 0 || a["queued"].as_array().is_some_and(|q| !q.is_empty()) {
        return ("keep", vec![reason("work", "has a message waiting")]);
    }
    if let Some(t) = &f.timer {
        return ("keep", vec![reason("pin", t.clone())]);
    }
    if f.snoozed {
        return ("keep", vec![reason("pin", "kept for today")]);
    }
    if f.idle < threshold {
        return ("keep", vec![reason("work", "active recently")]);
    }
    let mut r = vec![];
    if let Some((chip, reply)) = &f.q {
        let mut x = reason("q", chip.clone().unwrap_or_else(|| "Asks you a question".into()));
        x["fix"] = json!({"kind": "answer", "reply": reply});
        r.push(x);
    }
    if f.draft.is_some() {
        let mut x = reason("draft", "Unsent draft in its box");
        x["fix"] = json!({"kind": "send", "label": "Send draft"});
        r.push(x);
    }
    if f.dirty > 0 || f.unpushed > 0 {
        let t = match (f.dirty, f.unpushed) {
            (d, 0) => format!("{d} file{} not committed", if d == 1 { "" } else { "s" }),
            (0, u) => format!("{u} commit{} not pushed", if u == 1 { "" } else { "s" }),
            (d, u) => format!("{d} file{} not committed · {u} not pushed", if d == 1 { "" } else { "s" }),
        };
        let mut x = reason("git", t);
        let text = if f.dirty > 0 { "commit and push it" } else { "push it" };
        x["fix"] = json!({"kind": "push", "label": "Push", "text": text});
        r.push(x);
    }
    for (port, what, tree, up) in &f.servers {
        if *tree {
            r.push(reason("srv", format!("{what} on :{port} will stop")));
        } else {
            let mut x = reason("srv", format!("{what} on :{port}{}", up.map(|u| format!(" · up {}", dur(u))).unwrap_or_default()));
            x["fix"] = json!({"kind": "stop", "label": format!("Stop :{port}"), "port": port, "text": format!("Stop the server on :{port} now that we're done, then you're finished here.")});
            r.push(x);
        }
    }
    let tree_srv = f.servers.iter().filter(|s| s.2).count() as u64;
    if f.shells > tree_srv {
        let n = f.shells - tree_srv;
        r.push(reason("shell", format!("{n} background shell{} will stop", if n == 1 { "" } else { "s" })));
    }
    if f.subs > 0 {
        r.push(reason("sub", format!("{} subagent{} running", f.subs, if f.subs == 1 { "" } else { "s" })));
    }
    if !r.is_empty() {
        r.extend(f.ok.iter().map(|o| reason("ok", o.clone())));
        return ("check", r);
    }
    ("safe", f.ok.iter().map(|o| reason("ok", o.clone())).collect())
}

// ---------------------------------------------------------------------------
// GET /api/tidy

pub(crate) fn row(a: &Value, idle: i64, reasons: Vec<Value>, full: Option<&str>) -> Value {
    let one = |t: &str| t.replace("**", "").split_whitespace().collect::<Vec<_>>().join(" ");
    let last = a["last"]["text"].as_str().map(one).unwrap_or_default();
    json!({
        "pane": a["pane"], "sid": a["session"], "name": name_of(a), "tab": a["tabLabel"], "color": a["color"], "cwd": a["cwd"], "home": a["home"] == true,
        "status": a["status"], "idleMs": idle, "last": last, "full": full,
        "ctx": {"pct": a["ctx"]["pct"], "tokens": a["ctx"]["tokens"]}, "reasons": reasons,
    })
}

async fn facts_for(app: &Shared, a: &Value, f: &mut Facts, pr: &Procs, listen: &HashMap<u16, String>, desc: &HashMap<String, String>, timers: &[Value]) -> Option<String> {
    let pane = s(a, "pane");
    let sid = s(a, "session");
    let cwd = s(a, "cwd");
    // draft: only a real one (Claude's faint suggestion is not)
    if let Some(crate::inputbox::Input::Draft(t)) = crate::inputbox::read(&pane).await {
        f.draft = Some(t);
    }
    let (sid2, cwd2) = (sid.clone(), cwd.clone());
    let app2 = app.clone();
    let sc = tokio::task::spawn_blocking(move || {
        let sc = scan_for(&app2, &sid2, &cwd2);
        let g = git_left(&sc);
        (sc, g)
    })
    .await
    .ok();
    let (sc, (dirty, unpushed, _)) = sc.unwrap_or_default();
    f.said = said_json(&sc);
    f.dirty = dirty;
    f.unpushed = unpushed;
    let full = sc.asst.last().cloned().or_else(|| a["last"]["text"].as_str().filter(|_| a["last"]["role"] == "assistant").map(str::to_string));
    // the question
    let key = q_key(a);
    let cached = with(|s| s.q.get(&sid).filter(|q| q.key == key).cloned());
    match cached {
        Some(q) if q.asks => {
            f.wait = q.wait.clone();
            f.q = Some((q.chip, q.reply));
        }
        Some(_) => {}
        None => {
            if let Some(t) = &full {
                if asks_guess(t) {
                    f.q = Some((None, None));
                }
                q_later(app, sid.clone(), key, t.clone());
            }
        }
    }
    // servers in the pane's tree stop with it
    f.shells = a["jobs"]["running"].as_u64().unwrap_or(0);
    f.subs = a["subagents"]["running"].as_u64().unwrap_or(0);
    let shell = crate::herdr::call("pane.process_info", json!({"pane_id": pane}), Duration::from_secs(2)).await.ok().and_then(|v| v["process_info"]["shell_pid"].as_u64()).map(|p| p as u32);
    let tree: Vec<u32> = shell.map(|s| descendants(pr, s)).unwrap_or_default();
    let mine: Vec<u32> = tree.iter().copied().filter(|p| pr.comm.get(p).is_none_or(|c| c != "claude")).collect();
    for (_, port) in crate::apps::listening(&mine) {
        let pid = listen.get(&port).and_then(|i| owner_of(i));
        let what = pid.and_then(|p| pr.comm.get(&p).cloned()).unwrap_or_else(|| "Server".into());
        f.servers.push((port, what, true, None));
    }
    // what it mentions: a detached server (offer Stop) or one that keeps running (a systemd unit)
    let said = sc.asst.join("\n");
    for port in ports_in(&said) {
        if f.servers.iter().any(|s| s.0 == port) || port == 7878 {
            continue;
        }
        let Some(inode) = listen.get(&port) else { continue };
        let Some(pid) = tokio::task::spawn_blocking({
            let i = inode.clone();
            move || owner_of(&i)
        })
        .await
        .ok()
        .flatten() else {
            continue;
        };
        if tree.contains(&pid) {
            continue;
        }
        match unit_of(pid) {
            Some(u) => f.ok.push(format!("{} stays up on :{port}", unit_name(&u, desc.get(&u).map(String::as_str).unwrap_or("")))),
            None => {
                let what = pr.comm.get(&pid).cloned().unwrap_or_else(|| "Server".into());
                let what = if what.starts_with("python") || what.starts_with("node") { "Server".to_string() } else { what };
                f.servers.push((port, what, false, uptime_ms(pid)));
            }
        }
    }
    let low = said.to_lowercase();
    for t in timers {
        let name = t["name"].as_str().unwrap_or("");
        if t["active"] == "active" && name.len() >= 5 && t["agentPane"].is_null() && low.contains(&name.to_lowercase()) {
            f.ok.push(format!("{name} timer keeps running"));
        }
    }
    f.ok.truncate(2);
    full
}

async fn build(app: &Shared) -> Value {
    let agents = live(app);
    let now = now_ms();
    let threshold = idle_ms();
    let (snooze, hidden_until, never_sids) = with(|s| (s.snooze.clone(), s.hidden_until, s.never.clone()));
    let never = never_panes();
    let (desc, timers) = units().await;
    let (pr, listen) = tokio::task::spawn_blocking(|| (procs(), listen_table())).await.unwrap_or_else(|_| (Procs { ppid: HashMap::new(), comm: HashMap::new() }, HashMap::new()));
    let soon: HashSet<String> = rt(|r| r.pending.iter().filter(|p| !p.cancelled).flat_map(|p| p.items.iter().map(|i| i.0.clone())).collect());
    let (mut safe, mut check, mut keep) = (vec![], vec![], vec![]);
    for a in agents.iter().filter(|a| a["kind"] == "claude" && a["session"].is_string()) {
        let pane = s(a, "pane");
        if soon.contains(&pane) {
            continue; // going to sleep right now
        }
        let sid = s(a, "session");
        let last_ts = a["last"]["ts"].as_i64().unwrap_or(0).max(a["since"].as_i64().unwrap_or(0));
        let idle = if last_ts > 0 { now - last_ts } else { 0 };
        let mut f = Facts {
            idle,
            never: never_why(a, &never, &never_sids),
            held: crate::inputbox::held_for(&pane).len(),
            snoozed: snooze.get(&sid).is_some_and(|t| *t > now),
            timer: timers.iter().find(|t| t["agentPane"].as_str() == Some(pane.as_str()) && t["active"] == "active").map(|t| {
                t["next"].as_i64().map(|n| format!("a timer wakes it in {}", dur(n - now))).unwrap_or_else(|| "a timer prompts it".into())
            }),
            ..Default::default()
        };
        let pre = classify(a, &f, threshold);
        let mut full = None;
        if pre.0 != "keep" {
            full = facts_for(app, a, &mut f, &pr, &listen, &desc, &timers).await;
        }
        let (b, reasons) = classify(a, &f, threshold);
        let mut r = row(a, idle, reasons, full.as_deref().filter(|_| b == "check"));
        if b == "check" {
            r["said"] = std::mem::take(&mut f.said);
            r["wait"] = json!(f.wait);
        }
        match b {
            "keep" => keep.push(r),
            "check" => check.push(r),
            _ => safe.push(r),
        }
    }
    let by_idle = |v: &mut Vec<Value>| v.sort_by_key(|r| r["idleMs"].as_i64().unwrap_or(0));
    by_idle(&mut safe);
    by_idle(&mut check);
    json!({"safe": safe, "check": check, "keep": keep, "hiddenUntil": hidden_until, "idleMin": threshold / 60_000, "undoMs": UNDO_MS})
}

async fn api_tidy(State(app): State<Shared>) -> Response {
    if let Some((at, v)) = rt(|r| r.cache.clone()) {
        if now_ms() - at < 8_000 {
            return Json(awake_again(&app, with_pending(v))).into_response();
        }
    }
    let v = build(&app).await;
    rt(|r| r.cache = Some((now_ms(), v.clone())));
    Json(awake_again(&app, with_pending(v))).into_response()
}

/// `slept` without sessions that are live again in any pane (woken here, or resumed by hand / by another agent):
/// matched by session id, so "Just put to sleep" never offers Wake for an agent that is already up.
fn awake_again(app: &App, mut v: Value) -> Value {
    let live: HashSet<String> = live(app).iter().filter_map(|a| a["session"].as_str().map(str::to_string)).collect();
    if let Some(m) = v["slept"].as_object_mut() {
        m.retain(|sid, _| !live.contains(sid));
    }
    v
}

fn with_pending(mut v: Value) -> Value {
    let p = pending_json();
    v["pending"] = p["pending"].clone();
    v["commits"] = p["commits"].clone();
    v["slept"] = p["slept"].clone();
    v["justHidden"] = p["justHidden"].clone();
    v["justMs"] = p["justMs"].clone();
    v
}

// ---------------------------------------------------------------------------
// Sleep: deferred close, undo, notes

#[derive(Deserialize)]
struct SleepBody {
    panes: Vec<String>,
    #[serde(default)]
    note: bool,
}

/// Starts a deferred close of these agents; `Err` = refused (never-sleep panes, nothing to close).
fn start_sleep(app: &Shared, agents: &[Value], panes: &[String], note: bool, by: &'static str, src: Value) -> Result<Value, (StatusCode, Value)> {
    let mut items = vec![];
    let mut refused = vec![];
    for p in panes {
        let Some(a) = agents.iter().find(|a| a["pane"].as_str() == Some(p.as_str())) else { continue };
        if let Some(w) = guard(a) {
            refused.push(json!({"pane": p, "name": name_of(a), "why": w}));
            continue;
        }
        let Some(sid) = a["session"].as_str() else { continue };
        items.push((p.clone(), sid.to_string(), name_of(a)));
    }
    if !refused.is_empty() {
        let names: Vec<&str> = refused.iter().filter_map(|r| r["name"].as_str()).collect();
        return Err((StatusCode::FORBIDDEN, json!({"error": format!("{} never sleep{}", names.join(", "), if names.len() == 1 { "s" } else { "" }), "refused": refused})));
    }
    if items.is_empty() {
        return Err((StatusCode::NOT_FOUND, json!({"error": "no such agents"})));
    }
    let now = now_ms();
    let id = rt(|r| {
        r.next += 1;
        let id = format!("z{:x}{}", now / 1000 % 0xfffff, r.next);
        r.pending.retain(|p| now - p.due < 600_000);
        r.pending.push(Pending { id: id.clone(), items: items.clone(), at: now, due: now + UNDO_MS, note, by, cancelled: false, done: false, src: src.clone() });
        id
    });
    log_event("sleep", json!({"id": id, "by": by, "src": src, "note": note, "panes": items.iter().map(|i| &i.0).collect::<Vec<_>>(),
        "sids": items.iter().map(|i| &i.1).collect::<Vec<_>>(), "names": items.iter().map(|i| &i.2).collect::<Vec<_>>(), "dueMs": UNDO_MS}));
    invalidate();
    if note {
        for (_, sid, name) in &items {
            write_note_later(app, sid.clone(), name.clone(), id.clone());
        }
    }
    let app2 = app.clone();
    let id2 = id.clone();
    tokio::spawn(async move { finish(&app2, &id2).await });
    app.refresh.notify_one();
    Ok(json!({"id": id, "at": now, "due": now + UNDO_MS, "n": items.len(), "names": items.iter().map(|i| i.2.clone()).collect::<Vec<_>>(), "note": note}))
}

/// After the undo window: close each pane that is still idle and still the same session.
async fn finish(app: &Shared, id: &str) {
    tokio::time::sleep(Duration::from_millis(UNDO_MS as u64)).await;
    let Some((items, by, src)) = rt(|r| r.pending.iter_mut().find(|p| p.id == id && !p.cancelled).map(|p| {
        p.done = true;
        (p.items.clone(), p.by, p.src.clone())
    })) else {
        return;
    };
    let agents = live(app);
    for (pane, sid, _) in items {
        let Some(a) = agents.iter().find(|a| a["pane"].as_str() == Some(pane.as_str())) else { continue };
        if a["session"].as_str() != Some(sid.as_str()) || guard(a).is_some() || matches!(a["status"].as_str(), Some("working") | Some("blocked")) {
            eprintln!("tidy: kept {pane} awake (changed since)");
            log_event("close", json!({"id": id, "by": "deferred close after the undo window", "asked": by, "src": src, "pane": pane, "sid": sid, "result": "kept awake: changed since"}));
            continue;
        }
        match crate::herdr::quick("pane.close", json!({"pane_id": pane})).await {
            Ok(_) => {
                with(|s| s.slept.insert(sid.clone(), now_ms()));
                log_event("close", json!({"id": id, "by": "deferred close after the undo window", "asked": by, "src": src, "pane": pane, "sid": sid, "result": "closed"}));
            }
            Err(e) => {
                eprintln!("tidy: close {pane}: {e}");
                log_event("close", json!({"id": id, "by": "deferred close after the undo window", "asked": by, "src": src, "pane": pane, "sid": sid, "result": format!("error: {e}")}));
            }
        }
    }
    save();
    invalidate();
    app.refresh.notify_one();
}

async fn api_sleep(State(app): State<Shared>, h: HeaderMap, Json(b): Json<SleepBody>) -> Response {
    if b.panes.is_empty() || b.panes.len() > 60 {
        return json_err(StatusCode::BAD_REQUEST, "panes: 1-60");
    }
    let agents = live(&app);
    let src = origin(&h, "POST /api/tidy/sleep");
    match start_sleep(&app, &agents, &b.panes, b.note, "user", src.clone()) {
        Ok(v) => Json(v).into_response(),
        Err((c, v)) => {
            log_event("sleep.refused", json!({"src": src, "panes": b.panes, "status": c.as_u16(), "error": v["error"]}));
            (c, Json(v)).into_response()
        }
    }
}

#[derive(Deserialize)]
struct IdBody {
    id: String,
}

async fn api_undo(State(app): State<Shared>, h: HeaderMap, Json(b): Json<IdBody>) -> Response {
    log_event("undo", json!({"id": b.id, "src": origin(&h, "POST /api/tidy/undo")}));
    let r = rt(|r| {
        let p = r.pending.iter_mut().find(|p| p.id == b.id)?;
        if p.done || now_ms() >= p.due {
            return Some(Err(()));
        }
        p.cancelled = true;
        Some(Ok(p.items.iter().map(|i| i.1.clone()).collect::<Vec<_>>()))
    });
    match r {
        None => json_err(StatusCode::NOT_FOUND, "no such sleep"),
        Some(Err(())) => json_err(StatusCode::CONFLICT, "already asleep: Continue wakes it"),
        Some(Ok(sids)) => {
            for sid in sids {
                let _ = std::fs::remove_file(note_path(&sid)); // a note written for this sleep goes too
            }
            invalidate();
            app.refresh.notify_one();
            Json(json!({"ok": true})).into_response()
        }
    }
}

fn note_path(sid: &str) -> PathBuf {
    crate::state_dir().join("handoffs").join(format!("{sid}.md"))
}

fn note_prompt(name: &str, cwd: &str, sc: &Scan, ports: &[u16]) -> String {
    let files: Vec<String> = sc.files.iter().rev().take(8).map(|f| f.replace(&crate::home().to_string_lossy().to_string(), "~")).collect();
    let commits: Vec<&str> = sc.commits.iter().rev().take(5).map(|c| c.1.as_str()).collect();
    let convo: String = sc.convo.iter().map(|(r, t)| format!("{}: {t}\n", if r == "user" { "User" } else { "Agent" })).collect();
    format!(
        "Write a handoff note for a Claude coding agent (\"{name}\") that is being put to sleep, so the user and the agent itself (when \
         continued) know where it stood. Exactly 5 lines, plain text, no markdown, each under 120 characters, starting with these \
         labels in this order: \"Done: \", \"Open: \", \"Next: \", \"Files: \", \"Ports: \". Write \"none\" when there is nothing.\n\n\
         Folder: {cwd}\nFirst request: {}\nFiles it edited (newest first): {}\nCommits: {}\nPorts mentioned: {}\n\
         Recent conversation (oldest first):\n<<<\n{convo}>>>",
        sc.first,
        if files.is_empty() { "none".into() } else { files.join(", ") },
        if commits.is_empty() { "none".into() } else { commits.join(", ") },
        if ports.is_empty() { "none".into() } else { ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(", ") },
    )
}

const LABELS: [&str; 5] = ["Done", "Open", "Next", "Files", "Ports"];

/// The 5 labelled lines out of Haiku's reply; `None` unless all five are there.
pub fn parse_note(s: &str) -> Option<String> {
    let mut out = vec![];
    for l in LABELS {
        let line = s.lines().map(|x| x.trim().trim_start_matches(['-', '*', ' '])).find(|x| x.starts_with(&format!("{l}:")))?;
        let body = line[l.len() + 1..].trim();
        let body: String = body.chars().take(160).collect();
        out.push(format!("{l}: {}", if body.is_empty() { "none" } else { &body }));
    }
    Some(out.join("\n"))
}

/// Without Haiku: the same 5 lines from what the scan knows.
fn fallback_note(sc: &Scan, ports: &[u16]) -> String {
    let one = |t: &str| t.replace("**", "").split_whitespace().collect::<Vec<_>>().join(" ").chars().take(110).collect::<String>();
    let done = sc.asst.last().map(|t| one(t)).unwrap_or_else(|| "none".into());
    let files: Vec<String> = sc.files.iter().rev().take(3).map(|f| f.rsplit('/').next().unwrap_or(f).to_string()).collect();
    format!(
        "Done: {done}\nOpen: unknown (no summary)\nNext: Continue to pick up from the last message\nFiles: {}\nPorts: {}",
        if files.is_empty() { "none".into() } else { files.join(", ") },
        if ports.is_empty() { "none".into() } else { ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(", ") }
    )
}

fn write_note_later(app: &Shared, sid: String, name: String, id: String) {
    let app = app.clone();
    tokio::spawn(async move {
        let cwd = crate::find::place(&sid).and_then(|i| i.cwd).unwrap_or_default();
        let (app2, sid2, cwd2) = (app.clone(), sid.clone(), cwd.clone());
        let sc = tokio::task::spawn_blocking(move || scan_for(&app2, &sid2, &cwd2)).await.unwrap_or_default();
        let ports = ports_in(&sc.asst.join("\n"));
        let text = if let Ok(f) = std::env::var("SHEPRD_FAKE_TIDY") {
            let _ = f;
            fallback_note(&sc, &ports)
        } else if take_call() {
            match crate::newagent::run_haiku(None, &note_prompt(&name, &cwd, &sc, &ports), Duration::from_secs(90)).await {
                Ok(v) => parse_note(v["result"].as_str().unwrap_or("")).unwrap_or_else(|| fallback_note(&sc, &ports)),
                Err(e) => {
                    eprintln!("tidy note {sid}: {e}");
                    fallback_note(&sc, &ports)
                }
            }
        } else {
            fallback_note(&sc, &ports)
        };
        if rt(|r| r.pending.iter().any(|p| p.id == id && p.cancelled)) {
            return; // undone meanwhile
        }
        let p = note_path(&sid);
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(&p, format!("{text}\n"));
    });
}

async fn api_note(AxPath(sid): AxPath<String>) -> Response {
    if !crate::valid_sid(&sid) {
        return json_err(StatusCode::BAD_REQUEST, "bad session id");
    }
    let p = note_path(&sid);
    match std::fs::read_to_string(&p) {
        Ok(t) => {
            let at = std::fs::metadata(&p).map(|m| crate::transcript::mtime_ms(&m)).unwrap_or(0);
            Json(json!({"note": t.trim_end(), "at": at})).into_response()
        }
        Err(_) => json_err(StatusCode::NOT_FOUND, "no note"),
    }
}

// ---------------------------------------------------------------------------
// Keep, Not now, prefs

#[derive(Deserialize)]
struct PaneBody {
    pane: String,
}

async fn api_snooze(State(app): State<Shared>, Json(b): Json<PaneBody>) -> Response {
    let agents = live(&app);
    let Some(sid) = agents.iter().find(|a| a["pane"].as_str() == Some(b.pane.as_str())).and_then(|a| a["session"].as_str()) else {
        return json_err(StatusCode::NOT_FOUND, "unknown pane");
    };
    let until = now_ms() + SNOOZE_MS;
    with(|s| s.snooze.insert(sid.to_string(), until));
    save();
    invalidate();
    Json(json!({"ok": true, "until": until})).into_response()
}

#[derive(Deserialize)]
struct HideBody {
    hours: Option<f64>,
}

async fn api_hide(Json(b): Json<HideBody>) -> Response {
    let h = b.hours.unwrap_or(6.0).clamp(0.0, 72.0);
    let until = now_ms() + (h * 3_600_000.0) as i64;
    with(|s| s.hidden_until = until);
    save();
    invalidate();
    Json(json!({"ok": true, "hiddenUntil": until})).into_response()
}

#[derive(Deserialize)]
struct PrefsBody {
    #[serde(rename = "idleMin")]
    idle_min: u32,
}

async fn api_prefs(Json(b): Json<PrefsBody>) -> Response {
    if !(5..=24 * 60).contains(&b.idle_min) {
        return json_err(StatusCode::BAD_REQUEST, "idleMin: 5-1440");
    }
    with(|s| s.idle_min = b.idle_min);
    save();
    invalidate();
    Json(json!({"ok": true, "idleMin": b.idle_min})).into_response()
}

// ---------------------------------------------------------------------------
// Fixes with a commitment to sleep

#[derive(Deserialize)]
struct FixBody {
    pane: String,
    /// answer | push | stop | send
    kind: String,
    text: Option<String>,
    #[serde(default)]
    note: bool,
}

async fn api_fix(State(app): State<Shared>, h: HeaderMap, Json(b): Json<FixBody>) -> Response {
    log_event("fix", json!({"pane": b.pane, "kind": b.kind, "src": origin(&h, "POST /api/tidy/fix")}));
    let agents = live(&app);
    let Some(a) = agents.iter().find(|a| a["pane"].as_str() == Some(b.pane.as_str())).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "unknown pane");
    };
    if let Some(w) = guard(&a) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": format!("{} {w}", name_of(&a))}))).into_response();
    }
    let Some(sid) = a["session"].as_str().map(str::to_string) else { return json_err(StatusCode::NOT_FOUND, "pane has no session") };
    let commit = Commit { sid, name: name_of(&a), seq0: a["completionSeq"].as_u64().unwrap_or(0), at: now_ms(), saw_working: false, note: b.note };
    let r = match b.kind.as_str() {
        "answer" | "push" | "stop" => {
            let text = b.text.as_deref().map(str::trim).unwrap_or("").to_string();
            if text.is_empty() || text.len() > 20_000 {
                return json_err(StatusCode::BAD_REQUEST, "text required");
            }
            rt(|r| r.commits.insert(b.pane.clone(), commit));
            let body = crate::PromptBody { text, nonce: None, from: None };
            crate::api_prompt(State(app.clone()), AxPath(b.pane.clone()), HeaderMap::new(), Json(body)).await
        }
        "send" => {
            if !matches!(crate::inputbox::read(&b.pane).await, Some(crate::inputbox::Input::Draft(_))) {
                return json_err(StatusCode::CONFLICT, "no draft in its input box");
            }
            rt(|r| r.commits.insert(b.pane.clone(), commit));
            match crate::herdr::quick("pane.send_keys", json!({"pane_id": b.pane, "keys": ["enter"]})).await {
                Ok(_) => Json(json!({"ok": true})).into_response(),
                Err(e) => crate::herdr_err(e),
            }
        }
        _ => return json_err(StatusCode::BAD_REQUEST, "kind: answer | push | stop | send"),
    };
    if !r.status().is_success() {
        rt(|r| r.commits.remove(&b.pane));
    } else {
        invalidate();
    }
    r
}

/// Watches committed panes: once the turn the fix started is over and the agent asks nothing new, it goes to sleep.
async fn watch_commits(app: &Shared) {
    let due: Vec<(String, String, bool)> = {
        let agents = live(app);
        let now = now_ms();
        let mut out = vec![];
        rt(|r| {
            r.commits.retain(|pane, c| {
                let Some(a) = agents.iter().find(|a| a["pane"].as_str() == Some(pane.as_str())) else { return false };
                if a["session"].as_str() != Some(c.sid.as_str()) || now - c.at > COMMIT_TTL || a["status"] == "blocked" {
                    return false; // gone, changed, stale, or asking (a permission prompt / menu)
                }
                if a["status"] == "working" {
                    c.saw_working = true;
                    return true;
                }
                let seq = a["completionSeq"].as_u64().unwrap_or(0);
                if (c.saw_working || seq > c.seq0) && now - c.at > 3_000 {
                    out.push((pane.clone(), c.sid.clone(), c.note));
                    return false;
                }
                true
            })
        });
        out
    };
    for (pane, sid, note) in due {
        let app = app.clone();
        tokio::spawn(async move {
            let agents = live(&app);
            let Some(a) = agents.iter().find(|a| a["pane"].as_str() == Some(pane.as_str())).cloned() else { return };
            let cwd = s(&a, "cwd");
            let (app2, sid2) = (app.clone(), sid.clone());
            let sc = tokio::task::spawn_blocking(move || scan_for(&app2, &sid2, &cwd)).await.unwrap_or_default();
            let text = sc.asst.last().cloned().or_else(|| a["last"]["text"].as_str().map(str::to_string)).unwrap_or_default();
            let (asks, chip, reply, wait, by) = match ask_haiku(&text).await {
                Some((x, c, r, w)) => (x, c, r, w, "haiku"),
                None => (asks_guess(&text), None, None, None, "guess"),
            };
            with(|s| s.q.insert(sid.clone(), QInfo { key: q_key(&a), asks, chip, reply, wait, by: by.into() }));
            save();
            if asks {
                eprintln!("tidy: {pane} asked again: stays awake");
                invalidate();
                return;
            }
            let src = json!({"source": "sleep commitment after a fix (its turn ended with no new question)", "endpoint": "tidy::watch_commits", "sid": sid});
            if let Err((_, e)) = start_sleep(&app, &agents, std::slice::from_ref(&pane), note, "commit", src) {
                eprintln!("tidy: commitment sleep {pane}: {e}");
            }
        });
    }
}

fn pending_json() -> Value {
    let now = now_ms();
    rt(|r| {
        let p: Vec<Value> = r
            .pending
            .iter()
            .filter(|p| !p.cancelled && now - p.due < 60_000)
            .map(|p| json!({"id": p.id, "n": p.items.len(), "names": p.items.iter().map(|i| i.2.clone()).collect::<Vec<_>>(), "panes": p.items.iter().map(|i| i.0.clone()).collect::<Vec<_>>(), "sids": p.items.iter().map(|i| i.1.clone()).collect::<Vec<_>>(), "at": p.at, "due": p.due, "by": p.by, "note": p.note, "done": p.done}))
            .collect();
        let c: Vec<Value> = r.commits.iter().map(|(pane, c)| json!({"pane": pane, "sid": c.sid, "name": c.name, "at": c.at})).collect();
        let (slept, just_hidden) = with(|s| (json!(s.slept), s.just_hidden));
        json!({"pending": p, "commits": c, "slept": slept, "justHidden": just_hidden, "justMs": JUST_MS, "now": now})
    })
}

/// Who sent a request, for tidy-log.jsonl: the phone app (a browser user agent) or anything else (curl, an agent,
/// a script), with the endpoint and user agent.
pub fn origin(h: &HeaderMap, endpoint: &str) -> Value {
    let ua = h.get("user-agent").and_then(|v| v.to_str().ok()).map(|s| s.chars().take(200).collect::<String>());
    let source = if ua.as_deref().is_some_and(|u| u.contains("Mozilla")) { "phone / browser (sheprd app)" } else { "api (curl / agent / script)" };
    json!({"source": source, "endpoint": endpoint, "ua": ua})
}

/// Appends one line to `<state>/tidy-log.jsonl` (every Tidy sleep, undo, deferred close, fix, wake / resume, pane
/// close, with who triggered it). Rotates to `.1` past 4 MB.
pub fn log_event(ev: &str, mut v: Value) {
    use std::io::Write;
    v["ev"] = json!(ev);
    v["t"] = json!(now_ms());
    let path = crate::state_dir().join("tidy-log.jsonl");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 4 << 20) {
        let _ = std::fs::rename(&path, path.with_extension("jsonl.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{v}");
    }
}

/// How long the phone lists agents under "Just put to sleep" (one-tap Wake), unless the user hides the group.
const JUST_MS: i64 = 6 * 3_600_000;

/// Hide the "Just put to sleep" group (the agents stay in Asleep).
async fn api_just_hide(h: HeaderMap) -> Response {
    let at = now_ms();
    log_event("just.hide", json!({"src": origin(&h, "POST /api/tidy/just/hide")}));
    with(|s| s.just_hidden = at);
    save();
    invalidate();
    Json(json!({"ok": true, "justHidden": at})).into_response()
}

async fn api_pending(State(app): State<Shared>) -> Response {
    Json(awake_again(&app, pending_json())).into_response()
}

// ---------------------------------------------------------------------------
// Background: commitments every 2 s; question labels for idle agents while the phone is in use

pub async fn run(app: Shared) {
    let mut tick = 0u64;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        tick += 1;
        if rt(|r| !r.commits.is_empty()) {
            watch_commits(&app).await;
        }
        if tick % 10 != 0 {
            continue;
        }
        let viewed = now_ms() - app.inner.lock().unwrap().last_visible < 15 * 60_000;
        if !viewed {
            continue;
        }
        // warm the question cache: idle agents whose last message has no verdict yet, 2 at a time (every 20 s). Under
        // the idle threshold (catch-up's "N waiting on you" on Home) only once the turn settled (1 min) and only when the
        // heuristic sees a question, so the daily cap goes to messages that may wait on the user.
        let threshold = idle_ms();
        let now = now_ms();
        let agents = live(&app);
        let mut n = 0;
        for a in agents.iter().filter(|a| a["kind"] == "claude" && a["last"]["role"] == "assistant" && guard(a).is_none()) {
            let idle = now - a["last"]["ts"].as_i64().unwrap_or(now);
            if idle < 60_000 || !matches!(a["status"].as_str(), Some("idle") | Some("done")) {
                continue;
            }
            let sid = s(a, "session");
            let key = q_key(a);
            if sid.is_empty() || with(|s| s.q.get(&sid).is_some_and(|q| q.key == key)) {
                continue;
            }
            let (app2, sid2, cwd) = (app.clone(), sid.clone(), s(a, "cwd"));
            let sc = tokio::task::spawn_blocking(move || scan_for(&app2, &sid2, &cwd)).await.unwrap_or_default();
            if let Some(t) = sc.asst.last().filter(|t| idle >= threshold || asks_guess(t)) {
                q_later(&app, sid, key, t.clone());
                n += 1;
            }
            if n >= 2 {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(pane: &str, status: &str) -> Value {
        json!({"pane": pane, "session": format!("s-{pane}"), "kind": "claude", "status": status, "title": "x", "cwd": "/home/user/Work",
               "last": {"role": "assistant", "text": "done", "ts": 1}, "since": 1, "queued": [], "pinned": false, "service": null})
    }

    #[test]
    fn never_sleep_guard() {
        let never = vec!["w6:pB".to_string()];
        let mut a = agent("w6:pE", "idle");
        a["home"] = json!(true);
        assert_eq!(never_why(&a, &never, &[]), Some("The home agent never sleeps"));
        let mut a = agent("w6:p9", "idle");
        a["name"] = json!("master-5e9");
        assert_eq!(never_why(&a, &never, &[]), Some("The home agent never sleeps"));
        let mut m = agent("wX:p3", "idle");
        m["service"] = json!({"name": "Nightly backup"});
        assert_eq!(never_why(&m, &never, &[]), Some("Background services never sleep"));
        let mut c = agent("wX:p5", "idle");
        c["title"] = json!("cron-usage-report");
        assert_eq!(never_why(&c, &never, &[]), None);
        assert!(never_why(&agent("w6:pB", "idle"), &never, &[]).is_some()); // a SHEPRD_NEVER_SLEEP pane
        assert!(never_why(&agent("w9:p1", "idle"), &[], &["s-w9:p1".into()]).is_some()); // its session, renumbered
        assert_eq!(never_why(&agent("w6:p2B", "done"), &never, &[]), None);
    }

    #[test]
    fn classification_order() {
        let th = 3_600_000;
        let f = Facts { idle: 2 * th, ..Default::default() };
        assert_eq!(classify(&agent("a", "done"), &f, th).0, "safe");
        assert_eq!(classify(&agent("a", "working"), &f, th).0, "keep");
        assert_eq!(classify(&agent("a", "done"), &Facts { idle: 60_000, ..f.clone() }, th).0, "keep"); // active recently
        let mut p = agent("a", "done");
        p["pinned"] = json!(true);
        assert_eq!(classify(&p, &f, th).0, "keep");
        assert_eq!(classify(&agent("a", "done"), &Facts { never: Some("The home agent never sleeps"), q: Some((None, None)), ..f.clone() }, th).1[0]["kind"], "lock");
        assert_eq!(classify(&agent("a", "done"), &Facts { held: 1, ..f.clone() }, th).0, "keep");
        let (b, r) = classify(&agent("a", "done"), &Facts { q: Some((Some("Waiting on your measurements".into()), Some("12 cm".into()))), ..f.clone() }, th);
        assert_eq!((b, r[0]["kind"].as_str(), r[0]["fix"]["kind"].as_str()), ("check", Some("q"), Some("answer")));
        let (b, r) = classify(&agent("a", "done"), &Facts { draft: Some("hi".into()), ..f.clone() }, th);
        assert_eq!((b, r[0]["fix"]["kind"].as_str()), ("check", Some("send")));
        let (b, r) = classify(&agent("a", "done"), &Facts { unpushed: 2, ..f.clone() }, th);
        assert_eq!((b, r[0]["text"].as_str(), r[0]["fix"]["text"].as_str()), ("check", Some("2 commits not pushed"), Some("push it")));
        let (b, r) = classify(&agent("a", "done"), &Facts { servers: vec![(8790, "Server".into(), false, Some(4 * 86_400_000))], ..f.clone() }, th);
        assert_eq!((b, r[0]["text"].as_str(), r[0]["fix"]["kind"].as_str()), ("check", Some("Server on :8790 · up 4d"), Some("stop")));
        let (b, r) = classify(&agent("a", "done"), &Facts { servers: vec![(5173, "vite".into(), true, None)], shells: 1, ..f.clone() }, th);
        assert_eq!((b, r.len()), ("check", 1)); // the shell is that server
        let (b, r) = classify(&agent("a", "done"), &Facts { ok: vec!["Grafana stays up on :3417".into()], ..f.clone() }, th);
        assert_eq!((b, r[0]["kind"].as_str()), ("safe", Some("ok")));
    }

    #[test]
    fn questions_and_ports() {
        assert!(asks_guess("Is \"got nothing from him\" about earlier messages, or this one?"));
        assert!(asks_guess("Send me the benchmark numbers and I'll rerun it."));
        assert!(asks_guess("Tell me when you've watched the video and I'll trim it."));
        assert!(!asks_guess("The dashboard is finished and nothing is left to do. Let me know if you want more."));
        assert!(!asks_guess("Deployed: see https://x.dev/?q=1 for the page."));
        assert_eq!(parse_q("{\"asks\": true, \"chip\": \"Waiting on your measurements.\", \"reply\": \"\"}"), Some((true, Some("Waiting on your measurements".into()), None, None)));
        assert_eq!(
            parse_q("{\"asks\": true, \"chip\": \"Wants your pick\", \"for\": \"Waiting on you for: your pick between the two layouts.\", \"reply\": \"B\"}").unwrap().3.as_deref(),
            Some("your pick between the two layouts")
        );
        assert_eq!(parse_q("nope"), None);
        assert_eq!(ports_in("it's up at **http://100.64.0.7:3417** and on localhost:8790; also port 5173, not 12:30 or 80"), vec![3417, 8790, 5173]);
    }

    #[test]
    fn you_said() {
        assert!(system_note("[Decision: Home layout] The user picked: B [b]"));
        assert!(system_note("[Build report — 1 new failure]\n• x"));
        assert!(system_note("continue") && system_note("Continue.") && system_note("/rename x"));
        assert!(!system_note("[Image #1] what's wrong here?"));
        assert!(!system_note("continue with the enclosure, but measure twice"));
        let dir = std::env::temp_dir().join(format!("mm-said-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let u = |ts: &str, t: &str| json!({"type": "user", "timestamp": ts, "message": {"role": "user", "content": t}});
        let lines = [
            u("2026-10-10T09:00:00.000Z", "Fix the enclosure fit and tell me what you need from me"),
            json!({"type": "assistant", "timestamp": "2026-10-10T09:01:00.000Z", "message": {"id": "m1", "content": [{"type": "text", "text": "Send me the numbers?"}]}}),
            u("2026-10-10T09:02:00.000Z", "continue"),
            u("2026-10-10T09:03:00.000Z", "[Decision: Fit] The user picked: A [a]"),
            u("2026-10-10T09:04:00.000Z", "[Message from agent \"Nightly Build\" (pane w6:p3) via sheprd]\nThe build is green, rerun the benchmarks?"),
            u("2026-10-10T09:05:00.000Z", "<command-name>/clear</command-name>"),
        ];
        std::fs::write(&p, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
        let mut sc = Scan::default();
        scan_more(&mut sc, &p, "/home");
        let v = said_json(&sc);
        assert_eq!(v["text"], "Fix the enclosure fit and tell me what you need from me");
        assert_eq!(v["ts"], crate::transcript::parse_ts("2026-10-10T09:00:00.000Z"));
        assert_eq!(v["then"]["from"], "Nightly Build");
        assert_eq!(v["then"]["text"], "The build is green, rerun the benchmarks?");
        assert_eq!(sc.asst_ts, crate::transcript::parse_ts("2026-10-10T09:01:00.000Z"));
        // the user writes again: the steer note goes
        let more = u("2026-10-10T09:06:00.000Z", "Width is 112 mm");
        std::fs::OpenOptions::new().append(true).open(&p).map(|mut f| std::io::Write::write_all(&mut f, (more.to_string() + "\n").as_bytes())).unwrap().unwrap();
        scan_more(&mut sc, &p, "/home");
        assert_eq!(said_json(&sc)["text"], "Width is 112 mm");
        assert!(said_json(&sc)["then"].is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notes_and_transcripts() {
        let n = parse_note("Here:\nDone: built it\nOpen: none\nNext: deploy\n- Files: a.rs\nPorts: :8790").unwrap();
        assert_eq!(n.lines().count(), 5);
        assert!(n.starts_with("Done: built it") && n.contains("Files: a.rs"));
        assert!(parse_note("Done: x\nOpen: y").is_none());
        let dir = std::env::temp_dir().join(format!("mm-tidy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let lines = [
            json!({"type": "user", "message": {"role": "user", "content": "build the thing"}}),
            json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "tool_use", "id": "t1", "name": "Write", "input": {"file_path": "/x/a.rs"}}]}}),
            json!({"type": "assistant", "message": {"id": "m2", "content": [{"type": "tool_use", "id": "t2", "name": "Bash", "input": {"command": "cd /x && git commit -m hi"}}]}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t2", "content": "[main 1a2b3c4] hi\n 1 file changed"}]}}),
            json!({"type": "assistant", "message": {"id": "m3", "content": [{"type": "text", "text": "Done. Up on :8790."}]}}),
            json!({"type": "assistant", "message": {"id": "m3", "content": [{"type": "text", "text": "Want me to push?"}]}}),
        ];
        std::fs::write(&p, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
        let mut sc = Scan::default();
        scan_more(&mut sc, &p, "/home");
        assert_eq!(sc.files, vec!["/x/a.rs"]);
        assert_eq!(sc.commits, vec![("/x".to_string(), "1a2b3c4".to_string())]);
        assert_eq!(sc.asst.last().unwrap(), "Done. Up on :8790.\n\nWant me to push?");
        assert_eq!(sc.first, "build the thing");
        // incremental: only new lines
        let more = json!({"type": "assistant", "message": {"id": "m4", "content": [{"type": "text", "text": "Pushed."}]}});
        std::fs::OpenOptions::new().append(true).open(&p).map(|mut f| std::io::Write::write_all(&mut f, (more.to_string() + "\n").as_bytes())).unwrap().unwrap();
        scan_more(&mut sc, &p, "/home");
        assert_eq!(sc.asst.last().unwrap(), "Pushed.");
        assert_eq!(sc.files.len(), 1);
        assert_eq!(repo_of_cmd("git -C ~/Work/x commit -m y", "/a"), crate::home().join("Work/x").to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
