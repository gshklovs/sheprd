//! Voice notes: phone audio → ffmpeg (16 kHz mono wav) → whisper.cpp → text.
//!
//! Nothing stays resident: every note spawns a niced `ffmpeg` and a niced
//! `whisper-cli` child (killed if the phone hangs up), one note at a time; the
//! others wait on a single-permit gate. The upload is streamed to disk, never
//! buffered in the server. The transcript is biased toward the user's words with
//! whisper's initial prompt (agent titles, pane ids, ~/Work projects, commands),
//! built from the cached state on each request.
//!
//! Install layout (see README, FAQ "Voice notes"): `~/.local/share/sheprd/whisper/`
//! may hold `pkg/usr/{bin,lib}` (an unpacked whisper-cpp + ggml build) and
//! `models/ggml-base.en.bin`. Overrides: `SHEPRD_WHISPER_BIN`,
//! `SHEPRD_WHISPER_MODEL`; a system `whisper-cli` on PATH also works.

use crate::{Shared, home, json_err};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

/// Uploads larger than this are refused (a minute of iPhone video is ~60 MB).
const MAX_BYTES: usize = 96 * 1024 * 1024;
/// Longest note we transcribe; ffmpeg cuts the rest.
const MAX_SECS: u32 = 600;
/// whisper's initial prompt is capped at n_text_ctx/2 = 224 tokens; stay well under.
const PROMPT_CHARS: usize = 600;

/// Below the agents' compiles, but not so far that a note waits a minute behind them.
const NICE: &str = "5";

static GATE: Semaphore = Semaphore::const_new(1);
static WAITING: AtomicUsize = AtomicUsize::new(0);
static BUSY: AtomicUsize = AtomicUsize::new(0);

pub fn routes() -> Router<Shared> {
    Router::new().route("/api/voice", get(api_status).post(api_voice))
}

fn base_dir() -> PathBuf {
    home().join(".local/share/sheprd/whisper")
}

fn env_path(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(name)).find(|f| f.is_file()))
}

fn whisper_bin() -> Option<PathBuf> {
    env_path("SHEPRD_WHISPER_BIN")
        .or_else(|| Some(base_dir().join("pkg/usr/bin/whisper-cli")))
        .filter(|p| p.is_file())
        .or_else(|| which("whisper-cli"))
}

fn whisper_model() -> Option<PathBuf> {
    if let Some(m) = env_path("SHEPRD_WHISPER_MODEL") {
        return m.is_file().then_some(m);
    }
    let dir = base_dir().join("models");
    let pref = dir.join("ggml-base.en.bin");
    if pref.is_file() {
        return Some(pref);
    }
    let mut all: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "bin"))
        .collect();
    all.sort();
    all.into_iter().next()
}

fn model_name(p: &Path) -> String {
    let s = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    s.strip_prefix("ggml-").unwrap_or(&s).to_string()
}

fn cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

/// (busy, total) jiffies from the first line of /proc/stat.
fn cpu_jiffies() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    let v: Vec<u64> = s.lines().next()?.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    let idle = v.get(3)? + v.get(4).unwrap_or(&0);
    Some((v.iter().sum::<u64>() - idle, v.iter().sum()))
}

/// Threads for whisper = idle cores right now (1..=4). ggml's threads spin on
/// barriers, so on a box busy compiling, 4 threads take minutes where 1 takes
/// seconds (measured: 20 s note under 4 busy cores, -t 4 > 60 s, -t 1 16 s).
async fn threads() -> usize {
    let n = cpus().min(4);
    let Some(a) = cpu_jiffies() else { return n };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let Some(b) = cpu_jiffies() else { return n };
    threads_for(b.0.saturating_sub(a.0), b.1.saturating_sub(a.1), cpus())
}

fn threads_for(busy: u64, total: u64, cpus: usize) -> usize {
    if total == 0 {
        return cpus.clamp(1, 4);
    }
    let idle = (total - busy.min(total)) as f64 / total as f64 * cpus as f64;
    (idle.round() as usize).clamp(1, cpus.min(4))
}

#[derive(Deserialize)]
struct VoiceQ {
    /// The chat the note is for: its agent's title and project lead the vocabulary.
    pane: Option<String>,
    name: Option<String>,
}

async fn api_status(State(app): State<Shared>, Query(q): Query<VoiceQ>) -> Response {
    let (bin, model) = (whisper_bin(), whisper_model());
    Json(json!({
        "available": bin.is_some() && model.is_some() && which("ffmpeg").is_some(),
        "bin": bin, "model": model.as_deref().map(model_name),
        "ffmpeg": which("ffmpeg").is_some(),
        "busy": BUSY.load(Ordering::Relaxed) > 0, "queued": WAITING.load(Ordering::Relaxed),
        "prompt": prompt(&app, q.pane.as_deref()),
    }))
    .into_response()
}

/// Deletes the note's temp files however the request ends (done, error, phone hung up).
struct Tmp(Vec<PathBuf>);
impl Drop for Tmp {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

struct Busy;
impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn api_voice(State(app): State<Shared>, Query(q): Query<VoiceQ>, headers: HeaderMap, body: Body) -> Response {
    let t0 = Instant::now();
    let (Some(bin), Some(model)) = (whisper_bin(), whisper_model()) else {
        return json_err(StatusCode::SERVICE_UNAVAILABLE, "whisper.cpp is not installed on the box (see the README FAQ: Voice notes)");
    };
    if which("ffmpeg").is_none() {
        return json_err(StatusCode::SERVICE_UNAVAILABLE, "ffmpeg is not installed on the box");
    }
    let dir = home().join(".cache/sheprd/voice");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    let stamp = format!("{}-{}", crate::now_ms(), std::process::id());
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let src = dir.join(format!("{stamp}.{}", ext_for(q.name.as_deref().unwrap_or(""), ct)));
    let wav = dir.join(format!("{stamp}.16k.wav"));
    let _tmp = Tmp(vec![src.clone(), wav.clone()]);

    // Stream the upload to disk (no big buffer in the server).
    let mut n = 0usize;
    {
        let mut f = match tokio::fs::File::create(&src).await {
            Ok(f) => f,
            Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, e),
        };
        let mut s = body.into_data_stream();
        while let Some(chunk) = s.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => return json_err(StatusCode::BAD_REQUEST, format!("upload: {e}")),
            };
            n += chunk.len();
            if n > MAX_BYTES {
                return json_err(StatusCode::PAYLOAD_TOO_LARGE, "recording too large (max 96 MB)");
            }
            if let Err(e) = f.write_all(&chunk).await {
                return json_err(StatusCode::INTERNAL_SERVER_ERROR, e);
            }
        }
        if let Err(e) = f.flush().await {
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
    }
    if n == 0 {
        return json_err(StatusCode::BAD_REQUEST, "empty recording");
    }
    let up_ms = t0.elapsed().as_millis() as u64;

    // One note at a time; the rest queue here.
    WAITING.fetch_add(1, Ordering::Relaxed);
    let permit = GATE.acquire().await;
    WAITING.fetch_sub(1, Ordering::Relaxed);
    let Ok(_permit) = permit else {
        return json_err(StatusCode::SERVICE_UNAVAILABLE, "voice queue closed");
    };
    BUSY.fetch_add(1, Ordering::Relaxed);
    let _busy = Busy;
    let t1 = Instant::now();
    let queue_ms = (t1 - t0).as_millis() as u64 - up_ms;

    // Any container iOS/Chrome produce (m4a, caf, mov, webm, mp4, wav) → 16 kHz mono PCM.
    let mut c = tokio::process::Command::new("nice");
    c.args(["-n", NICE, "ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&src)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-t", &MAX_SECS.to_string()])
        .args(["-map_metadata", "-1", "-fflags", "+bitexact", "-c:a", "pcm_s16le"])
        .arg(&wav);
    if let Err(e) = run(c, Duration::from_secs(90)).await {
        return json_err(StatusCode::UNPROCESSABLE_ENTITY, format!("could not read the audio: {e}"));
    }
    let secs = std::fs::metadata(&wav).map(|m| wav_secs(m.len())).unwrap_or(0.0);
    let t2 = Instant::now();
    let convert_ms = (t2 - t1).as_millis() as u64;
    // Whisper hallucinates "you" / "Thank you." on silence: skip notes with no loud window.
    let w2 = wav.clone();
    let level = tokio::task::spawn_blocking(move || loudest_window_db(&w2)).await.unwrap_or(0.0);
    if secs < 0.3 || level < SILENCE_DB {
        let d = (secs * 10.0).round() / 10.0;
        return Json(json!({"text": "", "duration": d, "empty": true, "level": level.round()})).into_response();
    }

    let p = prompt(&app, q.pane.as_deref());
    let mut c = tokio::process::Command::new("nice");
    c.args(["-n", NICE]).arg(&bin).arg("-m").arg(&model).arg("-f").arg(&wav);
    let nt = threads().await;
    c.args(["-t", &nt.to_string(), "-np", "-nt", "-l", "en"]);
    if !p.is_empty() {
        c.arg("--prompt").arg(&p);
    }
    // The unpacked package keeps its libs beside bin/ (../lib); a system install needs nothing.
    if let Some(lib) = bin.parent().and_then(Path::parent).map(|d| d.join("lib")).filter(|l| l.join("libwhisper.so.1").exists()) {
        c.env("LD_LIBRARY_PATH", lib);
    }
    let out = match run(c, Duration::from_secs(60 + 3 * secs as u64)).await {
        Ok(o) => o,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("whisper: {e}")),
    };
    let text = clean(&out);
    let whisper_ms = t2.elapsed().as_millis() as u64;
    let model = model_name(&model);
    eprintln!(
        "voice: {secs:.1}s note ({n} B) → {} chars; up {up_ms} ms, queue {queue_ms} ms, ffmpeg {convert_ms} ms, whisper {model} -t {nt} {whisper_ms} ms",
        text.len()
    );
    Json(json!({
        "text": text, "duration": (secs * 10.0).round() / 10.0, "model": model, "threads": nt,
        "ms": {"upload": up_ms, "queue": queue_ms, "convert": convert_ms, "whisper": whisper_ms, "total": t0.elapsed().as_millis() as u64},
    }))
    .into_response()
}

/// A saved audio file → text, through the same gate, ffmpeg and whisper as a voice note (send.rs: a voice memo
/// shared to the home agent). `Ok("")` = silence. The file itself is left alone.
pub async fn transcribe_file(app: &Shared, src: &Path) -> Result<String, String> {
    let (Some(bin), Some(model)) = (whisper_bin(), whisper_model()) else { return Err("whisper.cpp is not installed on the box".into()) };
    if which("ffmpeg").is_none() {
        return Err("ffmpeg is not installed on the box".into());
    }
    let dir = home().join(".cache/sheprd/voice");
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let wav = dir.join(format!("{}-{}-shared.16k.wav", crate::now_ms(), std::process::id()));
    let _tmp = Tmp(vec![wav.clone()]);
    WAITING.fetch_add(1, Ordering::Relaxed);
    let permit = GATE.acquire().await;
    WAITING.fetch_sub(1, Ordering::Relaxed);
    let _permit = permit.map_err(|_| "voice queue closed".to_string())?;
    BUSY.fetch_add(1, Ordering::Relaxed);
    let _busy = Busy;
    let mut c = tokio::process::Command::new("nice");
    c.args(["-n", NICE, "ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-t", &MAX_SECS.to_string()])
        .args(["-map_metadata", "-1", "-fflags", "+bitexact", "-c:a", "pcm_s16le"])
        .arg(&wav);
    run(c, Duration::from_secs(90)).await.map_err(|e| format!("could not read the audio: {e}"))?;
    let secs = std::fs::metadata(&wav).map(|m| wav_secs(m.len())).unwrap_or(0.0);
    let w2 = wav.clone();
    let level = tokio::task::spawn_blocking(move || loudest_window_db(&w2)).await.unwrap_or(0.0);
    if secs < 0.3 || level < SILENCE_DB {
        return Ok(String::new());
    }
    let p = prompt(app, None);
    let mut c = tokio::process::Command::new("nice");
    c.args(["-n", NICE]).arg(&bin).arg("-m").arg(&model).arg("-f").arg(&wav);
    c.args(["-t", &threads().await.to_string(), "-np", "-nt", "-l", "en"]);
    if !p.is_empty() {
        c.arg("--prompt").arg(&p);
    }
    if let Some(lib) = bin.parent().and_then(Path::parent).map(|d| d.join("lib")).filter(|l| l.join("libwhisper.so.1").exists()) {
        c.env("LD_LIBRARY_PATH", lib);
    }
    let out = run(c, Duration::from_secs(60 + 3 * secs as u64)).await.map_err(|e| format!("whisper: {e}"))?;
    Ok(clean(&out))
}


/// Runs a child to completion with a deadline; killed if the deadline passes or
/// the request is dropped. Returns stdout.
async fn run(mut c: tokio::process::Command, limit: Duration) -> Result<String, String> {
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let child = c.spawn().map_err(|e| e.to_string())?;
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Err(_) => Err(format!("timed out after {} s", limit.as_secs())),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(o)) if !o.status.success() => {
            let err = String::from_utf8_lossy(&o.stderr);
            let last = err.lines().rev().find(|l| !l.trim().is_empty() && !l.contains("search path")).unwrap_or("");
            Err(format!("{} {}", o.status, last.trim()))
        }
        Ok(Ok(o)) => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
    }
}

/// Below this, the loudest 50 ms of a note is room noise, not speech (dBFS).
const SILENCE_DB: f64 = -42.0;

/// RMS level (dBFS) of the loudest 50 ms window of a 16 kHz s16 wav, read in
/// small chunks so a long note never sits in memory.
fn loudest_window_db(p: &Path) -> f64 {
    use std::io::Read;
    let Ok(f) = std::fs::File::open(p) else { return 0.0 };
    let mut r = std::io::BufReader::with_capacity(64 * 1024, f);
    let mut hdr = [0u8; 44];
    if r.read_exact(&mut hdr).is_err() {
        return -120.0;
    }
    const WIN: usize = 800;
    let (mut sum, mut k, mut best) = (0f64, 0usize, 0f64);
    let mut buf = vec![0u8; 64 * 1024];
    let mut have = 0; // an odd trailing byte carried to the next read
    loop {
        let n = match r.read(&mut buf[have..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => have + n,
        };
        let even = n & !1;
        for s in buf[..even].chunks_exact(2) {
            let v = i16::from_le_bytes([s[0], s[1]]) as f64 / 32768.0;
            sum += v * v;
            k += 1;
            if k == WIN {
                best = f64::max(best, sum / WIN as f64);
                (sum, k) = (0.0, 0);
            }
        }
        have = n - even;
        if have == 1 {
            buf[0] = buf[even];
        }
    }
    if k > 0 {
        best = f64::max(best, sum / k as f64);
    }
    if best <= 0.0 { -120.0 } else { 10.0 * best.log10() }
}

/// 16 kHz mono s16 with the 44-byte bitexact header.
fn wav_secs(len: u64) -> f64 {
    len.saturating_sub(44) as f64 / 32000.0
}

fn ext_for(name: &str, ct: &str) -> String {
    let e = Path::new(name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if !e.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()) {
        return e;
    }
    let ct = ct.split(';').next().unwrap_or("").trim();
    match ct {
        "audio/mp4" | "audio/x-m4a" | "audio/m4a" | "audio/aac" => "m4a",
        "audio/webm" | "video/webm" => "webm",
        "audio/ogg" => "ogg",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/x-caf" => "caf",
        "audio/mpeg" => "mp3",
        "video/quicktime" => "mov",
        "video/mp4" => "mp4",
        _ => "bin",
    }
    .into()
}

/// whisper's stdout → one paragraph, minus non-speech tags like `[BLANK_AUDIO]`.
fn clean(out: &str) -> String {
    let mut s = String::new();
    let mut depth = 0;
    for ch in out.chars() {
        match ch {
            '[' => depth += 1,
            ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => s.push(ch),
            _ => {}
        }
    }
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    // A note that is only a sound tag, e.g. "(wind blowing)".
    if s.starts_with('(') && s.ends_with(')') && !s[1..].contains('(') { String::new() } else { s }
}

fn prompt(app: &Shared, pane: Option<&str>) -> String {
    let state = app.inner.lock().unwrap().state.clone();
    let st: Value = serde_json::from_str(&state).unwrap_or(Value::Null);
    let mut work: Vec<String> = std::fs::read_dir(crate::work_dir())
        .map(|d| {
            d.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    work.sort();
    vocab(&st, pane, &work)
}

const COMMON: &[&str] = &[
    "Claude", "herdr", "sheprd", "cargo test", "git", "gh", "PR", "commit", "push", "rebase", "diff", "npm", "README",
    "API", "Tailscale", "deploy", "subagent", "TODO",
];

/// A comma-separated glossary in priority order: the chat's own agent, the other
/// agents (titles, names, pane ids, project dirs), ~/Work projects, then common
/// command words; deduped, capped to fit whisper's prompt window.
fn vocab(st: &Value, pane: Option<&str>, work: &[String]) -> String {
    let empty = vec![];
    let agents = st["agents"].as_array().unwrap_or(&empty);
    let mut ordered: Vec<&Value> = agents.iter().filter(|a| pane.is_some() && a["pane"].as_str() == pane).collect();
    ordered.extend(agents.iter().filter(|a| pane.is_none() || a["pane"].as_str() != pane));
    let mut terms: Vec<String> = vec![];
    for a in ordered {
        if let Some(t) = a["title"].as_str() {
            // Long titles are sentences; their first words carry the names.
            terms.push(t.split_whitespace().take(6).collect::<Vec<_>>().join(" "));
        }
        for k in ["name", "pane"] {
            if let Some(v) = a[k].as_str() {
                terms.push(v.into());
            }
        }
        if let Some(c) = a["cwd"].as_str().and_then(|c| Path::new(c).file_name()) {
            terms.push(c.to_string_lossy().into_owned());
        }
    }
    terms.extend(work.iter().cloned());
    terms.extend(COMMON.iter().map(|s| s.to_string()));
    let mut seen = HashSet::new();
    let mut out = String::new();
    for t in terms {
        let t = t.trim().trim_end_matches(['.', ',']).to_string();
        if t.len() < 2 || ["home", "work", "user"].contains(&t.to_lowercase().as_str()) || !seen.insert(t.to_lowercase()) {
            continue;
        }
        if out.len() + t.len() + 2 > PROMPT_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(&t);
    }
    if out.is_empty() { out } else { format!("Glossary: {out}.") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_tags_and_joins_lines() {
        assert_eq!(clean("\n Hello there,\n [BLANK_AUDIO] check the logs.\n"), "Hello there, check the logs.");
        assert_eq!(clean(" [BLANK_AUDIO]\n"), "");
        assert_eq!(clean(" (wind blowing)\n"), "");
        assert_eq!(clean(" Run it (twice) please."), "Run it (twice) please.");
    }

    #[test]
    fn ext_from_name_then_type() {
        assert_eq!(ext_for("note.M4A", ""), "m4a");
        assert_eq!(ext_for("", "audio/webm;codecs=opus"), "webm");
        assert_eq!(ext_for("blob", "video/quicktime"), "mov");
        assert_eq!(ext_for("x.../../", "nope"), "bin");
    }

    #[test]
    fn loudness_finds_the_loud_window() {
        let p = std::env::temp_dir().join(format!("mm-voice-test-{}.wav", std::process::id()));
        let mut b = vec![0u8; 44];
        for i in 0..32000u32 {
            // 1 s of near-silence, then 1 s of a half-scale square wave
            let v: i16 = if i < 16000 { 3 } else if i % 40 < 20 { 16384 } else { -16384 };
            b.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&p, &b).unwrap();
        let db = loudest_window_db(&p);
        assert!((db + 6.0).abs() < 0.5, "{db}");
        std::fs::write(&p, &b[..44 + 32000]).unwrap();
        assert!(loudest_window_db(&p) < SILENCE_DB);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn threads_follow_idle_cores() {
        assert_eq!(threads_for(0, 100, 4), 4);
        assert_eq!(threads_for(100, 100, 4), 1);
        assert_eq!(threads_for(50, 100, 4), 2);
        assert_eq!(threads_for(0, 100, 16), 4);
        assert_eq!(threads_for(0, 0, 2), 2);
    }

    #[test]
    fn wav_duration() {
        assert_eq!(wav_secs(44 + 32000 * 20), 20.0);
        assert_eq!(wav_secs(10), 0.0);
    }

    #[test]
    fn vocab_puts_this_chat_first_and_dedupes() {
        let st = json!({"agents": [
            {"pane": "w1:p1", "title": "Fix the flaky CI job in api-server", "name": "ci", "cwd": "/home/u/Work/api-server"},
            {"pane": "w6:p1", "title": "Voice notes", "name": "voice", "cwd": "/home/u/Work/sheprd"},
        ]});
        let v = vocab(&st, Some("w6:p1"), &["api-server".into(), "robot-sim".into()]);
        assert!(v.starts_with("Glossary: Voice notes, voice, w6:p1, sheprd, Fix the flaky CI job in"), "{v}");
        assert_eq!(v.matches("api-server").count(), 1);
        assert_eq!(v.matches("sheprd").count(), 1);
        assert!(v.contains("robot-sim") && v.contains("cargo test"));
        assert!(v.len() <= PROMPT_CHARS + 12);
        assert_eq!(vocab(&Value::Null, None, &[]).matches("Claude").count(), 1);
    }
}
