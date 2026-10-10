//! Never type over what the user typed. herdr's `agent.prompt` types text + Enter into whatever is in Claude's input
//! box, so a steer or prompt sent while the box held an unsent draft went out glued to the draft (and the draft was
//! gone from the box). `herdr::quick("agent.prompt")` asks `guard` first: a box holding a real draft (or an open
//! menu) refuses with `input_draft` / `input_menu`, and the steer / prompt routes hold the message here instead.
//! `run` delivers held messages, oldest first, once the box is empty again (draft sent or cleared), or through the
//! mod's inbox while the agent works (an append never touches the input box).
//!
//! Draft detection reads the screen with its colours (`pane.read`, format `ansi`): text after the box's `❯` drawn
//! faint (SGR 2) or in Claude's placeholder grey (38;2;153;153;153) is Claude's own suggestion, not a draft.

use crate::{Shared, now_ms};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

#[derive(Debug, PartialEq)]
pub enum Input {
    /// the box is on screen and empty (or shows only Claude's placeholder / hints)
    Empty,
    /// the box holds text someone typed or pasted
    Draft(String),
    /// no input box, a menu or permission prompt is open
    Menu,
    /// no input box and no menu recognised (another agent's TUI, a scrolled view): not judged
    Unknown,
}

/// One screen line as (char, faint) pairs.
type Line = Vec<(char, bool)>;

/// Splits an ANSI screen into lines, marking each char faint when SGR 2 or the placeholder grey is in effect.
fn styled_lines(ansi: &str) -> Vec<Line> {
    let mut out = vec![Vec::new()];
    let (mut dim, mut grey) = (false, false);
    let mut it = ansi.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut fin = None;
                    for d in it.by_ref() {
                        if ('\x40'..='\x7e').contains(&d) {
                            fin = Some(d);
                            break;
                        }
                        params.push(d);
                    }
                    if fin == Some('m') {
                        sgr(&params, &mut dim, &mut grey);
                    }
                }
                Some(']') => {
                    // OSC: up to BEL or ST
                    while let Some(d) = it.next() {
                        if d == '\x07' || (d == '\x1b' && it.peek() == Some(&'\\')) {
                            if d == '\x1b' {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' => out.push(Vec::new()),
            '\r' => {}
            c if c.is_control() => {}
            c => out.last_mut().unwrap().push((c, dim || grey)),
        }
    }
    out
}

fn sgr(params: &str, dim: &mut bool, grey: &mut bool) {
    let p: Vec<u32> = params.split(';').map(|x| x.parse().unwrap_or(0)).collect();
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            0 => (*dim, *grey) = (false, false),
            2 => *dim = true,
            22 => *dim = false,
            39 => *grey = false,
            38 | 48 => {
                let fg = p[i] == 38;
                match p.get(i + 1) {
                    Some(2) => {
                        if fg {
                            *grey = p.get(i + 2..i + 5) == Some(&[153, 153, 153][..]);
                        }
                        i += 4;
                    }
                    Some(5) => {
                        if fg {
                            *grey = false;
                        }
                        i += 2;
                    }
                    _ => {}
                }
            }
            30..=37 | 90..=97 => *grey = false,
            _ => {}
        }
        i += 1;
    }
}

fn plain(l: &Line) -> String {
    l.iter().map(|(c, _)| *c).collect()
}

/// What Claude's input box holds, from a `pane.read` ANSI screen.
pub fn classify(ansi: &str) -> Input {
    let lines = styled_lines(ansi);
    let text: Vec<String> = lines.iter().map(plain).collect();
    // Box rules may carry a label (the session name): "──── name ─".
    let rule = |l: &str| l.trim_start().starts_with("────");
    let Some(at) = (1..text.len()).rev().find(|&i| text[i].starts_with('❯') && rule(&text[i - 1])) else {
        return if crate::parse_menu(&text.join("\n")).len() >= 2 { Input::Menu } else { Input::Unknown };
    };
    let mut draft: Vec<String> = Vec::new();
    for (k, l) in lines.iter().enumerate().skip(at) {
        if k > at && rule(&text[k]) {
            break;
        }
        let skip = if k == at { l.iter().position(|(c, _)| *c == '❯').map_or(0, |p| p + 1) } else { 0 };
        draft.push(l.iter().skip(skip).filter(|(_, faint)| !faint).map(|(c, _)| *c).collect::<String>().trim().to_string());
    }
    let d = draft.join("\n").trim().to_string();
    if d.is_empty() || d.starts_with(crate::talk::QUEUE_HINT) { Input::Empty } else { Input::Draft(d) }
}

/// The pane's input box now; None when the screen can't be read.
pub async fn read(pane: &str) -> Option<Input> {
    let r = crate::herdr::call("pane.read", json!({"pane_id": pane, "source": "visible", "format": "ansi"}), Duration::from_secs(5))
        .await
        .ok()?;
    r["read"]["text"].as_str().map(classify)
}

/// Err(code) when typing into `pane` now would touch what is in its input box: `input_draft` | `input_menu`.
pub async fn guard(pane: &str) -> Result<(), &'static str> {
    match read(pane).await {
        Some(Input::Draft(_)) => Err("input_draft"),
        Some(Input::Menu) => Err("input_menu"),
        _ => Ok(()),
    }
}

pub fn is_busy(code: &str) -> bool {
    code == "input_draft" || code == "input_menu"
}

// ---------- messages held for an empty box ----------

#[derive(Clone)]
pub struct Held {
    pub id: u64,
    pub text: String,
    pub ts: i64,
}

static HELD: LazyLock<Mutex<HashMap<String, VecDeque<Held>>>> = LazyLock::new(Default::default);
/// panes whose front message `run` is delivering right now (it is not held again behind the rest)
static FLUSHING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
static NEXT: LazyLock<Mutex<u64>> = LazyLock::new(|| Mutex::new(now_ms() as u64));

/// Whether a new message to `pane` must wait behind held ones (strict FIFO per pane).
pub fn must_wait(pane: &str) -> bool {
    !FLUSHING.lock().unwrap().contains(pane) && HELD.lock().unwrap().get(pane).is_some_and(|q| !q.is_empty())
}

/// Holds `text` for `pane` → its id ("h<id>" in state). While `run` delivers this pane's front message, a refusal puts
/// it back in front.
pub fn hold(pane: &str, text: &str) -> u64 {
    let id = {
        let mut n = NEXT.lock().unwrap();
        *n += 1;
        *n
    };
    let h = Held { id, text: text.to_string(), ts: now_ms() };
    let mut m = HELD.lock().unwrap();
    let q = m.entry(pane.to_string()).or_default();
    if FLUSHING.lock().unwrap().contains(pane) { q.push_front(h) } else { q.push_back(h) }
    eprintln!("held for {pane}: the input box holds a draft or a menu ({} waiting)", q.len());
    id
}

/// The `/steer` / `/prompt` answer for a held message.
pub fn held_reply(pane: &str, text: &str, code: &str) -> Value {
    let id = hold(pane, text);
    json!({"ok": true, "via": "held", "how": if code == "input_menu" { "menu" } else { "draft" }, "id": format!("h{id}")})
}

pub fn held_for(pane: &str) -> Vec<Held> {
    HELD.lock().unwrap().get(pane).map(|q| q.iter().cloned().collect()).unwrap_or_default()
}

/// Takes back a held message (phone ×) → its text.
pub fn unhold(pane: &str, id: u64) -> Option<String> {
    let mut m = HELD.lock().unwrap();
    let q = m.get_mut(pane)?;
    let k = q.iter().position(|h| h.id == id)?;
    q.remove(k).map(|h| h.text)
}

/// Delivers held messages, oldest first, one per pane per pass: through `/steer`'s own path, which appends through the
/// mod while the agent works and otherwise types only into an empty box (still a draft: it stays held, in front).
pub async fn run(app: Shared) {
    loop {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let panes: Vec<String> = HELD.lock().unwrap().iter().filter(|(_, q)| !q.is_empty()).map(|(p, _)| p.clone()).collect();
        for pane in panes {
            if !app.inner.lock().unwrap().status.contains_key(&pane) {
                if let Some(q) = HELD.lock().unwrap().remove(&pane) {
                    eprintln!("held for {pane}: pane gone, dropped {}", q.len());
                }
                continue;
            }
            let working = app.inner.lock().unwrap().status.get(&pane).is_some_and(|s| s.0 == "working");
            // idle with a draft: nothing to try (saves a steer round trip); working: the mod may append
            if !working && !matches!(read(&pane).await, Some(Input::Empty | Input::Unknown)) {
                continue;
            }
            let Some(h) = HELD.lock().unwrap().get_mut(&pane).and_then(|q| q.pop_front()) else { continue };
            FLUSHING.lock().unwrap().insert(pane.clone());
            let r = crate::modbridge::steer(app.clone(), pane.clone(), h.text.clone()).await;
            FLUSHING.lock().unwrap().remove(&pane);
            let ok = r.status().is_success();
            if !ok {
                // e.g. agent_blocked: back in front, tried again later
                HELD.lock().unwrap().entry(pane.clone()).or_default().push_front(h);
            } else {
                eprintln!("held for {pane}: sent on");
            }
            app.refresh.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(name: &str) -> String {
        std::fs::read_to_string(format!("{}/fixtures/inputbox/{name}.ansi", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn faint_placeholder_is_not_a_draft() {
        // `❯ Try "fix typecheck errors"` in SGR 2
        assert_eq!(classify(&fx("placeholder-dim")), Input::Empty);
        // the same suggestion in Claude's placeholder grey
        assert_eq!(classify(&fx("placeholder-grey")), Input::Empty);
    }

    #[test]
    fn empty_box() {
        assert_eq!(classify(&fx("empty")), Input::Empty);
    }

    #[test]
    fn typed_draft() {
        assert_eq!(classify(&fx("draft")), Input::Draft("my unsent draft about the parser".into()));
    }

    #[test]
    fn multi_line_draft() {
        assert_eq!(classify(&fx("multiline")), Input::Draft("first line of draft\nsecond line".into()));
    }

    #[test]
    fn pasted_chip_counts() {
        assert_eq!(classify(&fx("pasted")), Input::Draft("see this: [Pasted text #1 +40 lines]".into()));
        assert_eq!(classify(&fx("chip-only")), Input::Draft("[Pasted text #2 +30 lines]".into()));
    }

    #[test]
    fn permission_menu_is_not_typed_into() {
        assert_eq!(classify(&fx("menu")), Input::Menu);
    }

    #[test]
    fn history_prompts_above_the_box_are_ignored() {
        // the screen after the bug: earlier prompts (`❯ …` on a grey row) above an empty box
        assert_eq!(classify(&fx("history-empty")), Input::Empty);
    }

    #[test]
    fn sgr_parsing() {
        let r = "\x1b[38;2;136;136;136m────\x1b[0m\r\n";
        assert_eq!(classify(&format!("{r}❯\u{a0}\x1b[0m\x1b[2mhint\x1b[0m\r\n{r}")), Input::Empty);
        assert_eq!(classify(&format!("{r}❯\u{a0}\x1b[0m\x1b[2mhint\x1b[22m typed\r\n{r}")), Input::Draft("typed".into()));
        assert_eq!(classify(&format!("{r}❯\u{a0}\x1b[38;2;153;153;153mgrey\x1b[38;2;255;255;255m real\r\n{r}")), Input::Draft("real".into()));
        assert_eq!(classify(&format!("{r}❯\u{a0}Press up to edit queued messages\r\n{r}")), Input::Empty);
        assert_eq!(classify("$ ls\r\nfoo\r\n"), Input::Unknown);
    }

    #[test]
    fn held_fifo_and_unhold() {
        let p = "test:held";
        let a = hold(p, "one");
        let b = hold(p, "two");
        assert!(must_wait(p));
        assert_eq!(held_for(p).iter().map(|h| h.text.as_str()).collect::<Vec<_>>(), ["one", "two"]);
        assert_eq!(unhold(p, a).as_deref(), Some("one"));
        assert_eq!(unhold(p, a), None);
        FLUSHING.lock().unwrap().insert(p.into());
        assert!(!must_wait(p));
        hold(p, "zero"); // a refused flush goes back in front
        FLUSHING.lock().unwrap().remove(p);
        assert_eq!(held_for(p).iter().map(|h| h.text.as_str()).collect::<Vec<_>>(), ["zero", "two"]);
        unhold(p, b);
    }
}
