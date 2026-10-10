//! Running apps: background Bash jobs that serve something (`python -m http.server`, `npm run dev`, `vite`,
//! `tailscale serve`, …), told apart by the activity taxonomy (activity::APP_ACTS). Home lists them in their own
//! small section, never as In-flight work or Briefing clauses. `info` says where one listens and which URL the
//! phone can open: the tailnet address when it binds to it (or to every interface), the `tailscale serve` HTTPS
//! name for a serve job, else only a local URL (no proxy yet).
//!
//! Ports come from the command line and, when its processes are found, from their LISTEN sockets in /proc.
//!
//! `POST /api/apps/about {id}`: one or two plain sentences saying what the app is, written once by Haiku (thinking off)
//! from the job's description and command, port, folder, owning agent and the served page's `<title>`; cached by
//! session + port + command in `<state>/app-about.json`, at most `HAIKU_PER_DAY` new ones a day. Only asked when the
//! phone opens an app (never on refresh); until it is ready the phone shows the job description.

use crate::Shared;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

/// `{port, host, url, local, alive, via}` for a running app job; Null when the command is not a server.
pub fn info(id: &str, cmd: &str, ts_name: &str, ts_ip: &str) -> Value {
    if !crate::activity::app_cmd(cmd) {
        return Value::Null;
    }
    let pids = job_pids(id, cmd);
    let listen = listening(&pids);
    let seg = main_seg(cmd);
    if let Some(t) = tailscale_serve(&seg) {
        let url = (!ts_name.is_empty()).then(|| format!("https://{ts_name}{}", t.path));
        return json!({"via": "tailscale", "port": t.port, "url": url, "local": t.port.map(|p| format!("http://127.0.0.1:{p}")), "alive": !pids.is_empty() || t.bg});
    }
    let (cport, chost) = from_cmd(&seg);
    // a socket the process really listens on wins; prefer the port the command names
    let sock = listen
        .iter()
        .find(|(h, p)| Some(*p) == cport && chost.as_deref().is_none_or(|c| c == h || c == "localhost" && h == "127.0.0.1"))
        .or_else(|| listen.iter().find(|(_, p)| Some(*p) == cport))
        .or_else(|| listen.first())
        .cloned();
    let port = sock.as_ref().map(|s| s.1).or(cport);
    let host = sock.map(|s| s.0).or(chost).unwrap_or_default();
    let Some(port) = port else {
        return json!({"via": null, "port": null, "url": null, "local": null, "alive": !pids.is_empty()});
    };
    let open = host == ts_ip || host.starts_with("100.") || matches!(host.as_str(), "0.0.0.0" | "::" | "*" | "");
    // an unnamed bind: python's http.server takes every interface, dev servers mostly localhost
    let open = open && (host != "" || seg.contains("http.server"));
    let url = if !open || ts_ip.is_empty() {
        None
    } else if host.starts_with("100.") {
        Some(format!("http://{host}:{port}"))
    } else {
        Some(format!("http://{ts_ip}:{port}"))
    };
    json!({"via": if url.is_some() { "tailnet" } else { "local" }, "port": port, "host": host, "url": url, "local": format!("http://127.0.0.1:{port}"), "alive": !pids.is_empty()})
}

/// The command's first real segment, whole: no `cd …&&`, env assignments or wrappers (`nohup`, `exec`, `uv run`).
fn main_seg(cmd: &str) -> String {
    let line = cmd.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let seg = line.split("&&").map(str::trim).find(|s| !s.starts_with("cd ") && *s != "cd" && !s.is_empty()).unwrap_or("");
    let seg = seg.split(['|', ';']).next().unwrap_or("").trim_end_matches('&').trim();
    let toks: Vec<&str> = seg.split_whitespace().map(|t| t.trim_matches(['\'', '"'])).collect();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if matches!(t, "nohup" | "exec" | "env" | "setsid" | "stdbuf") || (t.contains('=') && !t.starts_with('-')) {
            i += 1;
        } else if (t == "uv" || t == "uvx") && toks.get(i + 1) == Some(&"run") {
            i += 2;
        } else {
            break;
        }
    }
    let port_env = toks[..i].iter().find(|t| t.starts_with("PORT=")).map(|t| format!(" {t}")).unwrap_or_default();
    toks[i..].join(" ") + &port_env
}

struct Serve {
    port: Option<u16>,
    path: String,
    bg: bool,
}

fn tailscale_serve(seg: &str) -> Option<Serve> {
    let rest = seg.strip_prefix("sudo ").unwrap_or(seg);
    let rest = rest.strip_prefix("tailscale serve").or_else(|| rest.strip_prefix("tailscale funnel"))?;
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let mut path = String::new();
    let mut port = None;
    let mut bg = false;
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if t == "--bg" {
            bg = true;
        } else if let Some(v) = t.strip_prefix("--set-path=") {
            path = v.to_string();
        } else if t == "--set-path" {
            path = toks.get(i + 1).unwrap_or(&"").to_string();
            i += 1;
        } else if !t.starts_with('-') && t != "reset" && t != "status" {
            port = t.rsplit(':').next().and_then(|p| p.trim_end_matches('/').parse().ok()).or(port);
        }
        i += 1;
    }
    if !path.is_empty() && !path.starts_with('/') {
        path.insert(0, '/');
    }
    Some(Serve { port, path, bg })
}

/// (port, bind host) named on the command line.
fn from_cmd(seg: &str) -> (Option<u16>, Option<String>) {
    let toks: Vec<&str> = seg.split_whitespace().collect();
    let num = |s: &str| s.parse::<u16>().ok().filter(|p| *p >= 80);
    let mut port = None;
    let mut host = None;
    for (i, t) in toks.iter().enumerate() {
        let next = toks.get(i + 1).copied().unwrap_or("");
        let (k, v) = match t.split_once('=') {
            Some((k, v)) if k.starts_with('-') => (k, v),
            _ => (*t, next),
        };
        match k {
            "--port" | "-p" | "--listen-port" | "--server.port" => port = port.or(num(v)),
            "--bind" | "-b" | "--host" | "--hostname" | "--address" | "--ip" | "--server.address" | "-H" => {
                if let Some((h, p)) = v.rsplit_once(':').filter(|(_, p)| num(p).is_some()) {
                    host = Some(h.trim_matches(['[', ']']).to_string());
                    port = port.or(num(p));
                } else if !v.is_empty() {
                    host = Some(v.to_string());
                }
            }
            "--listen" | "-l" | "--addr" => {
                if let Some((h, p)) = v.rsplit_once(':') {
                    port = port.or(num(p));
                    if !h.is_empty() {
                        host = Some(h.to_string());
                    }
                } else {
                    port = port.or(num(v));
                }
            }
            "-S" => {
                if let Some((h, p)) = v.rsplit_once(':') {
                    host = Some(h.to_string());
                    port = port.or(num(p));
                }
            }
            _ => {}
        }
    }
    // `python -m http.server 4180`, `serve 4180`
    if port.is_none() {
        if let Some(at) = toks.iter().position(|t| *t == "http.server" || *t == "serve" || *t == "http-server") {
            port = toks[at + 1..].iter().find(|t| !t.starts_with('-')).and_then(|t| num(t));
        }
    }
    if port.is_none() {
        port = toks.iter().find_map(|t| t.strip_prefix("PORT=").and_then(num));
    }
    (port, host)
}

/// The job's processes: the process matching most of its command, and every descendant.
fn job_pids(id: &str, cmd: &str) -> Vec<u32> {
    // Always a /proc walk: the sampler's job match (res.rs) looks at two leading tokens only and can give two
    // `python3 -m http.server 4180 --bind …` jobs the same process (the tailnet one then read as local only).
    let _ = id;
    let mut parent: Vec<(u32, u32)> = Vec::new();
    let mut root = None;
    {
        let toks = crate::res::job_tokens(cmd);
        if toks.is_empty() {
            return vec![];
        }
        // two servers can share the leading tokens (`python3 -m http.server 4180 --bind …` twice): the process
        // matching most of the whole command wins
        let full = main_seg(cmd);
        let words: Vec<&str> = full.split_whitespace().collect();
        let mut best = 0usize;
        let Ok(rd) = std::fs::read_dir("/proc") else { return vec![] };
        let me = std::process::id();
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            let ppid = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| s.rsplit_once(')').and_then(|(_, r)| r.split_whitespace().nth(1).and_then(|x| x.parse().ok())))
                .unwrap_or(0);
            parent.push((pid, ppid));
            if pid == me {
                continue;
            }
            let Ok(b) = std::fs::read(format!("/proc/{pid}/cmdline")) else { continue };
            let c = String::from_utf8_lossy(&b).replace('\0', " ");
            let first = c.split_whitespace().next().unwrap_or("");
            let shell = first.ends_with("bash") || first.ends_with("/sh") || first == "sh" || first.ends_with("zsh");
            if !shell && toks.iter().all(|t| c.contains(t.as_str())) {
                let score = 1 + words.iter().filter(|w| c.contains(*w)).count();
                if score > best {
                    best = score;
                    root = Some(pid);
                }
            }
        }
    }
    let Some(root) = root else { return vec![] };
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(p, pp) in &parent {
        kids.entry(pp).or_default().push(p);
    }
    let mut out = vec![];
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if out.len() > 256 {
            break;
        }
        out.push(p);
        stack.extend(kids.get(&p).into_iter().flatten().copied().filter(|&c| c != p));
    }
    out
}

/// LISTEN TCP sockets of these processes: (address, port), lowest port first.
fn listening(pids: &[u32]) -> Vec<(String, u16)> {
    if pids.is_empty() {
        return vec![];
    }
    let mut inodes = HashSet::new();
    for p in pids {
        let Ok(rd) = std::fs::read_dir(format!("/proc/{p}/fd")) else { continue };
        for e in rd.flatten() {
            if let Ok(l) = std::fs::read_link(e.path()) {
                if let Some(n) = l.to_str().and_then(|s| s.strip_prefix("socket:[")).and_then(|s| s.strip_suffix(']')) {
                    inodes.insert(n.to_string());
                }
            }
        }
    }
    let mut out: Vec<(String, u16)> = vec![];
    for (f, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(s) = std::fs::read_to_string(f) else { continue };
        for (addr, port, inode) in parse_listen(&s, v6) {
            if inodes.contains(&inode) && !out.iter().any(|(_, p)| *p == port) {
                out.push((addr, port));
            }
        }
    }
    out.sort_by_key(|x| x.1);
    out
}

/// Where the phone can open a port on this box: `Some(url)` when something listens on it on every interface or the
/// tailnet address (the tailnet URL), `None` when nothing listens or only loopback does (the phone can't reach it).
pub fn phone_url(port: u16, ts_ip: &str) -> Option<String> {
    let mut rows = vec![];
    for (f, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        if let Ok(s) = std::fs::read_to_string(f) {
            rows.extend(parse_listen(&s, v6).into_iter().filter(|r| r.1 == port).map(|r| r.0));
        }
    }
    reach_of(&rows, port, ts_ip)
}

fn reach_of(addrs: &[String], port: u16, ts_ip: &str) -> Option<String> {
    if let Some(h) = addrs.iter().find(|h| h.starts_with("100.")) {
        return Some(format!("http://{h}:{port}/"));
    }
    let open = addrs.iter().any(|h| h == "0.0.0.0" || h == "::" || (!ts_ip.is_empty() && h == ts_ip));
    (open && !ts_ip.is_empty()).then(|| format!("http://{ts_ip}:{port}/"))
}

/// LISTEN rows of /proc/net/tcp{,6}: (address, port, inode).
fn parse_listen(s: &str, v6: bool) -> Vec<(String, u16, String)> {
    let mut out = vec![];
    for l in s.lines().skip(1) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 10 || f[3] != "0A" {
            continue;
        }
        let Some((a, p)) = f[1].split_once(':') else { continue };
        let Ok(port) = u16::from_str_radix(p, 16) else { continue };
        let addr = if !v6 {
            match u32::from_str_radix(a, 16) {
                Ok(n) => std::net::Ipv4Addr::from(n.swap_bytes()).to_string(),
                Err(_) => continue,
            }
        } else if a.chars().all(|c| c == '0') {
            "::".into()
        } else if a == "00000000000000000000000001000000" {
            "::1".into()
        } else {
            a.to_string()
        };
        out.push((addr, port, f[9].to_string()));
    }
    out
}

// ---------------------------------------------------------------------------
// About: what the app is, in a sentence or two

const HAIKU_PER_DAY: u32 = 20;

/// What /api/jobs last said about each running app job (id → the job JSON with `app`), so /about needs only an id.
static SEEN: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(Default::default);

/// Called by /api/jobs with every running app job.
pub fn remember(jobs: &[Value]) {
    let mut m = SEEN.lock().unwrap();
    m.clear();
    for j in jobs.iter().filter(|j| !j["app"].is_null()) {
        if let Some(id) = j["id"].as_str() {
            m.insert(id.to_string(), j.clone());
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct AboutStore {
    about: HashMap<String, String>,
    day: String,
    haiku_n: u32,
}
static STORE: LazyLock<Mutex<AboutStore>> = LazyLock::new(|| {
    Mutex::new(std::fs::read(about_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())
});
static ASKING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

fn about_file() -> std::path::PathBuf {
    crate::state_dir().join("app-about.json")
}
fn save(s: &AboutStore) {
    if let Ok(b) = serde_json::to_vec_pretty(s) {
        let tmp = about_file().with_extension("json.tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(tmp, about_file());
        }
    }
}

/// Cache key: owner session + port + command (FNV-1a of the command).
fn about_key(j: &Value) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in j["cmd"].as_str().unwrap_or("").bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    format!("{}:{}:{h:016x}", j["tid"].as_str().unwrap_or(""), j["app"]["port"])
}

/// The folder the command runs in (`cd <dir> && …`).
fn cmd_dir(cmd: &str) -> String {
    cmd.lines()
        .next()
        .and_then(|l| l.trim().strip_prefix("cd "))
        .and_then(|r| r.split("&&").next())
        .map(|d| d.trim().trim_matches(['"', '\'']).to_string())
        .unwrap_or_default()
}

/// The served page's `<title>`: one plain HTTP/1.0 GET on the address it listens on, 1.5 s, 64 KB.
async fn page_title(host: &str, port: u16) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let h = if host.is_empty() || host == "0.0.0.0" || host == "::" || host == "*" || host == "localhost" { "127.0.0.1" } else { host };
    if !(h.starts_with("127.") || h.starts_with("100.")) {
        return None;
    }
    let fut = async {
        let mut s = tokio::net::TcpStream::connect((h, port)).await.ok()?;
        s.write_all(format!("GET / HTTP/1.0\r\nHost: localhost:{port}\r\nAccept: text/html\r\n\r\n").as_bytes()).await.ok()?;
        let mut buf = Vec::new();
        let _ = (&mut s).take(65536).read_to_end(&mut buf).await;
        let t = String::from_utf8_lossy(&buf);
        let low = t.to_lowercase();
        let a = low.find("<title")?;
        let a = a + low[a..].find('>')? + 1;
        let b = a + low[a..].find("</title")?;
        let title = t.get(a..b)?.split_whitespace().collect::<Vec<_>>().join(" ");
        (!title.is_empty()).then(|| title.chars().take(120).collect())
    };
    tokio::time::timeout(Duration::from_millis(1500), fut).await.ok().flatten()
}

async fn haiku_about(j: &Value, agent_activity: &str) -> Result<String, String> {
    let app = &j["app"];
    let port = app["port"].as_u64().map(|p| p as u16);
    let title = match port {
        Some(p) => page_title(app["host"].as_str().unwrap_or(""), p).await,
        None => None,
    };
    let sys = "You tell a phone user, in plain words, what a program running on their computer is. Reply with one or two \
               short sentences (at most 30 words in all): what it is, then what it is for. No markdown, no URLs, no port \
               numbers, no file paths, no quotes. Do not mention Claude, agents or background jobs.";
    let mut prompt = String::from("A background server one of my coding agents started.\n");
    let mut line = |k: &str, v: &str| {
        if !v.trim().is_empty() {
            prompt.push_str(&format!("{k}: {}\n", v.chars().take(300).collect::<String>()));
        }
    };
    line("Description", j["desc"].as_str().unwrap_or(""));
    line("Command", &main_seg(j["cmd"].as_str().unwrap_or("")));
    line("Folder", &cmd_dir(j["cmd"].as_str().unwrap_or("")));
    line("Port", &port.map(|p| p.to_string()).unwrap_or_default());
    line("Served page title", title.as_deref().unwrap_or(""));
    line("Agent working on", j["agentTitle"].as_str().unwrap_or(""));
    line("Agent's activity", agent_activity);
    let v = crate::newagent::run_haiku(Some(sys), &prompt, Duration::from_secs(60)).await?;
    let r = v["result"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    let r = r.trim().trim_matches('"').to_string();
    if r.is_empty() || r.len() > 400 {
        return Err(format!("unusable reply: {r}"));
    }
    Ok(r)
}

#[derive(Deserialize)]
struct AboutBody {
    id: String,
}

/// `{about: "…" | null, pending: bool}`; 404 for a job that is not a running app.
async fn api_about(State(app): State<Shared>, Json(b): Json<AboutBody>) -> Response {
    let Some(j) = SEEN.lock().unwrap().get(&b.id).cloned() else {
        return crate::json_err(axum::http::StatusCode::NOT_FOUND, "not a running app");
    };
    let key = about_key(&j);
    let mut s = STORE.lock().unwrap();
    if let Some(a) = s.about.get(&key) {
        return Json(json!({"about": a, "pending": false})).into_response();
    }
    if ASKING.lock().unwrap().contains(&key) {
        return Json(json!({"about": null, "pending": true})).into_response();
    }
    let day = format!("{}", crate::transcript::now_ms() / 86_400_000);
    if s.day != day {
        s.day = day;
        s.haiku_n = 0;
    }
    if s.haiku_n >= HAIKU_PER_DAY || std::env::var("SHEPRD_APP_HAIKU").is_ok_and(|v| v == "0") {
        return Json(json!({"about": null, "pending": false})).into_response();
    }
    s.haiku_n += 1;
    save(&s);
    drop(s);
    ASKING.lock().unwrap().insert(key.clone());
    // the owning agent's current activity phrase (activity.rs), when the state has one
    let st = app.inner.lock().unwrap().state.clone();
    let act = serde_json::from_str::<Value>(&st)
        .ok()
        .and_then(|v| {
            let a = v["agents"].as_array()?.iter().find(|a| a["pane"] == j["pane"])?.clone();
            a["activity"]["phrase"].as_str().map(str::to_string)
        })
        .unwrap_or_default();
    tokio::spawn(async move {
        match haiku_about(&j, &act).await {
            Ok(a) => {
                let mut s = STORE.lock().unwrap();
                s.about.insert(key.clone(), a);
                save(&s);
            }
            Err(e) => eprintln!("app about: {e}"),
        }
        ASKING.lock().unwrap().remove(&key);
    });
    Json(json!({"about": null, "pending": true})).into_response()
}

pub fn routes() -> Router<Shared> {
    Router::new().route("/api/apps/about", post(api_about))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_apps() {
        for c in [
            "cd /tmp/x/web-art-dist && python3 -m http.server 4180 --bind 100.64.0.5",
            "npm run dev",
            "cd web && npx vite --port 5174",
            "tailscale serve --bg 4180",
            "uvicorn app:app --port 8001",
            "python3 web-dev/mock.py 7884",
        ] {
            assert!(crate::activity::app_cmd(c), "{c}");
        }
        for c in ["cargo test", "python3 train.py --epochs 10", "sleep 60 && curl localhost:7878", "tailscale status", "npm run build"] {
            assert!(!crate::activity::app_cmd(c), "{c}");
        }
    }

    #[test]
    fn main_segment() {
        assert_eq!(main_seg("cd /tmp/x && python3 -m http.server 4180 --bind 127.0.0.1"), "python3 -m http.server 4180 --bind 127.0.0.1");
        assert_eq!(main_seg("PORT=3001 nohup npm run dev > log 2>&1 &"), "npm run dev > log 2>&1 PORT=3001");
        assert_eq!(main_seg("tailscale serve --bg 4180"), "tailscale serve --bg 4180");
    }

    #[test]
    fn ports_from_commands() {
        assert_eq!(from_cmd("python3 -m http.server 4180 --bind 100.64.0.5"), (Some(4180), Some("100.64.0.5".into())));
        assert_eq!(from_cmd("python3 -m http.server 4180 --bind 127.0.0.1"), (Some(4180), Some("127.0.0.1".into())));
        assert_eq!(from_cmd("npx vite --port=5174 --host 0.0.0.0"), (Some(5174), Some("0.0.0.0".into())));
        assert_eq!(from_cmd("uvicorn app:app --host 127.0.0.1:8001"), (Some(8001), Some("127.0.0.1".into())));
        assert_eq!(from_cmd("php -S localhost:8080"), (Some(8080), Some("localhost".into())));
        assert_eq!(from_cmd("npm run dev"), (None, None));
        let t = tailscale_serve("tailscale serve --bg --set-path art http://127.0.0.1:4180").unwrap();
        assert_eq!((t.port, t.path.as_str(), t.bg), (Some(4180), "/art", true));
    }

    #[test]
    fn phone_reach() {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(reach_of(&v(&["127.0.0.1"]), 7896, "100.64.0.7"), None);
        assert_eq!(reach_of(&v(&[]), 7896, "100.64.0.7"), None);
        assert_eq!(reach_of(&v(&["0.0.0.0"]), 5173, "100.64.0.7").as_deref(), Some("http://100.64.0.7:5173/"));
        assert_eq!(reach_of(&v(&["::1", "::"]), 5173, "100.64.0.7").as_deref(), Some("http://100.64.0.7:5173/"));
        assert_eq!(reach_of(&v(&["100.64.0.7"]), 8790, "").as_deref(), Some("http://100.64.0.7:8790/"));
        assert_eq!(reach_of(&v(&["0.0.0.0"]), 5173, ""), None);
    }

    #[test]
    fn listen_rows() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1054 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000\n   1: 0100007F:1F90 0100007F:9999 01 00000000:00000000 00:00000000 00000000  1000        0 999 1 0000\n";
        assert_eq!(parse_listen(tcp, false), vec![("127.0.0.1".to_string(), 4180, "12345".to_string())]);
    }
}
