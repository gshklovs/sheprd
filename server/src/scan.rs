//! Streaming scans over many transcripts (recent prompts, global search): read
//! line by line, parse only candidate lines, keep nothing cached.

use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

const MAX_LINE: usize = 256 * 1024;

/// Main transcripts (`~/.claude/projects/*/<sid>.jsonl`), newest first, with mtime.
pub fn main_transcripts() -> Vec<(PathBuf, i64)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(crate::home().join(".claude/projects")) else { return out };
    for d in rd.flatten() {
        let Ok(files) = std::fs::read_dir(d.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().is_some_and(|e| e == "jsonl") {
                if let Ok(m) = f.metadata() {
                    out.push((p, crate::transcript::mtime_ms(&m)));
                }
            }
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

/// Calls `f(offset, line)` for every line of `path` up to `MAX_LINE` bytes
/// (longer ones are skipped). `f` returns false to stop.
fn each_line(path: &Path, mut f: impl FnMut(u64, &[u8]) -> bool) {
    let Ok(file) = File::open(path) else { return };
    let mut r = BufReader::with_capacity(64 * 1024, file);
    let mut buf = Vec::new();
    let mut off = 0u64;
    loop {
        buf.clear();
        let n = match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if n <= MAX_LINE && !f(off, &buf) {
            break;
        }
        off += n as u64;
    }
}

/// A human prompt on a transcript line: (text, ts). Skips tool results, meta
/// lines and harness messages (text starting with `<`).
pub fn human_prompt(v: &Value) -> Option<(String, i64)> {
    let ts = v.get("timestamp").and_then(Value::as_str).map(crate::transcript::parse_ts).unwrap_or(0);
    let text = match v.get("type").and_then(Value::as_str)? {
        "user" => {
            if v.get("isMeta").and_then(Value::as_bool).unwrap_or(false) {
                return None;
            }
            match &v["message"]["content"] {
                Value::String(s) => s.clone(),
                Value::Array(blocks) => {
                    if blocks.iter().any(|b| b["type"] == "tool_result") {
                        return None;
                    }
                    let t: Vec<&str> = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
                    t.join("\n")
                }
                _ => return None,
            }
        }
        "attachment" if v["attachment"]["type"] == "queued_command" => v["attachment"]["prompt"].as_str()?.to_string(),
        _ => return None,
    };
    let text = crate::transcript::strip_reminders(&text);
    if text.is_empty() || text.starts_with('<') || text.starts_with("[Request interrupted") || text.starts_with("Another Claude session sent") {
        return None;
    }
    Some((text, ts))
}

fn assistant_text(v: &Value) -> Option<(String, i64)> {
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let ts = v.get("timestamp").and_then(Value::as_str).map(crate::transcript::parse_ts).unwrap_or(0);
    let t: Vec<&str> =
        v["message"]["content"].as_array()?.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    let t = t.join("\n");
    if t.trim().is_empty() { None } else { Some((t, ts)) }
}

/// Distinct recent human prompts: (text, ts, tid, n) newest first, from the 12
/// most recently modified main transcripts.
pub fn recent_prompts(limit: usize) -> Vec<(String, i64, String, usize)> {
    let mut all: Vec<(String, i64, String)> = Vec::new();
    for (path, _) in main_transcripts().into_iter().take(12) {
        let tid = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        each_line(&path, |_, line| {
            if contains(line, b"\"type\":\"user\"") || contains(line, b"queued_command") {
                if let Some((t, ts)) = serde_json::from_slice::<Value>(line).ok().as_ref().and_then(human_prompt) {
                    if t.len() <= 500 {
                        all.push((t, ts, tid.clone()));
                    }
                }
            }
            true
        });
    }
    all.sort_by(|a, b| b.1.cmp(&a.1));
    let mut out: Vec<(String, i64, String, usize)> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for (t, ts, tid) in all {
        let key = t.split_whitespace().collect::<Vec<_>>().join(" ");
        match keys.iter().position(|k| *k == key) {
            Some(i) => out[i].3 += 1,
            None => {
                keys.push(key);
                out.push((t, ts, tid, 1));
            }
        }
    }
    out.truncate(limit);
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// One search match: the source line's byte offset, role, ts and a snippet.
pub struct Hit {
    pub off: u64,
    pub role: &'static str,
    pub ts: i64,
    pub snippet: String,
}

/// Case-insensitive search of human prompts and assistant text in one transcript,
/// in file order. `q` must be lowercase. Stops at `deadline` (returns true if cut).
pub fn search_file(path: &Path, q: &str, deadline: std::time::Instant, max: usize) -> (Vec<Hit>, bool) {
    let mut hits = Vec::new();
    let mut cut = false;
    // Raw-line prefilter: valid when the query has nothing JSON escapes.
    let raw_ok = q.is_ascii() && !q.contains(['"', '\\']);
    let qb = q.as_bytes();
    let mut n = 0usize;
    each_line(path, |off, line| {
        n += 1;
        if n % 256 == 0 && std::time::Instant::now() > deadline {
            cut = true;
            return false;
        }
        if raw_ok && !line.windows(qb.len()).any(|w| w.eq_ignore_ascii_case(qb)) {
            return true;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { return true };
        let (found, role) = match human_prompt(&v) {
            Some(x) => (Some(x), "user"),
            None => (assistant_text(&v), "assistant"),
        };
        if let Some((text, ts)) = found {
            if let Some(snippet) = snippet_around(&text, q) {
                hits.push(Hit { off, role, ts, snippet });
                if hits.len() > max {
                    hits.remove(0); // keep the newest `max`
                }
            }
        }
        true
    });
    (hits, cut)
}

/// ≤160 chars of `text` around the first case-insensitive match of `q`
/// (lowercase), whitespace collapsed. None if no match.
pub fn snippet_around(text: &str, q: &str) -> Option<String> {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = flat.to_lowercase();
    let at = lower.find(q)?;
    // Map the byte position in `lower` to a char index (lowercasing can change lengths).
    let chars: Vec<char> = flat.chars().collect();
    let ci = lower[..at].chars().count().min(chars.len());
    let mut start = ci.saturating_sub(60);
    let end = (start + 160).min(chars.len());
    if end - start < 160 {
        start = end.saturating_sub(160);
    }
    let mut s: String = chars[start..end].iter().collect();
    if start > 0 {
        s = format!("…{s}");
    }
    if end < chars.len() {
        s.push('…');
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snippet() {
        assert_eq!(snippet_around("Hello  World\nfoo", "world").as_deref(), Some("Hello World foo"));
        let long = format!("{} the 5070 Ti card {}", "a ".repeat(100), "b ".repeat(100));
        let s = snippet_around(&long, "5070 ti").unwrap();
        assert!(s.contains("5070 Ti") && s.starts_with('…') && s.ends_with('…'));
        assert!(s.chars().count() <= 162);
        assert_eq!(snippet_around("nothing", "zzz"), None);
    }
    #[test]
    fn search_and_prompts() {
        let dir = std::env::temp_dir().join(format!("mm-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let lines = [
            r#"{"type":"user","message":{"content":"where is the Pruner config?"},"timestamp":"2026-10-05T09:00:00.000Z"}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"x","content":"pruner output"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"The pruner lives in ~/.local/bin."}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"pruner meta"}}"#,
            r#"{"type":"user","message":{"content":"<command-name>/pruner</command-name>"}}"#,
        ];
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        let far = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (hits, cut) = search_file(&p, "pruner", far, 40);
        assert!(!cut);
        assert_eq!(hits.iter().map(|h| h.role).collect::<Vec<_>>(), ["user", "assistant"]);
        assert_eq!(hits[0].off, 0);
        assert_eq!(hits[1].off, (lines[0].len() + lines[1].len() + 2) as u64);
        let v: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(human_prompt(&v).unwrap().0, "where is the Pruner config?");
    }
}
