//! Omarchy theme bridge: the PWA can wear the desktop's current Omarchy theme.
//!
//! `GET /api/theme` reads the active theme (`~/.local/state/omarchy/current/theme`, written by
//! `omarchy-theme-set`; override with `SHEPRD_THEME_DIR` for tests) and returns its palette:
//! `{name, mode, colors{background, foreground, accent, red, …}, ansi[16]?, border{colors[], angle}, sig}`.
//! Source of truth is the theme's `colors.toml`; themes without one fall back to `alacritty.toml`.
//! The Hyprland active-border gradient comes from `colors.toml` (`hyprland_active_border`),
//! `hyprland.lua` or `hyprland.conf`. The web side (web/theme.js) maps the palette onto CSS tokens.
//!
//! `GET /api/theme?name=<theme>` returns an installed theme by name instead (Settings → Look → Palette
//! pins one, independent of the desktop), and `GET /api/themes` lists the installed themes with a
//! swatch: `{current, themes: [{name, mode, bg, fg, accent, user}]}`. Themes are looked up in
//! `~/.config/omarchy/themes`, `~/.local/share/omarchy/themes`, `/usr/share/omarchy/themes` (first wins;
//! `SHEPRD_THEMES` = a `:`-separated list overrides it for tests).
//!
//! While any client holds the event stream, the file is re-read every 3 s; a change (the user ran
//! `omarchy-theme-set`) is broadcast as SSE event `theme` with the same JSON.

use crate::Shared;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub fn routes() -> Router<Shared> {
    Router::new().route("/api/theme", get(api_theme)).route("/api/themes", get(api_themes))
}

#[derive(serde::Deserialize)]
struct ThemeQ {
    name: Option<String>,
}

fn json_resp(v: Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], v.to_string()).into_response()
}

async fn api_theme(State(_app): State<Shared>, Query(q): Query<ThemeQ>) -> Response {
    let v = match q.name.filter(|n| !n.is_empty()) {
        Some(n) => named_dir(&n).and_then(|d| read_theme(&d)).map(|mut v| {
            v["name"] = json!(n);
            v
        }),
        None => read_theme(&theme_dir()),
    };
    json_resp(v.unwrap_or_else(|| json!({"name": null})))
}

async fn api_themes(State(_app): State<Shared>) -> Response {
    let cur = read_theme(&theme_dir()).and_then(|v| v["name"].as_str().map(str::to_string));
    json_resp(json!({"current": cur, "themes": list_themes(&theme_roots())}))
}

fn theme_roots() -> Vec<(PathBuf, bool)> {
    if let Some(s) = std::env::var_os("SHEPRD_THEMES").filter(|d| !d.is_empty()) {
        return std::env::split_paths(&s).map(|p| (p, false)).collect();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    vec![(home.join(".config/omarchy/themes"), true), (home.join(".local/share/omarchy/themes"), false), (PathBuf::from("/usr/share/omarchy/themes"), false)]
}

/// A theme name is one path component of `[A-Za-z0-9._-]`, never `.`/`..`.
fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && !n.starts_with('.') && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn named_dir(n: &str) -> Option<PathBuf> {
    if !valid_name(n) {
        return None;
    }
    theme_roots().into_iter().map(|(r, _)| r.join(n)).find(|d| d.is_dir())
}

/// Installed themes that have a palette, sorted by name; a name in an earlier root shadows later ones.
fn list_themes(roots: &[(PathBuf, bool)]) -> Vec<Value> {
    let mut seen = std::collections::BTreeMap::new();
    for (r, user) in roots {
        let Ok(rd) = std::fs::read_dir(r) else { continue };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !valid_name(&n) || seen.contains_key(&n) || !e.path().is_dir() {
                continue;
            }
            let Some(v) = read_theme(&e.path()) else { continue };
            let c = &v["colors"];
            seen.insert(n.clone(), json!({"name": n, "mode": v["mode"], "bg": c["background"], "fg": c["foreground"],
                "accent": c.get("accent").or(c.get("blue")).cloned().unwrap_or(Value::Null), "user": user}));
        }
    }
    seen.into_values().collect()
}

/// Watches the theme while someone is connected and broadcasts changes.
pub async fn run(app: Shared) {
    let mut last = String::new();
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if app.events.receiver_count() == 0 {
            continue;
        }
        let Some(v) = read_theme(&theme_dir()) else { continue };
        let sig = v["sig"].as_str().unwrap_or("").to_string();
        if !last.is_empty() && sig != last {
            let _ = app.events.send(("theme", Arc::new(v.to_string())));
        }
        last = sig;
    }
}

fn theme_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SHEPRD_THEME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".local/state/omarchy/current/theme")
}

/// Reads a theme directory into the API JSON, or None when it has no palette.
pub fn read_theme(dir: &Path) -> Option<Value> {
    let rd = |f: &str| std::fs::read_to_string(dir.join(f)).ok();
    let mut colors = rd("colors.toml").map(|s| parse_colors_toml(&s)).unwrap_or_default();
    let ala = rd("alacritty.toml").map(|s| parse_alacritty(&s));
    if let Some((c, _)) = &ala {
        for (k, v) in c {
            colors.entry(k.clone()).or_insert(v.clone());
        }
    }
    let bg = colors.get("background").and_then(Value::as_str).and_then(hex)?;
    colors.get("foreground").and_then(Value::as_str).and_then(hex)?;
    let mode = colors.remove("mode").and_then(|m| m.as_str().map(str::to_string)).unwrap_or_else(|| {
        if luminance(bg) > 0.4 { "light".into() } else { "dark".into() }
    });
    let border = colors
        .remove("hyprland_active_border")
        .and_then(|v| v.as_str().map(parse_border))
        .or_else(|| rd("hyprland.lua").and_then(|s| s.lines().find(|l| l.contains("active_border") && !l.contains("inactive")).map(parse_border)).filter(|b| !b.0.is_empty()))
        .or_else(|| rd("hyprland.conf").and_then(|s| conf_border(&s)))
        .unwrap_or_default();
    colors.retain(|k, v| k != "hyprland_inactive_border" && v.as_str().is_some_and(|s| s.starts_with('#')));
    let name = if std::env::var_os("SHEPRD_THEME_DIR").is_some_and(|d| !d.is_empty()) {
        dir.file_name().map(|n| n.to_string_lossy().to_string())
    } else {
        dir.parent().and_then(|p| std::fs::read_to_string(p.join("theme.name")).ok()).map(|s| s.trim().to_string())
    };
    let mut v = json!({
        "name": name, "mode": mode, "colors": colors,
        "ansi": ala.map(|(_, a)| a).filter(|a| a.iter().all(|c| !c.is_empty())),
        "border": {"colors": border.0, "angle": border.1},
    });
    let sig = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        v.to_string().hash(&mut h);
        format!("{:x}", h.finish())
    };
    v["sig"] = json!(sig);
    Some(v)
}

/// Normalizes `#rgb`, `#rrggbb`, `#rrggbbaa`, `0xrrggbb` to lowercase `#rrggbb[aa]`.
fn norm(s: &str) -> Option<String> {
    let t = s.trim().trim_matches(|c| c == '"' || c == '\'');
    let h = t.strip_prefix('#').or_else(|| t.strip_prefix("0x"))?;
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let h = h.to_ascii_lowercase();
    match h.len() {
        3 => Some(format!("#{}", h.chars().flat_map(|c| [c, c]).collect::<String>())),
        6 | 8 => Some(format!("#{h}")),
        _ => None,
    }
}

fn hex(s: &str) -> Option<(f64, f64, f64)> {
    let h = norm(s)?;
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).map(|x| x as f64 / 255.0).ok();
    Some((p(1)?, p(3)?, p(5)?))
}

fn luminance((r, g, b): (f64, f64, f64)) -> f64 {
    let f = |c: f64| if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

fn kv(line: &str) -> Option<(String, String)> {
    let line = line.split_once(" #").map_or(line, |(a, _)| a).trim();
    if line.starts_with('#') || line.starts_with('[') {
        return None;
    }
    let (k, v) = line.split_once('=')?;
    let v = v.trim();
    let v = v.strip_prefix('"').and_then(|x| x.split('"').next()).or_else(|| v.strip_prefix('\'').and_then(|x| x.split('\'').next())).unwrap_or(v);
    Some((k.trim().to_string(), v.to_string()))
}

/// Omarchy `colors.toml`: flat `key = "value"` pairs.
fn parse_colors_toml(s: &str) -> Map<String, Value> {
    let mut m = Map::new();
    for l in s.lines() {
        let Some((k, v)) = kv(l) else { continue };
        let v = if k == "mode" || k.starts_with("hyprland_") { v } else if let Some(h) = norm(&v) { h } else { continue };
        m.insert(k, json!(v));
    }
    m
}

const ANSI: [&str; 8] = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];

/// `alacritty.toml` → (colors.toml-shaped palette, 16 ANSI colors; empty strings where missing).
fn parse_alacritty(s: &str) -> (Map<String, Value>, Vec<String>) {
    let mut m = Map::new();
    let mut ansi = vec![String::new(); 16];
    let mut sec = String::new();
    for l in s.lines() {
        let t = l.trim();
        if let Some(x) = t.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
            sec = x.trim().to_string();
            continue;
        }
        let Some((k, v)) = kv(t) else { continue };
        let Some(v) = norm(&v) else { continue };
        let slot = ANSI.iter().position(|a| *a == k);
        match (sec.as_str(), slot) {
            ("colors.primary", _) if k == "background" || k == "foreground" => {
                m.insert(k, json!(v));
            }
            ("colors.normal", Some(i)) => {
                ansi[i] = v.clone();
                if !matches!(k.as_str(), "black" | "white") {
                    m.insert(k, json!(v));
                }
            }
            ("colors.bright", Some(i)) => {
                ansi[i + 8] = v.clone();
                if !matches!(k.as_str(), "black" | "white") {
                    m.insert(format!("bright_{k}"), json!(v));
                }
            }
            ("colors.selection", _) if k == "background" => {
                m.insert("selection".into(), json!(v));
            }
            _ => {}
        }
    }
    (m, ansi)
}

/// Gradient from `rgba(61afefff) rgba(56b6c2ff) 90deg` or the hyprland.lua form
/// `{ colors = { "rgba(..)", … }, angle = 90 }`. Colors come back as `#rrggbbaa`.
fn parse_border(s: &str) -> (Vec<String>, u32) {
    let re = regex_lite::Regex::new(r"rgba?\(\s*([0-9a-fA-F]{6,8})\s*\)").unwrap();
    let cols: Vec<String> = re.captures_iter(s).filter_map(|c| norm(&format!("#{}", &c[1]))).collect();
    let ang = regex_lite::Regex::new(r"(?:(\d+)\s*deg|angle\s*=\s*(\d+))").unwrap();
    let a = ang.captures(s).and_then(|c| c.get(1).or(c.get(2))).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
    (cols, a)
}

/// hyprland.conf: `col.active_border = rgba(..) rgba(..) 45deg`, possibly via `$var` definitions.
fn conf_border(s: &str) -> Option<(Vec<String>, u32)> {
    let mut vars = std::collections::HashMap::new();
    let mut line = None;
    for l in s.lines() {
        let Some((k, v)) = kv(l) else { continue };
        if let Some(n) = k.strip_prefix('$') {
            vars.insert(n.to_string(), v.clone());
        } else if k == "col.active_border" {
            line = Some(v);
        }
    }
    let mut line = line?;
    for (n, v) in &vars {
        line = line.replace(&format!("${n}"), v);
    }
    let b = parse_border(&line);
    (!b.0.is_empty()).then_some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_toml() {
        let m = parse_colors_toml("mode = \"dark\"\n\naccent = \"#61AFEF\"\n# note\nbackground = \"#282c34\"\nhyprland_active_border = \"rgba(61afefff) rgba(56b6c2ff) 90deg\"\n");
        assert_eq!(m["accent"], "#61afef");
        assert_eq!(m["mode"], "dark");
        assert_eq!(parse_border(m["hyprland_active_border"].as_str().unwrap()), (vec!["#61afefff".into(), "#56b6c2ff".into()], 90));
    }

    #[test]
    fn alacritty() {
        let (m, a) = parse_alacritty("[colors.primary]\nbackground = '#1A1B26'\nforeground = \"#c0caf5\"\n[colors.normal]\nblack = \"#15161e\"\nred = \"#f7768e\"\n[colors.bright]\nred = '0xff899d'\n");
        assert_eq!(m["background"], "#1a1b26");
        assert_eq!(m["red"], "#f7768e");
        assert_eq!(m["bright_red"], "#ff899d");
        assert_eq!(a[0], "#15161e");
        assert_eq!(a[9], "#ff899d");
        assert_eq!(a[2], "");
    }

    #[test]
    fn hypr_lua_and_conf() {
        let lua = "local active_border_color = { colors = { \"rgba(61afefff)\", \"rgba(c678ddff)\" }, angle = 90 }\nlocal inactive_border_color = \"rgba(595959aa)\"";
        let first = lua.lines().next().unwrap();
        assert_eq!(parse_border(first), (vec!["#61afefff".into(), "#c678ddff".into()], 90));
        let conf = "$a = rgb(ff79c6)\n$b = rgba(bd93f9ee)\ngeneral {\n  col.active_border = $a $b 45deg\n}\n";
        assert_eq!(conf_border(conf), Some((vec!["#ff79c6".into(), "#bd93f9ee".into()], 45)));
    }

    #[test]
    fn named_and_list() {
        assert!(valid_name("tokyo-night") && valid_name("one_dark.pro"));
        assert!(!valid_name("..") && !valid_name("a/b") && !valid_name(".x") && !valid_name(""));
        let r = std::env::temp_dir().join(format!("mm-themes-{}", std::process::id()));
        for (n, bg) in [("zed", "#000000"), ("alpha", "#ffffff")] {
            std::fs::create_dir_all(r.join(n)).unwrap();
            std::fs::write(r.join(n).join("colors.toml"), format!("background = \"{bg}\"\nforeground = \"#888888\"\naccent = \"#61afef\"\n")).unwrap();
        }
        std::fs::create_dir_all(r.join("empty")).unwrap();
        let l = list_themes(&[(r.clone(), true)]);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0]["name"], "alpha");
        assert_eq!(l[0]["mode"], "light");
        assert_eq!(l[1]["accent"], "#61afef");
        std::fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn whole_dir() {
        let d = std::env::temp_dir().join(format!("mm-theme-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("alacritty.toml"), "[colors.primary]\nbackground = \"#ffffff\"\nforeground = \"#000000\"\n").unwrap();
        let v = read_theme(&d).unwrap();
        assert_eq!(v["mode"], "light");
        assert_eq!(v["colors"]["background"], "#ffffff");
        assert!(v["ansi"].is_null());
        std::fs::remove_dir_all(&d).ok();
        assert!(read_theme(&d).is_none());
    }
}
