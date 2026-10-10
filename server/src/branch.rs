//! Branch a chat from one of the agent's messages (API.md "Branch"): `POST /api/t/{sid}/branch {i, text}`.
//!
//! The new agent's conversation is the original transcript up to and including item `i` (an assistant text), then
//! the user's reply. The original session is never touched:
//! 1. its JSONL is copied up to the end of item `i`'s line into `<same project dir>/<new sid>.jsonl` (uuid /
//!    parentUuid chain kept as is, `sessionId` rewritten; the parent's title / colour / queue / continued-in rows are
//!    left out, and tool calls on the cut line itself are dropped so no tool_use is left without its result);
//! 2. the branch's name (a heuristic from the reply, 2-4 words) and a colour that differs from the parent's are
//!    written as `custom-title` / `agent-name` / `agent-color` rows: the same rows `/rename` and `/color` write, so
//!    the fleet UI, History and Claude itself show them at once and keep them;
//! 3. a tab opens in the parent's workspace and folder, `claude --resume <new sid>` starts in it (the parent's model)
//!    and the reply goes in once it is ready (newagent.rs boot queue);
//! 4. find.rs records the branch (`branchOf`), fleet.rs hangs it under its parent (`via: "branch"`);
//! 5. Haiku then names it from its new direction; a better name goes in through the phone-rename path
//!    (`/rename` typed once idle, tab label at once). Never the master: a branch is always a new agent.

use crate::transcript::now_ms;
use crate::{Shared, herdr, json_err};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

pub fn routes() -> Router<Shared> {
    Router::new().route("/api/t/{tid}/branch", post(api_branch))
}

#[derive(Deserialize)]
struct BranchBody {
    /// the assistant text item to branch after
    i: usize,
    /// the user's reply: the branch's first new message
    text: String,
}

/// Transcript rows the branch must not inherit: the parent's name and colour (the branch gets its own), Claude's
/// own title for it, typed-ahead queue rows (they would show as queued messages) and handoff markers.
const SKIP_TYPES: [&str; 7] = ["custom-title", "agent-name", "agent-color", "ai-title", "last-prompt", "queue-operation", "continued-in"];

/// A random v4 uuid (no crate: 16 bytes from the kernel).
fn uuid4() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        let n = now_ms() as u128 ^ (std::process::id() as u128) << 64;
        b = n.to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// What a copy needs: the branch's name and colour.
pub struct Copy<'a> {
    pub old_sid: &'a str,
    pub new_sid: &'a str,
    pub title: &'a str,
    pub color: &'a str,
}

/// The branch transcript: `head` = the parent's JSONL bytes up to and including the cut line (its newline optional).
/// Returns (jsonl, model of the last assistant row) or an error when the cut line is not an assistant message.
pub fn branch_jsonl(head: &[u8], c: &Copy) -> Result<(String, Option<String>), String> {
    let text = String::from_utf8_lossy(head);
    let lines: Vec<&str> = text.split('\n').filter(|l| !l.trim().is_empty()).collect();
    let Some((last, rest)) = lines.split_last() else { return Err("empty transcript".into()) };
    let mut cut: Value = serde_json::from_str(last).map_err(|_| "the message line is not JSON".to_string())?;
    if cut["type"] != "assistant" || cut["uuid"].as_str().is_none() {
        return Err("only an agent message can be branched from".into());
    }
    let from = format!("\"sessionId\":\"{}\"", c.old_sid);
    let to = format!("\"sessionId\":\"{}\"", c.new_sid);
    let mut out = String::with_capacity(head.len() + 512);
    let mut model = None;
    for l in rest {
        // rows are only parsed for their type: everything kept goes out byte for byte (bar the session id)
        let ty = row_type(l);
        if ty.as_deref().is_some_and(|t| SKIP_TYPES.contains(&t)) {
            continue;
        }
        if ty.as_deref() == Some("assistant") {
            if let Some(m) = serde_json::from_str::<Value>(l).ok().and_then(|v| v["message"]["model"].as_str().map(str::to_string)) {
                model = Some(m);
            }
        }
        out.push_str(&l.replace(&from, &to));
        out.push('\n');
    }
    // the cut row: tool calls dropped (their results are not in the branch), the text kept
    if let Some(blocks) = cut["message"]["content"].as_array_mut() {
        let had = blocks.len();
        blocks.retain(|b| b["type"] != "tool_use" && b["type"] != "server_tool_use");
        if blocks.is_empty() {
            return Err("that message is only a tool call".into());
        }
        if blocks.len() != had && cut["message"]["stop_reason"] == "tool_use" {
            cut["message"]["stop_reason"] = json!("end_turn");
        }
    }
    if cut.get("sessionId").is_some() {
        cut["sessionId"] = json!(c.new_sid);
    }
    if let Some(m) = cut["message"]["model"].as_str() {
        model = Some(m.to_string());
    }
    out.push_str(&cut.to_string());
    out.push('\n');
    for row in [
        json!({"type": "custom-title", "customTitle": c.title, "sessionId": c.new_sid}),
        json!({"type": "agent-name", "agentName": c.title, "sessionId": c.new_sid}),
        json!({"type": "agent-color", "agentColor": c.color, "sessionId": c.new_sid}),
    ] {
        out.push_str(&row.to_string());
        out.push('\n');
    }
    Ok((out, model.filter(|m| m.starts_with("claude-") && !m.contains("synthetic"))))
}

/// A row's `type` without parsing the whole (possibly huge) line when it leads, as Claude writes it.
fn row_type(l: &str) -> Option<String> {
    if let Some(r) = l.strip_prefix("{\"type\":\"") {
        return r.split('"').next().map(str::to_string);
    }
    serde_json::from_str::<Value>(l).ok().and_then(|v| v["type"].as_str().map(str::to_string))
}

/// Words that say how, not what ("ok actually let's try X instead" is about X).
const FILLER: &[&str] = &[
    "a", "about", "actually", "again", "also", "am", "an", "and", "any", "are", "as", "at", "be", "but", "by", "can",
    "could", "did", "do", "does", "dont", "don't", "for", "from", "go", "going", "great", "hmm", "how", "i", "i'd",
    "i'm", "if", "in", "instead", "into", "is", "it", "it's", "its", "just", "let", "let's", "lets", "like", "make",
    "maybe", "me", "my", "need", "no", "nope", "not", "now", "of", "ok", "okay", "on", "or", "our", "please", "rather",
    "really", "should", "so", "some", "sure", "that", "the", "their", "them", "then", "there", "these", "this", "those",
    "thanks", "to", "try", "us", "use", "using", "very", "want", "was", "we", "we'll", "we're", "well", "what", "when",
    "where", "which", "why", "will", "with", "would", "yeah", "yes", "you", "your", "cool", "nice", "hey", "hi",
];

/// 2-4 words from the reply's first sentence, Title case: the branch's name until Haiku's is in.
pub fn heuristic_name(reply: &str) -> String {
    let first = reply.split(['\n', '.', '?', '!', ';']).map(str::trim).find(|s| !s.is_empty()).unwrap_or("");
    let words: Vec<String> = first
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-' && c != '+' && c != '#').replace('’', "'"))
        .filter(|w| !w.is_empty() && !w.starts_with("http"))
        .collect();
    let keep: Vec<&String> = words.iter().filter(|w| !FILLER.contains(&w.to_lowercase().as_str())).take(4).collect();
    let pick: Vec<&String> = if keep.len() >= 2 { keep } else { words.iter().take(4).collect() };
    let name = pick
        .iter()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) if w.chars().all(|x| !x.is_uppercase()) => f.to_uppercase().collect::<String>() + c.as_str(),
                _ => w.to_string(), // keep "API", "iOS"
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let name: String = name.chars().take(40).collect();
    if name.trim().is_empty() { "Branch".into() } else { name.trim().to_string() }
}

/// The branch's colour: never the parent's; otherwise the least-used pastel, starting after the parent's.
pub fn branch_color(parent: Option<&str>, used: &[String]) -> String {
    let parent = parent.map(|c| if c == "red" { "pink" } else { c });
    let pick = crate::newagent::PICK;
    let at = parent.and_then(|p| pick.iter().position(|c| *c == p)).map(|k| (k + 1) % pick.len()).unwrap_or(4);
    let mut used: Vec<String> = used.to_vec();
    if let Some(p) = parent {
        // count the parent's colour as taken many times over, so it is only ever picked when nothing else exists
        used.extend(std::iter::repeat_n(p.to_string(), 64));
    }
    crate::newagent::spread(pick[at], &used)
}

async fn api_branch(State(app): State<Shared>, AxPath(tid): AxPath<String>, Json(b): Json<BranchBody>) -> Response {
    if tid.contains('~') || !crate::valid_sid(&tid) {
        return json_err(StatusCode::BAD_REQUEST, "a subagent's transcript can't be branched");
    }
    let reply = b.text.trim().to_string();
    if reply.is_empty() || reply.len() > 20_000 {
        return json_err(StatusCode::BAD_REQUEST, "type a reply (up to 20k chars)");
    }
    let Some(tx) = crate::transcript(&app, &tid) else { return json_err(StatusCode::NOT_FOUND, "no such transcript") };
    let (off, path, t_cwd, t_title, t_color) = {
        let t = tx.lock().unwrap();
        let Some(it) = t.items.iter().find(|x| x.i == b.i) else { return json_err(StatusCode::NOT_FOUND, "no such message") };
        if it.k != "text" {
            return json_err(StatusCode::BAD_REQUEST, "only an agent message can be branched from");
        }
        (it.off, t.path.clone(), t.cwd.clone(), t.title().map(str::to_string), t.color.clone())
    };
    // the parent: live (its pane, workspace, title) or asleep (where it last lived)
    let st: Value = serde_json::from_str(&app.inner.lock().unwrap().state).unwrap_or(Value::Null);
    let agents = st["agents"].as_array().cloned().unwrap_or_default();
    let live = agents.iter().find(|a| a["session"].as_str() == Some(tid.as_str()) && a["agentId"].is_null()).cloned();
    let place = crate::find::place(&tid);
    let cwd = live
        .as_ref()
        .and_then(|a| a["cwd"].as_str().map(str::to_string))
        .or(t_cwd)
        .or_else(|| place.as_ref().and_then(|p| p.cwd.clone()));
    let Some(cwd) = cwd.filter(|c| Path::new(c).is_dir()) else { return json_err(StatusCode::CONFLICT, "its folder no longer exists") };
    let live_ws: Vec<String> = st["workspaces"].as_array().into_iter().flatten().filter_map(|w| w["ws"].as_str().map(str::to_string)).collect();
    let ws = live
        .as_ref()
        .and_then(|a| a["ws"].as_str().map(str::to_string))
        .or_else(|| place.as_ref().and_then(|p| p.ws.clone()).filter(|w| live_ws.contains(w)))
        .or_else(|| crate::fleet::pick_ws(&app, &cwd));
    let parent_name = match &live {
        Some(a) if a["home"] == true => crate::home_name(),
        Some(a) => a["short"].as_str().or(a["title"].as_str()).unwrap_or("").to_string(),
        None => place.as_ref().and_then(|p| p.name.clone().or(p.nick.clone())).or(t_title).unwrap_or_default(),
    };
    let parent_color = live.as_ref().and_then(|a| a["color"].as_str().map(str::to_string)).or(t_color);
    let used: Vec<String> = agents
        .iter()
        .filter(|a| a["home"] != true && a["service"].is_null())
        .filter_map(|a| a["color"].as_str().map(str::to_string))
        .collect();
    let title = heuristic_name(&reply);
    let color = branch_color(parent_color.as_deref(), &used);
    let new_sid = uuid4();

    // the copy
    let copy = {
        let (path, tid, new_sid, title, color) = (path.clone(), tid.clone(), new_sid.clone(), title.clone(), color.clone());
        tokio::task::spawn_blocking(move || -> Result<(std::path::PathBuf, Option<String>), String> {
            let bytes = std::fs::read(&path).map_err(|e| format!("can't read the transcript: {e}"))?;
            let start = off as usize;
            if start >= bytes.len() {
                return Err("the message is not in the transcript".into());
            }
            let end = bytes[start..].iter().position(|&c| c == b'\n').map(|k| start + k + 1).unwrap_or(bytes.len());
            let (jsonl, model) = branch_jsonl(&bytes[..end], &Copy { old_sid: &tid, new_sid: &new_sid, title: &title, color: &color })?;
            let dst = path.with_file_name(format!("{new_sid}.jsonl"));
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&dst).map_err(|e| format!("can't write the branch: {e}"))?;
            std::io::Write::write_all(&mut f, jsonl.as_bytes()).map_err(|e| format!("can't write the branch: {e}"))?;
            Ok((dst, model))
        })
        .await
    };
    let (dst, model) = match copy {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return json_err(StatusCode::CONFLICT, e),
        Err(_) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, "copy failed"),
    };
    app.session_paths.lock().unwrap().insert(new_sid.clone(), dst.clone());
    crate::find::branched(&new_sid, &tid, &parent_name, b.i);

    let (pane, tab, wsid) = match crate::fleet::new_tab(ws.as_deref(), &cwd, &title).await {
        Ok(x) => x,
        Err(r) => {
            let _ = std::fs::remove_file(&dst);
            return r;
        }
    };
    crate::find::resumed(&new_sid, &pane);
    crate::newagent::labelled(&pane); // named already: auto-name leaves it alone
    crate::newagent::boot_begin(&pane, Some(reply.clone()));
    let mut args = vec!["--resume".to_string(), new_sid.clone(), "--permission-mode".into(), "auto".into()];
    args.push("--model".into());
    args.push(model.unwrap_or_else(|| crate::newagent::DEFAULT_MODEL.into()));
    // the herdr agent name: "br-…", never `master*` (that prefix marks the home agent)
    let name = crate::agent_slug(&format!("br {title}"));
    {
        let (app, pane, title, parent_name) = (app.clone(), pane.clone(), title.clone(), parent_name.clone());
        let last = {
            let t = tx.lock().unwrap();
            t.items.iter().find(|x| x.i == b.i).and_then(|x| x.text.clone()).unwrap_or_default()
        };
        tokio::spawn(async move {
            if let Err(e) = crate::start_claude(&pane, &name, args).await {
                eprintln!("branch start {pane}: {e}");
            }
            app.refresh.notify_one();
            crate::newagent::finish_boot(&app, &pane).await;
            rename_later(&app, &pane, &title, &parent_name, &last, &reply).await;
        });
    }
    app.refresh.notify_one();
    Json(json!({"pane": pane, "tab": tab, "ws": wsid, "sid": new_sid, "title": title, "color": color, "parent": tid})).into_response()
}

/// Haiku names the branch after its new direction; a different name goes in as a phone rename would.
async fn rename_later(app: &Shared, pane: &str, first: &str, parent: &str, msg: &str, reply: &str) {
    let Ok(name) = haiku_branch_name(parent, msg, reply).await else { return };
    if name.eq_ignore_ascii_case(first) {
        return;
    }
    // the agent shows up in the state once herdr reports it
    for _ in 0..60 {
        if app.inner.lock().unwrap().status.contains_key(pane) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let tab = {
        let mut i = app.inner.lock().unwrap();
        if i.meta.contains_key(pane) || !i.status.contains_key(pane) {
            return; // the user renamed it already, or it is gone
        }
        let st: Value = serde_json::from_str(&i.state).unwrap_or(Value::Null);
        let a = st["agents"].as_array().into_iter().flatten().find(|a| a["pane"] == pane).cloned().unwrap_or(Value::Null);
        // never the master, never a name the user gave it since
        if a["home"] == true || a["name"].as_str().is_some_and(|n| n.starts_with("master")) || a["title"].as_str().is_some_and(|t| t != first) {
            return;
        }
        i.meta.insert(pane.to_string(), crate::MetaReq { title: Some(name.clone()), color: None, applied: 0 });
        i.pane_tabs.get(pane).cloned()
    };
    crate::patch_agent(app, pane, |a| {
        a["title"] = json!(name);
        a["autoNamed"] = json!(now_ms());
    });
    if let Some(tab) = tab {
        let _ = herdr::quick("tab.rename", json!({"tab_id": tab, "label": name})).await;
    }
    app.refresh.notify_one();
}

async fn haiku_branch_name(parent: &str, msg: &str, reply: &str) -> Result<String, String> {
    if let Ok(fake) = std::env::var("SHEPRD_FAKE_AUTONAME") {
        return Ok(fake.split('|').next().unwrap_or("").to_string());
    }
    let tail: String = {
        let c: Vec<char> = msg.chars().collect();
        c[c.len().saturating_sub(500)..].iter().collect()
    };
    let reply: String = reply.chars().take(600).collect();
    let prompt = format!(
        "the user went back to an earlier message in a chat with a coding agent and replied differently, starting a new \
         branch of the chat. Name the branch after its NEW direction (what the user's reply asks for), not the old chat. \
         Reply with JSON only, no prose: {{\"title\": \"<2-4 word title, Title case, no quotes or emoji>\", \"color\": \"blue\"}}.\n\
         Old chat: {parent}\nAgent's message (end):\n<<<\n{tail}\n>>>\nthe user's reply:\n<<<\n{reply}\n>>>"
    );
    let v = crate::newagent::run_haiku(None, &prompt, Duration::from_secs(60)).await?;
    let text = v["result"].as_str().unwrap_or("");
    let (title, _) = crate::newagent::parse_reply(text).ok_or("unusable reply")?;
    if title.split_whitespace().count() > 5 {
        return Err("too long".into());
    }
    Ok(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const NEW: &str = "bbbbbbbb-0000-4000-8000-000000000002";

    fn fixture() -> String {
        [
            format!(r#"{{"type":"custom-title","customTitle":"Old Name","sessionId":"{OLD}"}}"#),
            format!(r#"{{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"{OLD}","message":{{"role":"user","content":"hi"}}}}"#),
            format!(r#"{{"type":"agent-color","agentColor":"green","sessionId":"{OLD}"}}"#),
            format!(r#"{{"type":"queue-operation","operation":"enqueue","content":"later","sessionId":"{OLD}"}}"#),
            format!(r#"{{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"{OLD}","message":{{"model":"claude-opus-5-5","stop_reason":"tool_use","content":[{{"type":"text","text":"Two options."}},{{"type":"tool_use","id":"t1","name":"Bash","input":{{}}}}]}}}}"#),
        ]
        .join("\n")
            + "\n"
    }

    #[test]
    fn copies_up_to_the_message() {
        let (out, model) = branch_jsonl(fixture().as_bytes(), &Copy { old_sid: OLD, new_sid: NEW, title: "Try Websockets", color: "cyan" }).unwrap();
        assert_eq!(model.as_deref(), Some("claude-opus-5-5"));
        let rows: Vec<Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let types: Vec<&str> = rows.iter().map(|r| r["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["user", "assistant", "custom-title", "agent-name", "agent-color"]);
        assert!(rows.iter().all(|r| r["sessionId"] == NEW));
        assert!(!out.contains(OLD));
        // chain kept; the dangling tool call is gone
        assert_eq!(rows[1]["parentUuid"], "u1");
        assert_eq!(rows[1]["message"]["content"].as_array().unwrap().len(), 1);
        assert_eq!(rows[1]["message"]["stop_reason"], "end_turn");
        assert_eq!(rows[2]["customTitle"], "Try Websockets");
        assert_eq!(rows[4]["agentColor"], "cyan");
    }

    #[test]
    fn refuses_non_assistant_cut() {
        let f = fixture();
        let head: String = f.lines().take(2).collect::<Vec<_>>().join("\n");
        assert!(branch_jsonl(head.as_bytes(), &Copy { old_sid: OLD, new_sid: NEW, title: "x", color: "blue" }).is_err());
    }

    #[test]
    fn names_from_the_reply() {
        assert_eq!(heuristic_name("ok actually let's try websockets instead of polling"), "Websockets Polling");
        assert_eq!(heuristic_name("Rewrite the parser in Rust. Then bench it."), "Rewrite Parser Rust");
        assert_eq!(heuristic_name("Use the iOS API for haptics and sound"), "iOS API Haptics Sound");
        assert_eq!(heuristic_name("why?"), "Why");
        assert_eq!(heuristic_name("   "), "Branch");
    }

    #[test]
    fn color_differs_from_parent() {
        assert_eq!(branch_color(Some("blue"), &[]), "purple");
        assert_eq!(branch_color(Some("blue"), &["purple".into()]), "pink");
        let all: Vec<String> = crate::newagent::PICK.iter().map(|c| c.to_string()).collect();
        for p in crate::newagent::PICK {
            assert_ne!(branch_color(Some(p), &all), p);
        }
        assert_ne!(branch_color(Some("red"), &[]), "pink");
        assert_eq!(uuid4().len(), 36);
    }
}
