//! App launcher: the user's self-hosted apps as a grid on the phone's Apps tab (`web/apps-tab.js`). Distinct from
//! apps.rs ("Running apps": servers agents start as background jobs, shown on Home); a registered app that is one of
//! those running servers (same port on this machine) is linked to it (`running`).
//!
//! Any agent on this machine registers its app: `POST /api/launcher` with the fields, or with `{"manifest": URL}` and
//! the server pulls the app's own `/app.json` (same shape) and re-pulls it hourly while the tab is looked at.
//! Thumbnails (4:3, 800×600 PNG suggested) are fetched by the server, checked by magic bytes (PNG/JPEG/WebP/GIF only,
//! ≤ 3 MB) and served same-origin from `<state>/launcher/`, so the https PWA never loads http tailnet URLs. Health =
//! a server-side GET of the app URL (3 s), only while the tab is viewed. API.md "Apps (launcher)".

use crate::Shared;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as AxPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const MAX_APPS: usize = 48;
const THUMB_MAX: usize = 3 << 20;
const MANIFEST_MAX: usize = 64 << 10;
const NAME_MAX: usize = 40;
const DESC_MAX: usize = 160;
const URL_MAX: usize = 512;
/// health is re-checked when older than this (and only when the list is asked for)
const HEALTH_MS: i64 = 20_000;
/// a manifest is re-pulled when older than this (same: only when the list is asked for)
const PULL_MS: i64 = 3600_000;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
struct Thumb {
    file: String,
    mime: String,
    hash: String,
    w: u32,
    h: u32,
    bytes: usize,
    at: i64,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
struct Entry {
    id: String,
    name: String,
    description: String,
    url: String,
    /// where the thumbnail came from (http/https); None when uploaded inline or not given
    thumbnail: Option<String>,
    manifest: Option<String>,
    accent: Option<String>,
    background: Option<String>,
    added: i64,
    updated: i64,
    pulled: i64,
    pull_error: Option<String>,
    thumb_error: Option<String>,
    thumb: Option<Thumb>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Store {
    apps: Vec<Entry>,
}

#[derive(Clone, Serialize)]
struct Health {
    state: &'static str,
    code: u16,
    ms: u64,
    at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| Mutex::new(std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()));
static HEALTH: LazyLock<Mutex<HashMap<String, Health>>> = LazyLock::new(Default::default);
static CHECKING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
static PULLING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
static SERVE: LazyLock<Mutex<Option<(Instant, Vec<(String, u16)>)>>> = LazyLock::new(Default::default);

fn file() -> PathBuf {
    crate::state_dir().join("launcher.json")
}
fn thumb_dir() -> PathBuf {
    crate::state_dir().join("launcher")
}
fn now_ms() -> i64 {
    crate::transcript::now_ms()
}
fn save(s: &Store) {
    if let Ok(b) = serde_json::to_vec_pretty(s) {
        let tmp = file().with_extension("json.tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(tmp, file());
        }
    }
}
fn get_entry(id: &str) -> Option<Entry> {
    STORE.lock().unwrap().apps.iter().find(|e| e.id == id).cloned()
}

// ---------------------------------------------------------------------------
// validation (everything from agents and manifests is untrusted)

fn valid_id(s: &str) -> Result<String, String> {
    let s = s.trim().to_ascii_lowercase();
    let ok = !s.is_empty() && s.len() <= 48 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') && s.as_bytes()[0].is_ascii_alphanumeric();
    if ok { Ok(s) } else { Err("id: 1–48 of a-z 0-9 - _, starting with a letter or digit".into()) }
}

/// One line of plain text: control characters and runs of whitespace become one space, cut at `max` characters.
fn clean(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut sp = false;
    for c in s.chars() {
        if c.is_control() || c.is_whitespace() || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            sp = !out.is_empty();
            continue;
        }
        if sp {
            out.push(' ');
            sp = false;
        }
        out.push(c);
    }
    if out.chars().count() > max {
        out = out.chars().take(max - 1).collect::<String>().trim_end().to_string() + "…";
    }
    out
}

/// (scheme, host, port, rest) of an http(s) URL. No userinfo, no odd characters, a host.
fn split_url(s: &str) -> Option<(&'static str, String, u16, String)> {
    let (scheme, rest) = if s.len() > 8 && s[..8].eq_ignore_ascii_case("https://") {
        ("https", &s[8..])
    } else if s.len() > 7 && s[..7].eq_ignore_ascii_case("http://") {
        ("http", &s[7..])
    } else {
        return None;
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (auth, tail) = rest.split_at(end);
    if auth.is_empty() || auth.contains('@') {
        return None;
    }
    let (host, port) = if let Some(h6) = auth.strip_prefix('[') {
        let (h, p) = h6.split_once(']')?;
        (format!("[{h}]"), p)
    } else {
        match auth.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p),
            None => (auth.to_string(), ""),
        }
    };
    let port = match port.strip_prefix(':').unwrap_or(port) {
        "" => if scheme == "https" { 443 } else { 80 },
        p => p.parse::<u16>().ok().filter(|p| *p > 0)?,
    };
    let host = host.to_ascii_lowercase();
    if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._[]:".contains(&b)) {
        return None;
    }
    Some((scheme, host, port, tail.to_string()))
}

fn valid_url(s: &str, what: &str) -> Result<String, String> {
    let s = s.trim();
    let bad = s.len() > URL_MAX || s.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '"' | '<' | '>' | '\\' | '`' | '{' | '}' | '|' | '^'));
    match split_url(s) {
        Some((scheme, ..)) if !bad => Ok(format!("{scheme}{}", &s[s.find("://").unwrap()..])),
        _ => Err(format!("{what}: an http:// or https:// URL (≤ {URL_MAX} characters, no user:password@)")),
    }
}

fn valid_color(s: &str) -> Option<String> {
    let s = s.trim();
    let hex = s.strip_prefix('#')?;
    ((hex.len() == 6 || hex.len() == 3) && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

/// Fields from a POST body or an app.json (aliases: desc, thumb, bg). Unknown keys are ignored.
#[derive(Default, Debug)]
struct Input {
    id: Option<String>,
    name: Option<String>,
    description: Option<String>,
    url: Option<String>,
    thumbnail: Option<String>,
    manifest: Option<String>,
    accent: Option<String>,
    background: Option<String>,
}
fn input(v: &Value) -> Input {
    let s = |ks: &[&str]| ks.iter().find_map(|k| v[*k].as_str()).map(str::to_string);
    Input {
        id: s(&["id"]),
        name: s(&["name", "title"]),
        description: s(&["description", "desc"]),
        url: s(&["url"]),
        thumbnail: s(&["thumbnail", "thumb"]),
        manifest: s(&["manifest"]),
        accent: s(&["accent"]),
        background: s(&["background", "bg"]),
    }
}
/// `top` over `base`, field by field.
fn merge(top: Input, base: Input) -> Input {
    Input {
        id: top.id.or(base.id),
        name: top.name.or(base.name),
        description: top.description.or(base.description),
        url: top.url.or(base.url),
        thumbnail: top.thumbnail.or(base.thumbnail),
        manifest: top.manifest.or(base.manifest),
        accent: top.accent.or(base.accent),
        background: top.background.or(base.background),
    }
}

/// Image type by magic bytes and its size when cheap to read (PNG, GIF, WebP VP8X/VP8/VP8L). SVG and anything else: None.
fn sniff(b: &[u8]) -> Option<(&'static str, &'static str, u32, u32)> {
    let be = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let le16 = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as u32;
    if b.len() >= 24 && b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(("image/png", "png", be(16), be(20)));
    }
    if b.len() >= 4 && b.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(("image/jpeg", "jpg", 0, 0));
    }
    if b.len() >= 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return Some(("image/gif", "gif", le16(6), le16(8)));
    }
    if b.len() >= 30 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        let (w, h) = match &b[12..16] {
            b"VP8X" => (1 + (b[24] as u32 | (b[25] as u32) << 8 | (b[26] as u32) << 16), 1 + (b[27] as u32 | (b[28] as u32) << 8 | (b[29] as u32) << 16)),
            b"VP8 " => (le16(26) & 0x3fff, le16(28) & 0x3fff),
            b"VP8L" if b.len() >= 25 => {
                let v = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
                ((v & 0x3fff) + 1, ((v >> 14) & 0x3fff) + 1)
            }
            _ => (0, 0),
        };
        return Some(("image/webp", "webp", w, h));
    }
    None
}

fn hash_hex(b: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    b.hash(&mut h);
    format!("{:016x}", h.finish())
}

// ---------------------------------------------------------------------------
// fetching (curl: http/https only, redirects too, bounded size and time)

async fn fetch(url: &str, max: usize, secs: u64) -> Result<(u16, Vec<u8>), String> {
    use tokio::io::AsyncReadExt;
    let mut c = tokio::process::Command::new("curl");
    c.args(["-s", "-L", "--max-redirs", "3", "--proto", "=http,https", "--proto-redir", "=http,https", "--max-time"])
        .arg(secs.to_string())
        .arg("--max-filesize")
        .arg(max.to_string())
        .args(["-A", "sheprd-launcher/1", "-w", "%{stderr}%{http_code}", "-o", "-", "--", url])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut ch = c.spawn().map_err(|e| format!("curl: {e}"))?;
    let mut out = ch.stdout.take().ok_or("curl: no stdout")?;
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(secs + 2), (&mut out).take(max as u64 + 1).read_to_end(&mut buf)).await;
    if buf.len() > max {
        let _ = ch.kill().await;
        return Err(format!("too big (over {} KB)", max >> 10));
    }
    if read.is_err() {
        let _ = ch.kill().await;
        return Err("timed out".into());
    }
    drop(out);
    let o = tokio::time::timeout(Duration::from_secs(3), ch.wait_with_output()).await.map_err(|_| "timed out".to_string())?.map_err(|e| e.to_string())?;
    let code: u16 = String::from_utf8_lossy(&o.stderr).trim().parse().unwrap_or(0);
    match o.status.code() {
        Some(0) => Ok((code, buf)),
        Some(6) => Err("can't resolve the host".into()),
        Some(7) => Err("connection refused".into()),
        Some(28) => Err("timed out".into()),
        Some(63) => Err(format!("too big (over {} KB)", max >> 10)),
        Some(35 | 60) => Err("TLS error".into()),
        Some(47) => Err("too many redirects".into()),
        Some(1) => Err("redirect to a non-http(s) URL".into()),
        Some(n) => Err(format!("fetch failed (curl {n})")),
        None => Err("fetch killed".into()),
    }
}

async fn fetch_manifest(url: &str) -> Result<Input, String> {
    let (code, b) = fetch(url, MANIFEST_MAX, 4).await.map_err(|e| format!("manifest: {e}"))?;
    if !(200..300).contains(&code) {
        return Err(format!("manifest: HTTP {code}"));
    }
    let v: Value = serde_json::from_slice(&b).map_err(|_| "manifest: not JSON".to_string())?;
    if !v.is_object() {
        return Err("manifest: not a JSON object".into());
    }
    Ok(input(&v))
}

/// The thumbnail bytes: a fetched http(s) URL or an inline `data:image/…;base64,…`.
async fn load_thumb(src: &str) -> Result<Vec<u8>, String> {
    if let Some(rest) = src.strip_prefix("data:") {
        use base64::Engine;
        let (meta, data) = rest.split_once(',').ok_or("thumbnail: a bad data: URL")?;
        if !meta.ends_with(";base64") {
            return Err("thumbnail: data: URLs must be base64".into());
        }
        let b = base64::engine::general_purpose::STANDARD.decode(data.trim()).map_err(|_| "thumbnail: bad base64".to_string())?;
        if b.len() > THUMB_MAX {
            return Err(format!("thumbnail: too big (over {} KB)", THUMB_MAX >> 10));
        }
        return Ok(b);
    }
    let (code, b) = fetch(src, THUMB_MAX, 6).await.map_err(|e| format!("thumbnail: {e}"))?;
    if !(200..300).contains(&code) {
        return Err(format!("thumbnail: HTTP {code}"));
    }
    Ok(b)
}

/// Store the image for `id`; the old file goes when the bytes changed.
fn store_thumb(id: &str, b: &[u8], old: Option<&Thumb>) -> Result<Thumb, String> {
    let Some((mime, ext, w, h)) = sniff(b) else {
        return Err("thumbnail: not a PNG, JPEG, WebP or GIF image".into());
    };
    let hash = hash_hex(b);
    if let Some(o) = old.filter(|o| o.hash == hash) {
        return Ok(o.clone());
    }
    let d = thumb_dir();
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    let name = format!("{id}-{}.{ext}", &hash[..10]);
    let tmp = d.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, b).and_then(|_| std::fs::rename(&tmp, d.join(&name))).map_err(|e| format!("thumbnail: {e}"))?;
    if let Some(o) = old.filter(|o| o.file != name) {
        let _ = std::fs::remove_file(d.join(&o.file));
    }
    Ok(Thumb { file: name, mime: mime.into(), hash, w, h, bytes: b.len(), at: now_ms() })
}

async fn check_health(id: String, url: String) {
    let t = Instant::now();
    let mut c = tokio::process::Command::new("curl");
    c.args(["-s", "-o", "/dev/null", "--max-time", "3", "--proto", "=http,https", "-A", "sheprd-launcher/1", "-w", "%{http_code}", "--", &url])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(5), c.output()).await;
    let ms = t.elapsed().as_millis() as u64;
    let (code, exit) = match &out {
        Ok(Ok(o)) => (String::from_utf8_lossy(&o.stdout).trim().parse::<u16>().unwrap_or(0), o.status.code()),
        _ => (0, None),
    };
    let h = match code {
        0 => Health {
            state: "down",
            code,
            ms,
            at: now_ms(),
            error: Some(match exit {
                Some(7) => "connection refused".into(),
                Some(28) | None => "timed out".into(),
                Some(6) => "can't resolve the host".into(),
                Some(n) => format!("curl {n}"),
            }),
        },
        500.. => Health { state: "error", code, ms, at: now_ms(), error: Some(format!("HTTP {code}")) },
        _ => Health { state: "up", code, ms, at: now_ms(), error: None },
    };
    HEALTH.lock().unwrap().insert(id.clone(), h);
    CHECKING.lock().unwrap().remove(&id);
}

/// Health checks for the stale ones (in the background); true when any is running.
fn kick_health(apps: &[Entry]) -> bool {
    let now = now_ms();
    let mut busy = false;
    for e in apps {
        let stale = HEALTH.lock().unwrap().get(&e.id).is_none_or(|h| now - h.at > HEALTH_MS);
        let mut ck = CHECKING.lock().unwrap();
        if ck.contains(&e.id) {
            busy = true;
        } else if stale {
            ck.insert(e.id.clone());
            busy = true;
            tokio::spawn(check_health(e.id.clone(), e.url.clone()));
        }
    }
    busy
}

// ---------------------------------------------------------------------------
// tailscale serve: the https form of an app on this machine

/// `(https base, local port)` for every `tailscale serve` web handler at `/` that proxies to a local http port.
fn parse_serve(v: &Value) -> Vec<(String, u16)> {
    let mut out = Vec::new();
    let Some(web) = v["Web"].as_object() else { return out };
    for (hp, cfg) in web {
        let Some(p) = cfg["Handlers"]["/"]["Proxy"].as_str() else { continue };
        let Some((_, host, port, _)) = split_url(p) else { continue };
        if host != "127.0.0.1" && host != "localhost" || !p.starts_with("http://") {
            continue;
        }
        let base = match hp.rsplit_once(':') {
            Some((h, "443")) => format!("https://{h}"),
            Some((h, sp)) => format!("https://{h}:{sp}"),
            None => format!("https://{hp}"),
        };
        out.push((base.to_ascii_lowercase(), port));
    }
    out.sort();
    out
}

async fn serve_map() -> Vec<(String, u16)> {
    if let Some((t, v)) = SERVE.lock().unwrap().as_ref()
        && t.elapsed() < Duration::from_secs(60)
    {
        return v.clone();
    }
    let out = tokio::time::timeout(Duration::from_secs(3), tokio::process::Command::new("tailscale").args(["serve", "status", "--json"]).kill_on_drop(true).output()).await;
    let v = match out {
        Ok(Ok(o)) => serde_json::from_slice::<Value>(&o.stdout).map(|v| parse_serve(&v)).unwrap_or_default(),
        _ => Vec::new(),
    };
    *SERVE.lock().unwrap() = Some((Instant::now(), v.clone()));
    v
}

struct Here<'a> {
    ts_name: &'a str,
    ts_ip: &'a str,
    serve: &'a [(String, u16)],
    own_port: u16,
}

/// The local port an app URL reaches on this machine (directly or through `tailscale serve`), and the https form the
/// phone can open without leaving the PWA's https (only an existing serve mapping, never a made-up URL).
fn locate(url: &str, h: &Here) -> (Option<u16>, Option<String>, &'static str) {
    let Some((scheme, host, port, rest)) = split_url(url) else { return (None, None, "web") };
    let ours = host == "127.0.0.1" || host == "localhost" || (!h.ts_ip.is_empty() && host == h.ts_ip) || (!h.ts_name.is_empty() && host == h.ts_name.to_ascii_lowercase());
    if scheme == "https" {
        let base = if port == 443 { format!("https://{host}") } else { format!("https://{host}:{port}") };
        let local = h.serve.iter().find(|(b, _)| *b == base).map(|(_, p)| *p);
        return (local, None, if host.ends_with(".ts.net") { "https" } else { "web" });
    }
    if !ours {
        return (None, None, if host.starts_with("100.") || host.ends_with(".ts.net") { "tailnet" } else { "web" });
    }
    let https = (port != h.own_port).then(|| h.serve.iter().find(|(_, p)| *p == port)).flatten().map(|(b, _)| {
        let r = if rest.is_empty() { "/".to_string() } else { rest.clone() };
        format!("{b}{r}")
    });
    let reach = if host == "127.0.0.1" || host == "localhost" { "local" } else { "tailnet" };
    (Some(port), https, reach)
}

// ---------------------------------------------------------------------------
// views

fn view(e: &Entry, h: &Here, running: &[Value]) -> Value {
    let (port, https, reach) = locate(&e.url, h);
    let run = port.and_then(|p| running.iter().find(|j| j["app"]["port"].as_u64() == Some(p as u64))).map(|j| {
        json!({"pane": j["pane"], "agentTitle": j["agentTitle"], "job": j["id"], "port": port, "desc": j["desc"]})
    });
    let health = HEALTH.lock().unwrap().get(&e.id).cloned();
    json!({
        "id": e.id, "name": e.name, "description": e.description, "url": e.url,
        "https": https, "open": https.clone().unwrap_or_else(|| e.url.clone()), "reach": if https.is_some() { "https" } else { reach },
        "thumb": e.thumb.as_ref().map(|t| format!("/api/launcher/{}/thumb?v={}", e.id, &t.hash[..10])),
        "thumbSize": e.thumb.as_ref().map(|t| json!([t.w, t.h])),
        "thumbnail": e.thumbnail, "manifest": e.manifest, "accent": e.accent, "background": e.background,
        "added": e.added, "updated": e.updated, "pulled": (e.pulled > 0).then_some(e.pulled),
        "pullError": e.pull_error, "thumbError": e.thumb_error,
        "health": health, "running": run, "port": port,
    })
}

fn register_line(port: u16) -> String {
    format!(
        "curl -s -X POST 127.0.0.1:{port}/api/launcher -H 'content-type: application/json' -d '{{\"manifest\":\"http://<host>:<port>/app.json\"}}'"
    )
}

async fn here_ctx(app: &crate::App) -> (String, String, Vec<(String, u16)>) {
    (app.ts_name.clone(), app.ts_ip.clone(), serve_map().await)
}

async fn api_list(State(app): State<Shared>) -> Response {
    let apps = STORE.lock().unwrap().apps.clone();
    let checking = kick_health(&apps);
    // hourly re-pull of manifests, in the background
    let now = now_ms();
    for e in apps.iter().filter(|e| e.manifest.is_some() && now - e.pulled > PULL_MS) {
        if PULLING.lock().unwrap().insert(e.id.clone()) {
            let id = e.id.clone();
            tokio::spawn(async move {
                let _ = refresh(&id).await;
                PULLING.lock().unwrap().remove(&id);
            });
        }
    }
    let (n, ip, serve) = here_ctx(&app).await;
    let h = Here { ts_name: &n, ts_ip: &ip, serve: &serve, own_port: crate::port() };
    let running = crate::apps::running();
    let list: Vec<Value> = apps.iter().map(|e| view(e, &h, &running)).collect();
    Json(json!({"apps": list, "checking": checking, "register": register_line(crate::port())})).into_response()
}

type Fail = (StatusCode, String);

/// Validate, pull the manifest / thumbnail as asked, upsert. Returns (entry, created).
async fn upsert(body: Input) -> Result<(Entry, bool), Fail> {
    let bad = |m: String| (StatusCode::BAD_REQUEST, m);
    let manifest = body.manifest.as_deref().map(|m| valid_url(m, "manifest")).transpose().map_err(bad)?;
    let (inp, pulled) = match &manifest {
        Some(m) => (merge(body, fetch_manifest(m).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?), now_ms()),
        None => (body, 0),
    };
    let id = valid_id(inp.id.as_deref().ok_or_else(|| bad("id: missing (in the body or the manifest)".into()))?).map_err(bad)?;
    let old = get_entry(&id);
    let name = inp.name.as_deref().map(|s| clean(s, NAME_MAX)).or(old.as_ref().map(|o| o.name.clone())).filter(|s| !s.is_empty()).ok_or_else(|| bad("name: missing".into()))?;
    let url = match inp.url.as_deref() {
        Some(u) => valid_url(u, "url").map_err(bad)?,
        None => old.as_ref().map(|o| o.url.clone()).ok_or_else(|| bad("url: missing".into()))?,
    };
    let description = inp.description.as_deref().map(|s| clean(s, DESC_MAX)).or(old.as_ref().map(|o| o.description.clone())).unwrap_or_default();
    let color = |v: &Option<String>, what: &str, keep: Option<String>| -> Result<Option<String>, Fail> {
        match v.as_deref() {
            None => Ok(keep),
            Some("") => Ok(None),
            Some(c) => valid_color(c).map(Some).ok_or_else(|| bad(format!("{what}: a #rrggbb colour"))),
        }
    };
    let accent = color(&inp.accent, "accent", old.as_ref().and_then(|o| o.accent.clone()))?;
    let background = color(&inp.background, "background", old.as_ref().and_then(|o| o.background.clone()))?;
    let thumb_src = inp.thumbnail.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (thumbnail, thumb, thumb_error) = match thumb_src {
        Some(src) if src.starts_with("data:") => match load_thumb(src).await.and_then(|b| store_thumb(&id, &b, old.as_ref().and_then(|o| o.thumb.as_ref()))) {
            Ok(t) => (None, Some(t), None),
            Err(e) => return Err(bad(e)),
        },
        Some(src) => {
            let src = valid_url(src, "thumbnail").map_err(bad)?;
            match load_thumb(&src).await.and_then(|b| store_thumb(&id, &b, old.as_ref().and_then(|o| o.thumb.as_ref()))) {
                Ok(t) => (Some(src), Some(t), None),
                // the app is still registered; the old picture (if any) stays
                Err(e) => (Some(src), old.as_ref().and_then(|o| o.thumb.clone()), Some(e)),
            }
        }
        None => (old.as_ref().and_then(|o| o.thumbnail.clone()), old.as_ref().and_then(|o| o.thumb.clone()), old.as_ref().and_then(|o| o.thumb_error.clone())),
    };
    let now = now_ms();
    let e = Entry {
        id: id.clone(),
        name,
        description,
        url,
        thumbnail,
        manifest: manifest.or(old.as_ref().and_then(|o| o.manifest.clone())),
        accent,
        background,
        added: old.as_ref().map(|o| o.added).unwrap_or(now),
        updated: now,
        pulled: if pulled > 0 { pulled } else { old.as_ref().map(|o| o.pulled).unwrap_or(0) },
        pull_error: None,
        thumb_error,
        thumb,
    };
    let mut s = STORE.lock().unwrap();
    let created = match s.apps.iter_mut().find(|x| x.id == id) {
        Some(x) => {
            *x = e.clone();
            false
        }
        None => {
            if s.apps.len() >= MAX_APPS {
                return Err((StatusCode::INSUFFICIENT_STORAGE, format!("at most {MAX_APPS} apps; remove one first")));
            }
            s.apps.push(e.clone());
            true
        }
    };
    save(&s);
    drop(s);
    HEALTH.lock().unwrap().remove(&id);
    Ok((e, created))
}

/// Re-pull the manifest (and thumbnail) of a registered app. A failed pull keeps what was there and says why.
async fn refresh(id: &str) -> Result<(Entry, bool), Fail> {
    let e = get_entry(id).ok_or((StatusCode::NOT_FOUND, "no such app".into()))?;
    let inp = match &e.manifest {
        Some(m) => match fetch_manifest(m).await {
            Ok(mut i) => {
                if i.id.as_deref().is_some_and(|x| x.trim().to_ascii_lowercase() != e.id) {
                    i.id = None; // the entry keeps its id
                }
                Input { id: Some(e.id.clone()), manifest: None, thumbnail: i.thumbnail.clone().or(e.thumbnail.clone()), ..i }
            }
            Err(err) => {
                let mut s = STORE.lock().unwrap();
                if let Some(x) = s.apps.iter_mut().find(|x| x.id == id) {
                    x.pull_error = Some(err.clone());
                    x.pulled = now_ms();
                }
                save(&s);
                return Err((StatusCode::BAD_GATEWAY, err));
            }
        },
        None => Input { id: Some(e.id.clone()), thumbnail: e.thumbnail.clone(), ..Default::default() },
    };
    let (mut ne, c) = upsert(inp).await?;
    if e.manifest.is_some() {
        ne.pulled = now_ms();
        let mut s = STORE.lock().unwrap();
        if let Some(x) = s.apps.iter_mut().find(|x| x.id == id) {
            x.pulled = ne.pulled;
        }
        save(&s);
    }
    Ok((ne, c))
}

async fn guard(app: &crate::App, h: &HeaderMap) -> Result<(), Response> {
    crate::share::check_sender(h, &crate::share::own_hosts(app).await).map_err(|e| crate::json_err(StatusCode::FORBIDDEN, e))
}

async fn reply(app: &crate::App, r: Result<(Entry, bool), Fail>) -> Response {
    match r {
        Ok((e, created)) => {
            check_health(e.id.clone(), e.url.clone()).await;
            let (n, ip, serve) = here_ctx(app).await;
            let h = Here { ts_name: &n, ts_ip: &ip, serve: &serve, own_port: crate::port() };
            let v = view(&e, &h, &crate::apps::running());
            let mut warn = Vec::new();
            if let Some(t) = &e.thumb_error {
                warn.push(t.clone());
            } else if let Some(t) = &e.thumb
                && t.w > 0
                && t.h > 0
                && (t.w * 3).abs_diff(t.h * 4) > t.h / 10
            {
                warn.push(format!("thumbnail is {}×{}; the tile is 4:3 (800×600 suggested), so it gets cropped", t.w, t.h));
            }
            if e.thumb.is_none() && e.thumb_error.is_none() {
                warn.push("no thumbnail: the tile shows the name on its background colour".into());
            }
            Json(json!({"ok": true, "created": created, "app": v, "warnings": warn})).into_response()
        }
        Err((c, m)) => crate::json_err(c, m),
    }
}

async fn api_upsert(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = guard(&app, &headers).await {
        return r;
    }
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if ct.split(';').next().unwrap_or("").trim() != "application/json" {
        return crate::json_err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "content-type must be application/json");
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return crate::json_err(StatusCode::BAD_REQUEST, "body: not JSON");
    };
    if !v.is_object() {
        return crate::json_err(StatusCode::BAD_REQUEST, "body: a JSON object");
    }
    let r = upsert(input(&v)).await;
    reply(&app, r).await
}

async fn api_refresh(State(app): State<Shared>, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    if let Err(r) = guard(&app, &headers).await {
        return r;
    }
    let r = refresh(&id).await;
    reply(&app, r).await
}

async fn api_delete(State(app): State<Shared>, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    if let Err(r) = guard(&app, &headers).await {
        return r;
    }
    let mut s = STORE.lock().unwrap();
    let Some(i) = s.apps.iter().position(|e| e.id == id) else {
        return crate::json_err(StatusCode::NOT_FOUND, "no such app");
    };
    let e = s.apps.remove(i);
    save(&s);
    drop(s);
    if let Some(t) = e.thumb {
        let _ = std::fs::remove_file(thumb_dir().join(t.file));
    }
    HEALTH.lock().unwrap().remove(&id);
    Json(json!({"ok": true, "deleted": id})).into_response()
}

async fn api_get(State(app): State<Shared>, AxPath(id): AxPath<String>) -> Response {
    let Some(e) = get_entry(&id) else { return crate::json_err(StatusCode::NOT_FOUND, "no such app") };
    let (n, ip, serve) = here_ctx(&app).await;
    let h = Here { ts_name: &n, ts_ip: &ip, serve: &serve, own_port: crate::port() };
    Json(view(&e, &h, &crate::apps::running())).into_response()
}

async fn api_thumb(AxPath(id): AxPath<String>) -> Response {
    let Some(t) = get_entry(&id).and_then(|e| e.thumb) else { return StatusCode::NOT_FOUND.into_response() };
    let Ok(b) = tokio::fs::read(thumb_dir().join(&t.file)).await else { return StatusCode::NOT_FOUND.into_response() };
    // re-checked on the way out: only an image of the stored type is ever served
    let Some((mime, ..)) = sniff(&b) else { return StatusCode::NOT_FOUND.into_response() };
    let mut r = b.into_response();
    let hd = r.headers_mut();
    hd.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    hd.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
    hd.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    hd.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; sandbox"));
    r
}

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/launcher", get(api_list).post(api_upsert).layer(DefaultBodyLimit::max(THUMB_MAX * 4 / 3 + (64 << 10))))
        .route("/api/launcher/{id}", get(api_get).delete(api_delete))
        .route("/api/launcher/{id}/refresh", post(api_refresh))
        .route("/api/launcher/{id}/thumb", get(api_thumb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(valid_url(" http://100.64.0.7:3417/ ", "url").unwrap(), "http://100.64.0.7:3417/");
        assert_eq!(valid_url("HTTPS://devbox.ts.net:8444/latest", "url").unwrap(), "https://devbox.ts.net:8444/latest");
        for bad in ["javascript:alert(1)", "file:///etc/passwd", "http://", "http://user:pw@host/", "http://a b/", "http://h/\"><script>", "ftp://h/", "data:text/html,x", "http://h:99999/", "//h/x"] {
            assert!(valid_url(bad, "url").is_err(), "{bad}");
        }
        assert_eq!(split_url("http://[::1]:8080/x").unwrap(), ("http", "[::1]".into(), 8080, "/x".into()));
    }

    #[test]
    fn text_and_ids() {
        assert_eq!(clean("  Grafana\n\t<b>x</b>\u{202e} ", 40), "Grafana <b>x</b>");
        assert_eq!(clean(&"a".repeat(50), 10).chars().count(), 10);
        assert_eq!(valid_id("Notes-App").unwrap(), "notes-app");
        assert!(valid_id("../x").is_err() && valid_id("").is_err() && valid_id("-x").is_err());
        assert_eq!(valid_color("#B0372A").unwrap(), "#b0372a");
        assert!(valid_color("red").is_none() && valid_color("#12345g").is_none() && valid_color("#fff;x").is_none());
    }

    #[test]
    fn images() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(800u32.to_be_bytes());
        png.extend(600u32.to_be_bytes());
        assert_eq!(sniff(&png), Some(("image/png", "png", 800, 600)));
        assert!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'/>").is_none());
        assert!(sniff(b"<html><script>").is_none());
        assert_eq!(sniff(b"GIF89a\x20\x03\x58\x02....").map(|x| (x.2, x.3)), Some((800, 600)));
    }

    #[test]
    fn serve_and_locate() {
        let v: Value = serde_json::from_str(
            r#"{"Web":{"g.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:7878"}}},
                "g.ts.net:8444":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8791"}}},
                "g.ts.net:9000":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3417"}}},
                "g.ts.net:9100":{"Handlers":{"/x":{"Proxy":"http://127.0.0.1:5000"}}}}}"#,
        )
        .unwrap();
        let s = parse_serve(&v);
        assert_eq!(s, vec![("https://g.ts.net".into(), 7878), ("https://g.ts.net:8444".into(), 8791), ("https://g.ts.net:9000".into(), 3417)]);
        let h = Here { ts_name: "g.ts.net", ts_ip: "100.64.0.7", serve: &s, own_port: 7878 };
        assert_eq!(locate("http://100.64.0.7:3417/read?a=1", &h), (Some(3417), Some("https://g.ts.net:9000/read?a=1".into()), "tailnet"));
        assert_eq!(locate("https://g.ts.net:8444/latest", &h), (Some(8791), None, "https"));
        assert_eq!(locate("http://127.0.0.1:5000/", &h), (Some(5000), None, "local"));
        assert_eq!(locate("http://100.64.1.2:80/", &h), (None, None, "tailnet"));
        assert_eq!(locate("http://localhost:7878/", &h).1, None, "never offers the app itself");
    }

    #[test]
    fn merge_body_over_manifest() {
        let m = input(&json!({"id": "grafana", "name": "Grafana", "desc": "d", "thumb": "http://h/t.png", "bg": "#fff"}));
        let b = input(&json!({"manifest": "http://h/app.json", "name": "Grafana 2"}));
        let x = merge(b, m);
        assert_eq!((x.id.as_deref(), x.name.as_deref(), x.description.as_deref(), x.thumbnail.as_deref(), x.background.as_deref()), (Some("grafana"), Some("Grafana 2"), Some("d"), Some("http://h/t.png"), Some("#fff")));
    }
}
