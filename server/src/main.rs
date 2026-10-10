//! sheprd: drive herdr-hosted Claude agents from an iPhone.
//!
//! One small process: herdr socket client + Claude transcript reader + HTTP/SSE
//! API + embedded PWA. See ../docs/API.md for the contract.

mod activity;
mod alerts;
mod apps;
mod asks;
mod branch;
mod artifacts;
mod chips;
mod cli;
mod cloud;
mod desk;
mod disk;
mod find;
mod fleet;
mod follow;
mod git;
mod gpu;
mod herdr;
mod inputbox;
mod keep;
mod launcher;
mod live;
mod modbridge;
mod newagent;
mod prefs;
mod ranged;
mod repos;
mod push;
mod res;
mod scan;
mod send;
mod share;
mod talk;
mod sched;
mod schedicon;
mod services;
mod slash;
mod surfaces;
mod theme;
mod tidy;
mod transcript;
mod usage;
mod v3;
mod voice;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as AxPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, broadcast, watch};
use transcript::{Transcript, now_ms};

mod assets {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

pub const DEFAULT_PORT: u16 = 7878;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

/// The home agent's display name in the app and in pushes (`SHEPRD_HOME_NAME`, default "Master"). Display only:
/// herdr's agent/tab name stays `master`.
pub fn home_name() -> String {
    std::env::var("SHEPRD_HOME_NAME").ok().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Master".into())
}

/// Where new agents start and the folder picker looks first: `SHEPRD_WORK_DIR`, else ~/Work when it
/// exists, else home.
pub fn work_dir() -> PathBuf {
    std::env::var_os("SHEPRD_WORK_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| Some(home().join("Work")).filter(|w| w.is_dir()).unwrap_or_else(home))
}

/// `SHEPRD_PORT` (test instances run on another port, local-only).
pub fn port() -> u16 {
    std::env::var("SHEPRD_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}

pub fn state_dir() -> PathBuf {
    let d = std::env::var_os("SHEPRD_STATE_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state/sheprd"));
    let _ = std::fs::create_dir_all(&d);
    d
}

type Shared = Arc<App>;
type Tx = Arc<Mutex<Transcript>>;

struct App {
    events: broadcast::Sender<(&'static str, Arc<String>)>,
    inner: Mutex<Inner>,
    transcripts: Mutex<HashMap<String, Tx>>,
    session_paths: Mutex<HashMap<String, PathBuf>>,
    refresh: Notify,
    agent_panes: watch::Sender<Vec<String>>,
    push: Option<push::Push>,
    /// Thins out routine pushes (push.rs `Gate`).
    push_gate: push::Gate,
    host: String,
    ts_name: String,
    ts_ip: String,
    web_dir: Option<PathBuf>,
    /// Web build id (`/api/build`, SSE `build`, `<meta name="mm-build">`): the phone reloads itself when it changes.
    build: String,
    /// `SHEPRD_NO_PUSH=1`: log pushes instead of sending them (test instances).
    no_push: bool,
    prefs: Mutex<prefs::Prefs>,
    /// Plans, watches, re-pings, reviewers (v3.rs).
    v3: v3::V3,
    /// Phone → mod inbox and what the mod reports (modbridge.rs).
    bridge: modbridge::Bridge,
    /// Unsend, suggestion chips, TL;DRs, catch-up digests, /phone, permissions (talk.rs).
    talk: talk::Talk,
    /// Which session each pane shows now, when herdr's id went stale (follow.rs).
    follow: follow::Follow,
    /// Background services the mods reported (services.rs).
    services: services::Services,
}


impl Inner {
    fn is_home_pane(&self, pane: &str) -> bool {
        self.home.as_ref().and_then(|h| h["pane"].as_str()) == Some(pane)
    }
    /// What the user sees an agent called: `home_name()` for the home agent, else its herdr title.
    fn shown_title(&self, pane: &str) -> Option<String> {
        if self.is_home_pane(pane) { Some(home_name()) } else { self.pane_titles.get(pane).cloned() }
    }
}

#[derive(Default)]
struct Inner {
    state: Arc<String>,
    state_key: String,
    herdr_ok: bool,
    /// pane -> (status, since, completion_seq)
    status: HashMap<String, (String, i64, u64)>,
    surfaces: VecDeque<Value>,
    /// Fingerprints of dismissed surfaces (surfaces.rs): the same note re-posted stays dismissed.
    surf_gone: surfaces::Gone,
    viewing: Option<(Option<String>, bool, i64)>,
    /// pane -> session id, from the last refresh
    pane_sessions: HashMap<String, String>,
    pane_titles: HashMap<String, String>,
    pane_tabs: HashMap<String, String>,
    first_refresh_done: bool,
    /// The pinned long-running "home" agent: {pane, session, cwd, ws, label}.
    home: Option<Value>,
    home_live: bool,
    /// Last time any client said it was visible (push suppression for short turns).
    last_visible: i64,
    /// pane -> pending AskUserQuestion from the mod.
    asks: HashMap<String, Ask>,
    /// pane -> title/color set from the phone, until the transcript confirms them.
    meta: HashMap<String, MetaReq>,
    /// Recent prompt nonces (F14): (nonce, ms).
    nonces: VecDeque<(String, i64)>,
}

struct Ask {
    ts: i64,
    questions: Value,
}

/// A rename/recolor from the phone. `applied` = when it was typed into the agent
/// (0 while waiting for the agent to go idle).
#[derive(Clone, Default)]
struct MetaReq {
    title: Option<String>,
    color: Option<String>,
    applied: i64,
}

fn load_home() -> Option<Value> {
    std::fs::read(state_dir().join("home.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn save_home(h: &Value) {
    let _ = std::fs::write(state_dir().join("home.json"), h.to_string());
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    single_arena();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("serve") => {}
        // `sheprd disk-scan <path> …`: the short-lived disk scanner child (disk.rs).
        Some("disk-scan") => std::process::exit(disk::cli(&args[1..])),
        Some(_) => std::process::exit(cli::run(&args).await),
    }
    let host = std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_default();
    let (ts_name, ts_ip) = tailscale_identity().await;
    let push = push::Push::load(&state_dir(), format!("https://{}", if ts_name.is_empty() { &host } else { &ts_name }));
    let mut surfaces: VecDeque<Value> = std::fs::read(state_dir().join("surfaces.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    for s in surfaces.iter_mut() {
        for (k, d) in [("replies", json!([])), ("command", Value::Null), ("i", Value::Null)] {
            if s.get(k).is_none() {
                s[k] = d;
            }
        }
    }
    let (events, _) = broadcast::channel(64);
    let (agent_panes, panes_rx) = watch::channel(Vec::new());
    let app = Arc::new(App {
        events,
        inner: Mutex::new(Inner {
            surfaces,
            surf_gone: surfaces::Gone::load(&state_dir().join("surfaces-dismissed.json")),
            home: load_home(),
            ..Default::default()
        }),
        transcripts: Mutex::new(HashMap::new()),
        session_paths: Mutex::new(HashMap::new()),
        refresh: Notify::new(),
        agent_panes,
        push,
        push_gate: push::Gate::default(),
        host,
        ts_name,
        ts_ip: ts_ip.clone(),
        web_dir: std::env::var_os("SHEPRD_WEB_DIR").map(PathBuf::from),
        build: build_id(std::env::var_os("SHEPRD_WEB_DIR").map(PathBuf::from).as_deref()),
        no_push: std::env::var("SHEPRD_NO_PUSH").is_ok_and(|v| !v.is_empty() && v != "0"),
        prefs: Mutex::new(prefs::Prefs::load(&state_dir())),
        v3: v3::V3::load(&state_dir()),
        bridge: modbridge::Bridge::default(),
        talk: talk::Talk::default(),
        follow: follow::Follow::default(),
        services: services::Services::default(),
    });

    tokio::spawn(refresh_loop(app.clone()));
    tokio::spawn(inputbox::run(app.clone()));
    tokio::spawn(v3::run(app.clone()));
    tokio::spawn(sched::run(app.clone()));
    tokio::spawn(asks::run(app.clone()));
    tokio::spawn(git::run(app.clone()));
    tokio::spawn(find::run(app.clone()));
    tokio::spawn(artifacts::run(app.clone()));
    tokio::spawn(subscription_loop(app.clone(), panes_rx));
    alerts::init(&app, &state_dir());
    tokio::spawn(theme::run(app.clone()));
    tokio::spawn(send::run(app.clone()));
    tokio::spawn(push_digests(app.clone()));
    tokio::spawn(tidy::run(app.clone()));

    let api = Router::new()
        .route("/api/health", get(api_health))
        .route("/api/state", get(api_state))
        .route("/api/events", get(api_events))
        .route("/api/build", get(api_build))
        .route("/api/t/{tid}", get(api_transcript))
        .route("/api/t/{tid}/full/{i}", get(api_full))
        .route("/api/t/{tid}/stream", get(api_stream))
        .route("/api/img/{tid}/{off}/{n}", get(api_img))
        .route("/api/a/{pane}/prompt", post(api_prompt))
        .route("/api/a/{pane}/interrupt", post(api_interrupt))
        .route("/api/a/{pane}/keys", post(api_keys))
        .route("/api/a/{pane}/text", post(api_text))
        .route("/api/a/{pane}/screen", get(api_screen))
        .route("/api/a/{pane}/choose", post(api_choose))
        .route("/api/a/{pane}/close", post(api_close))
        .route("/api/a/{pane}/meta", post(api_meta))
        .route("/api/a/{pane}/rename", post(api_rename))
        .route("/api/dirs", get(api_dirs))
        .route("/api/new", post(api_new))
        .route("/api/home/ensure", post(api_home_ensure))
        .route("/api/home", get(api_home))
        .route("/api/home/new", post(api_home_new))
        .route("/api/upload", post(api_upload))
        .route("/api/file", get(api_file))
        .route("/api/push/key", get(api_push_key))
        .route("/api/push/sub", post(api_push_sub))
        .route("/api/push/test", post(api_push_test))
        .route("/api/viewing", post(api_viewing))
        .route("/api/clientlog", post(api_clientlog))
        .route("/api/surface", post(api_surface))
        .route("/api/hint", post(api_hint))
        .route("/api/prompts", get(api_prompts))
        .route("/api/search", get(api_search))
        .route("/api/jobs", get(api_jobs))
        .route("/api/t/{tid}/files", get(api_files))
        .route("/api/t/{tid}/files/diff", get(api_files_diff))
        .route("/api/prefs", get(api_prefs_get).post(api_prefs_post))
        .route("/api/surface/{id}", get(api_surface_get))
        .route("/api/surface/{id}/dismiss", post(api_surface_dismiss))
        .route("/api/surfaces/dismiss", post(api_surfaces_dismiss))
        .merge(v3::routes())
        .merge(fleet::routes())
        .merge(find::routes())
        .merge(branch::routes())
        .merge(sched::routes())
        .merge(cloud::routes())
        .merge(desk::routes())
        .merge(live::routes())
        .merge(modbridge::routes())
        .merge(services::routes())
        .merge(activity::routes())
        .merge(apps::routes())
        .merge(talk::routes())
        .merge(res::routes())
        .merge(disk::routes())
        .merge(git::routes())
        .merge(artifacts::routes())
        .merge(usage::routes())
        .merge(voice::routes())
        .merge(theme::routes())
        .merge(send::routes())
        .merge(asks::routes())
        .merge(repos::routes())
        .merge(launcher::routes())
        .merge(slash::routes())
        .merge(tidy::routes())
        .fallback(get(static_file))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        // never gzip media: it is already compressed, and a gzipped 200 drops Accept-Ranges (ranged.rs)
        .layer(tower_http::compression::CompressionLayer::new().compress_when({
            use tower_http::compression::predicate::{DefaultPredicate, NotForContentType, Predicate};
            DefaultPredicate::new()
                .and(NotForContentType::const_new("video/"))
                .and(NotForContentType::const_new("audio/"))
                .and(NotForContentType::const_new("application/octet-stream"))
        }))
        .with_state(app.clone());

    // Tailscale may come up after us (boot); keep trying to bind its address.
    // Test instances (non-default port) stay local-only.
    let port = port();
    let r = api.clone();
    if port == DEFAULT_PORT {
        tokio::spawn(async move {
            let mut ip = ts_ip;
            loop {
                if ip.is_empty() {
                    ip = tailscale_identity().await.1;
                }
                if !ip.is_empty() {
                    let a = format!("{ip}:{port}");
                    match tokio::net::TcpListener::bind(&a).await {
                        Ok(l) => {
                            eprintln!("sheprd listening on http://{a}");
                            let _ = axum::serve(l, r.clone()).await;
                        }
                        Err(e) => eprintln!("bind {a}: {e}"),
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    }
    // `SHEPRD_LISTEN=0.0.0.0:7878,…`: extra addresses (e.g. the LAN without Tailscale). No auth: trusted networks only.
    for a in std::env::var("SHEPRD_LISTEN").unwrap_or_default().split(',').map(str::trim).filter(|a| !a.is_empty()) {
        match tokio::net::TcpListener::bind(a).await {
            Ok(l) => {
                eprintln!("sheprd listening on http://{a}");
                let r = api.clone();
                tokio::spawn(async move { axum::serve(l, r).await });
            }
            Err(e) => eprintln!("bind {a}: {e}"),
        }
    }
    let a = format!("127.0.0.1:{port}");
    let l = tokio::net::TcpListener::bind(&a).await.unwrap_or_else(|e| {
        eprintln!("bind {a}: {e}");
        std::process::exit(1)
    });
    eprintln!("sheprd listening on http://{a}");
    let _ = axum::serve(l, api).await;
}

pub(crate) async fn tailscale_identity() -> (String, String) {
    let out = tokio::process::Command::new("tailscale").args(["status", "--json", "--self", "--peers=false"]).output().await;
    let Ok(out) = out else { return (String::new(), String::new()) };
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    let name = v["Self"]["DNSName"].as_str().unwrap_or("").trim_end_matches('.').to_string();
    let ip = v["Self"]["TailscaleIPs"]
        .as_array()
        .and_then(|a| a.iter().filter_map(Value::as_str).find(|s| s.contains('.')))
        .unwrap_or("")
        .to_string();
    (name, ip)
}

// ---------------------------------------------------------------------------
// herdr event subscription: wakes the refresher on any relevant change.

async fn subscription_loop(app: Shared, mut panes_rx: watch::Receiver<Vec<String>>) {
    use tokio::io::AsyncBufReadExt;
    loop {
        let panes = panes_rx.borrow_and_update().clone();
        let mut subs: Vec<Value> = [
            "workspace.created", "workspace.closed", "workspace.renamed", "workspace.updated", "tab.created",
            "tab.closed", "tab.renamed", "pane.created", "pane.closed", "pane.updated", "pane.exited",
            "pane.agent_detected", "pane.moved",
        ]
        .iter()
        .map(|t| json!({"type": t}))
        .collect();
        for p in &panes {
            subs.push(json!({"type": "pane.agent_status_changed", "pane_id": p}));
        }
        match herdr::subscribe(subs).await {
            Ok(mut reader) => {
                app.refresh.notify_one();
                let mut line = String::new();
                loop {
                    line.clear();
                    tokio::select! {
                        r = reader.read_line(&mut line) => {
                            match r {
                                Ok(0) | Err(_) => break,
                                Ok(_) => app.refresh.notify_one(),
                            }
                        }
                        r = panes_rx.changed() => {
                            if r.is_err() { return; }
                            break; // resubscribe with the new pane set
                        }
                    }
                }
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

async fn refresh_loop(app: Shared) {
    let mut last_trim = now_ms();
    loop {
        // Safety net for every feature that scans transcripts: hand freed heap
        // back to the OS once a minute (cheap; the targeted trims stay too).
        if now_ms() - last_trim > 60_000 {
            last_trim = now_ms();
            trim_heap();
        }
        // Poll as a safety net (and to pick up transcript growth for previews);
        // faster while a phone is connected.
        let idle = if app.events.receiver_count() > 0 { 1500 } else { 4000 };
        tokio::select! {
            _ = app.refresh.notified() => {
                tokio::time::sleep(Duration::from_millis(40)).await; // coalesce bursts
            }
            _ = tokio::time::sleep(Duration::from_millis(idle)) => {}
        }
        refresh(&app).await;
    }
}

fn transcript_path_for(app: &App, session: &Value) -> Option<(String, PathBuf)> {
    let kind = session.get("kind").and_then(Value::as_str).unwrap_or("id");
    let val = session.get("value").and_then(Value::as_str)?;
    if kind == "path" {
        let p = PathBuf::from(val);
        let sid = p.file_stem()?.to_string_lossy().to_string();
        app.session_paths.lock().unwrap().insert(sid.clone(), p.clone());
        return Some((sid, p));
    }
    find_session(app, val).map(|p| (val.to_string(), p))
}

fn find_session(app: &App, sid: &str) -> Option<PathBuf> {
    if let Some(p) = app.session_paths.lock().unwrap().get(sid) {
        if p.exists() {
            return Some(p.clone());
        }
    }
    let projects = home().join(".claude/projects");
    let file = format!("{sid}.jsonl");
    for e in std::fs::read_dir(projects).ok()?.flatten() {
        let p = e.path().join(&file);
        if p.exists() {
            app.session_paths.lock().unwrap().insert(sid.to_string(), p.clone());
            return Some(p);
        }
    }
    None
}

/// Resolves a tid (`session` or `session~agentId`) to its cached transcript.
fn transcript(app: &App, tid: &str) -> Option<Tx> {
    if let Some(t) = app.transcripts.lock().unwrap().get(tid) {
        return Some(t.clone());
    }
    let (sid, agent) = match tid.split_once('~') {
        Some((s, a)) => (s, Some(a)),
        None => (tid, None),
    };
    if !valid_sid(sid) {
        return None;
    }
    let main = find_session(app, sid)?;
    let path = match agent {
        Some(a) if a.chars().all(|c| c.is_ascii_alphanumeric()) => transcript::subagent_path(&main, a),
        Some(_) => return None,
        None => main,
    };
    if !path.exists() {
        return None;
    }
    let t = Arc::new(Mutex::new(Transcript::new(tid.to_string(), path)));
    let mut map = app.transcripts.lock().unwrap();
    // Keep memory bounded: drop idle subagent transcripts first when the cache grows.
    if map.len() > 40 {
        let victims: Vec<String> = map.keys().filter(|k| k.contains('~')).take(20).cloned().collect();
        for v in victims {
            map.remove(&v);
        }
    }
    map.insert(tid.to_string(), t.clone());
    Some(t)
}

/// Per-agent facts read from its main transcript in the refresh pass.
struct TInfo {
    last: Value,
    title: Option<String>,
    subs: Value,
    doing: Value,
    answer_i: Option<usize>,
    recap: Value,
    chips: Vec<String>,
    queued: Vec<Value>,
    jobs: Value,
    color: Option<String>,
    custom_title: Option<String>,
    /// first line of the first user prompt (title fallback until Claude names the session)
    first_prompt: Option<String>,
    /// An AskUserQuestion call is waiting for its answer.
    asking: bool,
    /// that call's (timestamp, questions), read from the transcript
    ask_q: Option<(i64, Value)>,
    /// when the newest AskUserQuestion call got its answer (0: none yet), for clearing the mod's hint
    ask_answered: i64,
    /// transcript context estimate (modbridge::decorate turns it into `ctx` when the mod has not reported)
    ctx_est: Value,
    /// the agent's task list progress {done,total,cur,ts} (TodoWrite / TaskCreate+TaskUpdate)
    todo: Value,
    /// first real user prompt (up to 600 chars) and the session's first timestamp, for Haiku auto-naming
    first_text: Option<String>,
    started: i64,
    /// rules-pass activity right now + the current turn (activity.rs)
    act: Option<activity::Activity>,
    turn: activity::Turn,
}

impl Default for TInfo {
    fn default() -> Self {
        TInfo {
            last: Value::Null,
            title: None,
            subs: json!({"running": 0, "total": 0}),
            doing: Value::Null,
            answer_i: None,
            recap: Value::Null,
            chips: Vec::new(),
            queued: Vec::new(),
            jobs: json!({"running": 0, "total": 0}),
            color: None,
            custom_title: None,
            first_prompt: None,
            asking: false,
            ask_q: None,
            ask_answered: 0,
            ctx_est: Value::Null,
            todo: Value::Null,
            first_text: None,
            started: 0,
            act: None,
            turn: activity::Turn::default(),
        }
    }
}

/// An agent's title before Claude names it: a specific terminal title, else the first prompt's first line,
/// else the cwd's folder name; Claude's generic "Claude Code" terminal title only as a last resort.
fn fallback_title(term: Option<&str>, first_prompt: Option<&str>, cwd: Option<&str>, kind: Option<&str>) -> String {
    let term = term.map(str::trim).filter(|t| !t.is_empty());
    let generic = |t: &str| t.eq_ignore_ascii_case("claude code") || t.eq_ignore_ascii_case("claude");
    if let Some(t) = term.filter(|t| !generic(t)) {
        return t.to_string();
    }
    if let Some(p) = first_prompt.filter(|p| !p.is_empty()) {
        return p.to_string();
    }
    let dir = cwd.map(|c| c.trim_end_matches('/')).and_then(|c| c.rsplit('/').next()).filter(|d| !d.is_empty());
    if let Some(d) = dir {
        return d.to_string();
    }
    term.or(kind).unwrap_or("agent").to_string()
}

/// The claude session id herdr reports for an agent, even before its transcript exists.
fn session_id_of(session: &Value) -> Option<String> {
    let val = session.get("value").and_then(Value::as_str)?;
    let sid = match session.get("kind").and_then(Value::as_str) {
        Some("path") => Path::new(val).file_stem()?.to_string_lossy().to_string(),
        _ => val.to_string(),
    };
    valid_sid(&sid).then_some(sid)
}

fn valid_sid(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Done-push suppression: muted agents, and short turns while a client is in use.
fn suppress_done(muted: bool, turn_ms: i64, since_visible_ms: i64) -> bool {
    muted || (turn_ms < 20_000 && since_visible_ms < 60_000)
}

/// Deep link for a done push: the final answer of the agent (master: the home view).
fn done_url(pane: &str, answer_i: Option<usize>, is_home: bool) -> String {
    let base = if is_home { "/#/".to_string() } else { format!("/#/a/{pane}") };
    match answer_i {
        Some(i) => format!("{base}?i={i}"),
        None => base,
    }
}

/// Deep link for a surface push: Home's "You should see this" panel with that card in view and highlighted
/// (web/app.js `#/see/<id>`; the card's own tap opens the agent's chat at the item).
fn see_url(id: &str) -> String {
    format!("/#/see/{}", urlencode(id))
}

/// Newest `mcp__sheprd__surface` tool row titled `title` that no earlier surface already points at.
fn surface_row(items: &[transcript::Item], title: &str, claimed: &[usize]) -> Option<usize> {
    items.iter().rposition(|it| {
        it.k == "tool"
            && it.name.as_deref() == Some("mcp__sheprd__surface")
            && it.input.as_ref().and_then(|v| v["title"].as_str()).map(str::trim) == Some(title)
            && !claimed.contains(&it.i)
    })
}

/// Surfaces a chat draws inline with the surface card: those posted through `POST /api/surface` (curl, the mod's
/// own notes) that have no `mcp__sheprd__surface` tool row of their own; a mod-tool surface is already drawn from
/// its row. `row` is set when `i` was resolved to the tool row; older surfaces are checked against the item at `i`.
/// Reviewer results are drawn by v3.js.
fn chat_surfaces(items: &[transcript::Item], list: Vec<Value>) -> Vec<Value> {
    list.into_iter()
        .filter(|s| s["review"].is_null() && s["row"].as_bool() != Some(true))
        .filter(|s| {
            let Some(i) = s["i"].as_u64() else { return true };
            !items.get(i as usize).is_some_and(|it| {
                it.k == "tool"
                    && it.name.as_deref() == Some("mcp__sheprd__surface")
                    && it.input.as_ref().and_then(|v| v["title"].as_str()).map(str::trim) == s["title"].as_str().map(str::trim)
            })
        })
        .collect()
}

/// Types a phone rename/recolor into an idle agent as Claude's own commands.
async fn apply_meta_req(pane: &str, m: &MetaReq) {
    if let Some(t) = &m.title {
        let _ = herdr::quick("agent.prompt", json!({"target": pane, "text": format!("/rename {t}")})).await;
        if m.color.is_some() {
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
    }
    if let Some(c) = &m.color {
        let _ = herdr::quick("agent.prompt", json!({"target": pane, "text": format!("/color {c}")})).await;
    }
}

async fn refresh(app: &Shared) {
    let snap = herdr::quick("session.snapshot", json!({})).await;
    let Ok(mut snap) = snap else {
        let changed = {
            let mut inner = app.inner.lock().unwrap();
            let was = inner.herdr_ok;
            inner.herdr_ok = false;
            was || !inner.first_refresh_done
        };
        if changed {
            publish_state(app, json!([]), json!([]), json!([]), false);
        }
        return;
    };
    let snap = snap.get_mut("snapshot").map(Value::take).unwrap_or(Value::Null);
    let empty = vec![];
    let ws_list = snap["workspaces"].as_array().unwrap_or(&empty);
    let tab_list = snap["tabs"].as_array().unwrap_or(&empty);
    let ws_label: HashMap<&str, &str> = ws_list
        .iter()
        .filter_map(|w| Some((w["workspace_id"].as_str()?, w["label"].as_str().unwrap_or(""))))
        .collect();
    let tab_label: HashMap<&str, &str> =
        tab_list.iter().filter_map(|t| Some((t["tab_id"].as_str()?, t["label"].as_str().unwrap_or("")))).collect();
    let agents = snap["agents"].as_array().unwrap_or(&empty);
    let panes = snap["panes"].as_array().unwrap_or(&empty);

    // Resolve transcripts and parse new bytes off the async thread. A fresh
    // session has an id before its transcript file exists: keep the id.
    // herdr's id goes stale when Claude hands the conversation to a background session:
    // follow.rs finds the session each pane shows now.
    let mut sids: HashMap<usize, String> = HashMap::new();
    let find = |s: &str| find_session(app, s);
    let projects = home().join(".claude/projects");
    let find_prefix = |p: &str| follow::by_prefix(&projects, p);
    for (ix, a) in agents.iter().enumerate() {
        let herdr_sid = match a.get("agent_session") {
            Some(s) => transcript_path_for(app, s).map(|(sid, _)| sid).or_else(|| session_id_of(s)),
            None => None,
        };
        let sid = herdr_sid.or_else(|| {
            let pane = a["pane_id"].as_str().filter(|_| a["agent"] == "claude")?;
            app.follow.orphan(pane, &herdr::socket_path(), now_ms(), &find_prefix)
        });
        if let Some(sid) = sid {
            let since = app.follow.since(a["pane_id"].as_str().unwrap_or(""), &sid, now_ms());
            sids.insert(ix, app.follow.current(&sid, since, &find));
        }
    }
    app.follow.retain(&agents.iter().filter_map(|a| a["pane_id"].as_str().map(str::to_string)).collect::<Vec<_>>());
    let mut work: Vec<(usize, String, Tx, String)> = Vec::new();
    for (ix, sid) in &sids {
        if let Some(t) = transcript(app, sid) {
            let cwd = agents[*ix]["foreground_cwd"].as_str().or(agents[*ix]["cwd"].as_str()).unwrap_or("").to_string();
            work.push((*ix, sid.clone(), t, cwd));
        }
    }
    let parsed: Vec<(usize, TInfo)> = tokio::task::spawn_blocking(move || {
        work.into_iter()
            .map(|(ix, sid, t, cwd)| {
                let mut t = t.lock().unwrap();
                t.refresh();
                let subs = transcript::list_subagents(&t.path, &sid);
                t.link_subagents(&subs);
                let running = subs.iter().filter(|s| s.running).count();
                // running apps (servers) are not jobs in flight: Home lists them on their own
                let jobs_running = t.jobs.iter().filter(|j| j.ended.is_none() && !activity::app_cmd(&j.cmd)).count();
                let title = t.title().unwrap_or("").to_string();
                let (act, turn) = activity::of_transcript(&t, t.todo.as_ref(), &activity::Ctx { title: &title, cwd: &cwd });
                let info = TInfo {
                    last: t.last.as_ref().map(|l| json!(l)).unwrap_or(Value::Null),
                    title: t.title().map(str::to_string),
                    subs: json!({"running": running, "total": subs.len()}),
                    doing: t.doing().map(|(label, ts, i)| json!({"label": label, "ts": ts, "i": i})).unwrap_or(Value::Null),
                    answer_i: t.answer_i,
                    recap: t.recap.as_ref().map(|r| json!(r)).unwrap_or(Value::Null),
                    chips: t.last_answer().map(chips::chips).unwrap_or_default(),
                    queued: t.queued(),
                    jobs: json!({"running": jobs_running, "total": t.jobs.len()}),
                    color: t.color.clone(),
                    custom_title: t.custom_title.clone(),
                    first_prompt: t.first_prompt.clone(),
                    asking: t.waiting_on("AskUserQuestion"),
                    ask_q: t.pending_ask(),
                    ask_answered: t.ask_answered(),
                    ctx_est: t.ctx_est.as_ref().map(modbridge::est_json).unwrap_or(Value::Null),
                    todo: t.todo.as_ref().map(|x| json!(x)).unwrap_or(Value::Null),
                    first_text: t.first_prompt.as_ref().and_then(|_| {
                        t.items.iter().find_map(|it| {
                            let x = it.text.as_deref().filter(|x| it.k == "user" && !x.starts_with("[Request interrupted") && !x.starts_with("! "))?;
                            Some(x.chars().take(600).collect())
                        })
                    }),
                    started: t.items.iter().map(|it| it.ts).find(|ts| *ts > 0).unwrap_or(0),
                    act,
                    turn,
                };
                (ix, info)
            })
            .collect()
    })
    .await
    .unwrap_or_default();
    let mut by_ix: HashMap<usize, TInfo> = parsed.into_iter().collect();

    let now = now_ms();
    let mut out_agents = Vec::new();
    let mut pushes: Vec<(Value, bool)> = Vec::new();
    let mut apply_meta: Vec<(String, MetaReq)> = Vec::new();
    let mut auto_cands: Vec<newagent::Cand> = Vec::new();
    let mut act_cands: Vec<activity::Cand> = Vec::new();
    let mut agent_pane_ids = Vec::new();
    {
        let mut inner = app.inner.lock().unwrap();
        let viewing = inner.viewing.clone();
        let home_pane = inner.home.as_ref().and_then(|h| h["pane"].as_str().map(str::to_string));
        let mut seen = HashSet::new();
        inner.pane_sessions.clear();
        for (ix, a) in agents.iter().enumerate() {
            let pane = a["pane_id"].as_str().unwrap_or("").to_string();
            seen.insert(pane.clone());
            agent_pane_ids.push(pane.clone());
            let mut status = a["agent_status"].as_str().unwrap_or("unknown").to_string();
            let cseq = a["completion_seq"].as_u64().unwrap_or(0);
            let sid = sids.remove(&ix).unwrap_or_default();
            let mut info = by_ix.remove(&ix).unwrap_or_default();
            act_cands.push(activity::Cand { pane: pane.clone(), act: info.act.take(), turn: std::mem::take(&mut info.turn) });
            // Pending question: the transcript's open AskUserQuestion call wins (complete and current); else the
            // mod's hint (Claude Code writes the call to the transcript only once it is answered). The hint lives
            // until the mod's ask_clear, the call's answer in the transcript, or 30 min — never herdr's status,
            // which flips between blocked and idle while the dialog is open (that dropped the card mid-question).
            let mut ask = Value::Null;
            if let Some((ts, qs)) = info.ask_q.take() {
                ask = json!({"ts": ts, "questions": qs});
                inner.asks.remove(&pane); // superseded: a stale hint must not outlive this call
            } else if let Some(q) = inner.asks.get(&pane) {
                if info.ask_answered > q.ts || now - q.ts > 30 * 60_000 {
                    inner.asks.remove(&pane);
                } else {
                    ask = json!({"ts": q.ts, "questions": q.questions});
                }
            }
            // herdr reports an AskUserQuestion menu as idle/done (or working, while a background agent runs):
            // it is waiting on the human.
            if (info.asking || !ask.is_null()) && matches!(status.as_str(), "idle" | "done" | "working") {
                status = "blocked".into();
            }
            let (pinned, muted) = if sid.is_empty() {
                (false, false)
            } else {
                let mut prefs = app.prefs.lock().unwrap();
                // pins and mutes belong to the pane, not to one Claude session of it
                if let Some(old) = inner.pane_sessions.get(&pane).filter(|o| !o.is_empty()) {
                    prefs.carry_flags(old, &sid);
                }
                prefs.flags(&sid)
            };
            // Phone renames/recolors: show them at once, type them in once idle,
            // drop them when the transcript has them (or after a minute).
            let mut meta = inner.meta.get(&pane).cloned();
            if let Some(m) = &mut meta {
                let confirmed = m.title.as_ref().is_none_or(|t| info.custom_title.as_ref() == Some(t))
                    && m.color.as_deref().is_none_or(|c| info.color.as_deref() == (c != "default").then_some(c));
                if confirmed || (m.applied > 0 && now - m.applied > 60_000) {
                    inner.meta.remove(&pane);
                } else if m.applied == 0 && (status == "idle" || status == "done") {
                    m.applied = now;
                    inner.meta.insert(pane.clone(), m.clone());
                    apply_meta.push((pane.clone(), m.clone()));
                }
            }
            auto_cands.push(newagent::Cand {
                pane: pane.clone(),
                sid: sid.clone(),
                named: info.custom_title.is_some() || meta.as_ref().is_some_and(|m| m.title.is_some()),
                fallback: info.title.is_none(),
                has_color: info.color.is_some() || meta.as_ref().is_some_and(|m| m.color.is_some()),
                first_text: info.first_text.clone(),
                started: info.started,
            });
            let title = meta.as_ref().and_then(|m| m.title.clone()).or(info.title.clone()).unwrap_or_else(|| {
                fallback_title(
                    a["terminal_title_stripped"].as_str(),
                    info.first_prompt.as_deref(),
                    a["foreground_cwd"].as_str().or(a["cwd"].as_str()),
                    a["agent"].as_str(),
                )
            });
            let color = match meta.as_ref().and_then(|m| m.color.as_deref()) {
                Some("default") => None,
                Some(c) => Some(c.to_string()),
                None => info.color.clone(),
            };
            let last = info.last;
            let last_ts = last.get("ts").and_then(Value::as_i64).unwrap_or(0);
            let prev = inner.status.get(&pane).cloned();
            let since = match &prev {
                Some((s, since, _)) if *s == status => *since,
                Some(_) => now,
                None => if last_ts > 0 && status != "working" { last_ts } else { now },
            };
            let is_home = home_pane.as_deref() == Some(pane.as_str());
            // Pushes call the home agent by its display name (herdr agent/tab stays `master`).
            let ptitle = if is_home { home_name() } else { title.clone() };
            // Push triggers (never on the first observation).
            if let Some((pstatus, psince, pseq)) = &prev {
                let watching = matches!(&viewing, Some((Some(p), true, t)) if *p == pane && now - t < 45_000);
                if !watching {
                    // Muted agents never push, "Needs you" included.
                    if status == "blocked" && pstatus != "blocked" && !muted {
                        let question = ask["questions"][0]["question"].clone();
                        pushes.push((json!({"kind":"blocked","pane":pane,"title":ptitle,"question":question}), true));
                    } else if cseq > *pseq && status != "working" && status != "blocked"
                        && !last["text"].as_str().is_some_and(|t| t.starts_with("[Request interrupted"))
                    {
                        let turn_ms = if pstatus == "working" { now - psince } else { i64::MAX };
                        if !suppress_done(muted, turn_ms, now - inner.last_visible) {
                            pushes.push((
                                json!({"kind":"done","pane":pane,"title":ptitle,"body":last.get("text").cloned().unwrap_or(Value::Null),
                                       "url": done_url(&pane, info.answer_i, is_home)}),
                                false,
                            ));
                        }
                    }
                }
            }
            inner.status.insert(pane.clone(), (status.clone(), since, cseq));
            if !sid.is_empty() {
                inner.pane_sessions.insert(pane.clone(), sid.clone());
            }
            inner.pane_titles.insert(pane.clone(), title.clone());
            let tab = a["tab_id"].as_str().unwrap_or("");
            let ws = a["workspace_id"].as_str().unwrap_or("");
            inner.pane_tabs.insert(pane.clone(), tab.to_string());
            let idle = status == "idle" || status == "done";
            let busy = status == "working" || status == "blocked";
            out_agents.push(json!({
                "pane": pane, "tab": tab, "ws": ws,
                "wsLabel": ws_label.get(ws).copied().unwrap_or(""),
                "tabLabel": tab_label.get(tab).copied().unwrap_or(""),
                "kind": a["agent"], "name": a["name"],
                "status": status, "since": since, "completionSeq": cseq,
                "title": title, "color": color,
                "cwd": a["foreground_cwd"].as_str().or(a["cwd"].as_str()).unwrap_or(""),
                "session": if sid.is_empty() { Value::Null } else { json!(sid) },
                "tid": if sid.is_empty() { Value::Null } else { json!(sid) },
                "last": last, "subagents": info.subs,
                "focused": a["focused"].as_bool().unwrap_or(false),
                "doing": info.doing, "answerI": info.answer_i, "recap": info.recap,
                "chips": if idle { info.chips } else { Vec::new() },
                "ask": ask,
                "queued": if busy { info.queued } else { Vec::new() },
                "jobs": info.jobs,
                "pinned": pinned, "muted": muted,
                "ctx": info.ctx_est,
                "todo": info.todo,
            }));
        }
        inner.asks.retain(|p, _| seen.contains(p));
        inner.meta.retain(|p, _| seen.contains(p));
        inner.status.retain(|p, _| seen.contains(p));
        update_home(&mut inner, &mut out_agents);
        inner.herdr_ok = true;
        inner.first_refresh_done = true;
    }
    let shells: Vec<Value> = panes
        .iter()
        .filter(|p| p.get("agent").map(Value::is_null).unwrap_or(true))
        .map(|p| {
            json!({
                "pane": p["pane_id"], "tab": p["tab_id"], "ws": p["workspace_id"],
                "title": p["terminal_title_stripped"], "cwd": p["foreground_cwd"].as_str().or(p["cwd"].as_str()),
            })
        })
        .collect();
    let workspaces: Vec<Value> = ws_list
        .iter()
        .map(|w| {
            let id = w["workspace_id"].as_str().unwrap_or("");
            let cwd = panes.iter().find(|p| p["workspace_id"].as_str() == Some(id)).and_then(|p| p["cwd"].as_str());
            json!({"ws": id, "label": w["label"], "cwd": cwd})
        })
        .collect();
    v3::decorate(app, &mut out_agents);
    fleet::decorate(app, &mut out_agents);
    modbridge::decorate(app, &mut out_agents);
    services::decorate(app, &mut out_agents);
    talk::decorate(app, &mut out_agents);
    git::decorate(app, &mut out_agents);
    artifacts::decorate(app, &mut out_agents);
    newagent::decorate(app, &mut out_agents, auto_cands);
    activity::decorate(app, &mut out_agents, act_cands);
    find::decorate(app, &mut out_agents); // after services
    publish_state(app, json!(out_agents), json!(shells), json!(workspaces), true);
    for (pane, m) in apply_meta {
        tokio::spawn(async move { apply_meta_req(&pane, &m).await });
    }
    if *app.agent_panes.borrow() != agent_pane_ids {
        let _ = app.agent_panes.send(agent_pane_ids);
    }
    for (p, urgent) in pushes {
        let app = app.clone();
        tokio::spawn(async move { push_agent(&app, p, urgent).await });
    }
}

/// Keeps the home pin pointing at the right pane: adopts the "master" tab the
/// first time, follows the session if the pane moved, and records session
/// changes (e.g. after /clear) so a revive resumes the latest one.
fn update_home(inner: &mut Inner, agents: &mut [Value]) {
    let find = |agents: &[Value], f: &dyn Fn(&Value) -> bool| agents.iter().position(|a| f(a));
    let ix = match &inner.home {
        None => find(agents, &|a| a["tabLabel"] == "master").or_else(|| find(agents, &|a| a["tabLabel"] == "home")),
        Some(h) => find(agents, &|a| a["pane"] == h["pane"])
            .or_else(|| find(agents, &|a| !h["session"].is_null() && a["session"] == h["session"])),
    };
    inner.home_live = ix.is_some();
    let Some(ix) = ix else { return };
    agents[ix]["home"] = json!(true);
    let a = &agents[ix];
    let label = inner.home.as_ref().and_then(|h| h["label"].as_str().map(str::to_string)).unwrap_or_else(|| {
        a["tabLabel"].as_str().filter(|s| !s.is_empty()).unwrap_or("master").to_string()
    });
    let next = json!({"pane": a["pane"], "session": a["session"], "cwd": a["cwd"], "ws": a["ws"], "label": label});
    if inner.home.as_ref() != Some(&next) {
        save_home(&next);
        inner.home = Some(next);
    }
}

fn publish_state(app: &App, agents: Value, shells: Value, workspaces: Value, herdr_ok: bool) {
    let mut inner = app.inner.lock().unwrap();
    let surfaces: Vec<&Value> =
        inner.surfaces.iter().filter(|s| !s["dismissed"].as_bool().unwrap_or(false)).take(50).collect();
    let home = if inner.home_live { inner.home.as_ref().map(|h| h["pane"].clone()).unwrap_or(Value::Null) } else { Value::Null };
    let mut v = json!({
        "home": home,
        "host": app.host, "workDir": work_dir(), "homeName": home_name(), "tsName": app.ts_name, "tsIp": app.ts_ip, "herdr": herdr_ok,
        "agents": agents, "shells": shells, "workspaces": workspaces, "surfaces": surfaces,
    });
    v["services"] = services::standalone(app, &v["agents"], herdr_ok);
    let jobs_running: u64 = v["agents"].as_array().map(|l| l.iter().filter_map(|a| a["jobs"]["running"].as_u64()).sum()).unwrap_or(0);
    v["jobsRunning"] = json!(jobs_running);
    let key = v.to_string();
    if key == inner.state_key {
        return;
    }
    v["now"] = json!(now_ms());
    let s = Arc::new(v.to_string());
    inner.state_key = key;
    inner.state = s.clone();
    drop(inner);
    let _ = app.events.send(("state", s));
}

/// Edits one agent in the published state and republishes it.
fn patch_agent(app: &App, pane: &str, f: impl FnOnce(&mut Value)) {
    let mut cur: Value = serde_json::from_str(&app.inner.lock().unwrap().state).unwrap_or(json!({}));
    let Some(a) = cur["agents"].as_array_mut().and_then(|l| l.iter_mut().find(|a| a["pane"] == pane)) else { return };
    f(a);
    publish_state(
        app,
        cur["agents"].take(),
        cur["shells"].take(),
        cur["workspaces"].take(),
        cur["herdr"].as_bool().unwrap_or(false),
    );
}

fn republish(app: &App) {
    // Force a state rebuild that includes surface changes.
    let inner = app.inner.lock().unwrap();
    let cur: Value = serde_json::from_str(&inner.state).unwrap_or(json!({}));
    drop(inner);
    publish_state(
        app,
        cur.get("agents").cloned().unwrap_or(json!([])),
        cur.get("shells").cloned().unwrap_or(json!([])),
        cur.get("workspaces").cloned().unwrap_or(json!([])),
        cur.get("herdr").and_then(Value::as_bool).unwrap_or(false),
    );
}

/// Sends a Web Push to every subscriber (only logged on `SHEPRD_NO_PUSH` instances). Routine (non-urgent) pushes go
/// through the gate: quiet while the phone is in use, at most one per agent per 10 min, the rest as a digest.
async fn send_push(app: &App, payload: &Value, urgent: bool) {
    if !urgent {
        let last_visible = app.inner.lock().unwrap().last_visible;
        match app.push_gate.routine(payload, now_ms(), last_visible) {
            push::Verdict::Now => {}
            push::Verdict::Quiet | push::Verdict::Held => return,
        }
    }
    deliver_push(app, payload, urgent).await;
}

/// Sends the held routine pushes whose window ended (push.rs `Gate::due`).
async fn push_digests(app: Shared) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        let last_visible = app.inner.lock().unwrap().last_visible;
        for p in app.push_gate.due(now_ms(), last_visible) {
            deliver_push(&app, &p, false).await;
        }
    }
}

async fn deliver_push(app: &App, payload: &Value, urgent: bool) {
    if app.no_push {
        eprintln!("push (not sent): {payload}");
        return;
    }
    if let Some(p) = &app.push {
        p.send(payload, urgent).await;
    }
}

async fn push_agent(app: &App, p: Value, urgent: bool) {
    if !app.no_push && !app.push.as_ref().is_some_and(|p| p.has_subscribers()) {
        return;
    }
    // Background services never push "Done" (services.rs pushes their errors).
    if p["kind"] == "done" && p["pane"].as_str().is_some_and(|pane| app.services.is_service(pane)) {
        return;
    }
    // The mod's exact turn record: answer text, answer index, aborted/failed turns.
    let p = if p["kind"] == "done" { match modbridge::enrich_done(app, p).await { Some(p) => p, None => return } } else { p };
    let pane = p["pane"].as_str().unwrap_or("");
    let title = p["title"].as_str().unwrap_or("agent");
    if v3::at_desk(app, pane).await {
        return;
    }
    let payload = if p["kind"] == "blocked" {
        // Show the question/approval the agent is waiting on.
        let screen = herdr::quick("pane.read", json!({"pane_id": pane, "source": "visible", "format": "text"}))
            .await
            .ok()
            .and_then(|r| r["read"]["text"].as_str().map(str::to_string))
            .unwrap_or_default();
        // The plan, question or prompt plus the menu's options.
        let plan = app.v3.plan_text(pane);
        let body = v3::blocked_body(plan.as_deref(), p["question"].as_str(), &screen);
        let head = if plan.is_some() { "Plan ready" } else { "Needs you" };
        json!({"title": format!("{head} · {title}"), "body": transcript::cut(&body, 300).0, "url": format!("/#/a/{pane}"), "tag": pane})
    } else {
        let body = p["body"].as_str().unwrap_or("Finished");
        json!({"title": format!("Done · {title}"), "body": transcript::cut(body, 300).0, "url": p["url"], "tag": pane})
    };
    send_push(app, &payload, urgent).await;
}

// ---------------------------------------------------------------------------
// HTTP handlers

fn json_err(code: StatusCode, msg: impl ToString) -> Response {
    (code, Json(json!({"error": msg.to_string()}))).into_response()
}

fn herdr_err(e: herdr::HerdrError) -> Response {
    let code = match e.code.as_str() {
        "agent_blocked" => StatusCode::CONFLICT,
        "unreachable" => StatusCode::SERVICE_UNAVAILABLE,
        "not_found" | "pane_not_found" | "agent_not_found" => StatusCode::NOT_FOUND,
        _ => StatusCode::BAD_GATEWAY,
    };
    (code, Json(json!({"error": e.code, "message": e.message}))).into_response()
}

fn json_raw(s: Arc<String>) -> Response {
    ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], s.to_string()).into_response()
}

/// `sheprd setup` / `sheprd doctor` probe this: who answers on the port, and is the phone side wired up.
async fn api_health(State(app): State<Shared>) -> Response {
    let (herdr, agents, last_phone) = {
        let i = app.inner.lock().unwrap();
        let st: Value = serde_json::from_str(&i.state).unwrap_or(Value::Null);
        (i.herdr_ok, st["agents"].as_array().map_or(0, Vec::len), i.last_visible)
    };
    Json(json!({
        "app": "sheprd", "version": VERSION, "pid": std::process::id(), "herdr": herdr, "herdrSocket": herdr::socket_path(), "agents": agents,
        "push": {"enabled": app.push.is_some(), "subscribers": app.push.as_ref().map_or(0, |p| p.count()), "noPush": app.no_push},
        "lastPhone": last_phone, "tsName": app.ts_name, "stateDir": state_dir(),
    }))
    .into_response()
}

async fn api_state(State(app): State<Shared>) -> Response {
    let s = app.inner.lock().unwrap().state.clone();
    if s.is_empty() {
        refresh(&app).await;
    }
    json_raw(app.inner.lock().unwrap().state.clone())
}

/// The web build id: `SHEPRD_BUILD_ID` (tests), else a hash of `SHEPRD_WEB_DIR` read at startup, else the
/// hash of the embedded assets computed by build.rs. It only changes on restart.
fn build_id(web_dir: Option<&Path>) -> String {
    if let Some(b) = std::env::var("SHEPRD_BUILD_ID").ok().filter(|b| valid_build(b)) {
        return b;
    }
    let Some(d) = web_dir else { return assets::BUILD.to_string() };
    fn walk(d: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if let Ok(b) = std::fs::read(&p) {
                out.push((p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/"), b));
            }
        }
    }
    let mut files = Vec::new();
    walk(d, d, &mut files);
    files.sort();
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (rel, b) in &files {
        rel.hash(&mut h);
        b.hash(&mut h);
    }
    format!("b{:012x}", h.finish() & 0xffff_ffff_ffff)
}

/// `b` + 12 lowercase hex: the only shape the client and the telemetry allowlist accept.
fn valid_build(b: &str) -> bool {
    b.len() == 13 && b.starts_with('b') && b[1..].bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

async fn api_build(State(app): State<Shared>) -> Response {
    ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], json!({"build": app.build}).to_string()).into_response()
}

async fn api_events(State(app): State<Shared>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = app.events.subscribe();
    let initial = app.inner.lock().unwrap().state.clone();
    app.refresh.notify_one();
    let build = json!({"build": app.build}).to_string();
    let first = stream::iter([
        Ok(Event::default().event("build").data(build)),
        Ok(Event::default().event("state").data(initial.as_str())),
    ]);
    let rest = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok((name, data)) => return Some((Ok(Event::default().event(name).data(data.as_str())), rx)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    });
    use futures_util::StreamExt;
    Sse::new(first.chain(rest)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[derive(Deserialize)]
struct TQuery {
    tail: Option<usize>,
    after: Option<usize>,
    before: Option<usize>,
    around: Option<usize>,
}

/// True for a session herdr reports whose transcript file is not written yet.
fn pending_session(app: &App, tid: &str) -> bool {
    !tid.contains('~') && app.inner.lock().unwrap().pane_sessions.values().any(|s| s == tid)
}

fn pane_for_session(app: &App, sid: &str) -> Option<String> {
    app.inner.lock().unwrap().pane_sessions.iter().find(|(_, s)| s.as_str() == sid).map(|(p, _)| p.clone())
}

async fn api_transcript(State(app): State<Shared>, AxPath(tid): AxPath<String>, Query(q): Query<TQuery>) -> Response {
    let sid = tid.split('~').next().unwrap_or("").to_string();
    let agent_id = tid.split_once('~').map(|(_, a)| a.to_string());
    let pane = pane_for_session(&app, &sid);
    let Some(t) = transcript(&app, &tid) else {
        if pending_session(&app, &tid) {
            // A fresh session: no messages yet.
            return Json(json!({
                "tid": tid, "session": sid, "agentId": null, "title": null, "cwd": null, "color": null,
                "total": 0, "first": 0, "items": [], "subagents": [], "pane": pane,
            }))
            .into_response();
        }
        return json_err(StatusCode::NOT_FOUND, "no such transcript");
    };
    // surfaces posted for this agent (incl. dismissed ones: the chat keeps its history); the ones without their
    // own surface tool row are drawn inline at `i` (chat_surfaces)
    let surfs: Vec<Value> = if agent_id.is_none() {
        let inner = app.inner.lock().unwrap();
        inner
            .surfaces
            .iter()
            .filter(|s| s["tid"].as_str() == Some(sid.as_str()) || (s["tid"].is_null() && pane.is_some() && s["pane"].as_str() == pane.as_deref()))
            .take(60)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let body = tokio::task::spawn_blocking(move || {
        let mut t = t.lock().unwrap();
        t.refresh();
        let surfs = chat_surfaces(&t.items, surfs);
        let subs = if agent_id.is_none() { transcript::list_subagents(&t.path, &sid) } else { Vec::new() };
        t.link_subagents(&subs);
        let total = t.items.len();
        let (start, end) = if let Some(a) = q.after {
            (a.min(total), total)
        } else if let Some(n) = q.around {
            (n.saturating_sub(150).min(total), (n + 50).min(total))
        } else {
            let end = q.before.unwrap_or(total).min(total);
            (end.saturating_sub(q.tail.unwrap_or(300)), end)
        };
        json!({
            "tid": t.tid, "session": sid, "agentId": agent_id, "title": t.title(), "cwd": t.cwd, "color": t.color,
            "total": total, "first": start, "items": &t.items[start..end], "subagents": subs, "pane": pane,
            "surfaces": surfs,
        })
        .to_string()
    })
    .await
    .unwrap_or_default();
    ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

async fn api_full(State(app): State<Shared>, AxPath((tid, i)): AxPath<(String, usize)>) -> Response {
    let Some(t) = transcript(&app, &tid) else { return json_err(StatusCode::NOT_FOUND, "no such transcript") };
    let text = tokio::task::spawn_blocking(move || t.lock().unwrap().full_text(i)).await.ok().flatten();
    match text {
        Some(text) => Json(json!({"text": text})).into_response(),
        None => json_err(StatusCode::NOT_FOUND, "no such item"),
    }
}

async fn api_stream(
    State(app): State<Shared>,
    AxPath(tid): AxPath<String>,
    Query(q): Query<TQuery>,
) -> Response {
    if transcript(&app, &tid).is_none() && !pending_session(&app, &tid) {
        return json_err(StatusCode::NOT_FOUND, "no such transcript");
    }
    let after = q.after.unwrap_or(0);
    // A fresh session's file appears with its first message: until then just wait.
    let s = stream::unfold((app, tid, after, true), |(app, tid, mut after, first)| async move {
        let mut wait = !first;
        loop {
            if wait {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            wait = true;
            let Some(t2) = transcript(&app, &tid) else { continue };
            let res = tokio::task::spawn_blocking(move || {
                let mut t = t2.lock().unwrap();
                t.refresh();
                let total = t.items.len();
                if total > after {
                    if let Some(main) = t.tid.split('~').next().map(str::to_string) {
                        if !t.tid.contains('~') {
                            let subs = transcript::list_subagents(&t.path, &main);
                            t.link_subagents(&subs);
                        }
                    }
                    Some((json!({"items": &t.items[after.min(total)..], "total": total}).to_string(), total))
                } else {
                    None
                }
            })
            .await
            .ok()
            .flatten();
            if let Some((data, total)) = res {
                after = total;
                return Some((Ok::<_, Infallible>(Event::default().event("items").data(data)), (app, tid, after, false)));
            }
        }
    });
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response()
}

async fn api_img(State(app): State<Shared>, AxPath((tid, off, n)): AxPath<(String, u64, usize)>) -> Response {
    let Some(t) = transcript(&app, &tid) else { return json_err(StatusCode::NOT_FOUND, "no such transcript") };
    let path = t.lock().unwrap().path.clone();
    match tokio::task::spawn_blocking(move || transcript::image_at(&path, off, n)).await.ok().flatten() {
        Some((mt, bytes)) => (
            [(header::CONTENT_TYPE, mt), (header::CACHE_CONTROL, "public, max-age=31536000, immutable".into())],
            bytes,
        )
            .into_response(),
        None => json_err(StatusCode::NOT_FOUND, "no such image"),
    }
}

#[derive(Deserialize)]
struct TextBody {
    text: String,
}

#[derive(Deserialize)]
struct PromptBody {
    text: String,
    nonce: Option<String>,
    /// The sending agent's pane, as on `/steer`: the text gets a first line naming it, and the receiving chat
    /// shows the message as that agent's card (`kind: "steer"`).
    #[serde(default)]
    from: Option<String>,
}

/// F14: true if `nonce` was already used in the last 10 minutes (else records it).
fn seen_nonce(app: &App, nonce: Option<&str>) -> bool {
    let Some(n) = nonce.filter(|n| !n.is_empty()) else { return false };
    let now = now_ms();
    let mut inner = app.inner.lock().unwrap();
    inner.nonces.retain(|(_, t)| now - t < 10 * 60_000);
    if inner.nonces.iter().any(|(x, _)| x == n) {
        return true;
    }
    inner.nonces.push_back((n.to_string(), now));
    if inner.nonces.len() > 100 {
        inner.nonces.pop_front();
    }
    false
}

fn forget_nonce(app: &App, nonce: Option<&str>) {
    if let Some(n) = nonce {
        app.inner.lock().unwrap().nonces.retain(|(x, _)| x != n);
    }
}

async fn api_prompt(State(app): State<Shared>, AxPath(pane): AxPath<String>, headers: HeaderMap, Json(b): Json<PromptBody>) -> Response {
    if seen_nonce(&app, b.nonce.as_deref()) {
        return Json(json!({"ok": true, "dup": true})).into_response();
    }
    let mut text = b.text;
    let phone_ask = b.from.as_deref().is_none_or(|f| f.trim().is_empty()).then(|| text.clone());
    if let Some(from) = b.from.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        if !transcript::is_pane(from) || from == pane {
            forget_nonce(&app, b.nonce.as_deref());
            return json_err(StatusCode::BAD_REQUEST, "from must be another agent's pane id");
        }
        let name = app.inner.lock().unwrap().pane_titles.get(from).cloned().unwrap_or_else(|| "agent".into());
        text = format!("{}\n{}", transcript::steer_header(&name, from), text.trim());
    }
    // A just-started agent: the prompt waits for Claude to be ready (newagent.rs).
    if newagent::boot_queue(&pane, &text) {
        if let Some(t) = &phone_ask {
            asks::record(&app, &headers, &pane, t, "prompt");
        }
        app.refresh.notify_one();
        return Json(json!({"ok": true, "queued": true})).into_response();
    }
    // Same per-agent ordering as /steer.
    let lock = app.bridge.pane_lock(&pane);
    let _turn = lock.lock().await;
    // Never over the user's unsent draft (inputbox.rs): held, typed once the box is empty.
    let sent = if inputbox::must_wait(&pane) {
        Err(herdr::HerdrError { code: "input_draft".into(), message: String::new() })
    } else {
        herdr::quick("agent.prompt", json!({"target": pane, "text": text})).await
    };
    match sent {
        Ok(_) => {
            if let Some(t) = &phone_ask {
                asks::record(&app, &headers, &pane, t, "prompt");
            }
            app.refresh.notify_one();
            Json(json!({"ok": true})).into_response()
        }
        Err(e) if inputbox::is_busy(&e.code) => {
            if let Some(t) = &phone_ask {
                asks::record(&app, &headers, &pane, t, "prompt");
            }
            app.refresh.notify_one();
            Json(inputbox::held_reply(&pane, &text, &e.code)).into_response()
        }
        Err(e) => {
            // Not sent: a retry with the same nonce must go through.
            forget_nonce(&app, b.nonce.as_deref());
            herdr_err(e)
        }
    }
}

#[derive(Deserialize, Default)]
struct InterruptBody {
    text: Option<String>,
    nonce: Option<String>,
}

async fn agent_status(pane: &str) -> String {
    herdr::quick("agent.get", json!({"target": pane}))
        .await
        .ok()
        .and_then(|a| a["agent"]["agent_status"].as_str().map(str::to_string))
        .unwrap_or_default()
}

fn pane_session(app: &App, pane: &str) -> Option<String> {
    app.inner.lock().unwrap().pane_sessions.get(pane).cloned()
}

async fn transcript_len(t: Option<Tx>) -> usize {
    let Some(t) = t else { return 0 };
    tokio::task::spawn_blocking(move || {
        let mut t = t.lock().unwrap();
        t.refresh();
        t.items.len()
    })
    .await
    .unwrap_or(0)
}

/// True if an "[Request interrupted…" item appeared at or after index `from`.
async fn interrupted_since(t: Option<Tx>, from: usize) -> bool {
    let Some(t) = t else { return false };
    tokio::task::spawn_blocking(move || {
        let mut t = t.lock().unwrap();
        t.refresh();
        t.items.iter().skip(from).any(|it| it.k == "user" && it.text.as_deref().is_some_and(|x| x.starts_with("[Request interrupted")))
    })
    .await
    .unwrap_or(false)
}

/// The text in Claude's input box: the `❯` line right under a rule, plus its
/// continuation lines up to the closing rule. None when no input box is visible.
fn input_box(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    // Box rules may carry a label (the session name): "──── name ─".
    let rule = |l: &str| l.trim_start().starts_with("────");
    let at = (1..lines.len()).rev().find(|&i| lines[i].starts_with('❯') && rule(lines[i - 1]))?;
    let mut text = vec![lines[at].trim_start_matches('❯').trim()];
    for l in &lines[at + 1..] {
        if rule(l) {
            break;
        }
        text.push(l.trim());
    }
    Some(text.join("\n").trim().to_string())
}

/// F2: stops the running turn with Esc (never ctrl+c: a second one exits
/// Claude), clears what Claude put back into the input box with ctrl+u
/// (kill-line), then optionally sends `text`.
async fn api_interrupt(State(app): State<Shared>, AxPath(pane): AxPath<String>, body: Option<Json<InterruptBody>>) -> Response {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    if seen_nonce(&app, b.nonce.as_deref()) {
        return Json(json!({"ok": true, "dup": true})).into_response();
    }
    let mut status = agent_status(&pane).await;
    if status == "blocked" {
        forget_nonce(&app, b.nonce.as_deref());
        return json_err(StatusCode::CONFLICT, "agent_blocked");
    }
    // the user's unsent draft in the box: the Esc still stops the turn, but nothing below empties the box.
    let draft = matches!(inputbox::read(&pane).await, Some(inputbox::Input::Draft(_)));
    if status == "working" {
        // Remember where the transcript ends, to see the interrupt land in it.
        let tx = pane_session(&app, &pane).and_then(|sid| transcript(&app, &sid));
        let mark = transcript_len(tx.clone()).await;
        eprintln!("interrupt {pane}: esc");
        if let Err(e) = herdr::quick("pane.send_keys", json!({"pane_id": pane, "keys": ["esc"]})).await {
            forget_nonce(&app, b.nonce.as_deref());
            return herdr_err(e);
        }
        let start = now_ms();
        let mut second = false;
        while status == "working" && now_ms() - start < 4000 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            status = agent_status(&pane).await;
            // Still running after 2.5 s with no interrupt in the transcript: one
            // more Esc, only while Claude shows "esc to interrupt" (two Escs on
            // an idle prompt open the rewind menu; a queued message may already
            // have started a new turn, which the transcript check spares).
            if status == "working" && !second && now_ms() - start > 2500 {
                second = true;
                let landed = interrupted_since(tx.clone(), mark).await;
                if !landed && read_screen(&pane).await.contains("esc to interrupt") {
                    eprintln!("interrupt {pane}: second esc");
                    let _ = herdr::quick("pane.send_keys", json!({"pane_id": pane, "keys": ["esc"]})).await;
                }
            }
        }
    }
    // Claude puts queued messages back into the input box; the client resends
    // them, so empty it (ctrl+u per line, joining lines with backspace).
    // Text that survives a ctrl+u is a placeholder hint, not input.
    let mut cleared = false;
    let mut prev: Option<String> = None;
    for _ in 0..if draft { 0 } else { 8 } {
        let now_text = input_box(&read_screen(&pane).await);
        if prev.is_some() && now_text != prev {
            cleared = true; // the last ctrl+u removed something
        }
        match now_text {
            Some(t) if !t.is_empty() && prev.as_ref() != Some(&t) => {
                let keys = if t.contains('\n') { json!(["ctrl+u", "backspace"]) } else { json!(["ctrl+u"]) };
                eprintln!("interrupt {pane}: clearing input with {keys}");
                let _ = herdr::quick("pane.send_keys", json!({"pane_id": pane, "keys": keys})).await;
                tokio::time::sleep(Duration::from_millis(120)).await;
                prev = Some(t);
            }
            _ => break,
        }
    }
    let mut held = Value::Null;
    if let Some(text) = b.text.filter(|t| !t.trim().is_empty()) {
        match herdr::quick("agent.prompt", json!({"target": pane, "text": text})).await {
            Err(e) if inputbox::is_busy(&e.code) => held = inputbox::held_reply(&pane, &text, &e.code)["id"].clone(),
            Err(e) => {
                forget_nonce(&app, b.nonce.as_deref());
                return herdr_err(e);
            }
            Ok(_) => {}
        }
    }
    app.refresh.notify_one();
    Json(json!({"ok": true, "status": status, "cleared": cleared, "draft": draft, "held": held})).into_response()
}

#[derive(Deserialize)]
struct KeysBody {
    keys: Vec<String>,
}

async fn api_keys(AxPath(pane): AxPath<String>, Json(b): Json<KeysBody>) -> Response {
    match herdr::quick("pane.send_keys", json!({"pane_id": pane, "keys": b.keys})).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => herdr_err(e),
    }
}

async fn api_text(AxPath(pane): AxPath<String>, Json(b): Json<TextBody>) -> Response {
    match herdr::quick("pane.send_text", json!({"pane_id": pane, "text": b.text})).await {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => herdr_err(e),
    }
}

#[derive(Deserialize)]
struct ScreenQ {
    ansi: Option<u8>,
}

async fn api_screen(AxPath(pane): AxPath<String>, Query(q): Query<ScreenQ>) -> Response {
    let ansi = q.ansi.unwrap_or(0) == 1;
    let params = json!({"pane_id": pane, "source": "visible", "format": if ansi {"ansi"} else {"text"}, "strip_ansi": !ansi});
    match herdr::quick("pane.read", params).await {
        Ok(r) => {
            let plain = if ansi { read_screen(&pane).await } else { r["read"]["text"].as_str().unwrap_or("").to_string() };
            let options: Vec<Value> =
                parse_menu(&plain).into_iter().map(|o| json!({"label": o.label, "selected": o.selected, "n": o.number})).collect();
            // herdr's read revision stays 0, so derive one from the content: the
            // client skips redraws while it is unchanged.
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            r["read"]["text"].as_str().unwrap_or("").hash(&mut h);
            let revision = h.finish() >> 11; // fits a JS number exactly
            Json(json!({"text": r["read"]["text"], "revision": revision, "options": options})).into_response()
        }
        Err(e) => herdr_err(e),
    }
}

#[derive(Deserialize)]
struct ChooseBody {
    index: usize,
}

async fn api_choose(State(app): State<Shared>, AxPath(pane): AxPath<String>, Json(b): Json<ChooseBody>) -> Response {
    // one menu action at a time per pane (a second tap waits for the first instead of fighting it)
    let lock = app.bridge.pane_lock(&pane);
    let r = {
        let _one = lock.lock().await;
        choose_option(&pane, b.index).await
    };
    app.refresh.notify_one();
    match r {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => json_err(StatusCode::CONFLICT, e),
    }
}

/// Colors Claude's `/color` accepts ("default" resets).
const COLORS: [&str; 9] = ["red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan", "default"];

#[derive(Deserialize)]
struct MetaBody {
    pinned: Option<bool>,
    muted: Option<bool>,
    title: Option<String>,
    color: Option<String>,
}

/// F9 pin/mute (prefs, by session) and phone rename/recolor: the title and color
/// go in as Claude's own `/rename` and `/color` once the agent is idle; the tab
/// label follows the title right away.
async fn api_meta(State(app): State<Shared>, AxPath(pane): AxPath<String>, Json(b): Json<MetaBody>) -> Response {
    let title = b.title.as_deref().map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "));
    if title.as_deref().is_some_and(|t| t.is_empty() || t.chars().count() > 100) {
        return json_err(StatusCode::BAD_REQUEST, "title must be 1-100 chars");
    }
    if let Some(c) = &b.color {
        if !COLORS.contains(&c.as_str()) {
            return json_err(StatusCode::BAD_REQUEST, format!("color must be one of {}", COLORS.join(", ")));
        }
    }
    let (sid, tab, known) = {
        let i = app.inner.lock().unwrap();
        (i.pane_sessions.get(&pane).cloned(), i.pane_tabs.get(&pane).cloned(), i.status.contains_key(&pane))
    };
    if !known {
        return json_err(StatusCode::NOT_FOUND, "unknown pane");
    }
    if b.pinned.is_some() || b.muted.is_some() {
        let Some(sid) = &sid else { return json_err(StatusCode::NOT_FOUND, "pane has no session") };
        app.prefs.lock().unwrap().set_flags(sid, b.pinned, b.muted);
    }
    if title.is_some() || b.color.is_some() {
        let mut i = app.inner.lock().unwrap();
        let m = i.meta.entry(pane.clone()).or_default();
        if m.applied > 0 {
            // already typed in: only the new edit gets typed (a recolor then a rename typed /color twice)
            *m = MetaReq::default();
        }
        if title.is_some() {
            m.title = title.clone();
        }
        if b.color.is_some() {
            m.color = b.color.clone();
        }
        m.applied = 0; // (re)type it in at the next idle refresh
    }
    // Optimistic: show it in state now; the next refresh keeps it until confirmed.
    patch_agent(&app, &pane, |a| {
        for (k, v) in [("pinned", b.pinned), ("muted", b.muted)] {
            if let Some(v) = v {
                a[k] = json!(v);
            }
        }
        if let Some(t) = &title {
            a["title"] = json!(t);
        }
        if let Some(c) = &b.color {
            a["color"] = if c == "default" { Value::Null } else { json!(c) };
        }
    });
    if let (Some(t), Some(tab)) = (&title, &tab) {
        let _ = herdr::quick("tab.rename", json!({"tab_id": tab, "label": t})).await;
    }
    app.refresh.notify_one();
    Json(json!({"ok": true})).into_response()
}

async fn api_close(State(app): State<Shared>, AxPath(pane): AxPath<String>) -> Response {
    match herdr::quick("pane.close", json!({"pane_id": pane})).await {
        Ok(_) => {
            app.refresh.notify_one();
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => herdr_err(e),
    }
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

async fn api_rename(State(app): State<Shared>, AxPath(pane): AxPath<String>, Json(b): Json<NameBody>) -> Response {
    let tab = app.inner.lock().unwrap().pane_tabs.get(&pane).cloned();
    let Some(tab) = tab else { return json_err(StatusCode::NOT_FOUND, "unknown pane") };
    match herdr::quick("tab.rename", json!({"tab_id": tab, "label": b.name})).await {
        Ok(_) => {
            app.refresh.notify_one();
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => herdr_err(e),
    }
}

async fn api_dirs(State(app): State<Shared>) -> Response {
    let mut dirs: Vec<(String, String, bool, i64)> = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |p: PathBuf, dirs: &mut Vec<(String, String, bool, i64)>| {
        let Ok(canon) = p.canonicalize() else { return };
        let key = canon.to_string_lossy().to_string();
        if !canon.is_dir() || !seen.insert(key.clone()) {
            return;
        }
        let name = canon.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| key.clone());
        let git = canon.join(".git").exists();
        let recent = std::fs::metadata(&canon).map(|m| transcript::mtime_ms(&m)).unwrap_or(0);
        dirs.push((key, name, git, recent));
    };
    // Agent cwds first: where work is happening now.
    let state: Value = serde_json::from_str(&app.inner.lock().unwrap().state).unwrap_or(Value::Null);
    if let Some(agents) = state["agents"].as_array() {
        for a in agents {
            if let Some(c) = a["cwd"].as_str() {
                add(PathBuf::from(c), &mut dirs);
            }
        }
    }
    let work = work_dir();
    if let Ok(rd) = std::fs::read_dir(&work) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.starts_with('.') && e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                add(e.path(), &mut dirs);
            }
        }
    }
    add(work, &mut dirs);
    add(home(), &mut dirs);
    let n_agent = dirs.len();
    let _ = n_agent;
    dirs.sort_by(|a, b| b.3.cmp(&a.3));
    let out: Vec<Value> = dirs.into_iter().map(|(path, name, git, recent)| json!({"path": path, "name": name, "git": git, "recent": recent})).collect();
    Json(json!({"dirs": out})).into_response()
}

#[derive(Deserialize)]
struct NewBody {
    cwd: String,
    label: Option<String>,
    prompt: Option<String>,
    ws: Option<String>,
    args: Option<Vec<String>>,
    /// Plain shell tab (no Claude): the phone's "open in a terminal".
    shell: Option<bool>,
}

fn agent_slug(s: &str) -> String {
    let mut out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if !out.starts_with(|c: char| c.is_ascii_lowercase()) {
        out = format!("a-{out}");
    }
    out.truncate(24);
    let suffix = format!("{:x}", now_ms() & 0xfff);
    format!("{}-{suffix}", out.trim_end_matches('-'))
}

/// Starts claude in a freshly created pane. herdr rejects the start until the
/// pane's shell reaches its prompt, so retry that for a few seconds.
async fn start_claude(pane: &str, name: &str, args: Vec<String>) -> Result<Value, herdr::HerdrError> {
    // Answer a trust prompt as soon as it shows, rather than after agent.start gives up.
    // (agent.start can report success while the prompt is still up, so the
    // watcher runs on its own until it answers or gives up after ~30s.)
    let p = pane.to_string();
    tokio::spawn(async move { accept_trust_prompt(&p).await });
    start_claude_inner(pane, name, args).await
}

async fn start_claude_inner(pane: &str, name: &str, args: Vec<String>) -> Result<Value, herdr::HerdrError> {
    let deadline = now_ms() + 15_000;
    loop {
        let r = herdr::call(
            "agent.start",
            json!({"name": name, "kind": "claude", "pane_id": pane, "args": args, "timeout_ms": 120000}),
            Duration::from_secs(130),
        )
        .await;
        match r {
            Err(e) if e.code == "agent_pane_busy" && now_ms() < deadline => {
                tokio::time::sleep(Duration::from_millis(300)).await
            }
            Ok(v) => return Ok(v),
            Err(e) => {
                // The user picked this folder in the app, which is the consent the
                // trust prompt asks for: answer it, then let the start proceed.
                if accept_trust_prompt(pane).await {
                    return Ok(json!({"trusted": true}));
                }
                return Err(e);
            }
        }
    }
}

/// One choice of a terminal menu (Claude's trust, permission and question prompts).
struct MenuOption {
    label: String,
    selected: bool,
    number: Option<u32>,
}

/// Finds the menu on a blocked agent's screen: the line marked with `❯` and its
/// sibling choices (same text column, or numbered "N. label" lines).
fn parse_menu(screen: &str) -> Vec<MenuOption> {
    let lines: Vec<&str> = screen.lines().collect();
    let Some(sel) = lines.iter().rposition(|l| l.trim_start().starts_with('❯')) else { return Vec::new() };
    let col = |l: &str| l.chars().take_while(|c| *c == ' ').count();
    let sel_line = lines[sel];
    let marker_col = col(sel_line);
    let text_col = marker_col + 2;
    let numbered = |l: &str| -> Option<(u32, String)> {
        let t = l.trim_start().trim_start_matches('❯').trim_start();
        let (n, rest) = t.split_once(". ")?;
        Some((n.parse().ok()?, rest.trim().to_string()))
    };
    // AskUserQuestion multiSelect: an unnumbered "Next" (more questions follow) or "Submit" row closes the ticks
    let is_nav = |l: &str| matches!(l.trim().trim_start_matches('❯').trim(), "Next" | "Submit");
    let num_menu = numbered(sel_line).is_some()
        || (is_nav(sel_line) && lines[sel.saturating_sub(12)..sel].iter().any(|l| numbered(l).is_some_and(|_| col(l) == text_col)));
    let is_option = |i: usize| -> bool {
        let l = lines[i];
        if l.trim().is_empty() {
            return false;
        }
        if i == sel {
            return true;
        }
        if num_menu {
            return (numbered(l).is_some() && (col(l) == text_col || col(l) == marker_col)) || is_nav(l);
        }
        col(l) == text_col && !l.trim_start().starts_with('❯')
    };
    let skippable = |l: &str| l.trim().is_empty() || col(l) > text_col || l.trim().chars().all(|c| "─━-═".contains(c));
    let mut idx = vec![sel];
    let mut i = sel;
    while i > 0 {
        i -= 1;
        if is_option(i) { idx.push(i) } else if skippable(lines[i]) && num_menu { continue } else { break }
    }
    let mut i = sel;
    while i + 1 < lines.len() {
        i += 1;
        if is_option(i) { idx.push(i) } else if skippable(lines[i]) && num_menu { continue } else { break }
    }
    idx.sort();
    if idx.len() < 2 {
        return Vec::new();
    }
    idx.into_iter()
        .map(|i| {
            let l = lines[i];
            let (number, label) = match numbered(l) {
                Some((n, t)) => (Some(n), t),
                None => (None, l.trim_start().trim_start_matches('❯').trim().to_string()),
            };
            MenuOption { label, selected: i == sel, number }
        })
        .collect()
}

async fn read_screen(pane: &str) -> String {
    herdr::quick("pane.read", json!({"pane_id": pane, "source": "visible", "format": "text"}))
        .await
        .ok()
        .and_then(|r| r["read"]["text"].as_str().map(str::to_string))
        .unwrap_or_default()
}

/// A multiSelect row ("[ ] red", "[✔] red"): Enter toggles it and the menu stays.
fn is_checkbox(label: &str) -> bool {
    ["[ ]", "[✔]", "[✓]", "[x]", "[X]"].iter().any(|p| label.starts_with(p))
}

/// Moves the menu selection to option `target` and confirms, one key at a time
/// against the live screen (a menu can ignore keys while it is still drawing).
/// Enter goes once: on a multiSelect row it toggles the tick and the menu stays, so pressing it again
/// "until the menu is gone" would untick it (the tap flickered on and off for 7 s, then failed). Done
/// once the screen changes; a plain row whose Enter was ignored gets one more.
async fn choose_option(pane: &str, target: usize) -> Result<(), String> {
    let mut enters = 0;
    for _ in 0..14 {
        let screen = read_screen(pane).await;
        let menu = parse_menu(&screen);
        if menu.len() < 2 {
            return Ok(()); // menu gone: answered
        }
        if target >= menu.len() {
            return Err("no such option".into());
        }
        let cur = menu.iter().position(|o| o.selected).unwrap_or(0);
        if cur != target {
            let key = if cur < target { "down" } else { "up" };
            herdr::quick("pane.send_keys", json!({"pane_id": pane, "keys": [key]})).await.map_err(|e| e.to_string())?;
            tokio::time::sleep(Duration::from_millis(180)).await;
            continue;
        }
        if enters == 2 || (enters == 1 && is_checkbox(&menu[target].label)) {
            break;
        }
        herdr::quick("pane.send_keys", json!({"pane_id": pane, "keys": ["enter"]})).await.map_err(|e| e.to_string())?;
        enters += 1;
        for _ in 0..15 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if read_screen(pane).await != screen {
                return Ok(());
            }
        }
    }
    Err("menu did not respond".into())
}

/// If the pane shows Claude's "trust this folder?" prompt, answers Yes.
/// Returns true when it answered the prompt.
async fn accept_trust_prompt(pane: &str) -> bool {
    for _ in 0..60 {
        let screen = read_screen(pane).await;
        if screen.contains("Yes, I trust this folder") {
            let menu = parse_menu(&screen);
            if let Some(t) = menu.iter().position(|o| o.label.starts_with("Yes, I trust")) {
                // Give the dialog a moment to start reading keys.
                tokio::time::sleep(Duration::from_millis(400)).await;
                return choose_option(pane, t).await.is_ok();
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

async fn api_new(State(app): State<Shared>, Json(b): Json<NewBody>) -> Response {
    let cwd = PathBuf::from(&b.cwd);
    if !cwd.is_dir() {
        return json_err(StatusCode::BAD_REQUEST, "cwd is not a directory");
    }
    let base = cwd.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "claude".into());
    let label = b.label.clone().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| base.clone());
    // Pick a workspace: explicit, else the one whose cwd matches, else the first.
    let ws = match b.ws.as_deref() {
        Some("new") => None,
        Some(w) if !w.is_empty() => Some(w.to_string()),
        _ => {
            let state: Value = serde_json::from_str(&app.inner.lock().unwrap().state).unwrap_or(Value::Null);
            let list = state["workspaces"].as_array().cloned().unwrap_or_default();
            let target = cwd.to_string_lossy().to_string();
            list.iter()
                .find(|w| w["cwd"].as_str() == Some(target.as_str()))
                .or_else(|| list.iter().filter(|w| w["cwd"].as_str().map(|c| target.starts_with(c)).unwrap_or(false)).max_by_key(|w| w["cwd"].as_str().map(str::len).unwrap_or(0)))
                .or(list.first())
                .and_then(|w| w["ws"].as_str().map(str::to_string))
        }
    };
    let created = match &ws {
        Some(w) => herdr::quick("tab.create", json!({"workspace_id": w, "cwd": b.cwd, "label": label, "focus": false})).await,
        None => herdr::quick("workspace.create", json!({"cwd": b.cwd, "label": label, "focus": false})).await,
    };
    let created = match created {
        Ok(c) => c,
        Err(e) => return herdr_err(e),
    };
    let pane = created["root_pane"]["pane_id"].as_str().unwrap_or("").to_string();
    let tab = created["tab"]["tab_id"].as_str().or(created["root_pane"]["tab_id"].as_str()).unwrap_or("").to_string();
    let wsid = created["root_pane"]["workspace_id"].as_str().unwrap_or("").to_string();
    if pane.is_empty() {
        return json_err(StatusCode::BAD_GATEWAY, "herdr did not return a pane");
    }
    if b.shell == Some(true) {
        app.refresh.notify_one();
        return Json(json!({"pane": pane, "tab": tab, "ws": wsid})).into_response();
    }
    let args = b.args.clone().unwrap_or_else(newagent::default_args);
    let name = agent_slug(&label);
    let prompt = b.prompt.clone().filter(|p| !p.trim().is_empty());
    // Booting until Claude is ready: the first prompt, and any the phone sends meanwhile, wait in order
    // (the agent may also sit on a startup dialog such as folder trust).
    newagent::boot_begin(&pane, prompt);
    if b.label.as_deref().is_some_and(|l| !l.trim().is_empty()) {
        newagent::labelled(&pane);
    }
    let app2 = app.clone();
    let pane2 = pane.clone();
    tokio::spawn(async move {
        if let Err(e) = start_claude(&pane2, &name, args).await {
            eprintln!("agent.start {pane2}: {e}");
        }
        app2.refresh.notify_one();
        newagent::finish_boot(&app2, &pane2).await;
    });
    app.refresh.notify_one();
    Json(json!({"pane": pane, "tab": tab, "ws": wsid})).into_response()
}

#[derive(Deserialize, Default)]
struct HomeNewBody {
    cwd: Option<String>,
}

/// Starts a fresh master chat (new session) and pins it as home. The previous
/// home keeps running as an ordinary agent; its tab is relabelled after its title.
async fn api_home_new(State(app): State<Shared>, body: Option<Json<HomeNewBody>>) -> Response {
    let cwd = body
        .and_then(|Json(b)| b.cwd)
        .filter(|c| Path::new(c).is_dir())
        // the work dir: usually already trusted (Claude never persists trust for $HOME, so a
        // master there would stall on the trust dialog) and shares its memory.
        .unwrap_or_else(|| work_dir().to_string_lossy().to_string());
    let (prev_pane, prev_tab, prev_title, ws) = {
        let i = app.inner.lock().unwrap();
        let prev = i.home.as_ref().filter(|_| i.home_live).and_then(|h| h["pane"].as_str().map(str::to_string));
        let tab = prev.as_ref().and_then(|p| i.pane_tabs.get(p).cloned());
        let title = prev.as_ref().and_then(|p| i.pane_titles.get(p).cloned());
        let st: Value = serde_json::from_str(&i.state).unwrap_or(Value::Null);
        let ws = st["workspaces"]
            .as_array()
            .and_then(|l| l.iter().find(|w| w["cwd"].as_str() == Some(cwd.as_str())).and_then(|w| w["ws"].as_str().map(str::to_string)));
        (prev, tab, title, ws)
    };
    let created = match &ws {
        Some(w) => herdr::quick("tab.create", json!({"workspace_id": w, "cwd": cwd, "label": "master", "focus": false})).await,
        None => herdr::quick("workspace.create", json!({"cwd": cwd, "focus": false})).await,
    };
    let created = match created {
        Ok(c) => c,
        Err(e) => return herdr_err(e),
    };
    let pane = created["root_pane"]["pane_id"].as_str().unwrap_or("").to_string();
    if ws.is_none() {
        if let Some(t) = created["tab"]["tab_id"].as_str().or(created["root_pane"]["tab_id"].as_str()) {
            let _ = herdr::quick("tab.rename", json!({"tab_id": t, "label": "master"})).await;
        }
    }
    if let (Some(_), Some(tab)) = (&prev_pane, &prev_tab) {
        let label: String = prev_title.unwrap_or_else(|| "previous".into()).chars().take(24).collect();
        let _ = herdr::quick("tab.rename", json!({"tab_id": tab, "label": label})).await;
    }
    {
        let mut i = app.inner.lock().unwrap();
        let next = json!({"pane": pane, "session": null, "cwd": cwd, "ws": created["root_pane"]["workspace_id"], "label": "master"});
        save_home(&next);
        i.home = Some(next);
        i.home_live = false;
    }
    let pane2 = pane.clone();
    let app2 = app.clone();
    tokio::spawn(async move {
        if let Err(e) = start_claude(&pane2, &agent_slug("master"), vec!["--permission-mode".into(), "auto".into()]).await {
            eprintln!("master agent.start: {e}");
        }
        app2.refresh.notify_one();
    });
    app.refresh.notify_one();
    Json(json!({"pane": pane, "previous": prev_pane})).into_response()
}

async fn api_home(State(app): State<Shared>) -> Response {
    let i = app.inner.lock().unwrap();
    match (&i.home, i.home_live) {
        (Some(h), true) => Json(json!({"pane": h["pane"], "session": h["session"]})).into_response(),
        _ => Json(json!({"pane": null, "session": null})).into_response(),
    }
}

/// Returns the live home pane, or recreates it: a tab in its old workspace
/// running `claude --resume <last session>` so the long-running context carries on.
async fn api_home_ensure(State(app): State<Shared>) -> Response {
    let (live, pinned) = {
        let i = app.inner.lock().unwrap();
        (i.home_live, i.home.clone())
    };
    if live {
        if let Some(h) = &pinned {
            return Json(json!({"pane": h["pane"], "revived": false})).into_response();
        }
    }
    let h = pinned.unwrap_or(json!({}));
    let cwd = h["cwd"].as_str().filter(|c| Path::new(c).is_dir()).map(str::to_string)
        .unwrap_or_else(|| work_dir().to_string_lossy().to_string());
    let label = h["label"].as_str().unwrap_or("master").to_string();
    let ws_exists = {
        let st: Value = serde_json::from_str(&app.inner.lock().unwrap().state).unwrap_or(Value::Null);
        st["workspaces"].as_array().map(|l| l.iter().any(|w| w["ws"] == h["ws"])).unwrap_or(false)
    };
    let created = if ws_exists {
        herdr::quick("tab.create", json!({"workspace_id": h["ws"], "cwd": cwd, "label": label, "focus": false})).await
    } else {
        herdr::quick("workspace.create", json!({"cwd": cwd, "label": "home", "focus": false})).await
    };
    let created = match created {
        Ok(c) => c,
        Err(e) => return herdr_err(e),
    };
    let pane = created["root_pane"]["pane_id"].as_str().unwrap_or("").to_string();
    let mut args = vec!["--permission-mode".to_string(), "auto".to_string()];
    if let Some(sid) = h["session"].as_str() {
        if find_session(&app, sid).is_some() {
            args.push("--resume".into());
            args.push(sid.into());
        }
    }
    {
        let mut i = app.inner.lock().unwrap();
        let next = json!({"pane": pane, "session": h["session"], "cwd": cwd, "ws": created["root_pane"]["workspace_id"], "label": label});
        save_home(&next);
        i.home = Some(next);
    }
    let pane2 = pane.clone();
    let app2 = app.clone();
    tokio::spawn(async move {
        if let Err(e) = start_claude(&pane2, &agent_slug("home"), args).await {
            eprintln!("home agent.start: {e}");
        }
        app2.refresh.notify_one();
    });
    app.refresh.notify_one();
    Json(json!({"pane": pane, "revived": true})).into_response()
}

#[derive(Deserialize)]
struct UploadQ {
    name: Option<String>,
}

async fn api_upload(Query(q): Query<UploadQ>, headers: HeaderMap, body: Bytes) -> Response {
    let dir = home().join(".cache/sheprd/uploads");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let given = q.name.unwrap_or_default();
    let ext = Path::new(&given)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| {
            match ct {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/heic" => "heic",
                "image/gif" => "gif",
                "image/webp" => "webp",
                "application/pdf" => "pdf",
                _ => "bin",
            }
            .into()
        });
    let path = dir.join(format!("{}.{ext}", now_ms()));
    match tokio::fs::write(&path, &body).await {
        Ok(_) => Json(json!({"path": path})).into_response(),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct FileQ {
    path: String,
    tail: Option<u64>,
    /// `?download=1`: Content-Disposition attachment (the phone's Save fallback)
    download: Option<String>,
}

fn content_type(p: &Path) -> &'static str {
    match p.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("heic") => "image/heic",
        Some("pdf") => "application/pdf",
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "webmanifest") => "application/json",
        Some("avif") => "image/avif",
        Some("mp4") => "video/mp4",
        Some("m4v") => "video/x-m4v",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("mkv") => "video/x-matroska",
        Some("ogv") => "video/ogg",
        Some("mp3") => "audio/mpeg",
        Some("m4a") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("wav") => "audio/wav",
        Some("ogg" | "oga") => "audio/ogg",
        Some("opus") => "audio/opus",
        Some("flac") => "audio/flac",
        Some("woff2") => "font/woff2",
        Some("ico") => "image/x-icon",
        Some(
            "txt" | "md" | "log" | "rs" | "py" | "ts" | "tsx" | "toml" | "yaml" | "yml" | "sh" | "csv" | "jsonl" | "c" | "h"
            | "cpp" | "go" | "lua" | "conf",
        ) => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn api_file(headers: HeaderMap, Query(q): Query<FileQ>) -> Response {
    let download = q.download.as_deref().is_some_and(|v| !v.is_empty() && v != "0");
    let Ok(p) = tokio::fs::canonicalize(&q.path).await else {
        // gone (an agent's /tmp screenshot): the copy kept when it was surfaced or last shown (keep.rs)
        let raw = q.path.clone();
        if let Ok(Some(k)) = tokio::task::spawn_blocking(move || keep::find(&raw)).await {
            if k.is_file() {
                return ranged::serve(&k, content_type(&k), &headers, download).await;
            }
        }
        return json_err(StatusCode::NOT_FOUND, "not found");
    };
    let allowed = [home(), PathBuf::from("/tmp")];
    if !allowed.iter().any(|r| p.starts_with(r)) {
        return json_err(StatusCode::FORBIDDEN, "outside allowed roots");
    }
    if p.is_dir() {
        let mut entries = Vec::new();
        if let Ok(mut rd) = tokio::fs::read_dir(&p).await {
            while let Ok(Some(e)) = rd.next_entry().await {
                let dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                entries.push(json!({"name": e.file_name().to_string_lossy(), "dir": dir}));
            }
        }
        return Json(json!({"path": p, "entries": entries})).into_response();
    }
    if let Some(n) = q.tail {
        // The last `n` bytes as text, starting at a line boundary (F11 job output).
        let r = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::File::open(&p)?;
            let len = f.metadata()?.len();
            let start = len.saturating_sub(n.min(200_000));
            f.seek(SeekFrom::Start(start))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            if start > 0 {
                let nl = buf.iter().position(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
                buf.drain(..nl);
            }
            Ok(buf)
        })
        .await;
        return match r {
            Ok(Ok(b)) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], b).into_response(),
            Ok(Err(e)) => json_err(StatusCode::NOT_FOUND, e),
            Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e),
        };
    }
    // streamed from disk, honouring Range (iOS plays <video> only with 206 Partial Content; ranged.rs)
    let ct = content_type(&p);
    let mut r = ranged::serve(&p, ct, &headers, download).await;
    if !r.status().is_success() {
        return r;
    }
    if ct.starts_with("image/") && !ct.starts_with("image/svg") {
        let (raw, p2) = (q.path.clone(), p.clone());
        tokio::task::spawn_blocking(move || keep::stash(&raw, &p2));
    }
    // Agent-written HTML/SVG runs in an opaque-origin sandbox that cannot call this API (artifacts.rs).
    if ct.starts_with("text/html") || ct.starts_with("image/svg") {
        r.headers_mut().insert(header::CONTENT_SECURITY_POLICY, header::HeaderValue::from_static(artifacts::CSP));
    }
    r
}

async fn api_push_key(State(app): State<Shared>) -> Response {
    match &app.push {
        Some(p) => Json(json!({"key": p.public_key})).into_response(),
        None => json_err(StatusCode::SERVICE_UNAVAILABLE, "push unavailable"),
    }
}

async fn api_push_sub(State(app): State<Shared>, Json(v): Json<Value>) -> Response {
    let Some(p) = &app.push else { return json_err(StatusCode::SERVICE_UNAVAILABLE, "push unavailable") };
    match p.subscribe(&v) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, e),
    }
}

async fn api_push_test(State(app): State<Shared>) -> Response {
    if app.push.is_none() {
        return json_err(StatusCode::SERVICE_UNAVAILABLE, "push unavailable");
    }
    send_push(&app, &json!({"title": "sheprd", "body": "Push notifications work.", "url": "/#/", "tag": "test"}), true).await;
    Json(json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct ViewingBody {
    pane: Option<String>,
    visible: Option<bool>,
}

/// Client-side errors (window.onerror etc.) land in the service journal, so
/// problems on the phone are visible here.
async fn api_clientlog(headers: HeaderMap, body: Bytes) -> Response {
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let text = String::from_utf8_lossy(&body[..body.len().min(4000)]).to_string();
    eprintln!("client error [{}]: {}", transcript::cut(ua, 80).0, text);
    StatusCode::NO_CONTENT.into_response()
}

async fn api_viewing(State(app): State<Shared>, Json(b): Json<ViewingBody>) -> Response {
    let visible = b.visible.unwrap_or(true);
    let now = now_ms();
    let home = {
        let mut inner = app.inner.lock().unwrap();
        inner.viewing = Some((b.pane.clone(), visible, now));
        if visible {
            inner.last_visible = now;
        }
        inner.home.as_ref().and_then(|h| h["pane"].as_str()).map(str::to_string)
    };
    if visible {
        auto_clear_surfaces(&app, b.pane.filter(|p| home.as_deref() != Some(p.as_str())), now);
    }
    Json(json!({"ok": true})).into_response()
}

const AUTO_CLEAR_LOOK_MS: u64 = 4_000;
const AUTO_CLEAR_AGE_MS: i64 = 24 * 3600_000;

/// Cards clear themselves (surfaces::auto_clearable: plain info/done notes only): a card from the agent whose
/// chat the user has had open for 4 s (not Master: Home always shows its cards), and any such card older than a day
/// once the app is open again. Through `dismiss_surfaces`, so their fingerprints stay dismissed.
fn auto_clear_surfaces(app: &Shared, pane: Option<String>, since: i64) {
    let old = |s: &Value| surfaces::auto_clearable(s) && s["ts"].as_i64().is_some_and(|t| since - t > AUTO_CLEAR_AGE_MS);
    if app.inner.lock().unwrap().surfaces.iter().any(old) {
        let n = dismiss_surfaces(app, old);
        eprintln!("surfaces: {n} day-old notes cleared");
    }
    let Some(pane) = pane else { return };
    let app = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(AUTO_CLEAR_LOOK_MS)).await;
        let still = matches!(&app.inner.lock().unwrap().viewing, Some((Some(p), true, t)) if *p == pane && *t == since);
        if !still {
            return;
        }
        let seen = |s: &Value| surfaces::auto_clearable(s) && s["pane"].as_str() == Some(pane.as_str()) && s["ts"].as_i64().is_some_and(|t| t < since);
        if app.inner.lock().unwrap().surfaces.iter().any(seen) {
            let n = dismiss_surfaces(&app, seen);
            eprintln!("surfaces: {n} cleared after looking at {pane}");
        }
    });
}

async fn api_surface(State(app): State<Shared>, Json(mut b): Json<Value>) -> Response {
    v3::redirect_surface(&app, &mut b); // a reviewer's result goes to the reviewed agent
    let title = b["title"].as_str().unwrap_or("").trim().to_string();
    if title.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "title required");
    }
    // The mod's automatic "Dev server :<port>" card (any localhost URL a command printed) only when the phone can
    // open that port, linked to the address it can reach: a loopback-only or stopped server gave a blank page
    // (2026-10-10). Those still show in Home's Running apps, with why the phone can't open them.
    if let Some(port) = title.strip_prefix("Dev server :").and_then(|p| p.parse::<u16>().ok()) {
        let ts_ip = app.ts_ip.clone();
        match tokio::task::spawn_blocking(move || apps::phone_url(port, &ts_ip)).await.ok().flatten() {
            Some(url) => b["links"] = json!([{"url": url, "label": format!(":{port}")}]),
            None => return Json(json!({"ok": true, "skipped": "the phone can't reach that port"})).into_response(),
        }
    }
    let level = match b["level"].as_str() {
        Some("attention") => "attention",
        Some("done") => "done",
        _ => "info",
    };
    let mut session = b["session"].as_str().map(str::to_string);
    let mut pane = b["pane"].as_str().map(str::to_string);
    let agent_title;
    let shown_who; // the push says the home agent's display name; the card's agentTitle stays the herdr title
    {
        let inner = app.inner.lock().unwrap();
        if pane.is_none() {
            if let Some(s) = &session {
                pane = inner.pane_sessions.iter().find(|(_, v)| *v == s).map(|(p, _)| p.clone());
            }
        }
        if session.is_none() {
            if let Some(p) = &pane {
                session = inner.pane_sessions.get(p).cloned();
            }
        }
        agent_title = pane.as_ref().and_then(|p| inner.pane_titles.get(p).cloned());
        shown_who = pane.as_ref().and_then(|p| inner.shown_title(p));
    }
    let raw_images: Vec<String> = b["images"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    // keep a copy so the card's picture outlives the agent's temp file (keep.rs); same roots as /api/file
    let to_keep = raw_images.clone();
    tokio::task::spawn_blocking(move || {
        for raw in to_keep {
            if let Ok(p) = std::fs::canonicalize(&raw) {
                if p.starts_with(home()) || p.starts_with("/tmp") {
                    keep::stash(&raw, &p);
                }
            }
        }
    });
    let images: Vec<String> = raw_images.iter().map(|p| format!("/api/file?path={}", urlencode(p))).collect();
    let links: Vec<Value> = b["links"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| {
                    let url = l["url"].as_str().or(l.as_str())?;
                    Some(json!({"url": url, "label": l["label"].as_str().unwrap_or(url)}))
                })
                .collect()
        })
        .unwrap_or_default();
    let replies: Vec<String> = b["replies"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).filter(|r| !r.trim().is_empty()).take(4).map(|r| r.chars().take(40).collect()).collect())
        .unwrap_or_default();
    let command = b["command"].as_str().filter(|c| !c.trim().is_empty()).map(|c| transcript::cut(c, 500).0);
    let (level, shown, push_url) = (level, title.clone(), b["pushUrl"].as_str().filter(|u| u.starts_with("/#/")).map(str::to_string));
    let id = format!("s{:x}", now_ms());
    // Where it happened in the chat: the surface tool call with this title. Claude writes that row a few seconds
    // after the tool ran, so the item is stored and broadcast now with `i: null` and `i` is resolved in the
    // background (pushes wait for it, ~8 s at most). Surfaces the mod makes itself (`tool: false`) never match:
    // they take the newest item at once and push without waiting. An older mod sends no flag: wait as for a tool.
    let from_tool = b["tool"].as_bool() != Some(false);
    let body_text = b["body"].as_str().unwrap_or("").to_string();
    let item = json!({
        "id": id, "ts": now_ms(), "pane": pane, "session": session, "tid": session,
        "agentTitle": agent_title, "title": shown, "body": body_text,
        "level": level, "images": images, "links": links, "replies": replies, "command": command, "i": null,
        "review": b.get("review").cloned().unwrap_or(Value::Null),
    });
    let fps = surfaces::keys(session.as_deref(), pane.as_deref(), &shown, item["body"].as_str().unwrap_or(""));
    let claimed: Vec<usize> = {
        let mut inner = app.inner.lock().unwrap();
        // the same note from the same agent: dismissed before → not shown, toasted or pushed again;
        // still on screen → this one replaces it (server-made surfaces with their own link always go through)
        let Inner { surfaces: list, surf_gone, .. } = &mut *inner;
        if push_url.is_none() && !surfaces::admit(list, surf_gone, &fps) {
            return Json(json!({"ok": true, "id": id, "suppressed": "dismissed before"})).into_response();
        }
        let claimed = inner
            .surfaces
            .iter()
            .filter(|s| s["tid"].as_str().is_some() && s["tid"].as_str() == session.as_deref())
            .filter_map(|s| s["i"].as_u64().map(|i| i as usize))
            .collect();
        inner.surfaces.push_front(item.clone());
        inner.surfaces.truncate(200);
        save_surfaces(&inner.surfaces);
        claimed
    };
    let _ = app.events.send(("surface", Arc::new(item.to_string())));
    republish(&app);
    let tx = session.as_deref().and_then(|s| transcript(&app, s));
    let app2 = app.clone();
    let (id2, body) = (id.clone(), Some(body_text.clone()).filter(|t| !t.is_empty()));
    tokio::spawn(async move {
        let (mut at, mut row) = (None, false);
        if let Some(t) = tx {
            let mut fallback = None;
            for k in 0..if from_tool { 28 } else { 1 } {
                if k > 0 {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                let (t, title, claimed) = (t.clone(), title.clone(), claimed.clone());
                let Ok((hit, last)) = tokio::task::spawn_blocking(move || {
                    let mut t = t.lock().unwrap();
                    t.refresh();
                    (surface_row(&t.items, &title, &claimed), t.items.len().checked_sub(1))
                })
                .await
                else {
                    break;
                };
                fallback = fallback.or(last);
                if hit.is_some() {
                    (at, row) = (hit, true);
                    break;
                }
            }
            at = at.or(fallback);
        }
        if at.is_some() {
            let mut inner = app2.inner.lock().unwrap();
            if let Some(s) = inner.surfaces.iter_mut().find(|s| s["id"] == id2.as_str()) {
                s["i"] = json!(at);
                s["row"] = json!(row); // a surface tool row of its own: the chat draws it from there
            }
            save_surfaces(&inner.surfaces);
        }
        republish(&app2);
        if level != "info" && !v3::at_desk(&app2, pane.as_deref().unwrap_or("")).await {
            let who = shown_who.unwrap_or_else(|| "agent".into());
            // `pushUrl`: a deep link of the server's own, else Home's "You should see this" panel at this card
            let url = push_url.unwrap_or_else(|| see_url(&id2));
            let body = v3::surface_body(body.as_deref(), item["command"].as_str(), &who);
            let payload = json!({"title": shown, "body": transcript::cut(&body, 300).0, "url": url, "tag": id2});
            send_push(&app2, &payload, level == "attention").await;
        }
    });
    Json(json!({"ok": true, "id": id})).into_response()
}

/// F3 hints from the mod: a pending AskUserQuestion (`ask`) or its end (`ask_clear`).
async fn api_hint(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let pane = {
        let inner = app.inner.lock().unwrap();
        b["pane"].as_str().map(str::to_string).or_else(|| {
            let s = b["session"].as_str()?;
            inner.pane_sessions.iter().find(|(_, v)| v.as_str() == s).map(|(p, _)| p.clone())
        })
    };
    let Some(pane) = pane else { return json_err(StatusCode::NOT_FOUND, "unknown agent") };
    match b["kind"].as_str() {
        Some("ask") => {
            let Some(qs) = b["questions"].as_array().filter(|q| !q.is_empty()) else {
                return json_err(StatusCode::BAD_REQUEST, "questions required");
            };
            let ask = Ask { ts: now_ms(), questions: Value::Array(qs.iter().take(4).cloned().collect()) };
            app.inner.lock().unwrap().asks.insert(pane, ask);
        }
        Some("ask_clear") => {
            app.inner.lock().unwrap().asks.remove(&pane);
        }
        Some("plan" | "plan_clear") => {
            if let Err(e) = v3::plan_hint(&app, &pane, &b) {
                return json_err(StatusCode::BAD_REQUEST, e);
            }
        }
        _ => return json_err(StatusCode::BAD_REQUEST, "unknown kind"),
    }
    app.refresh.notify_one();
    Json(json!({"ok": true})).into_response()
}

async fn api_prefs_get(State(app): State<Shared>) -> Response {
    let v = app.prefs.lock().unwrap().v.clone();
    Json(v).into_response()
}

async fn api_prefs_post(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    if let Some(r) = b.get("resAlerts") {
        let cfg = alerts::set_prefs(&app, r);
        if b.get("snippets").is_none() {
            return Json(json!({"ok": true, "resAlerts": cfg})).into_response();
        }
    }
    let Some(list) = b["snippets"].as_array() else { return json_err(StatusCode::BAD_REQUEST, "snippets required") };
    match app.prefs.lock().unwrap().set_snippets(list) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => json_err(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
struct LimitQ {
    limit: Option<usize>,
}

/// F13: distinct recent human prompts.
async fn api_prompts(Query(q): Query<LimitQ>) -> Response {
    let limit = q.limit.unwrap_or(30).clamp(1, 200);
    let list = tokio::task::spawn_blocking(move || scan::recent_prompts(limit)).await.unwrap_or_default();
    let prompts: Vec<Value> = list.into_iter().map(|(text, ts, tid, n)| json!({"text": text, "ts": ts, "tid": tid, "n": n})).collect();
    trim_heap();
    Json(json!({"prompts": prompts})).into_response()
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
unsafe extern "C" {
    fn malloc_trim(pad: usize) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

/// One malloc arena: blocking-pool threads would otherwise each keep their own
/// arena full of freed transcript-parse memory (RSS doubled after a scan).
fn single_arena() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        mallopt(-8, 1); // M_ARENA_MAX
    }
}

/// Hands freed heap back to the OS after a big transient scan (glibc only).
fn trim_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        malloc_trim(0);
    }
}

#[derive(Deserialize)]
struct SearchQ {
    q: String,
    limit: Option<usize>,
}

/// F10: substring search over human prompts and assistant text of all main
/// transcripts, newest first, within a 1.5 s budget.
async fn api_search(State(app): State<Shared>, Query(sq): Query<SearchQ>) -> Response {
    let q = sq.q.trim().to_lowercase();
    let n = q.chars().count();
    if !(2..=100).contains(&n) {
        return json_err(StatusCode::BAD_REQUEST, "q must be 2-100 chars");
    }
    let limit = sq.limit.unwrap_or(40).clamp(1, 100);
    let app2 = app.clone();
    let q2 = q.clone();
    let (results, partial) = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        let mut out: Vec<Value> = Vec::new();
        let mut partial = false;
        for (path, _) in scan::main_transcripts() {
            if out.len() >= limit {
                break;
            }
            if std::time::Instant::now() > deadline {
                partial = true;
                break;
            }
            let (hits, cut) = scan::search_file(&path, &q2, deadline, limit - out.len());
            partial |= cut;
            if hits.is_empty() {
                continue;
            }
            let tid = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            // Item indexes come from the parsed transcript: the cached one, else a
            // throwaway parse (search hits are never cached).
            let cached = app2.transcripts.lock().unwrap().get(&tid).cloned();
            let tx = cached.unwrap_or_else(|| Arc::new(Mutex::new(Transcript::new(tid.clone(), path.clone()))));
            let mut t = tx.lock().unwrap();
            t.refresh();
            let pane = pane_for_session(&app2, &tid);
            for h in hits.iter().rev() {
                let want = if h.role == "user" { "user" } else { "text" };
                let i = t
                    .items
                    .iter()
                    .filter(|it| it.off == h.off && it.k == want)
                    .find(|it| it.text.as_deref().is_some_and(|x| x.to_lowercase().contains(&q2)))
                    .or_else(|| t.items.iter().find(|it| it.off == h.off))
                    .map(|it| it.i);
                out.push(json!({
                    "tid": tid, "i": i, "ts": h.ts, "role": h.role, "title": t.title(), "cwd": t.cwd,
                    "pane": pane, "snippet": h.snippet,
                }));
            }
        }
        out.truncate(limit);
        (out, partial)
    })
    .await
    .unwrap_or_default();
    trim_heap();
    Json(json!({"q": q, "partial": partial, "results": results})).into_response()
}

/// F11: background Bash jobs of the cached main transcripts (live agents and
/// recently opened chats).
async fn api_jobs(State(app): State<Shared>) -> Response {
    let txs: Vec<Tx> = app.transcripts.lock().unwrap().iter().filter(|(k, _)| !k.contains('~')).map(|(_, t)| t.clone()).collect();
    let (pane_of, titles): (HashMap<String, String>, HashMap<String, String>) = {
        let i = app.inner.lock().unwrap();
        (i.pane_sessions.iter().map(|(p, s)| (s.clone(), p.clone())).collect(), i.pane_titles.clone())
    };
    let (ts_name, ts_ip) = (app.ts_name.clone(), app.ts_ip.clone());
    let mut jobs: Vec<Value> = tokio::task::spawn_blocking(move || {
        let now = now_ms();
        let mut out = Vec::new();
        for tx in txs {
            let mut t = tx.lock().unwrap();
            t.refresh();
            if t.jobs.is_empty() {
                continue;
            }
            let mtime = std::fs::metadata(&t.path).map(|m| transcript::mtime_ms(&m)).unwrap_or(0);
            let pane = pane_of.get(&t.tid).cloned();
            let title = pane.as_ref().and_then(|p| titles.get(p).cloned()).or(t.title().map(str::to_string));
            for j in &t.jobs {
                if j.ended.is_some_and(|e| now - e > 24 * 3600_000) {
                    continue;
                }
                let mut v = json!(j);
                if j.ended.is_none() && now - mtime > 2 * 3600_000 {
                    v["status"] = json!("unknown");
                }
                v["pane"] = json!(pane);
                v["tid"] = json!(t.tid);
                v["agentTitle"] = json!(title);
                // a server (http.server, npm run dev, tailscale serve…) is a running app, not work in flight
                if v["status"] == "running" {
                    let a = apps::info(&j.id, &j.cmd, &ts_name, &ts_ip);
                    if !a.is_null() {
                        v["app"] = a;
                    }
                }
                out.push(v);
            }
        }
        out
    })
    .await
    .unwrap_or_default();
    let key = |j: &Value| (j["status"] != "running", -j["ended"].as_i64().unwrap_or(j["started"].as_i64().unwrap_or(0)));
    jobs.sort_by_key(key);
    res::decorate_jobs(&mut jobs);
    apps::remember(&jobs);
    Json(json!({"jobs": jobs})).into_response()
}

/// Runs `git status` in `cwd` (2 s timeout): {cwd, branch, changed:[{path, st}]}, or null.
async fn git_status(cwd: &str) -> Value {
    let out = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("git").args(["-C", cwd, "status", "--porcelain=v1", "-b"]).kill_on_drop(true).output(),
    )
    .await;
    let Ok(Ok(out)) = out else { return Value::Null };
    if !out.status.success() {
        return Value::Null;
    }
    parse_git_status(cwd, &String::from_utf8_lossy(&out.stdout))
}

fn parse_git_status(cwd: &str, text: &str) -> Value {
    let mut branch = Value::Null;
    let mut changed = Vec::new();
    for l in text.lines() {
        if let Some(b) = l.strip_prefix("## ") {
            let b = b.split("...").next().unwrap_or(b).split(' ').next().unwrap_or(b);
            branch = json!(b.strip_prefix("No commits yet on ").unwrap_or(b));
        } else if l.len() > 3 {
            let path = l[3..].rsplit(" -> ").next().unwrap_or(&l[3..]).trim_matches('"');
            changed.push(json!({"path": path, "st": l[..2].trim()}));
        }
    }
    json!({"cwd": cwd, "branch": branch, "changed": changed})
}

/// Visits the main transcript `tid` (refreshed) and then each subagent
/// transcript. Subagents not in the cache are parsed one at a time for this
/// request only and dropped right after their visit. False if `tid` is unknown.
fn each_session_transcript(app: &App, tid: &str, mut f: impl FnMut(usize, &mut Transcript)) -> bool {
    let Some(main) = transcript(app, tid) else { return false };
    let path = {
        let mut t = main.lock().unwrap();
        t.refresh();
        f(0, &mut t);
        t.path.clone()
    };
    for (n, s) in transcript::list_subagents(&path, tid).into_iter().enumerate() {
        let cached = app.transcripts.lock().unwrap().get(&s.tid).cloned();
        match cached {
            Some(tx) => {
                let mut t = tx.lock().unwrap();
                t.refresh();
                f(n + 1, &mut t);
            }
            None => {
                let mut t = Transcript::new(s.tid.clone(), transcript::subagent_path(&path, &s.agent_id));
                t.refresh();
                f(n + 1, &mut t);
            }
        }
    }
    true
}

/// F8: files edited in a session (and its subagents), newest first, plus git status.
async fn api_files(State(app): State<Shared>, AxPath(tid): AxPath<String>) -> Response {
    if tid.contains('~') {
        return json_err(StatusCode::BAD_REQUEST, "main session tid required");
    }
    let app2 = app.clone();
    let tid2 = tid.clone();
    let r = tokio::task::spawn_blocking(move || {
        let mut files: HashMap<String, Value> = HashMap::new();
        let mut cwd = None;
        let found = each_session_transcript(&app2, &tid2, |n, t| {
            if n == 0 {
                cwd = t.cwd.clone();
            }
            for (path, touch) in &t.touched {
                let f = files.entry(path.clone()).or_insert_with(|| {
                    json!({"path": path, "edits": 0, "writes": 0, "lastI": null, "lastTid": null, "lastTs": 0, "subs": []})
                });
                f["edits"] = json!(f["edits"].as_u64().unwrap_or(0) + touch.edits as u64);
                f["writes"] = json!(f["writes"].as_u64().unwrap_or(0) + touch.writes as u64);
                if n > 0 {
                    f["subs"].as_array_mut().unwrap().push(json!(t.tid));
                }
                // lastI points into the main transcript when it touched the file.
                if f["lastI"].is_null() || (n > 0 && f["lastTid"] != json!(tid2) && touch.last_ts > f["lastTs"].as_i64().unwrap_or(0)) {
                    f["lastI"] = json!(touch.last_i);
                    f["lastTid"] = json!(t.tid);
                }
                if touch.last_ts > f["lastTs"].as_i64().unwrap_or(0) {
                    f["lastTs"] = json!(touch.last_ts);
                }
            }
        });
        if !found {
            return None;
        }
        let mut files: Vec<Value> = files.into_values().collect();
        files.sort_by_key(|f| -f["lastTs"].as_i64().unwrap_or(0));
        Some((files, cwd))
    })
    .await
    .ok()
    .flatten();
    trim_heap();
    let Some((files, cwd)) = r else {
        if pending_session(&app, &tid) {
            // A fresh session whose transcript file does not exist yet: nothing touched so far.
            return Json(json!({"files": [], "git": null})).into_response();
        }
        return json_err(StatusCode::NOT_FOUND, "no such transcript");
    };
    let git = match &cwd {
        Some(c) => git_status(c).await,
        None => Value::Null,
    };
    Json(json!({"files": files, "git": git})).into_response()
}

#[derive(Deserialize)]
struct PathQ {
    path: String,
}

/// F8: every change to one file in the session and its subagents, oldest first.
async fn api_files_diff(State(app): State<Shared>, AxPath(tid): AxPath<String>, Query(q): Query<PathQ>) -> Response {
    let path = q.path.clone();
    let pending = pending_session(&app, &tid);
    let r = tokio::task::spawn_blocking(move || {
        let mut changes: Vec<Value> = Vec::new();
        let found = each_session_transcript(&app, &tid, |_, t| {
            let items = t.touched.get(&path).map(|x| x.items.clone()).unwrap_or_default();
            for ix in items {
                changes.extend(t.changes_at(ix));
            }
        });
        if !found {
            return None;
        }
        changes.sort_by_key(|c| (c["ts"].as_i64().unwrap_or(0), c["i"].as_u64().unwrap_or(0)));
        Some(changes)
    })
    .await
    .ok()
    .flatten();
    trim_heap();
    match r {
        Some(changes) => Json(json!({"path": q.path, "changes": changes})).into_response(),
        None if pending => Json(json!({"path": q.path, "changes": []})).into_response(),
        None => json_err(StatusCode::NOT_FOUND, "no such transcript"),
    }
}

fn save_surfaces(s: &VecDeque<Value>) {
    if let Ok(b) = serde_json::to_vec(s) {
        surfaces::write_atomic(&state_dir().join("surfaces.json"), &b);
    }
}

/// Dismisses the surfaces `pick` selects (and identical copies), saved with the surfaces and as fingerprints.
fn dismiss_surfaces(app: &App, pick: impl Fn(&Value) -> bool) -> usize {
    let n = {
        let mut inner = app.inner.lock().unwrap();
        let Inner { surfaces: list, surf_gone, .. } = &mut *inner;
        let n = surfaces::dismiss(list, surf_gone, now_ms(), pick);
        save_surfaces(list);
        surf_gone.save(&state_dir().join("surfaces-dismissed.json"));
        n
    };
    republish(app);
    n
}

/// One card by id (a push-opened `#/see/<id>`), dismissed ones included: `{surface, gone}`; 404 `{gone:true}`
/// when the server no longer has it.
async fn api_surface_get(State(app): State<Shared>, AxPath(id): AxPath<String>) -> Response {
    let inner = app.inner.lock().unwrap();
    match inner.surfaces.iter().find(|s| s["id"].as_str() == Some(id.as_str())) {
        Some(s) => Json(json!({"surface": s, "gone": s["dismissed"].as_bool().unwrap_or(false)})).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"gone": true}))).into_response(),
    }
}

async fn api_surface_dismiss(State(app): State<Shared>, AxPath(id): AxPath<String>) -> Response {
    let n = dismiss_surfaces(&app, |s| s["id"].as_str() == Some(id.as_str()));
    Json(json!({"ok": true, "found": n > 0})).into_response()
}

/// Clear all: `{ids: [...]}` dismisses those (what the phone shows, so a card arriving meanwhile stays);
/// without `ids`, every surface.
async fn api_surfaces_dismiss(State(app): State<Shared>, body: Option<Json<Value>>) -> Response {
    let ids: Option<Vec<String>> = body.and_then(|Json(b)| {
        b["ids"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
    });
    let n = dismiss_surfaces(&app, |s| {
        !s["dismissed"].as_bool().unwrap_or(false)
            && ids.as_ref().is_none_or(|l| l.iter().any(|i| s["id"].as_str() == Some(i.as_str())))
    });
    Json(json!({"ok": true, "n": n})).into_response()
}

fn urlencode(s: &str) -> String {
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

/// Hash of every web asset (embedded, or `SHEPRD_WEB_DIR` on disk during development).
async fn shell_hash(web_dir: Option<&Path>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match web_dir {
        None => assets::ASSETS.iter().for_each(|(p, b)| (p, b).hash(&mut h)),
        Some(d) => {
            let mut files: Vec<PathBuf> = std::fs::read_dir(d).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
            files.sort();
            for f in files {
                if let Ok(b) = tokio::fs::read(&f).await {
                    (f, b).hash(&mut h);
                }
            }
        }
    }
    h.finish()
}

async fn static_file(State(app): State<Shared>, headers: HeaderMap, uri: Uri) -> Response {
    let mut rel = uri.path().trim_start_matches('/').to_string();
    if rel.is_empty() {
        rel = "index.html".into();
    }
    if rel.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ct = content_type(Path::new(&rel));
    let bytes: Option<Vec<u8>> = match &app.web_dir {
        Some(d) => tokio::fs::read(d.join(&rel)).await.ok(),
        None => assets::ASSETS.iter().find(|(p, _)| *p == rel).map(|(_, b)| b.to_vec()),
    };
    let Some(mut b) = bytes else { return StatusCode::NOT_FOUND.into_response() };
    // The service worker's cache name follows the content of the whole web app, so every upgrade
    // replaces the phone's cached shell without anyone bumping a version by hand.
    if rel == "sw.js" {
        let v = format!("sheprd-shell-{:x}", shell_hash(app.web_dir.as_deref()).await);
        b = String::from_utf8_lossy(&b).replace("sheprd-shell-v1", &v).into_bytes();
    }
    // the shell carries the build it belongs to (web/app.js self-update compares it with /api/build)
    if rel == "index.html" {
        b = String::from_utf8_lossy(&b).replace("__MM_BUILD__", &app.build).into_bytes();
    }
    // Always revalidate (ETag -> 304), so a deploy can never leave the phone
    // running a mix of old and new modules, which breaks module linking.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    let etag = format!("\"{:x}\"", h.finish());
    let fresh = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str());
    let mut r = if fresh { StatusCode::NOT_MODIFIED.into_response() } else { Response::new(Body::from(b)) };
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        r.headers_mut().insert(header::ETAG, v);
    }
    r
}

#[cfg(test)]
mod menu_tests {
    use super::parse_menu;
    #[test]
    fn trust_dialog() {
        let s = " Security guide\n\n ❯ No, exit\n   Yes, I trust this folder\n\n Enter to confirm · Esc to cancel\n";
        let m = parse_menu(s);
        assert_eq!(m.len(), 2);
        assert_eq!(m[1].label, "Yes, I trust this folder");
        assert!(m[0].selected);
    }
    #[test]
    fn question() {
        let s = "Do you prefer tea or coffee?\n❯ 1. Tea\n     You prefer tea.\n  2. Coffee\n     You prefer coffee.\n  3. Type something.\n────\n  4. Chat about this\nEnter to select";
        let m = parse_menu(s);
        assert_eq!(m.iter().map(|o| o.number.unwrap()).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        assert_eq!(m[1].label, "Coffee");
    }
    // Claude Code 2.1 AskUserQuestion, multiSelect with more questions after it (captured screen)
    const MULTI: &str = "←  ☒ Fruit  ☐ Colors  ✔ Submit  →\nPick fruits\n❯ 1. [ ] apple\n         A red or green fruit\n  2. [✔] banana\n         A yellow fruit\n  3. [ ] Type something\n     Next\n────────\n  4. Chat about this\nEnter to select · Tab/Arrow keys to navigate";
    #[test]
    fn multiselect_lists_next() {
        let m = parse_menu(MULTI);
        let labels: Vec<_> = m.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, vec!["[ ] apple", "[✔] banana", "[ ] Type something", "Next", "Chat about this"]);
        assert!(m[0].selected && m[3].number.is_none() && super::is_checkbox(&m[1].label) && !super::is_checkbox(&m[3].label));
    }
    #[test]
    fn multiselect_cursor_on_next() {
        let s = MULTI.replace("❯ 1. [ ] apple", "  1. [ ] apple").replace("     Next", "❯    Next");
        let m = parse_menu(&s);
        assert_eq!(m.len(), 5);
        assert!(m[3].selected && m[3].label == "Next");
    }
}

#[cfg(test)]
mod input_tests {
    use super::input_box;
    #[test]
    fn input_line() {
        let r = "─".repeat(40);
        assert_eq!(input_box(&format!("● hi\n{r}\n❯ \n{r}\n  ⏵⏵ auto mode")).as_deref(), Some(""));
        assert_eq!(input_box(&format!("{r}\n❯ by ui i mean\n  web app\n{r}\n")).as_deref(), Some("by ui i mean\nweb app"));
        assert_eq!(input_box(&format!("{r} my tab ─\n❯ queued text\n{r}\n")).as_deref(), Some("queued text"));
        // A menu's ❯ is not the input box.
        assert_eq!(input_box("Pick one\n❯ 1. Tea\n  2. Coffee"), None);
    }
}

#[cfg(test)]
mod git_tests {
    #[test]
    fn porcelain() {
        let v = super::parse_git_status("/w", "## main...origin/main [ahead 1]\n M web/app.js\nR  a.rs -> b.rs\n?? new.txt\n");
        assert_eq!(v["branch"], "main");
        let c: Vec<_> = v["changed"].as_array().unwrap().iter().map(|x| (x["path"].as_str().unwrap(), x["st"].as_str().unwrap())).collect();
        assert_eq!(c, [("web/app.js", "M"), ("b.rs", "R"), ("new.txt", "??")]);
    }
}

#[cfg(test)]
mod push_tests {
    use super::{chat_surfaces, done_url, fallback_title, see_url, suppress_done, surface_row, transcript};
    use serde_json::json;
    #[test]
    fn urls() {
        assert_eq!(done_url("w6:p3", Some(803), false), "/#/a/w6:p3?i=803");
        assert_eq!(done_url("w6:p1", Some(12), true), "/#/?i=12");
        assert_eq!(done_url("w6:p3", None, false), "/#/a/w6:p3");
        assert_eq!(see_url("s19a2b3c4d5e"), "/#/see/s19a2b3c4d5e");
        assert_eq!(see_url("a b"), "/#/see/a%20b");
    }
    #[test]
    fn fallback_title_prefers_specific_names() {
        let t = |term, fp, cwd| fallback_title(term, fp, cwd, Some("claude"));
        assert_eq!(t(Some("Fix login bug"), Some("hi"), Some("/w/app")), "Fix login bug");
        assert_eq!(t(Some("Claude Code"), Some("Make the tests pass"), Some("/w/app")), "Make the tests pass");
        assert_eq!(t(Some("Claude Code"), None, Some("/home/u/Work/sheprd/")), "sheprd");
        assert_eq!(t(Some(""), None, None), "claude");
        assert_eq!(t(Some("Claude Code"), None, Some("/")), "Claude Code");
    }
    #[test]
    fn surface_row_matches_title_and_skips_claimed() {
        let row = |i: usize, title: &str| transcript::Item {
            i, k: "tool", name: Some("mcp__sheprd__surface".into()), input: Some(json!({"title": title})), ..Default::default()
        };
        let other = transcript::Item { i: 2, k: "text", text: Some("x".into()), ..Default::default() };
        let items = vec![row(0, "QA one"), row(1, "QA two"), other, row(3, "QA two")];
        assert_eq!(surface_row(&items, "QA two", &[]), Some(3));
        assert_eq!(surface_row(&items, "QA two", &[3]), Some(1));
        assert_eq!(surface_row(&items, "QA three", &[]), None);
        assert_eq!(surface_row(&items, "QA one", &[0]), None);
    }
    #[test]
    fn chat_surfaces_skip_tool_rows_and_reviews() {
        let row = transcript::Item {
            i: 0, k: "tool", name: Some("mcp__sheprd__surface".into()), input: Some(json!({"title": "From the tool"})), ..Default::default()
        };
        let items = vec![row, transcript::Item { i: 1, k: "text", text: Some("x".into()), ..Default::default() }];
        let l = vec![
            json!({"id": "a", "title": "From the tool", "i": 0}),        // old: its row is at i
            json!({"id": "b", "title": "Pick a design", "i": 1}),        // curl: anchored after the last item
            json!({"id": "c", "title": "Tool, resolved", "i": 0, "row": true}),
            json!({"id": "d", "title": "Review", "i": 1, "review": {"pane": "w1:p2"}}),
            json!({"id": "e", "title": "Not resolved yet", "i": null}),
        ];
        let ids: Vec<_> = chat_surfaces(&items, l).iter().map(|s| s["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["b", "e"]);
    }
    #[test]
    fn suppression() {
        assert!(suppress_done(true, 600_000, i64::MAX));
        assert!(suppress_done(false, 5_000, 10_000));
        assert!(!suppress_done(false, 5_000, 120_000));
        assert!(!suppress_done(false, 30_000, 1_000));
    }
}
