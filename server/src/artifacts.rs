//! Artifacts and outputs: what an agent made for a human to look at.
//!
//! - `artifact`: a Claude Artifact tool publish (`Published <path> at https://claude.ai/artifact/<id> (Version N, …)`).
//! - `file`: a viewable file the agent wrote with Write (html, md, pdf, svg, images, csv) outside a repo's code.
//!
//! Tracked per transcript, incrementally: each pass only looks at transcript items added since the last one
//! (a cursor per tid), and a finished subagent transcript is never parsed again once its size stops changing.
//! `GET /api/t/:tid/artifacts` lists a session's artifacts (subagents included), newest first; every agent in
//! `/api/state` gets `artifact` = its most recent one.

use crate::transcript::{self, Item, Transcript};
use crate::{App, Shared};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Content-Security-Policy for HTML/SVG served by `/api/file`: an opaque-origin sandbox that may run inline
/// scripts and load page assets from the web, but can never talk to this server (no fetch/XHR/WebSocket, no
/// form posts, no frames). Artifact previews add `sandbox="allow-scripts"` on the iframe as well.
pub const CSP: &str = "sandbox allow-scripts; default-src 'none'; \
script-src 'unsafe-inline' 'unsafe-eval' https:; style-src 'unsafe-inline' https:; \
img-src data: blob: https:; font-src data: https:; media-src data: blob: https:; \
connect-src 'none'; form-action 'none'; frame-src 'none'; worker-src 'none'; base-uri 'none'";

/// Kinds of written files that count as outputs.
const OUT_EXT: &[&str] = &["html", "htm", "md", "markdown", "pdf", "svg", "png", "jpg", "jpeg", "webp", "gif", "csv"];
/// Markdown files that are project plumbing, not something to look at.
const MD_SKIP: &[&str] = &["readme.md", "claude.md", "agents.md", "changelog.md", "memory.md", "skill.md", "contributing.md", "license.md"];

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Art {
    pub kind: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Item index of the newest publish/write, in transcript `tid`.
    pub i: usize,
    pub ts: i64,
    pub tid: String,
}

/// Per-transcript scan state.
#[derive(Default)]
struct Track {
    /// Items already looked at.
    seen: usize,
    /// Transcript file size when last scanned (subagents: skip unchanged files).
    size: u64,
    /// tool_use id → item index, for Artifact/Write calls waiting for their result.
    pending: HashMap<String, usize>,
    arts: Vec<Art>,
    /// Main sessions: subagent tids.
    subs: Vec<String>,
    touched: i64,
}

fn st() -> &'static Mutex<HashMap<String, Track>> {
    static S: OnceLock<Mutex<HashMap<String, Track>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

pub fn routes() -> Router<Shared> {
    Router::new().route("/api/t/{tid}/artifacts", get(api_artifacts))
}

/// Background pass for subagents of live sessions (main transcripts are scanned in `decorate`).
pub async fn run(app: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let sids: Vec<String> = app.inner.lock().unwrap().pane_sessions.values().cloned().collect();
        let app2 = app.clone();
        let changed = tokio::task::spawn_blocking(move || sids.iter().fold(false, |c, s| scan_session(&app2, s) | c))
            .await
            .unwrap_or(false);
        if changed {
            app.refresh.notify_one();
        }
    }
}

/// Sets `artifact` (the newest one, small) on every agent. Scans the main transcripts' new items: refresh() has
/// just parsed them.
pub fn decorate(app: &App, agents: &mut [Value]) {
    for a in agents.iter_mut() {
        let Some(sid) = a["tid"].as_str().map(str::to_string) else { continue };
        let cached = app.transcripts.lock().unwrap().get(&sid).cloned();
        if let Some(t) = cached {
            if let Ok(t) = t.try_lock() {
                scan(&t);
            }
        }
        a["artifact"] = match latest(&sid) {
            Some(mut x) => {
                x.description = x.description.map(|d| transcript::short(&d, 140));
                json!(x)
            }
            None => Value::Null,
        };
    }
}

fn latest(sid: &str) -> Option<Art> {
    let s = st().lock().unwrap();
    let main = s.get(sid)?;
    std::iter::once(main)
        .chain(main.subs.iter().filter_map(|t| s.get(t)))
        .flat_map(|t| t.arts.iter())
        .max_by_key(|a| (rank(a), a.ts))
        .cloned()
}

/// Chip preference: published artifacts and visual outputs (html, svg, images, pdf) over notes (md, csv).
fn rank(a: &Art) -> u8 {
    let e = a.path.as_deref().map(|p| ext(Path::new(p))).unwrap_or_default();
    u8::from(a.kind == "artifact" || !matches!(e.as_str(), "md" | "markdown" | "csv"))
}

/// Scans a main session and its subagents; true when anything new turned up.
fn scan_session(app: &App, sid: &str) -> bool {
    let Some(main) = crate::transcript(app, sid) else { return false };
    let (mut changed, path) = {
        let mut t = main.lock().unwrap();
        t.refresh();
        (scan(&t), t.path.clone())
    };
    let subs = transcript::list_subagents(&path, sid);
    for s in &subs {
        let p = transcript::subagent_path(&path, &s.agent_id);
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        if st().lock().unwrap().get(&s.tid).is_some_and(|t| t.size == size) {
            continue;
        }
        let cached = app.transcripts.lock().unwrap().get(&s.tid).cloned();
        changed |= match cached {
            Some(tx) => {
                let mut t = tx.lock().unwrap();
                t.refresh();
                scan(&t)
            }
            None => {
                let mut t = Transcript::new(s.tid.clone(), p);
                t.refresh();
                scan(&t)
            }
        };
        if let Some(t) = st().lock().unwrap().get_mut(&s.tid) {
            t.size = size;
        }
    }
    let mut g = st().lock().unwrap();
    let t = g.entry(sid.to_string()).or_default();
    t.subs = subs.into_iter().map(|s| s.tid).collect();
    changed
}

/// Looks at items added since the last scan of `t`. True when an artifact appeared or changed.
fn scan(t: &Transcript) -> bool {
    let mut g = st().lock().unwrap();
    if g.len() > 600 {
        // Bounded: forget the least recently scanned transcripts.
        let mut by_age: Vec<(i64, String)> = g.iter().map(|(k, v)| (v.touched, k.clone())).collect();
        by_age.sort();
        for (_, k) in by_age.into_iter().take(200) {
            g.remove(&k);
        }
    }
    let tr = g.entry(t.tid.clone()).or_default();
    tr.touched = transcript::now_ms();
    if t.items.len() < tr.seen {
        // Transcript was rewritten: start over.
        let subs = std::mem::take(&mut tr.subs);
        *tr = Track { subs, touched: tr.touched, ..Default::default() };
    }
    let mut changed = false;
    for it in &t.items[tr.seen..] {
        changed |= step(tr, &t.items, it, &t.tid);
    }
    tr.seen = t.items.len();
    changed
}

fn step(tr: &mut Track, items: &[Item], it: &Item, tid: &str) -> bool {
    match it.k {
        "tool" => {
            let name = it.name.as_deref().unwrap_or("");
            let input = it.input.as_ref().unwrap_or(&Value::Null);
            let s = |k: &str| input.get(k).and_then(Value::as_str);
            let want = match name {
                "Artifact" => {
                    matches!(s("action"), None | Some("publish")) && input.get("asset").and_then(Value::as_bool) != Some(true)
                }
                "Write" => s("file_path").is_some_and(|p| {
                    let known = tr.arts.iter().any(|a| a.path.as_deref() == Some(p));
                    known || is_output(Path::new(p))
                }),
                _ => false,
            };
            if want {
                if let Some(id) = &it.id {
                    tr.pending.insert(id.clone(), it.i);
                }
            }
            false
        }
        "result" => {
            let Some(ix) = it.id.as_ref().and_then(|id| tr.pending.remove(id)) else { return false };
            let Some(call) = items.get(ix) else { return false };
            if it.err == Some(true) {
                return false;
            }
            let input = call.input.clone().unwrap_or(Value::Null);
            let ts = if call.ts > 0 { call.ts } else { it.ts };
            let text = it.text.as_deref().unwrap_or("");
            match call.name.as_deref() {
                Some("Artifact") => match parse_publish(text) {
                    Some(p) => {
                        upsert_artifact(tr, &input, p, ix, ts, tid);
                        true
                    }
                    None => false,
                },
                Some("Write") => {
                    let Some(path) = input.get("file_path").and_then(Value::as_str) else { return false };
                    upsert_file(tr, path, ix, ts, tid);
                    true
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// What a successful publish result says.
#[derive(Debug, PartialEq, Default)]
pub struct Publish {
    pub path: Option<String>,
    pub url: String,
    pub version: Option<u32>,
    pub icon: Option<String>,
}

/// Parses `Published <path> at https://claude.ai/artifact/<id> (Version N, version id …) Icon: "chart".`
/// Anything else (errors, quickstart/list/read output, asset uploads) → None.
pub fn parse_publish(text: &str) -> Option<Publish> {
    parse_publish_line(text).or_else(|| parse_publish_json(text))
}

/// Is `u` a claude.ai artifact link (`/artifact/<id>` or `/code/artifact/<uuid>`)?
pub fn is_artifact_url(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("https://claude.ai/") else { return false };
    let id = rest.strip_prefix("code/").unwrap_or(rest).strip_prefix("artifact/").unwrap_or("");
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A JSON publish result: `{"url": "https://claude.ai/artifact/<id>", "file_path"|"path": …, "version": N, "icon": …}`.
fn parse_publish_json(text: &str) -> Option<Publish> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    let g = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let url = g("url").filter(|u| is_artifact_url(u))?;
    let version = v.get("version").and_then(|x| x.as_u64().or_else(|| x.as_str()?.parse().ok())).map(|n| n as u32);
    let path = g("file_path").or_else(|| g("path")).filter(|p| p.starts_with('/'));
    Some(Publish { path, url, version, icon: g("icon").filter(|i| !i.is_empty()) })
}

fn parse_publish_line(text: &str) -> Option<Publish> {
    let line = text.trim_start().lines().next()?;
    let rest = line.strip_prefix("Published ")?;
    let at = rest.find(" at https://")?;
    let path = rest[..at].trim();
    let after = &rest[at + 4..];
    let url: String = after.chars().take_while(|c| !c.is_whitespace()).collect();
    let url = url.trim_end_matches(['.', ',', ')']).to_string();
    if !url.starts_with("https://claude.ai/") {
        return None;
    }
    let version = after
        .split("(Version ")
        .nth(1)
        .and_then(|v| v.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok());
    let icon = after
        .split("Icon: \"")
        .nth(1)
        .and_then(|v| v.split('"').next())
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    Some(Publish { path: (path.starts_with('/')).then(|| path.to_string()), url, version, icon })
}

fn upsert_artifact(tr: &mut Track, input: &Value, p: Publish, ix: usize, ts: i64, tid: &str) {
    let s = |k: &str| input.get(k).and_then(Value::as_str).filter(|v| !v.trim().is_empty()).map(|v| v.trim().to_string());
    let path = s("file_path").or(p.path);
    let pos = tr.arts.iter().position(|a| a.url.as_deref() == Some(p.url.as_str()) || (path.is_some() && a.path == path));
    let prev = pos.map(|k| tr.arts.remove(k));
    let description = s("description").or_else(|| prev.as_ref().and_then(|a| a.description.clone()));
    let icon = s("icon").or(p.icon).or_else(|| prev.as_ref().and_then(|a| a.icon.clone()));
    let title = path
        .as_deref()
        .and_then(|p| file_title(Path::new(p)))
        .or_else(|| s("title"))
        .or_else(|| prev.as_ref().filter(|a| a.kind == "artifact").map(|a| a.title.clone()))
        .or_else(|| path.as_deref().map(file_name))
        .or_else(|| description.as_deref().map(|d| transcript::short(d, 80)))
        .unwrap_or_else(|| "Artifact".into());
    tr.arts.push(Art {
        kind: "artifact",
        title,
        description,
        icon,
        url: Some(p.url),
        path,
        version: p.version.or_else(|| prev.as_ref().and_then(|a| a.version)),
        i: ix,
        ts,
        tid: tid.to_string(),
    });
}

fn upsert_file(tr: &mut Track, path: &str, ix: usize, ts: i64, tid: &str) {
    let title = file_title(Path::new(path)).unwrap_or_else(|| file_name(path));
    if let Some(k) = tr.arts.iter().position(|a| a.path.as_deref() == Some(path)) {
        // A published artifact's source rewritten: same artifact, newer content.
        let mut a = tr.arts.remove(k);
        if a.kind == "file" || file_title(Path::new(path)).is_some() {
            a.title = title;
        }
        a.i = ix;
        a.ts = ts;
        tr.arts.push(a);
        return;
    }
    tr.arts.push(Art {
        kind: "file",
        title,
        description: None,
        icon: None,
        url: None,
        path: Some(path.to_string()),
        version: None,
        i: ix,
        ts,
        tid: tid.to_string(),
    });
}

fn file_name(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

fn ext(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// A written file worth showing: a viewable type, not project plumbing, and (for html/svg) not app source in
/// a git work tree.
fn is_output(p: &Path) -> bool {
    let e = ext(p);
    if !OUT_EXT.contains(&e.as_str()) {
        return false;
    }
    let s = p.to_string_lossy();
    if s.contains("/.claude/") || s.contains("/node_modules/") || s.contains("/.git/") {
        return false;
    }
    let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if (e == "md" || e == "markdown") && MD_SKIP.contains(&name.as_str()) {
        return false;
    }
    if matches!(e.as_str(), "html" | "htm" | "svg") && in_git_tree(p) {
        return false;
    }
    true
}

fn in_git_tree(p: &Path) -> bool {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut d = p.parent();
    while let Some(dir) = d {
        if Some(dir) == home.as_deref() || dir == Path::new("/") {
            return false;
        }
        if dir.join(".git").exists() {
            return true;
        }
        d = dir.parent();
    }
    false
}

/// Title from the file itself: HTML `<title>`, or a markdown file's first `# ` heading.
fn file_title(p: &Path) -> Option<String> {
    let e = ext(p);
    if !matches!(e.as_str(), "html" | "htm" | "md" | "markdown" | "svg") {
        return None;
    }
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(p).ok()?.take(256 * 1024).read_to_end(&mut buf).ok()?;
    let s = String::from_utf8_lossy(&buf);
    if e == "md" || e == "markdown" {
        return md_title(&s);
    }
    html_title(&s)
}

pub fn html_title(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    let mut from = 0;
    // `<title>` or `<title attr…>`, not `<titlefoo>`
    let open = loop {
        let k = from + lower[from..].find("<title")?;
        let c = lower.as_bytes().get(k + 6).copied().unwrap_or(b'>');
        if c == b'>' || c.is_ascii_whitespace() {
            break k;
        }
        from = k + 6;
    };
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    let t = decode_entities(s[start..end].split_whitespace().collect::<Vec<_>>().join(" ").as_str());
    (!t.is_empty()).then(|| transcript::short(&t, 120))
}

fn md_title(s: &str) -> Option<String> {
    let mut lines = s.lines();
    // skip YAML front matter
    if s.starts_with("---") {
        lines.next();
        for l in lines.by_ref() {
            if l.trim() == "---" {
                break;
            }
        }
    }
    lines
        .take(40)
        .find_map(|l| l.strip_prefix("# "))
        .map(|t| transcript::short(t.replace("**", "").replace('`', "").trim(), 120))
        .filter(|t| !t.is_empty())
}

fn decode_entities(s: &str) -> String {
    let mut out = s.replace("&amp;", "\u{0}");
    for (a, b) in [("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&#39;", "'"), ("&apos;", "'"), ("&nbsp;", " "),
        ("&mdash;", "—"), ("&ndash;", "–"), ("&middot;", "·"), ("&hellip;", "…")] {
        out = out.replace(a, b);
    }
    out.replace('\u{0}', "&")
}

async fn api_artifacts(State(app): State<Shared>, AxPath(tid): AxPath<String>) -> Response {
    let app2 = app.clone();
    let tid2 = tid.clone();
    let r = tokio::task::spawn_blocking(move || -> Option<Vec<Art>> {
        if tid2.contains('~') {
            let tx = crate::transcript(&app2, &tid2)?;
            let mut t = tx.lock().unwrap();
            t.refresh();
            scan(&t);
        } else {
            crate::transcript(&app2, &tid2)?;
            scan_session(&app2, &tid2);
        }
        let s = st().lock().unwrap();
        let main = s.get(&tid2)?;
        let mut all: Vec<Art> = std::iter::once(main)
            .chain(main.subs.iter().filter_map(|t| s.get(t)))
            .flat_map(|t| t.arts.iter().cloned())
            .collect();
        all.sort_by_key(|a| std::cmp::Reverse((a.ts, a.kind == "artifact")));
        // One entry per source (a subagent and its parent may both touch it): keep the newest.
        let mut seen = std::collections::HashSet::new();
        all.retain(|a| seen.insert(a.url.clone().or_else(|| a.path.clone()).unwrap_or_default()));
        Some(all)
    })
    .await
    .ok()
    .flatten();
    match r {
        Some(list) => Json(json!({"tid": tid, "artifacts": list})).into_response(),
        // a fresh session whose transcript file does not exist yet
        None if crate::pending_session(&app, &tid) => Json(json!({"tid": tid, "artifacts": []})).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "no such transcript"}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_result() {
        let t = "Published /tmp/x/scratchpad/rtx-5070-ti-buying-sheet.html at https://claude.ai/artifact/JYNf48p3U1DRVu3STKRS68 (Version 1, version id 1791191203-9bad) Icon: \"chart\".\n\nLive subscription: arming";
        let p = parse_publish(t).unwrap();
        assert_eq!(p.path.as_deref(), Some("/tmp/x/scratchpad/rtx-5070-ti-buying-sheet.html"));
        assert_eq!(p.url, "https://claude.ai/artifact/JYNf48p3U1DRVu3STKRS68");
        assert_eq!(p.version, Some(1));
        assert_eq!(p.icon.as_deref(), Some("chart"));
        let t2 = "Published /a b/c.html at https://claude.ai/artifact/Z9 (Version 12, version id 1-2)\n\nmore";
        let p2 = parse_publish(t2).unwrap();
        assert_eq!(p2.path.as_deref(), Some("/a b/c.html"));
        assert_eq!(p2.version, Some(12));
        assert_eq!(p2.icon, None);
        // Not publishes
        assert!(parse_publish("Quickstart for a page. Published /x at https://claude.ai/artifact/1").is_none());
        assert!(parse_publish("Error: publish refused").is_none());
        assert!(parse_publish("Published /x at https://evil.example/artifact/1 (Version 1)").is_none());
        assert!(parse_publish("Uploaded /x.png to https://claude.ai/artifact/1").is_none());
        // newer shapes: the /code/ path and a JSON result
        let c = parse_publish("Published /t/doc.md at https://claude.ai/code/artifact/0a1b2c3d-4e5f-6a7b (Version 3, version id z)").unwrap();
        assert_eq!((c.url.as_str(), c.path.as_deref(), c.version), ("https://claude.ai/code/artifact/0a1b2c3d-4e5f-6a7b", Some("/t/doc.md"), Some(3)));
        let j = parse_publish(r#"{"url":"https://claude.ai/artifact/AbCdEf0123456789xYz012","file_path":"/t/p.html","version":1,"icon":"printer"}"#).unwrap();
        assert_eq!((j.url.as_str(), j.path.as_deref(), j.version, j.icon.as_deref()), ("https://claude.ai/artifact/AbCdEf0123456789xYz012", Some("/t/p.html"), Some(1), Some("printer")));
        assert!(parse_publish(r#"{"url":"https://evil.example/artifact/1"}"#).is_none());
        assert!(parse_publish(r#"{"artifacts":[]}"#).is_none());
        assert!(is_artifact_url("https://claude.ai/artifact/AbCd") && !is_artifact_url("https://claude.ai/artifact/") && !is_artifact_url("https://claude.ai/chat/x"));
    }

    #[test]
    fn titles() {
        assert_eq!(html_title("<html><head><title>RTX 5070 Ti &amp; friends</title>").as_deref(), Some("RTX 5070 Ti & friends"));
        assert_eq!(html_title("<TITLE lang=en>\n  Two\n words </TITLE>").as_deref(), Some("Two words"));
        assert_eq!(html_title("<titlex>no</titlex><title>yes</title>").as_deref(), Some("yes"));
        assert_eq!(html_title("<title></title>"), None);
        assert_eq!(html_title("<p>no title</p>"), None);
        assert_eq!(md_title("---\ntitle: x\n---\nintro\n# The **Plan**\n").as_deref(), Some("The Plan"));
        assert_eq!(md_title("no heading"), None);
    }

    fn line(v: Value) -> String {
        format!("{}\n", v)
    }

    #[test]
    fn tracks_publishes_and_outputs() {
        let dir = std::env::temp_dir().join(format!("mm-art-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("sheet.html");
        std::fs::write(&src, "<title>Buying Sheet</title>").unwrap();
        let note = dir.join("notes.md");
        std::fs::write(&note, "# Findings\n").unwrap();
        let src_s = src.to_string_lossy().to_string();
        let note_s = note.to_string_lossy().to_string();
        let use_ = |id: &str, name: &str, input: Value| {
            line(json!({"type":"assistant","timestamp":"2026-10-05T10:00:00Z","message":{"content":[{"type":"tool_use","id":id,"name":name,"input":input}]}}))
        };
        let res = |id: &str, text: &str| {
            line(json!({"type":"user","timestamp":"2026-10-05T10:00:01Z","message":{"content":[{"type":"tool_result","tool_use_id":id,"content":text}]}}))
        };
        let mut body = String::new();
        body += &use_("q", "Artifact", json!({"action":"quickstart","intent":"other"}));
        body += &res("q", "Published nothing");
        body += &use_("w1", "Write", json!({"file_path": src_s, "content": "<title>Buying Sheet</title>"}));
        body += &res("w1", "File created");
        body += &use_("a1", "Artifact", json!({"file_path": src_s, "icon":"chart", "description":"Listings"}));
        body += &res("a1", &format!("Published {src_s} at https://claude.ai/artifact/AB1 (Version 1, version id x) Icon: \"chart\"."));
        let path = dir.join("t.jsonl");
        std::fs::write(&path, &body).unwrap();
        let tid = format!("test-{}", std::process::id());
        let mut t = Transcript::new(tid.clone(), path.clone());
        t.refresh();
        assert!(scan(&t));
        let a = latest_of(&tid);
        assert_eq!(a.len(), 1, "write + publish of one source is one artifact");
        assert_eq!(a[0].kind, "artifact");
        assert_eq!(a[0].title, "Buying Sheet");
        assert_eq!(a[0].version, Some(1));
        assert_eq!(a[0].icon.as_deref(), Some("chart"));
        // Incremental: a second publish (no description) and a markdown output arrive later.
        let mut more = String::new();
        more += &use_("a2", "Artifact", json!({"file_path": src_s}));
        more += &res("a2", &format!("Published {src_s} at https://claude.ai/artifact/AB1 (Version 2, version id y)"));
        more += &use_("w2", "Write", json!({"file_path": note_s, "content": "# Findings"}));
        more += &res("w2", "File created");
        more += &use_("w3", "Write", json!({"file_path": dir.join("x.rs").to_string_lossy(), "content": ""}));
        more += &res("w3", "File created");
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(more.as_bytes()).unwrap();
        t.refresh();
        assert!(scan(&t));
        let a = latest_of(&tid);
        assert_eq!(a.len(), 2);
        let art = a.iter().find(|x| x.kind == "artifact").unwrap();
        assert_eq!(art.version, Some(2));
        assert_eq!(art.description.as_deref(), Some("Listings"));
        let md = a.iter().find(|x| x.kind == "file").unwrap();
        assert_eq!(md.title, "Findings");
        assert!(!scan(&t), "nothing new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The item shapes of a Master publish (fixtures/artifact-publish.json): the
    /// Artifact input has no title, so the title comes from the source's `<title>`, never the description.
    #[test]
    fn live_publish_fixture() {
        let fx: Value = serde_json::from_str(include_str!("../fixtures/artifact-publish.json")).unwrap();
        let dir = std::env::temp_dir().join(format!("mm-art-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("compare.html");
        std::fs::write(&src, "<title>Plan A vs Plan B</title>\n<style></style>").unwrap();
        let src_s = src.to_string_lossy().to_string();
        let sub = |v: &Value| serde_json::from_str::<Value>(&v.to_string().replace("{SRC}", &src_s)).unwrap();
        let (call, res) = (sub(&fx["items"][0]), sub(&fx["items"][1]));
        let p = parse_publish(res["text"].as_str().unwrap()).unwrap();
        assert_eq!(p.url, "https://claude.ai/artifact/AbCdEf0123456789xYz012");
        assert_eq!((p.path.as_deref(), p.version, p.icon.as_deref()), (Some(src_s.as_str()), Some(1), Some("chart")));
        let mut body = String::new();
        body += &line(json!({"type":"assistant","timestamp":"2026-10-06T23:37:31Z","message":{"content":[{"type":"tool_use","id":call["id"],"name":"Artifact","input":call["input"]}]}}));
        body += &line(json!({"type":"user","timestamp":"2026-10-06T23:37:32Z","message":{"content":[{"type":"tool_result","tool_use_id":res["id"],"content":res["text"]}]}}));
        let path = dir.join("t.jsonl");
        std::fs::write(&path, &body).unwrap();
        let tid = format!("live-{}", std::process::id());
        let mut t = Transcript::new(tid.clone(), path);
        t.refresh();
        assert!(scan(&t));
        let a = latest_of(&tid);
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].title.as_str(), a[0].url.as_deref(), a[0].icon.as_deref()), ("Plan A vs Plan B", Some("https://claude.ai/artifact/AbCdEf0123456789xYz012"), Some("chart")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn latest_of(tid: &str) -> Vec<Art> {
        st().lock().unwrap().get(tid).map(|t| t.arts.clone()).unwrap_or_default()
    }
}
