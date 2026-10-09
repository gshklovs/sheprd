//! Slash-command autocomplete for the composer (API.md "Slash commands"): `GET /api/a/{pane}/commands` lists what
//! that agent can run as `/name`: its project skills and commands (`<cwd and parents>/.claude/{skills,commands}`),
//! the user's (`~/.claude/skills` incl. `synced/<bucket>/`, `~/.claude/commands`), enabled plugins' skills and
//! commands (`plugin:name`), then the phone-friendly built-ins. Only names and one-line descriptions are read
//! (the frontmatter of SKILL.md / command .md files, first 8 KB); parsed files are cached by mtime.

use crate::{Shared, json_err};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

pub fn routes() -> Router<Shared> {
    Router::new().route("/api/a/{pane}/commands", get(api_commands))
}

/// Built-in Claude Code commands and bundled skills that make sense from a phone.
const BUILTIN: &[(&str, &str)] = &[
    ("compact", "Summarize the conversation to free up context (optional: what to keep)"),
    ("clear", "Start fresh: clear the conversation history"),
    ("context", "Show what is filling the context window"),
    ("usage", "Plan usage limits and when they reset"),
    ("cost", "Token use and cost of this session"),
    ("model", "Switch the model"),
    ("plan", "Switch to plan mode"),
    ("resume", "Resume an earlier conversation"),
    ("rewind", "Rewind the conversation and code to an earlier point"),
    ("fork", "Fork this conversation into a new session"),
    ("background", "Move this conversation to a background job"),
    ("btw", "Ask a quick side question without interrupting the work"),
    ("todos", "List the current todo items"),
    ("rename", "Rename this session"),
    ("color", "Set this session's color"),
    ("memory", "Edit the CLAUDE.md memory files"),
    ("agents", "Manage subagents"),
    ("mcp", "MCP servers and their status"),
    ("status", "Version, model, account and connectivity"),
    ("export", "Export the conversation"),
    ("init", "Write a CLAUDE.md for this codebase"),
    ("code-review", "Review the current diff or a PR for bugs"),
    ("simplify", "Clean up the changed code for reuse and simplicity"),
    ("security-review", "Security review of the pending changes"),
    ("loop", "Run a prompt or command on an interval"),
    ("schedule", "Create or run scheduled cloud agents"),
];

#[derive(Clone)]
struct Cmd {
    name: String,
    desc: String,
}

/// path → (mtime, parsed); a miss or a changed mtime re-reads the file.
fn cache() -> &'static Mutex<HashMap<PathBuf, (SystemTime, Option<Cmd>)>> {
    static C: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, Option<Cmd>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn head(p: &Path) -> Option<String> {
    let mut b = Vec::new();
    std::fs::File::open(p).ok()?.take(8192).read_to_end(&mut b).ok()?;
    Some(String::from_utf8_lossy(&b).into_owned())
}

/// `name`, `description` and `user-invocable` from YAML frontmatter (plain, quoted, folded `>` / literal `|`).
fn frontmatter(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return out;
    }
    let mut key: Option<String> = None;
    let mut val: Vec<String> = Vec::new();
    let mut flush = |k: &mut Option<String>, v: &mut Vec<String>| {
        if let Some(k) = k.take() {
            let s = v.join(" ");
            let s = s.trim();
            let s = if s.len() >= 2 && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\''))) {
                s[1..s.len() - 1].replace("\\\"", "\"").replace("''", "'")
            } else {
                s.to_string()
            };
            out.insert(k, s);
        }
        v.clear();
    };
    for l in lines {
        if l.trim() == "---" {
            break;
        }
        let top = !l.starts_with([' ', '\t']) && !l.trim().is_empty();
        if top {
            flush(&mut key, &mut val);
            if let Some((k, v)) = l.split_once(':') {
                let k = k.trim().to_lowercase();
                if matches!(k.as_str(), "name" | "description" | "user-invocable") {
                    key = Some(k);
                    let v = v.trim();
                    if !matches!(v, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
                        val.push(v.to_string());
                    }
                }
            }
        } else if key.is_some() && !l.trim().is_empty() {
            val.push(l.trim().to_string());
        }
    }
    flush(&mut key, &mut val);
    out
}

/// One line, at most 160 chars.
fn one_line(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= 160 {
        return s;
    }
    let mut t: String = s.chars().take(157).collect();
    t = t.trim_end().to_string();
    t.push('…');
    t
}

/// Parse a SKILL.md (`skill`) or a command .md; `fallback` = the dir or file stem.
fn parse(p: &Path, fallback: &str, skill: bool) -> Option<Cmd> {
    let mtime = std::fs::metadata(p).and_then(|m| m.modified()).ok()?;
    if let Some((t, c)) = cache().lock().unwrap().get(p) {
        if *t == mtime {
            return c.clone();
        }
    }
    let c = head(p).and_then(|text| {
        let fm = frontmatter(&text);
        if fm.get("user-invocable").map(|v| v.trim() == "false").unwrap_or(false) {
            return None;
        }
        let name = if skill { fm.get("name").filter(|n| !n.is_empty()).cloned() } else { None }.unwrap_or_else(|| fallback.to_string());
        let mut desc = fm.get("description").cloned().unwrap_or_default();
        if desc.is_empty() && !skill {
            // first non-empty body line after the frontmatter
            let body = if text.starts_with("---") { text.splitn(3, "---").nth(2).unwrap_or("") } else { &text };
            desc = body.lines().map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("").to_string();
        }
        let ok = !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || "-_:.".contains(c));
        ok.then(|| Cmd { name, desc: one_line(&desc) })
    });
    cache().lock().unwrap().insert(p.to_path_buf(), (mtime, c.clone()));
    c
}

fn sorted_dir(d: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(d).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

/// `<dir>/<skill>/SKILL.md`
fn skills_in(d: &Path, ns: &str, out: &mut Vec<(Cmd, &'static str)>, src: &'static str) {
    for p in sorted_dir(d) {
        let f = p.join("SKILL.md");
        let stem = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if stem.starts_with('.') || !f.is_file() {
            continue;
        }
        if let Some(mut c) = parse(&f, &stem, true) {
            c.name = format!("{ns}{}", c.name);
            out.push((c, src));
        }
    }
}

/// `<dir>/<name>.md`, one level of subfolders (shown as `/name` like Claude Code does).
fn commands_in(d: &Path, ns: &str, out: &mut Vec<(Cmd, &'static str)>, src: &'static str, depth: u8) {
    for p in sorted_dir(d) {
        let stem = p.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if stem.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            if depth == 0 {
                commands_in(&p, ns, out, src, 1);
            }
        } else if p.extension().is_some_and(|e| e == "md") {
            if let Some(mut c) = parse(&p, &stem, false) {
                c.name = format!("{ns}{}", c.name);
                out.push((c, src));
            }
        }
    }
}

/// Enabled plugins: (name, install path). Reads only `enabledPlugins` keys from settings and install paths.
fn plugins(claude: &Path) -> Vec<(String, PathBuf)> {
    let read = |p: PathBuf| -> Value { std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null) };
    let settings = read(claude.join("settings.json"));
    let enabled: HashSet<String> = settings["enabledPlugins"]
        .as_object()
        .map(|m| m.iter().filter(|(_, v)| v.as_bool() == Some(true)).map(|(k, _)| k.clone()).collect())
        .unwrap_or_default();
    let inst = read(claude.join("plugins/installed_plugins.json"));
    let mut v = Vec::new();
    if let Some(m) = inst["plugins"].as_object() {
        let mut keys: Vec<&String> = m.keys().filter(|k| enabled.contains(*k)).collect();
        keys.sort();
        for k in keys {
            let Some(path) = m[k].as_array().and_then(|a| a.iter().rev().find_map(|e| e["installPath"].as_str())) else { continue };
            let name = k.split('@').next().unwrap_or(k).to_string();
            v.push((name, PathBuf::from(path)));
        }
    }
    v
}

/// Everything `/` can run for an agent in `cwd`, in menu order, deduplicated by name.
pub fn list(home: &Path, cwd: Option<&Path>) -> Vec<Value> {
    let claude = home.join(".claude");
    let mut all: Vec<(Cmd, &'static str)> = Vec::new();
    // project: the cwd and its parents below $HOME (the home .claude is the user scope)
    if let Some(cwd) = cwd {
        for d in cwd.ancestors() {
            if d == home {
                break;
            }
            skills_in(&d.join(".claude/skills"), "", &mut all, "project");
            commands_in(&d.join(".claude/commands"), "", &mut all, "project", 0);
        }
    }
    skills_in(&claude.join("skills"), "", &mut all, "user");
    commands_in(&claude.join("commands"), "", &mut all, "user", 0);
    // claude.ai skills synced into ~/.claude/skills/synced/<bucket>/ (Claude Code shows them as anthropic-skills:x)
    for b in sorted_dir(&claude.join("skills/synced")) {
        if b.is_dir() && !b.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
            skills_in(&b, "anthropic-skills:", &mut all, "synced");
        }
    }
    for (name, path) in plugins(&claude) {
        let ns = format!("{name}:");
        skills_in(&path.join("skills"), &ns, &mut all, "plugin");
        commands_in(&path.join("commands"), &ns, &mut all, "plugin", 0);
    }
    for (n, d) in BUILTIN {
        all.push((Cmd { name: n.to_string(), desc: d.to_string() }, "builtin"));
    }
    let mut seen = HashSet::new();
    all.into_iter().filter(|(c, _)| seen.insert(c.name.clone())).map(|(c, src)| json!({"name": c.name, "desc": c.desc, "src": src})).collect()
}

async fn api_commands(State(app): State<Shared>, AxPath(pane): AxPath<String>) -> Response {
    let cwd = {
        let s: Value = serde_json::from_str(&app.inner.lock().unwrap().state).unwrap_or(Value::Null);
        let a = s["agents"].as_array().and_then(|l| l.iter().find(|a| a["pane"] == pane.as_str()).cloned());
        match a {
            Some(a) => a["cwd"].as_str().filter(|c| !c.is_empty()).map(PathBuf::from),
            None => return json_err(StatusCode::NOT_FOUND, "no such agent"),
        }
    };
    let home = crate::home();
    match tokio::task::spawn_blocking(move || list(&home, cwd.as_deref())).await {
        Ok(items) => Json(json!({"commands": items})).into_response(),
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_forms() {
        let fm = frontmatter("---\nname: omarchy\ndescription: >\n  REQUIRED for desktop.\n  Use when editing.\nother: x\n---\nbody");
        assert_eq!(fm["name"], "omarchy");
        assert_eq!(fm["description"], "REQUIRED for desktop. Use when editing.");
        let fm = frontmatter("---\nname: \"pdf\"\ndescription: 'It''s a pdf: tool'\n---\n");
        assert_eq!(fm["name"], "pdf");
        assert_eq!(fm["description"], "It's a pdf: tool");
        assert!(frontmatter("no frontmatter").is_empty());
        let fm = frontmatter("---\ndescription: one\n  two\nuser-invocable: false\n---\n");
        assert_eq!(fm["description"], "one two");
        assert_eq!(fm["user-invocable"], "false");
    }

    #[test]
    fn lists_sources_in_order() {
        let t = std::env::temp_dir().join(format!("mm-slash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let home = t.join("home");
        let w = |p: PathBuf, s: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, s).unwrap();
        };
        let cl = home.join(".claude");
        w(cl.join("skills/foo/SKILL.md"), "---\nname: foo\ndescription: Foo things\n---\n");
        w(cl.join("skills/hidden/SKILL.md"), "---\nname: hidden\nuser-invocable: false\n---\n");
        w(cl.join("skills/synced/b1/docs/SKILL.md"), "---\nname: docs\ndescription: Docs\n---\n");
        w(cl.join("commands/deploy.md"), "# Deploy the thing\nsteps");
        w(cl.join("commands/git/ship.md"), "---\ndescription: Ship it\n---\n");
        let pl = t.join("plug");
        w(pl.join("skills/bar/SKILL.md"), "---\nname: bar\ndescription: Bar\n---\n");
        w(pl.join("commands/baz.md"), "---\ndescription: Baz\n---\n");
        w(cl.join("settings.json"), r#"{"enabledPlugins":{"pk@m":true,"off@m":false},"env":{"SECRET":"x"}}"#);
        w(cl.join("plugins/installed_plugins.json"), &json!({"plugins":{"pk@m":[{"installPath": pl}], "off@m":[{"installPath": pl}]}}).to_string());
        let proj = home.join("Work/proj");
        w(proj.join(".claude/skills/foo/SKILL.md"), "---\nname: foo\ndescription: Project foo\n---\n");
        w(home.join("Work/.claude/commands/up.md"), "Parent command");
        let l = list(&home, Some(&proj));
        let names: Vec<&str> = l.iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert_eq!(&names[..8], &["foo", "up", "deploy", "ship", "anthropic-skills:docs", "pk:bar", "pk:baz", "compact"]);
        assert_eq!(l[0]["desc"], "Project foo");
        assert_eq!(l[1]["desc"], "Parent command");
        assert_eq!(l[2]["desc"], "Deploy the thing");
        assert!(!names.contains(&"hidden") && !names.iter().any(|n| n.starts_with("off:")));
        assert!(!l.iter().any(|v| v.to_string().contains("SECRET")));
        // mtime cache: an edit is picked up
        std::thread::sleep(std::time::Duration::from_millis(20));
        w(cl.join("skills/foo/SKILL.md"), "---\nname: foo\ndescription: Changed\n---\n");
        let l = list(&home, None);
        assert_eq!(l[0]["desc"], "Changed");
        let _ = std::fs::remove_dir_all(&t);
    }
}
