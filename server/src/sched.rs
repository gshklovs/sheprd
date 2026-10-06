//! Recurring agents: plain-language schedules ("every weekday 8:00", "every 2h")
//! parsed into cron expressions that the server runs. At fire time an agent is
//! started (or the previous run's pane is re-prompted when `reuse` is on and it
//! is idle) and a push goes out when that run finishes.
//!
//! One task sleeps until the earliest due time (or until a schedule changes);
//! it only watches state events while a run is pending. Persisted in
//! `<state dir>/sched.json`. Missed fire times (server down) are skipped.

use crate::transcript::{cut, now_ms};
use crate::{App, Shared, herdr, json_err};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

const PENDING_TTL_MS: i64 = 12 * 3600_000;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/sched", get(api_list).post(api_create))
        .route("/api/sched/parse", post(api_parse))
        .route("/api/sched/{id}", post(api_update).delete(api_delete))
        .route("/api/sched/{id}/run", post(api_run_now))
}

// ---------------------------------------------------------------------------
// Cron

/// One 5-field cron expression as bitsets.
#[derive(Debug, Clone, PartialEq)]
struct Cron {
    min: u64,
    hour: u32,
    dom: u32,
    mon: u16,
    dow: u8,
    dom_any: bool,
    dow_any: bool,
}

fn field(s: &str, lo: u32, hi: u32) -> Result<(u64, bool), String> {
    let mut bits = 0u64;
    let mut any = false;
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (r, st.parse::<u32>().map_err(|_| format!("bad step in {part:?}"))?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(format!("bad step in {part:?}"));
        }
        let (a, b) = if range == "*" {
            any = step == 1;
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().map_err(|_| format!("bad range {part:?}"))?, b.parse().map_err(|_| format!("bad range {part:?}"))?)
        } else {
            let v: u32 = range.parse().map_err(|_| format!("bad value {part:?}"))?;
            (v, if part.contains('/') { hi } else { v })
        };
        if a < lo || b > hi || a > b {
            return Err(format!("{part:?} is out of range {lo}-{hi}"));
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, any))
}

fn parse_cron(expr: &str) -> Result<Cron, String> {
    let f: Vec<&str> = expr.split_whitespace().collect();
    if f.len() != 5 {
        return Err("a cron expression has 5 fields: minute hour day-of-month month day-of-week".into());
    }
    let (min, _) = field(f[0], 0, 59)?;
    let (hour, _) = field(f[1], 0, 23)?;
    let (dom, dom_any) = field(f[2], 1, 31)?;
    let (mon, _) = field(f[3], 1, 12)?;
    let (mut dow, dow_any) = field(f[4], 0, 7)?;
    if dow & (1 << 7) != 0 {
        dow = (dow | 1) & 0x7f;
    }
    Ok(Cron { min, hour: hour as u32, dom: dom as u32, mon: mon as u16, dow: dow as u8, dom_any, dow_any })
}

/// Local broken-down time of `secs` (unix seconds).
fn local(secs: i64) -> libc::tm {
    unsafe extern "C" {
        fn tzset();
    }
    static TZ: std::sync::Once = std::sync::Once::new();
    TZ.call_once(|| unsafe { tzset() });
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm
}

impl Cron {
    fn day_ok(&self, tm: &libc::tm) -> bool {
        let dom = self.dom & (1 << tm.tm_mday) != 0;
        let dow = self.dow & (1 << tm.tm_wday) != 0;
        let mon = self.mon & (1 << (tm.tm_mon + 1)) != 0;
        mon && match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }
    /// First minute strictly after `after_ms`, within ~400 days.
    fn next(&self, after_ms: i64) -> Option<i64> {
        let mut t = (after_ms.div_euclid(60_000) + 1) * 60;
        let end = t + 400 * 86400;
        while t < end {
            let tm = local(t);
            if !self.day_ok(&tm) {
                // jump to the next local midnight (+ a minute of slack for DST)
                t += ((23 - tm.tm_hour as i64) * 3600 + (59 - tm.tm_min as i64) * 60 + 60).max(60);
                continue;
            }
            if self.hour & (1 << tm.tm_hour) == 0 {
                t += (60 - tm.tm_min as i64) * 60;
                continue;
            }
            if self.min & (1 << tm.tm_min) != 0 {
                return Some(t * 1000);
            }
            t += 60;
        }
        None
    }
}

/// Earliest next fire of a `;`-separated list of cron expressions.
fn next_fire(crons: &str, after_ms: i64) -> Option<i64> {
    crons.split(';').filter_map(|c| parse_cron(c.trim()).ok()).filter_map(|c| c.next(after_ms)).min()
}

// ---------------------------------------------------------------------------
// Plain language → cron

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn day_of(w: &str) -> Option<u8> {
    Some(match w.trim_end_matches('s') {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tuesday" => 2,
        "wed" | "wednesday" => 3,
        "thu" | "thur" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        _ => return None,
    })
}

fn list(bits: impl Iterator<Item = u32>) -> String {
    let v: Vec<u32> = bits.collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 {
            j += 1;
        }
        out.push(if j - i >= 2 { format!("{}-{}", v[i], v[j]) } else if j > i { format!("{},{}", v[i], v[j]) } else { v[i].to_string() });
        i = j + 1;
    }
    out.join(",")
}

static RAW_CRON: LazyLock<regex_lite::Regex> =
    LazyLock::new(|| regex_lite::Regex::new(r"^[0-9*/,-]+(\s+[0-9*/,-]+){4}(\s*;\s*[0-9*/,-]+(\s+[0-9*/,-]+){4})*$").unwrap());
static INTERVAL: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r"^(?:every|each)?\s*(\d+)?\s*(minutes?|mins?|m|hours?|hrs?|h)$").unwrap()
});
static TOKEN: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
    regex_lite::Regex::new(r"(\d{1,2}):(\d{2})\s*(am|pm|a\.m\.|p\.m\.)?|(\d{1,2})\s*(am|pm|a\.m\.|p\.m\.)|(\d{1,2})|([a-z]+)|(-)").unwrap()
});

/// Parses a schedule: plain language or raw cron. Returns (cron, description).
pub fn parse_when(text: &str) -> Result<(String, String), String> {
    let t = text.trim().to_lowercase();
    if t.is_empty() {
        return Err("say when, e.g. \"every weekday 8:00\" or \"every 2h\"".into());
    }
    if RAW_CRON.is_match(&t) {
        for c in t.split(';') {
            parse_cron(c.trim())?;
        }
        let norm = t.split(';').map(|c| c.split_whitespace().collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("; ");
        return Ok((norm.clone(), format!("cron {norm}")));
    }
    let t2 = t.replace("every other", "every 2");
    let t2 = if t2 == "hourly" || t2 == "every hour" { "every 1h".to_string() } else { t2 };
    if let Some(c) = INTERVAL.captures(t2.trim()) {
        let n: u32 = c.get(1).map(|m| m.as_str().parse().unwrap_or(0)).unwrap_or(1);
        let unit = &c[2];
        if unit.starts_with('m') {
            if !(5..=60).contains(&n) || 60 % n != 0 {
                return Err("minute intervals must divide an hour and be ≥ 5 (5, 10, 15, 20, 30, 60)".into());
            }
            return Ok(if n == 60 { ("0 * * * *".into(), "Every hour".into()) } else { (format!("*/{n} * * * *"), format!("Every {n} minutes")) });
        }
        if !(1..=24).contains(&n) {
            return Err("hour intervals go from 1 to 24".into());
        }
        let cron = match n {
            1 => "0 * * * *".to_string(),
            24 => "0 0 * * *".to_string(),
            _ if 24 % n == 0 => format!("0 */{n} * * *"),
            _ => format!("0 {} * * *", list((0..24).step_by(n as usize))),
        };
        return Ok((cron, if n == 1 { "Every hour".into() } else { format!("Every {n} hours") }));
    }
    let mut days: u8 = 0;
    let mut times: Vec<(u32, u32)> = Vec::new();
    let mut implied: Vec<(u32, u32)> = Vec::new();
    let mut last_day: Option<u8> = None;
    let mut range = false;
    let mut unknown: Vec<String> = Vec::new();
    for c in TOKEN.captures_iter(&t2) {
        let ampm = |h: u32, s: Option<&str>| -> Result<u32, String> {
            match s.map(|x| x.starts_with('p')) {
                None => Ok(h),
                Some(_) if !(1..=12).contains(&h) => Err(format!("{h} is not a 12-hour time")),
                Some(true) => Ok(if h == 12 { 12 } else { h + 12 }),
                Some(false) => Ok(if h == 12 { 0 } else { h }),
            }
        };
        if let (Some(h), Some(m)) = (c.get(1), c.get(2)) {
            let h = ampm(h.as_str().parse().unwrap_or(99), c.get(3).map(|x| x.as_str()))?;
            times.push((h, m.as_str().parse().unwrap_or(99)));
        } else if let Some(h) = c.get(4) {
            times.push((ampm(h.as_str().parse().unwrap_or(99), c.get(5).map(|x| x.as_str()))?, 0));
        } else if let Some(h) = c.get(6) {
            times.push((h.as_str().parse().unwrap_or(99), 0));
        } else if c.get(8).is_some() {
            range = last_day.is_some();
            continue;
        } else if let Some(w) = c.get(7).map(|x| x.as_str()) {
            if let Some(d) = day_of(w) {
                if range {
                    let a = last_day.unwrap_or(d);
                    let mut x = a;
                    loop {
                        days |= 1 << x;
                        if x == d {
                            break;
                        }
                        x = (x + 1) % 7;
                    }
                } else {
                    days |= 1 << d;
                }
                last_day = Some(d);
                range = false;
                continue;
            }
            match w {
                "weekday" | "weekdays" | "workday" | "workdays" => days |= 0b0111110,
                "weekend" | "weekends" => days |= 0b1000001,
                "daily" | "day" | "days" | "everyday" | "night" | "nightly" => {
                    days |= 0x7f;
                    if w.starts_with("night") {
                        implied.push((21, 0));
                    }
                }
                "noon" | "midday" => times.push((12, 0)),
                "midnight" => times.push((0, 0)),
                "morning" | "mornings" => {
                    days |= 0x7f;
                    implied.push((8, 0));
                }
                "afternoon" | "afternoons" => {
                    days |= 0x7f;
                    implied.push((14, 0));
                }
                "evening" | "evenings" | "tonight" => {
                    days |= 0x7f;
                    implied.push((18, 0));
                }
                "every" | "each" | "at" | "on" | "and" | "the" | "in" | "of" | "o" | "clock" | "h" | "hrs" => {}
                _ => unknown.push(w.to_string()),
            }
        }
        last_day = None;
        range = false;
    }
    if !unknown.is_empty() {
        return Err(format!("didn't understand {:?} — try \"every weekday 8:00\", \"mon,wed 19:30\", \"every 2h\" or a cron line", unknown.join(" ")));
    }
    // A word like "morning" implies a time; explicit times win.
    if times.is_empty() {
        times = implied;
    }
    times.sort();
    times.dedup();
    if times.is_empty() {
        return Err("add a time, e.g. \"8:00\" or \"9am\"".into());
    }
    if let Some(bad) = times.iter().find(|(h, m)| *h > 23 || *m > 59) {
        return Err(format!("{}:{:02} is not a time of day", bad.0, bad.1));
    }
    if days == 0 {
        days = 0x7f;
    }
    let dow = if days == 0x7f { "*".to_string() } else { list((0..7).filter(|d| days & (1 << d) != 0)) };
    let mut by_min: Vec<(u32, Vec<u32>)> = Vec::new();
    for (h, m) in &times {
        match by_min.iter_mut().find(|(mm, _)| mm == m) {
            Some((_, hs)) => hs.push(*h),
            None => by_min.push((*m, vec![*h])),
        }
    }
    let cron = by_min.iter().map(|(m, hs)| format!("{m} {} * * {dow}", list(hs.iter().copied()))).collect::<Vec<_>>().join("; ");
    let dd = match days {
        0x7f => "Every day".to_string(),
        0b0111110 => "Weekdays".to_string(),
        0b1000001 => "Weekends".to_string(),
        _ => (0..7).filter(|d| days & (1 << d) != 0).map(|d| DAY_NAMES[d]).collect::<Vec<_>>().join(", "),
    };
    let tt = times.iter().map(|(h, m)| format!("{h:02}:{m:02}")).collect::<Vec<_>>();
    let tt = match tt.len() {
        1 => tt[0].clone(),
        n => format!("{} and {}", tt[..n - 1].join(", "), tt[n - 1]),
    };
    Ok((cron, format!("{dd} at {tt}")))
}

// ---------------------------------------------------------------------------
// Schedules

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default)]
struct Pending {
    pane: String,
    started: i64,
    working: bool,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
struct Sched {
    id: String,
    label: String,
    cwd: String,
    prompt: String,
    when: String,
    cron: String,
    desc: String,
    paused: bool,
    reuse: bool,
    ws: Option<String>,
    created: i64,
    next: Option<i64>,
    last_run: Option<i64>,
    last_pane: Option<String>,
    last_status: Option<String>,
    runs: u32,
    pending: Option<Pending>,
}

static SCHEDS: LazyLock<Mutex<Vec<Sched>>> = LazyLock::new(|| Mutex::new(load()));
static WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);

fn file() -> std::path::PathBuf {
    crate::state_dir().join("sched.json")
}

fn load() -> Vec<Sched> {
    let mut v: Vec<Sched> = std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let now = now_ms();
    for s in v.iter_mut() {
        s.next = if s.paused { None } else { next_fire(&s.cron, now) };
    }
    v
}

fn save(v: &[Sched]) {
    if let Ok(b) = serde_json::to_vec_pretty(v) {
        let tmp = file().with_extension("json.tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(tmp, file());
        }
    }
}

fn view(s: &Sched) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
    let mut upcoming = Vec::new();
    let mut t = now_ms();
    if !s.paused {
        for _ in 0..3 {
            match next_fire(&s.cron, t) {
                Some(n) => {
                    upcoming.push(n);
                    t = n;
                }
                None => break,
            }
        }
    }
    v["upcoming"] = json!(upcoming);
    v
}

fn with<R>(f: impl FnOnce(&mut Vec<Sched>) -> R) -> R {
    let mut g = SCHEDS.lock().unwrap();
    let r = f(&mut g);
    save(&g);
    r
}

async fn api_list() -> Response {
    let list: Vec<Value> = SCHEDS.lock().unwrap().iter().map(view).collect();
    let timers = systemd_timers().await;
    Json(json!({"schedules": list, "timers": timers, "now": now_ms()})).into_response()
}

// ---------------------------------------------------------------------------
// systemd user timers (read-only): unit files and transient `systemd-run --on-calendar` ones,
// e.g. an agent's `herdr agent prompt <pane> "…"` wake-up.

async fn systemctl(args: &[&str]) -> Option<String> {
    let mut c = tokio::process::Command::new("systemctl");
    c.arg("--user").args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
    let o = tokio::time::timeout(Duration::from_secs(4), c.output()).await.ok()?.ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// `systemctl show` output (blank-line separated blocks) → one map per unit, keyed by Id.
fn show_blocks(out: &str) -> std::collections::HashMap<String, std::collections::HashMap<String, String>> {
    let mut all = std::collections::HashMap::new();
    for block in out.split("\n\n") {
        let m: std::collections::HashMap<String, String> =
            block.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        if let Some(id) = m.get("Id").cloned() {
            all.insert(id, m);
        }
    }
    all
}

static AGENT_PROMPT: LazyLock<regex_lite::Regex> =
    LazyLock::new(|| regex_lite::Regex::new(r#"\bagent\s+prompt\s+['"]?([^\s'"]+)['"]?\s*(.*)$"#).unwrap());

/// The command a timer's service runs: systemd-run keeps the quoted form in Description.
fn timer_command(desc: &str, exec: &str) -> String {
    if let Some(c) = desc.strip_prefix("[systemd-run] ") {
        return c.to_string();
    }
    exec.split_once("argv[]=").map(|(_, r)| r.split(" ; ").next().unwrap_or(r).trim().to_string()).unwrap_or_default()
}

/// (pane, prompt text) when the command prompts a herdr agent.
fn agent_target(cmd: &str) -> Option<(String, String)> {
    let c = AGENT_PROMPT.captures(cmd)?;
    let msg = c[2].trim();
    let msg = msg.strip_prefix('"').and_then(|m| m.strip_suffix('"')).or_else(|| msg.strip_prefix('\'').and_then(|m| m.strip_suffix('\''))).unwrap_or(msg);
    Some((c[1].to_string(), msg.to_string()))
}

/// "OnCalendar=2026-10-08 22:42:00 America/Los_Angeles" from `{ OnCalendar=… ; next_elapse=… }`.
fn timer_spec(cal: &str, mono: &str) -> String {
    let take = |s: &str| {
        s.split(" } { ")
            .filter_map(|e| {
                let e = e.trim().trim_start_matches('{').trim();
                let (k, rest) = e.split_once('=')?;
                Some(format!("{k} {}", rest.split(" ;").next().unwrap_or(rest).trim()))
            })
            .collect::<Vec<_>>()
    };
    let mut v = take(cal);
    v.extend(take(mono));
    v.join("; ")
}

async fn systemd_timers() -> Vec<Value> {
    let Some(list) = systemctl(&["list-timers", "--all", "--output=json", "--no-pager"]).await else { return vec![] };
    let list: Vec<Value> = serde_json::from_str(&list).unwrap_or_default();
    if list.is_empty() {
        return vec![];
    }
    let mut units: Vec<String> = Vec::new();
    for t in &list {
        units.extend([t["unit"].as_str(), t["activates"].as_str()].into_iter().flatten().map(str::to_string));
    }
    let mut args = vec!["show", "--no-pager", "-p", "Id,Description,FragmentPath,TimersCalendar,TimersMonotonic,ActiveState,ExecStart,WorkingDirectory"];
    args.extend(units.iter().map(String::as_str));
    let info = show_blocks(&systemctl(&args).await.unwrap_or_default());
    let none = std::collections::HashMap::new();
    let us = |v: &Value| v.as_i64().filter(|n| *n > 0).map(|n| n / 1000);
    let mut out: Vec<Value> = list
        .iter()
        .map(|t| {
            let unit = t["unit"].as_str().unwrap_or("");
            let svc = t["activates"].as_str().unwrap_or("");
            let ti = info.get(unit).unwrap_or(&none);
            let si = info.get(svc).unwrap_or(&none);
            let g = |m: &std::collections::HashMap<String, String>, k: &str| m.get(k).cloned().unwrap_or_default();
            let cmd = timer_command(&g(si, "Description"), &g(si, "ExecStart"));
            let target = agent_target(&cmd);
            json!({
                "unit": unit, "service": svc,
                "name": unit.trim_end_matches(".timer"),
                "desc": g(si, "Description"),
                "command": cmd,
                "cwd": g(si, "WorkingDirectory").trim_start_matches('!'),
                "spec": timer_spec(&g(ti, "TimersCalendar"), &g(ti, "TimersMonotonic")),
                "next": us(&t["next"]), "last": us(&t["last"]),
                "active": g(ti, "ActiveState"),
                "transient": g(ti, "FragmentPath").contains("/transient/"),
                "agentPane": target.as_ref().map(|t| t.0.clone()),
                "agentPrompt": target.map(|t| t.1),
            })
        })
        .collect();
    // agent wake-ups first, then soonest
    out.sort_by_key(|t| (t["agentPane"].is_null(), t["next"].as_i64().unwrap_or(i64::MAX)));
    out
}

#[derive(Deserialize)]
struct ParseBody {
    when: String,
}

async fn api_parse(Json(b): Json<ParseBody>) -> Response {
    match parse_when(&b.when) {
        Ok((cron, desc)) => {
            let mut next = Vec::new();
            let mut t = now_ms();
            for _ in 0..3 {
                if let Some(n) = next_fire(&cron, t) {
                    next.push(n);
                    t = n;
                }
            }
            Json(json!({"cron": cron, "desc": desc, "next": next})).into_response()
        }
        Err(e) => json_err(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Body {
    label: Option<String>,
    cwd: Option<String>,
    prompt: Option<String>,
    when: Option<String>,
    reuse: Option<bool>,
    paused: Option<bool>,
    ws: Option<String>,
}

fn apply(s: &mut Sched, b: Body) -> Result<(), String> {
    if let Some(w) = b.when {
        let (cron, desc) = parse_when(&w)?;
        s.when = w.trim().to_string();
        s.cron = cron;
        s.desc = desc;
    }
    if let Some(c) = b.cwd {
        let c = c.trim().to_string();
        let c = if let Some(rest) = c.strip_prefix("~") { format!("{}{rest}", crate::home().to_string_lossy()) } else { c };
        if !std::path::Path::new(&c).is_dir() {
            return Err(format!("{c} is not a directory"));
        }
        s.cwd = c;
    }
    if let Some(p) = b.prompt {
        let p = p.trim().to_string();
        if p.is_empty() || p.len() > 8000 {
            return Err("prompt must be 1–8000 characters".into());
        }
        s.prompt = p;
    }
    if let Some(l) = b.label {
        s.label = l.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
    }
    if let Some(r) = b.reuse {
        s.reuse = r;
    }
    if let Some(p) = b.paused {
        s.paused = p;
    }
    if let Some(w) = b.ws {
        s.ws = Some(w).filter(|w| !w.is_empty());
    }
    if s.label.is_empty() {
        s.label = s.prompt.split_whitespace().take(4).collect::<Vec<_>>().join(" ");
    }
    s.next = if s.paused { None } else { next_fire(&s.cron, now_ms()) };
    Ok(())
}

async fn api_create(Json(b): Json<Body>) -> Response {
    if b.cwd.is_none() || b.prompt.is_none() || b.when.is_none() {
        return json_err(StatusCode::BAD_REQUEST, "cwd, prompt and when are required");
    }
    let mut s = Sched { id: format!("s{:x}", now_ms()), created: now_ms(), reuse: b.reuse.unwrap_or(true), ..Default::default() };
    if let Err(e) = apply(&mut s, b) {
        return json_err(StatusCode::BAD_REQUEST, e);
    }
    let v = view(&s);
    with(|l| l.push(s));
    WAKE.notify_one();
    Json(json!({"ok": true, "schedule": v})).into_response()
}

async fn api_update(AxPath(id): AxPath<String>, Json(b): Json<Body>) -> Response {
    let r = with(|l| {
        let s = l.iter_mut().find(|s| s.id == id).ok_or((StatusCode::NOT_FOUND, "no such schedule".to_string()))?;
        let mut copy = s.clone();
        apply(&mut copy, b).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        *s = copy;
        Ok::<Value, (StatusCode, String)>(view(s))
    });
    WAKE.notify_one();
    match r {
        Ok(v) => Json(json!({"ok": true, "schedule": v})).into_response(),
        Err((c, e)) => json_err(c, e),
    }
}

async fn api_delete(AxPath(id): AxPath<String>) -> Response {
    let found = with(|l| {
        let n = l.len();
        l.retain(|s| s.id != id);
        n != l.len()
    });
    WAKE.notify_one();
    if found { Json(json!({"ok": true})).into_response() } else { json_err(StatusCode::NOT_FOUND, "no such schedule") }
}

async fn api_run_now(State(app): State<Shared>, AxPath(id): AxPath<String>) -> Response {
    let Some(s) = SCHEDS.lock().unwrap().iter().find(|s| s.id == id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "no such schedule");
    };
    match fire(&app, s).await {
        Ok(pane) => Json(json!({"ok": true, "pane": pane})).into_response(),
        Err(e) => json_err(StatusCode::CONFLICT, e),
    }
}

// ---------------------------------------------------------------------------
// Running

fn agent(app: &App, pane: &str) -> Option<Value> {
    let s = app.inner.lock().unwrap().state.clone();
    let mut v: Value = serde_json::from_str(&s).ok()?;
    let list = v["agents"].as_array_mut()?;
    let i = list.iter().position(|a| a["pane"] == pane)?;
    Some(list.swap_remove(i))
}

fn mark(id: &str, f: impl FnOnce(&mut Sched)) {
    with(|l| {
        if let Some(s) = l.iter_mut().find(|s| s.id == id) {
            f(s);
        }
    });
}

/// Starts one run of `s`. Returns the pane it runs in.
async fn fire(app: &Shared, s: Sched) -> Result<String, String> {
    eprintln!("sched {} ({}) firing", s.id, s.label);
    if !std::path::Path::new(&s.cwd).is_dir() {
        mark(&s.id, |x| x.last_status = Some(format!("failed: {} is gone", s.cwd)));
        return Err(format!("{} is not a directory", s.cwd));
    }
    if s.reuse {
        if let Some(p) = &s.last_pane {
            if let Some(a) = agent(app, p) {
                let st = a["status"].as_str().unwrap_or("");
                if st == "working" || st == "blocked" {
                    mark(&s.id, |x| {
                        x.last_status = Some(format!("skipped: previous run still {st}"));
                        x.last_run = Some(now_ms());
                    });
                    return Err(format!("previous run is still {st}"));
                }
                if a["cwd"].as_str() == Some(s.cwd.as_str())
                    && herdr::quick("agent.prompt", json!({"target": p, "text": s.prompt})).await.is_ok()
                {
                    let pane = p.clone();
                    mark(&s.id, |x| {
                        x.last_run = Some(now_ms());
                        x.runs += 1;
                        x.last_status = Some("sent".into());
                        x.pending = Some(Pending { pane: pane.clone(), started: now_ms(), working: false });
                    });
                    app.refresh.notify_one();
                    WAKE.notify_one();
                    return Ok(pane);
                }
            }
        }
    }
    let ws = s.ws.clone().or_else(|| crate::fleet::pick_ws(app, &s.cwd));
    let label = crate::fleet::tab_label("cron", &s.label);
    let (pane, _, _) = crate::fleet::new_tab(ws.as_deref().filter(|w| *w != "new"), &s.cwd, &label)
        .await
        .map_err(|_| "could not create a tab (is herdr running?)".to_string())?;
    mark(&s.id, |x| {
        x.last_run = Some(now_ms());
        x.runs += 1;
        x.last_pane = Some(pane.clone());
        x.last_status = Some("starting".into());
        x.pending = Some(Pending { pane: pane.clone(), started: now_ms(), working: false });
    });
    let (app2, pane2, id, prompt) = (app.clone(), pane.clone(), s.id.clone(), s.prompt.clone());
    tokio::spawn(async move {
        let ok = crate::fleet::start_and_prompt(&app2, &pane2, &label, vec!["--permission-mode".into(), "auto".into()], Some(prompt)).await;
        mark(&id, |x| {
            if x.pending.as_ref().is_some_and(|p| p.pane == pane2) {
                x.last_status = Some(if ok { "sent".into() } else { "failed: agent did not start".into() });
                if !ok {
                    x.pending = None;
                }
            }
        });
        WAKE.notify_one();
    });
    app.refresh.notify_one();
    WAKE.notify_one();
    Ok(pane)
}

/// Checks pending runs against the live state; pushes when one finished.
async fn check_pending(app: &Shared) {
    let pend: Vec<(String, String, Pending)> = SCHEDS
        .lock()
        .unwrap()
        .iter()
        .filter_map(|s| s.pending.clone().map(|p| (s.id.clone(), s.label.clone(), p)))
        .collect();
    let now = now_ms();
    for (id, label, p) in pend {
        let a = agent(app, &p.pane);
        let status = a.as_ref().and_then(|a| a["status"].as_str()).unwrap_or("").to_string();
        if a.is_none() {
            if now - p.started > 120_000 {
                mark(&id, |x| {
                    x.pending = None;
                    x.last_status = Some("closed".into());
                });
            }
            continue;
        }
        if now - p.started > PENDING_TTL_MS {
            mark(&id, |x| x.pending = None);
            continue;
        }
        if status == "working" || status == "blocked" {
            if !p.working {
                mark(&id, |x| {
                    if let Some(pp) = x.pending.as_mut() {
                        pp.working = true;
                    }
                    x.last_status = Some("running".into());
                });
            }
            continue;
        }
        if p.working && (status == "idle" || status == "done") {
            let a = a.unwrap();
            mark(&id, |x| {
                x.pending = None;
                x.last_status = Some("done".into());
            });
            let body = a["last"]["text"].as_str().map(|t| cut(t, 300).0).unwrap_or_else(|| "Finished.".into());
            let url = match a["answerI"].as_u64() {
                Some(i) => format!("/#/a/{}?i={i}", p.pane),
                None => format!("/#/a/{}", p.pane),
            };
            let push = json!({"title": format!("Scheduled · {label} finished"), "body": body, "url": url, "tag": p.pane});
            eprintln!("sched {id} finished in {}", p.pane);
            crate::send_push(app, &push, false).await;
        }
    }
}

/// The scheduler: one timer to the earliest due schedule.
pub async fn run(app: Shared) {
    let mut events = app.events.subscribe();
    loop {
        let (due, pending) = {
            let l = SCHEDS.lock().unwrap();
            (l.iter().filter(|s| !s.paused).filter_map(|s| s.next).min(), l.iter().any(|s| s.pending.is_some()))
        };
        let wait = due.map(|d| (d - now_ms()).clamp(0, 6 * 3600_000)).unwrap_or(6 * 3600_000) as u64;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
            _ = WAKE.notified() => {}
            ev = events.recv(), if pending => {
                if matches!(ev, Ok(("state", _))) {
                    check_pending(&app).await;
                }
                if let Err(tokio::sync::broadcast::error::RecvError::Closed) = ev {
                    return;
                }
            }
        }
        let now = now_ms();
        let fire_now: Vec<Sched> = SCHEDS.lock().unwrap().iter().filter(|s| !s.paused && s.next.is_some_and(|n| n <= now + 500)).cloned().collect();
        if fire_now.is_empty() {
            continue;
        }
        with(|l| {
            for s in l.iter_mut().filter(|s| fire_now.iter().any(|f| f.id == s.id)) {
                s.next = next_fire(&s.cron, now.max(s.next.unwrap_or(now)) + 1000);
            }
        });
        for s in fire_now {
            if let Err(e) = fire(&app, s.clone()).await {
                eprintln!("sched {}: {e}", s.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain() {
        let p = |s: &str| parse_when(s).map(|x| x.0);
        assert_eq!(p("every weekday 8:00").unwrap(), "0 8 * * 1-5");
        assert_eq!(parse_when("every weekday 8:00").unwrap().1, "Weekdays at 08:00");
        assert_eq!(p("every 2h").unwrap(), "0 */2 * * *");
        assert_eq!(p("every 5 hours").unwrap(), "0 0,5,10,15,20 * * *");
        assert_eq!(p("every 30 minutes").unwrap(), "*/30 * * * *");
        assert_eq!(p("hourly").unwrap(), "0 * * * *");
        assert!(p("every 7 minutes").is_err());
        assert_eq!(p("daily at 9am").unwrap(), "0 9 * * *");
        assert_eq!(p("mon, wed and fri at 7:30pm").unwrap(), "30 19 * * 1,3,5");
        assert_eq!(p("mon-fri 9:15").unwrap(), "15 9 * * 1-5");
        assert_eq!(p("fri-mon 10").unwrap(), "0 10 * * 0,1,5,6");
        assert_eq!(p("weekends at noon").unwrap(), "0 12 * * 0,6");
        assert_eq!(p("every morning").unwrap(), "0 8 * * *");
        assert_eq!(p("every morning at 9").unwrap(), "0 9 * * *");
        assert_eq!(p("tuesdays 10:00").unwrap(), "0 10 * * 2");
        assert_eq!(p("8:00 and 17:30").unwrap(), "0 8 * * *; 30 17 * * *");
        assert_eq!(parse_when("8:00 and 17:30").unwrap().1, "Every day at 08:00 and 17:30");
        assert_eq!(p("every tuesday 12am").unwrap(), "0 0 * * 2");
        assert_eq!(p("0 8 * * 1-5").unwrap(), "0 8 * * 1-5");
        assert!(p("every blursday 8").is_err());
        assert!(p("every weekday").is_err());
        assert!(p("25:00").is_err());
        assert!(p("* * *").is_err());
    }

    #[test]
    fn cron_fields() {
        let c = parse_cron("*/15 9-17 * * 1-5").unwrap();
        assert_eq!(c.min, (1 << 0) | (1 << 15) | (1 << 30) | (1 << 45));
        assert_eq!(c.dow, 0b0111110);
        assert!(c.dom_any && !c.dow_any);
        assert_eq!(parse_cron("0 0 * * 7").unwrap().dow, 1);
        assert!(parse_cron("60 * * * *").is_err());
    }

    #[test]
    fn next_times() {
        // Every next fire is in the future, at a whole minute, and matches the fields.
        let now = now_ms();
        let n = next_fire("0 8 * * 1-5", now).unwrap();
        assert!(n > now && n % 60_000 == 0 && n - now <= 4 * 86400_000);
        let tm = local(n / 1000);
        assert_eq!((tm.tm_hour, tm.tm_min), (8, 0));
        assert!((1..=5).contains(&tm.tm_wday));
        let n2 = next_fire("*/30 * * * *", now).unwrap();
        assert!(n2 - now <= 30 * 60_000);
        assert_eq!(local(n2 / 1000).tm_min % 30, 0);
        // two crons: the earlier wins
        let a = next_fire("0 8 * * *", now).unwrap();
        let b = next_fire("30 17 * * *", now).unwrap();
        assert_eq!(next_fire("0 8 * * *; 30 17 * * *", now).unwrap(), a.min(b));
        // the next run after a fire time is a day later
        assert_eq!(next_fire("0 8 * * *", a).unwrap() - a, 86400_000 + dst_shift(a));
    }

    #[test]
    fn systemd_bits() {
        let cmd = timer_command("[systemd-run] /usr/bin/herdr agent prompt w6:p1V \"CHECK-IN WINDOW OPEN: run phase 3\"", "");
        assert_eq!(agent_target(&cmd).unwrap(), ("w6:p1V".into(), "CHECK-IN WINDOW OPEN: run phase 3".into()));
        assert_eq!(timer_command("Wake", "{ path=/bin/x ; argv[]=/bin/x a b ; ignore_errors=no }"), "/bin/x a b");
        assert!(agent_target("/bin/true").is_none());
        assert_eq!(
            timer_spec("{ OnCalendar=2026-10-08 22:42:00 America/Los_Angeles ; next_elapse=Thu 2026-10-08 22:42:00 PDT }", ""),
            "OnCalendar 2026-10-08 22:42:00 America/Los_Angeles"
        );
        let b = show_blocks("Id=a.timer\nDescription=x=y\n\nId=a.service\nExecStart=z\n");
        assert_eq!(b["a.timer"]["Description"], "x=y");
        assert_eq!(b["a.service"]["ExecStart"], "z");
    }

    fn dst_shift(a: i64) -> i64 {
        let t1 = local(a / 1000).tm_gmtoff;
        let t2 = local(a / 1000 + 86400).tm_gmtoff;
        (t1 - t2) * 1000
    }
}
