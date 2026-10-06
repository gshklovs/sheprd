//! "Look at my screen": on-demand screenshots of the desktop (Hyprland + grim).
//! Nothing is captured unless the phone asks; a locked desktop or a screen that
//! is off is reported instead of captured. The last few shots are kept in
//! `<state dir>/desk/` (JPEG) and served by id.

use crate::transcript::now_ms;
use crate::{Shared, json_err};
use axum::extract::Path as AxPath;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

const KEEP: usize = 6;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/desk", get(api_desk))
        .route("/api/desk/shot", post(api_shot))
        .route("/api/desk/shot/{id}", get(api_shot_get))
}

fn shots_dir() -> PathBuf {
    let d = crate::state_dir().join("desk");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn hypr_socket() -> Option<PathBuf> {
    let rt = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| "/run/user/1000".into());
    if let Some(sig) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        let p = rt.join("hypr").join(sig).join(".socket.sock");
        if p.exists() {
            return Some(p);
        }
    }
    std::fs::read_dir(rt.join("hypr"))
        .ok()?
        .flatten()
        .map(|e| e.path().join(".socket.sock"))
        .filter(|p| p.exists())
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

async fn hypr(cmd: &str) -> Option<Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let sock = hypr_socket()?;
    let fut = async {
        let mut s = tokio::net::UnixStream::connect(sock).await.ok()?;
        s.write_all(cmd.as_bytes()).await.ok()?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.ok()?;
        serde_json::from_slice(&buf).ok()
    };
    tokio::time::timeout(Duration::from_millis(1500), fut).await.ok().flatten()
}

/// Desktop state from Hyprland's monitors + clients.
fn summarize(mons: &Value, clients: &Value) -> Value {
    let ms = mons.as_array().cloned().unwrap_or_default();
    let locked = ms.iter().any(|m| m["solitaryBlockedBy"].as_array().is_some_and(|b| b.iter().any(|x| x == "LOCK")));
    let on = ms.iter().any(|m| m["dpmsStatus"].as_bool() != Some(false));
    let saver = clients.as_array().is_some_and(|c| c.iter().any(|w| w["class"].as_str().is_some_and(|k| k.contains("screensaver"))));
    let active: Vec<i64> = ms.iter().filter_map(|m| m["activeWorkspace"]["id"].as_i64()).collect();
    let outputs: Vec<Value> = ms
        .iter()
        .map(|m| {
            json!({"name": m["name"], "w": m["width"], "h": m["height"], "x": m["x"], "y": m["y"], "scale": m["scale"],
                   "focused": m["focused"], "on": m["dpmsStatus"].as_bool() != Some(false), "ws": m["activeWorkspace"]["name"]})
        })
        .collect();
    let mut windows: Vec<Value> = clients
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["mapped"] != false && c["class"].as_str().is_some_and(|k| !k.is_empty() && !k.contains("screensaver")))
        .map(|c| {
            let ws = c["workspace"]["id"].as_i64().unwrap_or(0);
            json!({"id": c["stableId"], "addr": c["address"], "class": c["class"], "title": c["title"],
                   "ws": c["workspace"]["name"], "visible": active.contains(&ws) && c["hidden"] != true,
                   "at": c["at"], "size": c["size"], "focus": c["focusHistoryID"]})
        })
        .collect();
    windows.sort_by_key(|w| w["focus"].as_i64().unwrap_or(99));
    let state = if locked { "locked" } else if !on { "off" } else if saver { "screensaver" } else { "ok" };
    json!({"state": state, "locked": locked, "screenOn": on, "outputs": outputs, "windows": windows})
}

async fn desk_state() -> Option<Value> {
    let mons = hypr("j/monitors").await?;
    let clients = hypr("j/clients").await.unwrap_or(json!([]));
    Some(summarize(&mons, &clients))
}

fn grim_ok() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("grim").is_file()))
        .unwrap_or(false)
        || std::path::Path::new("/usr/bin/grim").is_file()
}

async fn api_desk() -> Response {
    match desk_state().await {
        Some(mut v) => {
            v["available"] = json!(grim_ok());
            v["shots"] = json!(list_shots().into_iter().map(|(id, ts)| json!({"id": id, "ts": ts, "url": format!("/api/desk/shot/{id}")})).collect::<Vec<_>>());
            Json(v).into_response()
        }
        None => Json(json!({"state": "nodesktop", "available": false, "outputs": [], "windows": [], "shots": []})).into_response(),
    }
}

fn list_shots() -> Vec<(String, i64)> {
    let mut v: Vec<(String, i64)> = std::fs::read_dir(shots_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            let id = n.strip_suffix(".jpg")?.to_string();
            let ts = e.metadata().ok().map(|m| crate::transcript::mtime_ms(&m)).unwrap_or(0);
            Some((id, ts))
        })
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

#[derive(Deserialize)]
struct ShotBody {
    /// "screen" (all outputs), "output:<name>" or "window:<stableId>"
    target: String,
}

fn valid_tok(s: &str) -> bool {
    !s.is_empty() && s.len() < 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

async fn run_grim(args: &[String], out: &std::path::Path) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("grim");
    cmd.args(args).arg(out).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("grim: {e}"))?;
    match tokio::time::timeout(Duration::from_secs(8), child.wait_with_output()).await {
        Err(_) => Err("grim timed out (screen off or locked?)".into()),
        Ok(Err(e)) => Err(format!("grim: {e}")),
        Ok(Ok(o)) if o.status.success() => Ok(()),
        Ok(Ok(o)) => Err(String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("grim failed").to_string()),
    }
}

async fn api_shot(Json(b): Json<ShotBody>) -> Response {
    let Some(st) = desk_state().await else { return json_err(StatusCode::SERVICE_UNAVAILABLE, "no desktop session (Hyprland not reachable)") };
    match st["state"].as_str() {
        Some("locked") => return (StatusCode::LOCKED, Json(json!({"error": "locked", "message": "The desktop is locked — nothing captured."}))).into_response(),
        Some("off") => return (StatusCode::CONFLICT, Json(json!({"error": "off", "message": "The screen is off — nothing captured."}))).into_response(),
        _ => {}
    }
    if !grim_ok() {
        return json_err(StatusCode::SERVICE_UNAVAILABLE, "grim is not installed");
    }
    let base = ["-t", "jpeg", "-q", "72"].map(String::from).to_vec();
    let (mut args, fallback): (Vec<String>, Option<Vec<String>>) = match b.target.split_once(':') {
        None if b.target == "screen" => (vec!["-s".into(), "0.6".into()], None),
        Some(("output", name)) if valid_tok(name) => {
            if !st["outputs"].as_array().is_some_and(|l| l.iter().any(|o| o["name"] == name)) {
                return json_err(StatusCode::NOT_FOUND, "no such output");
            }
            (vec!["-o".into(), name.into(), "-s".into(), "0.75".into()], None)
        }
        Some(("window", id)) if valid_tok(id) => {
            let Some(w) = st["windows"].as_array().and_then(|l| l.iter().find(|w| w["id"].as_str() == Some(id))).cloned() else {
                return json_err(StatusCode::NOT_FOUND, "no such window");
            };
            // Region of the window when it is on a visible workspace (fallback if
            // toplevel capture is refused).
            let geo = (w["visible"] == true).then(|| {
                let (x, y) = (w["at"][0].as_i64().unwrap_or(0), w["at"][1].as_i64().unwrap_or(0));
                let (ww, hh) = (w["size"][0].as_i64().unwrap_or(0), w["size"][1].as_i64().unwrap_or(0));
                vec!["-g".to_string(), format!("{x},{y} {ww}x{hh}")]
            });
            (vec!["-T".into(), id.into()], geo)
        }
        _ => return json_err(StatusCode::BAD_REQUEST, "target must be screen, output:<name> or window:<id>"),
    };
    args.splice(0..0, base.clone());
    let id = format!("s{}", now_ms());
    let out = shots_dir().join(format!("{id}.jpg"));
    let mut r = run_grim(&args, &out).await;
    if r.is_err() {
        if let Some(mut fb) = fallback {
            fb.splice(0..0, base);
            r = run_grim(&fb, &out).await;
        }
    }
    if let Err(e) = r {
        let _ = std::fs::remove_file(&out);
        return json_err(StatusCode::BAD_GATEWAY, e);
    }
    for (old, _) in list_shots().into_iter().skip(KEEP) {
        let _ = std::fs::remove_file(shots_dir().join(format!("{old}.jpg")));
    }
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Json(json!({"id": id, "url": format!("/api/desk/shot/{id}"), "ts": now_ms(), "bytes": bytes, "target": b.target})).into_response()
}

async fn api_shot_get(AxPath(id): AxPath<String>) -> Response {
    if !valid_tok(&id) {
        return json_err(StatusCode::BAD_REQUEST, "bad id");
    }
    match tokio::fs::read(shots_dir().join(format!("{id}.jpg"))).await {
        Ok(b) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=86400, immutable")], b).into_response(),
        Err(_) => json_err(StatusCode::NOT_FOUND, "no such shot"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn states() {
        let mons = json!([{"name":"HDMI-A-1","width":2560,"height":1440,"x":0,"y":0,"scale":1.0,"focused":true,"dpmsStatus":true,
                           "activeWorkspace":{"id":2,"name":"2"},"solitaryBlockedBy":["WINDOWED"]}]);
        let clients = json!([
            {"stableId":"18000026","address":"0x1","class":"chromium","title":"Docs","workspace":{"id":2,"name":"2"},"mapped":true,"hidden":false,"at":[12,12],"size":[800,600],"focusHistoryID":1},
            {"stableId":"18000027","address":"0x2","class":"foot","title":"shell","workspace":{"id":3,"name":"3"},"mapped":true,"hidden":false,"at":[0,0],"size":[10,10],"focusHistoryID":0}]);
        let s = summarize(&mons, &clients);
        assert_eq!(s["state"], "ok");
        assert_eq!(s["windows"][0]["class"], "foot"); // most recently focused first
        assert_eq!(s["windows"][0]["visible"], false);
        assert_eq!(s["windows"][1]["visible"], true);
        let mut locked = mons.clone();
        locked[0]["solitaryBlockedBy"] = json!(["LOCK"]);
        assert_eq!(summarize(&locked, &clients)["state"], "locked");
        let mut off = mons.clone();
        off[0]["dpmsStatus"] = json!(false);
        assert_eq!(summarize(&off, &clients)["state"], "off");
        assert!(valid_tok("18000026") && valid_tok("HDMI-A-1") && !valid_tok("../x") && !valid_tok(""));
    }
}
