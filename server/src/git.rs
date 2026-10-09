//! Branch vs main and CI/PR watch, per agent (read-only).
//!
//! - `GET /api/a/:pane/git` — the agent's repo: branch, ahead/behind vs the default branch, commits since the
//!   merge-base, files changed on the branch and uncommitted changes. Plain `git` with timeouts,
//!   `--no-optional-locks`: nothing here ever writes to a repo.
//! - `GET /api/a/:pane/git/diff?path=&scope=branch|work` — one file's unified diff.
//! - `GET /api/a/:pane/git/ci` — the branch's PR and its checks through `gh` (detects a missing login).
//! - `POST /api/a/:pane/git/watch {on}` — keep polling CI for this agent even when the phone does not look.
//! - `GET /api/a/:pane/git/fix` — a prompt with the failing checks' log tail (the phone sends it to the agent).
//!
//! The CI loop (`run`) only spends `gh` calls on agents the phone looked at in the last day or that have a
//! watch, only for branches that have an open PR, and at most every 2 min per branch. A newly failing head
//! commit pushes "CI red on <branch>". Commit / Push / Open PR are prompts the phone sends to the agent;
//! the server never runs a git write.
//! Hooks from main.rs: `routes`, `run`, `decorate` (the per-agent `ci` field).

use crate::transcript::{cut, now_ms};
use crate::{App, Shared, json_err, send_push};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const GIT_MS: u64 = 4000;
const GH_MS: u64 = 15_000;
const LOOK_TTL_MS: i64 = 24 * 3600_000;
/// Checks of an open PR: at most this often per branch.
const CHECKS_EVERY_MS: i64 = 120_000;
/// Is there a PR for this branch (yet)? At most this often.
const PR_EVERY_MS: i64 = 600_000;
const AUTH_EVERY_MS: i64 = 600_000;
const TICK_MS: u64 = 30_000;
const MAX_FILES: usize = 400;
const MAX_DIFF: usize = 400_000;

#[derive(Default)]
struct St {
    /// pane -> (cwd, session, title), from the last state publish
    agents: HashMap<String, (String, String, String)>,
    /// pane -> last time the phone showed it (or asked for its git view)
    looked: HashMap<String, i64>,
    /// session ids with a CI watch (persisted)
    watches: HashSet<String>,
    /// pane -> (repo root, branch) seen by the loop or the endpoints
    pane_key: HashMap<String, (String, String)>,
    /// (repo root, branch) -> CI cache
    ci: HashMap<(String, String), Ci>,
    /// (ok state, checked at)
    auth: Option<(GhAuth, i64)>,
}

#[derive(Clone, Default)]
struct Ci {
    pr: Option<Value>,
    pr_ts: i64,
    checks: Vec<Value>,
    checks_ts: i64,
    state: String,
    head: String,
    /// head commit the "CI red" push went out for
    alerted: String,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum GhAuth {
    Ok,
    NoAuth,
    Missing,
}

impl GhAuth {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            GhAuth::Ok => "ok",
            GhAuth::NoAuth => "no-auth",
            GhAuth::Missing => "missing",
        }
    }
}

fn st() -> &'static Mutex<St> {
    static S: OnceLock<Mutex<St>> = OnceLock::new();
    S.get_or_init(|| {
        let watches = std::fs::read(crate::state_dir().join("git.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["watches"].as_array().map(|l| l.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()))
            .unwrap_or_default();
        Mutex::new(St { watches, ..Default::default() })
    })
}

fn save_watches(w: &HashSet<String>) {
    let mut l: Vec<&String> = w.iter().collect();
    l.sort();
    let _ = std::fs::write(crate::state_dir().join("git.json"), json!({"watches": l}).to_string());
}

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/a/{pane}/git", get(api_git))
        .route("/api/a/{pane}/git/diff", get(api_diff))
        .route("/api/a/{pane}/git/ci", get(api_ci))
        .route("/api/a/{pane}/git/watch", post(api_watch))
        .route("/api/a/{pane}/git/fix", get(api_fix))
}

/// Per agent: `ci` = the cached PR/check summary of its branch, or null. Also remembers each agent's cwd and
/// which pane the phone is showing (the CI loop's "looked at in the last day").
pub fn decorate(app: &App, agents: &mut [Value]) {
    let viewing = app.inner.lock().unwrap().viewing.clone();
    let now = now_ms();
    let mut s = st().lock().unwrap();
    if let Some((Some(p), true, t)) = viewing {
        if now - t < 45_000 {
            s.looked.insert(p, now);
        }
    }
    s.agents.clear();
    for a in agents.iter_mut() {
        let pane = a["pane"].as_str().unwrap_or("").to_string();
        let sid = a["session"].as_str().unwrap_or("").to_string();
        let cwd = a["cwd"].as_str().unwrap_or("").to_string();
        let title = a["title"].as_str().unwrap_or("").to_string();
        let watch = !sid.is_empty() && s.watches.contains(&sid);
        let entry = s.pane_key.get(&pane).and_then(|k| s.ci.get(k));
        a["ci"] = match entry.and_then(|c| c.pr.as_ref().map(|pr| (c, pr))) {
            Some((c, pr)) => json!({"pr": pr["number"], "url": pr["url"], "prState": pr["state"],
                "state": if c.state.is_empty() { Value::Null } else { json!(c.state) }, "watch": watch}),
            None if watch => json!({"pr": null, "url": null, "prState": null, "state": null, "watch": true}),
            None => Value::Null,
        };
        if !pane.is_empty() {
            s.agents.insert(pane, (cwd, sid, title));
        }
    }
}

// ---------------------------------------------------------------------------
// process helpers

/// Runs a read-only git command in `cwd`: (exit ok, stdout). None on spawn failure / timeout.
pub(crate) async fn git(cwd: &str, args: &[&str]) -> Option<(bool, String)> {
    let mut c = tokio::process::Command::new("git");
    c.args(["--no-optional-locks", "-c", "core.quotepath=off", "-c", "color.ui=never", "-C", cwd])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_millis(GIT_MS), c.output()).await.ok()?.ok()?;
    Some((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

pub(crate) async fn git_ok(cwd: &str, args: &[&str]) -> Option<String> {
    git(cwd, args).await.filter(|(ok, _)| *ok).map(|(_, s)| s)
}

fn gh_bin() -> String {
    std::env::var("SHEPRD_GH").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "gh".into())
}

/// Runs `gh` (never interactive): Err(true) = not installed, Err(false) = timeout/spawn failure.
pub(crate) async fn gh(cwd: &str, args: &[&str], ms: u64) -> Result<(i32, String, String), bool> {
    let mut c = tokio::process::Command::new(gh_bin());
    c.args(args)
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_millis(ms), c.output()).await {
        Ok(Ok(o)) => Ok((
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )),
        Ok(Err(e)) => Err(e.kind() == std::io::ErrorKind::NotFound),
        Err(_) => Err(false),
    }
}

/// `gh auth status`, cached 10 min (`force` re-checks).
pub(crate) async fn gh_auth(force: bool) -> GhAuth {
    if !force {
        if let Some((a, t)) = st().lock().unwrap().auth {
            if now_ms() - t < AUTH_EVERY_MS {
                return a;
            }
        }
    }
    let home = crate::home();
    let a = match gh(&home.to_string_lossy(), &["auth", "status"], 8000).await {
        Ok((0, _, _)) => GhAuth::Ok,
        Ok(_) => GhAuth::NoAuth,
        Err(true) => GhAuth::Missing,
        Err(false) => GhAuth::NoAuth,
    };
    st().lock().unwrap().auth = Some((a, now_ms()));
    a
}

// ---------------------------------------------------------------------------
// repo inspection

/// The agent of `pane` from the last published state: (cwd, session, title).
fn agent(app: &App, pane: &str) -> Option<(String, String, String)> {
    if let Some(a) = st().lock().unwrap().agents.get(pane) {
        return Some(a.clone());
    }
    let s: Value = serde_json::from_str(&app.inner.lock().unwrap().state).ok()?;
    let a = s["agents"].as_array()?.iter().find(|a| a["pane"] == pane)?;
    Some((
        a["cwd"].as_str()?.to_string(),
        a["session"].as_str().unwrap_or("").to_string(),
        a["title"].as_str().unwrap_or("").to_string(),
    ))
}

struct Repo {
    root: String,
    /// None when detached
    branch: Option<String>,
    head: String,
    /// default branch name ("main")
    default: Option<String>,
    /// what we compare against ("origin/main" or "main")
    base: Option<String>,
}

async fn repo(cwd: &str) -> Option<Repo> {
    let top = git_ok(cwd, &["rev-parse", "--show-toplevel"]).await?;
    let root = top.trim().to_string();
    let branch = git_ok(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await.map(|s| s.trim().to_string());
    let head = git_ok(&root, &["rev-parse", "--verify", "--quiet", "HEAD"]).await.map(|s| s.trim().to_string()).unwrap_or_default();
    let origin_head = git_ok(&root, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]).await;
    let refs = git_ok(
        &root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/main",
            "refs/heads/master",
            "refs/remotes/origin/main",
            "refs/remotes/origin/master",
        ],
    )
    .await
    .unwrap_or_default();
    let (default, base) = pick_base(origin_head.as_deref().map(str::trim), &refs);
    Some(Repo { root, branch, head, default, base })
}

/// Default branch and the ref to compare against, from `origin/HEAD` and the existing main/master refs.
fn pick_base(origin_head: Option<&str>, refs: &str) -> (Option<String>, Option<String>) {
    let has = |r: &str| refs.lines().any(|l| l.trim() == r);
    if let Some(oh) = origin_head.filter(|s| s.starts_with("origin/") && s.len() > 7) {
        return (Some(oh[7..].to_string()), Some(oh.to_string()));
    }
    for b in ["main", "master"] {
        if has(&format!("refs/remotes/origin/{b}")) {
            return (Some(b.into()), Some(format!("origin/{b}")));
        }
        if has(&format!("refs/heads/{b}")) {
            return (Some(b.into()), Some(b.into()));
        }
    }
    (None, None)
}

/// `git diff --numstat` lines → [{path, add, del, bin}].
fn parse_numstat(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '\t');
            let (a, d, p) = (it.next()?, it.next()?, it.next()?);
            let bin = a == "-" && d == "-";
            Some(json!({"path": p, "add": a.parse::<u64>().ok(), "del": d.parse::<u64>().ok(), "bin": bin}))
        })
        .collect()
}

/// `git diff --name-status` lines → path -> status letter.
fn parse_name_status(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (s, p) = l.split_once('\t')?;
            Some((p.to_string(), s.chars().next()?.to_string()))
        })
        .collect()
}

/// `git status --porcelain=v1` → [{path, st, untracked}].
fn parse_status(text: &str) -> Vec<Value> {
    text.lines()
        .filter(|l| l.len() > 3)
        .map(|l| {
            let path = l[3..].rsplit(" -> ").next().unwrap_or(&l[3..]).trim_matches('"');
            let st = l[..2].trim();
            json!({"path": path, "st": st, "untracked": st == "??"})
        })
        .collect()
}

/// `git log --format=%H%x1f%h%x1f%s%x1f%an%x1f%ct` → [{sha, short, subject, author, ts}].
fn parse_log(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\x1f').collect();
            if f.len() < 5 {
                return None;
            }
            Some(json!({"sha": f[0], "short": f[1], "subject": f[2], "author": f[3],
                "ts": f[4].trim().parse::<i64>().unwrap_or(0) * 1000}))
        })
        .collect()
}

fn counts(text: Option<String>) -> Option<(u64, u64)> {
    let t = text?;
    let mut it = t.split_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

async fn overview(cwd: &str) -> Value {
    let Some(r) = repo(cwd).await else { return json!({"repo": false, "cwd": cwd}) };
    let root = r.root.as_str();
    let has_head = !r.head.is_empty();
    let mb = match (&r.base, has_head) {
        (Some(b), true) => git_ok(root, &["merge-base", b, "HEAD"]).await.map(|s| s.trim().to_string()),
        _ => None,
    };
    let (behind, ahead) = match (&r.base, has_head) {
        (Some(b), true) => counts(git_ok(root, &["rev-list", "--left-right", "--count", &format!("{b}...HEAD")]).await)
            .map(|(x, y)| (json!(x), json!(y)))
            .unwrap_or((Value::Null, Value::Null)),
        _ => (Value::Null, Value::Null),
    };
    let upstream = if r.branch.is_some() {
        git_ok(root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).await.map(|s| s.trim().to_string())
    } else {
        None
    };
    let (unpulled, unpushed) = match &upstream {
        Some(_) => counts(git_ok(root, &["rev-list", "--left-right", "--count", "@{u}...HEAD"]).await)
            .map(|(x, y)| (json!(x), json!(y)))
            .unwrap_or((Value::Null, Value::Null)),
        None => (Value::Null, Value::Null),
    };
    let (commits, more) = match (&mb, has_head) {
        (Some(m), true) => {
            let l = parse_log(
                &git_ok(root, &["log", "-n", "51", "--format=%H%x1f%h%x1f%s%x1f%an%x1f%ct", &format!("{m}..HEAD")])
                    .await
                    .unwrap_or_default(),
            );
            let more = l.len() > 50;
            (l.into_iter().take(50).collect::<Vec<_>>(), more)
        }
        (None, true) => (
            parse_log(&git_ok(root, &["log", "-n", "20", "--format=%H%x1f%h%x1f%s%x1f%an%x1f%ct"]).await.unwrap_or_default()),
            false,
        ),
        _ => (vec![], false),
    };
    let mut files = Vec::new();
    if let (Some(m), true) = (&mb, has_head) {
        let ns = git_ok(root, &["diff", "--no-renames", "--no-ext-diff", "--numstat", m, "HEAD"]).await.unwrap_or_default();
        let st = parse_name_status(
            &git_ok(root, &["diff", "--no-renames", "--no-ext-diff", "--name-status", m, "HEAD"]).await.unwrap_or_default(),
        );
        files = parse_numstat(&ns);
        for f in files.iter_mut() {
            let p = f["path"].as_str().unwrap_or("").to_string();
            f["st"] = json!(st.get(&p).cloned().unwrap_or_else(|| "M".into()));
        }
    }
    let mut work = parse_status(&git_ok(root, &["status", "--porcelain=v1", "--untracked-files=all"]).await.unwrap_or_default());
    if has_head && !work.is_empty() {
        let ns = parse_numstat(&git_ok(root, &["diff", "--no-renames", "--no-ext-diff", "--numstat", "HEAD"]).await.unwrap_or_default());
        let by: HashMap<&str, &Value> = ns.iter().map(|v| (v["path"].as_str().unwrap_or(""), v)).collect();
        for w in work.iter_mut() {
            let n = by.get(w["path"].as_str().unwrap_or("")).copied();
            w["add"] = n.map(|n| n["add"].clone()).unwrap_or(Value::Null);
            w["del"] = n.map(|n| n["del"].clone()).unwrap_or(Value::Null);
            w["bin"] = json!(n.is_some_and(|n| n["bin"] == true));
        }
    }
    let files_more = files.len() > MAX_FILES;
    let work_more = work.len() > MAX_FILES;
    files.truncate(MAX_FILES);
    work.truncate(MAX_FILES);
    let on_default = r.branch.is_some() && r.branch == r.default;
    json!({
        "repo": true, "cwd": cwd, "root": r.root, "branch": r.branch, "detached": r.branch.is_none(),
        "head": if has_head { json!(r.head) } else { Value::Null },
        "defaultBranch": r.default, "base": r.base, "onDefault": on_default, "mergeBase": mb,
        "ahead": ahead, "behind": behind,
        "upstream": upstream, "unpushed": unpushed, "unpulled": unpulled,
        "commits": commits, "commitsMore": more,
        "files": files, "filesMore": files_more, "work": work, "workMore": work_more,
    })
}

fn look(pane: &str) {
    st().lock().unwrap().looked.insert(pane.to_string(), now_ms());
}

async fn api_git(State(app): State<Shared>, AxPath(pane): AxPath<String>) -> Response {
    let Some((cwd, _, _)) = agent(&app, &pane) else { return json_err(StatusCode::NOT_FOUND, "no such agent") };
    look(&pane);
    let v = overview(&cwd).await;
    if let (Some(root), Some(b)) = (v["root"].as_str(), v["branch"].as_str()) {
        st().lock().unwrap().pane_key.insert(pane.clone(), (root.to_string(), b.to_string()));
    }
    Json(v).into_response()
}

#[derive(Deserialize)]
struct DiffQ {
    path: String,
    scope: Option<String>,
}

/// A repo-relative path without `..` (the untracked-file diff reads the file).
fn safe_rel(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('/') && !p.split('/').any(|c| c == "..") && !p.contains('\0')
}

async fn api_diff(State(app): State<Shared>, AxPath(pane): AxPath<String>, Query(q): Query<DiffQ>) -> Response {
    let Some((cwd, _, _)) = agent(&app, &pane) else { return json_err(StatusCode::NOT_FOUND, "no such agent") };
    if !safe_rel(&q.path) {
        return json_err(StatusCode::BAD_REQUEST, "bad path");
    }
    let Some(r) = repo(&cwd).await else { return json_err(StatusCode::NOT_FOUND, "not a git repo") };
    let root = r.root.as_str();
    let scope = q.scope.as_deref().unwrap_or("branch");
    let p = q.path.as_str();
    let base = ["diff", "--no-color", "--no-ext-diff", "--no-renames", "-U3"];
    let out = match scope {
        "branch" => {
            let Some(b) = &r.base else { return json_err(StatusCode::NOT_FOUND, "no default branch") };
            let Some(mb) = git_ok(root, &["merge-base", b, "HEAD"]).await else {
                return json_err(StatusCode::NOT_FOUND, "no merge-base");
            };
            git(root, &[&base[..], &[mb.trim(), "HEAD", "--", p]].concat()).await
        }
        "work" => {
            let tracked = git(root, &["ls-files", "--error-unmatch", "--", p]).await.is_some_and(|(ok, _)| ok);
            if !tracked {
                // untracked: the whole file as added (exit 1 = differences)
                git(root, &[&base[..], &["--no-index", "--", "/dev/null", p]].concat()).await
            } else if r.head.is_empty() {
                git(root, &[&base[..], &["--cached", "--", p]].concat()).await
            } else {
                git(root, &[&base[..], &["HEAD", "--", p]].concat()).await
            }
        }
        _ => return json_err(StatusCode::BAD_REQUEST, "scope must be branch or work"),
    };
    let Some((_, text)) = out else { return json_err(StatusCode::GATEWAY_TIMEOUT, "git timed out") };
    let bin = text.lines().any(|l| l.starts_with("Binary files "));
    let (diff, cut_) = cut(&text, MAX_DIFF);
    Json(json!({"path": p, "scope": scope, "root": r.root, "diff": diff, "cut": cut_, "bin": bin})).into_response()
}

// ---------------------------------------------------------------------------
// CI / PR

const NO_AUTH_HINT: &str = "Connect GitHub: run `gh auth login` in a terminal";
const MISSING_HINT: &str = "Install the GitHub CLI (`gh`) to see PRs and CI";

/// Summarizes `gh pr checks --json` buckets: fail > pending > pass; "none" if there are no checks.
fn rollup(checks: &[Value]) -> &'static str {
    let b = |k: &str| checks.iter().any(|c| c["bucket"] == k);
    if checks.is_empty() {
        "none"
    } else if b("fail") {
        "fail"
    } else if b("pending") {
        "pending"
    } else {
        "pass"
    }
}

/// Refreshes the PR (and, while it is open, its checks) of `root`/`branch` when stale.
/// `force` (a phone refresh) shortens the throttles to 15 s.
async fn refresh_ci(root: &str, branch: &str, force: bool) -> Ci {
    let key = (root.to_string(), branch.to_string());
    let now = now_ms();
    let cur = st().lock().unwrap().ci.get(&key).cloned().unwrap_or_default();
    let mut c = cur.clone();
    let open = c.pr.as_ref().is_some_and(|p| p["state"] == "OPEN");
    let pr_every = if force { 15_000 } else if open { CHECKS_EVERY_MS } else { PR_EVERY_MS };
    if now - c.pr_ts >= pr_every {
        c.pr_ts = now;
        match gh(
            root,
            &["pr", "view", branch, "--json", "number,url,state,title,headRefName,headRefOid,isDraft,baseRefName"],
            GH_MS,
        )
        .await
        {
            Ok((0, out, _)) => c.pr = serde_json::from_str::<Value>(&out).ok(),
            Ok((_, _, err)) if err.contains("no pull requests found") || err.contains("no open pull requests") => {
                c.pr = None;
                c.checks.clear();
                c.state.clear();
            }
            _ => {} // keep what we had (network, rate limit)
        }
    }
    let open = c.pr.as_ref().is_some_and(|p| p["state"] == "OPEN");
    let checks_every = if force { 15_000 } else { CHECKS_EVERY_MS };
    if open && now - c.checks_ts >= checks_every {
        c.checks_ts = now;
        let n = c.pr.as_ref().and_then(|p| p["number"].as_u64()).unwrap_or(0).to_string();
        // exit 1 = some check failed, 8 = pending; the JSON is printed either way
        if let Ok((_, out, _)) = gh(root, &["pr", "checks", &n, "--json", "name,state,bucket,link,workflow,completedAt"], GH_MS).await {
            if let Ok(Value::Array(l)) = serde_json::from_str::<Value>(&out) {
                c.checks = l;
                c.state = rollup(&c.checks).to_string();
            }
        }
        c.head = c.pr.as_ref().and_then(|p| p["headRefOid"].as_str()).unwrap_or("").to_string();
    }
    if !open {
        c.checks.clear();
        c.state.clear();
    }
    st().lock().unwrap().ci.insert(key, c.clone());
    c
}

#[derive(Deserialize)]
struct CiQ {
    refresh: Option<String>,
}

async fn api_ci(State(app): State<Shared>, AxPath(pane): AxPath<String>, Query(q): Query<CiQ>) -> Response {
    let Some((cwd, sid, _)) = agent(&app, &pane) else { return json_err(StatusCode::NOT_FOUND, "no such agent") };
    look(&pane);
    let force = q.refresh.as_deref().is_some_and(|v| v == "1");
    let watch = st().lock().unwrap().watches.contains(&sid);
    let auth = gh_auth(force).await;
    let hint = match auth {
        GhAuth::Ok => Value::Null,
        GhAuth::NoAuth => json!(NO_AUTH_HINT),
        GhAuth::Missing => json!(MISSING_HINT),
    };
    let Some(r) = repo(&cwd).await else {
        return Json(json!({"gh": auth.as_str(), "hint": hint, "repo": false, "pr": null, "checks": [], "state": null, "watch": watch}))
            .into_response();
    };
    let Some(branch) = r.branch.clone() else {
        return Json(json!({"gh": auth.as_str(), "hint": hint, "repo": true, "branch": null, "pr": null, "checks": [], "state": null,
            "watch": watch, "note": "detached HEAD"}))
        .into_response();
    };
    st().lock().unwrap().pane_key.insert(pane.clone(), (r.root.clone(), branch.clone()));
    if auth != GhAuth::Ok {
        return Json(json!({"gh": auth.as_str(), "hint": hint, "repo": true, "branch": branch, "pr": null, "checks": [],
            "state": null, "watch": watch}))
        .into_response();
    }
    let c = refresh_ci(&r.root, &branch, force).await;
    Json(json!({"gh": "ok", "hint": null, "repo": true, "branch": branch, "pr": c.pr, "checks": c.checks,
        "state": if c.state.is_empty() { Value::Null } else { json!(c.state) },
        "ts": c.checks_ts.max(c.pr_ts), "watch": watch}))
    .into_response()
}

#[derive(Deserialize)]
struct WatchBody {
    on: bool,
}

async fn api_watch(State(app): State<Shared>, AxPath(pane): AxPath<String>, Json(b): Json<WatchBody>) -> Response {
    let Some((_, sid, _)) = agent(&app, &pane) else { return json_err(StatusCode::NOT_FOUND, "no such agent") };
    if sid.is_empty() {
        return json_err(StatusCode::NOT_FOUND, "agent has no session yet");
    }
    {
        let mut s = st().lock().unwrap();
        if b.on {
            s.watches.insert(sid);
        } else {
            s.watches.remove(&sid);
        }
        save_watches(&s.watches);
    }
    app.refresh.notify_one();
    Json(json!({"ok": true, "watch": b.on})).into_response()
}

/// "https://github.com/o/r/actions/runs/123/job/456" → ("123", Some("456")).
fn run_of(link: &str) -> Option<(String, Option<String>)> {
    let i = link.find("/actions/runs/")?;
    let rest = &link[i + 14..];
    let run: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if run.is_empty() {
        return None;
    }
    let job = rest.find("/job/").map(|j| rest[j + 5..].chars().take_while(char::is_ascii_digit).collect::<String>()).filter(|s| !s.is_empty());
    Some((run, job))
}

/// `gh run view --log-failed` lines are "<job>\t<step>\t<timestamp> <text>": keep "<step> | <text>",
/// drop ANSI escapes, keep the last `max` bytes.
fn tidy_log(raw: &str, max: usize) -> String {
    let ansi = regex_lite::Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap();
    let ts = regex_lite::Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z ?").unwrap();
    let mut lines: Vec<String> = Vec::new();
    for l in raw.lines() {
        let l = ansi.replace_all(l, "");
        let mut f = l.splitn(3, '\t');
        let (a, b, c) = (f.next().unwrap_or(""), f.next(), f.next());
        let line = match (b, c) {
            (Some(step), Some(text)) => format!("{} | {}", step.trim(), ts.replace(text, "")),
            _ => ts.replace(a, "").into_owned(),
        };
        lines.push(line);
    }
    let all = lines.join("\n");
    if all.len() <= max {
        return all;
    }
    let mut start = all.len() - max;
    while !all.is_char_boundary(start) {
        start += 1;
    }
    let s = &all[start..];
    s.split_once('\n').map(|(_, r)| r).unwrap_or(s).to_string()
}

/// The prompt that asks the agent to fix the failing checks, with the failing jobs' log tail.
async fn fix_prompt(root: &str, branch: &str, c: &Ci) -> String {
    let failing: Vec<&Value> = c.checks.iter().filter(|x| x["bucket"] == "fail").collect();
    let pr = c.pr.as_ref();
    let mut t = format!(
        "CI is red on PR #{} ({}), branch `{branch}`.\nFailing checks:\n",
        pr.and_then(|p| p["number"].as_u64()).unwrap_or(0),
        pr.and_then(|p| p["title"].as_str()).unwrap_or("")
    );
    for f in &failing {
        t += &format!("- {} {}\n", f["name"].as_str().unwrap_or("?"), f["link"].as_str().unwrap_or(""));
    }
    let mut runs: Vec<String> = failing.iter().filter_map(|f| run_of(f["link"].as_str()?).map(|r| r.0)).collect();
    runs.dedup();
    let mut logs = String::new();
    for run in runs.iter().take(2) {
        if let Ok((_, out, _)) = gh(root, &["run", "view", run, "--log-failed"], 30_000).await {
            if !out.trim().is_empty() {
                logs += &format!("\nLog tail of run {run} (failed steps):\n```\n{}\n```\n", tidy_log(&out, 6000));
            }
        }
    }
    if logs.is_empty() {
        t += "\n(No log could be fetched; use `gh pr checks` / `gh run view --log-failed` to read it.)\n";
    } else {
        t += &logs;
    }
    t += "\nFind the cause, fix it, run the relevant checks locally, then commit and push. Tell me what it was.";
    t
}

async fn api_fix(State(app): State<Shared>, AxPath(pane): AxPath<String>) -> Response {
    let Some((cwd, _, _)) = agent(&app, &pane) else { return json_err(StatusCode::NOT_FOUND, "no such agent") };
    if gh_auth(false).await != GhAuth::Ok {
        return json_err(StatusCode::CONFLICT, NO_AUTH_HINT);
    }
    let Some(r) = repo(&cwd).await else { return json_err(StatusCode::NOT_FOUND, "not a git repo") };
    let Some(branch) = r.branch else { return json_err(StatusCode::NOT_FOUND, "detached HEAD") };
    let c = refresh_ci(&r.root, &branch, false).await;
    if c.state != "fail" {
        return json_err(StatusCode::CONFLICT, "no failing checks");
    }
    let text = fix_prompt(&r.root, &branch, &c).await;
    Json(json!({"text": text, "failing": c.checks.iter().filter(|x| x["bucket"] == "fail").count()})).into_response()
}

// ---------------------------------------------------------------------------
// CI loop

/// Every 30 s: for agents the phone looked at in the last day or with a watch, refresh the CI of their branch
/// (throttled per branch) and push "CI red" once per failing head commit. Costs nothing while no agent qualifies.
pub async fn run(app: Shared) {
    tokio::time::sleep(Duration::from_secs(20)).await;
    loop {
        tick(&app).await;
        tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
    }
}

async fn tick(app: &Shared) {
    let now = now_ms();
    let cands: Vec<(String, String, String)> = {
        let mut s = st().lock().unwrap();
        s.looked.retain(|_, t| now - *t < LOOK_TTL_MS);
        let s = &*s;
        s.agents
            .iter()
            .filter(|(p, (_, sid, _))| s.looked.contains_key(*p) || (!sid.is_empty() && s.watches.contains(sid)))
            .map(|(p, (cwd, _, title))| (p.clone(), cwd.clone(), title.clone()))
            .collect()
    };
    if cands.is_empty() || gh_auth(false).await != GhAuth::Ok {
        return;
    }
    let mut done: HashSet<(String, String)> = HashSet::new();
    for (pane, cwd, title) in cands {
        let Some(r) = repo(&cwd).await else { continue };
        let Some(branch) = r.branch.clone() else { continue };
        st().lock().unwrap().pane_key.insert(pane.clone(), (r.root.clone(), branch.clone()));
        if Some(&branch) == r.default.as_ref() || !done.insert((r.root.clone(), branch.clone())) {
            continue;
        }
        let c = refresh_ci(&r.root, &branch, false).await;
        if c.state == "fail" && !c.head.is_empty() && c.alerted != c.head {
            if let Some(e) = st().lock().unwrap().ci.get_mut(&(r.root.clone(), branch.clone())) {
                e.alerted = c.head.clone();
            }
            let failing: Vec<&str> =
                c.checks.iter().filter(|x| x["bucket"] == "fail").filter_map(|x| x["name"].as_str()).take(4).collect();
            let n = c.pr.as_ref().and_then(|p| p["number"].as_u64()).unwrap_or(0);
            let who = if title.is_empty() { pane.clone() } else { title.clone() };
            let payload = json!({
                "title": format!("CI red on {branch}"),
                "body": cut(&format!("{who} · PR #{n} — failing: {}. Tap to send the log to the agent.", failing.join(", ")), 300).0,
                "url": format!("/#/g/{pane}?fix=1"), "tag": format!("ci-{pane}"),
            });
            send_push(app, &payload, false).await;
            app.refresh.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_choice() {
        assert_eq!(pick_base(Some("origin/trunk"), ""), (Some("trunk".into()), Some("origin/trunk".into())));
        assert_eq!(pick_base(None, "refs/heads/main\nrefs/remotes/origin/main\n"), (Some("main".into()), Some("origin/main".into())));
        assert_eq!(pick_base(None, "refs/heads/master\n"), (Some("master".into()), Some("master".into())));
        assert_eq!(pick_base(None, ""), (None, None));
    }

    #[test]
    fn parsers() {
        let n = parse_numstat("3\t1\tsrc/a.rs\n-\t-\timg.png\n");
        assert_eq!(n[0]["add"], 3);
        assert_eq!(n[1]["bin"], true);
        assert_eq!(n[1]["add"], Value::Null);
        let s = parse_status(" M web/app.js\n?? new.txt\nR  a -> b\n");
        assert_eq!(s[0]["st"], "M");
        assert_eq!(s[1]["untracked"], true);
        assert_eq!(s[2]["path"], "b");
        let l = parse_log("abc\x1fab\x1fFix it\x1fG\x1f1700000000\n");
        assert_eq!(l[0]["ts"], 1_700_000_000_000i64);
        assert_eq!(parse_name_status("A\tx.rs\nD\ty.rs\n").get("y.rs").map(String::as_str), Some("D"));
        assert_eq!(counts(Some("2\t5\n".into())), Some((2, 5)));
    }

    #[test]
    fn rollups_and_links() {
        assert_eq!(rollup(&[]), "none");
        assert_eq!(rollup(&[json!({"bucket": "pass"}), json!({"bucket": "skipping"})]), "pass");
        assert_eq!(rollup(&[json!({"bucket": "pass"}), json!({"bucket": "pending"})]), "pending");
        assert_eq!(rollup(&[json!({"bucket": "pending"}), json!({"bucket": "fail"})]), "fail");
        assert_eq!(
            run_of("https://github.com/o/r/actions/runs/123/job/456"),
            Some(("123".to_string(), Some("456".to_string())))
        );
        assert_eq!(run_of("https://ci.example.com/x"), None);
        assert!(safe_rel("src/a.rs") && !safe_rel("../x") && !safe_rel("/etc/passwd") && !safe_rel("a/../../b"));
    }

    #[test]
    fn log_tail() {
        let raw = "build\tRun tests\t2026-10-05T10:00:00.1234567Z \x1b[31merror\x1b[0m: boom\nbuild\tRun tests\t2026-10-05T10:00:01Z done\n";
        let t = tidy_log(raw, 6000);
        assert_eq!(t, "Run tests | error: boom\nRun tests | done");
        let long = "x\ty\t".to_string() + &"a".repeat(100) + "\nx\ty\tlast line";
        assert_eq!(tidy_log(&long, 20), "y | last line");
    }
}
