//! What each agent is doing, in words (API.md "Activity").
//!
//! 1. Rules (every refresh, zero cost): the taxonomy in `server/data/activities.toml` classifies the agent's open
//!    tool call (else the newest call of the turn) into `activity: {id, cat, icon, phrase, conf, src, now}`.
//!    The agent's own in-progress todo item, when it has one, is the phrase (`src: "todo"`).
//! 2. Haiku (cheap, rate-limited, only while the phone is in use): once per turn that runs past 45 s (again
//!    after 10 min, at most 3 times), Haiku picks a taxonomy id for the turn and fills its slots (`src: "haiku"`);
//!    once per finished turn longer than a minute it writes a short past-tense `outcome`. One call at a time,
//!    a daily cap, results cached in `<state>/activity.json`, kill switch `POST /api/activity {haiku: false}`
//!    (or `SHEPRD_ACTIVITY_HAIKU=0`).

use crate::transcript::{Item, Todo, Transcript, now_ms};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use regex_lite::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

const SRC: &str = include_str!("../data/activities.toml");

// ---------- taxonomy ----------

#[derive(Deserialize)]
struct RawTax {
    categories: BTreeMap<String, Cat>,
    #[serde(default)]
    sites: HashMap<String, String>,
    act: Vec<RawAct>,
}

#[derive(Deserialize, Clone)]
pub struct Cat {
    pub label: String,
    pub icon: String,
    pub phrase: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawAct {
    id: String,
    cat: String,
    phrase: String,
    alt: Option<String>,
    ask: Option<String>,
    icon: Option<String>,
    progress: bool,
    conf: Option<f32>,
    tool: Vec<String>,
    cmd: Vec<String>,
    desc: Vec<String>,
    path: Vec<String>,
    ext: Vec<String>,
    host: Vec<String>,
    url: Vec<String>,
    query: Vec<String>,
    agent: Vec<String>,
    todo: Vec<String>,
    not: Vec<String>,
    bare: Vec<String>,
    all: bool,
}

pub struct Act {
    pub id: String,
    pub cat: String,
    pub phrase: String,
    pub alt: Option<String>,
    /// the template Haiku fills (defaults to `phrase`)
    pub ask: Option<String>,
    pub icon: Option<String>,
    pub progress: bool,
    pub conf: f32,
    tool: Vec<String>,
    /// (field, regexes) pairs; any match is enough
    pats: Vec<(Field, Vec<Regex>)>,
    ext: Vec<String>,
    not: Vec<Regex>,
    /// tools that match without any pattern
    bare: Vec<String>,
    /// every pattern list must match
    all: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Field {
    Cmd,
    Desc,
    Path,
    Host,
    Url,
    Query,
    Agent,
    Todo,
}

pub struct Tax {
    pub cats: BTreeMap<String, Cat>,
    pub acts: Vec<Act>,
    sites: HashMap<String, String>,
    by_id: HashMap<String, usize>,
}

/// Ids that only catch what nothing else did: not counted as classified.
const FALLBACK: [&str; 4] = ["bash-other", "mcp-other", "tool-other", "desc-any"];

fn compile(src: &str) -> Result<Tax, String> {
    let raw: RawTax = toml::from_str(src).map_err(|e| e.to_string())?;
    let mut acts = Vec::new();
    let mut by_id = HashMap::new();
    for r in raw.act {
        if !raw.categories.contains_key(&r.cat) {
            return Err(format!("{}: unknown category {}", r.id, r.cat));
        }
        if by_id.insert(r.id.clone(), acts.len()).is_some() {
            return Err(format!("duplicate id {}", r.id));
        }
        let re = |list: &[String], flags: &str| -> Result<Vec<Regex>, String> {
            list.iter().map(|p| Regex::new(&format!("{flags}{p}")).map_err(|e| format!("{}: {p}: {e}", r.id))).collect()
        };
        let mut pats = Vec::new();
        for (f, list, flags) in [
            (Field::Cmd, &r.cmd, "(?m)"),
            (Field::Desc, &r.desc, "(?i)"),
            (Field::Path, &r.path, ""),
            (Field::Host, &r.host, "(?i)"),
            (Field::Url, &r.url, ""),
            (Field::Query, &r.query, "(?i)"),
            (Field::Agent, &r.agent, ""),
            (Field::Todo, &r.todo, "(?i)"),
        ] {
            if !list.is_empty() {
                pats.push((f, re(list, flags)?));
            }
        }
        acts.push(Act {
            not: re(&r.not, "(?m)")?,
            id: r.id,
            cat: r.cat,
            phrase: r.phrase,
            alt: r.alt,
            ask: r.ask,
            icon: r.icon,
            progress: r.progress,
            conf: r.conf.unwrap_or(0.8),
            tool: r.tool,
            bare: r.bare,
            all: r.all,
            pats,
            ext: r.ext.iter().map(|e| e.to_lowercase()).collect(),
        });
    }
    Ok(Tax { cats: raw.categories, acts, sites: raw.sites, by_id })
}

pub fn tax() -> &'static Tax {
    static T: LazyLock<Tax> = LazyLock::new(|| compile(SRC).expect("server/data/activities.toml"));
    &T
}

impl Tax {
    pub fn get(&self, id: &str) -> Option<&Act> {
        self.by_id.get(id).map(|&i| &self.acts[i])
    }
    fn icon(&self, a: &Act) -> String {
        a.icon.clone().unwrap_or_else(|| self.cats.get(&a.cat).map(|c| c.icon.clone()).unwrap_or_default())
    }
}

/// `pat` with `*` wildcards against a tool name.
fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let mut rest = s;
    for (k, p) in parts.iter().enumerate() {
        if k == 0 {
            let Some(r) = rest.strip_prefix(p) else { return false };
            rest = r;
        } else if k == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            let Some(at) = rest.find(p) else { return false };
            rest = &rest[at + p.len()..];
        }
    }
    true
}

// ---------- signals from one tool call ----------

#[derive(Default, Debug)]
pub struct Sig {
    pub tool: String,
    pub cmd: String,
    pub desc: String,
    pub path: String,
    pub url: String,
    pub host: String,
    pub query: String,
    pub agent: String,
    pub todo: String,
    pub skill: String,
    pub pattern: String,
}

static WRAP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"^(?:(?:\w+=(?:'[^']*'|"[^"]*"|\S*)\s+)|(?:timeout(?:\s+-\S+)*\s+\d+\w?\s+)|(?:\w+\(\)\s*\{\s*)"#,
        r"|(?:(?:then|do|else|time|sudo(?:\s+-\S+)*|nohup|exec|command|nice(?:\s+-n\s*-?\d+)?|stdbuf(?:\s+-\S+)+",
        r"|env(?:\s+-u\s+\S+|\s+-i)*|uv run|npx|bunx|pnpm exec|xvfb-run(?:\s+-\S+)*)\s+))+"
    ))
    .unwrap()
});
static NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(?:cd|export|set|source|local|unset|trap|wait|done|fi|esac|shopt|declare|readonly|return|exit|break|continue|true|false)\b|\.\s|:\s*$|[{}()]+$|#|EOF$|PYEOF$|'?E'?$)").unwrap()
});
static URL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"https?://[^\s'"<>)\]]+"#).unwrap());
static PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|[\s'"=(:>])((?:~|\$\w+|\.{1,2})?/?(?:[\w@.+-]+/)*[\w@+-][\w@.+-]*\.(?:rs|js|mjs|cjs|ts|tsx|jsx|py|go|rb|java|kt|swift|c|h|cc|cpp|hpp|css|scss|html|md|json|jsonl|toml|yaml|yml|lua|sh|sql|txt|log|output|png|jpg|jpeg|svg|webp|gif|pdf|csv|ipynb|step|stl|scad|xml|conf|ini|service|timer|vue|svelte))\b"#).unwrap()
});

/// Splits one shell line on `&& || ; |` outside quotes.
fn split_ops(line: &str) -> Vec<&str> {
    let b = line.as_bytes();
    let (mut out, mut start, mut q, mut i) = (Vec::new(), 0, 0u8, 0);
    while i < b.len() {
        let c = b[i];
        if q != 0 {
            if c == q {
                q = 0;
            } else if c == b'\\' && q == b'"' {
                i += 1;
            }
        } else if c == b'\'' || c == b'"' {
            q = c;
        } else if c == b'\\' {
            i += 1;
        } else if c == b';' || c == b'|' || (c == b'&' && b.get(i + 1) == Some(&b'&')) {
            out.push(&line[start..i]);
            let w = if (c == b'|' && b.get(i + 1) == Some(&b'|')) || c == b'&' { 2 } else { 1 };
            i += w;
            start = i;
            continue;
        }
        i += 1;
    }
    out.push(&line[start.min(line.len())..]);
    out
}

/// One command segment per line, with env assignments, `cd`, `export` and wrappers stripped.
pub fn norm_cmd(cmd: &str) -> String {
    let mut segs: Vec<String> = Vec::new();
    for line in cmd.lines().take(400) {
        for seg in split_ops(line) {
            let s = seg.trim().trim_start_matches(['(', '{', '!', ' ']).trim();
            let s = WRAP.replace(s, "");
            let s = s.trim();
            if s.is_empty() || NOISE.is_match(s) {
                continue;
            }
            segs.push(s.to_string());
        }
    }
    if segs.is_empty() {
        return cmd.trim().lines().next().unwrap_or("").to_string();
    }
    segs.join("\n")
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let h = rest.split(['/', '?', '#']).next().unwrap_or("");
    let h = h.rsplit('@').next().unwrap_or(h);
    h.split(':').next().unwrap_or("").to_lowercase()
}

pub fn sig_of(tool: &str, input: &Value) -> Sig {
    let mut g = Sig { tool: tool.to_string(), desc: s(input, "description").trim().to_string(), ..Default::default() };
    match tool {
        "Bash" | "PowerShell" => {
            let raw = s(input, "command");
            g.cmd = norm_cmd(raw);
            if let Some(m) = URL_RE.find(raw) {
                g.url = m.as_str().trim_end_matches(['"', '\'', ';', ',']).to_string();
            }
            let no_urls = URL_RE.replace_all(&g.cmd, " ");
            if let Some(c) = PATH_RE.captures(&no_urls) {
                g.path = c[1].to_string();
            }
        }
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            g.path = [s(input, "file_path"), s(input, "notebook_path")].into_iter().find(|p| !p.is_empty()).unwrap_or("").to_string();
        }
        "Grep" => {
            g.path = s(input, "path").to_string();
            g.pattern = s(input, "pattern").to_string();
        }
        "Glob" => g.path = s(input, "pattern").to_string(),
        "WebSearch" => g.query = s(input, "query").to_string(),
        "Agent" | "Task" => {
            g.agent = format!("{}: {}", s(input, "subagent_type"), s(input, "description"));
        }
        "Skill" => g.skill = s(input, "skill").to_string(),
        "TodoWrite" => {
            if let Some(t) = input["todos"].as_array().and_then(|l| l.iter().find(|t| t["status"] == "in_progress")) {
                g.todo = [s(t, "activeForm"), s(t, "content")].into_iter().find(|x| !x.is_empty()).unwrap_or("").to_string();
            }
        }
        _ => {}
    }
    if g.url.is_empty() {
        g.url = s(input, "url").to_string();
    }
    if !g.url.is_empty() {
        g.host = host_of(&g.url);
    }
    g
}

/// Input of a transcript tool/agent item, in the shape `sig_of` expects.
fn item_call(it: &Item) -> Option<(String, Value)> {
    match it.k {
        "agent" => Some((
            it.name.clone().unwrap_or_else(|| "Agent".into()),
            json!({"description": it.desc, "subagent_type": it.agent_type}),
        )),
        "tool" => Some((it.name.clone().unwrap_or_default(), it.input.clone().unwrap_or(Value::Null))),
        _ => None,
    }
}

impl Act {
    /// Named captures of the first matching pattern when this rule matches `g`.
    fn matches(&self, g: &Sig) -> Option<Vec<(String, String)>> {
        if !self.tool.is_empty() && !self.tool.iter().any(|t| glob(t, &g.tool)) {
            return None;
        }
        // pseudo tools ("@thinking") never match a call
        if self.tool.iter().all(|t| t.starts_with('@')) && !self.tool.is_empty() {
            return None;
        }
        let mut hit = (self.pats.is_empty() && self.ext.is_empty()) || self.bare.iter().any(|t| glob(t, &g.tool));
        let mut named = Vec::new();
        let mut kinds_hit = 0;
        for (f, list) in &self.pats {
            let text = match f {
                Field::Cmd => &g.cmd,
                Field::Desc => &g.desc,
                Field::Path => &g.path,
                Field::Host => &g.host,
                Field::Url => &g.url,
                Field::Query => &g.query,
                Field::Agent => &g.agent,
                Field::Todo => &g.todo,
            };
            if text.is_empty() {
                continue;
            }
            // every matching list contributes its named captures (the first matching regex of each list)
            for re in list {
                if let Some(c) = re.captures(text) {
                    for name in re.capture_names().flatten() {
                        if let Some(m) = c.name(name) {
                            named.push((name.to_string(), m.as_str().to_string()));
                        }
                    }
                    kinds_hit += 1;
                    break;
                }
            }
        }
        if kinds_hit > 0 && (!self.all || kinds_hit == self.pats.len()) {
            hit = true;
        }
        if !hit && !self.ext.is_empty() {
            let ext = g.path.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
            hit = !g.path.is_empty() && self.ext.contains(&ext);
        }
        if !hit {
            return None;
        }
        if self.not.iter().any(|re| re.is_match(&g.cmd) || re.is_match(&g.desc)) {
            return None;
        }
        Some(named)
    }
}

// ---------- slots and phrases ----------

const VERBS: &str = "add adjust allow analyse analyze annotate append apply ask audit benchmark block book bump build cache \
    calculate capture categorise categorize change check choose classify clean clear close collect commit compare compile \
    compute configure confirm connect consolidate convert copy count create crop cut debug dedupe define delete deploy \
    describe design detect diagnose diff disable document download draft drop dump edit enable ensure evaluate examine \
    explore export extend extract fetch figure fill filter find finish fix flag follow format gather generate get grab grep \
    guard handle hide hook identify implement import improve infer inspect install investigate keep kill label launch learn \
    let limit list load locate log look make map mark measure merge migrate mine mock move note open order patch peek pick \
    pin place plan play poll port prefer prepare print probe profile prune publish pull push put query read rebase rebuild \
    record reduce refactor refresh register reload remove rename render reorder repair replace reply report repro \
    reproduce request rerun re-run reschedule reset resize resolve restart restore restructure resume retest retry return \
    review revert rewrite run save scan scaffold schedule screenshot search see select send serve set shoot show shrink \
    simplify simulate skip sleep smoke snap sort spawn split start stash stop store strip study submit summarise summarize \
    support survey swap switch sync tag tally tear test tidy time toggle trace track train transcribe translate trim try \
    tune turn type undo unhook unify unpin update upgrade upload use validate verify view wait walk watch wire wrap write";

static VERB_SET: LazyLock<std::collections::HashSet<&'static str>> = LazyLock::new(|| VERBS.split_whitespace().collect());

/// "run" → "running", "write" → "writing", "tidy" → "tidying".
pub fn gerund(v: &str) -> String {
    let lower = v.to_lowercase();
    let dbl = [
        "run", "rerun", "re-run", "set", "get", "put", "cut", "stop", "plan", "ship", "drop", "skip", "swap", "wrap", "map",
        "tap", "pin", "unpin", "begin", "commit", "let", "hit", "trim", "snap", "tag", "log", "debug", "grep", "prep", "scan",
        "clip", "split", "submit", "omit", "dim", "grab", "shop", "chat", "fit", "flag",
    ];
    if dbl.contains(&lower.as_str()) {
        return format!("{lower}{}ing", lower.chars().last().unwrap());
    }
    if lower.ends_with("ie") {
        return format!("{}ying", &lower[..lower.len() - 2]);
    }
    if lower.ends_with("ee") || lower.ends_with("oe") || lower.ends_with("ye") || lower == "be" {
        return format!("{lower}ing");
    }
    if let Some(stem) = lower.strip_suffix('e') {
        return format!("{stem}ing");
    }
    format!("{lower}ing")
}

/// Words up to a clause break, at most `max` words, without a dangling joiner at the end.
fn clause(s: &str, max: usize) -> String {
    let s = s.split([';', '(', '\n']).next().unwrap_or("").split(" — ").next().unwrap_or("").split(", then").next().unwrap_or("");
    let mut w: Vec<&str> = s.split_whitespace().take(max).collect();
    while w.last().is_some_and(|x| {
        matches!(x.to_lowercase().trim_end_matches(',').as_ref(), "and" | "or" | "for" | "the" | "to" | "of" | "with" | "in" | "on" | "a" | "an" | "from" | "into" | "+" | "&" | "vs" | "at" | "by")
    }) {
        w.pop();
    }
    w.join(" ").trim_end_matches([',', '.', ':', ';']).to_string()
}

/// "Read the API docs" → ("reading the API docs", "the API docs") when it starts with a known verb.
fn desc_parts(desc: &str) -> (String, String) {
    let mut words = desc.split_whitespace();
    let Some(first) = words.next() else { return Default::default() };
    let f = first.trim_end_matches([',', ':']).to_lowercase();
    if !VERB_SET.contains(f.as_str()) {
        return Default::default();
    }
    let rest: Vec<&str> = words.collect();
    let mut rest = clause(&rest.join(" "), 7);
    // "Commit and deploy fixes": the object belongs to the second verb, keep just the first
    if rest.starts_with("and ") || rest.starts_with("& ") || rest.starts_with("+ ") {
        rest.clear();
    }
    let ing = if rest.is_empty() { gerund(&f) } else { format!("{} {rest}", gerund(&f)) };
    (ing, rest)
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        // keep acronyms and names ("API", "GPU", "iOS")
        Some(f) if f.is_uppercase() && c.clone().next().is_some_and(|n| n.is_uppercase()) => s.to_string(),
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn basename(p: &str) -> String {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

pub fn site_name(host: &str) -> String {
    let h = host.trim_start_matches("www.").trim_start_matches("m.").trim_start_matches("smile.");
    if h.is_empty() {
        return String::new();
    }
    if h == "localhost" || h.starts_with("127.") || h.starts_with("0.0.0.0") || h.starts_with("100.") || h.ends_with(".ts.net") || h == "[::1]" {
        return "the local app".into();
    }
    let t = tax();
    if let Some(n) = t.sites.get(h) {
        return n.clone();
    }
    let labels: Vec<&str> = h.split('.').collect();
    if labels.len() >= 2 {
        let reg = labels[labels.len() - 2..].join(".");
        if let Some(n) = t.sites.get(&reg) {
            return n.clone();
        }
        let l = labels[labels.len() - 2];
        if l.chars().all(|c| c.is_ascii_digit()) {
            return String::new();
        }
        let mut c = l.chars();
        return c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
    }
    h.to_string()
}

/// Search terms in a store/search URL (`?k=`, `st=`, `Ntt=`, `q=` ...).
fn item_of(url: &str) -> String {
    let q = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    for kv in q.split('&') {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        if matches!(k.to_lowercase().as_str(), "k" | "st" | "ntt" | "q" | "query" | "search" | "keyword" | "keywords" | "_nkw") && !v.is_empty() {
            let v = v.replace('+', " ");
            let mut out = Vec::new();
            let b = v.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' && i + 2 < b.len() {
                    if let Ok(x) = u8::from_str_radix(&v[i + 1..i + 3], 16) {
                        out.push(x);
                        i += 3;
                        continue;
                    }
                }
                out.push(b[i]);
                i += 1;
            }
            let s = String::from_utf8_lossy(&out).to_string();
            if s.contains(['$', '{', '%']) {
                return String::new();
            }
            return clause(&s, 5);
        }
    }
    String::new()
}

/// Context for slots: the agent's title and folder.
#[derive(Default, Clone, Copy)]
pub struct Ctx<'a> {
    pub title: &'a str,
    pub cwd: &'a str,
}

fn clean(v: &str, max_words: usize) -> String {
    let v = v.trim().trim_matches(['"', '\'', '`', '.', ',', ':']).trim();
    clause(v, max_words)
}

fn slots_for(g: &Sig, ctx: &Ctx, named: Vec<(String, String)>) -> HashMap<String, String> {
    let mut m: HashMap<String, String> = HashMap::new();
    let mut path = g.path.clone();
    for (k, v) in &named {
        if k == "path" {
            path = v.clone();
        }
    }
    if !path.is_empty() && !path.contains('*') {
        m.insert("file".into(), basename(&path));
    }
    // project: the repo folder (from ~/Work/<name>/… or the cwd), unless the title already says it
    let proj = path
        .split_once("/Work/")
        .and_then(|(_, r)| r.split('/').next().map(str::to_string))
        .filter(|p| !p.is_empty() && !path.ends_with(p.as_str()))
        .unwrap_or_else(|| basename(ctx.cwd));
    let tl = ctx.title.to_lowercase();
    if !proj.is_empty() && proj != crate::home().file_name().and_then(|n| n.to_str()).unwrap_or("") && !tl.contains(&proj.to_lowercase()) {
        m.insert("project".into(), proj);
    }
    if !g.host.is_empty() {
        let site = site_name(&g.host);
        if !site.is_empty() {
            m.insert("site".into(), site);
        }
        let item = item_of(&g.url);
        if !item.is_empty() {
            m.insert("item".into(), item);
        }
    }
    if !g.query.is_empty() {
        m.insert("query".into(), clean(&g.query, 6));
    }
    let (ing, what) = desc_parts(&g.desc);
    if !ing.is_empty() {
        m.insert("desc_ing".into(), ing);
    }
    if !what.is_empty() {
        m.insert("what".into(), lower_first(&what));
    }
    if !g.agent.is_empty() {
        let d = g.agent.split_once(": ").map(|(_, d)| d).unwrap_or("");
        let (ing, _) = desc_parts(d);
        let a = if ing.is_empty() { lower_first(&clause(d, 7)) } else { ing };
        if !a.is_empty() {
            m.insert("agent".into(), a);
        }
    }
    if !g.skill.is_empty() {
        m.insert("skill".into(), g.skill.rsplit(':').next().unwrap_or("").to_string());
    }
    if !g.todo.is_empty() {
        m.insert("todo".into(), lower_first(&clean(&g.todo, 8)));
    }
    if g.tool == "Grep" && Regex::new(r"^[\w .-]{3,24}$").unwrap().is_match(&g.pattern) {
        m.insert("thing".into(), g.pattern.clone());
    }
    for (k, v) in named {
        if k != "path" {
            let v = clean(&v, 5);
            if !v.is_empty() {
                m.insert(k, v);
            }
        }
    }
    m
}

/// Fills a phrase template: `{slot}`, `{slot|fallback}`, `[optional part]`. None when a required slot is empty.
pub fn render(tpl: &str, slots: &HashMap<String, String>, keep_optional: bool) -> Option<String> {
    fn fill(part: &str, slots: &HashMap<String, String>) -> Option<String> {
        let mut out = String::new();
        let mut rest = part;
        while let Some(a) = rest.find('{') {
            out.push_str(&rest[..a]);
            let b = rest[a..].find('}')? + a;
            let inner = &rest[a + 1..b];
            let (name, fb) = match inner.split_once('|') {
                Some((n, f)) => (n, Some(f)),
                None => (inner, None),
            };
            match slots.get(name).filter(|v| !v.is_empty()) {
                Some(v) => out.push_str(v),
                None => out.push_str(fb?),
            }
            rest = &rest[b + 1..];
        }
        out.push_str(rest);
        Some(out)
    }
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(a) = rest.find('[') {
        out.push_str(&fill(&rest[..a], slots)?);
        let b = rest[a..].find(']')? + a;
        if keep_optional {
            if let Some(x) = fill(&rest[a + 1..b], slots) {
                out.push_str(&x);
            }
        }
        rest = &rest[b + 1..];
    }
    out.push_str(&fill(rest, slots)?);
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let out = out.replace(" ,", ",").replace(" :", ":");
    let out = out.trim().trim_end_matches([':', ',']).trim().to_string();
    (!out.is_empty()).then_some(out)
}

const MAX_PHRASE: usize = 64;

fn phrase_for(t: &Tax, a: &Act, slots: &HashMap<String, String>) -> String {
    for keep in [true, false] {
        for tpl in [Some(&a.phrase), a.alt.as_ref()].into_iter().flatten() {
            if let Some(p) = render(tpl, slots, keep).filter(|p| p.len() <= MAX_PHRASE) {
                return p;
            }
        }
    }
    // too long even without the optional parts: cut a filled phrase at a word boundary
    if let Some(p) = render(&a.phrase, slots, false) {
        return clause(&crate::transcript::cut(&p, MAX_PHRASE).0, 9);
    }
    t.cats.get(&a.cat).map(|c| c.phrase.clone()).unwrap_or_else(|| "working".into())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub id: String,
    pub phrase: String,
    pub conf: f32,
}

impl Hit {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn classified(&self) -> bool {
        !FALLBACK.contains(&self.id.as_str())
    }
}

pub fn classify_sig(g: &Sig, ctx: &Ctx) -> Hit {
    let t = tax();
    for a in &t.acts {
        if let Some(named) = a.matches(g) {
            let slots = slots_for(g, ctx, named);
            return Hit { id: a.id.clone(), phrase: phrase_for(t, a, &slots), conf: a.conf };
        }
    }
    Hit { id: "tool-other".into(), phrase: "working".into(), conf: 0.2 }
}

pub fn classify(tool: &str, input: &Value, ctx: &Ctx) -> Hit {
    classify_sig(&sig_of(tool, input), ctx)
}

/// Activity types that keep running and serve something: a background job running one is a running app.
pub const APP_ACTS: [&str; 2] = ["dev-server", "serve-app"];

/// Whether a background Bash command is a server (a running app on Home, not In-flight work).
pub fn app_cmd(cmd: &str) -> bool {
    APP_ACTS.contains(&classify("Bash", &serde_json::json!({"command": cmd}), &Ctx::default()).id.as_str())
}

// ---------- per agent ----------

#[derive(Clone, Debug)]
pub struct Activity {
    pub id: String,
    pub phrase: String,
    pub conf: f32,
    /// "rules" | "todo" | "haiku"
    pub src: &'static str,
    /// the rules phrase for the call running right now
    pub now: String,
}

impl Activity {
    pub fn json(&self) -> Value {
        let t = tax();
        let a = t.get(&self.id);
        let cat = a.map(|a| a.cat.as_str()).unwrap_or("thinking");
        json!({
            "id": self.id, "cat": cat,
            "icon": a.map(|a| t.icon(a)).unwrap_or_default(),
            "phrase": self.phrase, "conf": (self.conf as f64 * 100.0).round() / 100.0,
            "src": self.src, "now": self.now,
            "progress": a.is_some_and(|a| a.progress),
        })
    }
}

/// What Haiku sees of a turn, gathered while the transcript is locked.
#[derive(Clone, Default, Debug)]
pub struct Turn {
    /// `<tid>:<index of the prompt>`
    pub key: String,
    pub start_ts: i64,
    pub last_ts: i64,
    pub prompt: String,
    pub calls: Vec<String>,
    pub todo: Option<String>,
    /// the turn's last answer text (for the outcome)
    pub answer: Option<String>,
}

fn is_prompt(it: &Item) -> bool {
    it.k == "user"
        && matches!(it.kind, None | Some("steer"))
        && it.text.as_deref().is_some_and(|t| !t.starts_with("[Request interrupted") && !t.starts_with('<'))
}

/// One line per call for Haiku: "Bash: Run cargo tests" / "Edit: web/home.js".
fn call_line(it: &Item) -> Option<String> {
    let (tool, input) = item_call(it)?;
    let short = tool.rsplit("__").next().unwrap_or(&tool).to_string();
    let d = input["description"].as_str().unwrap_or("");
    let what = if !d.is_empty() { d.to_string() } else { it.summary.clone().unwrap_or_default() };
    let what = what.replace('\n', " ");
    Some(format!("{short}: {}", crate::transcript::short(what.trim(), 110)))
}

/// Rules pass over one transcript: the activity right now plus what Haiku would need.
pub fn of_transcript(t: &Transcript, todo: Option<&Todo>, ctx: &Ctx) -> (Option<Activity>, Turn) {
    let items = &t.items;
    let start = items.iter().rposition(is_prompt);
    let turn_items = &items[start.map_or(0, |s| s + 1)..];
    let mut turn = Turn {
        key: format!("{}:{}", t.tid, start.map_or(0, |s| items[s].i)),
        start_ts: start.map_or(0, |s| items[s].ts),
        last_ts: items.last().map_or(0, |it| it.ts),
        prompt: start.and_then(|s| items[s].text.as_deref()).map(|p| crate::transcript::short(p.trim(), 300)).unwrap_or_default(),
        calls: turn_items.iter().filter_map(call_line).collect::<Vec<_>>().into_iter().rev().take(6).rev().collect(),
        todo: todo.and_then(|x| x.cur.clone()).filter(|_| todo.is_some_and(|x| x.done < x.total)),
        answer: turn_items.iter().rev().find(|it| it.k == "text").and_then(|it| it.text.as_deref()).map(|x| crate::transcript::short(x.trim(), 700)),
    };
    if turn.calls.is_empty() && start.is_none() && items.is_empty() {
        turn.key.clear();
    }
    let open = t.open_last().map(|ix| &items[ix]);
    let (hit, src_conf) = match open.and_then(|it| item_call(it).map(|c| (it, c))) {
        Some((_, (tool, input))) => (classify(&tool, &input, ctx), 1.0),
        None => match turn_items.iter().rev().find_map(|it| item_call(it)) {
            // between calls: the stretch is still about the last call, less surely
            Some((tool, input)) => {
                let h = classify(&tool, &input, ctx);
                (h, 0.7)
            }
            None => {
                let writing = turn_items.iter().rev().find(|it| it.k != "note").is_some_and(|it| it.k == "text");
                let id = if writing { "writing-reply" } else { "thinking" };
                let t = tax();
                let ph = t.get(id).map(|a| a.phrase.clone()).unwrap_or_else(|| "thinking".into());
                (Hit { id: id.into(), phrase: ph, conf: 0.3 }, 1.0)
            }
        },
    };
    let now = hit.phrase.clone();
    let mut act = Activity { id: hit.id.clone(), phrase: hit.phrase.clone(), conf: hit.conf * src_conf, src: "rules", now };
    // the agent's own words for its current step beat ours (keeps the id/category of the call)
    if let Some(cur) = turn.todo.as_deref() {
        let p = lower_first(&clean(cur, 9));
        if !p.is_empty() {
            act.phrase = p;
            act.conf = act.conf.max(0.9);
            act.src = "todo";
        }
    }
    (Some(act), turn)
}

// ---------- Haiku ----------

/// At most this many Haiku calls per day (activity + outcome).
const MAX_PER_DAY: u32 = 120;
const MAX_PER_HOUR: usize = 30;
/// A turn gets its first Haiku label after running this long.
const TURN_MIN_MS: i64 = 45_000;
const RELABEL_MS: i64 = 10 * 60_000;
const MAX_PER_TURN: u8 = 3;
/// Only label while the phone was visible this recently ("sample only while viewed").
const VIEWED_MS: i64 = 3 * 60_000;
/// Outcomes for turns at least this long, finished at most this long ago.
const OUTCOME_MIN_MS: i64 = 60_000;
const OUTCOME_MAX_AGE_MS: i64 = 3 * 3600_000;

#[derive(Default)]
struct HState {
    loaded: bool,
    enabled: bool,
    day: String,
    calls: u32,
    tok_in: u64,
    tok_out: u64,
    recent: Vec<i64>,
    busy: bool,
    /// turn key → (id, phrase, ts)
    labels: HashMap<String, (String, String, i64)>,
    /// turn key → (asked at, times)
    asked: HashMap<String, (i64, u8)>,
    /// turn key → (outcome text, ts); empty text = asked, nothing useful
    outcomes: HashMap<String, (String, i64)>,
    last_err: Option<String>,
}

static H: Mutex<Option<HState>> = Mutex::new(None);

fn store_path() -> std::path::PathBuf {
    crate::state_dir().join("activity.json")
}

fn today() -> String {
    // local-ish day key: UTC date shifted by the local offset is overkill here; UTC days are fine for a budget
    let d = now_ms() / 86_400_000;
    d.to_string()
}

fn env_off() -> bool {
    std::env::var("SHEPRD_ACTIVITY_HAIKU").is_ok_and(|v| v == "0" || v == "off" || v == "false")
}

fn with_h<R>(f: impl FnOnce(&mut HState) -> R) -> R {
    let mut g = H.lock().unwrap();
    let h = g.get_or_insert_with(HState::default);
    if !h.loaded {
        h.loaded = true;
        let v: Value = std::fs::read(store_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        h.enabled = v["enabled"].as_bool().unwrap_or(true);
        h.day = v["day"].as_str().unwrap_or("").to_string();
        h.calls = v["calls"].as_u64().unwrap_or(0) as u32;
        h.tok_in = v["tokIn"].as_u64().unwrap_or(0);
        h.tok_out = v["tokOut"].as_u64().unwrap_or(0);
        if let Some(m) = v["labels"].as_object() {
            for (k, x) in m {
                if let (Some(id), Some(p), Some(ts)) = (x[0].as_str(), x[1].as_str(), x[2].as_i64()) {
                    h.labels.insert(k.clone(), (id.into(), p.into(), ts));
                }
            }
        }
        if let Some(m) = v["outcomes"].as_object() {
            for (k, x) in m {
                if let (Some(o), Some(ts)) = (x[0].as_str(), x[1].as_i64()) {
                    h.outcomes.insert(k.clone(), (o.into(), ts));
                }
            }
        }
    }
    if h.day != today() {
        h.day = today();
        h.calls = 0;
        h.tok_in = 0;
        h.tok_out = 0;
    }
    f(h)
}

fn save(h: &mut HState) {
    let now = now_ms();
    h.labels.retain(|_, x| now - x.2 < 24 * 3600_000);
    h.outcomes.retain(|_, x| now - x.1 < 48 * 3600_000);
    h.asked.retain(|_, x| now - x.0 < 24 * 3600_000);
    let labels: serde_json::Map<String, Value> = h.labels.iter().map(|(k, x)| (k.clone(), json!([x.0, x.1, x.2]))).collect();
    let outcomes: serde_json::Map<String, Value> = h.outcomes.iter().map(|(k, x)| (k.clone(), json!([x.0, x.1]))).collect();
    let v = json!({"enabled": h.enabled, "day": h.day, "calls": h.calls, "tokIn": h.tok_in, "tokOut": h.tok_out,
                   "labels": labels, "outcomes": outcomes});
    let _ = std::fs::write(store_path(), v.to_string());
}

/// The system prompt for labels: every id Haiku may pick, with its template.
fn label_system() -> &'static str {
    static S: LazyLock<String> = LazyLock::new(|| {
        let t = tax();
        let mut s = String::from(
            "You label what an AI coding agent is doing right now, for a one-line status on its owner's phone \
             (\"<agent> is <phrase>\"). Pick the ONE id below that best fits what the agent is working on in this \
             turn, and fill every {slot} of its phrase with short, specific, plain words: {what} is the subject of \
             the work, taken from the request and the tool calls (2-6 words, e.g. \"the chat bubble colors\", \
             \"the Friday check-in timers\"). No file paths, no quotes, no trailing period; keep product and project \
             names. Prefer the work over the tool mechanics (\"styling the chat bubbles\" beats \"editing app.css\"). \
             Reply with JSON only: {\"id\": \"<id>\", \"slots\": {\"<slot>\": \"<words>\"}}.\n\nid: phrase\n",
        );
        for a in &t.acts {
            let tpl = a.ask.as_deref().unwrap_or(&a.phrase);
            if FALLBACK.contains(&a.id.as_str()) || a.id.starts_with("desc-") || tpl.contains("{desc_ing") || tpl.contains("{agent") || tpl.contains("{file") {
                continue;
            }
            s.push_str(&format!("{}: {tpl}\n", a.id));
        }
        s
    });
    &S
}

const OUTCOME_SYSTEM: &str = "You write a short outcome line for an AI coding agent's finished turn, shown on its \
    owner's phone after the agent's name (\"<agent> <outcome>\"). Past tense, 3-9 words, no subject, no trailing \
    period, specific and plain (e.g. \"restyled the chat bubbles\", \"found the flaky test\", \
    \"fixed the login redirect\"). If the agent ended by asking a question or could not finish, say so \
    (\"asked which colour to use\", \"got stuck on a failing migration\"). Reply with JSON only: {\"outcome\": \"...\"}.";

fn label_prompt(title: &str, cwd: &str, turn: &Turn) -> String {
    let folder = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let mut p = format!("Agent: {title}\nFolder: {folder}\nThe request that started this turn:\n<<<\n{}\n>>>\n", turn.prompt);
    if let Some(t) = &turn.todo {
        p.push_str(&format!("Current todo step: {t}\n"));
    }
    if !turn.calls.is_empty() {
        p.push_str("Latest tool calls (newest last):\n");
        for c in &turn.calls {
            p.push_str(&format!("- {c}\n"));
        }
    }
    p
}

fn outcome_prompt(title: &str, turn: &Turn) -> String {
    format!(
        "Agent: {title}\nRequest:\n<<<\n{}\n>>>\nThe agent's final message:\n<<<\n{}\n>>>",
        turn.prompt,
        turn.answer.as_deref().unwrap_or("")
    )
}

fn json_in(s: &str) -> Option<Value> {
    let a = s.find('{')?;
    let b = s.rfind('}')?;
    serde_json::from_str(s.get(a..=b)?).ok()
}

/// Haiku's {id, slots} → (id, phrase), checked against the taxonomy.
pub fn parse_label(reply: &str) -> Option<(String, String)> {
    let v = json_in(reply)?;
    let id = v["id"].as_str()?.trim().to_string();
    let t = tax();
    let a = t.get(&id)?;
    let tpl = a.ask.as_deref().unwrap_or(&a.phrase);
    if tpl.contains("{desc_ing") || tpl.contains("{agent") {
        return None;
    }
    let mut slots = HashMap::new();
    if let Some(m) = v["slots"].as_object() {
        for (k, x) in m {
            if let Some(x) = x.as_str() {
                let x = clean(&x.replace(['\n', '/', '\\', '{', '}'], " "), 7);
                if !x.is_empty() && x.len() <= 48 {
                    slots.insert(k.clone(), x);
                }
            }
        }
    }
    // {what} and {thing} are the same idea: take either
    for (x, y) in [("what", "thing"), ("thing", "what")] {
        if !slots.contains_key(x) {
            if let Some(v) = slots.get(y).cloned() {
                slots.insert(x.to_string(), v);
            }
        }
    }
    let p = render(tpl, &slots, true).filter(|p| p.len() <= MAX_PHRASE).or_else(|| render(tpl, &slots, false))?;
    Some((id, p))
}

pub fn parse_outcome(reply: &str) -> Option<String> {
    let v = json_in(reply)?;
    let o = v["outcome"].as_str()?.trim().trim_end_matches('.').trim();
    let words = o.split_whitespace().count();
    (o.is_empty() || (words <= 12 && o.len() <= 80)).then(|| lower_first(o))
}

/// What refresh knows about one agent, for `decorate`.
pub struct Cand {
    pub pane: String,
    pub act: Option<Activity>,
    pub turn: Turn,
}

/// Adds `activity` (working agents) and `outcome` (finished turns) to the state, and starts at most one
/// Haiku call when one is due. Runs after the other decorations (home/service flags are set).
pub fn decorate(app: &crate::Shared, agents: &mut [Value], cands: Vec<Cand>) {
    let now = now_ms();
    let viewed = now - app.inner.lock().unwrap().last_visible < VIEWED_MS;
    let by: HashMap<&str, &Cand> = cands.iter().map(|c| (c.pane.as_str(), c)).collect();
    let mut job: Option<(bool, String, String, String, Turn)> = None; // (is_outcome, pane, title, cwd, turn)
    with_h(|h| {
        let can_call = viewed && h.enabled && !env_off() && !h.busy && h.calls < MAX_PER_DAY && {
            h.recent.retain(|t| now - *t < 3600_000);
            h.recent.len() < MAX_PER_HOUR
        };
        for a in agents.iter_mut() {
            let pane = a["pane"].as_str().unwrap_or("").to_string();
            let pane = pane.as_str();
            let Some(c) = by.get(pane) else { continue };
            let status = a["status"].as_str().unwrap_or("").to_string();
            let status = status.as_str();
            let busy = status == "working" || status == "blocked";
            let eligible = a["home"] != true && a["service"].is_null() && a["kind"] == "claude" && !c.turn.key.is_empty();
            if busy {
                let Some(mut act) = c.act.clone() else { continue };
                if let Some((id, phrase, _)) = h.labels.get(&c.turn.key).filter(|_| act.src != "todo") {
                    act.id = id.clone();
                    act.phrase = phrase.clone();
                    act.conf = act.conf.max(0.85);
                    act.src = "haiku";
                }
                a["activity"] = act.json();
                if can_call && job.is_none() && eligible && status == "working" && act.src != "todo" && now - c.turn.start_ts >= TURN_MIN_MS {
                    let asked = h.asked.get(&c.turn.key).copied();
                    let due = match asked {
                        None => true,
                        Some((at, n)) => n < MAX_PER_TURN && now - at >= RELABEL_MS,
                    };
                    if due {
                        job = Some((false, pane.to_string(), a["title"].as_str().unwrap_or("").to_string(), a["cwd"].as_str().unwrap_or("").to_string(), c.turn.clone()));
                    }
                }
            } else if let Some((o, _)) = h.outcomes.get(&c.turn.key).filter(|o| !o.0.is_empty()) {
                a["outcome"] = json!({"text": o, "ts": c.turn.last_ts});
            } else if can_call
                && job.is_none()
                && eligible
                && (status == "idle" || status == "done")
                && !h.outcomes.contains_key(&c.turn.key)
                && c.turn.answer.is_some()
                && c.turn.last_ts - c.turn.start_ts >= OUTCOME_MIN_MS
                && now - c.turn.last_ts <= OUTCOME_MAX_AGE_MS
            {
                job = Some((true, pane.to_string(), a["title"].as_str().unwrap_or("").to_string(), String::new(), c.turn.clone()));
            }
        }
        if let Some((is_outcome, ..)) = &job {
            let key = &job.as_ref().unwrap().4.key;
            h.busy = true;
            h.calls += 1;
            h.recent.push(now);
            if *is_outcome {
                h.outcomes.insert(key.clone(), (String::new(), now));
            } else {
                let e = h.asked.entry(key.clone()).or_insert((now, 0));
                e.0 = now;
                e.1 += 1;
            }
        }
    });
    let Some((is_outcome, pane, title, cwd, turn)) = job else { return };
    let app = app.clone();
    tokio::spawn(async move {
        let (sys, prompt) = if is_outcome {
            (OUTCOME_SYSTEM.to_string(), outcome_prompt(&title, &turn))
        } else {
            (label_system().to_string(), label_prompt(&title, &cwd, &turn))
        };
        let r = run(&sys, &prompt, is_outcome).await;
        with_h(|h| {
            h.busy = false;
            match &r {
                Ok((text, tin, tout)) => {
                    h.tok_in += tin;
                    h.tok_out += tout;
                    h.last_err = None;
                    if is_outcome {
                        if let Some(o) = parse_outcome(text) {
                            h.outcomes.insert(turn.key.clone(), (o, now_ms()));
                        }
                    } else if let Some((id, phrase)) = parse_label(text) {
                        h.labels.insert(turn.key.clone(), (id, phrase, now_ms()));
                    }
                }
                Err(e) => {
                    eprintln!("activity haiku {pane}: {e}");
                    h.last_err = Some(e.clone());
                }
            }
            save(h);
        });
        app.refresh.notify_one();
    });
}

/// One Haiku call → (reply text, input tokens incl. cache, output tokens).
async fn run(system: &str, prompt: &str, is_outcome: bool) -> Result<(String, u64, u64), String> {
    if let Ok(fake) = std::env::var(if is_outcome { "SHEPRD_FAKE_OUTCOME" } else { "SHEPRD_FAKE_ACTIVITY" }) {
        return Ok((fake, 0, 0));
    }
    let v = crate::newagent::run_haiku(Some(system), prompt, Duration::from_secs(60)).await?;
    let u = &v["usage"];
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let tin = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
    Ok((v["result"].as_str().unwrap_or("").to_string(), tin, n("output_tokens")))
}

// ---------- API ----------

pub fn routes() -> Router<crate::Shared> {
    Router::new().route("/api/activity", get(api_get).post(api_set))
}

fn stats() -> Value {
    with_h(|h| {
        json!({
            "haiku": h.enabled && !env_off(), "enabled": h.enabled, "envOff": env_off(),
            "today": {"calls": h.calls, "tokensIn": h.tok_in, "tokensOut": h.tok_out},
            "maxPerDay": MAX_PER_DAY, "maxPerHour": MAX_PER_HOUR,
            "labels": h.labels.len(), "outcomes": h.outcomes.values().filter(|o| !o.0.is_empty()).count(),
            "lastError": h.last_err,
            "taxonomy": {"activities": tax().acts.len(),
                "categories": tax().cats.iter().map(|(k, c)| json!({"id": k, "label": c.label, "icon": c.icon})).collect::<Vec<_>>()},
        })
    })
}

async fn api_get() -> Response {
    Json(stats()).into_response()
}

/// `{haiku: false}` turns the Haiku pass off (kept across restarts); rules keep working.
async fn api_set(State(app): State<crate::Shared>, Json(b): Json<Value>) -> Response {
    if let Some(on) = b["haiku"].as_bool() {
        with_h(|h| {
            h.enabled = on;
            save(h);
        });
        app.refresh.notify_one();
    }
    Json(stats()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx<'static> {
        Ctx { title: "Agent", cwd: "/home/u/Work/sheprd" }
    }

    #[test]
    fn taxonomy_compiles() {
        let t = tax();
        assert!(t.acts.len() >= 150, "{} activities", t.acts.len());
        assert!((12..=16).contains(&t.cats.len()));
        for a in &t.acts {
            assert!(!a.phrase.is_empty(), "{}", a.id);
            assert!(a.phrase == a.phrase.to_lowercase() || a.phrase.contains('{') || a.phrase.chars().any(|c| c.is_uppercase()), "{}", a.id);
        }
        // the Haiku prompt stays small
        assert!(label_system().len() < 9000, "{}", label_system().len());
    }

    #[test]
    fn normalises_commands() {
        assert_eq!(norm_cmd("cd ~/x && FOO=1 timeout 300 cargo test 2>&1 | grep -E \"a|b\""), "cargo test 2>&1\ngrep -E \"a|b\"");
        assert_eq!(norm_cmd("export AGENT_BROWSER_SESSION=s; agent-browser open \"https://x.com\""), "agent-browser open \"https://x.com\"");
        assert_eq!(norm_cmd("for a in 1 2; do agent-browser click @e1; done"), "for a in 1 2\nagent-browser click @e1");
        assert_eq!(norm_cmd("sleep 30"), "sleep 30");
    }

    #[test]
    fn gerunds() {
        for (v, g) in [("run", "running"), ("write", "writing"), ("tidy", "tidying"), ("commit", "committing"), ("see", "seeing"), ("fix", "fixing"), ("tie", "tying")] {
            assert_eq!(gerund(v), g);
        }
        assert_eq!(desc_parts("Read API.md push and voice sections").0, "reading API.md push and voice sections");
        assert_eq!(desc_parts("PNY 50-series listing").0, "");
        assert_eq!(desc_parts("Commit the Pi tray CAD and docs").1, "the Pi tray CAD and docs");
    }

    #[test]
    fn renders_templates() {
        let m: HashMap<String, String> = [("site".to_string(), "Example".to_string())].into();
        assert_eq!(render("reading[ {item}] docs[ on {site}]", &m, true).unwrap(), "reading docs on Example");
        assert_eq!(render("waiting for {thing|something}", &m, true).unwrap(), "waiting for something");
        assert_eq!(render("editing {file}", &m, true), None);
        assert_eq!(render("reading the {site|} docs", &HashMap::new(), true).unwrap(), "reading the docs");
    }

    #[test]
    fn sites_and_items() {
        assert_eq!(site_name("www.huggingface.co"), "Hugging Face");
        assert_eq!(site_name("www.example.com"), "Example");
        assert_eq!(site_name("127.0.0.1"), "the local app");
        assert_eq!(item_of("https://www.example.com/search?q=rtx+5070+ti&intl=nosplash"), "rtx 5070 ti");
    }

    /// Recorded tool calls (generic, from real sessions) → expected activity id and phrase.
    #[test]
    fn classifies_fixtures() {
        let src = include_str!("../fixtures/activity/calls.jsonl");
        let mut bad = Vec::new();
        for line in src.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).expect(line);
            let ctx = Ctx { title: v["title"].as_str().unwrap_or("Agent"), cwd: v["cwd"].as_str().unwrap_or("/home/u/Work/sheprd") };
            let h = classify(v["tool"].as_str().unwrap(), &v["input"], &ctx);
            let want_id = v["id"].as_str().unwrap();
            let want_phrase = v["phrase"].as_str();
            if h.id != want_id || want_phrase.is_some_and(|p| p != h.phrase) {
                bad.push(format!("{} → {} / {:?} (want {} / {:?})", v["input"], h.id, h.phrase, want_id, want_phrase));
            }
        }
        assert!(bad.is_empty(), "\n{}", bad.join("\n"));
    }

    #[test]
    fn haiku_replies() {
        assert_eq!(
            parse_label("```json\n{\"id\": \"edit-css\", \"slots\": {\"thing\": \"the chat bubbles\"}}\n```"),
            Some(("edit-css".into(), "styling the chat bubbles".into()))
        );
        assert_eq!(parse_label("{\"id\": \"no-such\", \"slots\": {}}"), None);
        assert_eq!(parse_label("{\"id\": \"fix-bug\", \"slots\": {\"what\": \"the login redirect\"}}").unwrap().1, "fixing the login redirect");
        assert_eq!(parse_label("{\"id\": \"fix-bug\", \"slots\": {}}"), None); // every slot must be filled
        assert_eq!(parse_label("{\"id\": \"desc-read\", \"slots\": {\"desc_ing\": \"x\"}}"), None);
        assert_eq!(parse_outcome("{\"outcome\": \"Restyled the chat bubbles.\"}"), Some("restyled the chat bubbles".into()));
        assert_eq!(parse_outcome("{\"outcome\": \"\"}"), Some(String::new()));
        assert_eq!(parse_outcome("nope"), None);
    }

    #[test]
    fn turn_activity() {
        let dir = std::env::temp_dir().join(format!("mm-act-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let lines = [
            json!({"type":"user","timestamp":"2026-10-05T10:00:00Z","message":{"role":"user","content":"restyle the chat bubbles"}}),
            json!({"type":"assistant","timestamp":"2026-10-05T10:00:05Z","message":{"role":"assistant","content":[
                {"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/home/u/Work/sheprd/web/style.css","old_string":"a","new_string":"b"}}]}}),
            json!({"type":"user","timestamp":"2026-10-05T10:00:06Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}),
            json!({"type":"assistant","timestamp":"2026-10-05T10:00:09Z","message":{"role":"assistant","content":[
                {"type":"tool_use","id":"t2","name":"Bash","input":{"command":"cd server && cargo test 2>&1 | tail -3","description":"Run the server tests"}}]}}),
        ];
        std::fs::write(&p, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
        let mut t = Transcript::new("sid".into(), p.clone());
        t.refresh();
        let (a, turn) = of_transcript(&t, None, &ctx());
        let a = a.unwrap();
        assert_eq!(a.id, "test-rust");
        assert_eq!(a.phrase, "running the sheprd tests");
        assert_eq!(turn.prompt, "restyle the chat bubbles");
        assert_eq!(turn.calls.len(), 2);
        assert!(turn.key.starts_with("sid:"));
        // the call finishes: between calls the last call still describes the stretch, less surely
        let more = json!({"type":"user","timestamp":"2026-10-05T10:00:30Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}});
        std::fs::write(&p, lines.iter().chain([&more]).map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
        t.refresh();
        let (a, _) = of_transcript(&t, None, &ctx());
        let a = a.unwrap();
        assert_eq!(a.id, "test-rust");
        assert!(a.conf < 0.7);
        // a todo step in progress is the phrase
        let todo = Todo { done: 1, total: 3, cur: Some("Restyling the chat bubbles".into()), ts: 0 };
        let (a, _) = of_transcript(&t, Some(&todo), &ctx());
        assert_eq!(a.unwrap().phrase, "restyling the chat bubbles");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Before → after on real transcripts, with one Haiku label and one outcome each (costs tokens):
    /// `ACT_LIVE="<title>|<cwd>|<transcript.jsonl>;..." cargo test live_examples -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_examples() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for spec in std::env::var("ACT_LIVE").expect("ACT_LIVE").split(';').filter(|s| !s.is_empty()) {
            let mut it = spec.splitn(3, '|');
            let (title, cwd, path) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
            let mut t = Transcript::new("x".into(), path.into());
            t.refresh();
            let raw = t.doing().map(|d| d.0).unwrap_or_default();
            let (act, turn) = of_transcript(&t, t.todo.as_ref(), &Ctx { title, cwd });
            let act = act.unwrap();
            println!("== {title}\n   raw:   {raw}\n   rules: {} ({}, {:.2}, {})", act.phrase, act.id, act.conf, act.src);
            let r = rt.block_on(run(label_system(), &label_prompt(title, cwd, &turn), false));
            match r {
                Ok((text, tin, tout)) => println!("   haiku: {:?} [{tin} in / {tout} out] {}", parse_label(&text), text.replace('\n', " ")),
                Err(e) => println!("   haiku error: {e}"),
            }
            if turn.answer.is_some() {
                if let Ok((text, tin, tout)) = rt.block_on(run(OUTCOME_SYSTEM, &outcome_prompt(title, &turn), true)) {
                    println!("   outcome: {:?} [{tin} in / {tout} out]", parse_outcome(&text));
                }
            }
        }
    }

    /// Coverage over real tool calls: `ACT_CALLS=calls.jsonl cargo test coverage -- --ignored --nocapture`
    /// (one `{"name", "in"}` object per line, e.g. extracted from ~/.claude/projects).
    #[test]
    #[ignore]
    fn coverage() {
        let path = std::env::var("ACT_CALLS").expect("ACT_CALLS");
        let src = std::fs::read_to_string(path).unwrap();
        let (mut n, mut strict, mut loose) = (0usize, 0usize, 0usize);
        let mut by: BTreeMap<String, usize> = BTreeMap::new();
        let mut misses: Vec<String> = Vec::new();
        let mut cats: BTreeMap<String, usize> = BTreeMap::new();
        for line in src.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let tool = v["name"].as_str().unwrap_or("");
            let h = classify(tool, &v["in"], &ctx());
            n += 1;
            let only = std::env::var("ACT_ID").ok();
            if std::env::var("ACT_SAMPLE").is_ok_and(|k| n % k.parse::<usize>().unwrap_or(50) == 0 && only.as_ref().is_none_or(|o| *o == h.id)) {
                println!("SAMPLE {:18} {:48} | {} | {}", h.id, h.phrase, v["in"]["description"].as_str().unwrap_or(""), crate::transcript::short(&v["in"]["command"].as_str().or(v["in"]["file_path"].as_str()).unwrap_or("").replace('\n', " ⏎ "), 90));
            }
            *by.entry(h.id.clone()).or_default() += 1;
            *cats.entry(tax().get(&h.id).map(|a| a.cat.clone()).unwrap_or_default()).or_default() += 1;
            if h.classified() {
                loose += 1;
                if !h.id.starts_with("desc-") {
                    strict += 1;
                }
            } else if misses.len() < 400 {
                misses.push(format!("{tool} | {} | {}", v["in"]["description"].as_str().unwrap_or(""), crate::transcript::short(&v["in"]["command"].as_str().unwrap_or("").replace('\n', " ⏎ "), 140)));
            }
        }
        println!("calls {n}: taxonomy rules {:.1}%, incl. description fallbacks {:.1}%", strict as f64 * 100.0 / n as f64, loose as f64 * 100.0 / n as f64);
        let mut v: Vec<_> = by.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (id, c) in v.iter().take(80) {
            println!("{c:6} {id}");
        }
        println!("categories: {cats:?}");
        println!("--- unclassified sample");
        for m in misses.iter().take(120) {
            println!("{m}");
        }
    }
}
