//! What a share carries (Send to the home agent, send.rs, `POST /api/send`): the request body (JSON, urlencoded form or multipart: url, text, title, note, source, files),
//! upload checks (claimed mime family + magic bytes + size), generated file names, the request check and the small
//! native-looking confirmation page both answer when a browser (not a Shortcut) posts.

use crate::now_ms;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, header};
use serde::Deserialize;
use std::path::Path;

pub const MAX_IMAGE: usize = 20 << 20;
pub const MAX_PDF: usize = 40 << 20;
pub const MAX_AUDIO: usize = 48 << 20;
pub const MAX_TEXT: usize = 20_000;

pub fn local_tm(ms: i64) -> libc::tm {
    let t = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r writes only into `tm`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm
}

/// Local calendar date of a unix-ms time.
pub fn local_date(ms: i64) -> String {
    let tm = local_tm(ms);
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

/// This server's own Host header values: loopback, the tailnet name (Serve on 443) and IP, and `SHEPRD_LISTEN`
/// addresses. A request whose Host is anything else (DNS rebinding) is refused.
pub(crate) async fn own_hosts(app: &crate::App) -> Vec<String> {
    let p = crate::port();
    let mut v = vec![format!("127.0.0.1:{p}"), format!("localhost:{p}")];
    let (name, ip) = if app.ts_name.is_empty() || app.ts_ip.is_empty() {
        crate::tailscale_identity().await
    } else {
        (app.ts_name.clone(), app.ts_ip.clone())
    };
    if !name.is_empty() {
        v.push(name.to_ascii_lowercase());
        v.push(format!("{}:443", name.to_ascii_lowercase()));
    }
    if !ip.is_empty() {
        v.push(format!("{ip}:{p}"));
    }
    v.extend(std::env::var("SHEPRD_LISTEN").unwrap_or_default().split(',').map(str::trim).filter(|a| !a.is_empty()).map(str::to_ascii_lowercase));
    v
}

pub fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).collect::<String>() + "…" }
}

/// The text fields of a submission.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Fields {
    pub url: Option<String>,
    pub text: Option<String>,
    pub title: Option<String>,
    pub note: Option<String>,
    pub source: Option<String>,
    /// idempotency key (the app's Send sheet: a double tap sends once)
    pub nonce: Option<String>,
}

pub fn is_http(s: &str) -> bool {
    (s.starts_with("https://") || s.starts_with("http://")) && s.len() > 10 && s.len() < 4096 && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// First http(s) URL in free text (a share from Android or Shortcuts often puts the link in `text`).
pub fn find_url(s: &str) -> Option<String> {
    s.split_whitespace()
        .map(|w| w.trim_start_matches(['<', '(', '"', '\'']).trim_end_matches(['>', ')', '"', '\'', '.', ',', ';', '!', '?']))
        .find(|w| is_http(w))
        .map(str::to_string)
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Kind {
    Image,
    Pdf,
    Audio,
}

/// What an upload really is: the claimed mime family must match the magic bytes. → (kind, extension).
pub fn classify(ctype: &str, data: &[u8]) -> Result<(Kind, &'static str), String> {
    let ct = ctype.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let at = |o: usize, m: &[u8]| data.len() >= o + m.len() && &data[o..o + m.len()] == m;
    let brand = if at(4, b"ftyp") && data.len() >= 12 { std::str::from_utf8(&data[8..12]).unwrap_or("") } else { "" };
    let sniff: Option<(Kind, &'static str)> = if at(0, &[0xFF, 0xD8, 0xFF]) {
        Some((Kind::Image, "jpg"))
    } else if at(0, b"\x89PNG\r\n\x1a\n") {
        Some((Kind::Image, "png"))
    } else if at(0, b"GIF87a") || at(0, b"GIF89a") {
        Some((Kind::Image, "gif"))
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        Some((Kind::Image, "webp"))
    } else if matches!(brand, "heic" | "heix" | "mif1" | "msf1" | "heim" | "heis" | "hevc") {
        Some((Kind::Image, "heic"))
    } else if matches!(brand, "avif") {
        Some((Kind::Image, "avif"))
    } else if at(0, b"%PDF-") {
        Some((Kind::Pdf, "pdf"))
    } else if matches!(brand, "M4A " | "M4B " | "mp42" | "isom" | "mp41" | "3gp4" | "3gp5" | "qt  ") {
        Some((Kind::Audio, "m4a"))
    } else if at(0, b"RIFF") && at(8, b"WAVE") {
        Some((Kind::Audio, "wav"))
    } else if at(0, b"OggS") {
        Some((Kind::Audio, "ogg"))
    } else if at(0, &[0x1A, 0x45, 0xDF, 0xA3]) {
        Some((Kind::Audio, "webm"))
    } else if at(0, b"caff") {
        Some((Kind::Audio, "caf"))
    } else if at(0, b"ID3") || (data.len() > 1 && data[0] == 0xFF && data[1] & 0xE0 == 0xE0) {
        Some((Kind::Audio, if data.len() > 1 && data[1] & 0x06 == 0 { "aac" } else { "mp3" }))
    } else {
        None
    };
    let claimed = if ct.starts_with("image/") {
        Some(Kind::Image)
    } else if ct == "application/pdf" {
        Some(Kind::Pdf)
    } else if ct.starts_with("audio/") || ct == "video/mp4" || ct == "video/quicktime" || ct == "video/webm" {
        Some(Kind::Audio) // a voice memo in an mp4/mov container
    } else if ct.is_empty() || ct == "application/octet-stream" {
        sniff.map(|s| s.0)
    } else {
        None
    };
    let Some(claimed) = claimed else { return Err(format!("{ct} files can't be sent (images, PDFs and audio only)")) };
    match sniff {
        Some((k, e)) if k == claimed => {
            let max = match k {
                Kind::Image => MAX_IMAGE,
                Kind::Pdf => MAX_PDF,
                Kind::Audio => MAX_AUDIO,
            };
            if data.len() > max {
                return Err(format!("file too large ({} MB, max {} MB)", data.len() >> 20, max >> 20));
            }
            Ok((k, e))
        }
        _ => Err(format!("that file isn't a readable {}", match claimed {
            Kind::Image => "image",
            Kind::Pdf => "PDF",
            Kind::Audio => "audio file",
        })),
    }
}

/// `<prefix>-<local date+time>-<random>.<ext>`: never anything the client named.
pub fn blob_name(prefix: &str, ext: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_i64(now_ms());
    h.write_u32(std::process::id());
    let t = now_ms();
    let tm = local_tm(t);
    format!("{prefix}-{}-{:02}{:02}{:02}-{:08x}.{ext}", local_date(t).replace('-', ""), tm.tm_hour, tm.tm_min, tm.tm_sec, h.finish() as u32)
}

/// A file's display name: the client's name reduced to safe characters (only ever shown in notes).
pub fn shown_name(n: Option<&str>) -> Option<String> {
    let s: String = Path::new(n?).file_name()?.to_string_lossy().chars().filter(|c| c.is_alphanumeric() || " ._-()".contains(*c)).take(80).collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub struct Upload {
    pub filename: Option<String>,
    pub ctype: String,
    pub data: Bytes,
}

#[derive(Debug)]
pub struct Fail(pub StatusCode, pub String);

/// The user's file name for `--name`: its last path part (either separator), no control characters, trimmed, at
/// most 120 characters; `None` when nothing sensible is left. Generic names ("Memo.m4a") are kept.
pub fn upload_name(n: Option<&str>) -> Option<String> {
    let base = n?.rsplit(['/', '\\']).next().unwrap_or("");
    let s: String = base.chars().filter(|c| !c.is_control()).take(120).collect();
    let s = s.trim().trim_start_matches('-').trim().to_string();
    (!s.is_empty() && s != "." && s != "..").then_some(s)
}

pub fn looks_like_filename(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && !s.contains(char::is_whitespace) && s.rsplit_once('.').is_some_and(|(a, e)| !a.is_empty() && (1..=5).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric()))
}

// --- request bodies ----------------------------------------------------------------------------------------------

pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub ctype: Option<String>,
    pub data: Bytes,
}

pub fn boundary(ct: &str) -> Option<String> {
    let (mime, params) = ct.split_once(';')?;
    if !mime.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    params.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("boundary").then(|| v.trim().trim_matches('"').to_string()).filter(|b| !b.is_empty() && b.len() <= 200)
    })
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let mut i = from;
    while i + needle.len() <= hay.len() {
        match hay[i..hay.len() - needle.len() + 1].iter().position(|&b| b == first) {
            None => return None,
            Some(k) => {
                i += k;
                if &hay[i..i + needle.len()] == needle {
                    return Some(i);
                }
                i += 1;
            }
        }
    }
    None
}

/// `name="…"` from a Content-Disposition value (quoted or bare; `\"` unescaped).
fn disp_param(v: &str, key: &str) -> Option<String> {
    let mut rest = v;
    while let Some(i) = rest.find(';') {
        rest = rest[i + 1..].trim_start();
        let Some((k, after)) = rest.split_once('=') else { break };
        let k = k.trim();
        let (val, tail) = if let Some(q) = after.strip_prefix('"') {
            let mut out = String::new();
            let mut it = q.char_indices();
            let mut end = q.len();
            while let Some((j, c)) = it.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = it.next() {
                            out.push(n);
                        }
                    }
                    '"' => {
                        end = j + 1;
                        break;
                    }
                    _ => out.push(c),
                }
            }
            (out, &q[end.min(q.len())..])
        } else {
            let e = after.find(';').unwrap_or(after.len());
            (after[..e].trim().to_string(), &after[e..])
        };
        if k.eq_ignore_ascii_case(key) {
            return Some(val);
        }
        rest = tail;
    }
    None
}

/// A whole multipart/form-data body → its parts (the body is already capped by the router's limit).
pub fn parse_multipart(body: &Bytes, boundary: &str) -> Result<Vec<Part>, String> {
    let delim = format!("--{boundary}").into_bytes();
    let mut pos = find(body, &delim, 0).ok_or("no multipart boundary in the body")?;
    let mut parts = vec![];
    loop {
        pos += delim.len();
        if body[pos..].starts_with(b"--") {
            break;
        }
        if body[pos..].starts_with(b"\r\n") {
            pos += 2;
        }
        let he = find(body, b"\r\n\r\n", pos).ok_or("malformed multipart headers")?;
        let head = String::from_utf8_lossy(&body[pos..he]).into_owned();
        let start = he + 4;
        let mut end_delim = b"\r\n".to_vec();
        end_delim.extend_from_slice(&delim);
        let end = find(body, &end_delim, start).ok_or("unterminated multipart part")?;
        let (mut name, mut filename, mut ctype) = (None, None, None);
        for l in head.split("\r\n") {
            let Some((k, v)) = l.split_once(':') else { continue };
            if k.trim().eq_ignore_ascii_case("content-disposition") {
                name = disp_param(v, "name");
                filename = disp_param(v, "filename");
            } else if k.trim().eq_ignore_ascii_case("content-type") {
                ctype = Some(v.trim().to_string());
            }
        }
        if let Some(name) = name {
            parts.push(Part { name, filename, ctype, data: body.slice(start..end) });
        }
        if parts.len() > 64 {
            return Err("too many form parts".into());
        }
        pos = end + 2;
    }
    Ok(parts)
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn set_field(f: &mut Fields, k: &str, v: String) {
    let slot = match k {
        "url" | "link" => &mut f.url,
        "text" | "body" => &mut f.text,
        "title" | "name" => &mut f.title,
        "note" => &mut f.note,
        "source" => &mut f.source,
        "nonce" => &mut f.nonce,
        _ => return,
    };
    // repeated text fields (several shared things) are joined
    *slot = Some(match slot.take() {
        Some(old) if !old.trim().is_empty() && !v.trim().is_empty() && old != v => format!("{old}\n{v}"),
        Some(old) if v.trim().is_empty() => old,
        _ => v,
    });
}

/// Any of the three body kinds → fields + files.
pub fn read_body(headers: &HeaderMap, body: &Bytes) -> Result<(Fields, Vec<Upload>), Fail> {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let bad = |e: String| Fail(StatusCode::BAD_REQUEST, e);
    if let Some(b) = boundary(&ct) {
        let mut f = Fields::default();
        let mut files = vec![];
        for p in parse_multipart(body, &b).map_err(bad)? {
            let is_file = p.filename.is_some() || p.ctype.as_deref().is_some_and(|c| !c.starts_with("text/plain"));
            if is_file {
                files.push(Upload { filename: p.filename, ctype: p.ctype.unwrap_or_default(), data: p.data });
            } else if p.data.len() <= MAX_TEXT * 4 {
                set_field(&mut f, &p.name, String::from_utf8_lossy(&p.data).into_owned());
            }
        }
        return Ok((f, files));
    }
    if ct.starts_with("application/x-www-form-urlencoded") {
        let mut f = Fields::default();
        for kv in String::from_utf8_lossy(body).split('&') {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            set_field(&mut f, &urldecode(k), urldecode(v));
        }
        return Ok((f, vec![]));
    }
    if body.is_empty() {
        return Ok((Fields::default(), vec![]));
    }
    serde_json::from_slice::<Fields>(body).map(|f| (f, vec![])).map_err(|e| bad(format!("bad JSON: {e}")))
}

#[derive(Deserialize)]
pub struct FmtQ {
    pub format: Option<String>,
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The text a Shortcut sends next to a file is often just that file's name: true when `t` is one of the uploads'
/// names, or (`loose`: nothing else came with it) any word that looks like a file name.
pub fn is_upload_name(t: &str, files: &[Option<&str>], loose: bool) -> bool {
    let t = t.trim();
    let named = files.iter().flatten().any(|n| Path::new(n).file_name().is_some_and(|b| b.to_string_lossy().eq_ignore_ascii_case(t)));
    !files.is_empty() && !t.is_empty() && (named || (loose && looks_like_filename(t)))
}

fn hv<'a>(h: &'a HeaderMap, k: &str) -> &'a str {
    h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// The Host is ours (no DNS rebinding), not cross-site, not Funnel.
pub fn check_read(h: &HeaderMap, own: &[String]) -> Result<(), &'static str> {
    if h.contains_key("tailscale-funnel-request") {
        return Err("not over Funnel");
    }
    let host = hv(h, "host").to_ascii_lowercase();
    if !own.iter().any(|o| *o == host) {
        return Err("unknown host");
    }
    let site = hv(h, "sec-fetch-site");
    if !site.is_empty() && site != "same-origin" && site != "none" {
        return Err("cross-site request");
    }
    Ok(())
}

/// `check_read` plus Origin = one of ours and same-origin (for requests that come from a browser).
pub fn check_origin(h: &HeaderMap, own: &[String]) -> Result<(), &'static str> {
    check_read(h, own)?;
    let origin = hv(h, "origin").to_ascii_lowercase();
    let auth = origin.strip_prefix("https://").or_else(|| origin.strip_prefix("http://"));
    let ok_origin = auth.is_some_and(|a| {
        !a.contains('/') && (own.iter().any(|o| o == a) || (origin.starts_with("https://") && own.iter().any(|o| *o == format!("{a}:443"))))
    });
    if !ok_origin {
        return Err("origin not allowed");
    }
    let site = hv(h, "sec-fetch-site");
    if !site.is_empty() && site != "same-origin" {
        return Err("cross-site request");
    }
    Ok(())
}


/// Requests that may post a share: from this server's own page (Host + Origin ours) or from no browser at all (an
/// iOS Shortcut sends neither Origin nor Sec-Fetch-Site). A browser on another site always sends Origin, so a page
/// elsewhere can't post a form here; DNS rebinding fails the Host check; Funnel is refused.
pub fn check_sender(h: &HeaderMap, own: &[String]) -> Result<(), &'static str> {
    if h.get(header::ORIGIN).is_some() {
        return check_origin(h, own);
    }
    check_read(h, own)
}

/// The confirmation page a share answers in a browser (Android/desktop share target, or a Shortcut that opens the
/// result). Everything shown is escaped here.
pub struct Page<'a> {
    pub ok: bool,
    /// small caps line under the icon (the home agent's name)
    pub kicker: &'a str,
    pub head: &'a str,
    pub sub: &'a str,
    /// (label, href) of the main button and the quiet link under it
    pub go: (&'a str, &'a str),
    pub again: (&'a str, &'a str),
    /// headline in a serif or the system font
    pub serif: bool,
}

pub fn confirm_page(p: &Page) -> String {
    let icon = if p.ok {
        r#"<svg viewBox="0 0 52 52" aria-hidden="true"><circle cx="26" cy="26" r="25"/><path d="M15 27l7 7 15-16"/></svg>"#
    } else {
        r#"<svg viewBox="0 0 52 52" aria-hidden="true" class="x"><circle cx="26" cy="26" r="25"/><path d="M18 18l16 16M34 18L18 34"/></svg>"#
    };
    let hfont = if p.serif { r#"700 28px/1.1 ui-serif,"New York",Georgia,serif"# } else { "700 26px/1.15 -apple-system,BlinkMacSystemFont,system-ui,sans-serif" };
    format!(
        r##"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="light dark"><title>{head} · {kicker}</title>
<style>
:root{{--bg:#f2f2f7;--card:#fff;--fg:#000;--fg2:#6e6e73;--accent:#007aff;--fill:#006ae6;--ok:#34c759;--bad:#ff3b30;--paper:#f3efe6}}
@media (prefers-color-scheme:dark){{:root{{--bg:#000;--card:#1c1c1e;--fg:#fff;--fg2:#98989f;--accent:#0a84ff;--fill:#0a6de4;--ok:#30d158;--bad:#ff453a}}}}
*{{box-sizing:border-box}}html,body{{margin:0;height:100%}}
body{{background:var(--bg);color:var(--fg);font:17px/1.35 -apple-system,BlinkMacSystemFont,system-ui,sans-serif;display:flex;flex-direction:column;align-items:center;justify-content:center;padding:max(24px,env(safe-area-inset-top)) 16px max(24px,env(safe-area-inset-bottom));-webkit-font-smoothing:antialiased}}
.card{{width:100%;max-width:360px;background:var(--card);border-radius:20px;padding:30px 22px 22px;text-align:center;box-shadow:0 1px 2px rgba(0,0,0,.04),0 8px 28px rgba(0,0,0,.07)}}
svg{{width:64px;height:64px;fill:none;stroke-width:3.2;stroke-linecap:round;stroke-linejoin:round}}
svg circle{{fill:var(--ok);stroke:none;transform-origin:center;animation:pop .32s cubic-bezier(.2,1.4,.4,1)}}svg path{{stroke:#fff;stroke-dasharray:40;stroke-dashoffset:40;animation:tick .35s .12s ease-out forwards}}
svg.x circle{{fill:var(--bad)}}
@keyframes pop{{from{{transform:scale(.4);opacity:0}}}}@keyframes tick{{to{{stroke-dashoffset:0}}}}
.k{{font:600 11px/1 ui-monospace,Menlo,monospace;letter-spacing:.14em;text-transform:uppercase;color:var(--fg2);margin:18px 0 6px}}
h1{{font:{hfont};margin:0 0 8px;letter-spacing:-.01em}}
p{{margin:0 0 22px;color:var(--fg2);font-size:15px;overflow-wrap:anywhere;display:-webkit-box;-webkit-line-clamp:4;-webkit-box-orient:vertical;overflow:hidden}}
a{{display:block;padding:14px;border-radius:12px;background:var(--fill);color:#fff;text-decoration:none;font-weight:600}}
a.s{{background:none;color:var(--accent);padding:12px 0 2px;font-weight:500}}
@media (prefers-reduced-motion:reduce){{svg *{{animation:none!important;stroke-dashoffset:0!important}}}}
</style></head><body><main class="card">{icon}<div class="k">{kicker}</div><h1>{head}</h1><p>{sub}</p>
<a href="{go_href}" id="go">{go}</a><a class="s" href="{again_href}">{again}</a></main></body></html>"##,
        head = esc(p.head),
        kicker = esc(p.kicker),
        sub = esc(p.sub),
        go = esc(p.go.0),
        go_href = esc(p.go.1),
        again = esc(p.again.0),
        again_href = esc(p.again.1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn classify_checks_magic_and_size() {
        let jpg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0];
        assert_eq!(classify("image/jpeg", &jpg), Ok((Kind::Image, "jpg")));
        assert_eq!(classify("image/png", &jpg), Ok((Kind::Image, "jpg"))); // family matches; ext from the bytes
        assert!(classify("application/pdf", &jpg).is_err());
        assert_eq!(classify("application/pdf", b"%PDF-1.7\n"), Ok((Kind::Pdf, "pdf")));
        assert_eq!(classify("audio/x-m4a", b"\0\0\0\x20ftypM4A \0\0"), Ok((Kind::Audio, "m4a")));
        assert_eq!(classify("image/heic", b"\0\0\0\x18ftypheic\0\0"), Ok((Kind::Image, "heic")));
        assert_eq!(classify("", b"%PDF-1.4"), Ok((Kind::Pdf, "pdf")));
        assert!(classify("text/html", b"<html>").is_err());
        assert!(classify("image/svg+xml", b"<svg onload=alert(1)>").is_err());
        assert!(classify("application/x-sh", b"#!/bin/sh").is_err());
        let mut big = vec![0u8; MAX_IMAGE + 1];
        big[..3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        assert!(classify("image/jpeg", &big).unwrap_err().contains("too large"));
    }

    #[test]
    fn blob_names_are_generated() {
        let n = blob_name("img", "jpg");
        assert!(n.starts_with("img-") && n.ends_with(".jpg"));
        assert!(n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'));
        assert_eq!(shown_name(Some("../../etc/pa ss<wd>.pdf")).as_deref(), Some("pa sswd.pdf"));
        assert!(looks_like_filename("IMG_1234.HEIC") && !looks_like_filename("hello world.txt") && !looks_like_filename("nope"));
    }

    #[test]
    fn multipart_round_trip() {
        let body = Bytes::from_static(
            b"--XyZ\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nA \"t\"; x\r\n--XyZ\r\nContent-Disposition: form-data; name=\"url\"\r\n\r\nhttps://a.b/c\r\n--XyZ\r\nContent-Disposition: form-data; name=\"files\"; filename=\"p \\\"q\\\".jpg\"\r\nContent-Type: image/jpeg\r\n\r\n\xFF\xD8\xFF\r\n--x\r\n--XyZ--\r\n",
        );
        assert_eq!(boundary("multipart/form-data; boundary=XyZ").as_deref(), Some("XyZ"));
        assert_eq!(boundary("multipart/form-data; boundary=\"XyZ\"").as_deref(), Some("XyZ"));
        assert_eq!(boundary("application/json"), None);
        let p = parse_multipart(&body, "XyZ").unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!((p[0].name.as_str(), &p[0].data[..]), ("title", &b"A \"t\"; x"[..]));
        assert_eq!(p[2].filename.as_deref(), Some("p \"q\".jpg"));
        assert_eq!(&p[2].data[..], b"\xFF\xD8\xFF\r\n--x"); // a near-boundary inside the data stays data
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("multipart/form-data; boundary=XyZ"));
        let (f, files) = read_body(&h, &body).unwrap();
        assert_eq!(f.url.as_deref(), Some("https://a.b/c"));
        assert_eq!(files.len(), 1);
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-www-form-urlencoded"));
        let (f, _) = read_body(&h, &Bytes::from_static(b"text=hello+there%21&url=https%3A%2F%2Fq.r%2F&bad=%zz")).unwrap();
        assert_eq!((f.text.as_deref(), f.url.as_deref()), (Some("hello there!"), Some("https://q.r/")));
        assert!(parse_multipart(&Bytes::from_static(b"junk"), "XyZ").is_err());
    }

    #[test]
    fn senders() {
        let own = vec!["127.0.0.1:7878".to_string(), "devbox.example-tailnet.ts.net".to_string(), "devbox.example-tailnet.ts.net:443".to_string()];
        let h = |kv: &[(&'static str, &'static str)]| {
            let mut m = HeaderMap::new();
            for (k, v) in kv {
                m.insert(*k, HeaderValue::from_static(v));
            }
            m
        };
        // an iOS Shortcut: Host only
        assert!(check_sender(&h(&[("host", "devbox.example-tailnet.ts.net")]), &own).is_ok());
        // the app itself
        assert!(check_sender(&h(&[("host", "127.0.0.1:7878"), ("origin", "http://127.0.0.1:7878"), ("sec-fetch-site", "same-origin")]), &own).is_ok());
        // a page elsewhere posting a form, rebinding, Funnel
        assert!(check_sender(&h(&[("host", "127.0.0.1:7878"), ("origin", "https://evil.example"), ("sec-fetch-site", "cross-site")]), &own).is_err());
        assert!(check_sender(&h(&[("host", "127.0.0.1:7878"), ("origin", "null")]), &own).is_err());
        assert!(check_sender(&h(&[("host", "127.0.0.1:7878"), ("sec-fetch-site", "cross-site")]), &own).is_err());
        assert!(check_sender(&h(&[("host", "evil.example:7878")]), &own).is_err());
        assert!(check_sender(&h(&[("host", "devbox.example-tailnet.ts.net"), ("tailscale-funnel-request", "1")]), &own).is_err());
    }

    #[test]
    fn upload_name_text() {
        assert!(is_upload_name("Memo.m4a", &[Some("Memo.m4a")], false));
        assert!(is_upload_name("IMG_1.HEIC", &[None], true) && !is_upload_name("IMG_1.HEIC", &[None], false));
        assert!(!is_upload_name("look at this", &[Some("a.jpg")], true));
        assert!(!is_upload_name("Memo.m4a", &[], true));
    }

    #[test]
    fn page_escapes() {
        let p = confirm_page(&Page { ok: true, kicker: "MASTER", head: "Sent <b>", sub: "\"x\"<script>", go: ("Open", "/#/"), again: ("Again", "/#/?a=1&b=2"), serif: false });
        assert!(p.contains("Sent &lt;b&gt;") && p.contains("&quot;x&quot;&lt;script&gt;") && p.contains("/#/?a=1&amp;b=2"));
    }
}
