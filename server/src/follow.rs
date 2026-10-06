//! Which Claude session a pane is showing *now*.
//!
//! herdr's `agent_session` is fed only by the SessionStart hook. A conversation that
//! continues under a new session id without that hook running in the pane leaves herdr
//! pointing at the old transcript forever, so the phone tailed a dead file. Claude's
//! background sessions do this: the agents view (`←`) and `/background` hand the
//! conversation to a daemon job (`--session-id <new> --fork-session --resume <old>.jsonl`)
//! and the pane's TUI stays attached to it (or, after `/background`, `claude attach <id>`
//! reattaches it); no SessionStart runs in the pane.
//!
//! Sources, most reliable first:
//! 1. The `continued-in` record Claude appends to the old transcript at a handoff
//!    (`{"type":"continued-in","continuedInSessionId":<new>}`): written by Claude itself,
//!    exact, works for every session with or without our mod, and absent for `/fork`
//!    (a side copy while the pane keeps working in the old session). It only counts while
//!    it is the old transcript's last word: conversation records after it mean the old
//!    session was resumed again. Followed in a chain (a handoff of a handoff). A marker
//!    older than the moment the server saw the pane switch to that session is ignored:
//!    `claude --resume <old>` after a handoff shows the old session at once, not only after
//!    its first new message.
//! 2. herdr's id (SessionStart: new session, `/clear`, a plain `--resume`, which keeps the
//!    id).
//! 3. A pane where herdr sees Claude but no session (`claude attach <id>`): the process
//!    table. The Claude process with that pane's `HERDR_PANE_ID` (and our herdr socket)
//!    names the session on its command line; the job state maps the short id to the full
//!    one.
//!
//! Not used: the newest transcript sharing the conversation's root uuid (a `/fork` copy
//! looks the same), the mod's polls and `--session-id` processes (a `/fork` job inherits
//! the pane's environment), file mtimes (Claude rewrites an old transcript's cost record).

use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How much of a transcript's end is searched for a trailing `continued-in`.
const TAIL_BYTES: u64 = 256 * 1024;
/// Longest handoff chain followed.
const MAX_HOPS: usize = 8;

fn valid_sid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// A top-level user/assistant record: the conversation went on in this file.
fn is_turn(line: &str) -> bool {
    if !line.contains("\"type\":\"user\"") && !line.contains("\"type\":\"assistant\"") {
        return false;
    }
    serde_json::from_str::<Value>(line).is_ok_and(|v| v["type"] == "user" || v["type"] == "assistant")
}

/// The session the conversation continued in and when (unix ms), from a transcript's
/// tail: the last `continued-in` record, unless conversation records follow it.
pub fn continued_in(tail: &str) -> Option<(String, i64)> {
    let mut next = None;
    for line in tail.lines() {
        if line.starts_with("{\"type\":\"continued-in\"") {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(s) = v["continuedInSessionId"].as_str().filter(|s| valid_sid(s)) {
                    next = Some((s.to_string(), crate::transcript::parse_ts(v["timestamp"].as_str().unwrap_or(""))));
                }
            }
        } else if next.is_some() && is_turn(line) {
            next = None;
        }
    }
    next
}

fn read_tail(p: &Path, len: u64) -> Option<String> {
    let mut f = std::fs::File::open(p).ok()?;
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(TAIL_BYTES).read_to_end(&mut buf).ok()?;
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // Drop the partial first line.
        s = s.split_once('\n').map(|(_, r)| r.to_string()).unwrap_or_default();
    }
    Some(s)
}

/// The session id named by a Claude command line `claude attach <id or short id>`.
pub fn attach_arg(args: &[String]) -> Option<String> {
    let bin = args.iter().position(|a| a.rsplit('/').next() == Some("claude"))?;
    match &args[bin + 1..] {
        [cmd, id, ..] if cmd == "attach" && valid_sid(id) => Some(id.clone()),
        _ => None,
    }
}

/// Session ids (or short ids) that `claude attach` processes in herdr pane `pane` of the
/// herdr server at `socket` show.
fn attached_in(pane: &str, socket: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    let want_pane = format!("HERDR_PANE_ID={pane}");
    let want_sock = format!("HERDR_SOCKET_PATH={}", socket.display());
    for e in rd.flatten() {
        if !e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(cmd) = std::fs::read(e.path().join("cmdline")) else { continue };
        let args: Vec<String> =
            cmd.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        let Some(id) = attach_arg(&args) else { continue };
        let Ok(env) = std::fs::read(e.path().join("environ")) else { continue };
        let has = |w: &str| env.split(|b| *b == 0).any(|v| v == w.as_bytes());
        if has(&want_pane) && has(&want_sock) && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

pub struct Follow {
    /// Claude's background job states (`~/.claude/jobs/<short id>/state.json`).
    jobs: PathBuf,
    /// transcript -> (length when read, where it continued and when)
    next: Mutex<HashMap<PathBuf, (u64, Option<(String, i64)>)>>,
    /// pane -> (session it showed, since when; 0 = since before the server started)
    seen: Mutex<HashMap<String, (String, i64)>>,
    /// transcripts known to hold a conversation record
    turns: Mutex<std::collections::HashSet<PathBuf>>,
    born: i64,
    /// pane -> (when, session) from the process table (throttled)
    orphans: Mutex<HashMap<String, (i64, Option<String>)>>,
}

impl Default for Follow {
    fn default() -> Self {
        Follow::new(crate::home().join(".claude/jobs"))
    }
}

impl Follow {
    pub fn new(jobs: PathBuf) -> Self {
        Follow { jobs, next: Mutex::default(), orphans: Mutex::default(), seen: Mutex::default(), turns: Mutex::default(), born: crate::now_ms() }
    }

    /// Since when `pane` shows `sid` (herdr's or the attached one), as far as the server saw:
    /// 0 for what it found at startup.
    pub fn since(&self, pane: &str, sid: &str, now: i64) -> i64 {
        let mut seen = self.seen.lock().unwrap();
        match seen.get(pane) {
            Some((s, t)) if s == sid => *t,
            _ => {
                let t = if now - self.born < 10_000 { 0 } else { now };
                seen.insert(pane.to_string(), (sid.to_string(), t));
                t
            }
        }
    }

    /// Where the conversation in transcript `p` continued, if it did (cached per length).
    fn next_of(&self, p: &Path) -> Option<(String, i64)> {
        let len = std::fs::metadata(p).ok()?.len();
        if let Some((l, n)) = self.next.lock().unwrap().get(p) {
            if *l == len {
                return n.clone();
            }
        }
        let n = read_tail(p, len).and_then(|t| continued_in(&t));
        self.next.lock().unwrap().insert(p.to_path_buf(), (len, n.clone()));
        n
    }

    /// True once transcript `p` holds a conversation record (a handoff target gets the copied
    /// history with its first message; until then it only has its title).
    fn has_turn(&self, p: &Path) -> bool {
        if self.turns.lock().unwrap().contains(p) {
            return true;
        }
        let Ok(f) = std::fs::File::open(p) else { return false };
        let found = std::io::BufRead::lines(std::io::BufReader::new(f.take(TAIL_BYTES))).map_while(Result::ok).any(|l| is_turn(&l));
        if found {
            self.turns.lock().unwrap().insert(p.to_path_buf());
        }
        found
    }

    /// The live end of `sid`'s handoff chain. `find` finds a session's transcript; a target
    /// without a conversation record yet (no message since the handoff) is not followed:
    /// the old transcript holds the same conversation, the new one would show empty.
    /// Forgets panes that run no agent now: a Claude started there later (e.g. `claude
    /// --resume <old>` after the TUI quit) counts as switched at that moment.
    pub fn retain(&self, agent_panes: &[String]) {
        self.seen.lock().unwrap().retain(|p, _| agent_panes.contains(p));
        self.orphans.lock().unwrap().retain(|p, _| agent_panes.contains(p));
    }

    /// Handoffs of `sid` itself count only from `since` (unix ms) on.
    pub fn current(&self, sid: &str, since: i64, find: &dyn Fn(&str) -> Option<PathBuf>) -> String {
        let mut cur = sid.to_string();
        let mut seen = vec![cur.clone()];
        for _ in 0..MAX_HOPS {
            let Some(p) = find(&cur) else { break };
            let Some((n, ts)) = self.next_of(&p) else { break };
            if seen.contains(&n) || !find(&n).is_some_and(|p| self.has_turn(&p)) || (cur == sid && ts < since) {
                break;
            }
            seen.push(n.clone());
            cur = n;
        }
        cur
    }

    /// The full session id of a background job's short id (or of a transcript by prefix).
    fn full_id(&self, short: &str, find_prefix: &dyn Fn(&str) -> Option<String>) -> Option<String> {
        if short.len() >= 36 {
            return Some(short.to_string());
        }
        let job = std::fs::read(self.jobs.join(short).join("state.json")).ok();
        job.and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["sessionId"].as_str().filter(|s| s.starts_with(short) && valid_sid(s)).map(str::to_string))
            .or_else(|| find_prefix(short))
    }

    /// The session of a Claude pane herdr knows no session for (see module docs),
    /// re-read from the process table at most every few seconds.
    pub fn orphan(&self, pane: &str, socket: &Path, now: i64, find_prefix: &dyn Fn(&str) -> Option<String>) -> Option<String> {
        if let Some((ts, sid)) = self.orphans.lock().unwrap().get(pane) {
            if now - ts < 5_000 {
                return sid.clone();
            }
        }
        let sid = attached_in(pane, socket).iter().find_map(|id| self.full_id(id, find_prefix));
        self.orphans.lock().unwrap().insert(pane.to_string(), (now, sid.clone()));
        sid
    }
}

/// The newest transcript under `projects` whose session id starts with `prefix`.
pub fn by_prefix(projects: &Path, prefix: &str) -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for d in std::fs::read_dir(projects).into_iter().flatten().flatten() {
        for e in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
            let p = e.path();
            let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
            if !stem.starts_with(prefix) || p.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            let m = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(bm, _)| m > *bm) {
                best = Some((m, stem));
            }
        }
    }
    best.map(|(_, s)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARK_B: &str = r#"{"type":"continued-in","timestamp":"2026-10-05T19:53:09.262Z","sessionId":"aaaa","continuedInSessionId":"bbbb"}"#;
    const USER: &str = r#"{"parentUuid":null,"type":"user","message":{"role":"user","content":"hi"},"uuid":"u1"}"#;
    const ASSIST: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[]},"uuid":"u2"}"#;
    const COST: &str = r#"{"type":"cost-state","sessionId":"aaaa","totalCostUSD":9.1}"#;

    #[test]
    fn trailing_marker_is_followed() {
        // The Master's file: turns, cost record, then the handoff.
        assert_eq!(continued_in(&[USER, ASSIST, COST, MARK_B].join("\n")), Some(("bbbb".into(), 1791229989262)));
        // Bookkeeping after the marker does not cancel it.
        let mode = r#"{"type":"mode","mode":"normal","sessionId":"aaaa"}"#;
        assert_eq!(continued_in(&[USER, MARK_B, mode].join("\n")).map(|n| n.0).as_deref(), Some("bbbb"));
    }

    #[test]
    fn resumed_after_marker_stays() {
        // `claude --resume aaaa` after a handoff: the conversation went on in aaaa.
        assert_eq!(continued_in(&[USER, MARK_B, USER, ASSIST].join("\n")), None);
        // ... and a later handoff counts again (last one wins).
        let mark_c = MARK_B.replace("bbbb", "cccc");
        assert_eq!(continued_in(&[MARK_B, USER, mark_c.as_str()].join("\n")).map(|n| n.0).as_deref(), Some("cccc"));
    }

    #[test]
    fn no_marker_or_quoted_marker() {
        // `/fork` writes no marker; a marker quoted inside a tool result is not a record.
        assert_eq!(continued_in(&[USER, ASSIST].join("\n")), None);
        let quoted = r#"{"type":"user","message":{"content":"{\"type\":\"continued-in\",\"continuedInSessionId\":\"dddd\"}"}}"#;
        assert_eq!(continued_in(quoted), None);
        let bad = MARK_B.replace("bbbb", "../etc");
        assert_eq!(continued_in(&bad), None);
    }

    #[test]
    fn since_is_zero_at_startup_then_switch_time() {
        let mut f = Follow::new(PathBuf::from("/nonexistent"));
        assert_eq!(f.since("p", "a", f.born + 1), 0);
        assert_eq!(f.since("p", "a", f.born + 60_000), 0);
        f.born -= 60_000;
        let now = f.born + 70_000;
        assert_eq!(f.since("p", "b", now), now);
        assert_eq!(f.since("p", "b", now + 5), now);
    }

    #[test]
    fn attach_command_line() {
        let v = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert_eq!(attach_arg(&v("/x/claude attach 5ef63830")).as_deref(), Some("5ef63830"));
        assert_eq!(attach_arg(&v("/x/claude --session-id 92fa72b4 --fork-session")), None);
        assert_eq!(attach_arg(&v("/x/claude --permission-mode auto")), None);
        assert_eq!(attach_arg(&v("vim attach 5ef6")), None);
        assert_eq!(attach_arg(&v("/x/claude attach ../etc")), None);
    }

    #[test]
    fn chain_on_disk() {
        let dir = std::env::temp_dir().join(format!("mm-follow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("jobs/5ef63830")).unwrap();
        let mark = |from: &str, to: &str| MARK_B.replace("aaaa", from).replace("bbbb", to);
        // aaaa -> bbbb -> cccc (no file yet: no message since the handoff); dddd resumed after its handoff.
        let pad = format!("{{\"type\":\"pad\",\"x\":\"{}\"}}", "x".repeat(300 * 1024));
        std::fs::write(dir.join("aaaa.jsonl"), [USER, pad.as_str(), &mark("aaaa", "bbbb")].join("\n") + "\n").unwrap();
        std::fs::write(dir.join("bbbb.jsonl"), [USER, &mark("bbbb", "cccc")].join("\n") + "\n").unwrap();
        std::fs::write(dir.join("dddd.jsonl"), [USER, &mark("dddd", "eeee"), USER].join("\n") + "\n").unwrap();
        // A loop must end.
        std::fs::write(dir.join("ffff.jsonl"), mark("ffff", "ffff") + "\n").unwrap();
        let f = Follow::new(dir.join("jobs"));
        let find = |s: &str| Some(dir.join(format!("{s}.jsonl"))).filter(|p| p.exists());
        assert_eq!(f.current("aaaa", 0, &find), "bbbb");
        // Only a title so far: still not followed.
        std::fs::write(dir.join("cccc.jsonl"), "{\"type\":\"ai-title\",\"aiTitle\":\"x\"}\n").unwrap();
        assert_eq!(f.current("aaaa", 0, &find), "bbbb");
        std::fs::write(dir.join("cccc.jsonl"), USER.to_string() + "\n").unwrap();
        assert_eq!(f.current("aaaa", 0, &find), "cccc");
        assert_eq!(f.current("dddd", 0, &find), "dddd");
        assert_eq!(f.current("ffff", 0, &find), "ffff");
        assert_eq!(f.current("zzzz", 0, &find), "zzzz");
        // aaaa was resumed in a pane after its handoff (seen since later than the marker).
        assert_eq!(f.current("aaaa", 1791229989263, &find), "aaaa");
        assert_eq!(f.current("aaaa", 1791229989262, &find), "cccc");
        // bbbb grows with a new turn (resumed): the cache notices.
        std::fs::write(dir.join("bbbb.jsonl"), [USER, &mark("bbbb", "cccc"), ASSIST].join("\n") + "\n").unwrap();
        assert_eq!(f.current("aaaa", 0, &find), "bbbb");
        // Short job ids resolve through the job state.
        let full = "5ef63830-61d8-4354-99f5-c75e88d6888f";
        std::fs::write(dir.join("jobs/5ef63830/state.json"), format!("{{\"sessionId\":\"{full}\"}}")).unwrap();
        assert_eq!(f.full_id("5ef63830", &|_| None).as_deref(), Some(full));
        assert_eq!(f.full_id("12345678", &|p| Some(format!("{p}-x"))).as_deref(), Some("12345678-x"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
