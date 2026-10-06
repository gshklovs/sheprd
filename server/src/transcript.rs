//! Incremental parser for Claude Code transcript JSONL files.
//!
//! A transcript is parsed once, then only newly appended bytes are parsed on
//! each refresh. Items hold no image data: images are addressed by the byte
//! offset of their line plus their position in it, and decoded on request.

use base64::Engine;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const RESULT_CUT: usize = 3000;
const INPUT_STR_CUT: usize = 2000;
const LAST_CUT: usize = 240;

#[derive(Serialize, Clone, Default)]
pub struct Item {
    pub i: usize,
    pub ts: i64,
    pub k: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "agentId", skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cut: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    /// `kind: "steer"`: the sending agent's name (its pane is `agentId`, when known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Byte offset of the source line, for full-text and image lookups.
    #[serde(skip)]
    pub off: u64,
}

#[derive(Serialize, Clone)]
pub struct Last {
    pub role: &'static str,
    pub text: String,
    pub ts: i64,
}

/// The agent's own task list (TodoWrite, or TaskCreate/TaskUpdate): progress for Home's In-flight tiles.
#[derive(Serialize, Clone, Default, Debug, PartialEq)]
pub struct Todo {
    pub done: usize,
    pub total: usize,
    /// activeForm (else content/subject) of the item in progress
    pub cur: Option<String>,
    pub ts: i64,
}

#[derive(Serialize, Clone)]
pub struct Recap {
    pub text: String,
    pub ts: i64,
    pub i: usize,
}

/// A message typed while the agent was busy (`queue-operation` rows).
/// `hidden` entries (agent hand-backs, task notifications) only keep the queue
/// positions right, since `dequeue` rows carry no text.
#[derive(Clone)]
pub struct Queued {
    pub text: String,
    pub ts: i64,
    pub hidden: bool,
}

/// A `run_in_background` Bash call.
#[derive(Serialize, Clone)]
pub struct Job {
    pub id: String,
    pub cmd: String,
    pub desc: Option<String>,
    pub out: String,
    pub started: i64,
    pub ended: Option<i64>,
    pub status: &'static str,
    pub i: usize,
}

/// Edits of one file by this transcript; `items` are the tool items, oldest first.
#[derive(Clone, Default)]
pub struct Touch {
    pub edits: usize,
    pub writes: usize,
    pub last_i: usize,
    pub last_ts: i64,
    pub items: Vec<usize>,
}

pub struct Transcript {
    pub tid: String,
    pub path: PathBuf,
    offset: u64,
    pub items: Vec<Item>,
    tool_idx: HashMap<String, usize>,
    pub ai_title: Option<String>,
    pub custom_title: Option<String>,
    /// First line of the first real user prompt, cut short: the title until Claude names the session.
    pub first_prompt: Option<String>,
    pub color: Option<String>,
    pub cwd: Option<String>,
    pub last: Option<Last>,
    pub last_ts: i64,
    /// Bumped whenever items change, so watchers can cheaply detect growth.
    pub size: u64,
    /// Index of the newest assistant `text` item.
    pub answer_i: Option<usize>,
    pub recap: Option<Recap>,
    pub queue: Vec<Queued>,
    /// Tool/agent items still waiting for their result, oldest first.
    open: Vec<usize>,
    pub jobs: Vec<Job>,
    pub touched: HashMap<String, Touch>,
    /// Context fill from the newest main-loop model response (`ctx` when the mod has not reported).
    pub ctx_est: Option<CtxEst>,
    /// newest task list (TodoWrite replaces it whole; TaskCreate/TaskUpdate edit `tasks`)
    pub todo: Option<Todo>,
    tasks: Vec<(String, String, String)>, // (id, status, label)
}

/// What the transcript says about the context window: the newest response's prompt size.
#[derive(Default, Clone, Debug)]
pub struct CtxEst {
    pub model: String,
    /// input + cache read + cache write tokens of the newest response; None right after a compaction
    pub tokens: Option<u64>,
    /// largest prompt seen (evidence of a 1M window)
    pub max: u64,
    pub ts: i64,
    /// API-equivalent $ of the main loop's responses
    pub cost: f64,
    last_req: String,
}

impl Transcript {
    pub fn new(tid: String, path: PathBuf) -> Self {
        Transcript {
            tid,
            path,
            offset: 0,
            items: Vec::new(),
            tool_idx: HashMap::new(),
            ai_title: None,
            custom_title: None,
            first_prompt: None,
            color: None,
            cwd: None,
            last: None,
            last_ts: 0,
            size: 0,
            answer_i: None,
            recap: None,
            queue: Vec::new(),
            open: Vec::new(),
            jobs: Vec::new(),
            touched: HashMap::new(),
            ctx_est: None,
            todo: None,
            tasks: Vec::new(),
        }
    }

    pub fn title(&self) -> Option<&str> {
        self.custom_title.as_deref().or(self.ai_title.as_deref())
    }

    /// Parses any bytes appended since the last call. Returns true if items changed.
    pub fn refresh(&mut self) -> bool {
        let Ok(meta) = std::fs::metadata(&self.path) else { return false };
        let len = meta.len();
        if len == self.offset {
            return false;
        }
        if len < self.offset {
            // Rewritten from scratch; start over.
            *self = Transcript::new(std::mem::take(&mut self.tid), std::mem::take(&mut self.path));
        }
        let Ok(mut f) = File::open(&self.path) else { return false };
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        if f.take(len - self.offset).read_to_end(&mut buf).is_err() {
            return false;
        }
        let before = self.items.len();
        let mut pos = 0usize;
        while let Some(nl) = buf[pos..].iter().position(|&b| b == b'\n') {
            let line = &buf[pos..pos + nl];
            let off = self.offset + pos as u64;
            if !line.is_empty() {
                if let Ok(v) = serde_json::from_slice::<Value>(line) {
                    self.ingest(&v, off);
                }
            }
            pos += nl + 1;
        }
        self.offset += pos as u64;
        self.size = self.offset;
        self.items.len() != before || pos > 0
    }

    fn push(&mut self, mut item: Item) -> usize {
        item.i = self.items.len();
        if item.ts > self.last_ts {
            self.last_ts = item.ts;
        }
        match item.k {
            "user" => {
                if let Some(t) = &item.text {
                    if !t.starts_with("[Request interrupted") {
                        self.last = Some(Last { role: "user", text: snippet(t), ts: item.ts });
                        if self.first_prompt.is_none() && !t.starts_with("! ") {
                            self.first_prompt = prompt_title(t);
                        }
                    }
                    // A queued message that reached the model is no longer queued.
                    let t = t.trim();
                    self.queue.retain(|q| q.text != t);
                    if t.starts_with("[Request interrupted") {
                        self.open.clear();
                        self.queue.clear();
                    }
                }
            }
            "text" => {
                if let Some(t) = &item.text {
                    self.last = Some(Last { role: "assistant", text: snippet(t), ts: item.ts });
                }
                self.answer_i = Some(item.i);
            }
            "tool" | "agent" => self.open.push(item.i),
            "result" => {
                let id = item.id.as_deref();
                self.open.retain(|&ix| self.items[ix].id.as_deref() != id);
            }
            "note" if item.kind == Some("summary") => {
                self.recap = Some(Recap { text: item.text.clone().unwrap_or_default(), ts: item.ts, i: item.i });
            }
            _ => {}
        }
        self.items.push(item);
        self.items.len() - 1
    }

    fn ingest(&mut self, v: &Value, off: u64) {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        let ts = v.get("timestamp").and_then(Value::as_str).map(parse_ts).unwrap_or(0);
        if self.cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                self.cwd = Some(c.to_string());
            }
        }
        match ty {
            "ai-title" => self.ai_title = str_field(v, "aiTitle"),
            "custom-title" => self.custom_title = str_field(v, "customTitle").or_else(|| str_field(v, "title")),
            "agent-name" => {
                if self.custom_title.is_none() {
                    self.custom_title = str_field(v, "agentName");
                }
            }
            "agent-color" => self.color = str_field(v, "agentColor").filter(|c| !c.is_empty() && c != "default"),
            "queue-operation" => self.queue_op(v, ts),
            "user" => self.ingest_user(v, ts, off),
            "assistant" => {
                if v.get("isSidechain").and_then(Value::as_bool) != Some(true) {
                    self.note_usage(v, ts);
                }
                self.ingest_assistant(v, ts, off);
                if v["message"].get("stop_reason").and_then(Value::as_str) == Some("end_turn") {
                    self.turn_over();
                }
            }
            "system" => {
                let sub = v.get("subtype").and_then(Value::as_str).unwrap_or("");
                let content = v.get("content").and_then(Value::as_str).unwrap_or("");
                match sub {
                    "compact_boundary" => {
                        if let Some(e) = &mut self.ctx_est {
                            e.tokens = None;
                            e.ts = ts;
                        }
                        self.note("Conversation compacted", "compact", ts, off)
                    }
                    "local_command" => {
                        if let Some(cmd) = command_line(content) {
                            self.note(&cmd, "command", ts, off)
                        }
                    }
                    "away_summary" => self.note(content, "summary", ts, off),
                    _ => {}
                }
            }
            "attachment" => {
                let a = &v["attachment"];
                if a.get("type").and_then(Value::as_str) == Some("queued_command") {
                    let p = a.get("prompt").and_then(Value::as_str).unwrap_or("");
                    if let Some((from, body)) = agent_report(p) {
                        let it = Item { ts, k: "user", text: Some(body), kind: Some("report"), agent_id: Some(from), off, ..Default::default() };
                        self.push(it);
                    } else if p.starts_with("<agent-message") {
                        let from = p.split("from=\"").nth(1).and_then(|r| r.split('"').next()).map(str::to_string);
                        self.note_for("Subagent reported back", "handback", ts, off, from);
                    } else if p.starts_with("<task-notification>") {
                        self.task_note(p, ts, off);
                    } else if !p.trim().is_empty() {
                        self.push(Item { ts, k: "user", text: Some(strip_reminders(p)), off, ..Default::default() });
                    }
                }
            }
            _ => {}
        }
    }

    /// Tracks messages typed mid-turn: enqueue appends, remove/popAll drop the
    /// first equal text, dequeue pops the front.
    fn queue_op(&mut self, v: &Value, ts: i64) {
        let text = v.get("content").and_then(Value::as_str).unwrap_or("").trim();
        match v.get("operation").and_then(Value::as_str).unwrap_or("") {
            "enqueue" if !text.is_empty() => {
                let hidden = text.starts_with('<');
                self.queue.push(Queued { text: if hidden { String::new() } else { text.to_string() }, ts, hidden });
            }
            // popAll: ↑ pulled the queue back into the input box (one row per message)
            "remove" | "popAll" => {
                if let Some(p) = self.queue.iter().position(|q| q.text == text) {
                    self.queue.remove(p);
                }
            }
            "dequeue" if !self.queue.is_empty() => {
                self.queue.remove(0);
            }
            _ => {}
        }
        if self.queue.len() > 50 {
            self.queue.drain(..self.queue.len() - 50);
        }
    }

    /// The turn ended (or was interrupted): nothing is running or queued any more.
    fn turn_over(&mut self) {
        self.open.clear();
        self.queue.clear();
    }

    /// Visible queued messages.
    pub fn queued(&self) -> Vec<Value> {
        self.queue.iter().filter(|q| !q.hidden).map(|q| serde_json::json!({"text": q.text, "ts": q.ts})).collect()
    }

    /// What the agent is doing right now: the newest unresolved tool/agent call,
    /// else "Thinking"/"Writing" from the last item. `(label, ts, i)`.
    pub fn doing(&self) -> Option<(String, i64, usize)> {
        if let Some(&ix) = self.open.last() {
            let it = &self.items[ix];
            let label = if it.k == "agent" {
                format!("Subagent · {}", it.desc.as_deref().unwrap_or(""))
            } else {
                let name = it.name.as_deref().unwrap_or("tool");
                let name = name.rsplit("__").next().unwrap_or(name);
                let sum = it.summary.as_deref().unwrap_or("");
                if sum.is_empty() { name.to_string() } else { format!("{name} · {sum}") }
            };
            return Some((short(&label, 80), it.ts, ix));
        }
        let it = self.items.iter().rev().find(|it| it.k != "note")?;
        let label = if it.k == "text" { "Writing" } else { "Thinking" };
        Some((label.into(), it.ts, it.i))
    }

    /// Index (into `items`) of the newest tool/agent call still waiting for its result.
    pub fn open_last(&self) -> Option<usize> {
        self.open.last().copied()
    }

    /// True when the newest unresolved call is the tool `name`.
    pub fn waiting_on(&self, name: &str) -> bool {
        self.open.last().is_some_and(|&ix| self.items[ix].name.as_deref() == Some(name))
    }

    /// Text of the last visible item when it is an assistant answer (chips source).
    pub fn last_answer(&self) -> Option<&str> {
        let it = self.items.iter().rev().find(|it| it.k != "thinking" && it.k != "note")?;
        if it.k == "text" { it.text.as_deref() } else { None }
    }

    fn note(&mut self, text: &str, kind: &'static str, ts: i64, off: u64) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.push(Item { ts, k: "note", text: Some(cut(text, 600).0), kind: Some(kind), off, ..Default::default() });
    }

    fn task_note(&mut self, s: &str, ts: i64, off: u64) {
        let summary = tag(s, "summary").unwrap_or_else(|| "Background task finished".into());
        if let Some(id) = tag(s, "task-id") {
            let status = match tag(s, "status").as_deref() {
                Some("failed") => "failed",
                Some("killed" | "stopped" | "cancelled") => "killed",
                _ => "completed",
            };
            self.close_job(&id, status, ts);
        }
        self.note_for(&summary, "task", ts, off, tag(s, "task-id"));
    }

    fn close_job(&mut self, id: &str, status: &'static str, ts: i64) {
        if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id && j.ended.is_none()) {
            j.ended = Some(ts);
            j.status = status;
        }
    }

    /// A note that refers to a subagent: carries its id and transcript tid so the
    /// client can link it (only when that id names a subagent of this session).
    fn note_for(&mut self, text: &str, kind: &'static str, ts: i64, off: u64, agent: Option<String>) {
        self.note(text, kind, ts, off);
        let Some(aid) = agent.filter(|a| !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric())) else { return };
        if !subagent_path(&self.path, &aid).exists() {
            return;
        }
        let sid = self.tid.split('~').next().unwrap_or("").to_string();
        if let Some(last) = self.items.last_mut().filter(|it| it.k == "note") {
            last.agent_id = Some(aid.clone());
            last.sub = Some(format!("{sid}~{aid}"));
        }
    }

    fn ingest_user(&mut self, v: &Value, ts: i64, off: u64) {
        let is_meta = v.get("isMeta").and_then(Value::as_bool).unwrap_or(false);
        let content = &v["message"]["content"];
        if let Some(s) = content.as_str() {
            if is_meta {
                return;
            }
            self.user_string(s, ts, off);
            return;
        }
        let Some(blocks) = content.as_array() else { return };
        let mut img_n = 0usize;
        let mut texts: Vec<String> = Vec::new();
        let mut images: Vec<String> = Vec::new();
        let tool_result_meta = v.get("toolUseResult");
        for b in blocks {
            match b.get("type").and_then(Value::as_str).unwrap_or("") {
                "tool_result" => {
                    let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("").to_string();
                    let mut text = String::new();
                    let mut imgs = Vec::new();
                    match &b["content"] {
                        Value::String(s) => text.push_str(s),
                        Value::Array(parts) => {
                            for p in parts {
                                match p.get("type").and_then(Value::as_str) {
                                    Some("text") => {
                                        if !text.is_empty() {
                                            text.push('\n');
                                        }
                                        text.push_str(p.get("text").and_then(Value::as_str).unwrap_or(""));
                                    }
                                    Some("image") => {
                                        imgs.push(self.img_url(off, img_n));
                                        img_n += 1;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                    let err = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                    // Link Agent calls to the subagent they launched.
                    if let Some(aid) = tool_result_meta.and_then(|m| m.get("agentId")).and_then(Value::as_str) {
                        if let Some(&ix) = self.tool_idx.get(&id) {
                            if self.items[ix].k == "agent" && self.items[ix].agent_id.is_none() {
                                let sid = self.tid.split('~').next().unwrap_or("").to_string();
                                self.items[ix].agent_id = Some(aid.to_string());
                                self.items[ix].sub = Some(format!("{sid}~{aid}"));
                            }
                        }
                    }
                    if let Some(&ix) = self.tool_idx.get(&id) {
                        self.maybe_job(ix, &text, ts);
                    }
                    let (t, was_cut) = cut(&text, RESULT_CUT);
                    self.push(Item {
                        ts,
                        k: "result",
                        id: Some(id),
                        text: Some(t),
                        err: Some(err),
                        cut: Some(was_cut),
                        images: if imgs.is_empty() { None } else { Some(imgs) },
                        off,
                        ..Default::default()
                    });
                }
                "text" => {
                    let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                    if !is_meta {
                        texts.push(t.to_string());
                    } else if let Some(steer) = t.strip_prefix(PHONE_STEER) {
                        // a phone steer the mod appended into a running turn (modbridge)
                        texts.push(steer.to_string());
                    }
                }
                "image" => {
                    images.push(self.img_url(off, img_n));
                    img_n += 1;
                }
                _ => {}
            }
        }
        if !texts.is_empty() || !images.is_empty() {
            let joined = strip_reminders(&texts.join("\n"));
            if joined.trim().is_empty() && images.is_empty() {
                return;
            }
            self.push_user(ts, joined, if images.is_empty() { None } else { Some(images) }, off);
        }
    }

    /// Registers a background Bash job from its launch result
    /// ("running in background with ID: <id>. Output is being written to: <path>").
    fn maybe_job(&mut self, ix: usize, result: &str, ts: i64) {
        let it = &self.items[ix];
        if it.name.as_deref() != Some("Bash") {
            return;
        }
        // Only the harness's own launch line counts: the result starts with it. Output that merely quotes such a line (e.g. printing another
        // transcript) must not register a phantom job.
        let Some(rest) = result.trim_start().strip_prefix("Command running in background with ID: ") else { return };
        let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        let out = rest
            .split("Output is being written to: ")
            .nth(1)
            .map(|p| p.split_whitespace().next().unwrap_or("").trim_end_matches('.').to_string())
            .unwrap_or_default();
        if id.is_empty() || self.jobs.iter().any(|j| j.id == id) {
            return;
        }
        let input = it.input.as_ref();
        let cmd = input.and_then(|i| i.get("command")).and_then(Value::as_str).unwrap_or("");
        let desc = input.and_then(|i| i.get("description")).and_then(Value::as_str).map(str::to_string);
        let started = if it.ts > 0 { it.ts } else { ts };
        self.jobs.push(Job { id, cmd: cut(cmd, 500).0, desc, out, started, ended: None, status: "running", i: ix });
    }

    fn user_string(&mut self, s: &str, ts: i64, off: u64) {
        let t = s.trim_start();
        if t.starts_with("<task-notification>") {
            return self.task_note(t, ts, off);
        }
        if t.starts_with("<command-name>") || t.starts_with("<command-message>") {
            if let Some(cmd) = command_line(t) {
                self.note(&cmd, "command", ts, off);
            }
            return;
        }
        if t.starts_with("<local-command-stdout>") || t.starts_with("<local-command-stderr>") {
            let inner = tag(t, "local-command-stdout").or_else(|| tag(t, "local-command-stderr")).unwrap_or_default();
            return self.note(&inner, "info", ts, off);
        }
        if t.starts_with("<bash-input>") {
            let cmd = tag(t, "bash-input").unwrap_or_default();
            self.push(Item { ts, k: "user", text: Some(format!("! {cmd}")), off, ..Default::default() });
            return;
        }
        if t.starts_with("<bash-stdout>") || t.starts_with("<bash-stderr>") {
            let out = format!(
                "{}{}",
                tag(t, "bash-stdout").unwrap_or_default(),
                tag(t, "bash-stderr").unwrap_or_default()
            );
            return self.note(&out, "info", ts, off);
        }
        // agent → agent: a subagent's hand-back report, or a prompt pasted in by another agent (herdr)
        if let Some((from, body)) = agent_report(t) {
            let it = Item { ts, k: "user", text: Some(body), kind: Some("report"), agent_id: Some(from), off, ..Default::default() };
            self.push(it);
            return;
        }
        let text = strip_reminders(s);
        if text.trim().is_empty() {
            return;
        }
        if let Some(body) = unwrap_pasted(&text) {
            // a long `herdr agent prompt` arrives as a paste; one that names its sender is that agent's message
            if steer_from(&body).is_some() {
                return self.push_user(ts, body, None, off);
            }
            self.push(Item { ts, k: "user", text: Some(body), kind: Some("paste"), off, ..Default::default() });
            return;
        }
        self.push_user(ts, text, None, off);
    }

    /// A user message: a prompt a plugin submitted loses Claude's wrapper; one another agent sent through
    /// `/steer` with `from` becomes `kind: "steer"` with its sender.
    fn push_user(&mut self, ts: i64, text: String, images: Option<Vec<String>>, off: u64) {
        let text = unwrap_plugin(&text).unwrap_or(text);
        let it = match steer_from(&text) {
            Some((pane, name, body)) => {
                Item { ts, k: "user", text: Some(body), kind: Some("steer"), agent_id: pane, from: Some(name), images, off, ..Default::default() }
            }
            None => Item { ts, k: "user", text: Some(text), images, off, ..Default::default() },
        };
        self.push(it);
    }

    /// Context estimate from a model response's `usage`. Claude writes one row per content block of a
    /// response, all with the same prompt size; cost counts each request once (like usage.rs).
    fn note_usage(&mut self, v: &Value, ts: i64) {
        let m = &v["message"];
        let u = &m["usage"];
        let model = m["model"].as_str().unwrap_or("");
        if !u.is_object() || model.is_empty() || model == "<synthetic>" {
            return;
        }
        let n = |k: &str| u[k].as_u64().unwrap_or(0);
        let tokens = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
        if tokens == 0 {
            return;
        }
        let req = v["requestId"].as_str().or(m["id"].as_str()).unwrap_or("");
        let e = self.ctx_est.get_or_insert_with(CtxEst::default);
        if req.is_empty() || req != e.last_req {
            e.cost += crate::usage::cost_of(model, u);
            e.last_req = req.to_string();
        }
        e.model = model.to_string();
        e.tokens = Some(tokens);
        e.max = e.max.max(tokens);
        e.ts = ts;
    }

    fn ingest_assistant(&mut self, v: &Value, ts: i64, off: u64) {
        let Some(blocks) = v["message"]["content"].as_array() else { return };
        for b in blocks {
            match b.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                    if !t.trim().is_empty() {
                        self.push(Item { ts, k: "text", text: Some(t.to_string()), off, ..Default::default() });
                    }
                }
                "thinking" => {
                    let t = b.get("thinking").and_then(Value::as_str).unwrap_or("");
                    if !t.trim().is_empty() {
                        self.push(Item { ts, k: "thinking", text: Some(t.to_string()), off, ..Default::default() });
                    }
                }
                "tool_use" => {
                    let id = b.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let input = &b["input"];
                    let tname = name.clone();
                    let item = if name == "Agent" || name == "Task" {
                        Item {
                            ts,
                            k: "agent",
                            id: Some(id.clone()),
                            name: Some(name),
                            desc: str_field(input, "description"),
                            agent_type: str_field(input, "subagent_type").or(Some("general-purpose".into())),
                            prompt: str_field(input, "prompt").map(|p| cut(&p, INPUT_STR_CUT).0),
                            off,
                            ..Default::default()
                        }
                    } else {
                        Item {
                            ts,
                            k: "tool",
                            id: Some(id.clone()),
                            summary: Some(tool_summary(&name, input)),
                            name: Some(name),
                            input: Some(trim_value(input, 0)),
                            off,
                            ..Default::default()
                        }
                    };
                    if v.get("isSidechain").and_then(Value::as_bool) != Some(true) {
                        self.track_todo(&tname, input, ts);
                    }
                    let ix = self.push(item);
                    self.tool_idx.insert(id, ix);
                    self.track_tool(ix, input, ts);
                }
                _ => {}
            }
        }
    }

    /// Task-list progress: TodoWrite sends the whole list; TaskCreate adds (ids count up from 1 per session),
    /// TaskUpdate changes a status ("deleted" drops it). An all-done list is kept (the tile shows 5/5).
    fn track_todo(&mut self, name: &str, input: &Value, ts: i64) {
        let label = |x: &Value| str_field(x, "activeForm").or_else(|| str_field(x, "content")).or_else(|| str_field(x, "subject")).unwrap_or_default();
        match name {
            "TodoWrite" => {
                let Some(list) = input.get("todos").and_then(Value::as_array) else { return };
                self.tasks = list.iter().enumerate()
                    .map(|(k, x)| ((k + 1).to_string(), str_field(x, "status").unwrap_or_default(), label(x)))
                    .collect();
            }
            "TaskCreate" => {
                let id = (self.tasks.iter().filter_map(|t| t.0.parse::<usize>().ok()).max().unwrap_or(0) + 1).to_string();
                self.tasks.push((id, "pending".into(), label(input)));
            }
            "TaskUpdate" => {
                let id = input.get("taskId").map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).unwrap_or_default();
                let st = str_field(input, "status");
                if st.as_deref() == Some("deleted") {
                    self.tasks.retain(|t| t.0 != id);
                } else if let Some(t) = self.tasks.iter_mut().find(|t| t.0 == id) {
                    if let Some(st) = st { t.1 = st; }
                    if let Some(l) = str_field(input, "activeForm").or_else(|| str_field(input, "subject")) { t.2 = l; }
                }
            }
            _ => return,
        }
        self.todo = (!self.tasks.is_empty()).then(|| Todo {
            done: self.tasks.iter().filter(|t| t.1 == "completed").count(),
            total: self.tasks.len(),
            cur: self.tasks.iter().find(|t| t.1 == "in_progress").map(|t| t.2.clone()).filter(|l| !l.is_empty()),
            ts,
        });
    }

    /// File edits (for the files-touched index) and background-job kills.
    fn track_tool(&mut self, ix: usize, input: &Value, ts: i64) {
        let name = self.items[ix].name.as_deref().unwrap_or("");
        let (edits, writes) = match name {
            "Edit" | "NotebookEdit" => (1, 0),
            "MultiEdit" => (input.get("edits").and_then(Value::as_array).map(|a| a.len()).unwrap_or(1), 0),
            "Write" => (0, 1),
            "KillShell" | "KillBash" | "TaskStop" => {
                let id = ["shell_id", "task_id", "bash_id", "id"].iter().find_map(|k| str_field(input, k));
                if let Some(id) = id {
                    self.close_job(&id, "killed", ts);
                }
                return;
            }
            _ => return,
        };
        let Some(path) = str_field(input, "file_path").or_else(|| str_field(input, "notebook_path")) else { return };
        let t = self.touched.entry(path).or_default();
        t.edits += edits;
        t.writes += writes;
        t.last_i = ix;
        t.last_ts = ts;
        t.items.push(ix);
    }

    /// The full Edit/Write changes of tool item `ix`, re-read from the raw line.
    pub fn changes_at(&self, ix: usize) -> Vec<Value> {
        let Some(it) = self.items.get(ix) else { return Vec::new() };
        let Some(v) = read_line_at(&self.path, it.off) else { return Vec::new() };
        let Some(b) = v["message"]["content"]
            .as_array()
            .and_then(|a| a.iter().find(|b| b.get("id").and_then(Value::as_str) == it.id.as_deref()))
        else {
            return Vec::new();
        };
        let input = &b["input"];
        let tool = it.name.as_deref().unwrap_or("");
        let base = |extra: Value| {
            let mut m = serde_json::json!({"tid": self.tid, "i": ix, "ts": it.ts, "tool": tool});
            if let (Some(m), Value::Object(e)) = (m.as_object_mut(), extra) {
                m.extend(e);
            }
            m
        };
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        match tool {
            "Write" => vec![base(serde_json::json!({"content": cut(&s(input, "content"), 20000).0}))],
            "MultiEdit" => input
                .get("edits")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|e| base(serde_json::json!({"old": s(e, "old_string"), "new": s(e, "new_string")}))).collect())
                .unwrap_or_default(),
            "NotebookEdit" => vec![base(serde_json::json!({"old": "", "new": s(input, "new_source")}))],
            _ => vec![base(serde_json::json!({"old": s(input, "old_string"), "new": s(input, "new_string")}))],
        }
    }

    fn img_url(&self, off: u64, n: usize) -> String {
        format!("/api/img/{}/{}/{}", self.tid, off, n)
    }

    /// Fills agentId/sub on Agent items from subagent metadata (toolUseId -> agentId).
    pub fn link_subagents(&mut self, subs: &[Subagent]) {
        for s in subs {
            if let Some(&ix) = self.tool_idx.get(&s.tool_use_id) {
                let it = &mut self.items[ix];
                if it.k == "agent" && it.agent_id.is_none() {
                    it.agent_id = Some(s.agent_id.clone());
                    it.sub = Some(s.tid.clone());
                }
            }
        }
    }

    /// Full, uncut text of item `i` (re-read from disk).
    pub fn full_text(&self, i: usize) -> Option<String> {
        let it = self.items.get(i)?;
        let v = read_line_at(&self.path, it.off)?;
        if it.k == "result" {
            let id = it.id.as_deref()?;
            for b in v["message"]["content"].as_array()? {
                if b.get("tool_use_id").and_then(Value::as_str) == Some(id) {
                    return Some(match &b["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(parts) => parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    });
                }
            }
            return None;
        }
        if it.k == "agent" {
            for b in v["message"]["content"].as_array()? {
                if b.get("id").and_then(Value::as_str) == it.id.as_deref() {
                    return str_field(&b["input"], "prompt");
                }
            }
        }
        if it.k == "tool" {
            for b in v["message"]["content"].as_array()? {
                if b.get("id").and_then(Value::as_str) == it.id.as_deref() {
                    return serde_json::to_string_pretty(&b["input"]).ok();
                }
            }
        }
        it.text.clone()
    }
}

/// Decodes the `n`th image block (depth-first, tool_result contents included)
/// of the line at byte `off`.
pub fn image_at(path: &Path, off: u64, n: usize) -> Option<(String, Vec<u8>)> {
    let v = read_line_at(path, off)?;
    let mut found: Vec<&Value> = Vec::new();
    for b in v["message"]["content"].as_array()? {
        match b.get("type").and_then(Value::as_str) {
            Some("image") => found.push(b),
            Some("tool_result") => {
                if let Some(parts) = b["content"].as_array() {
                    found.extend(parts.iter().filter(|p| p.get("type").and_then(Value::as_str) == Some("image")));
                }
            }
            _ => {}
        }
    }
    let img = found.get(n)?;
    let src = &img["source"];
    let mt = src.get("media_type").and_then(Value::as_str).unwrap_or("image/png").to_string();
    let data = src.get("data").and_then(Value::as_str)?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    Some((mt, bytes))
}

fn read_line_at(path: &Path, off: u64) -> Option<Value> {
    let mut f = File::open(path).ok()?;
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut r = BufReader::new(f);
    let mut line = Vec::new();
    r.read_until(b'\n', &mut line).ok()?;
    serde_json::from_slice(&line).ok()
}

// ---------- subagents ----------

#[derive(Serialize, Clone)]
pub struct Subagent {
    #[serde(rename = "agentId")]
    pub agent_id: String,
    pub tid: String,
    pub desc: String,
    #[serde(rename = "type")]
    pub agent_type: String,
    #[serde(rename = "toolUseId")]
    pub tool_use_id: String,
    pub running: bool,
    pub updated: i64,
    pub items: usize,
}

/// Lists the subagents of the session whose transcript is `main_path`.
pub fn list_subagents(main_path: &Path, sid: &str) -> Vec<Subagent> {
    let dir = main_path.with_extension("").join("subagents");
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let now = now_ms();
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(aid) = name.strip_prefix("agent-").and_then(|n| n.strip_suffix(".meta.json")) else { continue };
        let meta: Value = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        let jl = dir.join(format!("agent-{aid}.jsonl"));
        let (updated, size) = std::fs::metadata(&jl).map(|m| (mtime_ms(&m), m.len())).unwrap_or((0, 0));
        let finished = tail_finished(&jl, size);
        out.push(Subagent {
            agent_id: aid.to_string(),
            tid: format!("{sid}~{aid}"),
            desc: str_field(&meta, "description").unwrap_or_default(),
            agent_type: str_field(&meta, "agentType").unwrap_or_else(|| "agent".into()),
            tool_use_id: str_field(&meta, "toolUseId").unwrap_or_default(),
            running: !finished && now - updated < 10 * 60 * 1000,
            updated,
            items: 0,
        });
    }
    out.sort_by_key(|s| s.updated);
    out
}

/// A subagent is finished when its last model message ended its turn without
/// calling a tool, or its last tool call delivered its report.
fn tail_finished(path: &Path, size: u64) -> bool {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (u64, bool)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&(s, done)) = cache.lock().unwrap().get(path) {
        if s == size {
            return done;
        }
    }
    let done = tail_finished_uncached(path, size);
    let mut c = cache.lock().unwrap();
    if c.len() > 2000 {
        c.clear();
    }
    c.insert(path.to_path_buf(), (size, done));
    done
}

fn tail_finished_uncached(path: &Path, size: u64) -> bool {
    let Ok(mut f) = File::open(path) else { return true };
    let start = size.saturating_sub(64 * 1024);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let stop = v["message"].get("stop_reason").and_then(Value::as_str);
                let has_tool = v["message"]["content"]
                    .as_array()
                    .map(|a| a.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use")))
                    .unwrap_or(false);
                return stop == Some("end_turn") || (!has_tool && stop.is_some());
            }
            Some("user") => {
                return line.contains("Report delivered to your caller");
            }
            _ => continue,
        }
    }
    false
}

/// Path of a subagent transcript given its main transcript.
pub fn subagent_path(main_path: &Path, agent_id: &str) -> PathBuf {
    main_path.with_extension("").join("subagents").join(format!("agent-{agent_id}.jsonl"))
}

// ---------- helpers ----------

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

/// Prefix of a user row the sheprd mod appends when the phone steers a running turn.
pub const PHONE_STEER: &str = "[from phone] ";

pub fn cut(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// Cuts `s` to at most `max` bytes, adding an ellipsis when cut.
pub fn short(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut c = cut(s, max.saturating_sub(3)).0; // '…' is 3 bytes
    c.push('…');
    c
}

/// A short title from a prompt: its first non-blank line, whitespace collapsed, at most ~48 bytes.
pub fn prompt_title(s: &str) -> Option<String> {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(short(&line, 48))
}

fn snippet(s: &str) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let (mut c, was_cut) = cut(&collapsed, LAST_CUT);
    if was_cut {
        c.push('…');
    }
    c
}

fn tag(s: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let a = s.find(&open)? + open.len();
    let b = s[a..].find(&close).map(|x| a + x).unwrap_or(s.len());
    Some(s[a..b].trim().to_string())
}

fn command_line(s: &str) -> Option<String> {
    let name = tag(s, "command-name")?;
    let args = tag(s, "command-args").unwrap_or_default();
    let name = if name.starts_with('/') { name } else { format!("/{name}") };
    Some(if args.is_empty() { name } else { format!("{name} {args}") })
}

pub fn strip_reminders(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(a) = rest.find("<system-reminder>") {
        out.push_str(&rest[..a]);
        match rest[a..].find("</system-reminder>") {
            Some(b) => rest = &rest[a + b + "</system-reminder>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

pub fn tool_summary(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let first_line = |t: String| t.lines().next().unwrap_or("").to_string();
    let out = match name {
        "Bash" => {
            let d = s("description");
            let c = first_line(s("command"));
            if c.is_empty() { d } else { c }
        }
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            let p = s("file_path");
            if p.is_empty() { s("notebook_path") } else { p }
        }
        "Grep" => {
            let p = s("path");
            if p.is_empty() { s("pattern") } else { format!("{}  in {}", s("pattern"), p) }
        }
        "Glob" => s("pattern"),
        "WebFetch" => s("url"),
        "WebSearch" => s("query"),
        "Skill" => s("skill"),
        "AskUserQuestion" => input["questions"][0]["question"].as_str().unwrap_or("").to_string(),
        "TodoWrite" => format!("{} todos", input.get("todos").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0)),
        _ => input
            .as_object()
            .and_then(|o| o.values().find_map(|v| v.as_str().map(|x| first_line(x.to_string()))))
            .unwrap_or_default(),
    };
    cut(&out, 300).0
}

fn trim_value(v: &Value, depth: usize) -> Value {
    match v {
        Value::String(s) => {
            let (c, was) = cut(s, INPUT_STR_CUT);
            Value::String(if was { c + "…" } else { c })
        }
        Value::Array(a) if depth < 4 => Value::Array(a.iter().take(50).map(|x| trim_value(x, depth + 1)).collect()),
        Value::Object(o) if depth < 4 => {
            let mut m = Map::new();
            for (k, x) in o {
                m.insert(k.clone(), trim_value(x, depth + 1));
            }
            Value::Object(m)
        }
        Value::Array(_) | Value::Object(_) => Value::String("…".into()),
        other => other.clone(),
    }
}

/// Parses an RFC3339 UTC timestamp like `2026-10-05T04:15:05.733Z` to unix ms.
pub fn parse_ts(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.len() < 19 {
        return 0;
    }
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
    let (y, mo, d) = (num(0..4), num(5..7), num(8..10));
    let (h, mi, se) = (num(11..13), num(14..16), num(17..19));
    let mut ms = 0;
    if b.len() > 20 && b[19] == b'.' {
        let frac: String = s[20..].chars().take_while(|c| c.is_ascii_digit()).take(3).collect();
        ms = frac.parse::<i64>().unwrap_or(0) * 10i64.pow(3 - frac.len() as u32);
    }
    // days_from_civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    ((days * 86400 + h * 3600 + mi * 60 + se) * 1000) + ms
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn mtime_ms(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// "Another Claude session sent a message: <agent-message from=ID> … The report follows: <indented report>"
/// → (ID, the report, dedented). Anything after the closing tag (harness boilerplate) is dropped.
fn agent_report(t: &str) -> Option<(String, String)> {
    const OPEN: &str = "<agent-message from=\"";
    let start = t.find(OPEN)?;
    if start > 80 {
        return None;
    }
    let rest = &t[start + OPEN.len()..];
    let from = rest.split('"').next()?.to_string();
    let body_start = rest.find('>')? + 1;
    let inner = &rest[body_start..];
    let inner = inner.split("</agent-message>").next().unwrap_or(inner);
    let inner = match inner.find("The report follows:") {
        Some(k) => &inner[k + 19..],
        None => inner,
    };
    let body: Vec<&str> = inner.lines().map(|l| l.strip_prefix("  ").unwrap_or(l)).collect();
    let body = body.join("\n").trim().to_string();
    (!body.is_empty()).then_some((from, body))
}

/// First line the server puts on a `/steer` sent with `from` (the sending agent's pane): tells the receiving agent who
/// wrote it and lets the transcript show it as that agent's message.
pub fn steer_header(name: &str, pane: &str) -> String {
    let name: String = name.chars().filter(|c| !matches!(c, '"' | '[' | ']' | '\n' | '\r')).take(60).collect();
    format!("[Message from agent \"{}\" (pane {pane}) via sheprd]", name.trim())
}

pub fn is_pane(p: &str) -> bool {
    let ok = |s: &str| !s.is_empty() && s.len() <= 8 && s.chars().all(|c| c.is_ascii_alphanumeric());
    p.strip_prefix('w').and_then(|r| r.split_once(":p")).is_some_and(|(w, n)| ok(w) && ok(n))
}

/// A user message another agent sent → (its pane if known, its name, the message without the header).
fn steer_from(t: &str) -> Option<(Option<String>, String, String)> {
    let t = t.trim();
    let (first, rest) = t.split_once('\n').unwrap_or((t, ""));
    if let Some(h) = first.strip_prefix("[Message from agent \"").and_then(|h| h.strip_suffix(") via sheprd]")) {
        let (name, pane) = h.rsplit_once("\" (pane ")?;
        return is_pane(pane).then(|| (Some(pane.to_string()), name.to_string(), rest.trim().to_string()));
    }
    loose_sender(t)
}

/// Hand-written sender headers agents put on messages they send each other with `herdr agent prompt`
/// (not through `/steer`): "[from the <name> agent, <pane>] text" or "[Message from agent <name> …] text".
/// The header must be the first bracket, on the first line, and short.
fn loose_sender(t: &str) -> Option<(Option<String>, String, String)> {
    let first = t.lines().next()?;
    let end = first.find(']')?;
    let inner = first.get(1..end).filter(|_| first.starts_with('['))?;
    if inner.len() > 120 {
        return None;
    }
    let lower = inner.to_ascii_lowercase();
    let rest = if lower.starts_with("from the ") && (lower.contains(" agent") || inner.split_whitespace().any(|w| is_pane(w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != ':')))) {
        &inner[9..]
    } else if lower.starts_with("message from agent ") {
        &inner[19..]
    } else if lower.starts_with("message from the ") {
        &inner[17..]
    } else {
        return None;
    };
    let pane = rest
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | '"' | ';'))
        .find(|w| is_pane(w))
        .map(str::to_string);
    // the name: before the pane / first comma or parenthesis, quotes and a trailing "agent" dropped
    let mut name = rest.split([',', '(', ';']).next().unwrap_or("").trim();
    if let Some(p) = pane.as_deref() {
        name = name.split(p).next().unwrap_or("").trim();
    }
    let name = name.trim_matches('"').trim();
    let name = name.strip_suffix(" agent").or_else(|| name.strip_suffix(" Agent")).unwrap_or(name).trim().trim_matches('"').trim();
    let name = name.strip_prefix("pane").map_or(name, str::trim);
    if (name.is_empty() || name.len() > 60) && pane.is_none() {
        return None;
    }
    let name = if name.is_empty() { "agent".to_string() } else { name.to_string() };
    let body = t[first.len()..].trim();
    let body = format!("{}{}{body}", first[end + 1..].trim(), if body.is_empty() || first[end + 1..].trim().is_empty() { "" } else { "\n" });
    (!body.trim().is_empty()).then(|| (pane, name, body.trim().to_string()))
}

/// "The <name> plugin sent a message:\n<text>\n\nThis is how Claude Code surfaces a prompt a plugin submits…" → text.
fn unwrap_plugin(t: &str) -> Option<String> {
    let (first, rest) = t.trim_start().split_once('\n')?;
    if first.len() > 80 || !first.starts_with("The ") || !first.ends_with(" plugin sent a message:") {
        return None;
    }
    let body = rest.rfind("\n\nThis is how Claude Code surfaces").map_or(rest, |k| &rest[..k]).trim();
    (!body.is_empty()).then(|| body.to_string())
}

/// A message made only of `<pasted_content id=…>…</pasted_content>` blocks (plus whitespace) → their text.
fn unwrap_pasted(t: &str) -> Option<String> {
    let mut rest = t.trim();
    if !rest.starts_with("<pasted_content") {
        return None;
    }
    let mut out: Vec<&str> = Vec::new();
    while let Some(r) = rest.strip_prefix("<pasted_content") {
        let r = &r[r.find('>')? + 1..];
        let end = r.find("</pasted_content")?;
        out.push(r[..end].trim());
        let close = r[end..].find('>')? + 1;
        rest = r[end + close..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    Some(out.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ts() {
        assert_eq!(parse_ts("1970-01-01T00:00:01.500Z"), 1500);
        assert_eq!(parse_ts("2026-10-05T04:15:05.733Z"), 1791173705733);
    }
    fn fixture(name: &str, lines: &[serde_json::Value]) -> Transcript {
        let dir = std::env::temp_dir().join(format!("mm-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc.jsonl");
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, body).unwrap();
        let mut t = Transcript::new("abc".into(), path);
        t.refresh();
        t
    }
    fn q(op: &str, content: Option<&str>) -> serde_json::Value {
        let mut v = serde_json::json!({"type": "queue-operation", "operation": op, "timestamp": "2026-10-05T09:00:00.000Z"});
        if let Some(c) = content {
            v["content"] = c.into();
        }
        v
    }
    fn user(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "user", "message": {"role": "user", "content": text}})
    }
    fn tool(id: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}})
    }
    fn result(id: &str, text: &str) -> serde_json::Value {
        serde_json::json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": text}]}})
    }
    #[test]
    fn queue_tracking() {
        let t = fixture("queue", &[
            user("do the thing"),
            q("enqueue", Some("first")),
            q("enqueue", Some("<agent-message from=\"x\">hi</agent-message>")),
            q("enqueue", Some("second")),
            q("enqueue", Some("third")),
            q("remove", Some("second")),
        ]);
        let texts: Vec<_> = t.queued().iter().map(|v| v["text"].as_str().unwrap().to_string()).collect();
        assert_eq!(texts, ["first", "third"]);
        // dequeue pops the front (hidden entries keep positions); a user item clears its text.
        let t = fixture("queue2", &[
            q("enqueue", Some("<task-notification>x</task-notification>")),
            q("enqueue", Some("a")),
            q("enqueue", Some("b")),
            q("dequeue", None),
            user("b"),
        ]);
        let texts: Vec<_> = t.queued().iter().map(|v| v["text"].as_str().unwrap().to_string()).collect();
        assert_eq!(texts, ["a"]);
        // turn end resets it
        let mut lines = vec![q("enqueue", Some("a"))];
        lines.push(serde_json::json!({"type": "assistant", "message": {"stop_reason": "end_turn", "content": [{"type": "text", "text": "ok"}]}}));
        assert!(fixture("queue3", &lines).queued().is_empty());
        // ↑ (popAll) pulls messages back into the input box
        let t = fixture("queue4", &[
            user("go"),
            q("enqueue", Some("a")),
            q("enqueue", Some("b")),
            q("popAll", Some("a")),
            q("popAll", Some("b")),
            q("enqueue", Some("b")),
        ]);
        let texts: Vec<_> = t.queued().iter().map(|v| v["text"].as_str().unwrap().to_string()).collect();
        assert_eq!(texts, ["b"]);
    }
    #[test]
    fn doing_and_answer() {
        let t = fixture("doing", &[
            user("build it"),
            serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Building."}]}}),
            tool("t1", "Bash", serde_json::json!({"command": "cargo build --release"})),
        ]);
        assert_eq!(t.doing().unwrap().0, "Bash · cargo build --release");
        assert_eq!(t.answer_i, Some(1));
        let t = fixture("doing2", &[user("x"), tool("t1", "Bash", serde_json::json!({"command": "ls"})), result("t1", "a")]);
        assert_eq!(t.doing().unwrap().0, "Thinking");
        assert_eq!(t.last_answer(), None);
    }
    #[test]
    fn todo_progress() {
        let todos = serde_json::json!({"todos": [
            {"content": "Read docs", "activeForm": "Reading docs", "status": "completed"},
            {"content": "Build hero", "activeForm": "Building the hero", "status": "in_progress"},
            {"content": "Ship", "activeForm": "Shipping", "status": "pending"}]});
        let t = fixture("todo", &[user("go"), tool("t1", "TodoWrite", todos)]);
        let d = t.todo.unwrap();
        assert_eq!((d.done, d.total, d.cur.as_deref()), (1, 3, Some("Building the hero")));
        let t = fixture("tasks", &[
            tool("t1", "TaskCreate", serde_json::json!({"subject": "A", "activeForm": "Doing A"})),
            tool("t2", "TaskCreate", serde_json::json!({"subject": "B"})),
            tool("t3", "TaskUpdate", serde_json::json!({"taskId": "1", "status": "completed"})),
            tool("t4", "TaskUpdate", serde_json::json!({"taskId": "2", "status": "in_progress"})),
        ]);
        let d = t.todo.unwrap();
        assert_eq!((d.done, d.total, d.cur.as_deref()), (1, 2, Some("B")));
    }
    #[test]
    fn jobs() {
        let launch = "Command running in background with ID: bwcy2z0xx. Output is being written to: /tmp/x/tasks/bwcy2z0xx.output. You will be notified when it completes.";
        let t = fixture("jobs", &[
            tool("t1", "Bash", serde_json::json!({"command": "python train.py", "description": "train", "run_in_background": true})),
            result("t1", launch),
            tool("t2", "Bash", serde_json::json!({"command": "sleep 99", "run_in_background": true})),
            result("t2", "Command running in background with ID: b2. Output is being written to: /tmp/x/b2.output"),
            user("<task-notification>\n<task-id>bwcy2z0xx</task-id>\n<status>completed</status>\n<summary>done</summary>\n</task-notification>"),
            tool("t3", "KillShell", serde_json::json!({"shell_id": "b2"})),
            // output that merely quotes a launch line is not a job
            tool("t4", "Bash", serde_json::json!({"command": "cat other.jsonl"})),
            result("t4", "4 result Command running in background with ID: zz9. Output is being written to: /tmp/zz9.output"),
        ]);
        assert_eq!(t.jobs.len(), 2);
        assert_eq!((t.jobs[0].id.as_str(), t.jobs[0].status, t.jobs[0].out.as_str()), ("bwcy2z0xx", "completed", "/tmp/x/tasks/bwcy2z0xx.output"));
        assert_eq!(t.jobs[0].desc.as_deref(), Some("train"));
        assert_eq!(t.jobs[0].i, 0);
        assert_eq!(t.jobs[1].status, "killed");
    }
    #[test]
    fn files_touched() {
        let t = fixture("files", &[
            tool("t1", "Write", serde_json::json!({"file_path": "/w/a.txt", "content": "1\n2\n3\n"})),
            result("t1", "ok"),
            tool("t2", "Edit", serde_json::json!({"file_path": "/w/a.txt", "old_string": "2", "new_string": "two"})),
            tool("t3", "MultiEdit", serde_json::json!({"file_path": "/w/b.rs", "edits": [{"old_string": "a", "new_string": "b"}, {"old_string": "c", "new_string": "d"}]})),
        ]);
        let a = &t.touched["/w/a.txt"];
        assert_eq!((a.edits, a.writes, a.last_i, a.items.clone()), (1, 1, 2, vec![0, 2]));
        assert_eq!(t.touched["/w/b.rs"].edits, 2);
        let c = t.changes_at(2);
        assert_eq!((c[0]["tool"].as_str(), c[0]["old"].as_str(), c[0]["new"].as_str()), (Some("Edit"), Some("2"), Some("two")));
        assert_eq!(t.changes_at(0)[0]["content"], "1\n2\n3\n");
        assert_eq!(t.changes_at(3).len(), 2);
    }
    #[test]
    fn ctx_estimate() {
        let resp = |req: &str, model: &str, inp: u64, read: u64, write: u64, side: bool| {
            serde_json::json!({"type": "assistant", "requestId": req, "isSidechain": side, "timestamp": "2026-10-05T09:00:00.000Z",
                "message": {"model": model, "content": [{"type": "text", "text": "x"}],
                    "usage": {"input_tokens": inp, "cache_read_input_tokens": read, "cache_creation_input_tokens": write, "output_tokens": 1_000_000}}})
        };
        let t = fixture("ctx", &[user("hi")]);
        assert!(t.ctx_est.is_none());
        let t = fixture("ctx2", &[
            user("hi"),
            resp("r1", "claude-opus-5-5", 10, 1000, 200, false),
            resp("r1", "claude-opus-5-5", 10, 1000, 200, false), // second content block, same request
            resp("r2", "claude-opus-5-5", 5, 250_000, 100, false),
            resp("s1", "claude-haiku-4-5", 5, 999_999, 0, true), // a sidechain row never counts
            resp("r3", "<synthetic>", 0, 0, 0, false),
        ]);
        let e = t.ctx_est.clone().unwrap();
        assert_eq!((e.model.as_str(), e.tokens, e.max), ("claude-opus-5-5", Some(250_105), 250_105));
        // two requests of 1M output tokens at $20/M plus a little input
        assert!((e.cost - 40.0).abs() < 0.2, "{}", e.cost);
        let mut lines = vec![user("hi"), resp("r1", "claude-opus-5-5", 10, 1000, 200, false)];
        lines.push(serde_json::json!({"type": "system", "subtype": "compact_boundary", "timestamp": "2026-10-05T09:01:00.000Z"}));
        let t = fixture("ctx3", &lines);
        assert_eq!(t.ctx_est.as_ref().unwrap().tokens, None);
    }
    #[test]
    fn reminders() {
        assert_eq!(strip_reminders("hi <system-reminder>x</system-reminder> there"), "hi  there");
    }
    #[test]
    fn prompt_title_first_line_cut() {
        assert_eq!(super::prompt_title("\n  fix   the login\nmore detail").as_deref(), Some("fix the login"));
        assert_eq!(super::prompt_title(&"x".repeat(100)).map(|t| t.len()), Some(48));
        assert_eq!(super::prompt_title(" \n "), None);
    }
    #[test]
    fn agent_messages_are_tagged() {
        let r = "Another Claude session sent a message:\n<agent-message from=\"a1b2\">\n[Subagent hand-back] frame text\n  more frame. The report follows:\n  **Done.** Fixed it.\n  - a\n</agent-message>\n\nThat other session…";
        assert_eq!(agent_report(r), Some(("a1b2".into(), "**Done.** Fixed it.\n- a".into())));
        assert_eq!(agent_report("hello"), None);
        let p = "\n\n<pasted_content id=\"47c0\">\nNew feature request\n</pasted_content id=\"47c0\">";
        assert_eq!(unwrap_pasted(p).as_deref(), Some("New feature request"));
        assert_eq!(unwrap_pasted("see <pasted_content id=\"1\">x</pasted_content>"), None);
        assert_eq!(unwrap_pasted("<pasted_content id=\"1\">a</pasted_content>\nb").as_deref(), Some("a\n\nb"));
        let h = steer_header("do\"cs]", "wG:p1");
        assert_eq!(h, "[Message from agent \"docs\" (pane wG:p1) via sheprd]");
        assert_eq!(steer_from("[Message from agent \"x\" (pane nope) via sheprd]\nhi"), None);
        assert_eq!(steer_from("hello"), None);
        // hand-written headers from `herdr agent prompt` (not /steer)
        let b = "[from the sheprd build agent, w6:pB] Fixed and deployed (1bd1ab9).";
        assert_eq!(steer_from(b), Some((Some("w6:pB".into()), "sheprd build".into(), "Fixed and deployed (1bd1ab9).".into())));
        let b = "[from the docs agent, w2:p7]\nTwo new pages.\nMore.";
        assert_eq!(steer_from(b), Some((Some("w2:p7".into()), "docs".into(), "Two new pages.\nMore.".into())));
        let b = "[Message from agent \"Nightly backup\" (pane w3:p1)] Backup done, 3 GB.";
        assert_eq!(steer_from(b), Some((Some("w3:p1".into()), "Nightly backup".into(), "Backup done, 3 GB.".into())));
        let b = "[Message from agent reviewer] Looks good.";
        assert_eq!(steer_from(b), Some((None, "reviewer".into(), "Looks good.".into())));
        assert_eq!(steer_from("[from the docs] see this"), None); // needs "agent" or a pane id
        assert_eq!(steer_from("[from the build, w1:p2] ok"), Some((Some("w1:p2".into()), "build".into(), "ok".into())));
        assert_eq!(steer_from("[from the sheprd agent, w6:pB]"), None); // header only
        assert_eq!(steer_from("[note] from the agent"), None);
        assert_eq!(steer_from("see [from the x agent, w1:p1] hi"), None);
        let m = format!("{h}\nhi");
        let w = format!("The sheprd plugin sent a message:\n{m}\n\nThis is how Claude Code surfaces a prompt a plugin submits between turns — it starts this turn in the user's place. Address the message above.");
        assert_eq!(unwrap_plugin(&w).as_deref(), Some(m.as_str()));
        assert_eq!(unwrap_plugin("hi\nthere"), None);
        assert!(is_pane("w6:pE") && is_pane("wG:p1") && !is_pane("w6") && !is_pane("w6:p") && !is_pane("x6:p1"));
    }

}
