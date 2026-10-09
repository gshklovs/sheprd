//! Web Push (VAPID) to the installed iPhone PWA.

use base64::Engine;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use web_push::{
    ContentEncoding, HyperWebPushClient, PartialVapidSignatureBuilder, SubscriptionInfo, Urgency, VapidSignatureBuilder,
    WebPushClient, WebPushError, WebPushMessageBuilder,
};

pub struct Push {
    pem: Vec<u8>,
    pub public_key: String,
    subs: Mutex<Vec<SubscriptionInfo>>,
    subs_path: PathBuf,
    client: HyperWebPushClient,
    subject: String,
}

fn gen_vapid(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use openssl::{ec::{EcGroup, EcKey}, nid::Nid};
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
    let pem = EcKey::generate(&group)?.private_key_to_pem()?;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?.write_all(&pem)?;
    Ok(())
}

impl Push {
    pub fn load(dir: &std::path::Path, subject: String) -> Option<Push> {
        let pem_path = dir.join("vapid.pem");
        // A fresh VAPID key per install, generated on first start; it never leaves the state dir.
        if !pem_path.exists() {
            if let Err(e) = gen_vapid(&pem_path) {
                eprintln!("push: could not generate a VAPID key ({e}); push disabled");
                return None;
            }
        }
        let pem = std::fs::read(&pem_path).ok()?;
        let partial: PartialVapidSignatureBuilder = VapidSignatureBuilder::from_pem_no_sub(&pem[..])
            .map_err(|e| eprintln!("push: bad VAPID key: {e}"))
            .ok()?;
        let public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(partial.get_public_key());
        let subs_path = dir.join("push-subscriptions.json");
        let subs: Vec<SubscriptionInfo> =
            std::fs::read(&subs_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Some(Push { pem, public_key, subs: Mutex::new(subs), subs_path, client: HyperWebPushClient::new(), subject })
    }

    pub fn subscribe(&self, v: &Value) -> Result<(), String> {
        let endpoint = v.get("endpoint").and_then(Value::as_str).ok_or("missing endpoint")?;
        let keys = v.get("keys").ok_or("missing keys")?;
        let p256dh = keys.get("p256dh").and_then(Value::as_str).ok_or("missing p256dh")?;
        let auth = keys.get("auth").and_then(Value::as_str).ok_or("missing auth")?;
        let info = SubscriptionInfo::new(endpoint, p256dh, auth);
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| s.endpoint != info.endpoint);
        subs.push(info);
        self.save(&subs);
        Ok(())
    }

    fn save(&self, subs: &[SubscriptionInfo]) {
        if let Ok(b) = serde_json::to_vec_pretty(subs) {
            let _ = std::fs::write(&self.subs_path, b);
        }
    }

    pub fn count(&self) -> usize {
        self.subs.lock().unwrap().len()
    }

    pub fn has_subscribers(&self) -> bool {
        !self.subs.lock().unwrap().is_empty()
    }

    /// Sends `payload` (JSON: title, body, url, tag) to every subscription,
    /// dropping subscriptions the push service says are gone.
    pub async fn send(&self, payload: &Value, urgent: bool) {
        let subs = self.subs.lock().unwrap().clone();
        if subs.is_empty() {
            return;
        }
        let body = serde_json::to_vec(&in_app(payload)).unwrap_or_default();
        let mut dead = Vec::new();
        for sub in &subs {
            let msg = (|| {
                let mut sig = VapidSignatureBuilder::from_pem(&self.pem[..], sub)?;
                sig.add_claim("sub", self.subject.as_str());
                let mut b = WebPushMessageBuilder::new(sub);
                b.set_payload(ContentEncoding::Aes128Gcm, &body);
                b.set_vapid_signature(sig.build()?);
                b.set_ttl(3600);
                if urgent {
                    b.set_urgency(Urgency::High);
                }
                b.build()
            })();
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("push: build failed: {e}");
                    continue;
                }
            };
            match tokio::time::timeout(std::time::Duration::from_secs(15), self.client.send(msg)).await {
                Ok(Ok(())) => {}
                Ok(Err(WebPushError::EndpointNotValid(_))) | Ok(Err(WebPushError::EndpointNotFound(_))) => {
                    dead.push(sub.endpoint.clone())
                }
                Ok(Err(e)) => eprintln!("push: send failed: {e}"),
                Err(_) => eprintln!("push: send timed out"),
            }
        }
        if !dead.is_empty() {
            let mut subs = self.subs.lock().unwrap();
            subs.retain(|s| !dead.contains(&s.endpoint));
            self.save(&subs);
        }
    }
}

/// Routine pushes (done notes, info/done cards, schedules, CI, services, machine alerts) are thinned out so a push
/// mostly means "you're needed": none while the phone was in use in the
/// last 2 min, at most one per agent (tag) per 10 min. The rest wait and go out as one digest when the window ends,
/// unless the phone was opened in the meantime. Urgent pushes (asks, permissions) skip this.
pub const QUIET_MS: i64 = 2 * 60_000;
pub const WINDOW_MS: i64 = 10 * 60_000;

#[derive(Default)]
pub struct Gate {
    buckets: Mutex<HashMap<String, Bucket>>,
}

#[derive(Default)]
struct Bucket {
    last: i64,
    held: Vec<(i64, Value)>,
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Now,
    /// The phone was in use moments ago: it already shows this.
    Quiet,
    /// Waits for the digest at the end of the agent's window.
    Held,
}

impl Gate {
    pub fn routine(&self, p: &Value, now: i64, last_visible: i64) -> Verdict {
        if now - last_visible < QUIET_MS {
            return Verdict::Quiet;
        }
        let mut buckets = self.buckets.lock().unwrap();
        let b = buckets.entry(gate_key(p)).or_default();
        if b.held.is_empty() && now - b.last >= WINDOW_MS {
            b.last = now;
            return Verdict::Now;
        }
        b.held.push((now, p.clone()));
        Verdict::Held
    }

    /// The digests whose window has ended, minus anything the phone was opened after.
    pub fn due(&self, now: i64, last_visible: i64) -> Vec<Value> {
        let mut out = Vec::new();
        for b in self.buckets.lock().unwrap().values_mut() {
            if b.held.is_empty() || now - b.last < WINDOW_MS {
                continue;
            }
            let held: Vec<Value> = b.held.drain(..).filter(|(t, _)| *t > last_visible).map(|(_, p)| p).collect();
            if let Some(d) = digest(held) {
                b.last = now;
                out.push(d);
            }
        }
        out
    }
}

fn gate_key(p: &Value) -> String {
    p["tag"].as_str().filter(|t| !t.is_empty()).unwrap_or("misc").to_string()
}

/// One push for several held ones: the newest one's link, the others' titles in the body.
fn digest(mut held: Vec<Value>) -> Option<Value> {
    let last = held.pop()?;
    if held.is_empty() {
        return Some(last);
    }
    let mut p = last.clone();
    let title = last["title"].as_str().unwrap_or("Update");
    p["title"] = Value::from(format!("{title} (+{} more)", held.len()));
    let mut lines: Vec<String> = held.iter().filter_map(|h| h["title"].as_str().map(str::to_string)).collect();
    lines.dedup();
    if let Some(b) = last["body"].as_str() {
        lines.push(b.to_string());
    }
    let body = lines.join("\n");
    p["body"] = Value::from(body.chars().take(300).collect::<String>());
    Some(p)
}

/// Tapping a notification always opens the app (the sending agent's chat, a card, …), never an external link:
/// a `url` that is not one of the app's own `/#/…` routes becomes `/#/`.
fn in_app(payload: &Value) -> Value {
    let mut p = payload.clone();
    let ok = p["url"].as_str().is_some_and(|u| u.starts_with("/#/"));
    if !ok {
        p["url"] = Value::from("/#/");
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    fn push(tag: &str, title: &str) -> Value {
        serde_json::json!({"title": title, "body": "b", "url": "/#/", "tag": tag})
    }

    #[test]
    fn routine_pushes_wait_for_the_window() {
        let g = Gate::default();
        let t0 = 1_000_000_000;
        // phone in use a minute ago: nothing
        assert_eq!(g.routine(&push("a", "Done · a"), t0, t0 - 60_000), Verdict::Quiet);
        // first one goes, the next two wait; another agent has its own window
        assert_eq!(g.routine(&push("a", "Done · a"), t0, 0), Verdict::Now);
        assert_eq!(g.routine(&push("a", "Done · a2"), t0 + 1000, 0), Verdict::Held);
        assert_eq!(g.routine(&push("a", "Done · a3"), t0 + 2000, 0), Verdict::Held);
        assert_eq!(g.routine(&push("b", "Done · b"), t0 + 2000, 0), Verdict::Now);
        assert!(g.due(t0 + 5 * 60_000, 0).is_empty());
        let d = g.due(t0 + WINDOW_MS, 0);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0]["title"], "Done · a3 (+1 more)");
        assert!(d[0]["body"].as_str().unwrap().starts_with("Done · a2"));
        // the digest started a new window
        assert_eq!(g.routine(&push("a", "Done · a4"), t0 + WINDOW_MS + 1000, 0), Verdict::Held);
    }

    #[test]
    fn opening_the_phone_drops_held_pushes() {
        let g = Gate::default();
        let t0 = 1_000_000_000;
        g.routine(&push("", "x"), t0, 0);
        assert_eq!(g.routine(&push("", "y"), t0 + 1000, 0), Verdict::Held);
        assert!(g.due(t0 + WINDOW_MS, t0 + 2000).is_empty());
    }

    #[test]
    fn urls_stay_in_the_app() {
        let u = |x: Value| in_app(&serde_json::json!({"title": "t", "url": x}))["url"].clone();
        assert_eq!(u("/#/a/w6:pE?i=403".into()), "/#/a/w6:pE?i=403");
        assert_eq!(u("https://example.com/x".into()), "/#/");
        assert_eq!(u("//evil.example/#/".into()), "/#/");
        assert_eq!(u(Value::Null), "/#/");
    }
}
