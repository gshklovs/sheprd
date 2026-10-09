//! An icon for every scheduled item (systemd timers, agent wake-ups, the app's own schedules).
//!
//! Order: a manual pick (long-press a row on the phone) > deterministic keyword rules over the
//! item's own text (unit name, description, command / prompt, label) > the same rules over the
//! owning agent's title > a one-time Haiku pick cached by key (only when no rule matched, at most
//! `HAIKU_PER_DAY` a day) > the clock. Keys: `t:<timer name>` (unit without `.timer`), `s:<schedule id>`.
//! Persisted in `<state dir>/sched-icons.json` ({manual, auto, day, haikuN}).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// The built-in set (the phone draws each as a line icon; same keys there).
pub const ICONS: &[&str] = &[
    "plane", "checkin", "train", "car", "calendar", "mail", "chart", "disk", "deploy", "gpu", "cart", "bell", "health", "money",
    "monitor", "photo", "clock",
];
pub const DEFAULT: &str = "clock";
const HAIKU_PER_DAY: u32 = 5;

/// (icon, words). A word matches a whole token; `*` at the end matches a prefix. Two-token
/// phrases are also matched joined ("check in" → "checkin"). First rule that matches wins, so
/// the specific ones (an airline check-in) come before the generic ones (check-in, monitor).
const RULES: &[(&str, &[&str])] = &[
    ("plane", &[
        "flight*", "fly", "flying", "plane", "airplane", "airport", "airline*", "boarding", "boardingpass", "layover", "united", "delta",
        "southwest", "jetblue", "lufthansa", "ryanair", "easyjet", "aeromexico", "klm", "airfrance", "britishairways", "emirates", "qatar",
        "alaskaair", "americanairlines", "aircanada", "frontier", "spirit", "sfo", "jfk", "lax", "ewr",
    ]),
    ("gpu", &["gpu*", "cuda", "training", "finetun*", "fine-tun*", "epoch*", "wandb", "checkpoint*", "nvidia", "vram", "lora", "sweep*"]),
    ("train", &["train", "trains", "amtrak", "caltrain", "bart", "railway", "eurostar", "metro", "subway"]),
    ("car", &["uber", "lyft", "taxi", "cab", "car", "ride", "rides", "rental", "parking", "pickup", "dropoff", "commute", "waymo"]),
    ("checkin", &["checkin", "check-in", "hotel*", "airbnb", "reservation*", "booking*", "itinerary", "ticket*"]),
    ("health", &["health", "doctor", "dentist", "therapy", "therapist", "meds", "medication*", "medicine", "pill*", "vitamin*", "gym", "workout*", "exercise"]),
    ("money", &[
        "bill", "bills", "billing", "pay", "payment*", "invoice*", "rent", "tax", "taxes", "bank*", "budget*", "expense*", "receipt*",
        "salary", "payroll", "stripe", "venmo", "refund*", "subscription*", "money",
    ]),
    ("cart", &["shop*", "buy", "purchase*", "cart", "amazon", "grocer*", "restock", "deliver*", "order", "orders", "costco", "instacart"]),
    ("mail", &["mail", "email*", "gmail", "inbox", "newsletter*", "imap", "mails"]),
    ("chart", &["report*", "usage", "stats", "metric*", "analytics", "dashboard*", "summary", "summar*", "digest", "kpi*", "weekly", "recap"]),
    ("disk", &["backup*", "snapshot*", "disk*", "rsync", "restic", "borg", "prune", "cleanup", "clean", "archive*", "storage", "trim", "fstrim", "btrfs", "scrub"]),
    ("deploy", &["deploy*", "build", "builds", "release*", "ci", "ship", "vercel", "publish*", "compile", "rollout", "migrat*", "upgrade*", "update", "updates"]),
    ("calendar", &["meeting*", "calendar", "call", "standup", "1on1", "interview*", "appointment*", "event*", "birthday*", "zoom", "meet", "agenda", "deadline*"]),
    ("bell", &["remind*", "alarm*", "nudge*", "ping", "notify", "notification*", "alert*", "todo", "followup", "follow-up"]),
    ("monitor", &["wake*", "monitor*", "watch*", "poll*", "check", "checks", "status", "heartbeat", "uptime", "display", "screen", "brightness", "health-check"]),
    ("photo", &["wallpaper*", "photo*", "image*", "picture*", "camera"]),
];

/// Airline flight numbers ("BA117", "dl 1234"): two-letter IATA codes that are not common words.
const AIRLINE_CODES: &[&str] = &["ua", "aa", "dl", "wn", "b6", "nk", "f9", "ba", "lh", "af", "kl", "ac", "as", "ek", "qr", "lx", "ib", "fr", "u2"];

fn tokens(text: &str) -> Vec<String> {
    let low = text.to_lowercase();
    let mut t: Vec<String> = low.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect();
    // joined pairs: "check in" / "check-in" → "checkin", "air canada" → "aircanada"
    let pairs: Vec<String> = t.windows(2).map(|w| format!("{}{}", w[0], w[1])).collect();
    // hyphenated words kept whole too ("fine-tune", "follow-up")
    let hy: Vec<String> = low.split(|c: char| !(c.is_alphanumeric() || c == '-')).filter(|w| w.contains('-')).map(|w| w.trim_matches('-').to_string()).collect();
    t.extend(pairs);
    t.extend(hy);
    t
}

fn flight_no(tok: &str) -> bool {
    let (a, n) = tok.split_at(tok.len().min(2));
    AIRLINE_CODES.contains(&a) && (1..=4).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit())
}

/// The rule icon for a piece of text, if any rule matches.
pub fn by_rules(text: &str) -> Option<&'static str> {
    let toks = tokens(text);
    if toks.is_empty() {
        return None;
    }
    if toks.iter().any(|t| flight_no(t)) {
        return Some("plane");
    }
    let set: HashSet<&str> = toks.iter().map(String::as_str).collect();
    for (icon, words) in RULES {
        for w in *words {
            let hit = match w.strip_suffix('*') {
                Some(p) => toks.iter().any(|t| t.starts_with(p)),
                None => set.contains(w),
            };
            if hit {
                return Some(icon);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Store

#[derive(Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Store {
    manual: HashMap<String, String>,
    auto: HashMap<String, String>,
    day: String,
    haiku_n: u32,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| Mutex::new(load()));
static ASKING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn file() -> std::path::PathBuf {
    crate::state_dir().join("sched-icons.json")
}

fn load() -> Store {
    std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(s: &Store) {
    if let Ok(b) = serde_json::to_vec_pretty(s) {
        let tmp = file().with_extension("json.tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(tmp, file());
        }
    }
}

/// Sets (Some) or clears (None) a manual pick.
pub fn set_manual(key: &str, icon: Option<&str>) -> Result<(), String> {
    if let Some(i) = icon
        && !ICONS.contains(&i)
    {
        return Err(format!("unknown icon {i}"));
    }
    let mut s = STORE.lock().unwrap();
    match icon {
        Some(i) => s.manual.insert(key.to_string(), i.to_string()),
        None => s.manual.remove(key),
    };
    save(&s);
    Ok(())
}

fn today() -> String {
    let t = crate::transcript::now_ms() / 1000;
    format!("{}", t / 86400)
}

/// (icon, how): how = manual | rule | agent | haiku | default. `own` = the item's own text,
/// `owner` = the owning agent's title. With no rule and no cached pick, a Haiku pick may be
/// started in the background (`allow_haiku`); this call still answers with the clock.
pub fn pick(key: &str, own: &str, owner: &str, allow_haiku: bool) -> (String, &'static str) {
    {
        let s = STORE.lock().unwrap();
        if let Some(i) = s.manual.get(key) {
            return (i.clone(), "manual");
        }
    }
    if let Some(i) = by_rules(own) {
        return (i.to_string(), "rule");
    }
    if let Some(i) = by_rules(owner) {
        return (i.to_string(), "agent");
    }
    let mut s = STORE.lock().unwrap();
    if let Some(i) = s.auto.get(key) {
        return (i.clone(), if i == DEFAULT { "default" } else { "haiku" });
    }
    if allow_haiku && std::env::var("SHEPRD_SCHED_HAIKU").map_or(true, |v| v != "0") {
        let d = today();
        if s.day != d {
            s.day = d;
            s.haiku_n = 0;
        }
        if s.haiku_n < HAIKU_PER_DAY && ASKING.lock().unwrap().insert(key.to_string()) {
            s.haiku_n += 1;
            save(&s);
            let (k, text) = (key.to_string(), format!("{own}\n{owner}"));
            tokio::spawn(async move {
                let icon = haiku(&text).await.unwrap_or_else(|_| DEFAULT.to_string());
                let mut s = STORE.lock().unwrap();
                s.auto.insert(k.clone(), icon);
                save(&s);
                ASKING.lock().unwrap().remove(&k);
            });
        }
    }
    (DEFAULT.to_string(), "default")
}

async fn haiku(text: &str) -> Result<String, String> {
    let sys = format!(
        "You pick one icon for a scheduled task shown on a phone. Answer with exactly one word from this list and nothing else: {}. \
         Use clock when nothing fits.",
        ICONS.join(", ")
    );
    let prompt = format!("Scheduled task:\n{}", text.chars().take(600).collect::<String>());
    let v = crate::newagent::run_haiku(Some(&sys), &prompt, Duration::from_secs(60)).await?;
    let r = v["result"].as_str().unwrap_or("").trim().trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    ICONS.iter().find(|i| **i == r).map(|i| i.to_string()).ok_or_else(|| format!("unusable reply: {r}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn airline_rows_get_the_plane() {
        // a timer: name, systemd-run description, agent prompt
        assert_eq!(by_rules("airline-checkin [systemd-run] /usr/bin/herdr agent prompt w6:p1V \"CHECK-IN WINDOW OPEN: run phase 3\""), Some("plane"));
        assert_eq!(by_rules("checkin-return CHECK-IN WINDOW OPEN: run phase 3 for RETURN BA117"), Some("plane"));
        assert_eq!(by_rules("remind me: BA117 leaves"), Some("plane")); // flight number beats "remind"
        assert_eq!(by_rules("dl 1234"), Some("plane"));
        assert_eq!(by_rules("Book flights to NYC"), Some("plane"));
        assert_eq!(by_rules("Delta check in"), Some("plane"));
    }

    #[test]
    fn each_icon_has_a_rule() {
        let cases = [
            ("hotel-checkin", "checkin"),
            ("check in for the Airbnb", "checkin"),
            ("amtrak tickets", "train"),
            ("Caltrain to SF", "train"),
            ("order an Uber home", "car"),
            ("Lyft pickup", "car"),
            ("team standup", "calendar"),
            ("Prep for the interview", "calendar"),
            ("Summarize email", "mail"),
            ("Usage report", "chart"),
            ("Morning digest", "chart"),
            ("restic backup", "disk"),
            ("btrfs scrub", "disk"),
            ("Deploy to Vercel", "deploy"),
            ("nightly build", "deploy"),
            ("check the training run on the GPU", "gpu"),
            ("finetune epoch 3", "gpu"),
            ("restock groceries", "cart"),
            ("buy the tickets", "checkin"), // a ticket is a booking first
            ("Morning digest: summarize overnight email", "mail"), // mail before the report
            ("reminder", "bell"),
            ("take meds", "health"),
            ("pay the rent", "money"),
            ("Lyft ride receipts", "car"),
            ("invoice due", "money"),
            ("monitor-wake", "monitor"),
            ("wake me if the job dies", "monitor"),
            ("Rotate the desktop wallpaper", "photo"),
        ];
        for (text, want) in cases {
            assert_eq!(by_rules(text), Some(want), "{text}");
        }
        for (icon, _) in RULES {
            assert!(ICONS.contains(icon), "{icon}");
        }
    }

    #[test]
    fn no_false_hits() {
        assert_eq!(by_rules(""), None);
        assert_eq!(by_rules("run phase 3"), None);
        assert_eq!(by_rules("w6:p1V"), None);
        assert_eq!(by_rules("ua"), None); // a bare code is not a flight
        assert_eq!(by_rules("ua12345"), None);
        assert_eq!(by_rules("constrain the trainee list"), None); // whole words, not substrings
        assert_eq!(by_rules("scaled the cart-pole sim"), Some("cart")); // (hyphen splits: an accepted miss)
    }

    #[test]
    fn manual_beats_rules_and_rejects_unknown() {
        assert!(set_manual("t:__test", Some("nope")).is_err());
        assert!(ICONS.contains(&DEFAULT));
    }
}
