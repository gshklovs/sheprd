//! Live terminal view: `GET /api/a/:pane/live` (SSE) instead of polling
//! `/screen`. While at least one phone views a pane, one
//! `herdr terminal session observe <pane>` child streams that terminal's
//! frames (NDJSON); each burst of frames triggers one `pane.read` whose text is
//! relayed to every viewer. The child is killed ~2 s after the last viewer
//! leaves. If the observer cannot run, the stream says `fallback` and the
//! client goes back to polling.

use crate::{Shared, herdr, json_err};
use axum::extract::Path as AxPath;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::stream;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

const DEBOUNCE: Duration = Duration::from_millis(70);

pub fn routes() -> Router<Shared> {
    Router::new().route("/api/a/{pane}/live", get(api_live)).route("/api/live/status", get(api_status))
}

struct Obs {
    tx: broadcast::Sender<(&'static str, Arc<String>)>,
    subs: usize,
    frames: u64,
    started: i64,
}

static OBS: LazyLock<Mutex<HashMap<String, Obs>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Decrements the viewer count when the SSE stream is dropped.
struct Viewer(String);
impl Drop for Viewer {
    fn drop(&mut self) {
        if let Some(o) = OBS.lock().unwrap().get_mut(&self.0) {
            o.subs = o.subs.saturating_sub(1);
        }
    }
}

fn revision(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish() >> 11
}

async fn screen_json(pane: &str, seq: u64) -> Option<String> {
    let r = herdr::quick("pane.read", json!({"pane_id": pane, "source": "visible", "format": "ansi", "strip_ansi": false})).await.ok()?;
    let text = r["read"]["text"].as_str().unwrap_or("");
    Some(json!({"text": text, "revision": revision(text), "seq": seq}).to_string())
}

fn frame_size(line: &str) -> Option<(u64, u64)> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["type"] != "terminal.frame" {
        return None;
    }
    Some((v["width"].as_u64().unwrap_or(0), v["height"].as_u64().unwrap_or(0)))
}

/// One observer per pane, alive while it has viewers.
async fn observe(pane: String, tx: broadcast::Sender<(&'static str, Arc<String>)>) {
    let child = tokio::process::Command::new("herdr")
        .args(["terminal", "session", "observe", &pane])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            eprintln!("live {pane}: observe failed: {e}");
            OBS.lock().unwrap().remove(&pane);
            let _ = tx.send(("fallback", Arc::new(json!({"error": e.to_string()}).to_string())));
            return;
        }
    };
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut dirty = false;
    let mut last_sent = Instant::now() - DEBOUNCE;
    let mut seq = 0u64;
    let mut frames = 0u64;
    let mut size = (0, 0);
    let end: &'static str = loop {
        let wait = if dirty { (last_sent + DEBOUNCE).saturating_duration_since(Instant::now()) } else { Duration::from_secs(2) };
        tokio::select! {
            l = lines.next_line() => match l {
                Ok(Some(l)) => {
                    if let Some(sz) = frame_size(&l) {
                        frames += 1;
                        dirty = true;
                        if sz != size {
                            size = sz;
                            let _ = tx.send(("size", Arc::new(json!({"cols": sz.0, "rows": sz.1}).to_string())));
                        }
                    }
                }
                _ => break "ended",
            },
            _ = tokio::time::sleep(wait) => {}
        }
        {
            let mut m = OBS.lock().unwrap();
            match m.get_mut(&pane) {
                Some(o) if o.subs > 0 => o.frames = frames,
                _ => {
                    m.remove(&pane);
                    break "idle";
                }
            }
        }
        if dirty && last_sent.elapsed() >= DEBOUNCE {
            dirty = false;
            last_sent = Instant::now();
            seq += 1;
            if let Some(s) = screen_json(&pane, seq).await {
                let _ = tx.send(("screen", Arc::new(s)));
            }
        }
    };
    let _ = child.start_kill();
    if end == "ended" {
        OBS.lock().unwrap().remove(&pane);
        let gone = herdr::quick("pane.get", json!({"pane_id": pane})).await.is_err();
        let _ = tx.send((if gone { "gone" } else { "fallback" }, Arc::new("{}".into())));
    }
}

async fn api_live(AxPath(pane): AxPath<String>) -> Response {
    let Some(first) = screen_json(&pane, 0).await else { return json_err(StatusCode::NOT_FOUND, "no such pane") };
    let rx = {
        let mut m = OBS.lock().unwrap();
        let o = m.entry(pane.clone()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(8);
            tokio::spawn(observe(pane.clone(), tx.clone()));
            Obs { tx, subs: 0, frames: 0, started: crate::transcript::now_ms() }
        });
        o.subs += 1;
        o.tx.subscribe()
    };
    let guard = Viewer(pane);
    let init = Event::default().event("screen").data(first);
    let s = stream::unfold((Some(init), rx, guard, false), |(init, mut rx, guard, done)| async move {
        if done {
            return None;
        }
        if let Some(ev) = init {
            return Some((Ok::<_, Infallible>(ev), (None, rx, guard, false)));
        }
        loop {
            match rx.recv().await {
                Ok((kind, data)) => {
                    let last = kind == "fallback" || kind == "gone";
                    return Some((Ok(Event::default().event(kind).data(data.as_str())), (None, rx, guard, last)));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    });
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response()
}

/// Observers running now (tests, debugging).
async fn api_status() -> Response {
    let m = OBS.lock().unwrap();
    let list: Vec<Value> =
        m.iter().map(|(p, o)| json!({"pane": p, "viewers": o.subs, "frames": o.frames, "since": o.started})).collect();
    axum::Json(json!({"observers": list})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames() {
        assert_eq!(frame_size(r#"{"bytes":"","encoding":"ansi","full":true,"height":40,"seq":1,"type":"terminal.frame","width":120}"#), Some((120, 40)));
        assert_eq!(frame_size(r#"{"type":"other"}"#), None);
        assert_eq!(frame_size("garbage"), None);
    }
}
