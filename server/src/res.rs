//! Live machine resources (CPU / RAM / GPU per agent), sampled only while polled.
//!
//! Each `GET /api/res` is a lease on a sampling tier (`glance` 5 s, `live` 2 s).
//! A single sampler task runs while a lease is held (or a resource alert is
//! armed, see alerts.rs) and stops 10 s after the last poll, dropping all of its
//! maps. Per tick: one `/proc` walk (stat only), `/proc/stat`, `/proc/meminfo`,
//! `/proc/loadavg`, hwmon, `statvfs`, and `nvidia-smi` (pmon only on `live`).
//! Processes are attributed to agents by herdr's `pane.process_info` shell pid
//! and the process subtree under it; reparented processes are recovered through
//! `HERDR_PANE_ID` in `/proc/<pid>/environ` (read once per process, never sent).

use crate::transcript::now_ms;
use crate::{Shared, gpu, json_err};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};

const MB: u64 = 1 << 20;
const GIB: f64 = (1u64 << 30) as f64;
const HIST: usize = 60;
/// The sampler stops this long after the last poll.
const STOP_MS: i64 = 10_000;
const EXIT_RING: usize = 32;
const EXIT_TTL_MS: i64 = 600_000;
/// Per-pane write history kept for `agent-write` windows.
const WRITE_KEEP_MS: i64 = 30 * 60_000;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Tier {
    /// Alerts only: nobody is looking.
    Watch,
    Glance,
    Live,
}

impl Tier {
    fn name(self) -> &'static str {
        match self {
            Tier::Watch => "watch",
            Tier::Glance => "glance",
            Tier::Live => "live",
        }
    }
    fn interval(self) -> u64 {
        match self {
            Tier::Watch => crate::alerts::interval_ms(),
            Tier::Glance => 5000,
            Tier::Live => 2000,
        }
    }
}

// ---------------------------------------------------------------------------
// /proc parsing (pure)

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Raw {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    pub state: char,
    pub ticks: u64,
    pub threads: u32,
    pub start_ticks: u64,
    pub rss_pages: u64,
    pub kthread: bool,
}

/// One `/proc/<pid>/stat` line. The comm may contain spaces and parens.
pub fn parse_stat(s: &str) -> Option<Raw> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let pid = s[..open].trim().parse().ok()?;
    let comm = s.get(open + 1..close)?.to_string();
    let f: Vec<&str> = s[close + 1..].split_whitespace().collect();
    let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
    Some(Raw {
        pid,
        comm,
        state: f.first()?.chars().next()?,
        ppid: n(1)? as u32,
        kthread: n(6).unwrap_or(0) & 0x0020_0000 != 0,
        ticks: n(11)? + n(12)?,
        threads: n(17).unwrap_or(1) as u32,
        start_ticks: n(19)?,
        rss_pages: n(21).unwrap_or(0),
    })
}

/// `/proc/stat` cpu lines → (total, idle) jiffies; index 0 is the machine.
pub fn parse_cpu_stat(s: &str) -> (Vec<(u64, u64)>, i64) {
    let mut out = Vec::new();
    let mut btime = 0;
    for l in s.lines() {
        if let Some(rest) = l.strip_prefix("cpu") {
            // "cpu  1 2 …" is the machine; "cpu3 1 2 …" a core (skip its id)
            let skip = usize::from(rest.starts_with(|c: char| c.is_ascii_digit()));
            let all: Vec<u64> = rest.split_whitespace().skip(skip).take(8).filter_map(|x| x.parse().ok()).collect();
            let total: u64 = all.iter().sum();
            let idle = all.get(3).copied().unwrap_or(0) + all.get(4).copied().unwrap_or(0);
            out.push((total, idle));
        } else if let Some(b) = l.strip_prefix("btime ") {
            btime = b.trim().parse().unwrap_or(0);
        }
    }
    (out, btime)
}

/// (MemTotal, MemAvailable, SwapTotal, SwapFree) in bytes.
pub fn parse_meminfo(s: &str) -> (u64, u64, u64, u64) {
    let mut v = [0u64; 4];
    for l in s.lines() {
        let key = ["MemTotal:", "MemAvailable:", "SwapTotal:", "SwapFree:"].iter().position(|k| l.starts_with(k));
        if let Some(i) = key {
            v[i] = l.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) * 1024;
        }
    }
    (v[0], v[1], v[2], v[3])
}

fn pane_from_environ(b: &[u8]) -> Option<String> {
    b.split(|c| *c == 0)
        .find_map(|kv| kv.strip_prefix(b"HERDR_PANE_ID="))
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .filter(|v| !v.is_empty())
}

/// `/proc/<pid>/io` → (read_bytes, write_bytes).
fn parse_io(s: &str) -> Option<(u64, u64)> {
    let get = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.trim().parse::<u64>().ok());
    Some((get("read_bytes:")?, get("write_bytes:")?))
}

fn read_small(path: &str, buf: &mut Vec<u8>) -> bool {
    buf.clear();
    std::fs::File::open(path).and_then(|mut f| f.read_to_end(buf)).is_ok()
}

// ---------------------------------------------------------------------------
// Labels, tags, redaction (pure)

fn base(t: &str) -> &str {
    t.rsplit('/').next().unwrap_or(t)
}

fn is_interp(t: &str) -> bool {
    let b = base(t);
    b.starts_with("python")
        || matches!(b, "node" | "bun" | "deno" | "ruby" | "perl" | "java" | "uv" | "uvx" | "bash" | "sh" | "zsh")
}

/// A short label: `python train.py bd1-walk-flat --steps 2e7` → `train.py bd1-walk-flat`,
/// `/usr/bin/foo --x` → `foo --x` cut short. Never an absolute path.
pub fn label(comm: &str, cmd: &str) -> String {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    let Some(&argv0) = toks.first() else { return comm.to_string() };
    let mut out = String::new();
    if is_interp(argv0) && toks.len() > 1 {
        let mut i = 1;
        // uv run [python] script ; node [flags] script
        loop {
            match toks.get(i) {
                Some(&"run") if matches!(base(toks[i - 1]), "uv" | "uvx") => i += 1,
                Some(t) if is_interp(t) && !t.contains('.') => i += 1,
                Some(&"-m") => {
                    if let Some(m) = toks.get(i + 1) {
                        out = m.to_string();
                        i += 1;
                    }
                    break;
                }
                Some(&"-c") => {
                    out = format!("{} -c", base(toks[i - 1]));
                    i = toks.len();
                    break;
                }
                Some(t) if t.starts_with('-') => i += 1,
                _ => break,
            }
        }
        if out.is_empty() {
            match toks.get(i) {
                Some(s) => out = base(s).to_string(),
                None => out = base(argv0).to_string(),
            }
        }
        let rest = &toks[(i + 1).min(toks.len())..];
        let mut args: Vec<&str> = rest.iter().take_while(|t| !t.starts_with('-')).take(2).map(|t| base(t)).collect();
        if args.is_empty() {
            args.extend(rest.iter().take(1).copied());
        }
        for a in args {
            out.push(' ');
            out.push_str(a);
        }
    } else {
        let b = base(argv0).trim_end_matches(':');
        out = if comm.is_empty() || b.starts_with(comm) || (comm.len() >= 15 && comm.starts_with(b)) {
            b.to_string()
        } else {
            comm.to_string()
        };
        if let Some(a) = toks.get(1).filter(|a| !a.starts_with('/') && !a.starts_with('-') && a.len() < 24) {
            out.push(' ');
            out.push_str(a);
        }
    }
    redact(&out).chars().take(60).collect()
}

/// A name for an uninteresting ancestor in a breadcrumb (`uv run`, `bash`).
fn chain_name(comm: &str, cmd: Option<&str>) -> String {
    let toks: Vec<&str> = cmd.unwrap_or("").split_whitespace().collect();
    let Some(a0) = toks.first() else { return comm.to_string() };
    let b = base(a0);
    let b = if b.is_empty() || !(b.starts_with(comm) || comm.len() >= 15) { comm } else { b };
    match toks.get(1) {
        Some(a) if matches!(b, "uv" | "uvx" | "npm" | "pnpm" | "bun" | "cargo" | "npx" | "yarn") && !a.starts_with('-') => {
            format!("{b} {a}")
        }
        _ => b.to_string(),
    }
}

fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
}

/// Contract tags: training | build | server | browser | desktop | agent | shell | game | system | None.
pub fn tag(comm: &str, cmd: &str) -> Option<&'static str> {
    let c = cmd.to_ascii_lowercase();
    let first = cmd.split_whitespace().next().unwrap_or(comm);
    let b = base(first).to_ascii_lowercase();
    let n = comm.to_ascii_lowercase();
    let is = |l: &[&str]| l.contains(&b.as_str()) || l.contains(&n.as_str());
    if words(&c).any(|w| {
        matches!(w, "train" | "training" | "torchrun" | "deepspeed" | "finetune" | "fine_tune" | "rsl_rl" | "ppo" | "isaac")
    }) {
        return Some("training");
    }
    if is(&["claude", "codex", "aider", "gemini", "opencode", "goose"]) {
        return Some("agent");
    }
    if is(&["cargo", "rustc", "cc1", "cc1plus", "gcc", "g++", "clang", "ld", "lld", "mold", "make", "ninja", "cmake", "tsc", "esbuild", "webpack", "rollup", "build-script-build"])
        || c.contains("vite build")
        || c.contains("go build")
    {
        return Some("build");
    }
    if is(&["chrome", "chromium", "firefox", "brave", "chrome-headless-shell", "headless_shell", "zen", "librewolf", "vivaldi"])
        || c.contains("/chrome/chrome")
        || c.contains("chromium")
    {
        return Some("browser");
    }
    if is(&["steam", "steamwebhelper", "proton", "wine", "wine64", "wineserver", "gamescope", "reaper"]) || c.contains("/steamapps/common/") {
        return Some("game");
    }
    if is(&["hyprland", "xwayland", "quickshell", "waybar", "mako", "swaync", "hyprpaper", "hypridle", "hyprlock", "walker", "herdr", "tmux", "foot", "alacritty", "kitty", "ghostty", "gnome-shell", "kwin_wayland", "plasmashell", "pipewire", "pipewire-pulse", "wireplumber", "swayosd-server", "uwsm", "elephant"])
        || n.starts_with("xdg-desktop-por")
        || n.starts_with("voxtype")
    {
        return Some("desktop");
    }
    if is(&["uvicorn", "gunicorn", "nginx", "postgres", "redis-server", "ollama", "llama-server", "vllm", "tensorboard", "sheprd", "sheprd", "caddy", "jupyter-lab"])
        || words(&c).any(|w| matches!(w, "serve" | "server" | "dev"))
    {
        return Some("server");
    }
    if is(&["bash", "sh", "zsh", "fish", "dash", "nu", "screen"]) {
        return Some("shell");
    }
    if n.starts_with("systemd") || is(&["dbus-daemon", "dbus-broker", "sshd", "tailscaled", "polkitd", "udisksd", "upowerd", "cupsd", "agetty", "login", "sudo"]) {
        return Some("system");
    }
    None
}

/// Masks secrets in a command line: values after `token|key|secret|pass|password|auth`
/// followed by `=` or `:`, and any path segment of ≥ 32 `[A-Za-z0-9+_-]` characters
/// (paths split at `/` so they stay readable). Cut to 300 chars.
pub fn redact(cmd: &str) -> String {
    let lower = cmd.to_ascii_lowercase();
    let mut out = String::with_capacity(cmd.len());
    let bytes = cmd.as_bytes();
    let mut i = 0;
    'outer: while i < bytes.len() {
        for k in ["password", "secret", "token", "pass", "auth", "key"] {
            if lower[i..].starts_with(k) && lower.is_char_boundary(i + k.len()) {
                let j = i + k.len();
                if matches!(bytes.get(j), Some(b'=') | Some(b':')) {
                    let mut e = j + 1;
                    while e < bytes.len() && !bytes[e].is_ascii_whitespace() {
                        e += 1;
                    }
                    if e > j + 1 {
                        out.push_str(&cmd[i..=j]);
                        out.push('…');
                        i = e;
                        continue 'outer;
                    }
                }
            }
        }
        let ch = cmd[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    // long opaque runs
    let tok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '-');
    let mut res = String::with_capacity(out.len());
    let mut run = String::new();
    for c in out.chars().chain(std::iter::once('\u{0}')) {
        if tok(c) {
            run.push(c);
            continue;
        }
        if run.chars().count() >= 32 {
            res.push('…');
        } else {
            res.push_str(&run);
        }
        run.clear();
        if c != '\u{0}' {
            res.push(c);
        }
    }
    if res.chars().count() > 300 {
        res = res.chars().take(299).collect::<String>() + "…";
    }
    res
}

/// The first two meaningful tokens of a job command, after `cd …&&`, `nohup`,
/// `timeout N`, `uv run`, `env`, `VAR=x` prefixes.
pub fn job_tokens(cmd: &str) -> Vec<String> {
    let line = cmd.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let seg = line.split("&&").map(str::trim).find(|s| !s.starts_with("cd ") && *s != "cd" && !s.is_empty()).unwrap_or("");
    let toks: Vec<String> = seg.split_whitespace().map(|t| t.trim_matches(|c| c == '\'' || c == '"').to_string()).collect();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i].as_str();
        if t == "nohup" || t == "env" || t == "exec" || t == "setsid" || t == "stdbuf" {
            i += 1;
        } else if t == "timeout" {
            i += 1;
            while i < toks.len() && toks[i].starts_with('-') {
                let takes_value = matches!(toks[i].as_str(), "-s" | "-k" | "--signal" | "--kill-after");
                i += if takes_value { 2 } else { 1 };
            }
            i += 1; // duration
        } else if (t == "uv" || t == "uvx") && toks.get(i + 1).map(String::as_str) == Some("run") {
            i += 2;
        } else if t.contains('=') && !t.starts_with('-') && t.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            i += 1;
        } else {
            break;
        }
    }
    let mut out: Vec<String> = toks.into_iter().skip(i).take(2).collect();
    if let Some(f) = out.first_mut() {
        *f = base(f).to_string();
    }
    out
}

// ---------------------------------------------------------------------------
// The process table and attribution

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Group {
    #[default]
    Other,
    Agent(String),
    Closed(String),
}

impl Group {
    pub fn pane(&self) -> Option<&str> {
        match self {
            Group::Agent(p) | Group::Closed(p) => Some(p),
            Group::Other => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct P {
    pub pid: u32,
    pub ppid: u32,
    pub start: i64,
    pub start_ticks: u64,
    pub comm: String,
    pub state: char,
    pub threads: u32,
    pub rss: u64,
    pub cpu: f64,
    pub cmd: Option<Arc<str>>,
    pub gpu_mem: u64,
    pub gpu_util: Option<f64>,
    pub group: Group,
    pub detached: bool,
    pub job: Option<String>,
    pub wbps: Option<f64>,
}

impl P {
    pub fn notable(&self) -> bool {
        self.cpu >= 1.0 || self.rss >= 100 * MB || self.gpu_mem > 0 || self.job.is_some()
    }
    /// Worth an environ read when not attributed by the tree.
    fn env_worthy(&self) -> bool {
        self.cpu >= 5.0 || self.rss >= 200 * MB || self.gpu_mem > 0
    }
    pub fn label(&self) -> String {
        label(&self.comm, self.cmd.as_deref().unwrap_or(""))
    }
    pub fn tag(&self) -> Option<&'static str> {
        tag(&self.comm, self.cmd.as_deref().unwrap_or(""))
    }
    pub fn key(&self) -> String {
        format!("{}@{}", self.pid, self.start)
    }
}

/// Assigns every process a group. `shells`: shell pid → agent pane. `env` reads
/// `HERDR_PANE_ID` for a process (cached by the caller). `agent_panes`/`live_panes`:
/// live agent panes / every live pane (agents and plain shells).
pub fn attribute(
    procs: &mut [P],
    shells: &HashMap<u32, String>,
    env: &mut dyn FnMut(&P) -> Option<String>,
    agent_panes: &HashSet<String>,
    live_panes: &HashSet<String>,
) {
    let idx: HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let mut kids: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, p) in procs.iter().enumerate() {
        kids.entry(p.ppid).or_default().push(i);
    }
    let spread = |procs: &mut [P], root: usize, g: Group, detached: bool, only_other: bool| {
        let mut stack = vec![root];
        while let Some(i) = stack.pop() {
            if only_other && procs[i].group != Group::Other && i != root {
                continue;
            }
            procs[i].group = g.clone();
            procs[i].detached = detached;
            let (pid, start) = (procs[i].pid, procs[i].start_ticks);
            for &c in kids.get(&pid).map(Vec::as_slice).unwrap_or(&[]) {
                // pid-reuse guard: a child never starts before its parent
                if procs[c].start_ticks >= start && c != i {
                    stack.push(c);
                }
            }
        }
    };
    for (spid, pane) in shells {
        if let Some(&i) = idx.get(spid) {
            spread(procs, i, Group::Agent(pane.clone()), false, false);
        }
    }
    // Reparented processes: HERDR_PANE_ID for the big unattributed ones.
    let cands: Vec<usize> = (0..procs.len()).filter(|&i| procs[i].group == Group::Other && procs[i].env_worthy()).collect();
    for i in cands {
        if procs[i].group != Group::Other {
            continue; // claimed by an ancestor found earlier
        }
        let Some(pane) = env(&procs[i]) else { continue };
        let g = if agent_panes.contains(&pane) {
            Group::Agent(pane)
        } else if !live_panes.contains(&pane) {
            Group::Closed(pane)
        } else {
            continue; // a live plain shell pane: not an agent's
        };
        // climb to the topmost unattributed ancestor that is also from this pane? keep it simple:
        spread(procs, i, g, true, true);
    }
}

#[derive(Clone, Debug, Default)]
pub struct Roll {
    pub pane: String,
    pub cpu: f64,
    pub rss: u64,
    pub gpu_mem: u64,
    pub gpu_util: Option<f64>,
    pub procs: usize,
    pub wbps: f64,
    pub wrote10m: u64,
    pub notable: Vec<&'static str>,
    pub jobs: Vec<String>,
    /// cpu of processes that belong to a running job's subtree
    pub job_cpu: f64,
}

fn roll_add(r: &mut Roll, p: &P) {
    r.cpu += p.cpu;
    r.rss += p.rss;
    r.gpu_mem += p.gpu_mem;
    if let Some(u) = p.gpu_util {
        r.gpu_util = Some((r.gpu_util.unwrap_or(0.0) + u).min(100.0));
    }
    r.procs += 1;
}

/// Display rows for one group: notable processes as a compressed tree (agent
/// groups) or a flat merged list (`flat`, the Other group). `limit` keeps the
/// union of the top `limit` by cpu, rss and VRAM.
pub fn group_rows(procs: &[P], members: &[usize], flat: bool, limit: usize) -> Vec<Value> {
    let set: HashSet<usize> = members.iter().copied().collect();
    let idx: HashMap<u32, usize> = members.iter().map(|&i| (procs[i].pid, i)).collect();
    let notable: Vec<usize> = members.iter().copied().filter(|&i| procs[i].notable()).collect();
    let nset: HashSet<usize> = notable.iter().copied().collect();
    struct Row {
        i: usize,
        parent: Option<usize>,
        depth: usize,
        chain: Vec<String>,
        n: usize,
        cpu: f64,
        rss: u64,
        gpu: u64,
        gutil: Option<f64>,
        pid: u32,
        label: String,
    }
    let mut rows: Vec<Row> = Vec::new();
    for &i in &notable {
        let mut chain = Vec::new();
        let mut parent = None;
        let mut depth = 0;
        if !flat {
            let mut cur = procs[i].ppid;
            let mut guard = 0;
            while let Some(&a) = idx.get(&cur) {
                guard += 1;
                if guard > 64 || !set.contains(&a) {
                    break;
                }
                if nset.contains(&a) {
                    if parent.is_none() {
                        parent = Some(a);
                    }
                    depth += 1;
                } else if parent.is_none() {
                    chain.push(chain_name(&procs[a].comm, procs[a].cmd.as_deref()));
                }
                cur = procs[a].ppid;
            }
            chain.reverse();
            if chain.len() > 3 {
                chain.drain(..chain.len() - 3);
            }
        }
        let p = &procs[i];
        rows.push(Row {
            i,
            parent,
            depth: depth.min(2),
            chain,
            n: 1,
            cpu: p.cpu,
            rss: p.rss,
            gpu: p.gpu_mem,
            gutil: p.gpu_util,
            pid: p.pid,
            label: p.label(),
        });
    }
    // Merge same-label siblings without notable children (`node ×4`).
    let has_kids: HashSet<usize> = rows.iter().filter_map(|r| r.parent).collect();
    let mut merged: Vec<Row> = Vec::new();
    let mut at: HashMap<(Option<usize>, String), usize> = HashMap::new();
    for r in rows {
        let key = (if flat { None } else { r.parent }, r.label.clone());
        if !has_kids.contains(&r.i) && procs[r.i].job.is_none() {
            if let Some(&m) = at.get(&key) {
                let t = &mut merged[m];
                if r.cpu + r.rss as f64 / 1e9 > t.cpu / t.n as f64 + t.rss as f64 / 1e9 / t.n as f64 {
                    t.pid = r.pid;
                    t.i = r.i;
                }
                t.n += 1;
                t.cpu += r.cpu;
                t.rss += r.rss;
                t.gpu += r.gpu;
                t.gutil = match (t.gutil, r.gutil) {
                    (Some(a), Some(b)) => Some((a + b).min(100.0)),
                    (a, b) => a.or(b),
                };
                continue;
            }
            at.insert(key, merged.len());
        }
        merged.push(r);
    }
    // union of top-`limit` by each metric
    let mut keep: HashSet<usize> = HashSet::new();
    let mut ord: Vec<usize> = (0..merged.len()).collect();
    ord.sort_by(|&a, &b| merged[b].cpu.total_cmp(&merged[a].cpu));
    keep.extend(ord.iter().take(limit));
    ord.sort_by_key(|&a| std::cmp::Reverse(merged[a].rss));
    keep.extend(ord.iter().take(limit));
    ord.sort_by_key(|&a| std::cmp::Reverse(merged[a].gpu));
    keep.extend(ord.iter().take(limit).filter(|&&a| merged[a].gpu > 0));
    // order: DFS by display parent, siblings by cpu desc; flat → by cpu desc
    let score = |r: &Row| r.cpu + r.gpu as f64 / 1e8 + r.rss as f64 / 1e9;
    let mut order: Vec<usize> = Vec::new();
    if flat {
        order = (0..merged.len()).collect();
        order.sort_by(|&a, &b| score(&merged[b]).total_cmp(&score(&merged[a])));
    } else {
        let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
        let pos: HashMap<usize, usize> = merged.iter().enumerate().map(|(k, r)| (r.i, k)).collect();
        for (k, r) in merged.iter().enumerate() {
            let parent = r.parent.filter(|p| pos.contains_key(p)).map(|p| pos[&p]);
            children.entry(parent).or_default().push(k);
        }
        for v in children.values_mut() {
            v.sort_by(|&a, &b| score(&merged[b]).total_cmp(&score(&merged[a])));
        }
        let mut stack: Vec<usize> = children.get(&None).cloned().unwrap_or_default();
        stack.reverse();
        let mut seen = HashSet::new();
        while let Some(k) = stack.pop() {
            if !seen.insert(k) {
                continue;
            }
            order.push(k);
            if let Some(c) = children.get(&Some(k)) {
                stack.extend(c.iter().rev());
            }
        }
    }
    order
        .into_iter()
        .filter(|k| keep.contains(k))
        .map(|k| {
            let r = &merged[k];
            let p = &procs[r.i];
            json!({
                "pid": r.pid, "ppid": p.ppid, "start": p.start, "name": p.comm, "label": r.label, "tag": p.tag(),
                "cpu": round1(r.cpu), "rss": r.rss, "gpuMem": r.gpu, "gpuUtil": r.gutil,
                "n": r.n, "depth": r.depth, "chain": r.chain, "detached": p.detached, "job": p.job,
            })
        })
        .collect()
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

pub fn proc_json(p: &P) -> Value {
    json!({
        "pid": p.pid, "ppid": p.ppid, "start": p.start, "name": p.comm, "label": p.label(), "tag": p.tag(),
        "cpu": round1(p.cpu), "rss": p.rss, "gpuMem": p.gpu_mem, "gpuUtil": p.gpu_util,
        "n": 1, "depth": 0, "chain": [], "detached": p.detached, "job": p.job,
    })
}

/// Job join: the topmost non-wrapper process in the pane's subtree whose cmdline
/// contains the job command's first two tokens.
pub fn match_job(procs: &[P], members: &[usize], cmd: &str) -> Option<usize> {
    let toks = job_tokens(cmd);
    if toks.is_empty() {
        return None;
    }
    let wrapper = |p: &P| {
        matches!(p.comm.as_str(), "bash" | "sh" | "zsh" | "dash" | "fish" | "timeout" | "nohup" | "uv" | "uvx" | "env" | "setsid" | "stdbuf" | "script")
    };
    let cands: Vec<usize> = members
        .iter()
        .copied()
        .filter(|&i| {
            let c = procs[i].cmd.as_deref().unwrap_or("");
            !wrapper(&procs[i]) && toks.iter().all(|t| c.contains(t.as_str()))
        })
        .collect();
    let cset: HashSet<u32> = cands.iter().map(|&i| procs[i].pid).collect();
    cands
        .iter()
        .copied()
        .filter(|&i| !cset.contains(&procs[i].ppid))
        .max_by(|&a, &b| (procs[a].cpu + procs[a].gpu_mem as f64 / 1e8).total_cmp(&(procs[b].cpu + procs[b].gpu_mem as f64 / 1e8)))
}

/// Indexes of `root` and all its descendants.
fn subtree(procs: &[P], kids: &HashMap<u32, Vec<usize>>, root: usize) -> Vec<usize> {
    let mut out = vec![];
    let mut stack = vec![root];
    while let Some(i) = stack.pop() {
        if out.len() > 4096 {
            break;
        }
        out.push(i);
        if let Some(c) = kids.get(&procs[i].pid) {
            stack.extend(c.iter().copied().filter(|&c| c != i));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Headline (pure)

pub fn fmt_gb(b: u64) -> String {
    let g = b as f64 / GIB;
    if g >= 10.0 { format!("{g:.0} GB") } else if g >= 0.95 { format!("{g:.1} GB") } else { format!("{} MB", (b / MB).max(1)) }
}

pub struct AgentInfo {
    pub pane: String,
    pub title: String,
}

fn place(p: &P, titles: &HashMap<String, String>) -> String {
    match &p.group {
        Group::Agent(pane) => match titles.get(pane) {
            Some(t) => format!(" in {t} ({pane})"),
            None => format!(" ({pane})"),
        },
        Group::Closed(pane) => format!(" from closed tab {pane}"),
        Group::Other => " (not from an agent)".to_string(),
    }
}

fn who(p: &P, titles: &HashMap<String, String>) -> String {
    format!("{}{}", p.label(), place(p, titles))
}

/// UX §Q4: the first matching rule wins.
pub fn headline(snap: &Snap, alerts: &[Value]) -> Value {
    let mk = |text: String, level: &str, pane: Option<&str>, pid: Option<u32>| {
        json!({"text": text, "level": level, "pane": pane, "pid": pid})
    };
    if let Some(a) = alerts.first() {
        return mk(a["text"].as_str().unwrap_or("").to_string(), "alert", a["pane"].as_str(), a["pid"].as_u64().map(|p| p as u32));
    }
    let titles: HashMap<String, String> = snap.agents.iter().map(|a| (a.pane.clone(), a.title.clone())).collect();
    // closed-tab orphan
    if let Some(p) = snap
        .procs
        .iter()
        .filter(|p| matches!(p.group, Group::Closed(_)) && (p.gpu_mem > 0 || p.cpu >= 50.0))
        .max_by_key(|p| (p.gpu_mem, p.cpu as u64))
    {
        let pane = p.group.pane().unwrap_or("");
        let what = if p.gpu_mem > 0 { format!("{} VRAM", fmt_gb(p.gpu_mem)) } else { format!("{:.0}% CPU", p.cpu) };
        return mk(format!("Orphan from closed tab {pane} still holds {what}"), "warn", Some(pane), Some(p.pid));
    }
    let util = snap.gpus.first().and_then(|g| g.util).unwrap_or(0.0);
    let top_gpu = snap.procs.iter().filter(|p| p.gpu_mem > 0).max_by_key(|p| p.gpu_mem);
    if util >= 30.0 || top_gpu.is_some_and(|p| p.gpu_mem >= 1 << 30) {
        let (text, pane, pid) = match top_gpu {
            Some(p) => (format!("GPU {util:.0}% · {}", who(p, &titles)), p.group.pane(), Some(p.pid)),
            None => (format!("GPU {util:.0}%"), None, None),
        };
        return mk(text, "busy", pane, pid);
    }
    let top_cpu = snap.procs.iter().max_by(|a, b| a.cpu.total_cmp(&b.cpu));
    if snap.cpu_pct >= 60.0 {
        let (text, pane, pid) = match top_cpu {
            Some(p) => (format!("CPU {:.0}% — {}", snap.cpu_pct, who(p, &titles)), p.group.pane(), Some(p.pid)),
            None => (format!("CPU {:.0}%", snap.cpu_pct), None, None),
        };
        return mk(text, "busy", pane, pid);
    }
    if snap.mem_total > 0 && snap.mem_avail < 2 << 30 {
        let top = snap.procs.iter().max_by_key(|p| p.rss);
        let text = match top {
            Some(p) => format!("RAM tight — {} free; biggest: {} {}", fmt_gb(snap.mem_avail), p.label(), fmt_gb(p.rss)),
            None => format!("RAM tight — {} free", fmt_gb(snap.mem_avail)),
        };
        return mk(text, "warn", top.and_then(|p| p.group.pane()), top.map(|p| p.pid));
    }
    // one process pinning a core (not in the UX rules: "Quiet" would be wrong)
    if let Some(p) = top_cpu.filter(|p| p.cpu >= 50.0) {
        return mk(format!("{} at {:.0}% CPU{}", p.label(), p.cpu, place(p, &titles)), "busy", p.group.pane(), Some(p.pid));
    }
    if let Some(p) = top_cpu.filter(|p| p.cpu >= 5.0) {
        return mk(format!("Light load — top: {} {:.0}% CPU", p.label(), p.cpu), "ok", p.group.pane(), Some(p.pid));
    }
    mk("Quiet — nothing above 5 %".into(), "ok", None, None)
}

// ---------------------------------------------------------------------------
// The sampler

pub struct JobRef {
    pub id: String,
    pub pane: String,
    pub cmd: String,
    pub desc: Option<String>,
    pub tid: String,
    pub i: usize,
}

/// One tick's result. Rendered per request (tier, pane).
#[derive(Default)]
pub struct Snap {
    pub t: i64,
    pub tier: Option<Tier>,
    pub procs: Vec<P>,
    pub agents: Vec<AgentInfo>,
    pub rolls: Vec<Roll>,
    pub closed: Vec<Roll>,
    pub other: Roll,
    pub cpu_pct: f64,
    pub cores: Vec<f64>,
    pub ncpu: usize,
    pub temp: Option<f64>,
    pub load: [f64; 3],
    pub mem_total: u64,
    pub mem_avail: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub gpus: Vec<gpu::Gpu>,
    pub gpu_err: Option<String>,
    pub volumes: Vec<crate::disk::Volume>,
    pub hist: [Vec<f64>; 4],
    pub jobs: Vec<JobRef>,
    /// job id → index of the matched process; subtree totals
    pub job_res: HashMap<String, (usize, f64, u64, u64, Option<f64>)>,
    pub alerts: Vec<Value>,
    pub headline: Value,
    /// pane → bytes written in the last N ms (for agent-write rules)
    pub writes: HashMap<String, VecDeque<(i64, u64)>>,
}

impl Snap {
    pub fn find(&self, pid: u32) -> Option<&P> {
        self.procs.iter().find(|p| p.pid == pid)
    }
    pub fn title(&self, pane: &str) -> Option<&str> {
        self.agents.iter().find(|a| a.pane == pane).map(|a| a.title.as_str())
    }
    pub fn wrote_in(&self, pane: &str, ms: i64) -> u64 {
        self.writes.get(pane).map(|r| r.iter().filter(|(t, _)| self.t - t <= ms).map(|(_, b)| *b).sum()).unwrap_or(0)
    }
}

struct Ctx {
    tier: Tier,
    agents: Vec<AgentInfo>,
    live_panes: HashSet<String>,
    shells: HashMap<u32, String>,
    jobs: Vec<JobRef>,
    gpu: gpu::Sample,
}

pub struct Sampler {
    clk: u64,
    page: u64,
    boot_ms: i64,
    armed_at: i64,
    prev: HashMap<u32, (u64, u64)>,
    prev_at: Option<Instant>,
    prev_cpu: Vec<(u64, u64)>,
    cmd: HashMap<u32, (u64, String, Arc<str>)>,
    env: HashMap<u32, (u64, Option<String>)>,
    io: HashMap<u32, (u64, u64)>,
    writes: HashMap<String, VecDeque<(i64, u64)>>,
    pane_cpu_prev: HashMap<String, f64>,
    hist: [VecDeque<f64>; 4],
    notable_prev: HashMap<(u32, u64), P>,
    shells: HashMap<String, u32>,
    shells_at: i64,
    gpu_last: Option<gpu::Sample>,
    gpu_at: i64,
    pmon_at: i64,
    apps_at: i64,
    buf: Vec<u8>,
}

fn sysconf(name: i32, dflt: u64) -> u64 {
    let v = unsafe { libc::sysconf(name) };
    if v > 0 { v as u64 } else { dflt }
}

impl Sampler {
    pub fn new() -> Sampler {
        let (_, btime) = parse_cpu_stat(&std::fs::read_to_string("/proc/stat").unwrap_or_default());
        Sampler {
            clk: sysconf(libc::_SC_CLK_TCK, 100),
            page: sysconf(libc::_SC_PAGESIZE, 4096),
            boot_ms: btime * 1000,
            armed_at: now_ms(),
            prev: HashMap::new(),
            prev_at: None,
            prev_cpu: Vec::new(),
            cmd: HashMap::new(),
            env: HashMap::new(),
            io: HashMap::new(),
            writes: HashMap::new(),
            pane_cpu_prev: HashMap::new(),
            hist: Default::default(),
            notable_prev: HashMap::new(),
            shells: HashMap::new(),
            shells_at: 0,
            gpu_last: None,
            gpu_at: 0,
            pmon_at: 0,
            apps_at: 0,
            buf: Vec::with_capacity(1024),
        }
    }

    fn read_raw(&mut self) -> Vec<Raw> {
        let mut out = Vec::with_capacity(400);
        let Ok(rd) = std::fs::read_dir("/proc") else { return out };
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(pid) = name.to_str().filter(|s| s.bytes().all(|b| b.is_ascii_digit())) else { continue };
            if !read_small(&format!("/proc/{pid}/stat"), &mut self.buf) {
                continue;
            }
            if let Some(r) = parse_stat(&String::from_utf8_lossy(&self.buf)) {
                if !r.kthread && r.pid != 2 && r.ppid != 2 {
                    out.push(r);
                }
            }
        }
        out
    }

    /// Only the tick baseline (first arm: a second sample follows 250 ms later).
    fn baseline(&mut self) {
        let raw = self.read_raw();
        self.prev = raw.iter().map(|r| (r.pid, (r.start_ticks, r.ticks))).collect();
        self.prev_cpu = parse_cpu_stat(&std::fs::read_to_string("/proc/stat").unwrap_or_default()).0;
        self.prev_at = Some(Instant::now());
    }

    fn cmdline(&mut self, pid: u32, start: u64, comm: &str) -> Option<Arc<str>> {
        if let Some((s, c, cmd)) = self.cmd.get(&pid) {
            if *s == start && c == comm {
                return Some(cmd.clone());
            }
        }
        if !read_small(&format!("/proc/{pid}/cmdline"), &mut self.buf) {
            return None;
        }
        let mut s = String::from_utf8_lossy(&self.buf).replace('\0', " ");
        s.truncate(s.trim_end().len());
        if s.len() > 2048 {
            let mut cut = 2048;
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            s.truncate(cut);
        }
        let a: Arc<str> = s.into();
        self.cmd.insert(pid, (start, comm.to_string(), a.clone()));
        Some(a)
    }

    fn sample(&mut self, ctx: Ctx) -> (Snap, Vec<P>) {
        let now = now_ms();
        let at = Instant::now();
        let dt = self.prev_at.map(|p| at.duration_since(p).as_secs_f64()).unwrap_or(0.0).max(0.05);
        let raw = self.read_raw();
        let (cpu_now, _) = parse_cpu_stat(&std::fs::read_to_string("/proc/stat").unwrap_or_default());
        let clk = self.clk as f64;
        let pm: HashMap<u32, &gpu::PmonRow> =
            ctx.gpu.pmon.iter().flatten().map(|r| (r.pid, r)).collect();
        let mut procs: Vec<P> = Vec::with_capacity(raw.len());
        let mut next_prev = HashMap::with_capacity(raw.len());
        for r in raw {
            let start = self.boot_ms + (r.start_ticks * 1000 / self.clk) as i64;
            let cpu = match self.prev.get(&r.pid) {
                Some(&(s, t)) if s == r.start_ticks => r.ticks.saturating_sub(t) as f64 / clk / dt * 100.0,
                _ if self.prev_at.is_some() => {
                    let age = ((now - start) as f64 / 1000.0).clamp(0.05, dt);
                    r.ticks as f64 / clk / age * 100.0
                }
                _ => 0.0,
            };
            next_prev.insert(r.pid, (r.start_ticks, r.ticks));
            procs.push(P {
                pid: r.pid,
                ppid: r.ppid,
                start,
                start_ticks: r.start_ticks,
                comm: r.comm,
                state: r.state,
                threads: r.threads,
                rss: r.rss_pages * self.page,
                cpu: cpu.min(100.0 * 256.0),
                gpu_mem: ctx.gpu.mem_of(r.pid),
                gpu_util: pm.get(&r.pid).and_then(|r| r.sm),
                ..Default::default()
            });
        }
        self.prev = next_prev;
        self.prev_at = Some(at);

        // attribution
        let agent_panes: HashSet<String> = ctx.agents.iter().map(|a| a.pane.clone()).collect();
        {
            let env_cache = &mut self.env;
            let buf = &mut self.buf;
            let mut env = |p: &P| -> Option<String> {
                if let Some((s, v)) = env_cache.get(&p.pid) {
                    if *s == p.start_ticks {
                        return v.clone();
                    }
                }
                let v = if read_small(&format!("/proc/{}/environ", p.pid), buf) { pane_from_environ(buf) } else { None };
                env_cache.insert(p.pid, (p.start_ticks, v.clone()));
                v
            };
            attribute(&mut procs, &ctx.shells, &mut env, &agent_panes, &ctx.live_panes);
        }
        // cmdlines for agent/closed members and notable others
        for i in 0..procs.len() {
            let need = procs[i].group != Group::Other || procs[i].notable();
            if need {
                let (pid, st, comm) = (procs[i].pid, procs[i].start_ticks, procs[i].comm.clone());
                procs[i].cmd = self.cmdline(pid, st, &comm);
            }
        }
        let alive: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.start_ticks)).collect();
        self.cmd.retain(|pid, (s, _, _)| alive.get(pid) == Some(s));
        self.env.retain(|pid, (s, _)| alive.get(pid) == Some(s));

        let mut kids: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, p) in procs.iter().enumerate() {
            kids.entry(p.ppid).or_default().push(i);
        }
        let mut members: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, p) in procs.iter().enumerate() {
            if let Some(pane) = p.group.pane() {
                members.entry(pane.to_string()).or_default().push(i);
            }
        }

        // job join
        let mut job_res = HashMap::new();
        let mut in_job: HashSet<usize> = HashSet::new();
        for j in &ctx.jobs {
            let Some(m) = members.get(&j.pane) else { continue };
            if let Some(i) = match_job(&procs, m, &j.cmd) {
                procs[i].job = Some(j.id.clone());
                let sub = subtree(&procs, &kids, i);
                let (mut c, mut r, mut g, mut u) = (0.0, 0u64, 0u64, None::<f64>);
                for &k in &sub {
                    c += procs[k].cpu;
                    r += procs[k].rss;
                    g += procs[k].gpu_mem;
                    if let Some(x) = procs[k].gpu_util {
                        u = Some((u.unwrap_or(0.0) + x).min(100.0));
                    }
                    in_job.insert(k);
                }
                job_res.insert(j.id.clone(), (i, c, r, g, u));
            }
        }

        // writes: notable members and agent shells' direct children
        let shell_pids: HashSet<u32> = ctx.shells.keys().copied().collect();
        let mut next_io = HashMap::new();
        let mut pane_delta: HashMap<String, u64> = HashMap::new();
        for p in procs.iter_mut() {
            let Some(pane) = p.group.pane() else { continue };
            if !(p.notable() || shell_pids.contains(&p.ppid)) {
                continue;
            }
            let Ok(s) = std::fs::read_to_string(format!("/proc/{}/io", p.pid)) else { continue };
            let Some((_, w)) = parse_io(&s) else { continue };
            let base = match self.io.get(&p.pid) {
                Some(&(st, b)) if st == p.start_ticks => Some(b),
                _ if p.start >= self.armed_at => Some(0),
                _ => None,
            };
            if let Some(b) = base {
                let d = w.saturating_sub(b);
                p.wbps = Some(d as f64 / dt);
                *pane_delta.entry(pane.to_string()).or_default() += d;
            }
            next_io.insert(p.pid, (p.start_ticks, w));
        }
        self.io = next_io;
        for (pane, d) in pane_delta {
            self.writes.entry(pane).or_default().push_back((now, d));
        }
        for r in self.writes.values_mut() {
            while r.front().is_some_and(|(t, _)| now - t > WRITE_KEEP_MS) {
                r.pop_front();
            }
        }
        self.writes.retain(|p, r| !r.is_empty() && (agent_panes.contains(p) || members.contains_key(p)));

        // machine
        let mut cores = Vec::new();
        for (k, &(tot, idle)) in cpu_now.iter().enumerate() {
            let (pt, pi) = self.prev_cpu.get(k).copied().unwrap_or((0, 0));
            let dtot = tot.saturating_sub(pt);
            let didle = idle.saturating_sub(pi);
            cores.push(if dtot == 0 { 0.0 } else { round1((dtot - didle.min(dtot)) as f64 * 100.0 / dtot as f64) });
        }
        self.prev_cpu = cpu_now;
        let cpu_pct = if cores.is_empty() { 0.0 } else { cores.remove(0) };
        let (mem_total, mem_avail, swap_total, swap_free) =
            parse_meminfo(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default());
        let la = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let mut load = [0.0; 3];
        for (k, v) in la.split_whitespace().take(3).enumerate() {
            load[k] = v.parse().unwrap_or(0.0);
        }
        let g0 = ctx.gpu.gpus.first();
        let push = |h: &mut VecDeque<f64>, v: f64| {
            h.push_back(v);
            if h.len() > HIST {
                h.pop_front();
            }
        };
        push(&mut self.hist[0], cpu_pct);
        push(&mut self.hist[1], mem_total.saturating_sub(mem_avail) as f64);
        if let Some(g) = g0 {
            push(&mut self.hist[2], g.util.unwrap_or(0.0));
            push(&mut self.hist[3], g.mem_used as f64);
        }

        // rollups
        let mut rolls: Vec<Roll> = Vec::new();
        let mut closed: HashMap<String, Roll> = HashMap::new();
        let mut other = Roll::default();
        let mut by_pane: HashMap<String, Roll> = ctx.agents.iter().map(|a| (a.pane.clone(), Roll { pane: a.pane.clone(), ..Default::default() })).collect();
        for (i, p) in procs.iter().enumerate() {
            let r = match &p.group {
                Group::Agent(pane) => by_pane.entry(pane.clone()).or_insert_with(|| Roll { pane: pane.clone(), ..Default::default() }),
                Group::Closed(pane) => closed.entry(pane.clone()).or_insert_with(|| Roll { pane: pane.clone(), ..Default::default() }),
                Group::Other => &mut other,
            };
            roll_add(r, p);
            if in_job.contains(&i) {
                r.job_cpu += p.cpu;
            }
            if let Some(w) = p.wbps {
                r.wbps += w;
            }
            if let Some(j) = &p.job {
                r.jobs.push(j.clone());
            }
        }
        for a in &ctx.agents {
            if let Some(mut r) = by_pane.remove(&a.pane) {
                r.wrote10m = self
                    .writes
                    .get(&a.pane)
                    .map(|q| q.iter().filter(|(t, _)| now - t <= 600_000).map(|(_, b)| *b).sum())
                    .unwrap_or(0);
                let prev_cpu = self.pane_cpu_prev.get(&a.pane).copied().unwrap_or(0.0);
                if r.gpu_mem >= 256 * MB {
                    r.notable.push("gpu");
                }
                if r.cpu >= 50.0 && prev_cpu >= 50.0 {
                    r.notable.push("cpu");
                }
                if r.rss >= 2 << 30 {
                    r.notable.push("rss");
                }
                if r.wbps >= 20e6 {
                    r.notable.push("write");
                }
                rolls.push(r);
            }
        }
        self.pane_cpu_prev = rolls.iter().map(|r| (r.pane.clone(), r.cpu)).collect();
        rolls.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        let mut closed: Vec<Roll> = closed.into_values().collect();
        closed.sort_by_key(|r| std::cmp::Reverse(r.gpu_mem));

        // exits of notable processes
        let mut exited = Vec::new();
        let mut next_notable = HashMap::new();
        for p in &procs {
            if p.notable() || p.group != Group::Other && p.rss >= 50 * MB {
                next_notable.insert((p.pid, p.start_ticks), p.clone());
            }
        }
        for (k, p) in self.notable_prev.drain() {
            if alive.get(&k.0) != Some(&k.1) {
                exited.push(p);
            }
        }
        self.notable_prev = next_notable;

        let snap = Snap {
            t: now,
            tier: Some(ctx.tier),
            ncpu: cores.len(),
            cores,
            cpu_pct,
            temp: cpu_temp(),
            load,
            mem_total,
            mem_avail,
            swap_total,
            swap_used: swap_total.saturating_sub(swap_free),
            gpus: ctx.gpu.gpus,
            gpu_err: ctx.gpu.err,
            volumes: crate::disk::volumes(),
            hist: [
                self.hist[0].iter().copied().collect(),
                self.hist[1].iter().copied().collect(),
                self.hist[2].iter().copied().collect(),
                self.hist[3].iter().copied().collect(),
            ],
            procs,
            agents: ctx.agents,
            rolls,
            closed,
            other,
            jobs: ctx.jobs,
            job_res,
            writes: self.writes.clone(),
            ..Default::default()
        };
        (snap, exited)
    }
}

static TEMP_PATH: LazyLock<Option<std::path::PathBuf>> = LazyLock::new(|| {
    let rd = std::fs::read_dir("/sys/class/hwmon").ok()?;
    let mut best = None;
    for e in rd.flatten() {
        let name = std::fs::read_to_string(e.path().join("name")).unwrap_or_default();
        let n = name.trim();
        if matches!(n, "coretemp" | "k10temp" | "zenpower" | "cpu_thermal") {
            return Some(e.path().join("temp1_input"));
        }
        if n == "acpitz" {
            best = Some(e.path().join("temp1_input"));
        }
    }
    best
});

fn cpu_temp() -> Option<f64> {
    let s = std::fs::read_to_string(TEMP_PATH.as_ref()?).ok()?;
    let c = s.trim().parse::<f64>().ok()? / 1000.0;
    (c > 0.0 && c < 150.0).then_some(c.round())
}

// ---------------------------------------------------------------------------
// Global state, the sampling loop, leases

#[derive(Default)]
struct Global {
    lease_live: i64,
    last_poll: i64,
    running: bool,
    snap: Option<Arc<Snap>>,
    ticks: u64,
    last_tick_ms: f64,
    exits: VecDeque<(i64, P, Option<String>)>,
}

static G: LazyLock<Mutex<Global>> = LazyLock::new(Default::default);
static TICK: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);
static WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);

fn leased_tier(g: &Global, now: i64) -> Option<Tier> {
    if g.last_poll == 0 || now - g.last_poll >= STOP_MS {
        None
    } else if now - g.lease_live < 5_000 {
        Some(Tier::Live)
    } else {
        Some(Tier::Glance)
    }
}

/// The latest snapshot, if the sampler is running.
pub fn current() -> Option<Arc<Snap>> {
    G.lock().unwrap().snap.clone()
}

/// Starts the loop if it is not running (polls, armed alerts).
pub fn ensure_running(app: &Shared) {
    let mut g = G.lock().unwrap();
    if !g.running {
        g.running = true;
        tokio::spawn(run(app.clone()));
    } else {
        WAKE.notify_one();
    }
}

/// A poll: takes a lease on `tier` and waits (≤ 3 s) for a snapshot taken at
/// that tier or better.
async fn poll(app: &Shared, tier: Tier) -> Option<Arc<Snap>> {
    let mut rx = TICK.subscribe();
    let need_wait = {
        let mut g = G.lock().unwrap();
        let now = now_ms();
        let upgrade = leased_tier(&g, now).is_none_or(|t| t < tier);
        g.last_poll = now;
        if tier == Tier::Live {
            g.lease_live = now;
        }
        upgrade || g.snap.as_ref().is_none_or(|s| s.tier.is_none_or(|t| t < tier))
    };
    ensure_running(app);
    if need_wait {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(s) = current() {
                if s.tier.is_some_and(|t| t >= tier) {
                    return Some(s);
                }
            }
            if tokio::time::timeout_at(deadline, rx.changed()).await.is_err() {
                break;
            }
        }
    }
    current()
}

async fn agents_info(app: &Shared) -> (Vec<AgentInfo>, HashSet<String>) {
    let state = app.inner.lock().unwrap().state.clone();
    let v: Value = serde_json::from_str(&state).unwrap_or(Value::Null);
    let mut agents = Vec::new();
    let mut live = HashSet::new();
    for a in v["agents"].as_array().into_iter().flatten() {
        let Some(pane) = a["pane"].as_str() else { continue };
        live.insert(pane.to_string());
        agents.push(AgentInfo { pane: pane.to_string(), title: a["title"].as_str().unwrap_or("agent").to_string() });
    }
    for s in v["shells"].as_array().into_iter().flatten() {
        if let Some(p) = s["pane"].as_str() {
            live.insert(p.to_string());
        }
    }
    (agents, live)
}

fn running_jobs(app: &Shared) -> Vec<JobRef> {
    let txs: Vec<crate::Tx> =
        app.transcripts.lock().unwrap().iter().filter(|(k, _)| !k.contains('~')).map(|(_, t)| t.clone()).collect();
    let pane_of: HashMap<String, String> =
        app.inner.lock().unwrap().pane_sessions.iter().map(|(p, s)| (s.clone(), p.clone())).collect();
    let mut out = Vec::new();
    for tx in txs {
        let Ok(t) = tx.try_lock() else { continue };
        let Some(pane) = pane_of.get(&t.tid) else { continue };
        for j in t.jobs.iter().filter(|j| j.ended.is_none()) {
            out.push(JobRef { id: j.id.clone(), pane: pane.clone(), cmd: j.cmd.clone(), desc: j.desc.clone(), tid: t.tid.clone(), i: j.i });
        }
    }
    out
}

async fn refresh_shells(s: &mut Sampler, agents: &[AgentInfo]) {
    let now = now_ms();
    let stale = now - s.shells_at > 60_000;
    let want: HashSet<&str> = agents.iter().map(|a| a.pane.as_str()).collect();
    s.shells.retain(|p, _| want.contains(p.as_str()));
    let missing: Vec<String> =
        agents.iter().filter(|a| stale || !s.shells.contains_key(&a.pane)).map(|a| a.pane.clone()).collect();
    if missing.is_empty() {
        return;
    }
    let calls = missing.iter().map(|p| crate::herdr::call("pane.process_info", json!({"pane_id": p}), Duration::from_secs(2)));
    let res = futures_util::future::join_all(calls).await;
    for (p, r) in missing.into_iter().zip(res) {
        if let Some(pid) = r.ok().and_then(|v| v["process_info"]["shell_pid"].as_u64()) {
            s.shells.insert(p, pid as u32);
        }
    }
    if stale {
        s.shells_at = now;
    }
}

/// One tick: herdr shells (on change), nvidia-smi, then the /proc work off-thread.
async fn tick(app: &Shared, s: Sampler, tier: Tier) -> Sampler {
    let mut s = s;
    let (agents, live_panes) = agents_info(app).await;
    refresh_shells(&mut s, &agents).await;
    let now = now_ms();
    // GPU: every tick while someone looks; for alerts only when it matters
    let gpu = if tier >= Tier::Glance || crate::alerts::wants_gpu(s.gpu_last.as_ref(), now - s.gpu_at) {
        // live: pmon (heaviest call, ~29 ms CPU) at most every ~4 s, reused in between;
        // it also gives per-process memory, so compute-apps is skipped there
        let live = tier == Tier::Live;
        let fresh_pmon = live && now - s.pmon_at >= 3_500;
        // glance/watch: compute-apps only when VRAM use moved (> 32 MB) or every 30 s
        let prev_mem = s.gpu_last.as_ref().and_then(|l| l.gpus.first()).map(|g| g.mem_used);
        let apps_due = now - s.apps_at >= 30_000;
        let mut g = gpu::sample(fresh_pmon, |gpus: &[gpu::Gpu]| {
            let mem = gpus.first().map(|g| g.mem_used);
            !live && (apps_due || prev_mem.is_none_or(|p| mem.is_none_or(|m| m.abs_diff(p) > 32 << 20)))
        })
        .await;
        if fresh_pmon {
            s.pmon_at = now;
        } else if live {
            g.pmon = s.gpu_last.as_ref().and_then(|l| l.pmon.clone());
        }
        if g.apps_fresh {
            s.apps_at = now;
        } else if !live {
            g.apps = s.gpu_last.as_ref().map(|l| l.apps.clone()).unwrap_or_default();
        }
        s.gpu_at = now;
        s.gpu_last = Some(g.clone());
        g
    } else {
        s.gpu_last.clone().unwrap_or_default()
    };
    let jobs = running_jobs(app);
    let shells: HashMap<u32, String> = s.shells.iter().map(|(p, pid)| (*pid, p.clone())).collect();
    let ctx = Ctx { tier, agents, live_panes, shells, jobs, gpu };
    let t0 = Instant::now();
    let res = tokio::task::spawn_blocking(move || {
        let (snap, exited) = s.sample(ctx);
        (s, snap, exited)
    })
    .await;
    let Ok((s, mut snap, exited)) = res else { return Sampler::new() };
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let alerts = crate::alerts::evaluate(app, &snap);
    snap.headline = headline(&snap, &alerts);
    snap.alerts = alerts;
    {
        let mut g = G.lock().unwrap();
        g.ticks += 1;
        g.last_tick_ms = (ms * 10.0).round() / 10.0;
        let t = snap.t;
        for p in exited {
            let title = p.group.pane().and_then(|pn| snap.title(pn)).map(str::to_string);
            g.exits.push_back((t, p, title));
        }
        while g.exits.len() > EXIT_RING || g.exits.front().is_some_and(|(e, _, _)| t - e > EXIT_TTL_MS) {
            g.exits.pop_front();
        }
        g.snap = Some(Arc::new(snap));
    }
    TICK.send_modify(|v| *v += 1);
    s
}

async fn run(app: Shared) {
    let mut s = Sampler::new();
    let first = {
        let g = G.lock().unwrap();
        leased_tier(&g, now_ms())
    };
    // warm first response: a baseline 250 ms before the first full sample
    s = tokio::task::spawn_blocking(move || {
        s.baseline();
        s
    })
    .await
    .unwrap_or_else(|_| Sampler::new());
    tokio::time::sleep(Duration::from_millis(if first.is_some() { 250 } else { 1000 })).await;
    loop {
        let tier = {
            let mut g = G.lock().unwrap();
            let now = now_ms();
            match leased_tier(&g, now) {
                Some(t) => t,
                None if crate::alerts::armed() => Tier::Watch,
                None => {
                    g.running = false;
                    g.snap = None;
                    break;
                }
            }
        };
        let t0 = tokio::time::Instant::now();
        s = tick(&app, s, tier).await;
        let mut interval = Duration::from_millis(tier.interval());
        if tier >= Tier::Glance && !crate::alerts::armed() {
            // wake right when the last lease runs out, so sampling stops on time
            let left = G.lock().unwrap().last_poll + STOP_MS - now_ms();
            interval = interval.min(Duration::from_millis(left.max(0) as u64 + 20));
        }
        tokio::select! {
            _ = tokio::time::sleep_until(t0 + interval) => {}
            _ = WAKE.notified() => {
                // a poll upgraded the tier: tick now (but not more often than 500 ms)
                tokio::time::sleep_until(t0 + Duration::from_millis(500)).await;
            }
        }
    }
    drop(s);
    crate::trim_heap();
}

// ---------------------------------------------------------------------------
// Rendering

fn roll_json(r: &Roll, top: Vec<Value>, agent: bool) -> Value {
    let mut v = json!({
        "pane": r.pane, "cpu": round1(r.cpu), "rss": r.rss, "gpuMem": r.gpu_mem, "gpuUtil": r.gpu_util,
        "procs": r.procs, "top": top,
    });
    if agent {
        v["wBps"] = json!(r.wbps.round());
        v["wrote10m"] = json!(r.wrote10m);
        v["notable"] = json!(r.notable);
        v["jobs"] = json!(r.jobs);
    }
    v
}

fn members_of(snap: &Snap, g: &Group) -> Vec<usize> {
    snap.procs.iter().enumerate().filter(|(_, p)| &p.group == g).map(|(i, _)| i).collect()
}

pub fn render(snap: &Snap, tier: Tier, pane: Option<&str>) -> Value {
    let live = tier == Tier::Live;
    let rows = |g: &Group, flat: bool, limit: usize| -> Vec<Value> {
        if !live {
            return vec![];
        }
        group_rows(&snap.procs, &members_of(snap, g), flat, limit)
    };
    let agents: Vec<Value> = snap
        .rolls
        .iter()
        .map(|r| {
            let lim = if pane == Some(r.pane.as_str()) { 60 } else { 8 };
            roll_json(r, rows(&Group::Agent(r.pane.clone()), false, lim), true)
        })
        .collect();
    let closed: Vec<Value> =
        snap.closed.iter().map(|r| roll_json(r, rows(&Group::Closed(r.pane.clone()), false, 8), false)).collect();
    let mut other = roll_json(&snap.other, rows(&Group::Other, true, 8), false);
    other.as_object_mut().map(|o| o.remove("pane"));
    let gpu: Vec<Value> = snap
        .gpus
        .iter()
        .map(|g| {
            json!({
                "idx": g.idx, "name": g.name, "util": g.util, "memUsed": g.mem_used, "memTotal": g.mem_total,
                "tempC": g.temp, "powerW": g.power, "powerCapW": g.power_cap, "fanPct": g.fan,
                "hist": { "util": if g.idx == 0 { snap.hist[2].clone() } else { vec![] },
                          "mem": if g.idx == 0 { snap.hist[3].clone() } else { vec![] } },
            })
        })
        .collect();
    let now = now_ms();
    json!({
        "t": snap.t, "tier": tier.name(), "interval": tier.interval(), "age": (now - snap.t).max(0),
        "headline": snap.headline,
        "cpu": { "pct": snap.cpu_pct, "ncpu": snap.ncpu, "cores": snap.cores, "tempC": snap.temp,
                 "load": snap.load, "hist": snap.hist[0] },
        "mem": { "used": snap.mem_total.saturating_sub(snap.mem_avail), "total": snap.mem_total, "avail": snap.mem_avail,
                 "swapUsed": snap.swap_used, "swapTotal": snap.swap_total, "hist": snap.hist[1] },
        "gpu": gpu, "gpuErr": snap.gpu_err,
        "volumes": snap.volumes.iter().map(crate::disk::Volume::json).collect::<Vec<_>>(),
        "agents": agents, "closed": closed, "other": other, "alerts": snap.alerts,
    })
}

fn detail(snap: Option<&Snap>, p: &P, title: Option<&str>, ended: Option<i64>) -> Value {
    let mut v = proc_json(p);
    let cwd = if ended.is_none() {
        std::fs::read_link(format!("/proc/{}/cwd", p.pid)).ok().map(|c| c.to_string_lossy().into_owned())
    } else {
        None
    };
    let io = if ended.is_none() {
        std::fs::read_to_string(format!("/proc/{}/io", p.pid)).ok().and_then(|s| parse_io(&s))
    } else {
        None
    };
    let job = p.job.as_ref().and_then(|id| {
        let j = snap?.jobs.iter().find(|j| &j.id == id)?;
        Some(json!({"id": j.id, "desc": j.desc, "tid": j.tid, "i": j.i}))
    });
    let mut ancestors = Vec::new();
    let mut children = Vec::new();
    if let Some(s) = snap {
        let mut cur = p.ppid;
        let mut guard = 0;
        while cur > 0 && guard < 64 {
            guard += 1;
            match s.find(cur) {
                Some(a) => {
                    ancestors.push(json!({"pid": a.pid, "name": chain_name(&a.comm, a.cmd.as_deref())}));
                    cur = a.ppid;
                }
                None => break,
            }
        }
        ancestors.reverse();
        let mut kids: Vec<&P> = s.procs.iter().filter(|c| c.ppid == p.pid && c.start >= p.start).collect();
        kids.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        children = kids.into_iter().take(40).map(proc_json).collect();
    }
    let o = v.as_object_mut().unwrap();
    o.insert("cmd".into(), json!(p.cmd.as_deref().map(redact).unwrap_or_else(|| p.comm.clone())));
    o.insert("cwd".into(), json!(cwd));
    o.insert("state".into(), json!(p.state.to_string()));
    o.insert("threads".into(), json!(p.threads));
    o.insert("pane".into(), json!(p.group.pane()));
    o.insert(
        "group".into(),
        json!(match p.group {
            Group::Agent(_) => "agent",
            Group::Closed(_) => "closed",
            Group::Other => "other",
        }),
    );
    o.insert("agentTitle".into(), json!(title));
    o.insert("job".into(), job.unwrap_or(Value::Null));
    o.insert(
        "io".into(),
        match io {
            Some((r, w)) => json!({"read": r, "write": w, "wBps": p.wbps.map(f64::round)}),
            None => Value::Null,
        },
    );
    o.insert("ancestors".into(), json!(ancestors));
    o.insert("children".into(), json!(children));
    o.insert("watched".into(), json!(crate::alerts::is_watched(&p.key())));
    o.insert("ended".into(), json!(ended));
    v
}

// ---------------------------------------------------------------------------
// Brief for the master agent

fn brief_md(snap: &Snap, disk: Option<String>) -> String {
    let mut out = String::new();
    out.push_str(&format!("**{}**\n", snap.headline["text"].as_str().unwrap_or("")));
    match snap.gpus.first() {
        Some(g) => {
            let mut l = format!(
                "GPU {}% · {:.1} of {:.1} GB",
                g.util.map(|u| format!("{u:.0}")).unwrap_or("?".into()),
                g.mem_used as f64 / GIB,
                g.mem_total as f64 / GIB
            );
            if let Some(t) = g.temp {
                l.push_str(&format!(" · {t:.0}°C"));
            }
            if let Some(w) = g.power {
                l.push_str(&format!(" {w:.0} W"));
            }
            out.push_str(&l);
            out.push('\n');
        }
        None => out.push_str(&format!("GPU: {}\n", snap.gpu_err.as_deref().unwrap_or("none"))),
    }
    let titles: HashMap<&str, &str> = snap.agents.iter().map(|a| (a.pane.as_str(), a.title.as_str())).collect();
    let mut top: Vec<&P> = snap.procs.iter().filter(|p| p.notable() && p.cpu + p.gpu_mem as f64 / 1e8 >= 1.0).collect();
    top.sort_by(|a, b| {
        let s = |p: &P| p.cpu + p.gpu_mem as f64 / 5e7 + p.rss as f64 / 5e8;
        s(b).total_cmp(&s(a))
    });
    for p in top.into_iter().take(5) {
        let mut l = format!("- [{}](#/r?pid={})", p.label().replace(['[', ']'], ""), p.pid);
        match &p.group {
            Group::Agent(pane) => l.push_str(&format!(" in {} {pane}", titles.get(pane.as_str()).unwrap_or(&"agent"))),
            Group::Closed(pane) => l.push_str(&format!(" from closed tab {pane}")),
            Group::Other => l.push_str(" (not from an agent)"),
        }
        let mut parts = vec![];
        if p.gpu_mem > 0 {
            parts.push(format!("{} VRAM", fmt_gb(p.gpu_mem)));
        }
        parts.push(format!("{:.0}% CPU", p.cpu));
        parts.push(format!("{} RAM", fmt_gb(p.rss)));
        l.push_str(&format!(" — {}", parts.join(", ")));
        if let Some(j) = p.job.as_ref().and_then(|id| snap.jobs.iter().find(|j| &j.id == id)) {
            l.push_str(&format!(" (job \"{}\")", j.desc.as_deref().unwrap_or(&j.id)));
        }
        out.push_str(&l);
        out.push('\n');
    }
    let vol = snap
        .volumes
        .iter()
        .filter(|v| !v.ram)
        .map(|v| format!("{} {} free", v.mount, fmt_gb(v.avail)))
        .collect::<Vec<_>>()
        .join(" · ");
    out.push_str(&format!(
        "CPU {:.0}% of {} cores · RAM {:.1} of {:.1} GB · {} · [open Machine](#/r)\n",
        snap.cpu_pct,
        snap.ncpu,
        snap.mem_total.saturating_sub(snap.mem_avail) as f64 / GIB,
        snap.mem_total as f64 / GIB,
        vol
    ));
    if let Some(d) = disk {
        out.push_str(&d);
    }
    if out.chars().count() > 1200 {
        out = out.chars().take(1199).collect::<String>() + "…";
    }
    out
}

// ---------------------------------------------------------------------------
// HTTP

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/res", get(api_res))
        .route("/api/res/p/{pid}", get(api_proc))
        .route("/api/res/brief", get(api_brief))
        .route("/api/res/status", get(api_status))
        .merge(crate::alerts::routes())
}

#[derive(Deserialize)]
struct ResQ {
    tier: Option<String>,
    pane: Option<String>,
}

async fn api_res(State(app): State<Shared>, Query(q): Query<ResQ>) -> Response {
    let tier = if q.tier.as_deref() == Some("live") { Tier::Live } else { Tier::Glance };
    match poll(&app, tier).await {
        Some(s) => Json(render(&s, tier, q.pane.as_deref())).into_response(),
        None => json_err(StatusCode::SERVICE_UNAVAILABLE, "sampler not ready"),
    }
}

async fn api_proc(State(app): State<Shared>, AxPath(pid): AxPath<u32>) -> Response {
    let snap = poll(&app, Tier::Live).await;
    if let Some(s) = &snap {
        if let Some(p) = s.find(pid) {
            let title = p.group.pane().and_then(|pn| s.title(pn));
            return Json(detail(Some(s), p, title, None)).into_response();
        }
    }
    let gone = G.lock().unwrap().exits.iter().rev().find(|(_, p, _)| p.pid == pid).cloned();
    match gone {
        Some((ended, p, title)) => Json(detail(snap.as_deref(), &p, title.as_deref(), Some(ended))).into_response(),
        None => json_err(StatusCode::NOT_FOUND, "no_such_process"),
    }
}

/// A one-off sample (two walks 500 ms apart) without arming anything.
async fn one_shot(app: &Shared) -> Snap {
    let mut s = Sampler::new();
    let (agents, live_panes) = agents_info(app).await;
    refresh_shells(&mut s, &agents).await;
    let gpu = gpu::sample(true, |_: &[gpu::Gpu]| true).await;
    s = tokio::task::spawn_blocking(move || {
        s.baseline();
        s
    })
    .await
    .unwrap_or_else(|_| Sampler::new());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let jobs = running_jobs(app);
    let shells: HashMap<u32, String> = s.shells.iter().map(|(p, pid)| (*pid, p.clone())).collect();
    let ctx = Ctx { tier: Tier::Live, agents, live_panes, shells, jobs, gpu };
    let mut snap = tokio::task::spawn_blocking(move || s.sample(ctx).0).await.unwrap_or_default();
    let alerts = crate::alerts::evaluate_passive(&snap);
    snap.headline = headline(&snap, &alerts);
    snap.alerts = alerts;
    snap
}

#[derive(Deserialize)]
struct BriefQ {
    disk: Option<String>,
}

async fn api_brief(State(app): State<Shared>, Query(q): Query<BriefQ>) -> Response {
    let fresh = current().filter(|s| now_ms() - s.t < 6000 && s.tier.is_some_and(|t| t >= Tier::Glance));
    let snap = match fresh {
        Some(s) => s,
        None => Arc::new(one_shot(&app).await),
    };
    let disk = if q.disk.as_deref().is_some_and(|d| d != "0") { Some(crate::disk::brief_lines(&app)) } else { None };
    let md = brief_md(&snap, disk);
    crate::trim_heap();
    ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], md).into_response()
}

async fn api_status() -> Response {
    let g = G.lock().unwrap();
    let now = now_ms();
    let tier = if g.running { leased_tier(&g, now) } else { None };
    Json(json!({
        "tier": tier.map(Tier::name),
        "running": g.running,
        "lastPollAgo": (g.last_poll > 0).then(|| now - g.last_poll),
        "ticks": g.ticks, "lastTickMs": g.last_tick_ms,
        "watcher": crate::alerts::status(),
        "disk": { "scanning": crate::disk::scanning_json() },
    }))
    .into_response()
}

/// (start ms, label) of a live pid, for watches.
pub fn identify(pid: u32) -> Option<(i64, String)> {
    let r = parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
    let (_, btime) = parse_cpu_stat(&std::fs::read_to_string("/proc/stat").ok()?);
    let clk = sysconf(libc::_SC_CLK_TCK, 100);
    let start = btime * 1000 + (r.start_ticks * 1000 / clk) as i64;
    let cmd = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " ").trim().to_string())
        .unwrap_or_default();
    Some((start, label(&r.comm, &cmd)))
}

/// `/api/jobs` gains `res` for running jobs matched by the sampler.
pub fn decorate_jobs(jobs: &mut [Value]) {
    let Some(s) = current() else { return };
    for j in jobs.iter_mut() {
        let Some(id) = j["id"].as_str() else { continue };
        if j["status"] != "running" {
            continue;
        }
        j["res"] = match s.job_res.get(id) {
            Some(&(i, cpu, rss, gm, gu)) => {
                json!({"pid": s.procs[i].pid, "cpu": round1(cpu), "rss": rss, "gpuMem": gm, "gpuUtil": gu})
            }
            None => Value::Null,
        };
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32, comm: &str, cmd: &str, cpu: f64, rss_mb: u64) -> P {
        P {
            pid,
            ppid,
            start_ticks: pid as u64,
            start: pid as i64,
            comm: comm.into(),
            cmd: Some(cmd.into()),
            cpu,
            rss: rss_mb * MB,
            state: 'S',
            ..Default::default()
        }
    }

    #[test]
    fn stat_parsing() {
        let s = "81234 (python3 (x) y) R 81200 81234 1 0 -1 4194304 1 0 0 0 9000 700 0 0 20 0 24 0 5000 1000 51200 18446744073709551615";
        let r = parse_stat(s).unwrap();
        assert_eq!(r.pid, 81234);
        assert_eq!(r.comm, "python3 (x) y");
        assert_eq!(r.state, 'R');
        assert_eq!(r.ppid, 81200);
        assert_eq!(r.ticks, 9700);
        assert_eq!(r.threads, 24);
        assert_eq!(r.start_ticks, 5000);
        assert_eq!(r.rss_pages, 51200);
        assert!(!r.kthread);
        let k = "2 (kthreadd) S 0 0 0 0 -1 2129984 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0 0";
        assert!(parse_stat(k).unwrap().kthread);
    }

    #[test]
    fn cpu_and_mem_parsing() {
        let s = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\ncpu1 50 0 25 400 25 0 0 0 0 0\nbtime 1700000000\n";
        let (c, b) = parse_cpu_stat(s);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], (1000, 850));
        assert_eq!(b, 1700000000);
        let (t, a, st, sf) = parse_meminfo("MemTotal: 16000 kB\nMemFree: 1 kB\nMemAvailable: 8000 kB\nSwapTotal: 100 kB\nSwapFree: 40 kB\n");
        assert_eq!((t, a, st, sf), (16000 * 1024, 8000 * 1024, 102400, 40960));
        assert_eq!(parse_io("rchar: 1\nread_bytes: 10\nwrite_bytes: 20\n"), Some((10, 20)));
        assert_eq!(pane_from_environ(b"A=1\0HERDR_PANE_ID=w6:p7\0B=2"), Some("w6:p7".into()));
        assert_eq!(pane_from_environ(b"A=1\0"), None);
    }

    #[test]
    fn labels() {
        assert_eq!(label("python3", "/x/.venv/bin/python train.py bd1-walk-flat --steps 2e7"), "train.py bd1-walk-flat");
        assert_eq!(label("uv", "uv run python train.py bd1 --x"), "train.py bd1");
        assert_eq!(label("python3", "python3 -m vllm.entrypoints serve"), "vllm.entrypoints serve");
        assert_eq!(label("python3", "python3 -c while True: pass"), "python3 -c");
        assert_eq!(label("node", "node /usr/lib/node_modules/x/cli.js serve"), "cli.js serve");
        assert_eq!(label("cargo", "/home/u/.cargo/bin/cargo build --release"), "cargo build");
        assert_eq!(label("Xwayland", "/usr/bin/Xwayland :0 -rootless"), "Xwayland :0");
        assert_eq!(label("chromium", "/usr/lib/chromium/chromium --type=renderer"), "chromium");
        assert_eq!(label("kworker", ""), "kworker");
        assert_eq!(chain_name("uv", Some("uv run python x.py")), "uv run");
        assert_eq!(chain_name("bash", Some("/bin/bash -c foo")), "bash");
    }

    #[test]
    fn tags() {
        assert_eq!(tag("python3", "python train.py x"), Some("training"));
        assert_eq!(tag("cargo", "cargo build"), Some("build"));
        assert_eq!(tag("claude", "/home/u/.local/bin/claude"), Some("agent"));
        assert_eq!(tag("chromium", "/usr/lib/chromium/chromium --type=renderer"), Some("browser"));
        assert_eq!(tag("Hyprland", "Hyprland"), Some("desktop"));
        assert_eq!(tag("steamwebhelper", "steamwebhelper"), Some("game"));
        assert_eq!(tag("bash", "/bin/bash"), Some("shell"));
        assert_eq!(tag("node", "node vite dev"), Some("server"));
        assert_eq!(tag("foo", "foo"), None);
    }

    #[test]
    fn redaction() {
        let r = redact("python x.py --token=abc123 --api-key secret PASSWORD:hunter2 auth=");
        assert_eq!(r, "python x.py --token=… --api-key secret PASSWORD:… auth=");
        let r = redact("curl -H X sk_live_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 /home/u/Work/proj/target/release");
        assert_eq!(r, "curl -H X … /home/u/Work/proj/target/release");
        let long = "a ".repeat(400);
        assert_eq!(redact(&long).chars().count(), 300);
    }

    #[test]
    fn job_token_prefixes() {
        assert_eq!(job_tokens("cd /x && nohup timeout -s INT 25 uv run python train.py --x"), vec!["python", "train.py"]);
        assert_eq!(job_tokens("FOO=1 /usr/bin/python3 -c 'while True: pass'"), vec!["python3", "-c"]);
        assert_eq!(job_tokens("timeout 25 python3 -c 'x'"), vec!["python3", "-c"]);
        assert!(job_tokens("").is_empty());
    }

    /// shell 100 (w6:p3) › claude 101 › bash 102 › uv 103 › python 104 (train) › worker 105
    /// shell 200 (w6:pB) › node ×3 ; orphan 300 (env w6:p3) ; orphan 301 (env w6:p7 closed); chromium 400
    fn table() -> Vec<P> {
        let mut v = vec![
            p(1, 0, "systemd", "/sbin/init", 0.0, 10),
            p(100, 1, "bash", "/bin/bash", 0.0, 5),
            p(101, 100, "claude", "claude", 3.0, 400),
            p(102, 101, "bash", "/bin/bash -c uv run python train.py bd1", 0.0, 4),
            p(103, 102, "uv", "uv run python train.py bd1", 0.0, 20),
            p(104, 103, "python3", "/x/python train.py bd1 --steps 9", 97.0, 2100),
            p(105, 104, "python3", "/x/python train.py bd1 --steps 9", 20.0, 300),
            p(200, 1, "bash", "/bin/bash", 0.0, 5),
            p(201, 200, "node", "node a.js", 5.0, 150),
            p(202, 200, "node", "node a.js", 3.0, 150),
            p(203, 200, "node", "node a.js", 2.0, 150),
            p(300, 1, "python3", "python3 leak.py", 60.0, 50),
            p(301, 1, "python3", "python3 old.py", 0.0, 900),
            p(400, 1, "chromium", "/usr/lib/chromium/chromium", 14.0, 3000),
        ];
        v[5].gpu_mem = 6 << 30;
        v[12].gpu_mem = 6 << 30;
        v
    }

    fn attributed() -> Vec<P> {
        let mut t = table();
        let shells: HashMap<u32, String> = [(100, "w6:p3".to_string()), (200, "w6:pB".to_string())].into_iter().collect();
        let agents: HashSet<String> = ["w6:p3", "w6:pB"].iter().map(|s| s.to_string()).collect();
        let mut live = agents.clone();
        live.insert("w6:p9".into());
        let mut asked = vec![];
        let mut env = |p: &P| {
            asked.push(p.pid);
            match p.pid {
                300 => Some("w6:p3".to_string()),
                301 => Some("w6:p7".to_string()),
                _ => None,
            }
        };
        attribute(&mut t, &shells, &mut env, &agents, &live);
        // environ read only for big unattributed processes
        assert_eq!(asked, vec![300, 301, 400]);
        t
    }

    #[test]
    fn attribution_and_groups() {
        let t = attributed();
        let g = |pid: u32| t.iter().find(|p| p.pid == pid).unwrap().group.clone();
        assert_eq!(g(104), Group::Agent("w6:p3".into()));
        assert_eq!(g(105), Group::Agent("w6:p3".into()));
        assert_eq!(g(201), Group::Agent("w6:pB".into()));
        assert_eq!(g(300), Group::Agent("w6:p3".into()));
        assert!(t.iter().find(|p| p.pid == 300).unwrap().detached);
        assert_eq!(g(301), Group::Closed("w6:p7".into()));
        assert_eq!(g(400), Group::Other);
        assert_eq!(g(1), Group::Other);
    }

    #[test]
    fn pid_reuse_guard() {
        let mut t = table();
        // pid 500 claims ppid 100 but started before the shell: a reused pid, not a child
        let mut x = p(500, 100, "python3", "python3 z.py", 50.0, 10);
        x.start_ticks = 50;
        t.push(x);
        let shells: HashMap<u32, String> = [(100, "w6:p3".to_string())].into_iter().collect();
        let set: HashSet<String> = ["w6:p3".to_string()].into_iter().collect();
        attribute(&mut t, &shells, &mut |_| None, &set, &set);
        assert_eq!(t.iter().find(|p| p.pid == 500).unwrap().group, Group::Other);
    }

    #[test]
    fn rows_chain_merge_and_job() {
        let mut t = attributed();
        let m: Vec<usize> = (0..t.len()).filter(|&i| t[i].group == Group::Agent("w6:p3".into())).collect();
        let j = match_job(&t, &m, "cd ~/x && uv run python train.py bd1 --steps 9").unwrap();
        assert_eq!(t[j].pid, 104);
        t[j].job = Some("job1".into());
        let rows = group_rows(&t, &m, false, 8);
        let pids: Vec<u64> = rows.iter().map(|r| r["pid"].as_u64().unwrap()).collect();
        // DFS, roots by load: the detached leak (60%), claude, then train.py under it (chain bash › uv run), its worker, the detached leak
        assert_eq!(pids, vec![300, 101, 104, 105]);
        assert_eq!(rows[0]["detached"], true);
        assert_eq!(rows[1]["depth"], 0);
        assert_eq!(rows[1]["chain"], json!(["bash"]));
        assert_eq!(rows[2]["depth"], 1);
        assert_eq!(rows[2]["chain"], json!(["bash", "uv run"]));
        assert_eq!(rows[2]["label"], "train.py bd1");
        assert_eq!(rows[2]["job"], "job1");
        assert_eq!(rows[3]["depth"], 2);
        // node ×3 merged
        let mb: Vec<usize> = (0..t.len()).filter(|&i| t[i].group == Group::Agent("w6:pB".into())).collect();
        let rows = group_rows(&t, &mb, false, 8);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["n"], 3);
        assert_eq!(rows[0]["cpu"], 10.0);
        assert_eq!(rows[0]["pid"], 201);
    }

    fn snap_of(t: Vec<P>) -> Snap {
        Snap {
            procs: t,
            agents: vec![AgentInfo { pane: "w6:p3".into(), title: "RL walker".into() }],
            mem_total: 16 << 30,
            mem_avail: 8 << 30,
            ..Default::default()
        }
    }

    #[test]
    fn headline_rules() {
        let t = attributed();
        // closed-tab orphan beats GPU busy
        let s = snap_of(t.clone());
        let h = headline(&s, &[]);
        assert_eq!(h["text"], "Orphan from closed tab w6:p7 still holds 6.0 GB VRAM");
        assert_eq!(h["level"], "warn");
        // alert first
        let h = headline(&s, &[json!({"text": "VRAM full", "pane": null, "pid": 7})]);
        assert_eq!(h["text"], "VRAM full");
        assert_eq!(h["pid"], 7);
        // GPU busy
        let mut t2: Vec<P> = t.into_iter().filter(|p| p.pid != 301).collect();
        let s = Snap { gpus: vec![gpu::Gpu { util: Some(97.0), ..Default::default() }], ..snap_of(t2.clone()) };
        let h = headline(&s, &[]);
        assert_eq!(h["text"], "GPU 97% · train.py bd1 in RL walker (w6:p3)");
        assert_eq!(h["pid"], 104);
        // CPU
        for p in t2.iter_mut() {
            p.gpu_mem = 0;
        }
        let s = Snap { cpu_pct: 78.0, ..snap_of(t2.clone()) };
        assert_eq!(headline(&s, &[])["text"], "CPU 78% — train.py bd1 in RL walker (w6:p3)");
        // RAM tight
        let s = Snap { mem_avail: 1 << 30, ..snap_of(t2.clone()) };
        assert_eq!(headline(&s, &[])["text"], "RAM tight — 1.0 GB free; biggest: chromium 2.9 GB");
        // one process pinning a core
        let s = snap_of(t2.clone());
        assert_eq!(headline(&s, &[])["text"], "train.py bd1 at 97% CPU in RL walker (w6:p3)");
        // light load, then quiet
        for p in t2.iter_mut() {
            p.cpu = p.cpu.min(12.0);
        }
        let s = snap_of(t2.clone());
        assert!(headline(&s, &[])["text"].as_str().unwrap().starts_with("Light load — top: "));
        for p in t2.iter_mut() {
            p.cpu = 1.0;
        }
        let s = snap_of(t2);
        assert_eq!(headline(&s, &[])["text"], "Quiet — nothing above 5 %");
        assert_eq!(headline(&s, &[])["level"], "ok");
    }
}
