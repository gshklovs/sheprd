//! Dismissals that stick. A surface the user dismissed stays dismissed: the dismissal is saved with the
//! surface, and its content fingerprint (agent + title + body, whitespace-insensitive) is kept in
//! `surfaces-dismissed.json` (30 days, 1000 newest) so the same note posted again by the same agent under a new id
//! (the "You should know" bridge after a mod reload, an agent repeating itself) is not shown, toasted or pushed
//! again. A new post identical to a card still on screen replaces that card instead of stacking a second one.
//! Surfaces the server makes itself with their own lifecycle (`pushUrl`)
//! are never suppressed.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;

const KEEP_MS: i64 = 30 * 24 * 3600 * 1000;
const KEEP_N: usize = 1000;

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// FNV-1a 64 over agent, title and body (whitespace collapsed).
pub fn fingerprint(agent: &str, title: &str, body: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [agent, &norm(title), &norm(body)] {
        for b in part.bytes().chain(std::iter::once(0)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// One fingerprint per way the agent is known (session, pane): a re-post matches if either does,
/// even when the pane ↔ session lookup resolved differently the second time.
pub fn keys(session: Option<&str>, pane: Option<&str>, title: &str, body: &str) -> Vec<String> {
    let mut out: Vec<String> = [session, pane]
        .into_iter()
        .flatten()
        .filter(|a| !a.is_empty())
        .map(|a| fingerprint(a, title, body))
        .collect();
    if out.is_empty() {
        out.push(fingerprint("", title, body));
    }
    out
}

pub fn keys_of(s: &Value) -> Vec<String> {
    keys(
        s["session"].as_str().or(s["tid"].as_str()),
        s["pane"].as_str(),
        s["title"].as_str().unwrap_or(""),
        s["body"].as_str().unwrap_or(""),
    )
}

fn dismissed(s: &Value) -> bool {
    s["dismissed"].as_bool().unwrap_or(false)
}

/// Fingerprints of dismissed surfaces with when they were dismissed, newest first.
#[derive(Default)]
pub struct Gone(VecDeque<(String, i64)>);

impl Gone {
    pub fn load(path: &Path) -> Self {
        let list: Vec<(String, i64)> =
            std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Gone(list.into())
    }
    pub fn save(&self, path: &Path) {
        if let Ok(b) = serde_json::to_vec(&self.0) {
            write_atomic(path, &b);
        }
    }
    pub fn has(&self, k: &str) -> bool {
        self.0.iter().any(|(f, _)| f == k)
    }
    fn add(&mut self, k: &str, now: i64) {
        self.0.retain(|(f, t)| f != k && now - t < KEEP_MS);
        self.0.push_front((k.to_string(), now));
        self.0.truncate(KEEP_N);
    }
}

/// Whether a new surface with these keys should be shown. false: the same content from the same agent
/// was dismissed (keep it out). true: shown, and any older undismissed identical card is dropped.
pub fn admit(list: &mut VecDeque<Value>, gone: &Gone, ks: &[String]) -> bool {
    let hit = |s: &Value| keys_of(s).iter().any(|k| ks.contains(k));
    if ks.iter().any(|k| gone.has(k)) || list.iter().any(|s| dismissed(s) && hit(s)) {
        return false;
    }
    list.retain(|s| dismissed(s) || !hit(s));
    true
}

/// Dismisses the surfaces `pick` selects plus every other card with the same content, and remembers their
/// fingerprints. Returns how many surfaces `pick` matched (0: unknown id).
pub fn dismiss(list: &mut VecDeque<Value>, gone: &mut Gone, now: i64, pick: impl Fn(&Value) -> bool) -> usize {
    let mut ks: Vec<String> = Vec::new();
    let mut n = 0;
    for s in list.iter_mut().filter(|s| pick(s)) {
        n += 1;
        ks.extend(keys_of(s));
        s["dismissed"] = json!(true);
    }
    for s in list.iter_mut().filter(|s| !dismissed(s)) {
        if keys_of(s).iter().any(|k| ks.contains(k)) {
            s["dismissed"] = json!(true);
        }
    }
    for k in &ks {
        gone.add(k, now);
    }
    n
}

/// Write to a temp file and rename it, so a restart mid-write never leaves a truncated file.
/// May this card clear itself once the user has seen what it points at (main.rs `api_viewing`)? Only plain notes:
/// info/done, no reply chips or command waiting on him, not a blocker
/// confirmation (their own lifecycle).
pub fn auto_clearable(s: &Value) -> bool {
    let empty = |v: &Value| v.is_null() || v.as_array().is_some_and(|a| a.is_empty()) || v.as_str().is_some_and(str::is_empty);
    !s["dismissed"].as_bool().unwrap_or(false)
        && matches!(s["level"].as_str(), Some("info" | "done"))
        && empty(&s["replies"])
        && empty(&s["command"])
        && s["blocker"].is_null()
        && s.get("pushUrl").is_none_or(Value::is_null)
        && !s["links"].as_array().is_some_and(|l| {
            l.iter().any(|x| x["url"].as_str().or(x.as_str()).is_some_and(|u| u.contains("#/browser/") || u.contains("#/buy/")))
        })
}

pub fn write_atomic(path: &Path, b: &[u8]) {
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, b).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, session: &str, title: &str, body: &str) -> Value {
        json!({"id": id, "session": session, "tid": session, "pane": "w1:p1", "title": title, "body": body})
    }

    #[test]
    fn fingerprint_ignores_whitespace_but_not_agent() {
        assert_eq!(fingerprint("a", "T", "x  y\n"), fingerprint("a", " T", "x y"));
        assert_ne!(fingerprint("a", "T", "x"), fingerprint("b", "T", "x"));
        assert_ne!(fingerprint("a", "T x", ""), fingerprint("a", "T", "x"));
    }

    #[test]
    fn dismissed_content_stays_dismissed() {
        let mut l: VecDeque<Value> = vec![s("s1", "abc", "Note", "body")].into();
        let mut g = Gone::default();
        assert_eq!(dismiss(&mut l, &mut g, 1, |x| x["id"] == "s1"), 1);
        assert!(dismissed(&l[0]));
        // the same note again (new id), also when only the pane is known this time
        assert!(!admit(&mut l, &g, &keys(Some("abc"), None, "Note", " body ")));
        assert!(!admit(&mut l, &g, &keys(None, Some("w1:p1"), "Note", "body")));
        // after it fell off the 200-item list the fingerprint still holds
        l.clear();
        assert!(!admit(&mut l, &g, &keys(Some("abc"), Some("w1:p1"), "Note", "body")));
        // genuinely new content, or the same text from another agent, is shown
        assert!(admit(&mut l, &g, &keys(Some("abc"), Some("w1:p1"), "Note", "other body")));
        assert!(admit(&mut l, &g, &keys(Some("zzz"), Some("w2:p2"), "Note", "body")));
    }

    #[test]
    fn repost_of_visible_card_replaces_it() {
        let mut l: VecDeque<Value> = vec![s("s1", "abc", "Note", "body"), s("s0", "abc", "Other", "")].into();
        let g = Gone::default();
        assert!(admit(&mut l, &g, &keys(Some("abc"), Some("w1:p1"), "Note", "body")));
        assert_eq!(l.len(), 1);
        assert_eq!(l[0]["id"], "s0");
    }

    #[test]
    fn dismiss_takes_identical_copies_and_clear_all() {
        let mut l: VecDeque<Value> =
            vec![s("s2", "abc", "Note", "body"), s("s1", "abc", "Note", "body"), s("s0", "abc", "Keep", "")].into();
        let mut g = Gone::default();
        dismiss(&mut l, &mut g, 1, |x| x["id"] == "s2");
        assert!(dismissed(&l[1]) && !dismissed(&l[2]));
        assert_eq!(dismiss(&mut l, &mut g, 1, |x| x["id"] == "nope"), 0);
        let ids = ["s0".to_string()];
        assert_eq!(dismiss(&mut l, &mut g, 1, |x| ids.iter().any(|i| x["id"] == i.as_str())), 1);
        assert!(l.iter().all(dismissed));
    }

    #[test]
    fn only_plain_notes_clear_themselves() {
        let base = json!({"id": "1", "level": "info", "title": "t", "body": "b", "replies": [], "command": null, "blocker": null, "links": []});
        assert!(auto_clearable(&base));
        let with = |k: &str, v: Value| { let mut x = base.clone(); x[k] = v; x };
        assert!(auto_clearable(&with("level", json!("done"))));
        assert!(!auto_clearable(&with("level", json!("attention"))));
        assert!(!auto_clearable(&with("replies", json!(["yes", "no"]))));
        assert!(!auto_clearable(&with("command", json!("sudo x"))));
        assert!(!auto_clearable(&with("blocker", json!({"kind": "captcha"}))));
        assert!(!auto_clearable(&with("pushUrl", json!("/#/browser/b1"))));
        assert!(!auto_clearable(&with("links", json!([{"url": "https://h/#/browser/b1", "label": "Open browser"}]))));
        assert!(!auto_clearable(&with("links", json!([{"url": "https://h/#/buy/p1"}]))));
        assert!(!auto_clearable(&with("dismissed", json!(true))));
        assert!(auto_clearable(&with("links", json!([{"url": "http://localhost:5173"}]))));
    }

    #[test]
    fn gone_expires_and_caps() {
        let mut g = Gone::default();
        g.add("old", 0);
        g.add("new", KEEP_MS + 1);
        assert!(g.has("new") && !g.has("old"));
        for i in 0..KEEP_N + 5 {
            g.add(&i.to_string(), KEEP_MS + 2);
        }
        assert_eq!(g.0.len(), KEEP_N);
    }
}
