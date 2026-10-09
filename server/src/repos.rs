//! Repos tab (`web/repos.js`): the git repos being worked on now, with GitHub / Vercel links and Deploy to Vercel.
//! API.md "Repos".
//!
//! - `GET /api/repos[?refresh=1]` — every repo a live agent's cwd is in (worktrees grouped under their main repo) plus
//!   repos under ~/Work (`SHEPRD_REPOS_ROOT`) with commits or uncommitted edits in the last 14 days: branches and
//!   worktrees, dirty count, ahead/behind upstream, last commits, agents, GitHub links (PRs via one cached
//!   `gh pr list` per repo), Vercel info and the repo's deploys.
//! - `POST /api/repos/deploy {repo, branch?, target: "preview"|"production", confirm?}` — one deploy per repo at a time.
//! - `GET /api/repos/deploy/{id}?after=N` — the deploy's state and log lines from N (the phone polls it).
//!
//! Read-only on repos: git.rs helpers (`--no-optional-locks`, 4 s timeouts), never a fetch. Built only when the phone
//! asks (20 s cache; the ~/Work scan and cwd → repo lookups 5 min; PRs 5 min and Vercel project info 30 min, both
//! refreshed in the background). A deploy exports the branch's committed tree with `git archive` into
//! ~/.cache/sheprd/deploy/<id>/<repo name> (uncommitted edits never ship), copies `.vercel/project.json` and runs
//! `vercel deploy` there in its own systemd scope (outside the service's 200 MB cap). The token comes from
//! `~/.config/sheprd/vercel-token` (mode 600) or `VERCEL_TOKEN`, goes to the CLI only through its environment,
//! and is never logged, returned or put in argv; log lines are scrubbed of it too.

use crate::git::{GhAuth, gh, gh_auth, git_ok};
use crate::transcript::{cut, now_ms};
use crate::{Shared, json_err};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const LIST_MS: i64 = 20_000;
const SCAN_MS: i64 = 300_000;
const CWD_MS: i64 = 300_000;
const RECENT_MS: i64 = 14 * 86_400_000;
const PR_MS: i64 = 300_000;
const VAPI_MS: i64 = 1_800_000;
const DEPLOY_MS: u64 = 15 * 60_000;
const EXPORT_MS: u64 = 180_000;
const LOG_LINES: usize = 1500;
const LINE_MAX: usize = 400;
const COMMITS: usize = 8;
const MAX_REPOS: usize = 30;
const MAX_BRANCHES: usize = 30;
const JOBS_KEPT: usize = 12;
const HIST_KEPT: usize = 60;
/// The Vercel CLI when none is installed: `npx --yes vercel@<major>` (npm's cache, nothing global).
const NPX_VERCEL: &str = "vercel@62";

struct Job {
    id: String,
    root: String,
    name: String,
    branch: String,
    sha: String,
    target: &'static str,
    state: &'static str,
    url: Option<String>,
    /// log lines; `first` = index of lines[0] (older lines dropped past LOG_LINES)
    lines: Vec<String>,
    first: usize,
    started: i64,
    ended: Option<i64>,
    error: Option<String>,
    dirty: u64,
}

impl Job {
    fn brief(&self) -> Value {
        json!({"id": self.id, "repo": self.root, "name": self.name, "branch": self.branch, "sha": self.sha,
            "target": self.target, "state": self.state, "url": self.url, "started": self.started, "ended": self.ended,
            "error": self.error, "dirty": self.dirty})
    }
    fn push(&mut self, line: String) {
        self.lines.push(line);
        if self.lines.len() > LOG_LINES {
            let n = self.lines.len() - LOG_LINES;
            self.lines.drain(..n);
            self.first += n;
        }
    }
}

#[derive(Default)]
struct St {
    /// the expensive part of the list (no deploy fields), with when it was built
    list: Option<(i64, Value)>,
    /// main roots under the scan root with recent activity
    scan: Option<(i64, Vec<String>)>,
    /// cwd -> (checked at, (toplevel, main root))
    cwds: HashMap<String, (i64, Option<(String, String)>)>,
    /// main root -> (fetched at, PRs newest first)
    prs: HashMap<String, (i64, Vec<Value>)>,
    prs_busy: HashSet<String>,
    /// Vercel projectId -> (fetched at, {dashboard, production, name})
    vapi: HashMap<String, (i64, Value)>,
    vapi_busy: HashSet<String>,
    jobs: VecDeque<Job>,
    /// main root -> running job id
    running: HashMap<String, String>,
    /// persisted (`<state>/repos.json`): {deploys: [...], links: {root: {projectId, orgId}}}
    hist: Option<Value>,
}

fn st() -> &'static Mutex<St> {
    static S: OnceLock<Mutex<St>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(St::default()))
}

/// One list build at a time (a second caller waits and gets the fresh cache).
fn build_lock() -> &'static tokio::sync::Mutex<()> {
    static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn hist_path() -> PathBuf {
    crate::state_dir().join("repos.json")
}

fn hist(s: &mut St) -> &mut Value {
    s.hist.get_or_insert_with(|| {
        std::fs::read(hist_path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({"deploys": [], "links": {}}))
    })
}

fn save_hist(v: &Value) {
    let p = hist_path();
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/repos", get(api_repos))
        .route("/api/repos/deploy", post(api_deploy))
        .route("/api/repos/deploy/{id}", get(api_job))
}

// ---------------------------------------------------------------------------
// paths, token, CLI

fn scan_root() -> PathBuf {
    std::env::var_os("SHEPRD_REPOS_ROOT").filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(crate::work_dir)
}

fn token_path() -> PathBuf {
    std::env::var_os("SHEPRD_VERCEL_TOKEN_FILE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".config/sheprd/vercel-token"))
}

fn tilde(p: &Path) -> String {
    let h = crate::home();
    match p.strip_prefix(&h) {
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    }
}

/// The one-line command that creates the token file (reads the token without echo, so it stays out of history).
fn setup_cmd() -> String {
    let p = tilde(&token_path());
    let dir = Path::new(&p).parent().map(|d| d.display().to_string()).unwrap_or_else(|| ".".into());
    format!("(umask 077; mkdir -p {dir} && read -rsp 'Vercel token: ' t && printf '%s\\n' \"$t\" > {p})")
}

/// Ok(Some(token)) from the token file (mode 600) or `VERCEL_TOKEN`; Ok(None) when there is none; Err when the
/// file is readable by others.
fn read_token() -> Result<Option<String>, String> {
    let p = token_path();
    if let Ok(meta) = std::fs::metadata(&p) {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(format!("{} is readable by others: chmod 600 it", tilde(&p)));
        }
        if let Ok(t) = std::fs::read_to_string(&p) {
            let t = t.trim().to_string();
            if !t.is_empty() {
                return Ok(Some(t));
            }
        }
    }
    Ok(std::env::var("VERCEL_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()))
}

fn which(bin: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// (program, leading args, label): `SHEPRD_VERCEL`, else `vercel` on PATH, else `npx --yes vercel@62`.
fn cli() -> Option<(String, Vec<String>, String)> {
    if let Some(p) = std::env::var("SHEPRD_VERCEL").ok().filter(|s| !s.is_empty()) {
        return Some((p.clone(), vec![], p));
    }
    if let Some(p) = which("vercel") {
        return Some((p.display().to_string(), vec![], "vercel".into()));
    }
    let npx = which("npx")?;
    Some((npx.display().to_string(), vec!["--yes".into(), NPX_VERCEL.into()], format!("npx {NPX_VERCEL}")))
}

/// Can we start a transient systemd scope (so the CLI's memory isn't charged to this service)? Checked once.
async fn scope_ok() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    if let Some(v) = OK.get() {
        return *v;
    }
    let mut c = tokio::process::Command::new("systemd-run");
    c.args(["--user", "--scope", "--quiet", "--collect", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let ok = matches!(tokio::time::timeout(Duration::from_secs(5), c.status()).await, Ok(Ok(s)) if s.success());
    let _ = OK.set(ok);
    ok
}

// ---------------------------------------------------------------------------
// pure helpers (unit-tested)

/// `owner/repo` of a GitHub remote: scp-style (`git@github.com:o/r.git`, ssh host aliases like
/// `github.com-work:o/r`), `ssh://`, `https://` (credentials dropped) and `git://`.
fn github_slug(remote: &str) -> Option<String> {
    let u = remote.trim();
    let (host, path) = if let Some((scheme, rest)) = u.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git") {
            return None;
        }
        let (auth, path) = rest.split_once('/')?;
        let host = auth.rsplit('@').next()?;
        (host.split(':').next()?.to_string(), path.to_string())
    } else {
        let (auth, path) = u.split_once(':')?;
        if auth.contains('/') {
            return None; // a local path
        }
        (auth.rsplit('@').next()?.to_string(), path.to_string())
    };
    let host = host.to_ascii_lowercase();
    if !(host == "github.com" || host == "www.github.com" || host == "ssh.github.com" || host.starts_with("github.com-")) {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut it = path.split('/');
    let (o, r) = (it.next()?, it.next()?);
    if it.next().is_some() || o.is_empty() || r.is_empty() {
        return None;
    }
    let ok = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    (ok(o) && ok(r)).then(|| format!("{o}/{r}"))
}

/// Minimal percent-encoding for a branch in a URL path (keeps `/`).
fn enc_path(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `%(upstream:track)` → (ahead, behind, gone): "[ahead 1, behind 2]", "[behind 3]", "[gone]", "".
fn parse_track(s: &str) -> (u64, u64, bool) {
    let s = s.trim().trim_start_matches('[').trim_end_matches(']');
    if s == "gone" {
        return (0, 0, true);
    }
    let (mut a, mut b) = (0, 0);
    for part in s.split(',') {
        let mut w = part.split_whitespace();
        match (w.next(), w.next().and_then(|n| n.parse::<u64>().ok())) {
            (Some("ahead"), Some(n)) => a = n,
            (Some("behind"), Some(n)) => b = n,
            _ => {}
        }
    }
    (a, b, false)
}

const BRANCH_FMT: &str =
    "%(HEAD)%1f%(refname:short)%1f%(committerdate:unix)%1f%(upstream:short)%1f%(upstream:track)%1f%(objectname:short)%1f%(worktreepath)%1f%(contents:subject)";

/// `git for-each-ref --format=BRANCH_FMT refs/heads` → branches, newest commit first.
fn parse_branches(text: &str) -> Vec<Value> {
    let mut v: Vec<Value> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\x1f').collect();
            if f.len() < 8 || f[1].is_empty() {
                return None;
            }
            let up = Some(f[3]).filter(|s| !s.is_empty());
            let (ahead, behind, gone) = parse_track(f[4]);
            Some(json!({"name": f[1], "current": f[0] == "*", "ts": f[2].trim().parse::<i64>().unwrap_or(0) * 1000,
                "upstream": up, "ahead": if up.is_some() && !gone { json!(ahead) } else { Value::Null },
                "behind": if up.is_some() && !gone { json!(behind) } else { Value::Null }, "gone": gone,
                "short": f[5], "wt": Some(f[6]).filter(|s| !s.is_empty()), "subject": f[7]}))
        })
        .collect();
    v.sort_by_key(|b| std::cmp::Reverse(b["ts"].as_i64().unwrap_or(0)));
    v
}

/// `git worktree list --porcelain` → [{path, head, branch, detached, bare}] (main worktree first).
fn parse_worktrees(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut w = json!({"path": null, "head": null, "branch": null, "detached": false, "bare": false, "prunable": false});
        for l in block.lines() {
            let (k, v) = l.split_once(' ').unwrap_or((l, ""));
            match k {
                "worktree" => w["path"] = json!(v),
                "HEAD" => w["head"] = json!(v.get(..7).unwrap_or(v)),
                "branch" => w["branch"] = json!(v.strip_prefix("refs/heads/").unwrap_or(v)),
                "detached" => w["detached"] = json!(true),
                "bare" => w["bare"] = json!(true),
                "prunable" => w["prunable"] = json!(true),
                _ => {}
            }
        }
        if w["path"].is_string() {
            out.push(w);
        }
    }
    out
}

/// `git log --format=%h%x1f%s%x1f%an%x1f%ct` → [{short, subject, author, ts}].
fn parse_log(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\x1f').collect();
            (f.len() >= 4).then(|| {
                json!({"short": f[0], "subject": f[1], "author": f[2], "ts": f[3].trim().parse::<i64>().unwrap_or(0) * 1000})
            })
        })
        .collect()
}

/// Vercel config in a checkout: `.vercel/project.json` (linked), `.vercel/repo.json` (monorepo link), `vercel.json`.
fn vercel_local(root: &Path) -> Value {
    let read = |p: &str| std::fs::read(root.join(p)).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let project = read(".vercel/project.json");
    let repo = read(".vercel/repo.json");
    let config = root.join("vercel.json").is_file();
    let (pid, oid, pname) = match (&project, &repo) {
        (Some(p), _) => (p["projectId"].clone(), p["orgId"].clone(), p["projectName"].clone()),
        (None, Some(r)) => {
            let first = &r["projects"][0];
            (first["id"].clone(), r["orgId"].clone().as_str().map(|s| json!(s)).unwrap_or(first["orgId"].clone()), first["name"].clone())
        }
        _ => (Value::Null, Value::Null, Value::Null),
    };
    json!({"linked": pid.is_string(), "projectId": pid, "orgId": oid, "projectName": pname, "config": config,
        "monorepo": repo.is_some() && project.is_none()})
}

/// The deployment URL: the last stdout line that is a bare https URL, else the last `*.vercel.app` URL anywhere.
fn deploy_url(lines: &[(bool, String)]) -> Option<String> {
    let bare = |l: &str| {
        let t = l.trim();
        (t.starts_with("https://") && !t.contains(char::is_whitespace)).then(|| t.to_string())
    };
    if let Some(u) = lines.iter().rev().filter(|(out, _)| *out).find_map(|(_, l)| bare(l)) {
        return Some(u);
    }
    let re = regex_lite::Regex::new(r"https://[A-Za-z0-9.-]+\.vercel\.app[^\s\])>'\x22]*").ok()?;
    lines.iter().rev().find_map(|(_, l)| re.find_iter(l).last().map(|m| m.as_str().to_string()))
}

/// Drops ANSI escapes and the token; cuts long lines.
fn clean_line(l: &str, token: &str) -> String {
    let re = ansi_re();
    let mut s = re.replace_all(l, "").replace('\r', "");
    if !token.is_empty() && s.contains(token) {
        s = s.replace(token, "***");
    }
    cut(s.trim_end(), LINE_MAX).0
}

fn ansi_re() -> &'static regex_lite::Regex {
    static R: OnceLock<regex_lite::Regex> = OnceLock::new();
    R.get_or_init(|| regex_lite::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07").unwrap())
}

/// A local branch name the phone may pass on (it must also be in the repo's branch list).
fn branch_ok(b: &str) -> bool {
    !b.is_empty() && !b.starts_with('-') && !b.contains("..") && b.chars().all(|c| c.is_ascii_alphanumeric() || "-_./+@".contains(c))
}

/// Vercel API project → {name, dashboard, production}.
fn vapi_info(project: &Value, slug: Option<&str>) -> Value {
    let name = project["name"].as_str();
    let prod = project["targets"]["production"]["alias"][0]
        .as_str()
        .or_else(|| project["alias"][0]["domain"].as_str())
        .map(|d| if d.starts_with("http") { d.to_string() } else { format!("https://{d}") });
    let dashboard = match (slug, name) {
        (Some(s), Some(n)) => Some(format!("https://vercel.com/{s}/{n}")),
        _ => None,
    };
    json!({"name": name, "dashboard": dashboard, "production": prod})
}

// ---------------------------------------------------------------------------
// discovery

/// cwd → (toplevel, main root); cached 5 min. The main root groups worktrees under their repo.
async fn resolve(cwd: &str) -> Option<(String, String)> {
    let now = now_ms();
    if let Some((t, v)) = st().lock().unwrap().cwds.get(cwd) {
        if now - t < CWD_MS {
            return v.clone();
        }
    }
    let v = git_ok(cwd, &["rev-parse", "--path-format=absolute", "--show-toplevel", "--git-common-dir"]).await.and_then(|o| {
        let mut l = o.lines();
        let top = l.next()?.trim().to_string();
        let common = l.next()?.trim().to_string();
        let main = common.strip_suffix("/.git").map(str::to_string).unwrap_or_else(|| top.clone());
        Some((top, main))
    });
    let mut s = st().lock().unwrap();
    if s.cwds.len() > 500 {
        s.cwds.clear();
    }
    s.cwds.insert(cwd.to_string(), (now, v.clone()));
    v
}

/// Latest activity of a repo: its newest local commit, else the newest mtime of an uncommitted file (≤ 200 looked at).
async fn activity(root: &str) -> i64 {
    let last = git_ok(root, &["for-each-ref", "--sort=-committerdate", "--count=1", "--format=%(committerdate:unix)", "refs/heads"])
        .await
        .and_then(|s| s.trim().parse::<i64>().ok())
        .map(|t| t * 1000)
        .unwrap_or(0);
    if now_ms() - last < RECENT_MS {
        return last;
    }
    let st_ = git_ok(root, &["status", "--porcelain=v1", "--untracked-files=normal"]).await.unwrap_or_default();
    let root = PathBuf::from(root);
    let paths: Vec<PathBuf> = st_
        .lines()
        .filter(|l| l.len() > 3)
        .take(200)
        .map(|l| root.join(l[3..].rsplit(" -> ").next().unwrap_or(&l[3..]).trim_matches('"')))
        .collect();
    let dirty = tokio::task::spawn_blocking(move || {
        paths
            .iter()
            .filter_map(|p| std::fs::symlink_metadata(p).ok()?.modified().ok())
            .filter_map(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .max()
            .unwrap_or(0)
    })
    .await
    .unwrap_or(0);
    last.max(dirty)
}

/// Main roots of repos under the scan root (depth ≤ 2, no hidden dirs) with activity in the last 14 days; 5 min cache.
async fn scan(force: bool) -> Vec<String> {
    if !force {
        if let Some((t, v)) = &st().lock().unwrap().scan {
            if now_ms() - t < SCAN_MS {
                return v.clone();
            }
        }
    }
    let root = scan_root();
    let dirs = tokio::task::spawn_blocking(move || {
        let mut found = Vec::new();
        let kids = |d: &Path| -> Vec<PathBuf> {
            let Ok(rd) = std::fs::read_dir(d) else { return vec![] };
            let mut v: Vec<PathBuf> = rd
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with('.') && n != "node_modules"))
                .collect();
            v.sort();
            v
        };
        for d in kids(&root) {
            if d.join(".git").exists() {
                found.push(d);
            } else {
                found.extend(kids(&d).into_iter().filter(|k| k.join(".git").exists()));
            }
        }
        found
    })
    .await
    .unwrap_or_default();
    let mut mains: Vec<String> = Vec::new();
    for d in dirs {
        if let Some((_, main)) = resolve(&d.to_string_lossy()).await {
            if !mains.contains(&main) {
                mains.push(main);
            }
        }
    }
    let acts = futures_util::future::join_all(mains.iter().map(|m| activity(m))).await;
    let cutoff = now_ms() - RECENT_MS;
    let mut recent: Vec<(i64, String)> = mains.into_iter().zip(acts).filter(|(_, a)| *a >= cutoff).map(|(m, a)| (a, m)).collect();
    recent.sort_by(|a, b| b.0.cmp(&a.0));
    let v: Vec<String> = recent.into_iter().map(|(_, m)| m).collect();
    st().lock().unwrap().scan = Some((now_ms(), v.clone()));
    v
}

/// Live agents from the last published state: {pane, title, color, home, status, cwd}.
fn live_agents(app: &crate::App) -> Vec<Value> {
    let s: Value = serde_json::from_str(&app.inner.lock().unwrap().state).unwrap_or(Value::Null);
    s["agents"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter(|a| a["cwd"].is_string())
                .map(|a| {
                    json!({"pane": a["pane"], "title": a["title"], "color": a["color"], "home": a["home"] == true,
                        "status": a["status"], "cwd": a["cwd"]})
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One repo's details (no PR / Vercel API / deploy fields yet).
async fn detail(main: &str, agents: Vec<(String, Value)>) -> Option<Value> {
    let wts_txt = git_ok(main, &["worktree", "list", "--porcelain"]).await.unwrap_or_default();
    let mut wts = parse_worktrees(&wts_txt);
    wts.retain(|w| w["bare"] != true && w["prunable"] != true);
    if wts.is_empty() {
        wts.push(json!({"path": main, "head": null, "branch": null, "detached": false}));
    }
    let br_txt = git_ok(main, &["for-each-ref", &format!("--format={BRANCH_FMT}"), "refs/heads"]).await.unwrap_or_default();
    let mut branches = parse_branches(&br_txt);
    let more = branches.len().saturating_sub(MAX_BRANCHES);
    // keep the ones checked out in a worktree even past the cap
    let mut kept = 0;
    branches.retain(|b| {
        kept += 1;
        kept <= MAX_BRANCHES || b["wt"].is_string()
    });
    let dirty = futures_util::future::join_all(wts.iter().map(|w| {
        let p = w["path"].as_str().unwrap_or(main).to_string();
        async move {
            git_ok(&p, &["status", "--porcelain=v1", "--untracked-files=normal"]).await.map(|s| s.lines().filter(|l| !l.is_empty()).count())
        }
    }))
    .await;
    for (w, d) in wts.iter_mut().zip(dirty) {
        w["dirty"] = json!(d);
        let p = w["path"].as_str().unwrap_or("").to_string();
        let mine: Vec<&Value> = agents.iter().filter(|(top, _)| *top == p).map(|(_, a)| a).collect();
        w["agents"] = json!(mine.iter().map(|a| a["pane"].clone()).collect::<Vec<_>>());
        w["main"] = json!(p == main);
        if let Some(b) = branches.iter().find(|b| b["name"] == w["branch"]) {
            w["ts"] = b["ts"].clone();
        }
    }
    let origin = git_ok(main, &["config", "--get", "remote.origin.url"]).await.map(|s| s.trim().to_string());
    let commits = parse_log(&git_ok(main, &["log", "-n", &COMMITS.to_string(), "--format=%h%x1f%s%x1f%an%x1f%ct", "HEAD"]).await.unwrap_or_default());
    let main_wt = &wts[0];
    let branch = main_wt["branch"].as_str().map(str::to_string);
    let cur = branch.as_ref().and_then(|b| branches.iter().find(|x| x["name"] == b.as_str()));
    let slug = origin.as_deref().and_then(github_slug);
    let name = Path::new(main).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| main.to_string());
    let ts = branches.iter().filter_map(|b| b["ts"].as_i64()).max().unwrap_or(0);
    let mut vercel = vercel_local(Path::new(main));
    vercel["dir"] = json!(main);
    let agents_v: Vec<Value> = agents
        .iter()
        .map(|(top, a)| {
            let mut a = a.clone();
            a["worktree"] = json!(top);
            a.as_object_mut().map(|o| o.remove("cwd"));
            a
        })
        .collect();
    Some(json!({
        "id": main, "name": name, "root": main, "live": !agents.is_empty(), "ts": ts,
        "branch": branch, "detached": main_wt["detached"], "head": main_wt["head"],
        "dirty": main_wt["dirty"], "upstream": cur.map(|c| c["upstream"].clone()), "ahead": cur.map(|c| c["ahead"].clone()),
        "behind": cur.map(|c| c["behind"].clone()), "gone": cur.map(|c| c["gone"].clone()),
        "last": commits.first(), "commits": commits,
        "branches": branches, "branchesMore": more, "worktrees": wts, "agents": agents_v,
        "origin": origin.as_deref().map(scrub_remote),
        "github": slug.map(|s| json!({"repo": s, "url": format!("https://github.com/{s}")})),
        "vercel": vercel,
    }))
}

/// A remote URL without credentials (`https://user:tok@host/…` → `https://host/…`).
fn scrub_remote(u: &str) -> String {
    match u.split_once("://") {
        Some((s, rest)) => match rest.split_once('/') {
            Some((auth, path)) if auth.contains('@') => format!("{s}://{}/{path}", auth.rsplit('@').next().unwrap_or("")),
            _ => u.to_string(),
        },
        None => u.to_string(),
    }
}

/// Builds (or returns the cached) list of repos.
async fn build(app: &crate::App, force: bool) -> Value {
    if !force {
        if let Some((t, v)) = &st().lock().unwrap().list {
            if now_ms() - t < LIST_MS {
                return v.clone();
            }
        }
    }
    let _g = build_lock().lock().await;
    if let Some((t, v)) = &st().lock().unwrap().list {
        if now_ms() - t < if force { 2_000 } else { LIST_MS } {
            return v.clone();
        }
    }
    let agents = live_agents(app);
    // agent cwd → repo
    let mut by_main: Vec<(String, Vec<(String, Value)>)> = Vec::new();
    for a in agents {
        let Some((top, main)) = resolve(a["cwd"].as_str().unwrap_or("")).await else { continue };
        match by_main.iter_mut().find(|(m, _)| *m == main) {
            Some((_, l)) => l.push((top, a)),
            None => by_main.push((main, vec![(top, a)])),
        }
    }
    for m in scan(force).await {
        if !by_main.iter().any(|(x, _)| *x == m) {
            by_main.push((m, vec![]));
        }
    }
    by_main.truncate(MAX_REPOS);
    let mut repos: Vec<Value> =
        futures_util::future::join_all(by_main.into_iter().map(|(m, a)| async move { detail(&m, a).await })).await.into_iter().flatten().collect();
    // live repos first, then by latest commit
    repos.sort_by(|a, b| (b["live"] == true).cmp(&(a["live"] == true)).then(b["ts"].as_i64().cmp(&a["ts"].as_i64())));
    let v = json!({"repos": repos, "root": tilde(&scan_root()), "built": now_ms()});
    st().lock().unwrap().list = Some((now_ms(), v.clone()));
    v
}

// ---------------------------------------------------------------------------
// PRs (gh) and Vercel project info, refreshed in the background

fn refresh_prs(roots: Vec<String>) {
    let now = now_ms();
    let todo: Vec<String> = {
        let mut s = st().lock().unwrap();
        let t: Vec<String> = roots
            .into_iter()
            .filter(|r| !s.prs_busy.contains(r) && s.prs.get(r).is_none_or(|(t, _)| now - t >= PR_MS))
            .collect();
        s.prs_busy.extend(t.iter().cloned());
        t
    };
    if todo.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for r in todo {
            let out = gh(&r, &["pr", "list", "--state", "all", "--limit", "40", "--json", "number,url,state,headRefName,title,isDraft"], 15_000).await;
            let mut s = st().lock().unwrap();
            s.prs_busy.remove(&r);
            match out {
                Ok((0, o, _)) => {
                    let l = serde_json::from_str::<Vec<Value>>(&o).unwrap_or_default();
                    s.prs.insert(r, (now_ms(), l));
                }
                // keep the old list; try again in a minute
                _ => {
                    let old = s.prs.get(&r).map(|(_, l)| l.clone()).unwrap_or_default();
                    s.prs.insert(r, (now_ms() - PR_MS + 60_000, old));
                }
            }
        }
    });
}

/// GET on the Vercel API with the token in a curl config on stdin (never argv).
async fn vercel_get(token: &str, path: &str) -> Option<Value> {
    use tokio::io::AsyncWriteExt;
    let base = std::env::var("SHEPRD_VERCEL_API").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://api.vercel.com".into());
    let mut c = tokio::process::Command::new("curl");
    c.args(["-sS", "--max-time", "10", "-K", "-", &format!("{base}{path}")])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut ch = c.spawn().ok()?;
    let mut si = ch.stdin.take()?;
    si.write_all(format!("header = \"Authorization: Bearer {token}\"\n").as_bytes()).await.ok()?;
    drop(si);
    let out = tokio::time::timeout(Duration::from_secs(12), ch.wait_with_output()).await.ok()?.ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("error").is_none().then_some(v)
}

fn refresh_vapi(projects: Vec<(String, String)>) {
    let Ok(Some(token)) = read_token() else { return };
    let now = now_ms();
    let todo: Vec<(String, String)> = {
        let mut s = st().lock().unwrap();
        let t: Vec<(String, String)> = projects
            .into_iter()
            .filter(|(p, _)| !s.vapi_busy.contains(p) && s.vapi.get(p).is_none_or(|(t, _)| now - t >= VAPI_MS))
            .collect();
        s.vapi_busy.extend(t.iter().map(|(p, _)| p.clone()));
        t
    };
    if todo.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for (pid, org) in todo {
            let team = org.starts_with("team_");
            let q = if team { format!("?teamId={org}") } else { String::new() };
            let proj = vercel_get(&token, &format!("/v9/projects/{pid}{q}")).await;
            let slug = if team {
                vercel_get(&token, &format!("/v2/teams/{org}")).await.and_then(|t| t["slug"].as_str().map(str::to_string))
            } else {
                vercel_get(&token, "/v2/user").await.and_then(|u| u["user"]["username"].as_str().map(str::to_string))
            };
            let mut s = st().lock().unwrap();
            s.vapi_busy.remove(&pid);
            let info = proj.map(|p| vapi_info(&p, slug.as_deref())).unwrap_or(Value::Null);
            s.vapi.insert(pid, (now_ms(), info));
        }
    });
}

// ---------------------------------------------------------------------------
// handlers

#[derive(Deserialize)]
struct ListQ {
    refresh: Option<String>,
}

async fn api_repos(State(app): State<Shared>, Query(q): Query<ListQ>) -> Response {
    let force = q.refresh.as_deref().is_some_and(|r| r == "1" || r == "true");
    let mut v = build(&app, force).await;
    let auth = gh_auth(false).await;
    let token = read_token();
    let now = now_ms();
    let mut pr_roots = Vec::new();
    let mut vprojects = Vec::new();
    let mut pending = false;
    {
        let mut s = st().lock().unwrap();
        let links = hist(&mut s)["links"].clone();
        let deploys = hist(&mut s)["deploys"].clone();
        for r in v["repos"].as_array_mut().into_iter().flatten() {
            let root = r["root"].as_str().unwrap_or("").to_string();
            // PRs per branch (gh, cached per repo)
            if auth == GhAuth::Ok && r["github"].is_object() {
                match s.prs.get(&root) {
                    Some((t, prs)) => {
                        if now - t >= PR_MS {
                            pr_roots.push(root.clone());
                        }
                        let of = |b: &str| prs.iter().find(|p| p["headRefName"] == b).map(|p| json!({"number": p["number"], "url": p["url"], "state": p["state"], "title": p["title"], "draft": p["isDraft"]}));
                        for b in r["branches"].as_array_mut().into_iter().flatten() {
                            if let Some(p) = b["name"].as_str().and_then(of) {
                                b["pr"] = p;
                            }
                        }
                        let open: Vec<Value> = r["branches"].as_array().into_iter().flatten().filter_map(|b| b.get("pr").cloned()).collect();
                        r["github"]["prs"] = json!(open);
                    }
                    None => {
                        pr_roots.push(root.clone());
                        pending = true;
                    }
                }
            }
            if let Some(slug) = r["github"]["repo"].as_str().map(str::to_string) {
                for b in r["branches"].as_array_mut().into_iter().flatten() {
                    let n = b["name"].as_str().unwrap_or("").to_string();
                    b["url"] = json!(format!("https://github.com/{slug}/tree/{}", enc_path(&n)));
                }
                if let Some(b) = r["branch"].as_str() {
                    r["github"]["branchUrl"] = json!(format!("https://github.com/{slug}/tree/{}", enc_path(b)));
                }
            }
            // Vercel: local link, else the one a deploy saved; API info when a token exists
            if r["vercel"]["linked"] != true {
                if let Some(l) = links.get(&root).filter(|l| l["projectId"].is_string()) {
                    r["vercel"]["linked"] = json!(true);
                    r["vercel"]["projectId"] = l["projectId"].clone();
                    r["vercel"]["orgId"] = l["orgId"].clone();
                    r["vercel"]["savedLink"] = json!(true);
                }
            }
            if let (Some(p), Some(o)) = (r["vercel"]["projectId"].as_str().map(str::to_string), r["vercel"]["orgId"].as_str().map(str::to_string)) {
                match s.vapi.get(&p) {
                    Some((_, info)) if info.is_object() => {
                        r["vercel"]["name"] = info["name"].clone();
                        r["vercel"]["dashboard"] = info["dashboard"].clone();
                        r["vercel"]["production"] = info["production"].clone();
                    }
                    _ => {}
                }
                vprojects.push((p, o));
            }
            if r["vercel"]["dashboard"].is_null() {
                r["vercel"]["dashboard"] = json!("https://vercel.com/dashboard");
            }
            // deploys: running / recent (newest first), the preview URLs we know
            let mine: Vec<Value> = deploys.as_array().into_iter().flatten().filter(|d| d["repo"] == root.as_str()).rev().take(5).cloned().collect();
            r["vercel"]["previews"] = json!(mine.iter().filter(|d| d["state"] == "ok" && d["target"] == "preview" && d["url"].is_string()).map(|d| json!({"url": d["url"], "branch": d["branch"], "ts": d["ended"]})).collect::<Vec<_>>());
            if r["vercel"]["production"].is_null() {
                if let Some(d) = mine.iter().find(|d| d["state"] == "ok" && d["target"] == "production") {
                    r["vercel"]["lastProduction"] = d["url"].clone();
                }
            }
            r["deploys"] = json!(mine);
            r["deploy"] = s.running.get(&root).and_then(|id| s.jobs.iter().find(|j| j.id == *id)).map(Job::brief).unwrap_or(Value::Null);
        }
    }
    refresh_prs(pr_roots);
    refresh_vapi(vprojects);
    let (has, terr) = match &token {
        Ok(t) => (t.is_some(), None),
        Err(e) => (false, Some(e.clone())),
    };
    v["now"] = json!(now);
    v["pending"] = json!(pending);
    v["gh"] = json!(auth.as_str());
    v["vercel"] = json!({"token": has, "tokenError": terr, "tokenPath": tilde(&token_path()), "setup": setup_cmd(),
        "cli": cli().map(|c| c.2)});
    Json(v).into_response()
}

#[derive(Deserialize)]
struct DeployReq {
    repo: String,
    branch: Option<String>,
    target: Option<String>,
    confirm: Option<String>,
}

async fn api_deploy(State(app): State<Shared>, Json(b): Json<DeployReq>) -> Response {
    let target: &'static str = match b.target.as_deref() {
        None | Some("preview") => "preview",
        Some("production") => "production",
        _ => return json_err(StatusCode::BAD_REQUEST, "target must be preview or production"),
    };
    if target == "production" && b.confirm.as_deref() != Some("production") {
        return json_err(StatusCode::BAD_REQUEST, "a production deploy needs confirm: \"production\"");
    }
    let token = match read_token() {
        Ok(Some(t)) => t,
        Ok(None) => return (StatusCode::PRECONDITION_FAILED, Json(json!({"error": "no Vercel token", "setup": setup_cmd()}))).into_response(),
        Err(e) => return (StatusCode::PRECONDITION_FAILED, Json(json!({"error": e, "setup": setup_cmd()}))).into_response(),
    };
    let list = build(&app, false).await;
    let Some(r) = list["repos"].as_array().and_then(|l| l.iter().find(|r| r["root"] == b.repo.as_str())).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "not a listed repo");
    };
    let root = b.repo.clone();
    let branch = match b.branch.as_deref().filter(|s| !s.is_empty()).or(r["branch"].as_str()) {
        Some(x) => x.to_string(),
        None => return json_err(StatusCode::BAD_REQUEST, "detached HEAD: pick a branch"),
    };
    // the list may be a few seconds old: a branch made since is checked against git below
    let known = r["branches"].as_array().is_some_and(|l| l.iter().any(|x| x["name"] == branch.as_str()));
    if !branch_ok(&branch) {
        return json_err(StatusCode::BAD_REQUEST, "bad branch name");
    }
    let Some(sha) = git_ok(&root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}^{{commit}}")]).await.map(|s| s.trim().to_string()) else {
        return json_err(StatusCode::BAD_REQUEST, if known { "branch has no commit" } else { "no such local branch" });
    };
    let Some(cli) = cli() else { return json_err(StatusCode::SERVICE_UNAVAILABLE, "no Vercel CLI: install `vercel` or node (npx)") };
    let dirty = r["worktrees"]
        .as_array()
        .and_then(|l| l.iter().find(|w| w["branch"] == branch.as_str()))
        .and_then(|w| w["dirty"].as_u64())
        .unwrap_or(0);
    let name = r["name"].as_str().unwrap_or("repo").to_string();
    let id = format!("d{:x}", now_ms());
    let link = {
        let mut s = st().lock().unwrap();
        if let Some(cur) = s.running.get(&root) {
            return (StatusCode::CONFLICT, Json(json!({"error": "a deploy of this repo is running", "id": cur}))).into_response();
        }
        s.running.insert(root.clone(), id.clone());
        let mut job = Job {
            id: id.clone(), root: root.clone(), name: name.clone(), branch: branch.clone(), sha: sha.clone(), target,
            state: "running", url: None, lines: vec![], first: 0, started: now_ms(), ended: None, error: None, dirty,
        };
        job.push(format!("Deploying {name}@{branch} ({}) to {target}", &sha[..sha.len().min(7)]));
        if dirty > 0 {
            job.push(format!("Note: {dirty} uncommitted change{} not included — only committed code ships", if dirty == 1 { " is" } else { "s are" }));
        }
        s.jobs.push_back(job);
        while s.jobs.len() > JOBS_KEPT {
            let Some(ix) = s.jobs.iter().position(|j| j.state != "running") else { break };
            s.jobs.remove(ix);
        }
        hist(&mut s)["links"].get(&root).cloned()
    };
    let local = r["vercel"]["linked"] == true && r["vercel"]["savedLink"] != true;
    tokio::spawn(run_deploy(app.clone(), id.clone(), root, name, branch, sha.clone(), target, token, cli, if local { None } else { link }));
    Json(json!({"ok": true, "id": id, "sha": sha, "dirty": dirty})).into_response()
}

fn log(id: &str, line: String) {
    let mut s = st().lock().unwrap();
    if let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) {
        j.push(line);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_deploy(
    app: Shared,
    id: String,
    root: String,
    name: String,
    branch: String,
    sha: String,
    target: &'static str,
    token: String,
    cli: (String, Vec<String>, String),
    link: Option<Value>,
) {
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    let base = std::env::var_os("SHEPRD_DEPLOY_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".cache/sheprd/deploy"))
        .join(&id);
    let dir = base.join(if safe.trim_matches('.').is_empty() { "repo".into() } else { safe });
    let res = deploy_in(&id, &root, &dir, &sha, target, &token, &cli, link.as_ref()).await;
    let _ = tokio::fs::remove_dir_all(&base).await;
    let (state, url, error) = match res {
        Ok(u) => ("ok", Some(u), None),
        Err(e) => ("failed", None, Some(e)),
    };
    let (brief, tail) = {
        let mut s = st().lock().unwrap();
        s.running.remove(&root);
        let Some(j) = s.jobs.iter_mut().find(|j| j.id == id) else { return };
        match (&url, &error) {
            (Some(u), _) => j.push(format!("Done: {u}")),
            (_, Some(e)) => j.push(format!("Failed: {e}")),
            _ => {}
        }
        j.state = state;
        j.url = url.clone();
        j.error = error.clone();
        j.ended = Some(now_ms());
        let brief = j.brief();
        let tail = j.lines.iter().rev().take(12).rev().cloned().collect::<Vec<_>>().join("\n");
        let h = hist(&mut s);
        if let Some(l) = h["deploys"].as_array_mut() {
            l.push(brief.clone());
            let n = l.len().saturating_sub(HIST_KEPT);
            l.drain(..n);
        }
        save_hist(h);
        (brief, tail)
    };
    let what = format!("{name}@{branch}");
    let surf = match &url {
        Some(u) => json!({"title": format!("Deployed {what} · {target}"), "body": u, "level": "done", "tool": false,
            "links": [{"url": u, "label": if target == "production" { "Open production" } else { "Open preview" }}],
            "pushUrl": format!("/#/repos?d={id}")}),
        None => json!({"title": format!("Deploy failed · {what}"), "level": "attention", "tool": false,
            "body": format!("{}\n\n{}", brief["error"].as_str().unwrap_or("failed"), tail),
            "pushUrl": format!("/#/repos?d={id}")}),
    };
    let _ = crate::api_surface(State(app.clone()), Json(surf)).await;
}

/// Export the committed tree, run the CLI, return the deployment URL.
#[allow(clippy::too_many_arguments)]
async fn deploy_in(
    id: &str,
    root: &str,
    dir: &Path,
    sha: &str,
    target: &str,
    token: &str,
    cli: &(String, Vec<String>, String),
    link: Option<&Value>,
) -> Result<String, String> {
    use tokio::io::AsyncBufReadExt;
    tokio::fs::create_dir_all(dir).await.map_err(|e| format!("mkdir: {e}"))?;
    log(id, "Exporting the committed tree (git archive)…".into());
    let mut ex = tokio::process::Command::new("sh");
    ex.args(["-c", "git --no-optional-locks -C \"$1\" archive --format=tar \"$2\" | tar -x -C \"$3\"", "sh", root, sha])
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_millis(EXPORT_MS), ex.output()).await {
        Ok(Ok(o)) if o.status.success() => {}
        Ok(Ok(o)) => return Err(format!("export failed: {}", String::from_utf8_lossy(&o.stderr).trim())),
        Ok(Err(e)) => return Err(format!("export failed: {e}")),
        Err(_) => return Err("export timed out".into()),
    }
    // the project link (usually git-ignored, so not in the archive)
    let mut linked = false;
    for f in ["project.json", "repo.json"] {
        let src = Path::new(root).join(".vercel").join(f);
        if src.is_file() {
            let _ = tokio::fs::create_dir_all(dir.join(".vercel")).await;
            if tokio::fs::copy(&src, dir.join(".vercel").join(f)).await.is_ok() && f == "project.json" {
                linked = true;
            }
        }
    }
    let scoped = scope_ok().await;
    let mut c = if scoped {
        let mut c = tokio::process::Command::new("systemd-run");
        c.args(["--user", "--scope", "--quiet", "--collect", "-p", "MemoryMax=2G", "--", &cli.0]);
        c
    } else {
        tokio::process::Command::new(&cli.0)
    };
    c.args(&cli.1).args(["deploy", "--yes"]);
    if target == "production" {
        c.arg("--prod");
    }
    if !linked {
        if let Some(l) = link {
            if let (Some(p), Some(o)) = (l["projectId"].as_str(), l["orgId"].as_str()) {
                c.env("VERCEL_PROJECT_ID", p).env("VERCEL_ORG_ID", o);
                linked = true;
            }
        }
    }
    if !linked {
        log(id, format!("Not linked to a Vercel project yet: the CLI links or creates project \"{}\"", dir.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
    }
    log(id, format!("$ {} deploy --yes{}", cli.2, if target == "production" { " --prod" } else { "" }));
    c.current_dir(dir)
        .env("VERCEL_TOKEN", token)
        .env("VERCEL_TELEMETRY_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .env("CI", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut ch = c.spawn().map_err(|e| format!("could not start {}: {e}", cli.2))?;
    let seen: std::sync::Arc<Mutex<Vec<(bool, String)>>> = Default::default();
    let pump = |r: Box<dyn tokio::io::AsyncRead + Unpin + Send>, out: bool| {
        let (id, tok, seen) = (id.to_string(), token.to_string(), seen.clone());
        tokio::spawn(async move {
            let mut l = tokio::io::BufReader::new(r).lines();
            while let Ok(Some(line)) = l.next_line().await {
                let line = clean_line(&line, &tok);
                if line.trim().is_empty() {
                    continue;
                }
                {
                    let mut s = seen.lock().unwrap();
                    s.push((out, line.clone()));
                    if s.len() > 400 {
                        s.drain(..100);
                    }
                }
                log(&id, line);
            }
        })
    };
    let a = pump(Box::new(ch.stdout.take().ok_or("no stdout")?), true);
    let b = pump(Box::new(ch.stderr.take().ok_or("no stderr")?), false);
    let status = match tokio::time::timeout(Duration::from_millis(DEPLOY_MS), ch.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("wait: {e}")),
        Err(_) => {
            let _ = ch.kill().await;
            return Err(format!("timed out after {} min", DEPLOY_MS / 60_000));
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = a.await;
        let _ = b.await;
    })
    .await;
    // remember the link the CLI made, so the next deploy goes to the same project (never written into the repo)
    if let Ok(b) = std::fs::read(dir.join(".vercel/project.json")) {
        if let Ok(p) = serde_json::from_slice::<Value>(&b) {
            if p["projectId"].is_string() && link.is_none() && !Path::new(root).join(".vercel/project.json").is_file() {
                let mut s = st().lock().unwrap();
                let h = hist(&mut s);
                h["links"][root] = json!({"projectId": p["projectId"], "orgId": p["orgId"]});
                save_hist(h);
            }
        }
    }
    let lines = seen.lock().unwrap().clone();
    if !status.success() {
        return Err(match status.code() {
            Some(c) => format!("vercel exited with {c}"),
            None => "vercel was killed".into(),
        });
    }
    deploy_url(&lines).ok_or_else(|| "no deployment URL in the output".into())
}

#[derive(Deserialize)]
struct JobQ {
    after: Option<usize>,
}

async fn api_job(AxPath(id): AxPath<String>, Query(q): Query<JobQ>) -> Response {
    let s = st().lock().unwrap();
    if let Some(j) = s.jobs.iter().find(|j| j.id == id) {
        let from = q.after.unwrap_or(0).max(j.first);
        let lines: Vec<&String> = j.lines.iter().skip(from - j.first).collect();
        let mut v = j.brief();
        v["lines"] = json!(lines);
        v["from"] = json!(from);
        v["next"] = json!(j.first + j.lines.len());
        return Json(v).into_response();
    }
    // an older deploy (after a restart): its record without the log
    let h = s.hist.as_ref().and_then(|h| h["deploys"].as_array().and_then(|l| l.iter().find(|d| d["id"] == id.as_str()).cloned()));
    drop(s);
    let h = h.or_else(|| {
        let mut s = st().lock().unwrap();
        hist(&mut s)["deploys"].as_array().and_then(|l| l.iter().find(|d| d["id"] == id.as_str()).cloned())
    });
    match h {
        Some(mut d) => {
            d["lines"] = json!([]);
            d["next"] = json!(0);
            d["expired"] = json!(true);
            Json(d).into_response()
        }
        None => json_err(StatusCode::NOT_FOUND, "no such deploy"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_remotes() {
        for (u, want) in [
            ("git@github.com:octo/widgets.git", Some("octo/widgets")),
            ("git@github.com:octo/widgets", Some("octo/widgets")),
            ("ssh://git@github.com/example-org/sheprd.git", Some("example-org/sheprd")),
            ("ssh://git@ssh.github.com:443/o/r.git", Some("o/r")),
            ("https://github.com/gshklovs/sheprd.git", Some("gshklovs/sheprd")),
            ("https://github.com/gshklovs/sheprd/", Some("gshklovs/sheprd")),
            ("https://x-access-token:ghp_secret@github.com/o/r.git", Some("o/r")),
            ("git://github.com/o/r.git", Some("o/r")),
            ("git@github.com-work:org/repo.name.git", Some("org/repo.name")),
            ("HTTPS://GitHub.com/O/R", None), // scheme is case-sensitive in git too
            ("https://gitlab.com/o/r.git", None),
            ("git@bitbucket.org:o/r.git", None),
            ("/home/user/Work/x", None),
            ("../other", None),
            ("https://github.com/o", None),
            ("https://github.com/o/r/tree/main", None),
            ("", None),
        ] {
            assert_eq!(github_slug(u).as_deref(), want, "{u}");
        }
        assert_eq!(scrub_remote("https://u:tok@github.com/o/r.git"), "https://github.com/o/r.git");
        assert_eq!(scrub_remote("git@github.com:o/r.git"), "git@github.com:o/r.git");
        assert_eq!(enc_path("feat/a b#1"), "feat/a%20b%231");
    }

    #[test]
    fn track_and_branches() {
        assert_eq!(parse_track("[ahead 2, behind 1]"), (2, 1, false));
        assert_eq!(parse_track("[behind 5]"), (0, 5, false));
        assert_eq!(parse_track("[gone]"), (0, 0, true));
        assert_eq!(parse_track(""), (0, 0, false));
        let us = '\x1f';
        let text = format!(
            "*{us}main{us}1791000000{us}origin/main{us}[ahead 1]{us}abc1234{us}/r{us}Fix: thing\n \
             {us}feat/x{us}1791100000{us}{us}{us}def5678{us}/r-wt{us}WIP\n \
             {us}old{us}1700000000{us}origin/old{us}[gone]{us}0001111{us}{us}subject{us}with unit sep\n"
        );
        let b = parse_branches(&text);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0]["name"], "feat/x");
        assert_eq!(b[0]["upstream"], Value::Null);
        assert_eq!(b[0]["ahead"], Value::Null);
        assert_eq!(b[0]["wt"], "/r-wt");
        assert_eq!(b[1]["name"], "main");
        assert_eq!(b[1]["current"], true);
        assert_eq!(b[1]["ahead"], 1);
        assert_eq!(b[1]["behind"], 0);
        assert_eq!(b[1]["ts"], 1791000000000i64);
        assert_eq!(b[2]["gone"], true);
        assert_eq!(b[2]["wt"], Value::Null);
        let w = parse_worktrees(
            "worktree /r\nHEAD 0123456789abcdef\nbranch refs/heads/main\n\nworktree /r-wt\nHEAD fedcba9876543210\nbranch refs/heads/feat/x\n\nworktree /r-d\nHEAD 1111111111\ndetached\n\nworktree /gone\nHEAD 2222222\nbranch refs/heads/z\nprunable gitdir file points to non-existent location\n",
        );
        assert_eq!(w.len(), 4);
        assert_eq!(w[0]["branch"], "main");
        assert_eq!(w[0]["head"], "0123456");
        assert_eq!(w[1]["branch"], "feat/x");
        assert_eq!(w[2]["detached"], true);
        assert_eq!(w[2]["branch"], Value::Null);
        assert_eq!(w[3]["prunable"], true);
        let l = parse_log("abc1234\x1fFix it\x1fAda\x1f1791000000\nbad line\n");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0]["ts"], 1791000000000i64);
        assert!(branch_ok("feat/x-1.2"));
        assert!(!branch_ok("--upload-pack=x"));
        assert!(!branch_ok("a..b"));
        assert!(!branch_ok("a b"));
    }

    #[test]
    fn vercel_detection() {
        let d = std::env::temp_dir().join(format!("mm-repos-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".vercel")).unwrap();
        let v = vercel_local(&d);
        assert_eq!(v["linked"], false);
        assert_eq!(v["config"], false);
        std::fs::write(d.join("vercel.json"), "{}").unwrap();
        assert_eq!(vercel_local(&d)["config"], true);
        std::fs::write(d.join(".vercel/repo.json"), r#"{"orgId":"team_1","projects":[{"id":"prj_m","name":"web","directory":"apps/web"}]}"#).unwrap();
        let v = vercel_local(&d);
        assert_eq!(v["linked"], true);
        assert_eq!(v["monorepo"], true);
        assert_eq!(v["projectId"], "prj_m");
        assert_eq!(v["orgId"], "team_1");
        std::fs::write(d.join(".vercel/project.json"), r#"{"projectId":"prj_abc","orgId":"team_xyz","projectName":"site"}"#).unwrap();
        let v = vercel_local(&d);
        assert_eq!(v["linked"], true);
        assert_eq!(v["monorepo"], false);
        assert_eq!(v["projectId"], "prj_abc");
        assert_eq!(v["projectName"], "site");
        let _ = std::fs::remove_dir_all(&d);
        let p = json!({"name": "site", "targets": {"production": {"alias": ["site.vercel.app", "x.com"]}}});
        assert_eq!(vapi_info(&p, Some("acme")), json!({"name": "site", "dashboard": "https://vercel.com/acme/site", "production": "https://site.vercel.app"}));
        assert_eq!(vapi_info(&json!({"name": "a"}), None)["dashboard"], Value::Null);
    }

    #[test]
    fn deploy_output() {
        let l = |o: bool, s: &str| (o, s.to_string());
        let lines = vec![
            l(false, "Vercel CLI 62.4.0"),
            l(false, "🔍  Inspect: https://vercel.com/acme/site/abc [2s]"),
            l(false, "✅  Preview: https://site-abc123-acme.vercel.app [4s]"),
            l(true, "https://site-abc123-acme.vercel.app"),
        ];
        assert_eq!(deploy_url(&lines).as_deref(), Some("https://site-abc123-acme.vercel.app"));
        // no stdout URL: the last *.vercel.app link anywhere
        assert_eq!(deploy_url(&lines[..3]).as_deref(), Some("https://site-abc123-acme.vercel.app"));
        assert_eq!(deploy_url(&lines[..1]), None);
        assert_eq!(clean_line("\x1b[32mtoken=SECRET123 ok\x1b[0m\r", "SECRET123"), "token=*** ok");
        assert!(setup_cmd().contains("vercel-token") && setup_cmd().contains("umask 077"));
    }
}
