//! User preferences persisted in `<state dir>/prefs.json`: snippets and
//! per-session flags (pinned, muted).

use serde_json::{Value, json};
use std::path::PathBuf;

pub struct Prefs {
    path: PathBuf,
    pub v: Value,
}

fn default_snippets() -> Value {
    let texts = [
        "continue",
        "hows that going?",
        "done",
        "yes please",
        "you can use subagents at will to go all out",
        "make sure it's wired into the system (keybinds/menus/pickers)",
    ];
    texts.iter().enumerate().map(|(n, t)| json!({"id": format!("d{n}"), "label": t, "text": t})).collect()
}

impl Prefs {
    pub fn load(dir: &std::path::Path) -> Prefs {
        let path = dir.join("prefs.json");
        let mut v: Value = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}));
        if !v.is_object() {
            v = json!({});
        }
        if !v["snippets"].is_array() {
            v["snippets"] = default_snippets();
        }
        if !v["sessions"].is_object() {
            v["sessions"] = json!({});
        }
        v["resAlerts"] = crate::alerts::normalize(&v["resAlerts"]);
        Prefs { path, v }
    }

    fn save(&self) {
        let _ = std::fs::write(&self.path, self.v.to_string());
    }

    /// Replaces one top-level key (e.g. `resAlerts`) and saves.
    pub fn set_key(&mut self, key: &str, val: Value) {
        self.v[key] = val;
        self.save();
    }

    /// (pinned, muted) for a session.
    pub fn flags(&self, sid: &str) -> (bool, bool) {
        let s = &self.v["sessions"][sid];
        (s["pinned"].as_bool().unwrap_or(false), s["muted"].as_bool().unwrap_or(false))
    }

    pub fn set_flags(&mut self, sid: &str, pinned: Option<bool>, muted: Option<bool>) {
        let s = &mut self.v["sessions"][sid];
        if !s.is_object() {
            *s = json!({"pinned": false, "muted": false});
        }
        if let Some(p) = pinned {
            s["pinned"] = json!(p);
        }
        if let Some(m) = muted {
            s["muted"] = json!(m);
        }
        self.save();
    }

    /// A pane moved to a new session (`/clear`, resume, a fresh session): its pin and mute go with it, unless the
    /// new session already has its own.
    pub fn carry_flags(&mut self, from: &str, to: &str) {
        let old = self.v["sessions"][from].clone();
        let set = |k: &str| old[k].as_bool().unwrap_or(false);
        if from == to || self.v["sessions"][to].is_object() || !(set("pinned") || set("muted")) {
            return;
        }
        self.set_flags(to, Some(set("pinned")), Some(set("muted")));
    }

    /// Replaces the snippet list (max 50, text ≤ 4000 chars, label defaults to the text).
    pub fn set_snippets(&mut self, list: &[Value]) -> Result<(), String> {
        if list.len() > 50 {
            return Err("at most 50 snippets".into());
        }
        let mut out = Vec::new();
        for (n, s) in list.iter().enumerate() {
            let text = s["text"].as_str().ok_or("snippet text required")?;
            if text.len() > 4000 {
                return Err("snippet text over 4000 chars".into());
            }
            let id = s["id"].as_str().filter(|i| !i.is_empty()).map(str::to_string).unwrap_or_else(|| format!("s{n}"));
            let label = s["label"].as_str().filter(|l| !l.trim().is_empty()).unwrap_or(text);
            out.push(json!({"id": id, "label": label, "text": text}));
        }
        self.v["snippets"] = Value::Array(out);
        self.save();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_follow_the_pane_to_a_new_session() {
        let dir = std::env::temp_dir().join(format!("mm-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = Prefs::load(&dir);
        p.set_flags("s1", Some(true), None);
        p.carry_flags("s1", "s2");
        assert_eq!(p.flags("s2"), (true, false));
        // a session with flags of its own keeps them; nothing to carry from an unflagged one
        p.set_flags("s3", Some(false), Some(true));
        p.carry_flags("s1", "s3");
        assert_eq!(p.flags("s3"), (false, true));
        p.carry_flags("s9", "s4");
        assert!(!p.v["sessions"]["s4"].is_object());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
