//! Byte-range file responses for `/api/file` (2026-10-06): iOS Safari plays a `<video>` only from a server that
//! answers `Range: bytes=…` with `206 Partial Content`; a 36 MB mp4 sent as a plain 200 never played or saved on
//! the phone. One range per request (what WebKit sends): `a-b`, `a-` and `-n`; anything unsatisfiable → 416;
//! a malformed or multi-range header → the whole file (RFC 9110 lets a server ignore Range). Every answer says
//! `Accept-Ranges: bytes` and carries a weak ETag (mtime+size) honoured by `If-None-Match` and `If-Range`.
//! The body streams from disk in 64 KiB chunks (seek + take), never the whole file in memory: the service is
//! capped at 200 MB. `download` adds `Content-Disposition: attachment` (the phone's Save fallback).

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub enum Want {
    Full,
    /// inclusive byte offsets
    Part(u64, u64),
    Unsatisfiable,
}

/// Parses a `Range` header value against a file of `len` bytes.
pub fn parse(h: &str, len: u64) -> Want {
    let Some(spec) = h.trim().strip_prefix("bytes=") else { return Want::Full };
    let spec = spec.trim();
    if spec.contains(',') {
        return Want::Full;
    }
    let Some((a, b)) = spec.split_once('-') else { return Want::Full };
    let (a, b) = (a.trim(), b.trim());
    let num = |s: &str| (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())).then(|| s.parse::<u64>().ok()).flatten();
    if a.is_empty() {
        // suffix: the last n bytes
        let Some(n) = num(b) else { return Want::Full };
        if n == 0 || len == 0 {
            return Want::Unsatisfiable;
        }
        return Want::Part(len.saturating_sub(n), len - 1);
    }
    let Some(start) = num(a) else { return Want::Full };
    let end = if b.is_empty() {
        len.saturating_sub(1)
    } else {
        match num(b) {
            Some(e) if e >= start => e.min(len.saturating_sub(1)),
            _ => return Want::Full,
        }
    };
    if start >= len {
        return Want::Unsatisfiable;
    }
    Want::Part(start, end)
}

fn etag(m: &std::fs::Metadata) -> String {
    let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    format!("W/\"{:x}-{:x}\"", m.len(), t)
}

/// `filename="…"` safe for a header: ASCII printable without quotes/backslashes, plus RFC 5987 `filename*`.
fn disposition(name: &str) -> String {
    let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let mut enc = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{enc}")
}

/// Serves `path` (already resolved and allowed by the caller) honouring Range / If-Range / If-None-Match.
pub async fn serve(path: &Path, ct: &'static str, req: &HeaderMap, download: bool) -> Response {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => return (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    };
    let meta = match f.metadata().await {
        Ok(m) => m,
        Err(e) => return (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    };
    let len = meta.len();
    let tag = etag(&meta);
    let hv = |k: header::HeaderName| req.get(k).and_then(|v| v.to_str().ok());
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(v) = HeaderValue::from_str(&tag) {
        headers.insert(header::ETAG, v);
    }
    if download {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
        if let Ok(v) = HeaderValue::from_str(&disposition(&name)) {
            headers.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    if hv(header::IF_NONE_MATCH).is_some_and(|v| v.split(',').any(|t| t.trim() == tag || t.trim() == "*")) {
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    // If-Range with a stale validator → the whole (new) file
    let range_ok = hv(header::IF_RANGE).is_none_or(|v| v.trim() == tag);
    let want = match hv(header::RANGE) {
        Some(r) if range_ok => parse(r, len),
        _ => Want::Full,
    };
    let (status, start, n) = match want {
        Want::Full => (StatusCode::OK, 0, len),
        Want::Part(a, b) => {
            if let Ok(v) = HeaderValue::from_str(&format!("bytes {a}-{b}/{len}")) {
                headers.insert(header::CONTENT_RANGE, v);
            }
            (StatusCode::PARTIAL_CONTENT, a, b - a + 1)
        }
        Want::Unsatisfiable => {
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{len}")) {
                headers.insert(header::CONTENT_RANGE, v);
            }
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(0u64));
            return (StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response();
        }
    };
    if start > 0 && f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(n));
    let reader = f.take(n);
    let stream = futures_util::stream::unfold(reader, |mut r| async move {
        let mut buf = vec![0u8; 64 * 1024];
        match r.read(&mut buf).await {
            Ok(0) => None,
            Ok(k) => {
                buf.truncate(k);
                Some((Ok::<_, std::io::Error>(buf), r))
            }
            Err(e) => Some((Err(e), r)),
        }
    });
    // fused: the gzip layer polls once more after the end, and a bare `unfold` panics then (the response was cut
    // off mid-body: ERR_INCOMPLETE_CHUNKED_ENCODING on any HTML/text file over ~600 KB, 2026-10-09)
    let stream = futures_util::StreamExt::fuse(stream);
    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let l = 1000;
        assert_eq!(parse("bytes=0-99", l), Want::Part(0, 99));
        assert_eq!(parse("bytes=0-0", l), Want::Part(0, 0));
        assert_eq!(parse("bytes=500-", l), Want::Part(500, 999));
        assert_eq!(parse("bytes=-100", l), Want::Part(900, 999));
        assert_eq!(parse("bytes=-5000", l), Want::Part(0, 999));
        assert_eq!(parse("bytes=990-5000", l), Want::Part(990, 999));
        assert_eq!(parse(" bytes= 10 - 20 ", l), Want::Part(10, 20));
        assert_eq!(parse("bytes=1000-", l), Want::Unsatisfiable);
        assert_eq!(parse("bytes=2000-3000", l), Want::Unsatisfiable);
        assert_eq!(parse("bytes=-0", l), Want::Unsatisfiable);
        assert_eq!(parse("bytes=0-", 0), Want::Unsatisfiable);
        // malformed / unsupported → whole file
        assert_eq!(parse("bytes=20-10", l), Want::Full);
        assert_eq!(parse("bytes=0-1,5-9", l), Want::Full);
        assert_eq!(parse("items=0-9", l), Want::Full);
        assert_eq!(parse("bytes=a-9", l), Want::Full);
        assert_eq!(parse("bytes=-", l), Want::Full);
        assert_eq!(parse("bytes=+5-9", l), Want::Full);
    }

    #[test]
    fn disposition_is_header_safe() {
        assert_eq!(disposition("clip.mp4"), "attachment; filename=\"clip.mp4\"; filename*=UTF-8''clip.mp4");
        let d = disposition("my \"vid\"é.mov");
        assert!(d.starts_with("attachment; filename=\"my _vid__.mov\""), "{d}");
        assert!(HeaderValue::from_str(&d).is_ok());
    }
}
