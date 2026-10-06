//! Usage rings: 5-hour / 7-day Claude limits and API-equivalent cost per session and per day.
//!
//! `GET /api/usage` →  the freshest rate-limit windows the mod reported (any session: they are account-wide),
//! else an estimate from transcript usage rows calibrated against the last reported percentage; plus cost/tokens
//! per day and per session computed from `~/.claude/projects/**.jsonl` assistant `usage` rows at API list
//! prices. Transcripts are read incrementally (only appended bytes, at most every 15 s, on request), rows are
//! deduplicated by request id, and only small aggregates are kept (10-minute buckets for 8 days, 60 days,
//! sessions of the last 30 days). Persisted: `<state dir>/usage.json` (last reported windows, calibration).
//! Hooks from main.rs: `routes`.

use crate::transcript::now_ms;
use crate::{App, Shared};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const BUCKET_MS: i64 = 600_000;
const DAY_MS: i64 = 86_400_000;
const KEEP_BUCKETS_MS: i64 = 8 * DAY_MS;
const KEEP_DAYS: usize = 60;
const KEEP_SESSIONS_MS: i64 = 30 * DAY_MS;
const SKIP_OLD_FILES_MS: i64 = 62 * DAY_MS;
const SCAN_EVERY_MS: i64 = 15_000;
const WINDOWS: [(&str, &str, i64); 2] = [("five_hour", "5-hour", 5 * 3600_000), ("seven_day", "7-day", 7 * DAY_MS)];

#[derive(Default)]
struct Sess {
    cost: f64,
    tokens: u64,
    last: i64,
    model: String,
}

#[derive(Default)]
struct Agg {
    /// transcript path -> bytes consumed (always at a line end)
    files: HashMap<PathBuf, u64>,
    /// request-id hash -> ts (dedupe: Claude writes one row per content block of a response)
    seen: HashMap<u64, i64>,
    /// 10-minute bucket start -> (cost, tokens)
    buckets: BTreeMap<i64, (f64, u64)>,
    /// local "YYYY-MM-DD" -> (cost, tokens)
    days: BTreeMap<String, (f64, u64)>,
    sessions: HashMap<String, Sess>,
    last_scan: i64,
    scan_ms: i64,
    rows: u64,
    /// oldest row seen (calibration needs the whole window covered)
    first_ts: i64,
    /// persisted: kind -> {pct, resetsAt, ts}
    limits: HashMap<String, Value>,
    /// persisted: kind -> estimated $ that fill the window
    calib: HashMap<String, f64>,
}

fn agg() -> &'static Mutex<Agg> {
    static A: OnceLock<Mutex<Agg>> = OnceLock::new();
    A.get_or_init(|| {
        let v: Value = std::fs::read(crate::state_dir().join("usage.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(json!({}));
        let limits = v["limits"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
        let calib = v["calib"].as_object().map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect()).unwrap_or_default();
        Mutex::new(Agg { limits, calib, ..Default::default() })
    })
}

fn save(a: &Agg) {
    if cfg!(test) {
        return;
    }
    let _ = std::fs::write(crate::state_dir().join("usage.json"), json!({"limits": a.limits, "calib": a.calib}).to_string());
}

pub fn routes() -> Router<Shared> {
    Router::new().route("/api/usage", get(api_usage))
}

// ---------------------------------------------------------------------------
// pricing

/// $ per million tokens: (input, output, cache read). Cache writes: 1.25× input (5 min), 2× input (1 h).
/// API list prices (Anthropic, 2026-09); on a subscription these are "API-equivalent" figures.
fn price(model: &str) -> (f64, f64, f64) {
    let m = model.trim_start_matches("anthropic.").trim_start_matches("us.anthropic.");
    let has = |p: &str| m.starts_with(p);
    if m == "<synthetic>" {
        (0.0, 0.0, 0.0)
    } else if has("claude-fable-5-1") || has("claude-mythos-5-1") {
        (10.0, 50.0, 0.25)
    } else if has("claude-fable") || has("claude-mythos") {
        (10.0, 50.0, 1.0)
    } else if has("claude-opus-5-5") {
        (4.0, 20.0, 0.20)
    } else if has("claude-opus-5") || has("claude-opus-4-5") || has("claude-opus-4-6") || has("claude-opus-4-7") || has("claude-opus-4-8") {
        (5.0, 25.0, 0.50)
    } else if has("claude-opus") || has("claude-3-opus") {
        (15.0, 75.0, 1.50)
    } else if has("claude-sonnet-5") {
        (2.0, 10.0, 0.20)
    } else if has("claude-sonnet") || has("claude-3-7-sonnet") || has("claude-3-5-sonnet") {
        (3.0, 15.0, 0.30)
    } else if has("claude-haiku-4") {
        (1.0, 5.0, 0.10)
    } else if has("claude-3-5-haiku") || has("claude-haiku-3") {
        (0.8, 4.0, 0.08)
    } else if m.contains("haiku") {
        (1.0, 5.0, 0.10)
    } else if m.contains("sonnet") {
        (2.0, 10.0, 0.20)
    } else {
        (4.0, 20.0, 0.20)
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation: Option<CacheW>,
    speed: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CacheW {
    ephemeral_5m_input_tokens: u64,
    ephemeral_1h_input_tokens: u64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Msg {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Row {
    #[serde(rename = "type")]
    t: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<Msg>,
}

/// (cost $, fresh tokens = input + cache writes + output) of one response.
fn cost(model: &str, u: &Usage) -> (f64, u64) {
    let (pi, po, pr) = price(model);
    let (w5, w1) = match &u.cache_creation {
        Some(c) if c.ephemeral_5m_input_tokens + c.ephemeral_1h_input_tokens > 0 => (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens),
        _ => (u.cache_creation_input_tokens, 0),
    };
    let mut c = (u.input_tokens as f64 * pi
        + u.output_tokens as f64 * po
        + u.cache_read_input_tokens as f64 * pr
        + w5 as f64 * pi * 1.25
        + w1 as f64 * pi * 2.0)
        / 1e6;
    if u.speed.as_deref() == Some("fast") {
        c *= 2.0;
    }
    (c, u.input_tokens + u.output_tokens + u.cache_creation_input_tokens)
}

/// $ of one response's transcript `usage` object (the transcript `ctx` estimate's session cost).
pub fn cost_of(model: &str, usage: &Value) -> f64 {
    Usage::deserialize(usage).map(|u| cost(model, &u).0).unwrap_or(0.0)
}

fn hash(s: &str) -> u64 {
    // FNV-1a
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// "2026-10-05T07:49:57.608Z" → unix ms (UTC "Z" or ±HH:MM).
fn iso_ms(s: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let mut rest = s.get(19..)?;
    let mut ms = 0;
    if let Some(r) = rest.strip_prefix('.') {
        let digits: String = r.chars().take_while(char::is_ascii_digit).collect();
        ms = format!("{:0<3}", &digits[..digits.len().min(3)]).parse::<i64>().unwrap_or(0);
        rest = &r[digits.len()..];
    }
    let off = match rest {
        "" | "Z" | "z" => 0,
        r if r.len() == 6 && (r.starts_with('+') || r.starts_with('-')) => {
            let v = r[1..3].parse::<i64>().ok()? * 60 + r[4..6].parse::<i64>().ok()?;
            if r.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + se) - off * 60) * 1000 + ms)
}

/// Local calendar date of `ms`.
fn local_date(ms: i64) -> String {
    let t: libc::time_t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r only writes into `tm`.
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

// ---------------------------------------------------------------------------
// incremental scan

fn transcripts() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(crate::home().join(".claude/projects")) else { return out };
    for p in projects.flatten() {
        let Ok(entries) = std::fs::read_dir(p.path()) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(sid) = name.strip_suffix(".jsonl") {
                out.push((path, sid.to_string()));
            } else if path.is_dir() {
                // <sid>/subagents/*.jsonl count toward their parent session
                if let Ok(subs) = std::fs::read_dir(path.join("subagents")) {
                    for s in subs.flatten() {
                        if s.file_name().to_string_lossy().ends_with(".jsonl") {
                            out.push((s.path(), name.clone()));
                        }
                    }
                }
            }
        }
    }
    out
}

impl Agg {
    fn add(&mut self, sid: &str, line: &[u8]) {
        let Ok(r) = serde_json::from_slice::<Row>(line) else { return };
        if r.t.as_deref() != Some("assistant") {
            return;
        }
        let Some(m) = r.message else { return };
        let Some(u) = m.usage else { return };
        let Some(ts) = r.timestamp.as_deref().and_then(iso_ms) else { return };
        let key = r.request_id.or(m.id).unwrap_or_default();
        if !key.is_empty() && self.seen.insert(hash(&key), ts).is_some() {
            return;
        }
        let model = m.model.unwrap_or_default();
        let (c, tok) = cost(&model, &u);
        if c == 0.0 && tok == 0 {
            return;
        }
        self.rows += 1;
        if self.first_ts == 0 || ts < self.first_ts {
            self.first_ts = ts;
        }
        let now = now_ms();
        if ts > now - KEEP_BUCKETS_MS {
            let b = self.buckets.entry(ts - ts.rem_euclid(BUCKET_MS)).or_default();
            b.0 += c;
            b.1 += tok;
        }
        if ts > now - (KEEP_DAYS as i64 + 1) * DAY_MS {
            let d = self.days.entry(local_date(ts)).or_default();
            d.0 += c;
            d.1 += tok;
        }
        if ts > now - KEEP_SESSIONS_MS {
            let s = self.sessions.entry(sid.to_string()).or_default();
            s.cost += c;
            s.tokens += tok;
            if ts >= s.last {
                s.last = ts;
                s.model = model;
            }
        }
    }

    /// Reads what was appended to every transcript since the last scan.
    fn scan(&mut self) {
        let t0 = now_ms();
        let now = t0;
        let mut buf = Vec::new();
        for (path, sid) in transcripts() {
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let len = meta.len();
            let off = self.files.get(&path).copied();
            if off == Some(len) {
                continue;
            }
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0);
            if off.is_none() && mtime < now - SKIP_OLD_FILES_MS {
                self.files.insert(path, len);
                continue;
            }
            let mut off = off.filter(|o| *o <= len).unwrap_or(0);
            let Ok(f) = std::fs::File::open(&path) else { continue };
            let mut rd = BufReader::with_capacity(64 * 1024, f);
            if rd.seek(SeekFrom::Start(off)).is_err() {
                continue;
            }
            loop {
                buf.clear();
                let Ok(n) = rd.read_until(b'\n', &mut buf) else { break };
                if n == 0 || buf.last() != Some(&b'\n') {
                    break; // EOF or a line still being written
                }
                off += n as u64;
                // cheap prefilter before parsing
                if memfind(&buf, b"\"usage\"") && memfind(&buf, b"\"assistant\"") {
                    self.add(&sid, &buf);
                }
            }
            self.files.insert(path, off);
            if buf.capacity() > 1 << 20 {
                buf = Vec::new();
            }
        }
        self.buckets.retain(|t, _| *t > now - KEEP_BUCKETS_MS);
        self.seen.retain(|_, t| *t > now - KEEP_BUCKETS_MS);
        self.sessions.retain(|_, s| s.last > now - KEEP_SESSIONS_MS);
        while self.days.len() > KEEP_DAYS {
            let k = self.days.keys().next().cloned().unwrap_or_default();
            self.days.remove(&k);
        }
        self.last_scan = now_ms();
        self.scan_ms = self.last_scan - t0;
    }

    fn window(&self, from: i64, to: i64) -> (f64, u64) {
        if from > to {
            return (0.0, 0);
        }
        self.buckets
            .range(from - from.rem_euclid(BUCKET_MS)..=to)
            .fold((0.0, 0), |(c, t), (_, (bc, bt))| (c + bc, t + bt))
    }
}

fn memfind(h: &[u8], n: &[u8]) -> bool {
    h.windows(n.len()).any(|w| w == n)
}

// ---------------------------------------------------------------------------
// limits

/// Newest reported window per kind across all agents' `ctx.rateLimits` (with the time it was measured).
fn reported(app: &App) -> HashMap<String, Value> {
    let s: Value = serde_json::from_str(&app.inner.lock().unwrap().state).unwrap_or(json!({}));
    let mut out: HashMap<String, Value> = HashMap::new();
    for a in s["agents"].as_array().into_iter().flatten() {
        let ts = a["ctx"]["ts"].as_i64().unwrap_or(0);
        for r in a["ctx"]["rateLimits"].as_array().into_iter().flatten() {
            let (Some(kind), Some(pct)) = (r["kind"].as_str(), r["pct"].as_f64()) else { continue };
            if out.get(kind).and_then(|v| v["ts"].as_i64()).is_none_or(|t| t < ts) {
                out.insert(kind.to_string(), json!({"pct": pct, "resetsAt": r["resetsAt"], "ts": ts}));
            }
        }
    }
    out
}

fn limits(a: &mut Agg, rep: HashMap<String, Value>) -> Vec<Value> {
    let now = now_ms();
    let mut dirty = false;
    for (k, v) in rep {
        if a.limits.get(&k).and_then(|o| o["ts"].as_i64()).is_none_or(|t| t < v["ts"].as_i64().unwrap_or(0)) {
            a.limits.insert(k, v);
            dirty = true;
        }
    }
    let mut out = Vec::new();
    for (kind, label, dur) in WINDOWS {
        let rep = a.limits.get(kind).cloned();
        let resets = rep.as_ref().and_then(|r| r["resetsAt"].as_str()).and_then(iso_ms);
        let live = rep.as_ref().zip(resets).filter(|(_, r)| *r > now);
        let (start, src, pct, measured) = match live {
            Some((r, reset)) => {
                let start = reset - dur;
                let pct = r["pct"].as_f64().unwrap_or(0.0);
                let ts = r["ts"].as_i64().unwrap_or(now);
                // calibrate: $ in the window when it was measured ≈ pct of the limit
                let (c_then, _) = a.window(start, ts);
                if pct >= 5.0 && c_then > 0.0 && a.first_ts > 0 && a.first_ts <= start {
                    a.calib.insert(kind.to_string(), c_then / (pct / 100.0));
                    dirty = true;
                }
                // add what was spent since the measurement (estimated), if any
                let (c_after, _) = a.window(ts + BUCKET_MS, now);
                let extra = a.calib.get(kind).filter(|l| **l > 0.0).map(|l| c_after / l * 100.0).unwrap_or(0.0);
                (start, if extra > 0.5 { "mod+est" } else { "mod" }, Some((pct + extra).min(100.0)), Some(ts))
            }
            None => {
                let start = now - dur;
                let (c, _) = a.window(start, now);
                let pct = a.calib.get(kind).filter(|l| **l > 0.0).map(|l| (c / l * 100.0).min(100.0));
                (start, if pct.is_some() { "est" } else { "transcript" }, pct, None)
            }
        };
        let (c, t) = a.window(start, now);
        out.push(json!({
            "kind": kind, "label": label, "pct": pct.map(|p| (p * 10.0).round() / 10.0), "src": src,
            "resetsAt": live.map(|(r, _)| r["resetsAt"].clone()).unwrap_or(Value::Null),
            "measuredAt": measured, "windowStart": start,
            "cost": (c * 100.0).round() / 100.0, "tokens": t,
        }));
    }
    if dirty {
        save(a);
    }
    out
}

async fn api_usage(State(app): State<Shared>) -> Response {
    let rep = reported(&app);
    let v = tokio::task::spawn_blocking(move || {
        let mut a = agg().lock().unwrap();
        if now_ms() - a.last_scan >= SCAN_EVERY_MS {
            a.scan();
            crate::trim_heap();
        }
        let now = now_ms();
        let limits = limits(&mut a, rep);
        let today = local_date(now);
        let days: Vec<Value> = a
            .days
            .iter()
            .rev()
            .take(14)
            .map(|(d, (c, t))| json!({"date": d, "cost": (c * 100.0).round() / 100.0, "tokens": t}))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let t = a.days.get(&today).copied().unwrap_or_default();
        let sessions: serde_json::Map<String, Value> = a
            .sessions
            .iter()
            .filter(|(_, s)| s.last > now - 7 * DAY_MS)
            .map(|(k, s)| (k.clone(), json!({"cost": (s.cost * 100.0).round() / 100.0, "tokens": s.tokens, "last": s.last, "model": s.model})))
            .collect();
        json!({
            "now": now, "limits": limits,
            "today": {"date": today, "cost": (t.0 * 100.0).round() / 100.0, "tokens": t.1},
            "days": days, "sessions": sessions,
            "scan": {"ms": a.scan_ms, "files": a.files.len(), "rows": a.rows, "at": a.last_scan},
        })
    })
    .await
    .unwrap_or(json!({"error": "scan failed"}));
    Json(v).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_and_cost() {
        assert_eq!(price("claude-opus-5-5"), (4.0, 20.0, 0.20));
        assert_eq!(price("claude-opus-5"), (5.0, 25.0, 0.50));
        assert_eq!(price("claude-haiku-4-5-20251001"), (1.0, 5.0, 0.10));
        assert_eq!(price("claude-sonnet-5-5"), (2.0, 10.0, 0.20));
        assert_eq!(price("claude-fable-5-1"), (10.0, 50.0, 0.25));
        assert_eq!(price("<synthetic>"), (0.0, 0.0, 0.0));
        let u = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
            cache_creation_input_tokens: 2_000_000,
            cache_creation: Some(CacheW { ephemeral_5m_input_tokens: 1_000_000, ephemeral_1h_input_tokens: 1_000_000 }),
            speed: None,
        };
        // 4 + 20 + 0.2 + 5 + 8
        let (c, t) = cost("claude-opus-5-5", &u);
        assert!((c - 37.2).abs() < 1e-9, "{c}");
        assert_eq!(t, 4_000_000);
        let fast = Usage { output_tokens: 1_000_000, speed: Some("fast".into()), ..Default::default() };
        assert!((cost("claude-opus-5-5", &fast).0 - 40.0).abs() < 1e-9);
    }

    #[test]
    fn iso() {
        assert_eq!(iso_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(iso_ms("2026-10-05T07:49:57.608Z"), Some(1_791_186_597_608));
        assert_eq!(iso_ms("2026-10-05T09:49:57+02:00"), Some(1_791_186_597_000));
    }

    #[test]
    fn rows_dedupe_and_windows() {
        let mut a = Agg::default();
        let ts = now_ms() - 3600_000;
        let iso = |ms: i64| {
            // build an ISO string from ms via the inverse: days/secs (UTC)
            let s = ms / 1000;
            let (days, rem) = (s.div_euclid(86400), s.rem_euclid(86400));
            let z = days + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z - era * 146_097;
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
            format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z", rem / 3600, rem / 60 % 60, rem % 60)
        };
        let row = |req: &str| {
            format!(
                r#"{{"type":"assistant","timestamp":"{}","requestId":"{req}","message":{{"id":"m","model":"claude-opus-5-5","usage":{{"input_tokens":0,"output_tokens":1000000}}}}}}"#,
                iso(ts)
            )
        };
        a.add("s1", row("r1").as_bytes());
        a.add("s1", row("r1").as_bytes()); // same response, second content block
        a.add("s1", row("r2").as_bytes());
        a.add("s1", br#"{"type":"user","message":{"usage":{}}}"#);
        assert_eq!(a.rows, 2);
        assert!((a.sessions["s1"].cost - 40.0).abs() < 1e-9);
        let (c, t) = a.window(now_ms() - 5 * 3600_000, now_ms());
        assert!((c - 40.0).abs() < 1e-9);
        assert_eq!(t, 2_000_000);
        assert_eq!(a.window(now_ms() - 600_000, now_ms()).0, 0.0);
        // calibration: 10% reported now with resetsAt in 4h → limit ≈ $400; a fallback window then reads 10%
        let resets = iso(now_ms() + 4 * 3600_000);
        a.first_ts = 1; // pretend transcripts cover the whole window
        let rep = HashMap::from([("five_hour".to_string(), json!({"pct": 10.0, "resetsAt": resets, "ts": now_ms()}))]);
        let l = limits(&mut a, rep);
        assert_eq!(l[0]["src"], "mod");
        assert_eq!(l[0]["pct"], 10.0);
        assert!((a.calib["five_hour"] - 400.0).abs() < 1e-6);
        a.limits.clear();
        let l = limits(&mut a, HashMap::new());
        assert_eq!(l[0]["src"], "est");
        assert_eq!(l[0]["pct"], 10.0);
        assert_eq!(l[1]["src"], "transcript");
        assert_eq!(l[1]["pct"], Value::Null);
    }
}
