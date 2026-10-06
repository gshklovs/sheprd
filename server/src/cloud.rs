//! Cloud routines tab: the user's scheduled Claude cloud agents (claude.ai
//! "routines" / remote triggers) and their runs.
//!
//! There is no `claude` subcommand for routines, and the API needs the user's
//! claude.ai login, which only Claude Code itself handles. So the supported read
//! path is Claude Code's own `RemoteTrigger` tool, run in a throwaway headless
//! session restricted to that one tool (read-only actions: `list`,
//! `list_runs`): `claude -p --tools RemoteTrigger --no-session-persistence …`.
//! The tool result (inline, or the file Claude persists large results to) is
//! parsed from the stream-json output. Each fetch costs one small Haiku turn, so
//! it only runs when the phone asks (Refresh), single-flight, and the result is
//! cached in `<state dir>/cloud.json`. The headless run is started through
//! `systemd-run --user` when available so it is not counted against this
//! service's memory cap.

use crate::transcript::{cut, now_ms};
use crate::{Shared, json_err};
use axum::extract::Path as AxPath;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/cloud", get(api_get))
        .route("/api/cloud/refresh", post(api_refresh))
        .route("/api/cloud/{id}/runs", post(api_runs))
}

#[derive(Default)]
struct Cloud {
    /// cached document: {fetchedAt, available, reason, routines, hasMore, runs:{id:{fetchedAt, runs}}}
    doc: Value,
    busy: Option<String>,
    loaded: bool,
}

static CLOUD: LazyLock<Mutex<Cloud>> = LazyLock::new(|| Mutex::new(Cloud::default()));

fn cache_file() -> PathBuf {
    crate::state_dir().join("cloud.json")
}

fn doc() -> Value {
    let mut c = CLOUD.lock().unwrap();
    if !c.loaded {
        c.loaded = true;
        c.doc = std::fs::read(cache_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}));
    }
    let mut d = c.doc.clone();
    d["busy"] = json!(c.busy);
    d
}

fn store(f: impl FnOnce(&mut Value)) {
    let mut c = CLOUD.lock().unwrap();
    if !c.doc.is_object() {
        c.doc = json!({});
    }
    f(&mut c.doc);
    let _ = std::fs::write(cache_file(), c.doc.to_string());
}

/// The real claude binary (the ~/.local/bin wrapper needs mise on PATH).
pub(crate) fn claude_bin() -> Option<PathBuf> {
    let h = crate::home();
    [h.join(".local/share/mise/installs/claude/latest/claude"), h.join(".local/bin/claude"), "/usr/bin/claude".into()]
        .into_iter()
        .find(|p| p.is_file())
}

pub(crate) fn which(name: &str) -> bool {
    ["/usr/bin", "/bin", "/usr/local/bin"].iter().any(|d| Path::new(d).join(name).is_file())
}

/// Runs one headless turn that calls RemoteTrigger and returns the tool results
/// (parsed JSON bodies) in call order, or an "unavailable" reason.
async fn remote_trigger(prompt: &str) -> Result<Vec<Value>, (bool, String)> {
    let Some(bin) = claude_bin() else { return Err((false, "claude is not installed".into())) };
    let dir = crate::state_dir().join("cloud-run");
    let _ = std::fs::create_dir_all(&dir);
    let args: Vec<String> = [
        "-p", "--model", "haiku", "--tools", "RemoteTrigger", "--allowedTools", "RemoteTrigger",
        "--no-session-persistence", "--strict-mcp-config", "--output-format", "stream-json", "--verbose", prompt,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let path = format!(
        "{}:{}:/usr/local/bin:/usr/bin:/bin",
        crate::home().join(".local/bin").display(),
        crate::home().join(".local/share/mise/shims").display()
    );
    let mut cmd = if which("systemd-run") {
        let mut c = tokio::process::Command::new("systemd-run");
        c.args(["--user", "--quiet", "--pipe", "--wait", "--collect", "-p", "MemoryMax=1500M", "--working-directory"])
            .arg(&dir)
            .arg(format!("--setenv=PATH={path}"))
            .arg("--")
            .arg(&bin)
            .args(&args);
        c
    } else {
        let mut c = tokio::process::Command::new(&bin);
        c.args(&args).current_dir(&dir).env("PATH", &path);
        c
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| (true, format!("could not start claude: {e}")))?;
    let out = match tokio::time::timeout(Duration::from_secs(150), child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err((true, format!("claude failed: {e}"))),
        Err(_) => return Err((true, "claude timed out".into())),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let r = parse_stream(&text);
    cleanup(&text); // always: the persisted tool output must not outlive this call
    r
}

/// Drops every key that names a token/credential (e.g. `api_token_hint`), at any
/// depth, so nothing token-related reaches the cache, logs or the phone.
fn scrub(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|k, _| {
                let k = k.to_lowercase();
                !(k.contains("token") || k.contains("secret") || k.contains("credential") || k.contains("api_key"))
            });
            m.values_mut().for_each(scrub);
        }
        Value::Array(a) => a.iter_mut().for_each(scrub),
        _ => {}
    }
}

/// Tool results of RemoteTrigger calls from stream-json output.
fn parse_stream(text: &str) -> Result<Vec<Value>, (bool, String)> {
    let mut tools_ok = None;
    let mut results = Vec::new();
    let mut ids = std::collections::HashSet::new();
    let mut final_err = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => {
                tools_ok = Some(v["tools"].as_array().is_some_and(|t| t.iter().any(|x| x == "RemoteTrigger")));
            }
            Some("assistant") => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    if c["type"] == "tool_use" && c["name"] == "RemoteTrigger" {
                        if let Some(id) = c["id"].as_str() {
                            ids.insert(id.to_string());
                        }
                    }
                }
            }
            Some("user") => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    if c["type"] != "tool_result" || !c["tool_use_id"].as_str().is_some_and(|i| ids.contains(i)) {
                        continue;
                    }
                    let body = match &c["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
                        _ => String::new(),
                    };
                    results.push(tool_body(&body, c["is_error"] == true));
                }
            }
            Some("result") if v["is_error"] == true => {
                final_err = v["result"].as_str().map(str::to_string);
            }
            _ => {}
        }
    }
    if tools_ok == Some(false) {
        return Err((false, "The RemoteTrigger tool is not enabled for this Claude account (needs a claude.ai login with remote sessions allowed).".into()));
    }
    if results.is_empty() {
        return Err((true, final_err.unwrap_or_else(|| "claude returned no RemoteTrigger result".into())));
    }
    if let Some(e) = results.iter().find_map(|r| r["error"].as_str()) {
        let lower = e.to_lowercase();
        let unavailable = lower.contains("not authenticated") || lower.contains("unavailable") || lower.contains("login");
        return Err((!unavailable, e.to_string()));
    }
    Ok(results)
}

/// `HTTP 200\n{json}`, or a `<persisted-output>` note naming the file holding it.
fn tool_body(body: &str, is_error: bool) -> Value {
    let mut text = body.to_string();
    if body.contains("<persisted-output>") {
        if let Some(p) = body.split("saved to: ").nth(1).and_then(|r| r.lines().next()).map(str::trim) {
            let p = PathBuf::from(p);
            if p.starts_with(crate::home().join(".claude/projects")) {
                if let Ok(t) = std::fs::read_to_string(&p) {
                    text = t;
                }
            }
        }
    }
    let (status, rest) = match text.strip_prefix("HTTP ") {
        Some(r) => {
            let (code, json) = r.split_once('\n').unwrap_or((r, ""));
            (code.trim().parse::<u16>().unwrap_or(0), json)
        }
        None => (0, text.as_str()),
    };
    // The JSON body may be followed by a summary line ("→ no run sessions recorded …").
    let mut it = serde_json::Deserializer::from_str(rest.trim()).into_iter::<Value>();
    let first = it.next().map(|r| r.map(|mut v| {
        scrub(&mut v);
        v
    }));
    match first {
        Some(Ok(mut v)) if (200..300).contains(&status) || (status == 0 && !is_error) => {
            let tail = rest.trim()[it.byte_offset()..].trim().trim_start_matches('→').trim();
            if !tail.is_empty() && v.is_object() {
                v["summary"] = json!(cut(tail, 400).0);
            }
            v
        }
        Some(Ok(v)) => json!({"error": format!("HTTP {status}: {}", cut(&v.to_string(), 300).0)}),
        _ if text.to_lowercase().contains("token") => json!({"error": "unreadable RemoteTrigger result"}),
        _ => json!({"error": cut(text.trim(), 400).0}),
    }
}

/// Removes the persisted tool-result files of our own throwaway run.
fn cleanup(stream: &str) {
    let sid = stream.lines().find_map(|l| {
        let v: Value = serde_json::from_str(l).ok()?;
        (v["type"] == "system" && v["subtype"] == "init").then(|| v["session_id"].as_str().map(str::to_string)).flatten()
    });
    let Some(sid) = sid.filter(|s| crate::valid_sid(s)) else { return };
    let dir = crate::state_dir().join("cloud-run");
    let proj = dir.to_string_lossy().replace(['/', '.'], "-");
    let pd = crate::home().join(".claude/projects").join(proj);
    let d = pd.join(&sid);
    if d.is_dir() {
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir(pd.join("memory")); // only if empty
        let _ = std::fs::remove_dir(&pd);
    }
}

fn s(v: &Value) -> Value {
    match v.as_str() {
        Some(x) if !x.is_empty() => json!(x),
        _ => Value::Null,
    }
}

fn status_word(s: &str) -> String {
    s.trim_start_matches("ROUTINE_RUN_STATUS_").to_lowercase().replace('_', " ")
}

/// A routine as the phone shows it.
fn routine(t: &Value) -> Value {
    let prompt = t["derived_state"]["prompt"].as_str().unwrap_or("");
    let last = &t["last_run"];
    let sid = t["persistent_session_id"].as_str().filter(|x| !x.is_empty()).or(last["session_id"].as_str());
    json!({
        "id": t["id"], "name": s(&t["name"]), "enabled": t["enabled"], "cron": s(&t["cron_expression"]),
        "kind": s(&t["created_kind"]), "runOnceAt": s(&t["run_once_at"]), "nextRunAt": s(&t["next_run_at"]),
        "endedReason": s(&t["ended_reason"]), "suspended": s(&t["suspension_reason"]),
        "created": s(&t["created_at"]), "updated": s(&t["updated_at"]), "model": s(&t["derived_state"]["model"]),
        "prompt": cut(prompt, 600).0,
        "lastRun": if last.is_object() { json!({"status": status_word(last["status"].as_str().unwrap_or("")),
            "firedAt": s(&last["fired_at"]), "finishedAt": s(&last["finished_at"]), "session": s(&last["session_id"]),
            "failure": last["failure_reason"].as_str().filter(|f| !f.ends_with("UNSPECIFIED")).map(status_word)}) } else { Value::Null },
        "url": sid.map(|x| format!("https://claude.ai/code/{x}")),
    })
}

fn run_row(r: &Value) -> Value {
    let id = r["id"].as_str().or(r["session_id"].as_str());
    json!({
        "id": id, "title": s(&r["title"]), "status": s(&r["session_status"]).as_str().map(status_word).map(Value::from).unwrap_or(s(&r["status"])),
        "created": s(&r["created_at"]), "updated": s(&r["updated_at"]),
        "url": id.map(|x| format!("https://claude.ai/code/{x}")),
    })
}

async fn api_get() -> Response {
    let mut d = doc();
    d["capable"] = json!(claude_bin().is_some());
    Json(d).into_response()
}

fn begin(what: &str) -> Result<(), Response> {
    let mut c = CLOUD.lock().unwrap();
    if let Some(b) = &c.busy {
        return Err((StatusCode::CONFLICT, Json(json!({"error": "busy", "busy": b}))).into_response());
    }
    c.busy = Some(what.to_string());
    Ok(())
}

fn end() {
    CLOUD.lock().unwrap().busy = None;
}

async fn api_refresh() -> Response {
    if let Err(r) = begin("list") {
        return r;
    }
    tokio::spawn(async {
        let r = remote_trigger("Call the RemoteTrigger tool once with action \"list\". Then reply with just: ok").await;
        store(|d| {
            d["fetchedAt"] = json!(now_ms());
            match r {
                Ok(res) => {
                    let page = &res[0];
                    let list: Vec<Value> = page["data"].as_array().into_iter().flatten().map(routine).collect();
                    d["available"] = json!(true);
                    d["reason"] = Value::Null;
                    d["routines"] = json!(list);
                    d["hasMore"] = json!(page["has_more"] == true || page["next_cursor"].as_str().is_some_and(|c| !c.is_empty()));
                }
                Err((transient, e)) => {
                    eprintln!("cloud routines: {e}");
                    if !transient {
                        d["available"] = json!(false);
                    }
                    d["reason"] = json!(e);
                }
            }
        });
        end();
    });
    (StatusCode::ACCEPTED, Json(json!({"ok": true}))).into_response()
}

async fn api_runs(AxPath(id): AxPath<String>) -> Response {
    if id.is_empty() || id.len() > 80 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return json_err(StatusCode::BAD_REQUEST, "bad routine id");
    }
    if let Err(r) = begin(&format!("runs:{id}")) {
        return r;
    }
    tokio::spawn(async move {
        let r = remote_trigger(&format!(
            "Call the RemoteTrigger tool once with action \"list_runs\" and trigger_id \"{id}\". Then reply with just: ok"
        ))
        .await;
        store(|d| {
            if !d["runs"].is_object() {
                d["runs"] = json!({});
            }
            d["runs"][&id] = match r {
                Ok(res) => {
                    let rows: Vec<Value> = res[0]["data"].as_array().into_iter().flatten().map(run_row).collect();
                    json!({"fetchedAt": now_ms(), "runs": rows, "summary": res[0]["summary"]})
                }
                Err((_, e)) => json!({"fetchedAt": now_ms(), "error": e}),
            };
        });
        end();
    });
    (StatusCode::ACCEPTED, Json(json!({"ok": true}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stream() {
        let ok = [
            r#"{"type":"system","subtype":"init","tools":["RemoteTrigger"],"session_id":"c38ed915-e811-49a3-8fdc-586f68e11706"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"RemoteTrigger","input":{"action":"list"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"HTTP 200\n{\"data\":[{\"id\":\"trig_1\",\"enabled\":true,\"name\":\"n\",\"cron_expression\":\"0 8 * * 1-5\",\"last_run\":{\"status\":\"ROUTINE_RUN_STATUS_SUCCEEDED\",\"session_id\":\"cse_1\",\"failure_reason\":\"ROUTINE_RUN_FAILURE_REASON_UNSPECIFIED\"}}],\"has_more\":false}"}]}}"#,
            r#"{"type":"result","is_error":false,"result":"ok"}"#,
        ]
        .join("\n");
        let r = parse_stream(&ok).unwrap();
        let x = routine(&r[0]["data"][0]);
        assert_eq!(x["cron"], "0 8 * * 1-5");
        assert_eq!(x["lastRun"]["status"], "succeeded");
        assert!(x["lastRun"]["failure"].is_null());
        assert_eq!(x["url"], "https://claude.ai/code/cse_1");
        let no_tool = r#"{"type":"system","subtype":"init","tools":[]}"#;
        assert!(matches!(parse_stream(no_tool), Err((false, _))));
        let auth = [
            r#"{"type":"system","subtype":"init","tools":["RemoteTrigger"]}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"RemoteTrigger","input":{}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"Not authenticated with a claude.ai account. Run /login and try again."}]}}"#,
        ]
        .join("\n");
        assert!(matches!(parse_stream(&auth), Err((false, _))));
        let b = tool_body("HTTP 200\n{\"data\":[]}\n\n→ no run sessions recorded", false);
        assert_eq!(b["data"], json!([]));
        assert_eq!(b["summary"], "no run sessions recorded");
        let t = tool_body("HTTP 200\n{\"data\":[{\"id\":\"x\",\"api_token_hint\":\"sk-…\",\"n\":{\"accessToken\":1}}]}", false);
        assert!(!t.to_string().contains("token") && !t.to_string().contains("Token"));
        assert_eq!(tool_body("HTTP 404\n{\"error\":\"x\"}", false)["error"].as_str().unwrap().starts_with("HTTP 404"), true);
    }
}
