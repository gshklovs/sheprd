//! Disk usage: volumes (live `statvfs`), cached home scans, agent scratch.
//!
//! Scans never run inside the server (a full home tree is ~50 MB): the server
//! spawns itself as `sheprd disk-scan <path> --depth 4 --top 12` under
//! `nice -n 19 ionice -c3`, single flight, cancel = kill. The child scans with
//! `disktree-core` (2 workers), classifies, computes "worth a look" findings,
//! prints progress lines `P <files> <bytes>` to stderr and a pruned JSON tree to
//! stdout, and exits. The server stores that at `<state>/disk/<hash(path)>.json`
//! and serves requests from the deepest cached scan that covers the path.
//! Nothing here deletes anything.

use crate::transcript::now_ms;
use crate::{Shared, json_err};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const STALE_MS: i64 = 6 * 3600_000;
const MIN_FINDING: u64 = 64 << 20;

// ---------------------------------------------------------------------------
// Volumes

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Volume {
    pub mount: String,
    pub fs: String,
    pub used: u64,
    pub total: u64,
    pub avail: u64,
    pub ram: bool,
}

impl Volume {
    pub fn json(&self) -> Value {
        json!({"mount": self.mount, "fs": self.fs, "used": self.used, "total": self.total, "avail": self.avail, "ram": self.ram})
    }
}

/// (source, mount point, fs type) of the mount that holds `path`.
fn mount_of<'a>(mounts: &'a [(String, String, String)], path: &str) -> Option<&'a (String, String, String)> {
    mounts
        .iter()
        .filter(|(_, m, _)| path == m || m == "/" || path.starts_with(&format!("{m}/")))
        .max_by_key(|(_, m, _)| m.len())
}

fn statvfs(path: &str) -> Option<(u64, u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let f = s.f_frsize as u64;
    let total = s.f_blocks as u64 * f;
    Some((total, total.saturating_sub(s.f_bfree as u64 * f), s.f_bavail as u64 * f))
}

/// `/home`, `/` (only if on another device than /home) and `/tmp`.
pub fn volumes() -> Vec<Volume> {
    let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let mounts: Vec<(String, String, String)> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 3).then(|| (f[0].to_string(), f[1].replace("\\040", " "), f[2].to_string()))
        })
        .collect();
    let mut out = Vec::new();
    let mut seen_src: Vec<String> = Vec::new();
    for path in ["/home", "/", "/tmp"] {
        let Some((src, _, fs)) = mount_of(&mounts, path) else { continue };
        let ram = matches!(fs.as_str(), "tmpfs" | "ramfs");
        if !ram && seen_src.contains(src) {
            continue;
        }
        let Some((total, used, avail)) = statvfs(path) else { continue };
        seen_src.push(src.clone());
        out.push(Volume { mount: path.into(), fs: fs.clone(), used, total, avail, ram });
    }
    out
}

// ---------------------------------------------------------------------------
// The scan child (`sheprd disk-scan …`)

fn kind_of(n: &disktree_core::tree::Node) -> &'static str {
    use disktree_core::classify::{Category, Reclaim};
    match n.reclaim {
        Some(Reclaim::BuildOutput | Reclaim::Reinstallable) => return "build",
        Some(_) => return "cache",
        None => {}
    }
    match n.category {
        Category::Code => "code",
        Category::AgentScratch => "agent",
        Category::Toolchain => "toolchain",
        Category::Git => "vcs",
        Category::Media => "media",
        Category::Cache => "cache",
        _ => "other",
    }
}

/// Compact cached node: n name, b bytes, f files, d dirs, k kind, r reclaimable,
/// m mtime ms, dir, c children (present = expanded), more [count, bytes].
fn prune(n: &disktree_core::tree::Node, depth: usize, max_depth: usize, top: usize, min_bytes: u64) -> Value {
    let mut v = json!({"n": &*n.name, "b": n.bytes, "f": n.files, "d": n.dirs, "k": kind_of(n), "dir": n.is_dir()});
    if n.reclaim.is_some() {
        v["r"] = json!(true);
    }
    if n.modified > 0 {
        v["m"] = json!(n.modified * 1000);
    }
    let expand = n.is_dir() && depth < max_depth && (depth == 0 || n.bytes >= min_bytes) && !n.children.is_empty();
    if expand {
        // children are sorted by bytes, descending
        let c: Vec<Value> = n.children.iter().take(top).map(|c| prune(c, depth + 1, max_depth, top, min_bytes)).collect();
        v["c"] = json!(c);
        if n.children.len() > top {
            let rest = &n.children[top..];
            v["more"] = json!([rest.len(), rest.iter().map(|c| c.bytes).sum::<u64>()]);
        }
    } else if n.is_dir() && n.children.is_empty() {
        v["c"] = json!([]); // an empty directory is fully known
    }
    v
}

fn finding_json(root_path: &Path, root: &disktree_core::tree::Node, c: &disktree_core::insights::Candidate, now: i64) -> Value {
    use disktree_core::classify::Reclaim;
    use disktree_core::insights::Finding;
    let path = disktree_core::tree::path_of(root_path, root, &c.crumbs);
    let node = root.resolve(&c.crumbs);
    let age = node.map(|n| n.modified).filter(|m| *m > 0).map(|m| (now - m).max(0) / 86400);
    let (kind, why, age) = match &c.finding {
        Finding::Reclaimable(r) => {
            let why = match r {
                Reclaim::Regenerable => "cache — rebuilt on demand",
                Reclaim::BuildOutput => "build output — its build tool recreates it",
                Reclaim::Reinstallable => "dependencies — reinstalled from the manifest",
                Reclaim::PackageStore => "package store — refetched on demand",
                Reclaim::SandboxLayers => "sandbox layers — re-pulled on demand",
                Reclaim::Snapshots => "sandbox/VM snapshots",
                Reclaim::Trash => "trash — already deleted",
                Reclaim::Temporary => "scratch meant to be thrown away",
                Reclaim::SyncHistory => "sync history — old versions of files",
            };
            let kind = if matches!(r, Reclaim::BuildOutput | Reclaim::Reinstallable) { "build" } else { "cache" };
            (kind, why.to_string(), age)
        }
        Finding::Worktrees { count, oldest_days } => {
            ("worktree", format!("{count} agent worktree{} · oldest {oldest_days} days", if *count == 1 { "" } else { "s" }), Some(*oldest_days))
        }
        Finding::StaleExperiments { count } => (
            "stale-experiment",
            format!("{count} experiment{} untouched for 30+ days", if *count == 1 { "" } else { "s" }),
            age,
        ),
    };
    json!({"path": path.to_string_lossy(), "bytes": c.bytes, "kind": kind, "why": why, "ageDays": age})
}

fn arg_after(args: &[String], flag: &str) -> Option<usize> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok())
}

/// The `disk-scan` subcommand. Returns the process exit code.
pub fn cli(args: &[String]) -> i32 {
    use disktree_core::scan::{ScanHandle, ScanOptions};
    use disktree_core::scan_threads::ScanThreads;
    let Some(root) = args.first().filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: sheprd disk-scan <path> [--depth 4] [--top 12] [--workers 2]");
        return 2;
    };
    let depth = arg_after(args, "--depth").unwrap_or(4);
    let top = arg_after(args, "--top").unwrap_or(12);
    let workers = arg_after(args, "--workers").unwrap_or(2).max(1);
    let root = PathBuf::from(root);
    let options = ScanOptions {
        threads: ScanThreads { max_threads: workers, adaptive: false, system_cpu_limit: None, ..ScanThreads::default() },
        ..ScanOptions::default()
    };
    let t0 = Instant::now();
    let started = now_ms();
    let handle = ScanHandle::spawn(root.clone(), options);
    let mut last = Instant::now();
    let node = loop {
        if let Some(r) = handle.poll() {
            match r {
                Ok(n) => break n,
                Err(e) => {
                    eprintln!("E {e}");
                    return 1;
                }
            }
        }
        if last.elapsed() >= Duration::from_millis(250) {
            let s = handle.progress.snapshot();
            eprintln!("P {} {}", s.files, s.bytes);
            last = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let now = now_ms() / 1000;
    let min_bytes = (node.bytes / 2000).max(32 << 20);
    let findings: Vec<Value> = disktree_core::insights::worth_a_look(&node, now, 60)
        .iter()
        .filter(|c| c.bytes >= MIN_FINDING)
        .map(|c| finding_json(&root, &node, c, now))
        .collect();
    let out = json!({
        "root": root.to_string_lossy(), "scannedAt": started, "scanMs": t0.elapsed().as_millis() as u64,
        "depth": depth, "top": top, "files": node.files, "errors": handle.progress.snapshot().errors,
        "tree": prune(&node, 0, depth, top, min_bytes), "findings": findings,
    });
    println!("{out}");
    0
}

// ---------------------------------------------------------------------------
// Cache

fn cache_dir() -> PathBuf {
    let d = crate::state_dir().join("disk");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// FNV-1a, hex: the cache file name for a scanned path.
fn hash_name(p: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in p.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}.json")
}

#[derive(Clone, Debug, serde::Serialize, Deserialize)]
struct IndexEntry {
    root: String,
    file: String,
    #[serde(rename = "scannedAt")]
    scanned_at: i64,
    #[serde(rename = "scanMs")]
    scan_ms: u64,
}

fn read_index() -> Vec<IndexEntry> {
    std::fs::read(cache_dir().join("index.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_index(list: &[IndexEntry]) {
    let _ = std::fs::write(cache_dir().join("index.json"), serde_json::to_vec(list).unwrap_or_default());
}

fn store(root: &str, out: &[u8], v: &Value) -> Result<(), String> {
    let file = hash_name(root);
    std::fs::write(cache_dir().join(&file), out).map_err(|e| e.to_string())?;
    let mut idx = read_index();
    idx.retain(|e| e.root != root);
    idx.push(IndexEntry {
        root: root.to_string(),
        file,
        scanned_at: v["scannedAt"].as_i64().unwrap_or(0),
        scan_ms: v["scanMs"].as_u64().unwrap_or(0),
    });
    write_index(&idx);
    Ok(())
}

/// The node at `path` inside a cached tree rooted at `root`.
fn lookup<'a>(tree: &'a Value, root: &str, path: &str) -> Option<&'a Value> {
    let rel = if path == root { "" } else { path.strip_prefix(root)?.strip_prefix('/')? };
    let mut node = tree;
    for comp in rel.split('/').filter(|c| !c.is_empty()) {
        node = node["c"].as_array()?.iter().find(|c| c["n"] == comp)?;
    }
    Some(node)
}

// ---------------------------------------------------------------------------
// Scanning (single flight)

#[derive(Default)]
struct DiskState {
    seq: u64,
    scanning: Option<Scanning>,
    error: Option<(String, String)>,
}

struct Scanning {
    id: String,
    path: String,
    files: u64,
    bytes: u64,
    started: i64,
    kill: Option<tokio::sync::oneshot::Sender<()>>,
}

static D: LazyLock<Mutex<DiskState>> = LazyLock::new(Default::default);

pub fn scanning_json() -> Value {
    let d = D.lock().unwrap();
    match &d.scanning {
        Some(s) => json!({"scanId": s.id, "path": s.path, "files": s.files, "bytes": s.bytes, "startedAt": s.started}),
        None => Value::Null,
    }
}

fn self_exe() -> PathBuf {
    let p = std::fs::read_link("/proc/self/exe").unwrap_or_else(|_| PathBuf::from("sheprd"));
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s).to_string())
}

/// Starts a scan of `path` unless one runs: Ok(id) or Err(running id).
fn start_scan(path: &str) -> Result<String, String> {
    let (id, rx) = {
        let mut d = D.lock().unwrap();
        if let Some(s) = &d.scanning {
            return Err(s.id.clone());
        }
        d.seq += 1;
        let id = format!("s{}", d.seq);
        let (tx, rx) = tokio::sync::oneshot::channel();
        d.scanning = Some(Scanning { id: id.clone(), path: path.to_string(), files: 0, bytes: 0, started: now_ms(), kill: Some(tx) });
        (id, rx)
    };
    let path = path.to_string();
    let sid = id.clone();
    tokio::spawn(async move {
        let r = run_scan(&path, rx).await;
        let mut d = D.lock().unwrap();
        if d.scanning.as_ref().is_some_and(|s| s.id == sid) {
            d.scanning = None;
        }
        match r {
            Ok(()) => d.error = None,
            Err(e) if e == "cancelled" => {}
            Err(e) => d.error = Some((path.clone(), format!("scan failed: {e}"))),
        }
        drop(d);
        crate::trim_heap();
    });
    Ok(id)
}

async fn run_scan(path: &str, mut kill: tokio::sync::oneshot::Receiver<()>) -> Result<(), String> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let exe = self_exe();
    let args = ["disk-scan", path, "--depth", "4", "--top", "12", "--workers", "2"];
    let spawn = |niced: bool| {
        let mut c = if niced {
            let mut c = tokio::process::Command::new("nice");
            c.args(["-n", "19", "ionice", "-c3"]).arg(&exe);
            c
        } else {
            tokio::process::Command::new(&exe)
        };
        c.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn()
    };
    let mut child = spawn(true).or_else(|_| spawn(false)).map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let progress = async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut last_err = String::new();
        while let Ok(Some(l)) = lines.next_line().await {
            if let Some(rest) = l.strip_prefix("P ") {
                let mut it = rest.split_whitespace().filter_map(|x| x.parse::<u64>().ok());
                let (f, b) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
                if let Some(s) = D.lock().unwrap().scanning.as_mut() {
                    s.files = f;
                    s.bytes = b;
                }
            } else if !l.trim().is_empty() {
                last_err = l.trim_start_matches("E ").to_string();
            }
        }
        last_err
    };
    let read = async move {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    };
    let work = async {
        let (out, err) = tokio::join!(read, progress);
        let status = child.wait().await;
        (out, err, status)
    };
    let (out, err, status) = tokio::select! {
        r = work => r,
        _ = &mut kill => return Err("cancelled".into()),   // dropping the child kills it
    };
    let ok = status.map(|s| s.success()).unwrap_or(false);
    if !ok {
        return Err(if err.is_empty() { "scanner exited with an error".into() } else { err });
    }
    let v: Value = serde_json::from_slice(&out).map_err(|e| format!("bad scanner output: {e}"))?;
    store(path, &out, &v)
}

fn cancel_scan() -> bool {
    let mut d = D.lock().unwrap();
    match d.scanning.take() {
        Some(mut s) => {
            if let Some(k) = s.kill.take() {
                let _ = k.send(());
            }
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Agent scratch (cached 60 s)

/// Disk usage (st_blocks) of a tree, one filesystem, no symlinks followed.
/// Also returns the sizes of the second-level directories (`<project>/<session>`).
fn du(root: &Path, level2: bool) -> (u64, Vec<(PathBuf, u64)>) {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::symlink_metadata(root) else { return (0, vec![]) };
    let dev = meta.dev();
    let mut total = meta.blocks() * 512;
    let mut l2 = Vec::new();
    fn walk(p: &Path, dev: u64, depth: usize, level2: bool, l2: &mut Vec<(PathBuf, u64)>, n: &mut usize) -> u64 {
        use std::os::unix::fs::MetadataExt;
        let Ok(rd) = std::fs::read_dir(p) else { return 0 };
        let mut sum = 0;
        for e in rd.flatten() {
            *n += 1;
            if *n > 400_000 {
                break;
            }
            let Ok(m) = e.metadata() else { continue };
            let mut s = m.blocks() * 512;
            if m.is_dir() && m.dev() == dev {
                s += walk(&e.path(), dev, depth + 1, level2, l2, n);
                if level2 && depth == 1 {
                    l2.push((e.path(), s));
                }
            }
            sum += s;
        }
        sum
    }
    let mut n = 0;
    total += walk(root, dev, 0, level2, &mut l2, &mut n);
    (total, l2)
}

static SCRATCH: LazyLock<Mutex<Option<(Instant, Vec<(String, u64, bool, Vec<(String, u64)>)>)>>> = LazyLock::new(Default::default);

fn tmp_scratch() -> String {
    format!("/tmp/claude-{}", unsafe { libc::getuid() })
}

async fn scratch(app: &Shared) -> Vec<Value> {
    let cached = SCRATCH.lock().unwrap().as_ref().filter(|(t, _)| t.elapsed() < Duration::from_secs(60)).map(|(_, v)| v.clone());
    let list = match cached {
        Some(v) => v,
        None => {
            let v = tokio::task::spawn_blocking(|| {
                let home = crate::home().join(".claude");
                let (hb, _) = du(&home, false);
                let tmp = tmp_scratch();
                let (tb, l2) = du(Path::new(&tmp), true);
                let mut top: Vec<(String, u64)> = l2
                    .into_iter()
                    .filter(|(p, _)| p.file_name().is_some_and(|n| n.len() == 36 && n.to_string_lossy().matches('-').count() == 4))
                    .map(|(p, b)| (p.to_string_lossy().into_owned(), b))
                    .collect();
                top.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
                top.truncate(5);
                vec![(home.to_string_lossy().into_owned(), hb, false, vec![]), (tmp, tb, true, top)]
            })
            .await
            .unwrap_or_default();
            *SCRATCH.lock().unwrap() = Some((Instant::now(), v.clone()));
            v
        }
    };
    let pane_of: HashMap<String, String> =
        app.inner.lock().unwrap().pane_sessions.iter().map(|(p, s)| (s.clone(), p.clone())).collect();
    list.into_iter()
        .map(|(path, bytes, ram, top)| {
            let note = if ram { "in RAM (tmpfs), clears on reboot" } else { "transcripts are kept" };
            let mut v = json!({"path": path, "bytes": bytes, "ram": ram, "note": note});
            if ram {
                v["top"] = top
                    .iter()
                    .map(|(p, b)| {
                        let sid = p.rsplit('/').next().unwrap_or("").to_string();
                        json!({"path": p, "bytes": b, "session": sid, "pane": pane_of.get(&sid)})
                    })
                    .collect();
            }
            v
        })
        .collect()
}

// ---------------------------------------------------------------------------
// HTTP

pub fn routes() -> Router<Shared> {
    Router::new().route("/api/disk", get(api_disk)).route("/api/disk/scan", post(api_scan).delete(api_cancel))
}

/// `~`-expanded, normalized, and inside `$HOME` (else None).
fn resolve(p: &str) -> Option<String> {
    let home = crate::home().to_string_lossy().trim_end_matches('/').to_string();
    let p = p.trim();
    let abs = if p.is_empty() || p == "~" {
        home.clone()
    } else if let Some(r) = p.strip_prefix("~/") {
        format!("{home}/{r}")
    } else {
        p.to_string()
    };
    if abs.split('/').any(|c| c == ".." || c == ".") {
        return None;
    }
    let mut abs = abs.trim_end_matches('/').to_string();
    while abs.contains("//") {
        abs = abs.replace("//", "/");
    }
    (abs == home || abs.starts_with(&format!("{home}/"))).then_some(abs)
}

struct AgentCwd {
    pane: String,
    title: String,
    cwd: String,
}

fn agent_cwds(app: &Shared) -> Vec<AgentCwd> {
    let state = app.inner.lock().unwrap().state.clone();
    let v: Value = serde_json::from_str(&state).unwrap_or(Value::Null);
    v["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some(AgentCwd {
                pane: a["pane"].as_str()?.to_string(),
                title: a["title"].as_str().unwrap_or("agent").to_string(),
                cwd: a["cwd"].as_str()?.trim_end_matches('/').to_string(),
            })
        })
        .collect()
}

/// The live agent a path belongs to: the deepest agent cwd that is the path or an
/// ancestor of it (never `$HOME` itself); else, for a worktree path, the one
/// agent working inside it.
fn agent_for(agents: &[AgentCwd], path: &str) -> Value {
    let home = crate::home().to_string_lossy().trim_end_matches('/').to_string();
    let owner = agents
        .iter()
        .filter(|a| a.cwd != home && (a.cwd == path || path.starts_with(&format!("{}/", a.cwd))))
        .max_by_key(|a| a.cwd.len());
    let owner = owner.or_else(|| {
        let inside: Vec<&AgentCwd> = agents.iter().filter(|a| a.cwd.starts_with(&format!("{path}/"))).collect();
        (path.contains("/worktrees") && inside.len() == 1).then(|| inside[0])
    });
    match owner {
        Some(a) => json!({"pane": a.pane, "title": a.title}),
        None => Value::Null,
    }
}

fn render_node(node: &Value, path: &str, agents: &[AgentCwd]) -> Value {
    let children: Vec<Value> = node["c"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let name = c["n"].as_str().unwrap_or("");
            let cp = format!("{path}/{name}");
            let dir = c["dir"].as_bool().unwrap_or(false);
            json!({
                "name": name, "path": cp, "bytes": c["b"], "files": c["f"], "dir": dir, "kind": c["k"],
                "reclaim": c["r"].as_bool().unwrap_or(false), "deeper": dir && c.get("c").is_some(),
                "agent": if dir { agent_for(agents, &cp) } else { Value::Null },
            })
        })
        .collect();
    let more = node["more"].as_array().map(|m| json!({"count": m[0], "bytes": m[1]})).unwrap_or(Value::Null);
    json!({
        "path": path, "name": path.rsplit('/').next().unwrap_or(""), "bytes": node["b"], "files": node["f"],
        "dirs": node["d"], "kind": node["k"], "mtime": node["m"], "children": children, "more": more,
    })
}

#[derive(Deserialize)]
struct DiskQ {
    path: Option<String>,
    scan: Option<String>,
}

async fn api_disk(State(app): State<Shared>, Query(q): Query<DiskQ>) -> Response {
    let Some(path) = resolve(q.path.as_deref().unwrap_or("~")) else {
        return json_err(StatusCode::BAD_REQUEST, "path must be inside your home folder");
    };
    let auto = q.scan.as_deref() != Some("never");
    let home = crate::home().to_string_lossy().trim_end_matches('/').to_string();
    let path2 = path.clone();
    // Find the deepest cache that has `path` expanded; render off-thread.
    let found = tokio::task::spawn_blocking(move || {
        let mut idx = read_index();
        idx.retain(|e| e.root == path2 || path2.starts_with(&format!("{}/", e.root)));
        idx.sort_by_key(|e| std::cmp::Reverse(e.root.len()));
        let mut home_entry = None;
        let mut hit: Option<(IndexEntry, Value, Vec<Value>)> = None;
        for e in idx {
            if e.root == crate::home().to_string_lossy().trim_end_matches('/') {
                home_entry = Some(e.clone());
            }
            if hit.is_some() {
                continue;
            }
            let Ok(b) = std::fs::read(cache_dir().join(&e.file)) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&b) else { continue };
            if let Some(n) = lookup(&v["tree"], &e.root, &path2) {
                if n.get("c").is_some() || !n["dir"].as_bool().unwrap_or(true) {
                    let n = n.clone();
                    let findings: Vec<Value> = v["findings"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|f| f["path"].as_str().is_some_and(|p| p == path2 || p.starts_with(&format!("{path2}/"))))
                        .take(20)
                        .cloned()
                        .collect();
                    hit = Some((e, n, findings));
                }
            }
        }
        (hit, home_entry)
    })
    .await;
    let Ok((hit, home_entry)) = found else { return json_err(StatusCode::INTERNAL_SERVER_ERROR, "cache read failed") };
    let now = now_ms();
    let home_stale = home_entry.as_ref().is_none_or(|e| now - e.scanned_at > STALE_MS);
    if auto {
        if home_entry.is_none() || (home_stale && path == home) {
            let _ = start_scan(&home);
        } else if hit.is_none() {
            let _ = start_scan(&path);
        } else if home_stale {
            let _ = start_scan(&home);
        }
    }
    let agents = agent_cwds(&app);
    let (root, scanned_at, scan_ms, node, findings) = match &hit {
        Some((e, n, f)) => {
            let findings: Vec<Value> = f
                .iter()
                .map(|f| {
                    let mut f = f.clone();
                    f["agent"] = agent_for(&agents, f["path"].as_str().unwrap_or(""));
                    f
                })
                .collect();
            (e.root.clone(), json!(e.scanned_at), json!(e.scan_ms), render_node(n, &path, &agents), findings)
        }
        None => (home.clone(), Value::Null, Value::Null, Value::Null, vec![]),
    };
    let stale = hit.as_ref().is_some_and(|(e, _, _)| now - e.scanned_at > STALE_MS);
    let error = D.lock().unwrap().error.as_ref().filter(|(p, _)| *p == path || *p == home).map(|(_, e)| e.clone());
    let out = json!({
        "root": root, "path": path, "scannedAt": scanned_at, "scanMs": scan_ms, "stale": stale,
        "scanning": scanning_json(), "error": error, "node": node, "findings": findings,
        "scratch": scratch(&app).await,
        "volumes": volumes().iter().map(Volume::json).collect::<Vec<_>>(),
    });
    drop(hit);
    crate::trim_heap();
    Json(out).into_response()
}

async fn api_scan(body: Option<Json<Value>>) -> Response {
    let p = body.as_ref().and_then(|b| b["path"].as_str().map(str::to_string)).unwrap_or_else(|| "~".into());
    let Some(path) = resolve(&p) else { return json_err(StatusCode::BAD_REQUEST, "path must be inside your home folder") };
    match start_scan(&path) {
        Ok(id) => (StatusCode::ACCEPTED, Json(json!({"scanId": id}))).into_response(),
        Err(id) => (StatusCode::CONFLICT, Json(json!({"error": "busy", "scanId": id}))).into_response(),
    }
}

async fn api_cancel() -> Response {
    let was = cancel_scan();
    Json(json!({"ok": true, "cancelled": was})).into_response()
}

/// Disk lines for `/api/res/brief?disk=1`: volumes and the top 3 cached findings.
/// Never starts a scan.
pub fn brief_lines(_app: &Shared) -> String {
    let mut out = String::new();
    let vols: Vec<String> = volumes()
        .iter()
        .map(|v| {
            if v.ram {
                format!("{} (RAM) {} of {}", v.mount, crate::res::fmt_gb(v.used), crate::res::fmt_gb(v.total))
            } else {
                format!("{} {} free of {}", v.mount, crate::res::fmt_gb(v.avail), crate::res::fmt_gb(v.total))
            }
        })
        .collect();
    out.push_str(&format!("Disk: {}\n", vols.join(" · ")));
    let home = crate::home().to_string_lossy().trim_end_matches('/').to_string();
    let entry = read_index().into_iter().find(|e| e.root == home);
    let Some(e) = entry else {
        out.push_str("No disk scan yet — [open Disk](#/r/disk)\n");
        return out;
    };
    let v: Value = std::fs::read(cache_dir().join(&e.file)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let age_h = (now_ms() - e.scanned_at) / 3600_000;
    let age = if age_h < 1 { "< 1 h".to_string() } else if age_h < 48 { format!("{age_h} h") } else { format!("{} days", age_h / 24) };
    let items: Vec<String> = v["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .take(3)
        .map(|f| {
            let p = f["path"].as_str().unwrap_or("");
            let short = p.strip_prefix(&home).map(|r| format!("~{r}")).unwrap_or(p.to_string());
            format!(
                "[{short}](#/r/disk?path={}) {} ({})",
                crate::urlencode(p),
                crate::res::fmt_gb(f["bytes"].as_u64().unwrap_or(0)),
                f["kind"].as_str().unwrap_or("")
            )
        })
        .collect();
    if items.is_empty() {
        out.push_str(&format!("Nothing obvious to reclaim (scan {age} old) · [open Disk](#/r/disk)\n"));
    } else {
        out.push_str(&format!("Worth a look (scan {age} old): {} · [open Disk](#/r/disk)\n", items.join(" · ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use disktree_core::tree::{Node, NodeKind};

    fn dir(name: &str, kids: Vec<Node>) -> Node {
        let mut n = Node::directory(name);
        n.bytes = kids.iter().map(|k| k.bytes).sum();
        n.files = kids.iter().map(|k| k.files).sum();
        n.children = kids;
        n.children.sort_by_key(|k| std::cmp::Reverse(k.bytes));
        n
    }
    fn file(name: &str, b: u64) -> Node {
        Node::entry(name, NodeKind::File, b)
    }

    #[test]
    fn prune_and_lookup() {
        let big = 100 << 20;
        let t = dir("u", vec![
            dir("Work", vec![dir("a", vec![file("x", big)]), dir("b", vec![file("y", 1)]), file("z", 5)]),
            file("f1", 10),
            file("f2", 9),
            file("f3", 8),
        ]);
        let v = prune(&t, 0, 4, 2, 32 << 20);
        assert_eq!(v["c"].as_array().unwrap().len(), 2);
        assert_eq!(v["more"], json!([2, 17]));
        let work = lookup(&v, "/home/u", "/home/u/Work").unwrap();
        assert_eq!(work["c"][0]["n"], "a");
        // small dirs are not expanded: drilling there needs a subtree scan
        assert!(lookup(&v, "/home/u", "/home/u/Work/b").is_none() || lookup(&v, "/home/u", "/home/u/Work/b").unwrap().get("c").is_none());
        assert!(lookup(&v, "/home/u", "/home/u/Work/a").unwrap().get("c").is_some());
        assert!(lookup(&v, "/home/u", "/home/other").is_none());
        assert!(lookup(&v, "/home/u", "/home/uu").is_none());
    }

    #[test]
    fn resolve_paths() {
        let home = crate::home().to_string_lossy().trim_end_matches('/').to_string();
        assert_eq!(resolve("~").unwrap(), home);
        assert_eq!(resolve("~/Work/").unwrap(), format!("{home}/Work"));
        assert!(resolve("/etc").is_none());
        assert!(resolve("~/../x").is_none());
        assert!(resolve(&format!("{home}x")).is_none());
    }

    #[test]
    fn volumes_are_listed() {
        let v = volumes();
        assert!(v.iter().any(|v| v.mount == "/tmp" || v.mount == "/home" || v.mount == "/"));
        assert!(v.iter().all(|v| v.total >= v.avail));
    }

    #[test]
    fn agent_attribution() {
        let h = crate::home().to_string_lossy().trim_end_matches('/').to_string();
        let a = |pane: &str, cwd: String| AgentCwd { pane: pane.into(), title: pane.into(), cwd };
        let agents = vec![
            a("home", h.clone()),
            a("p1", format!("{h}/Work/sheprd")),
            a("p2", format!("{h}/Work/sheprd/server")),
            a("p3", format!("{h}/Work/tries/.claude/worktrees/agent-a3f")),
        ];
        assert_eq!(agent_for(&agents, &format!("{h}/Work/sheprd/server/target"))["pane"], "p2");
        assert_eq!(agent_for(&agents, &format!("{h}/Work/sheprd/web"))["pane"], "p1");
        assert!(agent_for(&agents, &format!("{h}/.cache")).is_null());
        assert_eq!(agent_for(&agents, &format!("{h}/Work/tries/.claude/worktrees"))["pane"], "p3");
        assert!(agent_for(&agents, &format!("{h}/Work")).is_null());
    }

    #[test]
    fn mount_matching() {
        let m = vec![("a".into(), "/".into(), "btrfs".into()), ("b".into(), "/home".into(), "btrfs".into())];
        assert_eq!(mount_of(&m, "/home").unwrap().0, "b");
        assert_eq!(mount_of(&m, "/homer").unwrap().0, "a");
        assert_eq!(mount_of(&m, "/tmp").unwrap().0, "a");
    }
}
