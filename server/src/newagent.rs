//! One-tap new agents and Haiku auto-naming (API.md "One-tap new agent").
//!
//! Boot queue: a pane started by `/api/new` is "booting" until Claude first reports idle. Prompts sent to it
//! meanwhile (the phone's first message, typed while Claude starts) wait here and go in, in order, once it is
//! ready, instead of being typed into a starting TUI or its trust dialog.
//!
//! Auto-name: an agent with no custom name gets a short title and a `/color` from Haiku after its first prompt,
//! applied through the same pending-meta path as the phone's rename (typed in as `/rename` + `/color` once idle).
//! One Haiku call per session; never the Master or services; only sessions started after this shipped,
//! plus older ones still showing a fallback title (no Claude title at all).

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use crate::transcript::now_ms;

/// The model a new agent starts on (exact id; `claude --model claude-opus-5-5` resolves to it, as does `opus`).
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";

/// Default spawn args for `/api/new` when the caller gives none.
pub fn default_args() -> Vec<String> {
    ["--permission-mode", "auto", "--model", DEFAULT_MODEL].iter().map(|s| s.to_string()).collect()
}

static BOOT: Mutex<Option<HashMap<String, Vec<String>>>> = Mutex::new(None);
/// Panes started with an explicit label (the sheet's Label field): the user named them, Haiku leaves them alone.
static LABELLED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

pub fn labelled(pane: &str) {
    LABELLED.lock().unwrap().get_or_insert_with(HashSet::new).insert(pane.to_string());
}

/// Marks `pane` as booting, with an optional first prompt queued.
pub fn boot_begin(pane: &str, first: Option<String>) {
    BOOT.lock().unwrap().get_or_insert_with(HashMap::new).insert(pane.to_string(), first.into_iter().collect());
}

/// Queues `text` if `pane` is still booting (true = queued, the caller must not send it).
pub fn boot_queue(pane: &str, text: &str) -> bool {
    let mut b = BOOT.lock().unwrap();
    match b.as_mut().and_then(|m| m.get_mut(pane)) {
        Some(q) => {
            q.push(text.to_string());
            true
        }
        None => false,
    }
}

/// Ends the boot: returns the queued prompts (oldest first).
pub fn boot_take(pane: &str) -> Vec<String> {
    BOOT.lock().unwrap().as_mut().and_then(|m| m.remove(pane)).unwrap_or_default()
}

pub fn booting(pane: &str) -> bool {
    BOOT.lock().unwrap().as_ref().is_some_and(|m| m.contains_key(pane))
}

/// Waits (up to 15 min) until the agent in `pane` is ready for input, then sends what was queued during boot.
pub async fn finish_boot(app: &crate::Shared, pane: &str) {
    let deadline = now_ms() + 15 * 60 * 1000;
    while now_ms() < deadline {
        if let Ok(a) = crate::herdr::quick("agent.get", json!({"target": pane})).await {
            let st = a["agent"]["agent_status"].as_str().unwrap_or("");
            if st == "idle" || st == "done" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let queued = boot_take(pane);
    for (n, text) in queued.iter().enumerate() {
        if n > 0 {
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
        let lock = app.bridge.pane_lock(pane);
        let _turn = lock.lock().await;
        if let Err(e) = crate::herdr::quick("agent.prompt", json!({"target": pane, "text": text})).await {
            eprintln!("boot prompt {pane}: {e}");
        }
    }
    app.refresh.notify_one();
}

// ---------- auto-name ----------

/// What refresh knows about one agent, for auto-naming.
pub struct Cand {
    pub pane: String,
    pub sid: String,
    /// The user named it: custom-title / agent-name in the transcript, or a pending phone rename.
    pub named: bool,
    /// Claude has no title for it either (the row shows a fallback title).
    pub fallback: bool,
    pub has_color: bool,
    pub first_text: Option<String>,
    pub started: i64,
}

struct Auto {
    since: i64,
    done: Vec<String>,
    inflight: HashSet<String>,
    /// pane -> when it was auto-named (the phone animates the new name)
    named_at: HashMap<String, i64>,
}

static AUTO: Mutex<Option<Auto>> = Mutex::new(None);

fn store_path() -> std::path::PathBuf {
    crate::state_dir().join("autoname.json")
}

fn with_auto<R>(f: impl FnOnce(&mut Auto) -> R) -> R {
    let mut g = AUTO.lock().unwrap();
    let a = g.get_or_insert_with(|| {
        let v: Value = std::fs::read(store_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        let since = v["since"].as_i64().unwrap_or_else(now_ms);
        let done = v["done"].as_array().map(|l| l.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default();
        let a = Auto { since, done, inflight: HashSet::new(), named_at: HashMap::new() };
        save(&a);
        a
    });
    f(a)
}

fn save(a: &Auto) {
    let _ = std::fs::write(store_path(), json!({"since": a.since, "done": a.done}).to_string());
}

/// Is this agent due for a Haiku name now?
fn due(c: &Cand, since: i64) -> bool {
    !c.named && !c.sid.is_empty() && c.first_text.as_deref().is_some_and(|t| !t.trim().is_empty()) && (c.started >= since || c.fallback)
}

/// Called from refresh after all decorations: marks recently auto-named agents (`autoNamed`) and starts a
/// Haiku call for each agent that is due. `agents` is the state list (home/service flags already set).
pub fn decorate(app: &crate::Shared, agents: &mut [Value], cands: Vec<Cand>) {
    let now = now_ms();
    let mut start = Vec::new();
    with_auto(|au| {
        au.named_at.retain(|_, t| now - *t < 120_000);
        let mut lab = LABELLED.lock().unwrap();
        let lab = lab.get_or_insert_with(HashSet::new);
        lab.retain(|p| booting(p) || agents.iter().any(|a| a["pane"] == p.as_str()));
        let by: HashMap<&str, &Cand> = cands.iter().map(|c| (c.pane.as_str(), c)).collect();
        for a in agents.iter_mut() {
            let pane = a["pane"].as_str().unwrap_or("").to_string();
            let pane = pane.as_str();
            if let Some(t) = au.named_at.get(pane) {
                a["autoNamed"] = json!(t);
            }
            let Some(c) = by.get(pane) else { continue };
            let skip = a["home"] == true
                || !a["service"].is_null()
                || a["kind"] != "claude"
                || lab.contains(pane)
                || booting(pane);
            if skip || !due(c, au.since) || au.done.contains(&c.sid) || au.inflight.contains(&c.sid) {
                continue;
            }
            au.inflight.insert(c.sid.clone());
            start.push((c.pane.clone(), c.sid.clone(), c.first_text.clone().unwrap_or_default(), c.has_color, a["cwd"].as_str().unwrap_or("").to_string()));
        }
    });
    for (pane, sid, text, has_color, cwd) in start {
        let app = app.clone();
        tokio::spawn(async move {
            let r = haiku_name(&text, &cwd).await;
            with_auto(|au| {
                au.inflight.remove(&sid);
                au.done.push(sid.clone());
                if au.done.len() > 400 {
                    au.done.drain(..100);
                }
                save(au);
            });
            match r {
                Ok((title, color)) => apply(&app, &pane, title, if has_color { None } else { Some(color) }).await,
                Err(e) => eprintln!("autoname {pane}: {e}"),
            }
        });
    }
}

/// Same path as the phone's rename/recolor (`/api/a/{pane}/meta`): pending meta typed in once idle,
/// shown at once, tab label follows.
async fn apply(app: &crate::Shared, pane: &str, title: String, color: Option<String>) {
    let (tab, color) = {
        let mut i = app.inner.lock().unwrap();
        if i.meta.contains_key(pane) || !i.status.contains_key(pane) {
            return; // a phone rename got there first, or the agent is gone
        }
        // spread the palette: colours already on screen (and ones still being typed in) count against Haiku's pick
        let color = color.map(|c| {
            let cur: Value = serde_json::from_str(&i.state).unwrap_or(Value::Null);
            let mut used: Vec<String> = cur["agents"]
                .as_array()
                .map(|l| {
                    l.iter()
                        .filter(|a| a["pane"] != pane && a["home"] != true && a["service"].is_null())
                        .filter_map(|a| a["color"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            used.extend(i.meta.values().filter_map(|m| m.color.clone()));
            spread(&c, &used)
        });
        i.meta.insert(pane.to_string(), crate::MetaReq { title: Some(title.clone()), color: color.clone(), applied: 0 });
        (i.pane_tabs.get(pane).cloned(), color)
    };
    let now = now_ms();
    with_auto(|au| {
        au.named_at.insert(pane.to_string(), now);
    });
    crate::patch_agent(app, pane, |a| {
        a["title"] = json!(title);
        if let Some(c) = &color {
            a["color"] = json!(c);
        }
        a["autoNamed"] = json!(now);
    });
    if let Some(tab) = tab {
        let _ = crate::herdr::quick("tab.rename", json!({"tab_id": tab, "label": title})).await;
    }
    app.refresh.notify_one();
}

/// The 7 pastels the phone shows (web/style.css "agent colors"), as Claude `/color` names. Claude also takes "red";
/// it is left out so an agent never looks like an error (a "red" reply becomes pink, the nearest pastel).
pub const PICK: [&str; 7] = ["orange", "yellow", "green", "cyan", "blue", "purple", "pink"];

/// Haiku's pick unless another colour is used less: then the next least-used one after it in palette order, so new
/// agents spread over the 7 and neighbours differ. `used` = colours of the other live agents ("red" counts as pink).
pub fn spread(pick: &str, used: &[String]) -> String {
    let n = |c: &str| used.iter().filter(|u| u.as_str() == c || (c == "pink" && u.as_str() == "red")).count();
    let at = PICK.iter().position(|c| *c == pick).unwrap_or(4);
    let min = PICK.iter().map(|c| n(c)).min().unwrap_or(0);
    (0..PICK.len()).map(|k| PICK[(at + k) % PICK.len()]).find(|c| n(c) == min).unwrap_or(PICK[at]).to_string()
}

fn prompt_for(first: &str, cwd: &str) -> String {
    let first: String = first.chars().take(600).collect();
    let folder = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    format!(
        "Name a coding-agent chat from its first message. Reply with JSON only, no prose: \
         {{\"title\": \"<2-5 word title, Title case, no quotes or emoji>\", \"color\": \"<one of {}>\"}}. \
         Pick the color that fits the topic.\nFolder: {folder}\nFirst message:\n<<<\n{first}\n>>>",
        PICK.join(", ")
    )
}

/// Pulls {"title","color"} out of Haiku's reply and checks it.
pub fn parse_reply(s: &str) -> Option<(String, String)> {
    let a = s.find('{')?;
    let b = s.rfind('}')?;
    let v: Value = serde_json::from_str(s.get(a..=b)?).ok()?;
    let title = v["title"].as_str()?.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = title.trim_matches(|c: char| c == '"' || c == '\'' || c == '.').to_string();
    let words = title.split_whitespace().count();
    if !(1..=6).contains(&words) || title.chars().count() > 48 {
        return None;
    }
    let color = v["color"].as_str().map(str::to_lowercase).map(|c| if c == "red" { "pink".into() } else { c });
    let color = color.filter(|c| PICK.contains(&c.as_str())).unwrap_or_else(|| "blue".into());
    Some((title, color))
}

/// One headless Haiku call: no tools, no settings/hooks/MCP, no session file, in its own cgroup (the server's
/// MemoryMax is far below a claude process), thinking off (it would cost 10-50x the reply). `system` replaces Claude Code's system prompt (a few hundred
/// tokens instead of several thousand). Returns claude's `--output-format json` object (result, usage).
pub async fn run_haiku(system: Option<&str>, prompt: &str, timeout: Duration) -> Result<Value, String> {
    let bin = crate::cloud::claude_bin().ok_or("claude is not installed")?;
    let dir = crate::state_dir().join("autoname-run");
    let _ = std::fs::create_dir_all(&dir);
    let mut args: Vec<String> = [
        "-p", "--model", "haiku", "--tools", "", "--setting-sources", "", "--strict-mcp-config",
        "--no-session-persistence", "--output-format", "json",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(sys) = system {
        args.push("--system-prompt".into());
        args.push(sys.to_string());
    }
    args.push(prompt.to_string());
    let path = crate::cloud::child_path();
    let mut cmd = if crate::cloud::user_scope() {
        let mut c = tokio::process::Command::new("systemd-run");
        c.args(["--user", "--quiet", "--pipe", "--wait", "--collect", "-p", "MemoryMax=1000M", "--working-directory"])
            .arg(&dir)
            .arg(format!("--setenv=PATH={path}"))
            .arg("--setenv=MAX_THINKING_TOKENS=0")
            .arg("--")
            .arg(&bin)
            .args(&args);
        c
    } else {
        let mut c = tokio::process::Command::new(&bin);
        c.args(&args).current_dir(&dir).env("PATH", &path).env("MAX_THINKING_TOKENS", "0");
        c
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("could not start claude: {e}"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| "haiku timed out".to_string())?
        .map_err(|e| format!("claude failed: {e}"))?;
    serde_json::from_slice(&out.stdout).map_err(|_| "no JSON from claude".to_string())
}

/// Haiku names a new agent.
async fn haiku_name(first: &str, cwd: &str) -> Result<(String, String), String> {
    if let Ok(fake) = std::env::var("SHEPRD_FAKE_AUTONAME") {
        // tests: "Title|color"
        let (t, c) = fake.split_once('|').unwrap_or((&fake, "blue"));
        return Ok((t.to_string(), c.to_string()));
    }
    let prompt = prompt_for(first, cwd);
    let v = run_haiku(None, &prompt, Duration::from_secs(60)).await?;
    let text = v["result"].as_str().unwrap_or("");
    parse_reply(text).ok_or_else(|| format!("unusable reply: {}", text.chars().take(120).collect::<String>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spreads_colors() {
        assert_eq!(spread("blue", &[]), "blue");
        assert_eq!(spread("blue", &["blue".into()]), "purple"); // a neighbour has blue: the next free one
        assert_eq!(spread("pink", &["red".into(), "orange".into()]), "yellow"); // red counts as pink
        let all: Vec<String> = PICK.iter().map(|c| c.to_string()).collect();
        assert_eq!(spread("green", &all), "green"); // every colour used once: Haiku's pick stands
        assert!(!PICK.contains(&"red"));
    }

    #[test]
    fn parses_haiku_reply() {
        assert_eq!(parse_reply("{\"title\": \"Fix Login Bug\", \"color\": \"Red\"}"), Some(("Fix Login Bug".into(), "pink".into())));
        assert_eq!(parse_reply("Sure!\n```json\n{\"title\":\"Bench test guide\",\"color\":\"teal\"}\n```"), Some(("Bench test guide".into(), "blue".into())));
        assert_eq!(parse_reply("{\"title\": \"\"}"), None);
        assert_eq!(parse_reply(&format!("{{\"title\": \"{}\"}}", "word ".repeat(9))), None);
        assert_eq!(parse_reply("no json"), None);
    }

    #[test]
    fn due_rules() {
        let c = |named, fallback, started, text: Option<&str>| Cand {
            pane: "w1:p1".into(), sid: "abc".into(), named, fallback, has_color: false, first_text: text.map(str::to_string), started,
        };
        assert!(due(&c(false, false, 200, Some("hi")), 100)); // new since ship
        assert!(!due(&c(false, false, 50, Some("hi")), 100)); // old, Claude titled it
        assert!(due(&c(false, true, 50, Some("hi")), 100)); // old but still a fallback title
        assert!(!due(&c(true, true, 200, Some("hi")), 100)); // named by the user
        assert!(!due(&c(false, true, 200, None), 100)); // no prompt yet
    }

    #[test]
    fn boot_queue_roundtrip() {
        assert!(!boot_queue("t:1", "x"));
        boot_begin("t:1", Some("first".into()));
        assert!(booting("t:1"));
        assert!(boot_queue("t:1", "second"));
        assert_eq!(boot_take("t:1"), vec!["first".to_string(), "second".to_string()]);
        assert!(!booting("t:1"));
    }

    #[test]
    fn default_args_pick_opus() {
        assert_eq!(default_args(), vec!["--permission-mode", "auto", "--model", "claude-opus-5-5"]);
    }
}
