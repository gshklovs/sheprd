//! Resource alerts (opt-in pushes) and "Watch this run".
//!
//! Prefs `resAlerts` (in prefs.json) hold a master switch (default off) and one
//! entry per rule. The sampler (res.rs) keeps running at the cheap `watch` tier
//! (every 30 s, 15 s while a watch exists) only while `armed()`: the master
//! switch is on with at least one rule on, or a watch exists. Every tick, from
//! any tier, feeds `evaluate`, which returns the currently active conditions
//! (shown in `/api/res` whether or not pushes are armed) and sends pushes for
//! armed rules: hysteresis (re-arm after 2 min clear), ≤ 1 push per rule per 30 min.

use crate::res::{Group, Snap, fmt_gb};
use crate::transcript::now_ms;
use crate::{Shared, json_err};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

const MB: u64 = 1 << 20;
const GIB: f64 = (1u64 << 30) as f64;
const REARM_MS: i64 = 120_000;
const PUSH_GAP_MS: i64 = 30 * 60_000;
pub const RULES: [&str; 7] = ["gpu-stall", "vram-full", "mem-low", "disk-low", "runaway", "agent-write", "gpu-hot"];

pub fn defaults() -> Value {
    json!({ "on": false, "rules": {
        "gpu-stall": { "on": true, "min": 3 }, "vram-full": { "on": true, "pct": 95 },
        "mem-low": { "on": true, "availMB": 1024 }, "disk-low": { "on": true, "freeGB": 10, "pct": 92 },
        "runaway": { "on": true, "pct": 300, "min": 10 }, "agent-write": { "on": true, "gb": 5, "min": 10 },
        "gpu-hot": { "on": false, "c": 85 } } })
}

/// Fills missing rules/fields with defaults and drops unknown keys.
pub fn normalize(v: &Value) -> Value {
    let mut out = defaults();
    out["on"] = json!(v["on"].as_bool().unwrap_or(false));
    for r in RULES {
        let d = out["rules"][r].clone();
        let given = &v["rules"][r];
        let mut rule = d.clone();
        for (k, dv) in d.as_object().unwrap() {
            let g = &given[k];
            let ok = if dv.is_boolean() { g.is_boolean() } else { g.as_f64().is_some_and(|x| x.is_finite() && x >= 0.0) };
            if ok {
                rule[k] = g.clone();
            }
        }
        out["rules"][r] = rule;
    }
    out
}

// ---------------------------------------------------------------------------
// The engine (pure)

#[derive(Clone, Debug, Default)]
pub struct RuleState {
    cond_since: Option<i64>,
    clear_since: Option<i64>,
    fired: bool,
}

/// A condition that holds right now. `key` is the rule plus its subject.
#[derive(Clone, Debug, PartialEq)]
pub struct Cond {
    pub key: String,
    pub rule: &'static str,
    pub need_ms: i64,
    pub title: String,
    pub text: String,
    pub pane: Option<String>,
    pub pid: Option<u32>,
    pub url: String,
}

/// Steps every known state with this tick's conditions. Returns the active
/// conditions (held for `need_ms`) with their `since`, and those that fire now.
pub fn step(states: &mut HashMap<String, RuleState>, conds: Vec<Cond>, now: i64) -> (Vec<(Cond, i64)>, Vec<Cond>) {
    let mut active = Vec::new();
    let mut fire = Vec::new();
    let mut present = std::collections::HashSet::new();
    for c in conds {
        present.insert(c.key.clone());
        let st = states.entry(c.key.clone()).or_default();
        st.clear_since = None;
        let since = *st.cond_since.get_or_insert(now);
        if now - since >= c.need_ms {
            if !st.fired {
                st.fired = true;
                fire.push(c.clone());
            }
            active.push((c, since));
        }
    }
    states.retain(|k, st| {
        if present.contains(k) {
            return true;
        }
        st.cond_since = None;
        let clear = *st.clear_since.get_or_insert(now);
        if st.fired && now - clear < REARM_MS {
            return true;
        }
        false // re-armed: forget it
    });
    (active, fire)
}

/// Rate limit: ≤ 1 push per rule (per watch) per 30 min.
pub fn may_push(last: &mut HashMap<String, i64>, key: &str, now: i64) -> bool {
    if last.get(key).is_some_and(|t| now - t < PUSH_GAP_MS) {
        return false;
    }
    last.insert(key.to_string(), now);
    true
}

fn num(cfg: &Value, rule: &str, k: &str) -> f64 {
    cfg["rules"][rule][k].as_f64().or_else(|| defaults()["rules"][rule][k].as_f64()).unwrap_or(0.0)
}

fn desktopish(t: Option<&str>) -> bool {
    matches!(t, Some("desktop" | "browser" | "game"))
}

/// The rule conditions that hold in this sample (UX §Q5).
pub fn conditions(snap: &Snap, cfg: &Value, swap: &VecDeque<(i64, u64)>) -> Vec<Cond> {
    let mut out = Vec::new();
    let title_of = |pane: &str| snap.title(pane).unwrap_or("agent").to_string();
    let mk = |key: String, rule: &'static str, need: i64, title: &str, text: String, pane: Option<String>, pid: Option<u32>, url: String| Cond {
        key,
        rule,
        need_ms: need,
        title: title.into(),
        text,
        pane,
        pid,
        url,
    };
    if let Some(g) = snap.gpus.first() {
        let util = g.util.unwrap_or(0.0);
        // gpu-stall: a non-desktop holder of ≥ 512 MB with the GPU idle
        let min = num(cfg, "gpu-stall", "min");
        for p in snap.procs.iter().filter(|p| p.gpu_mem >= 512 * MB && !(p.group == Group::Other && desktopish(p.tag()))) {
            if util < 5.0 {
                out.push(mk(
                    format!("gpu-stall:{}", p.key()),
                    "gpu-stall",
                    (min * 60_000.0) as i64,
                    "GPU stalled",
                    format!("{} holds {} VRAM but the GPU is idle ({min:.0} min)", p.label(), fmt_gb(p.gpu_mem)),
                    p.group.pane().map(str::to_string),
                    Some(p.pid),
                    format!("/#/r?pid={}", p.pid),
                ));
            }
        }
        let pct = num(cfg, "vram-full", "pct");
        if g.mem_total > 0 && g.mem_used as f64 * 100.0 / g.mem_total as f64 >= pct {
            let top = snap.procs.iter().filter(|p| p.gpu_mem > 0).max_by_key(|p| p.gpu_mem);
            let who = top.map(|p| format!(" — {} may OOM", p.label())).unwrap_or_default();
            out.push(mk(
                "vram-full".into(),
                "vram-full",
                60_000,
                "VRAM almost full",
                format!("VRAM {:.1} of {:.1} GB{who}", g.mem_used as f64 / GIB, g.mem_total as f64 / GIB),
                top.and_then(|p| p.group.pane().map(str::to_string)),
                top.map(|p| p.pid),
                "/#/r?f=gpu".into(),
            ));
        }
        let c = num(cfg, "gpu-hot", "c");
        if g.temp.is_some_and(|t| t >= c) {
            out.push(mk(
                "gpu-hot".into(),
                "gpu-hot",
                120_000,
                "GPU hot",
                format!("GPU {:.0}°C for 2 min", g.temp.unwrap_or(0.0)),
                None,
                None,
                "/#/r?f=gpu".into(),
            ));
        }
    }
    let avail_mb = num(cfg, "mem-low", "availMB");
    if snap.mem_total > 0 && (snap.mem_avail as f64) < avail_mb * MB as f64 {
        out.push(mk(
            "mem-low".into(),
            "mem-low",
            30_000,
            "RAM low",
            format!("RAM low — {} available", fmt_gb(snap.mem_avail)),
            None,
            None,
            "/#/r?f=mem".into(),
        ));
    } else if let Some(min) = swap.iter().filter(|(t, _)| snap.t - t <= 300_000).map(|(_, s)| *s).min() {
        let grew = snap.swap_used.saturating_sub(min);
        if grew > 1 << 30 {
            out.push(mk(
                "mem-low".into(),
                "mem-low",
                0,
                "RAM low",
                format!("Swap grew {} in 5 min", fmt_gb(grew)),
                None,
                None,
                "/#/r?f=mem".into(),
            ));
        }
    }
    let (free_gb, pct) = (num(cfg, "disk-low", "freeGB"), num(cfg, "disk-low", "pct"));
    for v in &snap.volumes {
        if v.total == 0 {
            continue;
        }
        let used_pct = v.used as f64 * 100.0 / v.total as f64;
        let hit = if v.ram { used_pct >= 60.0 } else { (v.avail as f64) < free_gb * GIB || used_pct >= pct };
        if hit {
            let text = if v.ram {
                format!("{} (RAM) {used_pct:.0}% full", v.mount)
            } else {
                format!("{}: {} free ({used_pct:.0}% used)", v.mount, fmt_gb(v.avail))
            };
            out.push(mk(format!("disk-low:{}", v.mount), "disk-low", 0, "Disk low", text, None, None, "/#/r/disk".into()));
        }
    }
    let (rpct, rmin) = (num(cfg, "runaway", "pct"), num(cfg, "runaway", "min"));
    let (wgb, wmin) = (num(cfg, "agent-write", "gb"), num(cfg, "agent-write", "min"));
    for r in &snap.rolls {
        let cpu = r.cpu - r.job_cpu;
        if cpu >= rpct {
            let top = snap
                .procs
                .iter()
                .filter(|p| p.group == Group::Agent(r.pane.clone()) && p.job.is_none())
                .max_by(|a, b| a.cpu.total_cmp(&b.cpu));
            out.push(mk(
                format!("runaway:{}", r.pane),
                "runaway",
                (rmin * 60_000.0) as i64,
                "Runaway CPU",
                format!(
                    "{} ({}) at {cpu:.0}% CPU for {rmin:.0} min{}",
                    title_of(&r.pane),
                    r.pane,
                    top.map(|p| format!(" — {}", p.label())).unwrap_or_default()
                ),
                Some(r.pane.clone()),
                top.map(|p| p.pid),
                format!("/#/r?pane={}", r.pane),
            ));
        }
        let wrote = snap.wrote_in(&r.pane, (wmin * 60_000.0) as i64);
        if wrote as f64 >= wgb * GIB {
            out.push(mk(
                format!("agent-write:{}", r.pane),
                "agent-write",
                0,
                "Heavy writes",
                format!("{} ({}) wrote {} in {wmin:.0} min", title_of(&r.pane), r.pane, fmt_gb(wrote)),
                Some(r.pane.clone()),
                None,
                format!("/#/r?pane={}", r.pane),
            ));
        }
    }
    out
}

fn alert_json(c: &Cond, since: i64) -> Value {
    json!({"rule": c.rule, "since": since, "text": c.text, "pane": c.pane, "pid": c.pid, "url": c.url})
}

// ---------------------------------------------------------------------------
// Watches

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Watch {
    pub key: String,
    pub pid: u32,
    pub start: i64,
    pub label: String,
    pub pane: Option<String>,
    pub since: i64,
    #[serde(default)]
    pub gpu_mem: u64,
}

fn dur(ms: i64) -> String {
    let m = ms / 60_000;
    if m >= 60 {
        format!("{}h {}m", m / 60, m % 60)
    } else if m >= 1 {
        format!("{m}m")
    } else {
        format!("{}s", (ms / 1000).max(1))
    }
}

/// Watch events this tick: (exited watches, stall conditions).
pub fn watch_events(snap: &Snap, watches: &mut [Watch]) -> (Vec<Cond>, Vec<Cond>) {
    let mut exits = Vec::new();
    let mut stalls = Vec::new();
    let util = snap.gpus.first().and_then(|g| g.util);
    for w in watches.iter_mut() {
        match snap.procs.iter().find(|p| p.key() == w.key) {
            None => {
                let freed = if w.gpu_mem > 0 { format!(" — released {} VRAM", fmt_gb(w.gpu_mem)) } else { String::new() };
                exits.push(Cond {
                    key: format!("watch-{}", w.pid),
                    rule: "watch-exit",
                    need_ms: 0,
                    title: "Run ended".into(),
                    text: format!("{} ended after {}{freed}", w.label, dur(snap.t - w.start)),
                    pane: w.pane.clone(),
                    pid: Some(w.pid),
                    url: format!("/#/r?pid={}", w.pid),
                });
            }
            Some(p) => {
                // the watched process plus its descendants
                let mut mem = p.gpu_mem;
                let mut stack = vec![p.pid];
                let mut n = 0;
                while let Some(pid) = stack.pop() {
                    n += 1;
                    if n > 512 {
                        break;
                    }
                    for c in snap.procs.iter().filter(|c| c.ppid == pid && c.start >= p.start) {
                        mem += c.gpu_mem;
                        stack.push(c.pid);
                    }
                }
                w.gpu_mem = mem;
                if mem >= 512 * MB && util.is_some_and(|u| u < 5.0) {
                    stalls.push(Cond {
                        key: format!("watch-stall:{}", w.key),
                        rule: "watch-stall",
                        need_ms: 180_000,
                        title: "Run stalled".into(),
                        text: format!("{}: GPU idle for 3 min (holds {})", w.label, fmt_gb(mem)),
                        pane: w.pane.clone(),
                        pid: Some(w.pid),
                        url: format!("/#/r?pid={}", w.pid),
                    });
                }
            }
        }
    }
    (exits, stalls)
}

// ---------------------------------------------------------------------------
// Global state

#[derive(Default)]
struct AState {
    cfg: Value,
    states: HashMap<String, RuleState>,
    last_push: HashMap<String, i64>,
    swap: VecDeque<(i64, u64)>,
    watches: Vec<Watch>,
    path: Option<PathBuf>,
}

static A: LazyLock<Mutex<AState>> = LazyLock::new(|| Mutex::new(AState { cfg: defaults(), ..Default::default() }));

fn save_watches(st: &AState) {
    if let Some(p) = &st.path {
        let _ = std::fs::write(p, serde_json::to_vec(&st.watches).unwrap_or_default());
    }
}

/// Startup: loads `resAlerts` and `<state>/watches.json`; starts the watcher if armed.
pub fn init(app: &Shared, state_dir: &std::path::Path) {
    let cfg = normalize(&app.prefs.lock().unwrap().v["resAlerts"]);
    let path = state_dir.join("watches.json");
    let watches: Vec<Watch> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    {
        let mut a = A.lock().unwrap();
        a.cfg = cfg;
        a.watches = watches;
        a.path = Some(path);
    }
    if armed() {
        crate::res::ensure_running(app);
    }
}

/// `POST /api/prefs {resAlerts}`: replaces the config (missing rules → defaults).
pub fn set_prefs(app: &Shared, v: &Value) -> Value {
    let cfg = normalize(v);
    app.prefs.lock().unwrap().set_key("resAlerts", cfg.clone());
    A.lock().unwrap().cfg = cfg.clone();
    if armed() {
        crate::res::ensure_running(app);
    }
    cfg
}

fn rule_on(cfg: &Value, rule: &str) -> bool {
    cfg["on"].as_bool().unwrap_or(false) && cfg["rules"][rule]["on"].as_bool().unwrap_or(false)
}

pub fn armed() -> bool {
    let a = A.lock().unwrap();
    !a.watches.is_empty() || RULES.iter().any(|r| rule_on(&a.cfg, r))
}

pub fn interval_ms() -> u64 {
    if A.lock().unwrap().watches.is_empty() { 30_000 } else { 15_000 }
}

/// Whether a watch-tier tick should run nvidia-smi.
pub fn wants_gpu(last: Option<&crate::gpu::Sample>, since_ms: i64) -> bool {
    let a = A.lock().unwrap();
    let gpu_rules = !a.watches.is_empty() || ["gpu-stall", "vram-full", "gpu-hot"].iter().any(|r| rule_on(&a.cfg, r));
    let busy = |l: &crate::gpu::Sample| !l.apps.is_empty() || l.pmon.iter().flatten().any(|r| r.kind.contains('C'));
    (gpu_rules && last.is_some_and(busy)) || since_ms >= 300_000
}

pub fn is_watched(key: &str) -> bool {
    A.lock().unwrap().watches.iter().any(|w| w.key == key)
}

pub fn status() -> Value {
    let a = A.lock().unwrap();
    let rules: Vec<&str> = RULES.iter().copied().filter(|r| rule_on(&a.cfg, r)).collect();
    let armed = !a.watches.is_empty() || !rules.is_empty();
    json!({"armed": armed, "interval": if a.watches.is_empty() { 30_000 } else { 15_000 }, "rules": rules, "watches": a.watches.len()})
}

/// One tick: steps every rule and watch, pushes what fires and is armed, and
/// returns the active conditions as `Alert`s.
pub fn evaluate(app: &Shared, snap: &Snap) -> Vec<Value> {
    let now = snap.t;
    let mut pushes: Vec<Value> = Vec::new();
    let active = {
        let mut guard = A.lock().unwrap();
        let a = &mut *guard;
        a.swap.push_back((now, snap.swap_used));
        while a.swap.front().is_some_and(|(t, _)| now - t > 300_000) {
            a.swap.pop_front();
        }
        let mut conds = conditions(snap, &a.cfg, &a.swap);
        let (exits, stalls) = watch_events(snap, &mut a.watches);
        conds.extend(stalls);
        let (active, fire) = step(&mut a.states, conds, now);
        for c in fire {
            let watch = c.rule == "watch-stall";
            if (watch || rule_on(&a.cfg, c.rule)) && may_push(&mut a.last_push, &if watch { c.key.clone() } else { c.rule.to_string() }, now) {
                let tag = if watch { format!("res-watch-{}", c.pid.unwrap_or(0)) } else { format!("res-{}", c.rule) };
                pushes.push(json!({"title": c.title, "body": c.text, "url": c.url, "tag": tag}));
            }
        }
        if !exits.is_empty() {
            for c in &exits {
                pushes.push(json!({"title": c.title, "body": c.text, "url": c.url, "tag": format!("res-watch-{}", c.pid.unwrap_or(0))}));
            }
            let gone: Vec<u32> = exits.iter().filter_map(|c| c.pid).collect();
            a.watches.retain(|w| !gone.contains(&w.pid) || snap.procs.iter().any(|p| p.key() == w.key));
            save_watches(a);
        }
        let mut out: Vec<Value> = active.iter().map(|(c, since)| alert_json(c, *since)).collect();
        out.extend(exits.iter().map(|c| alert_json(c, now)));
        out
    };
    for p in pushes {
        let app = app.clone();
        tokio::spawn(async move { crate::send_push(&app, &p, false).await });
    }
    active
}

/// For a one-off sample (the brief): conditions without durations, no pushes.
pub fn evaluate_passive(snap: &Snap) -> Vec<Value> {
    let a = A.lock().unwrap();
    conditions(snap, &a.cfg, &VecDeque::new()).iter().filter(|c| c.need_ms <= 60_000).map(|c| alert_json(c, snap.t)).collect()
}

// ---------------------------------------------------------------------------
// HTTP

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/res/watch", get(api_watches).post(api_watch_add))
        .route("/api/res/watch/{key}", delete(api_watch_del))
}

async fn api_watches() -> Response {
    let a = A.lock().unwrap();
    Json(json!({"watches": a.watches.iter().map(|w| json!({"key": w.key, "pid": w.pid, "label": w.label, "pane": w.pane, "since": w.since})).collect::<Vec<_>>()}))
        .into_response()
}

async fn api_watch_add(State(app): State<Shared>, Json(b): Json<Value>) -> Response {
    let Some(pid) = b["pid"].as_u64().map(|p| p as u32) else { return json_err(StatusCode::BAD_REQUEST, "pid required") };
    let Some((start, label)) = crate::res::identify(pid) else { return json_err(StatusCode::NOT_FOUND, "no_such_process") };
    let key = format!("{pid}@{start}");
    let snap = crate::res::current();
    let p = snap.as_ref().and_then(|s| s.procs.iter().find(|p| p.key() == key).cloned());
    {
        let mut a = A.lock().unwrap();
        if !a.watches.iter().any(|w| w.key == key) {
            a.watches.push(Watch {
                key: key.clone(),
                pid,
                start,
                label: p.as_ref().map(|p| p.label()).unwrap_or(label),
                pane: p.as_ref().and_then(|p| p.group.pane().map(str::to_string)),
                since: now_ms(),
                gpu_mem: p.as_ref().map(|p| p.gpu_mem).unwrap_or(0),
            });
            save_watches(&a);
        }
    }
    crate::res::ensure_running(&app);
    Json(json!({"ok": true, "key": key})).into_response()
}

async fn api_watch_del(AxPath(key): AxPath<String>) -> Response {
    let mut a = A.lock().unwrap();
    a.watches.retain(|w| w.key != key);
    save_watches(&a);
    Json(json!({"ok": true})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::Volume;
    use crate::res::{P, Roll};

    fn cond(key: &str, need: i64) -> Cond {
        Cond {
            key: key.into(),
            rule: "vram-full",
            need_ms: need,
            title: String::new(),
            text: String::new(),
            pane: None,
            pid: None,
            url: String::new(),
        }
    }

    #[test]
    fn fires_once_then_rearms_after_clear() {
        let mut st = HashMap::new();
        let mut last = HashMap::new();
        let mut pushed = 0;
        let mut run = |st: &mut HashMap<String, RuleState>, on: bool, t: i64| {
            let (active, fire) = step(st, if on { vec![cond("vram-full", 60_000)] } else { vec![] }, t);
            for c in fire {
                if may_push(&mut last, c.rule, t) {
                    pushed += 1;
                }
            }
            active.len()
        };
        assert_eq!(run(&mut st, true, 0), 0); // needs 60 s
        assert_eq!(run(&mut st, true, 30_000), 0);
        assert_eq!(run(&mut st, true, 60_000), 1);
        assert_eq!(run(&mut st, true, 90_000), 1);
        // short clear (< 2 min) then back: no new fire
        run(&mut st, false, 100_000);
        run(&mut st, true, 150_000);
        run(&mut st, true, 260_000);
        // clear 2+ min: re-arms; fires again but the 30 min push limit holds
        run(&mut st, false, 300_000);
        run(&mut st, false, 430_000);
        run(&mut st, true, 440_000);
        run(&mut st, true, 500_000);
        // after 30 min it may push again
        run(&mut st, false, 600_000);
        run(&mut st, false, 800_000);
        run(&mut st, true, 2_000_000);
        run(&mut st, true, 2_070_000);
        drop(run);
        assert_eq!(pushed, 2);
    }

    fn gp(pid: u32, gpu_mb: u64, comm: &str, group: Group) -> P {
        P { pid, ppid: 1, start: 1000, comm: comm.into(), cmd: Some(comm.into()), gpu_mem: gpu_mb * MB, group, ..Default::default() }
    }

    fn snap(procs: Vec<P>, util: f64) -> Snap {
        Snap {
            t: 10_000_000,
            procs,
            gpus: vec![crate::gpu::Gpu { util: Some(util), mem_used: 1 << 30, mem_total: 8 << 30, ..Default::default() }],
            mem_total: 16 << 30,
            mem_avail: 8 << 30,
            ..Default::default()
        }
    }

    #[test]
    fn gpu_stall_ignores_desktop() {
        let cfg = defaults();
        let s = snap(vec![gp(10, 900, "Hyprland", Group::Other), gp(11, 600, "chromium", Group::Other)], 0.0);
        assert!(conditions(&s, &cfg, &VecDeque::new()).is_empty());
        let s = snap(vec![gp(12, 6000, "python3", Group::Agent("w6:p3".into()))], 0.0);
        let c = conditions(&s, &cfg, &VecDeque::new());
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].rule, "gpu-stall");
        assert_eq!(c[0].need_ms, 180_000);
        assert_eq!(c[0].url, "/#/r?pid=12");
        // busy GPU: no stall
        let s = snap(vec![gp(12, 6000, "python3", Group::Agent("w6:p3".into()))], 90.0);
        assert!(conditions(&s, &cfg, &VecDeque::new()).is_empty());
    }

    #[test]
    fn disk_mem_runaway_rules() {
        let cfg = defaults();
        let mut s = snap(vec![], 50.0);
        s.volumes = vec![
            Volume { mount: "/home".into(), fs: "btrfs".into(), used: 230 << 30, total: 237 << 30, avail: 7 << 30, ram: false },
            Volume { mount: "/tmp".into(), fs: "tmpfs".into(), used: 1 << 30, total: 8 << 30, avail: 7 << 30, ram: true },
        ];
        s.mem_avail = 512 << 20;
        s.rolls = vec![Roll { pane: "w6:p3".into(), cpu: 390.0, job_cpu: 0.0, ..Default::default() }, Roll {
            pane: "w6:p4".into(),
            cpu: 390.0,
            job_cpu: 380.0,
            ..Default::default()
        }];
        let c = conditions(&s, &cfg, &VecDeque::new());
        let keys: Vec<&str> = c.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["mem-low", "disk-low:/home", "runaway:w6:p3"]);
        assert_eq!(c[1].text, "/home: 7.0 GB free (97% used)");
    }

    #[test]
    fn watch_exit_and_stall() {
        let mut w = vec![Watch {
            key: "12@1000".into(),
            pid: 12,
            start: 1000,
            label: "train.py".into(),
            pane: Some("w6:p3".into()),
            since: 0,
            gpu_mem: 0,
        }];
        let s = snap(vec![gp(12, 6144, "python3", Group::Agent("w6:p3".into()))], 1.0);
        let (exits, stalls) = watch_events(&s, &mut w);
        assert!(exits.is_empty());
        assert_eq!(stalls.len(), 1);
        assert_eq!(w[0].gpu_mem, 6144 * MB);
        let mut s2 = snap(vec![], 0.0);
        s2.t = 1000 + 2 * 3_600_000 + 14 * 60_000;
        let (exits, _) = watch_events(&s2, &mut w);
        assert_eq!(exits[0].text, "train.py ended after 2h 14m — released 6.0 GB VRAM");
        assert_eq!(exits[0].url, "/#/r?pid=12");
    }

    #[test]
    fn normalize_fills_defaults() {
        let v = normalize(&json!({"on": true, "rules": {"runaway": {"min": 20, "bogus": 1}, "gpu-hot": {"on": "yes"}}}));
        assert_eq!(v["on"], true);
        assert_eq!(v["rules"]["runaway"]["min"], 20);
        assert_eq!(v["rules"]["runaway"]["pct"], 300);
        assert!(v["rules"]["runaway"].get("bogus").is_none());
        assert_eq!(v["rules"]["gpu-hot"]["on"], false);
        assert_eq!(normalize(&Value::Null)["on"], false);
    }
}
