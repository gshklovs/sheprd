//! Minimal client for herdr's newline-delimited JSON socket API.
//! herdr closes a connection after answering one request, so every call opens
//! a fresh connection (a unix socket connect is ~microseconds). Subscriptions
//! keep their connection open and stream events.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("HERDR_SOCKET_PATH") {
        if !p.is_empty() {
            return p.into();
        }
    }
    crate::home().join(".config/herdr/herdr.sock")
}

#[derive(Debug)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

fn err(code: &str, message: impl ToString) -> HerdrError {
    HerdrError { code: code.into(), message: message.to_string() }
}

/// One request/response round trip. `timeout` bounds the whole exchange.
pub async fn call(method: &str, params: Value, timeout: Duration) -> Result<Value, HerdrError> {
    let fut = async {
        let mut stream = UnixStream::connect(socket_path()).await.map_err(|e| err("unreachable", e))?;
        let id = format!("mm{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let mut req = serde_json::to_vec(&json!({"id": id, "method": method, "params": params})).unwrap();
        req.push(b'\n');
        stream.write_all(&req).await.map_err(|e| err("io", e))?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.map_err(|e| err("io", e))?;
        let mut v: Value = serde_json::from_str(&line).map_err(|e| err("decode", e))?;
        if let Some(e) = v.get("error") {
            return Err(err(
                e.get("code").and_then(Value::as_str).unwrap_or("error"),
                e.get("message").and_then(Value::as_str).unwrap_or(""),
            ));
        }
        Ok(v.get_mut("result").map(Value::take).unwrap_or(Value::Null))
    };
    tokio::time::timeout(timeout, fut).await.unwrap_or_else(|_| Err(err("timeout", method)))
}

pub async fn quick(method: &str, params: Value) -> Result<Value, HerdrError> {
    call(method, params, Duration::from_secs(5)).await
}

/// Opens a subscription and returns a line reader positioned after the
/// `subscription_started` acknowledgement.
pub async fn subscribe(subs: Vec<Value>) -> Result<BufReader<UnixStream>, HerdrError> {
    let mut stream = UnixStream::connect(socket_path()).await.map_err(|e| err("unreachable", e))?;
    let req = json!({"id": "mm-sub", "method": "events.subscribe", "params": {"subscriptions": subs}});
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).await.map_err(|e| err("io", e))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .map_err(|_| err("timeout", "subscribe"))?
        .map_err(|e| err("io", e))?;
    let v: Value = serde_json::from_str(&line).map_err(|e| err("decode", e))?;
    if let Some(e) = v.get("error") {
        return Err(err("subscribe", e));
    }
    Ok(reader)
}
