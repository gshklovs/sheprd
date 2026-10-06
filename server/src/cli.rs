//! The command line around the server: `sheprd setup`, `sheprd doctor`, `sheprd qr`, `sheprd uninstall`.
//!
//! Setup is idempotent: every step checks first and only changes what is missing, so re-running it is the
//! fix for most problems. Nothing here needs root; the one step that can (Tailscale Serve without operator
//! rights) prints the exact command instead of running it.

use serde_json::Value;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{DEFAULT_PORT, VERSION, home, port, state_dir};

const UNIT: &str = "sheprd.service";
const MARKETPLACE: &str = "sheprd-local";
const PLUGIN: &str = "sheprd@sheprd-local";

const HELP: &str = "sheprd — your herdr coding agents, on your phone

Usage: sheprd [command]

Commands:
  setup       Install the background service, the Claude Code plugin and Tailscale Serve,
              then show a QR code to open sheprd on your phone (safe to re-run)
  doctor      Check every moving part and say how to fix what's broken
  qr          Print the phone URL and its QR code
  serve       Run the server in the foreground (what the service runs; the default)
  uninstall   Remove the service, the plugin and the Tailscale Serve route (--purge: state too)
  version     Print the version

Setup options:
  --no-service    don't install the systemd user service (run `sheprd serve` yourself)
  --no-plugin     don't install the Claude Code plugin
  --no-tailscale  don't touch Tailscale Serve
  --no-wait       don't wait for the phone to connect
  -y, --yes       never ask

Environment: SHEPRD_PORT (default 7878), SHEPRD_STATE_DIR, SHEPRD_LISTEN (extra bind addresses,
e.g. 0.0.0.0:7878 for a trusted LAN), HERDR_SOCKET_PATH, SHEPRD_NO_PUSH=1 (log pushes instead).
Docs: https://github.com/gshklovski/sheprd";

pub async fn run(args: &[String]) -> i32 {
    let rest = &args[1..];
    let flag = |f: &str| rest.iter().any(|a| a == f);
    match args[0].as_str() {
        "setup" => setup(Opts {
            service: !flag("--no-service"),
            plugin: !flag("--no-plugin"),
            tailscale: !flag("--no-tailscale"),
            wait: !flag("--no-wait"),
            yes: flag("-y") || flag("--yes"),
        }),
        "doctor" => doctor(),
        "qr" | "url" => {
            let url = phone_url(false).unwrap_or_else(|| local_url());
            println!("{url}\n");
            print_qr(&url);
            0
        }
        "uninstall" => uninstall(flag("--purge")),
        "version" | "-V" | "--version" => {
            println!("sheprd {VERSION}");
            0
        }
        "help" | "-h" | "--help" => {
            println!("{HELP}");
            0
        }
        other => {
            eprintln!("sheprd: unknown command `{other}`\n\n{HELP}");
            2
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Output

fn color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}
fn paint(code: &str, s: &str) -> String {
    if color() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}
fn ok(msg: &str) {
    println!("  {} {msg}", paint("32", "✓"));
}
fn warn(msg: &str) {
    println!("  {} {msg}", paint("33", "!"));
}
fn bad(msg: &str) {
    println!("  {} {msg}", paint("31", "✗"));
}
fn fix(cmd: &str) {
    println!("      {} {}", paint("2", "→"), paint("1", cmd));
}
fn note(msg: &str) {
    println!("      {}", paint("2", msg));
}
fn step(title: &str) {
    println!("\n{}", paint("1", title));
}

// ---------------------------------------------------------------------------------------------------------
// Small helpers

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(bin)).find(|f| f.is_file()))
}

/// Runs `cmd args`, returns (success, stdout+stderr). Never hangs longer than `secs`.
fn sh(cmd: &str, args: &[&str], secs: u64) -> (bool, String) {
    let Ok(mut child) = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return (false, format!("{cmd}: not found"));
    };
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let t_out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() < Duration::from_secs(secs) => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let text = t_out.join().unwrap_or_default() + &t_err.join().unwrap_or_default();
    (status.is_some_and(|s| s.success()), text.trim().to_string())
}

/// Plain HTTP/1.0 to the local server (no client library needed for loopback).
fn http(method: &str, path: &str, body: Option<&str>) -> Option<(u16, String)> {
    let mut s = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port()).into(), Duration::from_secs(2)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let b = body.unwrap_or("");
    write!(
        s,
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
        b.len()
    )
    .ok()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok()?;
    let raw = String::from_utf8_lossy(&raw);
    let (head, body) = raw.split_once("\r\n\r\n")?;
    let code = head.split_whitespace().nth(1)?.parse().ok()?;
    Some((code, body.to_string()))
}

fn health() -> Option<Value> {
    let (code, body) = http("GET", "/api/health", None)?;
    let v: Value = serde_json::from_str(&body).ok()?;
    (code == 200 && v["app"] == "sheprd").then_some(v)
}

fn local_url() -> String {
    format!("http://127.0.0.1:{}", port())
}

fn tailscale_status() -> Option<Value> {
    which("tailscale")?;
    let (_, out) = sh("tailscale", &["status", "--json", "--peers=false"], 5);
    serde_json::from_str(&out).ok()
}

/// MagicDNS name of this machine when Tailscale is up.
fn ts_name(st: &Value) -> Option<String> {
    (st["BackendState"] == "Running")
        .then(|| st["Self"]["DNSName"].as_str().unwrap_or("").trim_end_matches('.').to_string())
        .filter(|n| !n.is_empty())
}

/// Which HTTPS port of `tailscale serve` proxies to our server: Some(443) → https://name/.
fn serve_port() -> Option<u16> {
    let (_, out) = sh("tailscale", &["serve", "status", "--json"], 5);
    let v: Value = serde_json::from_str(&out).ok()?;
    let target = format!("127.0.0.1:{}", port());
    let target2 = format!("localhost:{}", port());
    for (hostport, h) in v["Web"].as_object()? {
        for (_, handler) in h["Handlers"].as_object()? {
            let p = handler["Proxy"].as_str().unwrap_or("");
            if p.contains(&target) || p.contains(&target2) {
                return hostport.rsplit(':').next()?.parse().ok();
            }
        }
    }
    None
}

/// Who else serves on an HTTPS port already (so setup never clobbers someone's route).
fn serve_taken(https_port: u16) -> bool {
    let (_, out) = sh("tailscale", &["serve", "status", "--json"], 5);
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    v["TCP"].as_object().is_some_and(|m| m.contains_key(&https_port.to_string()))
}

/// The URL the phone opens: Tailscale Serve HTTPS when configured, else the tailnet name over plain HTTP.
fn phone_url(quiet: bool) -> Option<String> {
    let st = tailscale_status()?;
    let name = ts_name(&st)?;
    match serve_port() {
        Some(443) => Some(format!("https://{name}/")),
        Some(p) => Some(format!("https://{name}:{p}/")),
        None if port() == DEFAULT_PORT => {
            if !quiet {
                warn("Tailscale Serve isn't set up: plain HTTP works, but iPhone push notifications need HTTPS.");
            }
            Some(format!("http://{name}:{}/", port()))
        }
        None => None,
    }
}

pub fn print_qr(url: &str) {
    let Ok(code) = qrcode::QrCode::new(url.as_bytes()) else { return };
    // Dense1x2: two QR rows per text row; light quiet zone so phones read it on dark terminals too.
    let s = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build();
    for line in s.lines() {
        println!("    {line}");
    }
}

fn ask(q: &str, yes: bool) -> bool {
    if yes || !std::io::stdin().is_terminal() {
        return true;
    }
    print!("  {q} [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut a = String::new();
    let _ = std::io::stdin().read_line(&mut a);
    !a.trim().to_lowercase().starts_with('n')
}

/// `herdr integration status` → the word after `claude:` ("current", "outdated", "not installed").
fn herdr_claude_integration() -> Option<String> {
    let (_, out) = sh("herdr", &["integration", "status"], 10);
    let rest = out.lines().find_map(|l| l.trim().strip_prefix("claude:"))?.trim();
    Some(if rest.starts_with("not installed") { "not installed".into() } else { rest.split_whitespace().next()?.to_string() })
}

fn systemd_user() -> bool {
    which("systemctl").is_some() && sh("systemctl", &["--user", "show-environment"], 5).0
}

fn unit_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("systemd/user")
        .join(UNIT)
}

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share")).join("sheprd")
}

/// Where the service runs the binary from. A stable install location is used as is; a binary run from a
/// build dir, a download folder or /tmp is copied to ~/.local/bin first.
fn installed_exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let stable = [home().join(".local/bin"), home().join(".cargo/bin"), PathBuf::from("/usr"), PathBuf::from("/opt")];
    if stable.iter().any(|d| exe.starts_with(d)) {
        return Ok(exe);
    }
    let dst = home().join(".local/bin/sheprd");
    std::fs::create_dir_all(dst.parent().unwrap())?;
    let tmp = dst.with_extension("new");
    std::fs::copy(&exe, &tmp)?;
    std::fs::rename(&tmp, &dst)?; // atomic: a running service keeps its old inode
    Ok(dst)
}

/// The unit, with the PATH of the shell that ran setup: the service then finds herdr, claude, git, gh,
/// tailscale and ffmpeg exactly where you do (systemd's default user PATH is minimal).
fn unit_text(exe: &Path) -> String {
    let mut env = String::new();
    let path = std::env::var("PATH").unwrap_or_default();
    env.push_str(&format!("Environment=\"PATH={}\"\n", path.replace('"', "")));
    for (k, v) in std::env::vars() {
        if (k.starts_with("SHEPRD_") || k == "HERDR_SOCKET_PATH") && !v.contains('"') && !v.contains('\n') {
            env.push_str(&format!("Environment=\"{k}={v}\"\n"));
        }
    }
    format!(
        "# Written by `sheprd setup` (re-run it to update). Your herdr agents, on your phone.
[Unit]
Description=sheprd: herdr agents on your phone
After=network-online.target tailscaled.service
Wants=network-online.target

[Service]
ExecStart={exe} serve
Restart=always
RestartSec=2
Nice=5
# Small and quiet: a runaway can never hurt the desktop.
MemoryMax=200M
Environment=RUST_BACKTRACE=0
{env}
[Install]
WantedBy=default.target
",
        exe = exe.display()
    )
}

fn wait_health(secs: u64) -> Option<Value> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        if let Some(h) = health() {
            return Some(h);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

// ---------------------------------------------------------------------------------------------------------
// setup

struct Opts {
    service: bool,
    plugin: bool,
    tailscale: bool,
    wait: bool,
    yes: bool,
}

fn setup(o: Opts) -> i32 {
    println!("{}", paint("1", &format!("sheprd {VERSION} setup")));
    println!("{}", paint("2", "Every step checks first; re-running setup is always safe."));

    // 1. What's here
    step("1/5  Checking this machine");
    let herdr = which("herdr");
    match &herdr {
        Some(p) => ok(&format!("herdr {}", sh(p.to_str().unwrap_or("herdr"), &["--version"], 5).1.trim_start_matches("herdr "))),
        None => {
            bad("herdr is not installed: sheprd shows the agents herdr runs.");
            fix("install it: https://github.com/herdrdev/herdr");
        }
    }
    let sock = crate::herdr::socket_path();
    if sock.exists() {
        ok(&format!("herdr is running ({})", tilde(&sock)));
    } else {
        warn("herdr isn't running yet; that's fine, sheprd connects as soon as it starts.");
        note("start it in a terminal with: herdr");
    }
    let claude = which("claude");
    match &claude {
        Some(_) => ok("Claude Code (claude)"),
        None => {
            warn("Claude Code isn't on PATH: chats, pushes and the plugin need it.");
            fix("curl -fsSL https://claude.ai/install.sh | bash");
        }
    }
    let ts = which("tailscale").and(tailscale_status());
    match ts.as_ref().map(|s| (s["BackendState"].as_str().unwrap_or("").to_string(), ts_name(s))) {
        None if which("tailscale").is_some() => {
            warn("Tailscale is installed but its daemon doesn't answer.");
            fix("sudo systemctl enable --now tailscaled && sudo tailscale up");
        }
        Some((_, Some(n))) => ok(&format!("Tailscale is up ({n})")),
        Some((state, None)) => {
            warn(&format!("Tailscale is installed but not connected ({}).", if state.is_empty() { "unknown" } else { &state }));
            fix("sudo tailscale up");
        }
        None => {
            warn("Tailscale isn't installed: it's how your phone reaches this machine, privately.");
            note("install it here and on your phone: https://tailscale.com/download");
        }
    }
    if herdr.is_none() {
        println!("\nInstall herdr, then run `sheprd setup` again.");
        return 1;
    }

    // 2. Service
    step("2/5  Background service");
    let running = if !o.service {
        note("skipped (--no-service): run `sheprd serve` yourself");
        health().is_some()
    } else if systemd_user() {
        install_service()
    } else {
        warn("No systemd user session here (container, WSL without systemd, macOS?).");
        match health() {
            Some(h) if h["version"] == VERSION => {
                ok(&format!("sheprd {VERSION} is already running on port {}", port()));
                true
            }
            Some(h) => {
                // an older version: stop it and start this one
                if let Some(pid) = h["pid"].as_i64() {
                    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                    let t = Instant::now();
                    while health().is_some() && t.elapsed() < Duration::from_secs(5) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                if health().is_some() {
                    warn(&format!("sheprd {} is running and couldn't be stopped; stop it, then re-run setup", h["version"].as_str().unwrap_or("?")));
                    true
                } else {
                    spawn_detached()
                }
            }
            None => spawn_detached(),
        }
    };

    // 3. Claude Code plugin
    step("3/5  Claude Code");
    if !o.plugin {
        note("skipped (--no-plugin)");
    } else if claude.is_none() {
        warn("skipped: install Claude Code, then re-run setup");
    } else {
        install_plugin();
    }
    if which("claude").is_some() {
        // a brand-new Claude install has no ~/.claude yet; herdr writes its hook there
        let _ = std::fs::create_dir_all(home().join(".claude"));
        match herdr_claude_integration().as_deref() {
            Some("current") => ok("herdr's Claude Code integration (agent status hooks)"),
            st => {
                let what = if st == Some("outdated") { "is outdated" } else { "isn't installed" };
                if ask(&format!("herdr's Claude Code integration {what}; without it agents show no status. Install it (herdr integration install claude)?"), o.yes) {
                    let (good, out) = sh("herdr", &["integration", "install", "claude"], 30);
                    if good {
                        ok("installed herdr's Claude Code integration (restart running Claude sessions to pick it up)");
                    } else {
                        bad(&format!("herdr integration install claude: {}", last_line(&out)));
                    }
                } else {
                    warn(&format!("herdr's Claude Code integration {what}"));
                    fix("herdr integration install claude");
                }
            }
        }
    }

    // 4. Tailscale Serve (HTTPS for the phone)
    step("4/5  Phone access");
    let url = if !o.tailscale {
        note("skipped (--no-tailscale)");
        None
    } else {
        tailscale_serve(&o)
    };
    let url = url.or_else(|| {
        let lan = std::env::var("SHEPRD_LISTEN").ok().filter(|l| !l.is_empty());
        if lan.is_some() {
            warn("Using SHEPRD_LISTEN: anyone on that network can drive your agents. Trusted networks only.");
        } else {
            note(&format!("Until then sheprd answers on this machine only: {}", local_url()));
        }
        None
    });

    // 5. Phone
    step("5/5  Your phone");
    let Some(url) = url else {
        println!("  Open {} in a browser here to look around; set up Tailscale for the phone.", local_url());
        return if running { 0 } else { 1 };
    };
    println!("  Scan with the iPhone Camera (Tailscale app on, same tailnet):\n");
    print_qr(&url);
    println!("\n  {}\n", paint("1;4", &url));
    println!("  1. Open it in {}", paint("1", "Safari"));
    println!("  2. Share {} {}", paint("2", "(square with ↑)"), paint("1", "→ Add to Home Screen → Add"));
    println!("  3. Open {} from the Home Screen, tap {} and {}", paint("1", "sheprd"), paint("1", "⚙"), paint("1", "Enable notifications"));
    println!("{}", paint("2", "     Android: open in Chrome → ⋮ → Install app."));
    if running && o.wait && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        wait_for_phone();
    }
    println!("\n{} Start agents in herdr; they show up on your phone.", paint("32;1", "Done."));
    println!("{}", paint("2", "Something off? `sheprd doctor`  ·  QR again: `sheprd qr`"));
    if !running { 1 } else { 0 }
}

fn tilde(p: &Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    }
}

fn install_service() -> bool {
    let exe = match installed_exe() {
        Ok(e) => e,
        Err(e) => {
            bad(&format!("couldn't install the binary to ~/.local/bin: {e}"));
            return false;
        }
    };
    ok(&format!("binary: {}", tilde(&exe)));
    let path = unit_path();
    let text = unit_text(&exe);
    let changed = std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str());
    if changed {
        if let Err(e) = std::fs::create_dir_all(path.parent().unwrap()).and_then(|_| std::fs::write(&path, &text)) {
            bad(&format!("couldn't write {}: {e}", tilde(&path)));
            return false;
        }
        let _ = sh("systemctl", &["--user", "daemon-reload"], 15);
    }
    let _ = sh("systemctl", &["--user", "enable", UNIT], 15);
    // restart on any change (new binary, new unit); start otherwise
    let up = health();
    let same = up.as_ref().is_some_and(|h| h["version"] == VERSION) && !changed;
    if !same {
        let (good, out) = sh("systemctl", &["--user", "restart", UNIT], 20);
        if !good {
            bad(&format!("systemctl --user restart {UNIT} failed: {out}"));
            fix(&format!("journalctl --user -u {UNIT} -n 30 --no-pager"));
            return false;
        }
    }
    match wait_health(15) {
        Some(_) => ok(&format!("{UNIT} is running on 127.0.0.1:{} and starts at login", port())),
        None => {
            bad(&format!("{UNIT} started but doesn't answer on port {}", port()));
            if let Some((_, b)) = http("GET", "/", None) {
                note(&format!("something else answers on port {}: {}", port(), b.chars().take(60).collect::<String>()));
                fix("SHEPRD_PORT=7879 sheprd setup");
            }
            fix(&format!("journalctl --user -u {UNIT} -n 30 --no-pager"));
            return false;
        }
    }
    // Without linger the service stops when you log out (and only starts at your next login).
    let user = std::env::var("USER").unwrap_or_default();
    if !user.is_empty() && which("loginctl").is_some() {
        let (_, out) = sh("loginctl", &["show-user", &user, "-p", "Linger"], 5);
        if out.trim() == "Linger=no" {
            note("optional, to keep it running while you're logged out and after reboots:");
            fix(&format!("sudo loginctl enable-linger {user}"));
        }
    }
    true
}

/// No systemd: start `sheprd serve` in the background, logging to the state dir.
fn spawn_detached() -> bool {
    // run the installed copy (a download in /tmp may vanish)
    let exe = installed_exe().or_else(|_| std::env::current_exe()).unwrap_or_else(|_| "sheprd".into());
    let log = state_dir().join("sheprd.log");
    let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) else {
        bad("couldn't open the log file");
        return false;
    };
    let mut cmd = Command::new(&exe);
    cmd.arg("serve").stdin(Stdio::null()).stdout(f.try_clone().unwrap()).stderr(f);
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Err(e) = cmd.spawn() {
        bad(&format!("couldn't start sheprd serve: {e}"));
        return false;
    }
    match wait_health(15) {
        Some(_) => {
            ok(&format!("started in the background on 127.0.0.1:{} (log: {})", port(), tilde(&log)));
            note("it won't restart by itself after a reboot: run `sheprd serve` from your startup scripts");
            true
        }
        None => {
            bad(&format!("sheprd didn't come up; see {}", tilde(&log)));
            false
        }
    }
}

fn install_plugin() {
    let dir = data_dir().join("plugin");
    let _ = std::fs::remove_dir_all(&dir);
    for (rel, bytes) in crate::assets::MOD {
        let p = dir.join(rel);
        if std::fs::create_dir_all(p.parent().unwrap()).and_then(|_| std::fs::write(&p, bytes)).is_err() {
            bad(&format!("couldn't write {}", tilde(&p)));
            return;
        }
    }
    let d = dir.to_string_lossy().to_string();
    let (_, list) = sh("claude", &["plugin", "marketplace", "list"], 30);
    let (good, out) = if list.contains(MARKETPLACE) {
        sh("claude", &["plugin", "marketplace", "update", MARKETPLACE], 60)
    } else {
        sh("claude", &["plugin", "marketplace", "add", &d], 60)
    };
    if !good {
        bad(&format!("couldn't register the plugin marketplace: {}", last_line(&out)));
        fix(&format!("claude plugin marketplace add {d}"));
        return;
    }
    let base = format!("baseUrl={}", local_url());
    let mut args = vec!["plugin", "install", PLUGIN, "--scope", "user", "-y"];
    if port() != DEFAULT_PORT {
        args.extend(["--config", base.as_str()]);
    }
    let (good, out) = sh("claude", &args, 120);
    if good || out.contains("already installed") {
        let _ = sh("claude", &["plugin", "enable", PLUGIN, "--scope", "user"], 30);
        ok("plugin `sheprd` installed: agents can send screenshots, links and questions to your phone");
        note("running Claude sessions pick it up when restarted");
    } else {
        bad(&format!("plugin install failed: {}", last_line(&out)));
        fix(&format!("claude plugin install {PLUGIN} --scope user"));
    }
}

fn last_line(s: &str) -> String {
    s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(200).collect()
}

fn tailscale_serve(o: &Opts) -> Option<String> {
    let st = tailscale_status()?;
    let Some(name) = ts_name(&st) else {
        warn("Tailscale isn't connected, so the phone can't reach this machine yet.");
        fix("sudo tailscale up    # then: sheprd setup");
        return None;
    };
    if let Some(p) = serve_port() {
        let url = if p == 443 { format!("https://{name}/") } else { format!("https://{name}:{p}/") };
        ok(&format!("Tailscale Serve: {url} → 127.0.0.1:{}", port()));
        return Some(url);
    }
    // 443 if free, else 8443 (never take over someone else's route)
    let https = if !serve_taken(443) { 443 } else if !serve_taken(8443) { 8443 } else { 10443 };
    let target = local_url();
    let flag = format!("--https={https}");
    let cmd = format!("tailscale serve --bg {flag} {target}");
    if !ask(&format!("Serve sheprd over HTTPS inside your tailnet ({cmd})?"), o.yes) {
        note("skipped");
        return None;
    }
    let (good, out) = sh("tailscale", &["serve", "--bg", &flag, &target], 20);
    if good && serve_port().is_some() {
        let url = if https == 443 { format!("https://{name}/") } else { format!("https://{name}:{https}/") };
        ok(&format!("Tailscale Serve: {url} → {target} (only your tailnet can reach it)"));
        return Some(url);
    }
    let lower = out.to_lowercase();
    if let Some(link) = out.split_whitespace().find(|w| w.starts_with("https://login.tailscale.com")) {
        warn("HTTPS certificates are off for your tailnet. Turn them on once (one click):");
        fix(link);
        note("then run `sheprd setup` again");
    } else if lower.contains("access denied") || lower.contains("permission") || lower.contains("operator") {
        warn("Tailscale needs permission to let you configure Serve. Run this once:");
        fix(&format!("sudo tailscale set --operator=$USER && sheprd setup"));
        note(&format!("or just: sudo {cmd}"));
    } else {
        bad(&format!("tailscale serve failed: {}", last_line(&out)));
        fix(&format!("sudo {cmd}"));
    }
    // plain HTTP over the tailnet still works (the server binds the Tailscale IP on the default port)
    (port() == DEFAULT_PORT).then(|| {
        note("meanwhile the phone can use plain HTTP (everything except push notifications):");
        format!("http://{name}:{}/", port())
    })
}

fn wait_for_phone() {
    let since = crate::transcript::now_ms();
    let subs0 = health().map(|h| h["push"]["subscribers"].as_u64().unwrap_or(0)).unwrap_or(0);
    println!("\n  {} (Ctrl-C to finish later)", paint("2", "Waiting for your phone…"));
    let start = Instant::now();
    let mut seen = false;
    while start.elapsed() < Duration::from_secs(15 * 60) {
        std::thread::sleep(Duration::from_secs(2));
        let Some(h) = health() else { continue };
        if !seen && h["lastPhone"].as_i64().unwrap_or(0) > since {
            seen = true;
            ok("sheprd is open on your phone");
            if h["push"]["subscribers"].as_u64().unwrap_or(0) <= subs0 {
                println!("  {}", paint("2", "Now tap ⚙ → Enable notifications (from the Home Screen app)…"));
            }
        }
        if h["push"]["subscribers"].as_u64().unwrap_or(0) > subs0 {
            ok("notifications enabled");
            if http("POST", "/api/push/test", Some("{}")).is_some_and(|(c, _)| c == 200) {
                ok("sent a test notification: it should be on your phone now");
            }
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// doctor

fn doctor() -> i32 {
    let mut fails = 0;
    let mut f = |m: &str| {
        bad(m);
        fails += 1;
    };
    println!("{}", paint("1", &format!("sheprd {VERSION} doctor")));

    step("Server");
    let exe = std::env::current_exe().ok();
    match which("sheprd") {
        Some(p) if exe.as_ref().and_then(|e| e.canonicalize().ok()) == p.canonicalize().ok() => ok(&format!("on PATH: {}", tilde(&p))),
        Some(p) => warn(&format!("`sheprd` on PATH is {}, not this binary", tilde(&p))),
        None => {
            warn("`sheprd` isn't on your PATH");
            fix("export PATH=\"$HOME/.local/bin:$PATH\"   # add to your shell rc");
        }
    }
    let h = health();
    match &h {
        Some(h) => {
            ok(&format!("server {} answers on 127.0.0.1:{}", h["version"].as_str().unwrap_or("?"), port()));
            if h["version"] != VERSION {
                warn(&format!("the running server is {}, this binary is {VERSION}", h["version"].as_str().unwrap_or("?")));
                fix("sheprd setup    # installs this version and restarts the service");
            }
        }
        None => match http("GET", "/", None) {
            Some(_) => {
                f(&format!("something that isn't sheprd answers on port {}", port()));
                fix(&format!("ss -ltnp 'sport = :{}'    # who is it?  or move sheprd: SHEPRD_PORT=7879 sheprd setup", port()));
            }
            None => {
                f(&format!("nothing answers on 127.0.0.1:{}", port()));
                fix("sheprd setup");
            }
        },
    }
    if systemd_user() {
        let (_, active) = sh("systemctl", &["--user", "is-active", UNIT], 5);
        let (_, enabled) = sh("systemctl", &["--user", "is-enabled", UNIT], 5);
        match (active.as_str(), enabled.as_str()) {
            ("active", "enabled") => ok(&format!("{UNIT}: active, starts at login")),
            (_, e) if e.contains("No such file") || e == "not-found" || !unit_path().exists() => {
                if h.is_some() {
                    note(&format!("{UNIT} isn't installed (running by hand?)"));
                } else {
                    f(&format!("{UNIT} isn't installed"));
                    fix("sheprd setup");
                }
            }
            (a, e) => {
                f(&format!("{UNIT}: {a}, {e}"));
                let (_, log) = sh("journalctl", &["--user", "-u", UNIT, "-n", "5", "--no-pager", "-o", "cat"], 5);
                for l in log.lines() {
                    note(l);
                }
                fix(&format!("systemctl --user restart {UNIT}; journalctl --user -u {UNIT} -f"));
            }
        }
        if let Ok(unit) = std::fs::read_to_string(unit_path()) {
            if let Some(bin) = unit.lines().find_map(|l| l.strip_prefix("ExecStart=")).and_then(|l| l.split_whitespace().next()) {
                if !Path::new(bin).exists() {
                    f(&format!("the service runs {bin}, which no longer exists"));
                    fix("sheprd setup");
                }
            }
        }
    }

    step("herdr");
    match which("herdr") {
        Some(p) => ok(&format!("{} ({})", sh(p.to_str().unwrap_or("herdr"), &["--version"], 5).1, tilde(&p))),
        None => {
            f("herdr isn't installed");
            fix("see https://github.com/herdrdev/herdr");
        }
    }
    match herdr_claude_integration().as_deref() {
        Some("current") => ok("herdr's Claude Code integration is current"),
        Some(st) => {
            f(&format!("herdr's Claude Code integration: {st} (agents show no status)"));
            fix("herdr integration install claude    # then restart Claude sessions");
        }
        None => {}
    }
    let sock = crate::herdr::socket_path();
    match &h {
        Some(h) if h["herdr"] == true => ok(&format!("server is connected to herdr ({} agents)", h["agents"])),
        Some(h) => {
            let srv_sock = h["herdrSocket"].as_str().unwrap_or("");
            if !sock.exists() {
                f(&format!("herdr isn't running (no socket at {})", tilde(&sock)));
                fix("herdr    # start it in a terminal; sheprd connects within seconds");
            } else if srv_sock != sock.to_string_lossy() {
                f(&format!("the server looks for herdr at {srv_sock}, but your herdr socket is {}", tilde(&sock)));
                fix("sheprd setup    # re-captures HERDR_SOCKET_PATH into the service");
            } else {
                warn("herdr is running but the server isn't connected yet (it retries every few seconds)");
            }
        }
        None if sock.exists() => ok(&format!("herdr socket: {}", tilde(&sock))),
        None => warn(&format!("herdr isn't running (no socket at {})", tilde(&sock))),
    }

    step("Claude Code");
    match which("claude") {
        Some(_) => {
            ok("claude is installed");
            let (_, list) = sh("claude", &["plugin", "list"], 30);
            let line = list.lines().skip_while(|l| !l.contains("sheprd@")).take(4).collect::<Vec<_>>().join(" ");
            if line.is_empty() {
                warn("the sheprd plugin isn't installed (no screenshots/questions from agents to the phone)");
                fix("sheprd setup");
            } else if line.to_lowercase().contains("disabled") {
                warn("the sheprd plugin is disabled");
                fix(&format!("claude plugin enable {PLUGIN} --scope user"));
            } else {
                ok("sheprd plugin installed");
            }
        }
        None => {
            warn("claude isn't on PATH");
            fix("curl -fsSL https://claude.ai/install.sh | bash");
        }
    }

    step("Phone access (Tailscale)");
    match which("tailscale") {
        None => {
            warn("Tailscale isn't installed; the phone has no way in (sheprd only listens on localhost)");
            note("install on this machine and the phone: https://tailscale.com/download, then `sheprd setup`");
        }
        Some(_) => match tailscale_status() {
            None => {
                f("`tailscale status` failed: is tailscaled running?");
                fix("sudo systemctl enable --now tailscaled && sudo tailscale up");
            }
            Some(st) => match ts_name(&st) {
                None => {
                    f(&format!("Tailscale is {}", st["BackendState"].as_str().unwrap_or("not running")));
                    fix("sudo tailscale up");
                }
                Some(name) => {
                    ok(&format!("Tailscale is up: {name}"));
                    match serve_port() {
                        Some(p) => {
                            let url = if p == 443 { format!("https://{name}/") } else { format!("https://{name}:{p}/") };
                            ok(&format!("Tailscale Serve → this server: {url}"));
                            if which("curl").is_some() {
                                let (good, out) = sh("curl", &["-fsS", "-m", "8", &format!("{url}api/health")], 10);
                                if good && out.contains("\"sheprd\"") {
                                    ok("the HTTPS URL works end to end");
                                } else {
                                    f(&format!("{url} doesn't answer: {}", last_line(&out)));
                                    note("first use can take ~30 s while Tailscale fetches the certificate; then retry");
                                }
                            }
                        }
                        None => {
                            warn("Tailscale Serve isn't pointing at sheprd: no HTTPS, so no push notifications on iPhone");
                            fix("sheprd setup");
                        }
                    }
                }
            },
        },
    }

    step("Push notifications");
    if let Some(h) = &h {
        if h["push"]["noPush"] == true {
            warn("SHEPRD_NO_PUSH is set: pushes are only logged");
        }
        if h["push"]["enabled"] != true {
            f("push is unavailable (no VAPID key); see the service log");
            fix(&format!("journalctl --user -u {UNIT} -n 30 --no-pager | grep push"));
        } else {
            match h["push"]["subscribers"].as_u64().unwrap_or(0) {
                0 => {
                    warn("no phone has enabled notifications yet");
                    note("iPhone: open the Home Screen app (not a Safari tab), ⚙ → Enable notifications. Needs the https:// URL.");
                }
                n => ok(&format!("{n} device(s) subscribed; test with: curl -XPOST {}/api/push/test", local_url())),
            }
        }
    } else {
        note("server not running: skipped");
    }

    step("Optional extras");
    for (bin, what) in [
        ("git", "branch / diff views"),
        ("gh", "CI and pull-request status (run `gh auth login`)"),
        ("ffmpeg", "voice notes (plus whisper.cpp, see the README)"),
        ("nvidia-smi", "GPU numbers"),
    ] {
        match which(bin) {
            Some(_) => ok(&format!("{bin}: {what}")),
            None => note(&format!("· {bin} not found: no {what}")),
        }
    }

    println!();
    if fails == 0 {
        println!("{}", paint("32;1", "All good."));
        0
    } else {
        println!("{}", paint("31;1", &format!("{fails} problem(s) found; the → lines say how to fix them.")));
        1
    }
}

// ---------------------------------------------------------------------------------------------------------
// uninstall

fn uninstall(purge: bool) -> i32 {
    if systemd_user() {
        let _ = sh("systemctl", &["--user", "disable", "--now", UNIT], 20);
        if std::fs::remove_file(unit_path()).is_ok() {
            let _ = sh("systemctl", &["--user", "daemon-reload"], 15);
            ok(&format!("removed {UNIT}"));
        }
    }
    // started without systemd (setup's fallback): stop it by pid
    if let Some(pid) = health().and_then(|h| h["pid"].as_i64()) {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        let t = Instant::now();
        while health().is_some() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if health().is_none() { ok("stopped the server") } else { warn(&format!("couldn't stop the server (pid {pid})")) }
    }
    if which("tailscale").is_some() {
        if let Some(p) = serve_port() {
            let (good, _) = sh("tailscale", &["serve", &format!("--https={p}"), "off"], 15);
            if good { ok("removed the Tailscale Serve route") } else { warn(&format!("run: tailscale serve --https={p} off")) }
        }
    }
    if which("claude").is_some() {
        let _ = sh("claude", &["plugin", "uninstall", PLUGIN, "--scope", "user"], 60);
        let _ = sh("claude", &["plugin", "marketplace", "remove", MARKETPLACE], 60);
        ok("removed the Claude Code plugin");
    }
    let _ = std::fs::remove_dir_all(data_dir());
    if purge {
        let _ = std::fs::remove_dir_all(state_dir());
        let _ = std::fs::remove_dir_all(home().join(".cache/sheprd"));
        ok("removed state and caches");
    } else {
        note(&format!("kept your settings in {} (--purge removes them)", tilde(&state_dir())));
    }
    let bin = home().join(".local/bin/sheprd");
    if bin.exists() {
        note(&format!("the binary stays: rm {}", tilde(&bin)));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_has_path_and_exec() {
        let u = unit_text(Path::new("/x/sheprd"));
        assert!(u.contains("ExecStart=/x/sheprd serve"));
        assert!(u.contains("Environment=\"PATH="));
        assert!(u.contains("WantedBy=default.target"));
    }

    #[test]
    fn qr_renders() {
        let code = qrcode::QrCode::new(b"https://devbox.example.ts.net/").unwrap();
        let s = code.render::<qrcode::render::unicode::Dense1x2>().build();
        assert!(s.lines().count() > 10);
    }
}
