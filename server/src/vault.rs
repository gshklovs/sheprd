//! Write-only secrets vault (docs/API.md "Vault") and agents' vault requests.
//!
//! One file per secret at `~/.config/sheprd/vault/<name>` (dir 0700, files 0600), written atomically: a temp
//! file in the same dir created 0600 with O_NOFOLLOW|O_EXCL, fsync, rename, fsync the dir. Other programs
//! (your scripts, the agents) read the files directly. **No endpoint ever returns a value**: `GET /api/vault` lists metadata
//! only (name, set, the last 4 characters of values of 12+ characters, mtime). Values never reach a log line,
//! an error message, telemetry, an SSE event or a push; responses are `Cache-Control: no-store`.
//!
//! Writes and deletes are accepted only from the app's own page: the Host and the Origin must both be one of
//! this server's own names (tailnet name, tailnet IP, 127.0.0.1/localhost on its port: DNS rebinding and other
//! sites fail), `Content-Type: application/json` and `X-Sheprd: vault` (both force a CORS preflight nothing
//! answers), `Sec-Fetch-Site` (when sent) same-origin, no Funnel. Bodies are parsed by hand so a parse error can
//! never quote the value. The rest of the API relies on axum's JSON content-type check only; this is stricter.
//! No CLI is ever run, so the code never lands in an argv; nothing logs the query string.

use crate::transcript::now_ms;
use crate::{Shared, json_err};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as AxPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MAX_VALUE: usize = 16 * 1024;
/// Values shorter than this show no tail at all.
const TAIL_MIN: usize = 12;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/vault", get(api_list))
        .route("/api/vault/{name}", axum::routing::put(api_put).post(api_put).delete(api_delete))
        .route("/api/vault/request", axum::routing::post(api_request))
        .route("/api/vault/request/{id}/saved", axum::routing::post(api_saved))
        .route("/api/vault/request/{id}/cancel", axum::routing::post(api_cancel))
        .layer(DefaultBodyLimit::max(MAX_VALUE + 4096))
        .layer(axum::middleware::map_response(no_store))
}

async fn no_store(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    r
}

/// `~/.config/sheprd/vault`; test instances (port ≠ 7878) may set `SHEPRD_VAULT_DIR`.
pub fn dir() -> PathBuf {
    if crate::port() != crate::DEFAULT_PORT {
        if let Some(d) = std::env::var_os("SHEPRD_VAULT_DIR").filter(|d| !d.is_empty()) {
            return PathBuf::from(d);
        }
    }
    crate::home().join(".config/sheprd/vault")
}

/// True when `p` (already canonicalized) is the vault dir or inside it: `/api/file` refuses those.
pub fn covers(p: &Path) -> bool {
    let d = dir();
    p.starts_with(&d) || std::fs::canonicalize(&d).is_ok_and(|c| p.starts_with(c))
}

/// `^[a-z0-9_]{1,64}$`: no dots, slashes or anything else, so no traversal and no clash with temp files.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::other(msg.to_string())
}

/// Creates the vault dir 0700 (or fixes its mode); refuses a symlink, a non-directory or one owned by someone else.
fn ensure_dir(d: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(d) {
        Ok(m) => {
            if m.file_type().is_symlink() || !m.is_dir() {
                return Err(io_err("vault path is not a plain directory"));
            }
            if m.uid() != unsafe { libc::geteuid() } {
                return Err(io_err("vault dir is owned by another user"));
            }
            if m.mode() & 0o777 != 0o700 {
                std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(p) = d.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::DirBuilder::new().mode(0o700).create(d)?;
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))
        }
        Err(e) => Err(e),
    }
}

fn rand_hex() -> String {
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        let n = now_ms() as u64 ^ std::process::id() as u64;
        b = n.to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Refuses a target that exists and is not a regular file (a symlink, a dir, a fifo…).
fn check_target(p: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if !m.file_type().is_file() => Err(io_err("slot is not a regular file (symlink?)")),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Atomic 0600 write of `data` to `<d>/<name>`.
pub fn write_atomic(d: &Path, name: &str, data: &[u8]) -> std::io::Result<()> {
    if !valid_name(name) {
        return Err(io_err("bad slot name"));
    }
    ensure_dir(d)?;
    let target = d.join(name);
    check_target(&target)?;
    let tmp = d.join(format!(".{name}.tmp-{}", rand_hex()));
    let r = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        check_target(&target)?;
        std::fs::rename(&tmp, &target)?;
        std::fs::File::open(d)?.sync_all()
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Deletes a slot. Ok(false) when it was not set; refuses a symlink.
pub fn delete(d: &Path, name: &str) -> std::io::Result<bool> {
    if !valid_name(name) {
        return Err(io_err("bad slot name"));
    }
    let p = d.join(name);
    match std::fs::symlink_metadata(&p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
        Ok(m) if !m.file_type().is_file() => Err(io_err("slot is not a regular file (symlink?)")),
        Ok(_) => {
            std::fs::remove_file(&p)?;
            let _ = std::fs::File::open(d).and_then(|f| f.sync_all());
            Ok(true)
        }
    }
}

/// The last 4 characters, only for values of 12+ characters.
pub fn tail(v: &str) -> Option<String> {
    let v = v.trim_end();
    let n = v.chars().count();
    (n >= TAIL_MIN).then(|| v.chars().skip(n - 4).collect())
}

/// Metadata of one slot: `{name, set, tail?, updated?}`. Never the value.
pub fn meta(d: &Path, name: &str) -> Value {
    let unset = json!({"name": name, "set": false});
    let p = d.join(name);
    // O_NOFOLLOW: a symlink is never read; O_NONBLOCK: a fifo can't hang the request.
    let Ok(mut f) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(&p)
    else {
        return unset;
    };
    let Ok(m) = f.metadata() else { return unset };
    if !m.is_file() {
        return unset;
    }
    let updated = crate::transcript::mtime_ms(&m);
    let mut buf = Vec::new();
    let _ = (&mut f).take(MAX_VALUE as u64 * 4).read_to_end(&mut buf);
    let s = String::from_utf8_lossy(&buf);
    let mut o = json!({"name": name, "set": true, "updated": updated});
    if let Some(t) = tail(&s) {
        o["tail"] = json!(t);
    }
    o
}

/// Every valid slot file in the dir (sorted).
pub fn list(d: &Path) -> Vec<Value> {
    let mut names: Vec<String> = Vec::new();
    let mut extra: Vec<String> = std::fs::read_dir(d)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| valid_name(n) && !names.contains(n))
                .collect()
        })
        .unwrap_or_default();
    extra.sort();
    names.extend(extra);
    names.iter().map(|n| meta(d, n)).collect()
}

// ---------------------------------------------------------------------------
// Request checks

/// This server's own `host[:port]` names (lowercase). Both Host and Origin must be one of them.
/// (Tailscale may come up after the server: then ask it again, per request, until it answers.)
async fn own_hosts(app: &crate::App) -> Vec<String> {
    let p = crate::port();
    let mut v = vec![format!("127.0.0.1:{p}"), format!("localhost:{p}")];
    let (name, ip) = if app.ts_name.is_empty() || app.ts_ip.is_empty() {
        crate::tailscale_identity().await
    } else {
        (app.ts_name.clone(), app.ts_ip.clone())
    };
    if !name.is_empty() {
        v.push(name.to_ascii_lowercase());
        v.push(format!("{}:443", name.to_ascii_lowercase()));
    }
    if !ip.is_empty() {
        v.push(format!("{ip}:{p}"));
    }
    v
}

fn hv<'a>(h: &'a HeaderMap, k: &str) -> &'a str {
    h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// Read access (`GET /api/vault`): the Host is ours (no DNS rebinding), not cross-site, not Funnel.
pub fn check_read(h: &HeaderMap, own: &[String]) -> Result<(), &'static str> {
    if h.contains_key("tailscale-funnel-request") {
        return Err("not over Funnel");
    }
    let host = hv(h, "host").to_ascii_lowercase();
    if !own.iter().any(|o| *o == host) {
        return Err("unknown host");
    }
    let site = hv(h, "sec-fetch-site");
    if !site.is_empty() && site != "same-origin" && site != "none" {
        return Err("cross-site request");
    }
    Ok(())
}

/// Writes: everything `check_read` wants, plus Origin = one of ours, JSON, `X-Sheprd: vault`, same-origin.
pub fn check_write(h: &HeaderMap, own: &[String], body_json: bool) -> Result<(), &'static str> {
    check_read(h, own)?;
    let origin = hv(h, "origin").to_ascii_lowercase();
    let auth = origin.strip_prefix("https://").or_else(|| origin.strip_prefix("http://"));
    let ok_origin = auth.is_some_and(|a| {
        !a.contains('/') && (own.iter().any(|o| o == a) || (origin.starts_with("https://") && own.iter().any(|o| *o == format!("{a}:443"))))
    });
    if !ok_origin {
        return Err("origin not allowed");
    }
    let site = hv(h, "sec-fetch-site");
    if !site.is_empty() && site != "same-origin" {
        return Err("cross-site request");
    }
    if hv(h, "x-sheprd") != "vault" {
        return Err("missing app header");
    }
    if body_json {
        let ct = hv(h, "content-type").to_ascii_lowercase();
        let mt = ct.split(';').next().unwrap_or("").trim();
        if mt != "application/json" {
            return Err("content-type must be application/json");
        }
    }
    Ok(())
}

/// `{"value": "<string>"}` → the value, trailing whitespace trimmed. Errors never quote the input.
pub fn parse_value(body: &[u8]) -> Result<String, &'static str> {
    if body.len() > MAX_VALUE + 4096 {
        return Err("too large");
    }
    let v: Value = serde_json::from_slice(body).map_err(|_| "body must be JSON {\"value\": \"…\"}")?;
    let s = v.get("value").and_then(Value::as_str).ok_or("body must be JSON {\"value\": \"…\"}")?;
    let s = s.trim_end();
    if s.is_empty() {
        return Err("empty value");
    }
    if s.len() > MAX_VALUE {
        return Err("value too large (16 KB max)");
    }
    if s.contains('\0') {
        return Err("value contains a NUL byte");
    }
    Ok(s.to_string())
}

// ---------------------------------------------------------------------------
// Handlers

async fn api_list(State(app): State<Shared>, h: HeaderMap) -> Response {
    if let Err(why) = check_read(&h, &own_hosts(&app).await) {
        return json_err(StatusCode::FORBIDDEN, why);
    }
    let d = dir();
    let slots = tokio::task::spawn_blocking(move || list(&d)).await.unwrap_or_default();
    Json(json!({"slots": slots, "dir": "~/.config/sheprd/vault"})).into_response()
}

async fn api_put(State(app): State<Shared>, AxPath(name): AxPath<String>, h: HeaderMap, body: Bytes) -> Response {
    if let Err(why) = check_write(&h, &own_hosts(&app).await, true) {
        return json_err(StatusCode::FORBIDDEN, why);
    }
    if !valid_name(&name) {
        return json_err(StatusCode::BAD_REQUEST, "slot names are a-z, 0-9 and _ (1–64)");
    }
    let value = match parse_value(&body) {
        Ok(v) => v,
        Err(why) => return json_err(StatusCode::BAD_REQUEST, why),
    };
    drop(body);
    let d = dir();
    let n = name.clone();
    let r = tokio::task::spawn_blocking(move || {
        let r = write_atomic(&d, &n, value.as_bytes());
        drop(value);
        r.map(|_| meta(&d, &n))
    })
    .await;
    match r {
        Ok(Ok(m)) => {
            eprintln!("vault: set {name}");
            Json(json!({"ok": true, "slot": m})).into_response()
        }
        Ok(Err(e)) => {
            eprintln!("vault: set {name} failed: {e}");
            json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("could not save: {e}"))
        }
        Err(_) => json_err(StatusCode::INTERNAL_SERVER_ERROR, "could not save"),
    }
}

async fn api_delete(State(app): State<Shared>, AxPath(name): AxPath<String>, h: HeaderMap) -> Response {
    if let Err(why) = check_write(&h, &own_hosts(&app).await, false) {
        return json_err(StatusCode::FORBIDDEN, why);
    }
    if !valid_name(&name) {
        return json_err(StatusCode::BAD_REQUEST, "slot names are a-z, 0-9 and _ (1–64)");
    }
    let d = dir();
    let n = name.clone();
    match tokio::task::spawn_blocking(move || delete(&d, &n)).await {
        Ok(Ok(was)) => {
            eprintln!("vault: delete {name}");
            Json(json!({"ok": true, "deleted": was})).into_response()
        }
        Ok(Err(e)) => json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("could not delete: {e}")),
        Err(_) => json_err(StatusCode::INTERNAL_SERVER_ERROR, "could not delete"),
    }
}

// ---------------------------------------------------------------------------
// Vault requests (API.md "Vault requests"): an agent asks the user for a secret; the card in its chat (and Home's
// needs-you) has a paste field per slot. The phone saves through `PUT /api/vault/<name>` as always, then tells
// `POST /api/vault/request/<id>/saved {names}`; the server checks each file really is fresh and steers the agent
// "[Vault] The user saved <names>" (names only: no value, no tail). The card, the state and the steer never hold a value.

/// Slots one request may ask for at most.
const REQ_MAX_SLOTS: usize = 6;
/// Path segments under /api/vault/ that are routes, not slots.
pub const RESERVED: [&str; 1] = ["request"];

fn clip(v: &Value, n: usize) -> String {
    crate::transcript::cut(v.as_str().unwrap_or("").trim(), n).0
}

/// A link the card may open: http(s), no spaces or control characters, ≤ 500 bytes.
fn ok_link(s: &str) -> bool {
    (s.starts_with("https://") || s.starts_with("http://")) && s.len() <= 500 && s.len() > 8 && s.bytes().all(|b| b > 0x20 && b != 0x7f)
}

/// Validates a request → (the card's `vault` object, its title). Errors never quote the input. A body that carries a
/// `value` anywhere is refused: secrets come from the user's phone, never from the agent.
pub fn parse_request(b: &Value) -> Result<(Value, String), String> {
    if !b.is_object() {
        return Err("body must be a JSON object".into());
    }
    let has_value = |o: &Value| ["value", "values", "secret"].iter().any(|k| o.get(k).is_some());
    if has_value(b) {
        return Err("a request never carries a value: the user pastes it on the phone".into());
    }
    let src: Vec<Value> = match b.get("slots") {
        Some(Value::Array(a)) if !a.is_empty() => a.clone(),
        Some(Value::Array(_)) => return Err("slots is empty".into()),
        Some(_) => return Err("slots must be an array of {name, label}".into()),
        None if b.get("name").is_some() => vec![json!({"name": b["name"], "label": b["label"], "hint": Value::Null})],
        None => return Err("name (or slots) required".into()),
    };
    if src.len() > REQ_MAX_SLOTS {
        return Err(format!("at most {REQ_MAX_SLOTS} slots per request"));
    }
    let mut slots: Vec<Value> = Vec::new();
    for (k, o) in src.iter().enumerate() {
        let (name, label, hint) = match o {
            Value::String(n) => (n.as_str(), String::new(), String::new()),
            Value::Object(_) if has_value(o) => return Err("a request never carries a value: the user pastes it on the phone".into()),
            Value::Object(_) => (o["name"].as_str().unwrap_or(""), clip(&o["label"], 60), clip(&o["hint"], 140)),
            _ => return Err(format!("slot {}: must be {{name, label}}", k + 1)),
        };
        if !valid_name(name) {
            return Err(format!("slot {}: names are a-z, 0-9 and _ (1–64)", k + 1));
        }
        if RESERVED.contains(&name) {
            return Err(format!("slot {}: that name is reserved", k + 1));
        }
        if slots.iter().any(|s| s["name"] == name) {
            return Err(format!("slot {}: listed twice", k + 1));
        }
        let label = if label.is_empty() { name.to_string() } else { label };
        slots.push(json!({"name": name, "label": label, "hint": hint, "saved": null}));
    }
    let link = match b["link"].as_str().map(str::trim).filter(|l| !l.is_empty()) {
        Some(l) if ok_link(l) => Some(l.to_string()),
        Some(_) => return Err("link must be an http(s) URL".into()),
        None => None,
    };
    let why = clip(&b["why"], 400);
    let labels: Vec<&str> = slots.iter().filter_map(|s| s["label"].as_str()).collect();
    let title = match clip(&b["title"], 70) {
        t if !t.is_empty() => t,
        _ => crate::transcript::cut(&format!("Paste {}", labels.join(" + ")), 70).0,
    };
    let v = json!({
        "slots": slots, "why": why, "hint": clip(&b["hint"], 200), "link": link,
        "linkLabel": Some(clip(&b["linkLabel"], 30)).filter(|l| !l.is_empty()), "state": "open", "closed": null,
    });
    Ok((v, title))
}

/// Guard + parse for `POST /api/vault/request`: an agent on this machine (share::check_sender: our Host, and an
/// Origin, when sent, of our own) → 403; a bad body → 400.
pub fn check_request(h: &HeaderMap, own: &[String], body: &[u8]) -> Result<(Value, Value, String), (StatusCode, String)> {
    crate::share::check_sender(h, own).map_err(|e| (StatusCode::FORBIDDEN, e.to_string()))?;
    let b: Value = serde_json::from_slice(body).map_err(|_| (StatusCode::BAD_REQUEST, "body must be JSON".to_string()))?;
    let (v, title) = parse_request(&b).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok((b, v, title))
}

fn agent_of(s: &Value) -> Option<&str> {
    s["session"].as_str().or(s["pane"].as_str()).filter(|a| !a.is_empty())
}

fn slot_names(v: &Value) -> Vec<String> {
    v["slots"].as_array().into_iter().flatten().filter_map(|s| s["name"].as_str().map(str::to_string)).collect()
}

/// A newer request from the same agent for any of the same slots closes the older open one.
pub fn supersede(list: &mut std::collections::VecDeque<Value>, item: &Value) -> usize {
    if !item["vault"].is_object() {
        return 0;
    }
    let (who, names) = (agent_of(item).map(str::to_string), slot_names(&item["vault"]));
    let mut n = 0;
    for s in list.iter_mut() {
        if s["vault"]["state"] == "open" && agent_of(s).map(str::to_string) == who && slot_names(&s["vault"]).iter().any(|x| names.contains(x)) {
            s["vault"]["state"] = json!("superseded");
            s["vault"]["closed"] = json!(now_ms());
            s["dismissed"] = json!(true);
            n += 1;
        }
    }
    n
}

/// Where a request's push opens: its agent's chat at the card.
pub fn push_url(pane: Option<&str>, at: Option<usize>) -> Option<String> {
    let p = pane.filter(|p| !p.is_empty())?;
    Some(format!("/#/a/{}{}", crate::urlencode(p), at.map(|i| format!("?i={i}")).unwrap_or_default()))
}

/// Records `names` as saved on the card: each must be a slot of this open request whose file in `d` was written
/// after the card was posted. All saved → the request closes (`saved`) and the card is dismissed. → the newly
/// saved names. Only metadata is read (meta()); nothing of the value is stored on the card.
pub fn mark_saved(card: &mut Value, names: &[String], d: &Path, now: i64) -> Result<Vec<String>, &'static str> {
    if card["vault"]["state"] != "open" {
        return Err("this request is closed");
    }
    let since = card["ts"].as_i64().unwrap_or(0) - 2000;
    let mut fresh = Vec::new();
    for n in names {
        let Some(k) = card["vault"]["slots"].as_array().and_then(|a| a.iter().position(|s| s["name"] == n.as_str())) else {
            return Err("not a slot of this request");
        };
        if !card["vault"]["slots"][k]["saved"].is_null() {
            continue;
        }
        let m = meta(d, n);
        if m["set"] != true || m["updated"].as_i64().unwrap_or(0) < since {
            return Err("that slot is not saved yet");
        }
        card["vault"]["slots"][k]["saved"] = json!(now);
        fresh.push(n.clone());
    }
    if fresh.is_empty() {
        return Err("already saved");
    }
    let all = card["vault"]["slots"].as_array().is_some_and(|a| a.iter().all(|s| !s["saved"].is_null()));
    if all {
        card["vault"]["state"] = json!("saved");
        card["vault"]["closed"] = json!(now);
        card["dismissed"] = json!(true);
    }
    Ok(fresh)
}

/// The steer the agent gets: names only. No tail: the transcript (and the model provider) should not see even part
/// of a secret; the agent reads `~/.config/sheprd/vault/<name>` itself.
pub fn steer_text(names: &[String]) -> String {
    format!("[Vault] The user saved {}", names.join(", "))
}

/// `{names: [..]}` of the phone's `/saved` call (vault-strict headers already checked).
pub fn parse_names(body: &[u8]) -> Result<Vec<String>, &'static str> {
    let v: Value = serde_json::from_slice(body).map_err(|_| "body must be JSON {\"names\": [..]}")?;
    let names: Vec<String> = v["names"].as_array().ok_or("body must be JSON {\"names\": [..]}")?.iter().filter_map(Value::as_str).map(str::to_string).collect();
    if names.is_empty() || names.len() > REQ_MAX_SLOTS || !names.iter().all(|n| valid_name(n)) {
        return Err("names: 1–6 slot names (a-z, 0-9, _)");
    }
    Ok(names)
}

async fn api_request(State(app): State<Shared>, h: HeaderMap, body: Bytes) -> Response {
    let (b, v, title) = match check_request(&h, &own_hosts(&app).await, &body) {
        Ok(x) => x,
        Err((code, why)) => return json_err(code, why),
    };
    let names = slot_names(&v).join(", ");
    let mut card = json!({"title": title, "body": v["why"], "level": "attention", "tool": false});
    for k in ["pane", "session", "push"] {
        if let Some(x) = b.get(k).filter(|x| !x.is_null()) {
            card[k] = x.clone();
        }
    }
    let r = crate::post_surface(app, card, Some(v)).await;
    if r.status().is_success() {
        eprintln!("vault: request for {names}");
    }
    r
}

async fn api_saved(State(app): State<Shared>, AxPath(id): AxPath<String>, h: HeaderMap, body: Bytes) -> Response {
    if let Err(why) = check_write(&h, &own_hosts(&app).await, true) {
        return json_err(StatusCode::FORBIDDEN, why);
    }
    let names = match parse_names(&body) {
        Ok(n) => n,
        Err(why) => return json_err(StatusCode::BAD_REQUEST, why),
    };
    let d = dir();
    let res = {
        let mut inner = app.inner.lock().unwrap();
        match inner.surfaces.iter_mut().find(|s| s["id"].as_str() == Some(id.as_str()) && s["vault"].is_object()) {
            None => Err(json_err(StatusCode::NOT_FOUND, "no such request")),
            Some(card) => match mark_saved(card, &names, &d, now_ms()) {
                Err(why) => Err(json_err(StatusCode::CONFLICT, why)),
                Ok(fresh) => {
                    let out = card.clone();
                    crate::save_surfaces(&inner.surfaces);
                    Ok((fresh, out))
                }
            },
        }
    };
    let (fresh, card) = match res {
        Ok(x) => x,
        Err(r) => return r,
    };
    crate::republish(&app);
    eprintln!("vault: request {id}: saved {}", fresh.join(", "));
    let done = card["vault"]["state"] == "saved";
    match crate::decide::deliver(&app, &card, steer_text(&fresh)).await {
        Ok((via, held)) => Json(json!({"ok": true, "saved": fresh, "done": done, "via": via, "held": held})).into_response(),
        // saved all the same; the agent just wasn't told (gone, or the steer failed)
        Err(_) => Json(json!({"ok": true, "saved": fresh, "done": done, "via": "none", "held": false, "undelivered": true})).into_response(),
    }
}

/// The agent takes its request back (`curl -X POST …/api/vault/request/<id>/cancel`).
async fn api_cancel(State(app): State<Shared>, AxPath(id): AxPath<String>, h: HeaderMap) -> Response {
    if let Err(why) = crate::share::check_sender(&h, &own_hosts(&app).await) {
        return json_err(StatusCode::FORBIDDEN, why);
    }
    let found = {
        let mut inner = app.inner.lock().unwrap();
        let hit = inner.surfaces.iter_mut().find(|s| s["id"].as_str() == Some(id.as_str()) && s["vault"]["state"] == "open").map(|s| {
            s["vault"]["state"] = json!("cancelled");
            s["vault"]["closed"] = json!(now_ms());
            s["dismissed"] = json!(true);
        });
        if hit.is_some() {
            crate::save_surfaces(&inner.surfaces);
        }
        hit.is_some()
    };
    if !found {
        return json_err(StatusCode::NOT_FOUND, "no open request with that id");
    }
    crate::republish(&app);
    Json(json!({"ok": true})).into_response()
}


#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mm-vault-test-{tag}-{}-{}", std::process::id(), rand_hex()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().mode() & 0o777
    }
    fn own() -> Vec<String> {
        vec!["127.0.0.1:7878".into(), "localhost:7878".into(), "devbox.example-tailnet.ts.net".into(), "devbox.example-tailnet.ts.net:443".into(), "100.64.0.7:7878".into()]
    }
    fn hdr(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn perms_and_atomic_write() {
        let root = tmp("perm");
        let d = root.join("cfg/vault");
        write_atomic(&d, "x_bearer_token", b"AAAAbbbbCCCCdddd1234").unwrap();
        assert_eq!(mode(&d), 0o700);
        assert_eq!(mode(&d.join("x_bearer_token")), 0o600);
        assert_eq!(std::fs::read(d.join("x_bearer_token")).unwrap(), b"AAAAbbbbCCCCdddd1234");
        // replace: same inode semantics as rename (new file), still 0600, no temp left behind
        let ino1 = std::fs::metadata(d.join("x_bearer_token")).unwrap().ino();
        write_atomic(&d, "x_bearer_token", b"second-value-xyz9").unwrap();
        let ino2 = std::fs::metadata(d.join("x_bearer_token")).unwrap().ino();
        assert_ne!(ino1, ino2, "replaced by rename, not rewritten in place");
        assert_eq!(std::fs::read(d.join("x_bearer_token")).unwrap(), b"second-value-xyz9");
        assert_eq!(mode(&d.join("x_bearer_token")), 0o600);
        let left: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "no temp files left: {left:?}");
        // a loose dir mode is tightened
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_atomic(&d, "x_client_id", b"abc").unwrap();
        assert_eq!(mode(&d), 0o700);
        // a stale temp file of ours does not block or get listed
        std::fs::write(d.join(".x_client_id.tmp-dead"), b"zzz").unwrap();
        assert!(list(&d).iter().all(|s| s["name"] != ".x_client_id.tmp-dead"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn names() {
        for ok in ["a", "x_bearer_token", "abc_123", &"a".repeat(64)] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "A", "a-b", "a.b", "..", "../x", "a/b", "x ", " x", "é", "a\0", &"a".repeat(65), ".x", "%2e%2e"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        let d = tmp("names");
        assert!(write_atomic(&d, "../escape", b"v").is_err());
        assert!(write_atomic(&d, "a/b", b"v").is_err());
        assert!(!d.parent().unwrap().join("escape").exists());
        assert!(delete(&d, "..").is_err());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn symlinks_refused() {
        let root = tmp("sym");
        let d = root.join("vault");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        let outside = root.join("outside");
        std::fs::write(&outside, b"do-not-touch-0000").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("x_client_secret")).unwrap();
        assert!(write_atomic(&d, "x_client_secret", b"new").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"do-not-touch-0000");
        assert!(std::fs::symlink_metadata(d.join("x_client_secret")).unwrap().file_type().is_symlink());
        // never read through a symlink either
        let m = meta(&d, "x_client_secret");
        assert_eq!(m["set"], false);
        assert!(!m.to_string().contains("0000"));
        assert!(delete(&d, "x_client_secret").is_err());
        assert!(outside.exists());
        // the vault dir itself being a symlink is refused
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("linkvault");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(write_atomic(&link, "x_bearer_token", b"v").is_err());
        assert!(!real.join("x_bearer_token").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn listing_never_returns_values() {
        let d = tmp("list");
        let secret = "sk-SECRETVALUE-abcdefghijklmnop-WXYZ";
        write_atomic(&d, "x_bearer_token", secret.as_bytes()).unwrap();
        write_atomic(&d, "x_client_id", b"short").unwrap();
        write_atomic(&d, "my_custom", b"0123456789ab").unwrap();
        let l = list(&d);
        let s = serde_json::to_string(&l).unwrap();
        assert!(!s.contains("SECRETVALUE") && !s.contains("short") && !s.contains("0123456789"), "{s}");
        let by = |n: &str| l.iter().find(|x| x["name"] == n).cloned().unwrap();
        assert_eq!(by("x_bearer_token")["tail"], "WXYZ");
        assert_eq!(by("x_bearer_token")["set"], true);
        assert!(by("x_client_id").get("tail").is_none(), "short values: no tail");
        assert_eq!(by("my_custom")["tail"], "89ab");
        for x in &l {
            let keys: Vec<&String> = x.as_object().unwrap().keys().collect();
            assert!(keys.iter().all(|k| ["name", "set", "tail", "updated"].contains(&k.as_str())), "{keys:?}");
        }
        assert_eq!(tail("abcdefghijk"), None);
        assert_eq!(tail("abcdefghijkl").as_deref(), Some("ijkl"));
        assert_eq!(tail("abcdefghijkl\n").as_deref(), Some("ijkl"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn value_parsing() {
        assert_eq!(parse_value(br#"{"value":"tok123\n  "}"#).unwrap(), "tok123");
        assert_eq!(parse_value(br#"{"value":"  keep leading"}"#).unwrap(), "  keep leading");
        assert!(parse_value(br#"{"value":"   "}"#).is_err());
        assert!(parse_value(br#"{"value":12345}"#).is_err());
        let big = format!(r#"{{"value":"{}"}}"#, "a".repeat(MAX_VALUE + 1));
        assert!(parse_value(big.as_bytes()).is_err());
        let ok = format!(r#"{{"value":"{}"}}"#, "a".repeat(MAX_VALUE));
        assert!(parse_value(ok.as_bytes()).is_ok());
        // errors never quote the input
        for b in [&br#"{"value":"hunter2-SECRET"#[..], br#"{"value":["hunter2-SECRET"]}"#, b"hunter2-SECRET"] {
            let e = parse_value(b).unwrap_err();
            assert!(!e.contains("hunter2"), "{e}");
        }
    }

    #[test]
    fn origin_checks() {
        let own = own();
        let good = [
            ("host", "devbox.example-tailnet.ts.net"),
            ("origin", "https://devbox.example-tailnet.ts.net"),
            ("content-type", "application/json"),
            ("x-sheprd", "vault"),
            ("sec-fetch-site", "same-origin"),
        ];
        assert_eq!(check_write(&hdr(&good), &own, true), Ok(()));
        // tailscale serve may hand us Host 127.0.0.1:7878 with the phone's Origin
        let mut v = good.to_vec();
        v[0] = ("host", "127.0.0.1:7878");
        assert_eq!(check_write(&hdr(&v), &own, true), Ok(()));
        // local dev page
        assert_eq!(
            check_write(&hdr(&[("host", "127.0.0.1:7878"), ("origin", "http://127.0.0.1:7878"), ("content-type", "application/json; charset=utf-8"), ("x-sheprd", "vault")]), &own, true),
            Ok(())
        );
        let bad: Vec<(&str, Vec<(&str, &str)>)> = vec![
            ("no origin", good.iter().filter(|(k, _)| *k != "origin").cloned().collect()),
            ("evil origin", good.iter().map(|&(k, v)| if k == "origin" { (k, "https://evil.example") } else { (k, v) }).collect()),
            ("lookalike origin", good.iter().map(|&(k, v)| if k == "origin" { (k, "https://devbox.example-tailnet.ts.net.evil.example") } else { (k, v) }).collect()),
            ("null origin", good.iter().map(|&(k, v)| if k == "origin" { (k, "null") } else { (k, v) }).collect()),
            ("wrong port", good.iter().map(|&(k, v)| if k == "origin" { (k, "http://127.0.0.1:9999") } else { (k, v) }).collect()),
            // DNS rebinding: evil.example resolving to 127.0.0.1, Origin and Host both evil
            (
                "rebinding",
                vec![("host", "evil.example:7878"), ("origin", "http://evil.example:7878"), ("content-type", "application/json"), ("x-sheprd", "vault")],
            ),
            ("text/plain", good.iter().map(|&(k, v)| if k == "content-type" { (k, "text/plain") } else { (k, v) }).collect()),
            ("form", good.iter().map(|&(k, v)| if k == "content-type" { (k, "application/x-www-form-urlencoded") } else { (k, v) }).collect()),
            ("no app header", good.iter().filter(|(k, _)| *k != "x-sheprd").cloned().collect()),
            ("cross-site", good.iter().map(|&(k, v)| if k == "sec-fetch-site" { (k, "cross-site") } else { (k, v) }).collect()),
            ("same-site", good.iter().map(|&(k, v)| if k == "sec-fetch-site" { (k, "same-site") } else { (k, v) }).collect()),
            ("funnel", good.iter().cloned().chain([("tailscale-funnel-request", "?1")]).collect()),
        ];
        for (why, h) in bad {
            assert!(check_write(&hdr(&h), &own, true).is_err(), "{why} should be refused");
        }
        // DELETE: no body, no content-type needed, everything else is
        let del = [("host", "127.0.0.1:7878"), ("origin", "http://127.0.0.1:7878"), ("x-sheprd", "vault")];
        assert_eq!(check_write(&hdr(&del), &own, false), Ok(()));
        assert!(check_write(&hdr(&del[..2]), &own, false).is_err());
        // reads: our Host only
        assert_eq!(check_read(&hdr(&[("host", "127.0.0.1:7878")]), &own), Ok(()));
        assert!(check_read(&hdr(&[("host", "evil.example:7878")]), &own).is_err());
        assert!(check_read(&hdr(&[("host", "127.0.0.1:7878"), ("sec-fetch-site", "cross-site")]), &own).is_err());
    }


    #[test]
    fn request_parsing() {
        let (v, title) = parse_request(&json!({"slots": [{"name": "x_client_id", "label": "X client ID"}, {"name": "x_client_secret", "label": "X client secret"}],
            "why": "to post", "link": "https://platform.example.com/keys"})).unwrap();
        assert_eq!(title, "Paste X client ID + X client secret");
        assert_eq!(v["slots"][1]["name"], "x_client_secret");
        assert_eq!(v["state"], "open");
        assert!(v["slots"][0]["saved"].is_null());
        let (v, _) = parse_request(&json!({"name": "openai_key", "label": "OpenAI key"})).unwrap();
        assert_eq!(v["slots"].as_array().unwrap().len(), 1);
        for bad in [
            json!({"name": "../etc/passwd"}),
            json!({"name": "A"}),
            json!({"name": "a.b"}),
            json!({"name": "request"}),
            json!({"slots": []}),
            json!({"slots": [{"name": "a"}, {"name": "a"}]}),
            json!({"slots": ["a", "b", "c", "d", "e", "f", "g"]}),
            json!({"name": "a", "link": "javascript:alert(1)"}),
            json!({"name": "a", "value": "sk-SHOULD-NOT"}),
            json!({"slots": [{"name": "a", "value": "sk-SHOULD-NOT"}]}),
            json!({}),
        ] {
            let e = parse_request(&bad).unwrap_err();
            assert!(!e.contains("SHOULD-NOT") && !e.contains("passwd"), "{e}");
        }
    }

    #[test]
    fn request_guard_bad_name_400_foreign_origin_403() {
        let own = own();
        let body = br#"{"name":"x_client_id","why":"w","pane":"w6:p2"}"#;
        // the mod (no Origin) and curl on this machine
        assert!(check_request(&hdr(&[("host", "127.0.0.1:7878")]), &own, body).is_ok());
        let (code, _) = check_request(&hdr(&[("host", "127.0.0.1:7878")]), &own, br#"{"name":"../x"}"#).unwrap_err();
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let (code, _) = check_request(&hdr(&[("host", "127.0.0.1:7878")]), &own, br#"{"name":"Bad Name"}"#).unwrap_err();
        assert_eq!(code, StatusCode::BAD_REQUEST);
        for h in [
            vec![("host", "127.0.0.1:7878"), ("origin", "https://evil.example")],
            vec![("host", "127.0.0.1:7878"), ("origin", "null")],
            vec![("host", "evil.example:7878")],
            vec![("host", "127.0.0.1:7878"), ("sec-fetch-site", "cross-site")],
            vec![("host", "devbox.example-tailnet.ts.net"), ("tailscale-funnel-request", "?1")],
        ] {
            let (code, _) = check_request(&hdr(&h), &own, body).unwrap_err();
            assert_eq!(code, StatusCode::FORBIDDEN, "{h:?}");
        }
        // the phone's /saved call is vault-strict: no app header → refused
        assert!(check_write(&hdr(&[("host", "127.0.0.1:7878"), ("origin", "http://127.0.0.1:7878"), ("content-type", "application/json")]), &own, true).is_err());
        assert_eq!(parse_names(br#"{"names":["../x"]}"#), Err("names: 1–6 slot names (a-z, 0-9, _)"));
        assert!(parse_names(br#"{"names":["x_client_id"]}"#).is_ok());
    }

    #[test]
    fn request_card_and_steer_never_hold_a_value() {
        let d = tmp("req");
        let (v, title) = parse_request(&json!({"slots": [{"name": "x_client_id", "label": "Client ID"}, {"name": "x_client_secret", "label": "Client secret"}], "why": "w"})).unwrap();
        let mut card = json!({"id": "s1", "ts": now_ms() - 10, "pane": "w6:p2", "title": title, "level": "attention", "vault": v});
        // not saved yet → refused, nothing marked
        assert_eq!(mark_saved(&mut card, &["x_client_id".into()], &d, now_ms()), Err("that slot is not saved yet"));
        assert_eq!(mark_saved(&mut card, &["other".into()], &d, now_ms()), Err("not a slot of this request"));
        let (id_val, secret) = ("CLIENTID-abcdefghijklmn-QQ", "sk-SECRETVALUE-abcdefghijklmnop-WZ");
        write_atomic(&d, "x_client_id", id_val.as_bytes()).unwrap();
        assert_eq!(mode(&d.join("x_client_id")), 0o600);
        let fresh = mark_saved(&mut card, &["x_client_id".into()], &d, now_ms()).unwrap();
        assert_eq!(fresh, ["x_client_id"]);
        assert_eq!(card["vault"]["state"], "open");
        assert_eq!(mark_saved(&mut card, &["x_client_id".into()], &d, now_ms()), Err("already saved"));
        write_atomic(&d, "x_client_secret", secret.as_bytes()).unwrap();
        let fresh = mark_saved(&mut card, &["x_client_secret".into()], &d, now_ms()).unwrap();
        assert_eq!(card["vault"]["state"], "saved");
        assert_eq!(card["dismissed"], true);
        let text = steer_text(&fresh);
        assert_eq!(text, "[Vault] The user saved x_client_secret");
        let s = card.to_string();
        for needle in ["SECRETVALUE", "CLIENTID", "-WZ", "-QQ", "abcdefghij"] {
            assert!(!s.contains(needle), "card holds {needle}: {s}");
            assert!(!text.contains(needle), "steer holds {needle}");
        }
        assert_eq!(mark_saved(&mut card, &["x_client_id".into()], &d, now_ms()), Err("this request is closed"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn request_supersedes_same_agent_same_slot() {
        let (v, _) = parse_request(&json!({"name": "x_client_id"})).unwrap();
        let mk = |id: &str, sess: &str| json!({"id": id, "session": sess, "vault": v.clone()});
        let mut l: std::collections::VecDeque<Value> = vec![mk("a", "s1"), mk("b", "s2")].into();
        assert_eq!(supersede(&mut l, &mk("c", "s1")), 1);
        assert_eq!(l[0]["vault"]["state"], "superseded");
        assert_eq!(l[1]["vault"]["state"], "open");
        assert_eq!(push_url(Some("w6:p2"), Some(7)).as_deref(), Some("/#/a/w6%3Ap2?i=7"));
    }

    #[test]
    fn covers_vault_paths() {
        let d = dir();
        assert!(covers(&d.join("x_bearer_token")));
        assert!(!covers(&crate::home().join(".config/sheprd/other")));
    }
}
