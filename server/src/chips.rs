//! Quick-reply chips for an agent's last answer (pure text rules, no model calls).

/// Up to 4 one-tap replies for `answer` (the last assistant text). First matching
/// rule wins (list options, yes/no, "done"), then staples fill the rest.
pub fn chips(answer: &str) -> Vec<String> {
    let mut start = answer.len().saturating_sub(600);
    while !answer.is_char_boundary(start) {
        start += 1;
    }
    let text = answer[start..].replace('’', "'");
    let lower = text.to_lowercase();
    let has_q = text.contains('?');
    let mut out: Vec<String> = Vec::new();
    let options = list_options(&text);
    if has_q && options.len() >= 2 {
        out = options;
    } else if yes_no(&lower) {
        out = vec!["yes please".into(), "no".into()];
    } else if wants_done(&lower) {
        out.push("done".into());
    }
    out.truncate(4);
    let mut staples = vec!["thanks"];
    if !has_q {
        staples.insert(0, "continue");
    }
    for s in staples {
        if out.len() < 4 && !out.iter().any(|o| o.eq_ignore_ascii_case(s)) {
            out.push(s.into());
        }
    }
    out
}

/// Labels of the trailing numbered/lettered list (`1. foo`, `b) bar`).
fn list_options(text: &str) -> Vec<String> {
    let item = |l: &str| -> Option<String> {
        let t = l.trim_start();
        let n = t.chars().take_while(char::is_ascii_digit).count();
        let marker = if n > 0 { n } else if t.starts_with(|c: char| ('a'..='d').contains(&c)) { 1 } else { return None };
        let rest = &t[marker..];
        let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let label = rest.trim().replace("**", "").replace('`', "");
        let len = label.chars().count();
        if !(2..=60).contains(&len) {
            return None;
        }
        Some(label.chars().take(40).collect())
    };
    let lines: Vec<&str> = text.lines().collect();
    let Some(last) = lines.iter().rposition(|l| item(l).is_some()) else { return Vec::new() };
    let mut out = Vec::new();
    for l in lines[..=last].iter().rev() {
        match item(l) {
            Some(x) => out.push(x),
            None if l.trim().is_empty() => continue,
            None => break,
        }
    }
    out.reverse();
    out
}

/// True when the last sentence is a yes/no question.
fn yes_no(lower: &str) -> bool {
    let trim = |s: &str| s.trim_end_matches(|c: char| c.is_whitespace() || "*_`".contains(c)).to_string();
    let mut t = trim(lower);
    // "... (yes or no)" / "(y/n)": an explicit yes/no ask; other trailing parentheticals are dropped
    while t.ends_with(')') {
        let Some(k) = t.rfind('(') else { break };
        let p = &t[k..];
        if p.contains("yes or no") || p.contains("yes/no") || p.contains("y/n") {
            return true;
        }
        t = trim(&t[..k]);
    }
    let Some(body) = t.strip_suffix('?') else { return false };
    // "I'm waiting for your answer: should I ...?" → the clause after the colon
    let sentence = body.rsplit(['.', '!', '?', '\n', ':']).next().unwrap_or(body);
    let sentence = sentence.trim_start_matches(|c: char| c.is_whitespace() || "*_>-#`".contains(c));
    const STARTS: [&str; 14] = [
        "should", "shall", "want", "do you", "would you", "can i", "are you ok", "ok to", "shall i", "want me to",
        "is that ok", "is it ok", "ok if", "does that",
    ];
    STARTS.iter().any(|s| sentence.starts_with(s))
}

/// True when the agent hands something to the human to do.
fn wants_done(lower: &str) -> bool {
    const KEYS: [&str; 12] = [
        "let me know", "once you", "when you're done", "when you are done", "when you done", "run this", "run it",
        "sudo", "plug", "reboot", "log in", "login",
    ];
    KEYS.iter().any(|k| lower.contains(k))
}

#[cfg(test)]
mod tests {
    use super::chips;
    #[test]
    fn numbered_options() {
        let t = "I can go two ways:\n\n1. **Rewrite** the parser\n2. Patch the regex\n\nWhich do you want?";
        assert_eq!(chips(t), vec!["Rewrite the parser", "Patch the regex", "thanks"]);
    }
    #[test]
    fn lettered_options_need_question() {
        let t = "Options:\na) keep it\nb) drop it";
        assert_eq!(chips(t), vec!["continue", "thanks"]);
        let t = "Options:\na) keep it\nb) drop it\nc) rename it\nd) move it\nPick one?";
        assert_eq!(chips(t), vec!["keep it", "drop it", "rename it", "move it"]);
    }
    #[test]
    fn yes_no_question() {
        assert_eq!(chips("All tests pass. Should I proceed with the deploy?"), vec!["yes please", "no", "thanks"]);
        assert_eq!(chips("Done.\n\n**Want me to push it?**"), vec!["yes please", "no", "thanks"]);
        // An open question is not yes/no.
        assert_eq!(chips("Which branch should it go on?"), vec!["thanks"]);
        // trailing parenthetical, clause after a colon, explicit (yes or no)
        assert_eq!(chips("Done.\n\nShould I proceed with cleanup? (yes or no)"), vec!["yes please", "no", "thanks"]);
        assert_eq!(chips("It finished.\n\nI'm still waiting for your answer: should I proceed?"), vec!["yes please", "no", "thanks"]);
        assert_eq!(chips("Keep the old config (y/n)"), vec!["yes please", "no", "continue", "thanks"]);
        assert_eq!(chips("Which one do you mean? (the first or the second)"), vec!["thanks"]);
    }
    #[test]
    fn done_handoff() {
        assert_eq!(chips("Run this and let me know:\n`sudo pacman -S mosh`"), vec!["done", "continue", "thanks"]);
        assert_eq!(chips("Plug the board in, then tell me."), vec!["done", "continue", "thanks"]);
    }
    #[test]
    fn staples() {
        assert_eq!(chips("Fixed the bug and pushed."), vec!["continue", "thanks"]);
    }
    #[test]
    fn long_text_uses_tail() {
        let t = format!("{}. Should I continue?", "é".repeat(700));
        assert_eq!(chips(&t), vec!["yes please", "no", "thanks"]);
    }
}
