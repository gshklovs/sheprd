//! Decisions (API.md "Decisions"): a surface card that asks the user to choose. An agent posts `POST /api/surface` with
//! `decision: {kind, options, key?, deadline?, allowNote?}` (or the mod's `decide` tool); the phone's Decide deck
//! (`#/decide`, web/decide.js) shows open ones one at a time and `POST /api/decide/<id>` sends the pick back to the
//! asking agent as `[Decision: <title>] The user picked: …` through the steer path (held while the user has a draft or a
//! menu is open there; an asleep agent is resumed with it). The decision lives inside its surface (`surfaces.json`):
//! `state` = open | answered | later | stale | withdrawn, `reason` for stale ones.
//! Stale: a newer card with the same `key` from the same agent supersedes an open one; the agent withdraws it; its
//! `deadline` passes; or the user wrote in that agent's chat after the card ("looks answered", a heuristic: any message
//! of his newer than the card that isn't another decision's answer). Superseded / withdrawn ones close themselves;
//! the others wait in the deck's Stale section for Clear (closed, kept as stale) or Keep (open again).
//! Legacy cards: an undismissed `level: attention` surface with reply chips and no `decision` counts as a pick
//! between its chips.

use crate::transcript::{cut, now_ms};
use crate::{App, Inner, Shared, json_err};
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::Duration;

const KINDS: [&str; 4] = ["pick", "rank", "multi", "yesno"];
const MAX_OPTIONS: usize = 8;
const LATER_MS: i64 = 3 * 3600_000;
const DONE_SHOWN: usize = 30;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/decisions", get(api_list))
        .route("/api/decide/{id}", post(api_decide))
        .route("/api/decide/{id}/later", post(api_later))
        .route("/api/decide/{id}/clear", post(api_clear))
        .route("/api/decide/{id}/keep", post(api_keep))
        .route("/api/decide/{id}/withdraw", post(api_withdraw))
        .route("/api/decide/{id}/stale", post(api_stale))
        .route("/api/decide/{id}/attach", post(api_attach))
}

// ---------------------------------------------------------------------------
// Shape

/// An option's images: the card's own `/api/file?path=` form, a kept copy made of each (keep.rs), like `images`.
fn image_urls(v: &Value) -> Vec<String> {
    let raw: Vec<String> = match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => vec![],
    };
    let raw: Vec<String> = raw.into_iter().filter(|s| !s.trim().is_empty()).take(6).collect();
    let keep = raw.clone();
    std::thread::spawn(move || {
        for r in keep {
            if let Ok(p) = std::fs::canonicalize(&r) {
                if p.starts_with(crate::home()) || p.starts_with("/tmp") {
                    crate::keep::stash(&r, &p);
                }
            }
        }
    });
    raw.iter()
        .map(|p| if p.starts_with("/api/") || p.starts_with("http") { p.clone() } else { format!("/api/file?path={}", crate::urlencode(p)) })
        .collect()
}

fn short(v: &Value, n: usize) -> String {
    cut(v.as_str().unwrap_or("").trim(), n).0
}

/// Validates and normalises a posted `decision` → the stored object (state open). `replies` stand in for options.
pub fn parse(d: &Value, replies: &[String]) -> Result<Value, String> {
    let mut kind = d["kind"].as_str().unwrap_or("pick").to_string();
    if !KINDS.contains(&kind.as_str()) {
        return Err(format!("decision.kind must be one of {}", KINDS.join(", ")));
    }
    let mut opts: Vec<Value> = Vec::new();
    let src: Vec<Value> = match d["options"].as_array() {
        Some(a) if !a.is_empty() => a.clone(),
        _ if kind == "yesno" => vec![json!("Yes"), json!("No")],
        _ if !replies.is_empty() => replies.iter().map(|r| json!(r)).collect(),
        _ => return Err("decision.options required (1–8 of {id, label, sub?, images?})".into()),
    };
    if src.len() > MAX_OPTIONS {
        return Err(format!("at most {MAX_OPTIONS} options"));
    }
    for (k, o) in src.iter().enumerate() {
        let (label, id, sub, images) = match o {
            Value::String(s) => (cut(s.trim(), 60).0, String::new(), String::new(), vec![]),
            _ => (short(&o["label"], 60), short(&o["id"], 24), short(&o["sub"], 140), image_urls(o.get("images").unwrap_or(&o["image"]))),
        };
        if label.is_empty() {
            return Err(format!("option {} has no label", k + 1));
        }
        let id = if id.is_empty() { ((b'a' + k as u8) as char).to_string() } else { id };
        if opts.iter().any(|x| x["id"] == id.as_str()) {
            return Err(format!("duplicate option id {id}"));
        }
        opts.push(json!({"id": id, "label": label, "sub": sub, "images": images}));
    }
    if opts.len() < 2 && kind != "multi" {
        kind = "multi".into(); // one option: a tick, not a choice
    }
    let deadline = d["deadline"].as_i64().or_else(|| d["deadlineMin"].as_i64().map(|m| now_ms() + m * 60_000));
    Ok(json!({
        "kind": kind, "options": opts,
        "key": d["key"].as_str().map(|k| cut(k.trim(), 80).0).filter(|k| !k.is_empty()),
        "deadline": deadline, "allowNote": d["allowNote"].as_bool().unwrap_or(true),
        "state": "open", "reason": null, "answer": null, "until": null, "keepTs": null,
    }))
}

/// Who posted a card: its session, else its pane.
fn agent_of(s: &Value) -> Option<&str> {
    s["session"].as_str().or(s["pane"].as_str()).filter(|a| !a.is_empty())
}

/// A newer card with the same `key` from the same agent closes the older open ones (stale, superseded).
pub fn supersede(list: &mut VecDeque<Value>, item: &Value) -> usize {
    let Some(key) = item["decision"]["key"].as_str() else { return 0 };
    let who = agent_of(item).map(str::to_string);
    let mut n = 0;
    for s in list.iter_mut() {
        if s["decision"]["key"].as_str() == Some(key) && agent_of(s).map(str::to_string) == who && open_like(&s["decision"]) {
            s["decision"]["state"] = json!("stale");
            s["decision"]["reason"] = json!("superseded by a newer card");
            s["decision"]["closed"] = json!(now_ms());
            s["dismissed"] = json!(true);
            n += 1;
        }
    }
    n
}

fn open_like(d: &Value) -> bool {
    matches!(d["state"].as_str(), Some("open" | "later")) || (d["state"] == "stale" && d["closed"].is_null())
}

/// The decision a card carries: its own, or the legacy reading of an attention card with reply chips.
pub fn decision_of(s: &Value) -> Option<Value> {
    if s["decision"].is_object() {
        return Some(s["decision"].clone());
    }
    let replies: Vec<String> = s["replies"].as_array()?.iter().filter_map(Value::as_str).map(str::to_string).collect();
    if s["dismissed"].as_bool().unwrap_or(false) || s["level"] != "attention" || replies.is_empty() || s["blocker"].is_object() {
        return None;
    }
    let mut d = parse(&json!({"kind": "pick"}), &replies).ok()?;
    d["legacy"] = json!(true);
    Some(d)
}

/// Where a decision stands now (later ones come back after `until`).
fn bucket(d: &Value, now: i64) -> &'static str {
    match d["state"].as_str() {
        Some("open") => "open",
        Some("later") if d["until"].as_i64().is_some_and(|u| u <= now) => "open",
        Some("later") => "later",
        Some("stale") if d["closed"].is_null() => "stale",
        _ => "done",
    }
}

fn row(s: &Value, d: Value) -> Value {
    json!({
        "id": s["id"], "ts": s["ts"], "pane": s["pane"], "session": s["session"], "agentTitle": s["agentTitle"],
        "title": s["title"], "body": s["body"], "images": s["images"], "links": s["links"], "decision": d,
    })
}

/// `{open, later, stale, done}` rows: open by deadline, then oldest first; stale and done newest first.
pub fn list(list: &VecDeque<Value>, now: i64) -> Value {
    let (mut open, mut later, mut stale, mut done) = (vec![], vec![], vec![], vec![]);
    for s in list {
        let Some(d) = decision_of(s) else { continue };
        let b = bucket(&d, now);
        let r = row(s, d);
        match b {
            "open" => open.push(r),
            "later" => later.push(r),
            "stale" => stale.push(r),
            _ => done.push(r),
        }
    }
    let ts = |r: &Value| r["ts"].as_i64().unwrap_or(0);
    open.sort_by_key(|r| (r["decision"]["deadline"].as_i64().unwrap_or(i64::MAX), ts(r)));
    later.sort_by_key(|r| r["decision"]["until"].as_i64().unwrap_or(0));
    done.truncate(DONE_SHOWN);
    json!({"open": open, "later": later, "stale": stale, "done": done})
}

/// For the state (Home's Decide card): counts, the next one's title and agent, up to 3 thumbnails.
pub fn summary(list: &VecDeque<Value>) -> Value {
    let all = self::list(list, now_ms());
    let open = all["open"].as_array().cloned().unwrap_or_default();
    let pics = open.iter().filter(|r| has_pictures(r)).count();
    let thumbs: Vec<Value> = open
        .iter()
        .flat_map(|r| {
            let mut v: Vec<Value> = r["decision"]["options"].as_array().into_iter().flatten().filter_map(|o| o["images"].get(0).cloned()).collect();
            if v.is_empty() {
                v = r["images"].as_array().cloned().unwrap_or_default();
            }
            v
        })
        .take(3)
        .collect();
    let oldest = open.iter().filter_map(|r| r["ts"].as_i64()).min();
    json!({
        "open": open.len(), "pictures": pics, "stale": all["stale"].as_array().map_or(0, Vec::len),
        "later": all["later"].as_array().map_or(0, Vec::len), "oldest": oldest, "thumbs": thumbs,
        "next": open.first().map(|r| json!({"id": r["id"], "title": r["title"], "pane": r["pane"], "agentTitle": r["agentTitle"]})),
    })
}

fn has_pictures(r: &Value) -> bool {
    r["decision"]["options"].as_array().is_some_and(|o| o.iter().any(|x| x["images"].as_array().is_some_and(|i| !i.is_empty())))
        || r["images"].as_array().is_some_and(|i| !i.is_empty())
}

/// The message the agent gets.
pub fn answer_text(title: &str, d: &Value, choice: &[String], note: &str) -> String {
    let label = |id: &str| {
        d["options"].as_array().and_then(|o| o.iter().find(|x| x["id"] == id)).and_then(|x| x["label"].as_str()).unwrap_or(id).to_string()
    };
    let named: Vec<String> = choice.iter().map(|c| format!("{} [{c}]", label(c))).collect();
    let what = if choice.is_empty() {
        "The user picked none of these".to_string()
    } else {
        match d["kind"].as_str() {
            Some("rank") if choice.len() > 1 => {
                format!("The user ranked: {}", named.iter().enumerate().map(|(k, n)| format!("{}. {n}", k + 1)).collect::<Vec<_>>().join(", "))
            }
            _ => format!("The user picked: {}", named.join(", ")),
        }
    };
    let mut t = format!("[Decision: {}] {what}", cut(title.trim(), 120).0);
    if !note.trim().is_empty() {
        t.push_str(&format!("\nTheir note: {}", note.trim()));
    }
    t
}

// ---------------------------------------------------------------------------
// Routes

async fn api_list(State(app): State<Shared>) -> Response {
    let inner = app.inner.lock().unwrap();
    Json(list(&inner.surfaces, now_ms())).into_response()
}

/// Edits the decision of card `id` (a legacy card gets its decision written first). None: no such decision.
fn edit(app: &App, id: &str, f: impl FnOnce(&mut Value, &mut Value)) -> Option<Value> {
    let out = {
        let mut inner = app.inner.lock().unwrap();
        let Inner { surfaces, .. } = &mut *inner;
        let s = surfaces.iter_mut().find(|s| s["id"].as_str() == Some(id))?;
        let mut d = decision_of(s)?;
        let mut card = s.clone();
        f(&mut card, &mut d);
        card["decision"] = d;
        *s = card.clone();
        crate::save_surfaces(surfaces);
        card
    };
    crate::republish(app);
    Some(out)
}

fn not_found() -> Response {
    json_err(StatusCode::NOT_FOUND, "no such decision")
}

/// `{choice: [option ids] (in order for rank), note?}`
async fn api_decide(State(app): State<Shared>, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    let card = {
        let inner = app.inner.lock().unwrap();
        inner.surfaces.iter().find(|s| s["id"].as_str() == Some(id.as_str())).cloned()
    };
    let Some(card) = card else { return not_found() };
    let Some(d) = decision_of(&card) else { return not_found() };
    if matches!(d["state"].as_str(), Some("answered" | "withdrawn")) || !d["closed"].is_null() {
        return json_err(StatusCode::CONFLICT, format!("already {}", d["state"].as_str().unwrap_or("closed")));
    }
    let choice: Vec<String> = b["choice"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let known = |c: &String| d["options"].as_array().is_some_and(|o| o.iter().any(|x| x["id"] == c.as_str()));
    if let Some(bad) = choice.iter().find(|c| !known(c)) {
        return json_err(StatusCode::BAD_REQUEST, format!("unknown option {bad}"));
    }
    if choice.len() > 1 && matches!(d["kind"].as_str(), Some("pick" | "yesno")) {
        return json_err(StatusCode::BAD_REQUEST, "this decision takes one option");
    }
    let note = cut(b["note"].as_str().unwrap_or("").trim(), 2000).0;
    if choice.is_empty() && note.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "pick an option or write a note");
    }
    let text = answer_text(card["title"].as_str().unwrap_or(""), &d, &choice, &note);
    let (via, held) = match deliver(&app, &card, text.clone()).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    let ans = json!({"choice": choice, "note": note, "ts": now_ms(), "via": via, "held": held, "text": text});
    let out = edit(&app, &id, |c, d| {
        d["state"] = json!("answered");
        d["answer"] = ans.clone();
        d["closed"] = json!(now_ms());
        c["dismissed"] = json!(true);
    });
    Json(json!({"ok": true, "via": via, "held": held, "surface": out})).into_response()
}

/// Steer to the live pane (held on a draft / menu), else resume the asleep session with it.
async fn deliver(app: &Shared, card: &Value, text: String) -> Result<(String, bool), Response> {
    let live = {
        let inner = app.inner.lock().unwrap();
        // a live agent pane (herdr reports it), else the pane its session runs in now
        let pane = card["pane"].as_str().filter(|p| inner.status.contains_key(*p)).map(str::to_string);
        pane.or_else(|| {
            let s = card["session"].as_str()?;
            inner.pane_sessions.iter().find(|(_, v)| v.as_str() == s).map(|(p, _)| p.clone())
        })
    };
    let (r, how) = if let Some(pane) = live {
        (crate::modbridge::steer(app.clone(), pane, text).await, "steer")
    } else if let Some(sid) = card["session"].as_str().filter(|s| crate::valid_sid(s)) {
        (crate::fleet::resume_with(app.clone(), sid.to_string(), text).await, "resume")
    } else {
        return Err(json_err(StatusCode::CONFLICT, "the agent that asked is gone"));
    };
    let ok = r.status().is_success();
    let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap_or_default();
    let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    if !ok {
        let msg = v["error"].as_str().unwrap_or("could not reach the agent").to_string();
        return Err(json_err(StatusCode::BAD_GATEWAY, msg));
    }
    let held = v["via"] == "held";
    Ok((if held { format!("held:{}", v["how"].as_str().unwrap_or("draft")) } else { how.to_string() }, held))
}

/// `{hours?}` (default 3): out of the deck until then.
async fn api_later(State(app): State<Shared>, AxPath(id): AxPath<String>, body: Option<Json<Value>>) -> Response {
    let h = body.and_then(|Json(b)| b["hours"].as_f64()).unwrap_or(LATER_MS as f64 / 3600e3).clamp(0.05, 24.0 * 14.0);
    let until = now_ms() + (h * 3600e3) as i64;
    match edit(&app, &id, |_, d| {
        d["state"] = json!("later");
        d["until"] = json!(until);
    }) {
        Some(c) => Json(json!({"ok": true, "surface": c})).into_response(),
        None => not_found(),
    }
}

/// Close it without answering (a stale one, or anything the user wants gone): kept as stale, the card dismissed.
async fn api_clear(State(app): State<Shared>, AxPath(id): AxPath<String>) -> Response {
    let r = edit(&app, &id, |c, d| {
        if d["state"] != "stale" {
            d["state"] = json!("stale");
            d["reason"] = json!("cleared by the user");
        }
        d["closed"] = json!(now_ms());
        c["dismissed"] = json!(true);
    });
    match r {
        Some(c) => Json(json!({"ok": true, "surface": c})).into_response(),
        None => not_found(),
    }
}

/// A stale one is still open after all: back in the deck; the user's messages before now don't count against it.
async fn api_keep(State(app): State<Shared>, AxPath(id): AxPath<String>) -> Response {
    match edit(&app, &id, |c, d| {
        d["state"] = json!("open");
        d["reason"] = Value::Null;
        d["closed"] = Value::Null;
        d["keepTs"] = json!(now_ms());
        c["dismissed"] = json!(false);
    }) {
        Some(c) => Json(json!({"ok": true, "surface": c})).into_response(),
        None => not_found(),
    }
}

/// The agent takes its question back (`{reason?}`).
async fn api_withdraw(State(app): State<Shared>, AxPath(id): AxPath<String>, body: Option<Json<Value>>) -> Response {
    let reason = body.map(|Json(b)| short(&b["reason"], 200)).unwrap_or_default();
    match edit(&app, &id, |c, d| {
        d["state"] = json!("withdrawn");
        d["reason"] = json!(if reason.is_empty() { "withdrawn by the agent".to_string() } else { reason.clone() });
        d["closed"] = json!(now_ms());
        c["dismissed"] = json!(true);
    }) {
        Some(c) => Json(json!({"ok": true, "surface": c})).into_response(),
        None => not_found(),
    }
}

/// Mark stale by hand: `{reason, clear?}` (clear: closed at once, as Clear would).
async fn api_stale(State(app): State<Shared>, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    let reason = short(&b["reason"], 200);
    let clear = b["clear"].as_bool().unwrap_or(false);
    match edit(&app, &id, |c, d| {
        d["state"] = json!("stale");
        d["reason"] = json!(if reason.is_empty() { "out of date".to_string() } else { reason.clone() });
        if clear {
            d["closed"] = json!(now_ms());
            c["dismissed"] = json!(true);
        }
    }) {
        Some(c) => Json(json!({"ok": true, "surface": c})).into_response(),
        None => not_found(),
    }
}

/// Turn an existing card into a decision (body = the `decision` object).
async fn api_attach(State(app): State<Shared>, AxPath(id): AxPath<String>, Json(b): Json<Value>) -> Response {
    let res = {
        let mut inner = app.inner.lock().unwrap();
        let Inner { surfaces, .. } = &mut *inner;
        match surfaces.iter_mut().find(|s| s["id"].as_str() == Some(id.as_str())) {
            None => Err(not_found()),
            Some(s) => {
                let replies: Vec<String> = s["replies"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
                match parse(&b, &replies) {
                    Err(e) => Err(json_err(StatusCode::BAD_REQUEST, e)),
                    Ok(d) => {
                        s["decision"] = d;
                        add_link(s);
                        let out = s.clone();
                        crate::save_surfaces(surfaces);
                        Ok(out)
                    }
                }
            }
        }
    };
    match res {
        Ok(c) => {
            crate::republish(&app);
            Json(json!({"ok": true, "surface": c})).into_response()
        }
        Err(r) => r,
    }
}

/// The card's "Decide" link (its push opens the deck at it: an in-app link is the push URL).
pub fn add_link(s: &mut Value) {
    let url = format!("#/decide/{}", s["id"].as_str().unwrap_or(""));
    let links = s["links"].as_array().cloned().unwrap_or_default();
    if links.iter().any(|l| l["url"] == url.as_str()) {
        return;
    }
    let mut v = vec![json!({"url": url, "label": "Decide"})];
    v.extend(links);
    s["links"] = json!(v);
}

// ---------------------------------------------------------------------------
// Staleness watcher

/// the user's own message in that chat after the card (or after Keep): not an agent's, not a server note, not another
/// decision's answer. → its first words.
pub fn wrote_after(items: &[crate::transcript::Item], since: i64) -> Option<String> {
    items.iter().rev().take_while(|it| it.ts > since).filter(|it| it.k == "user" && it.from.is_none() && it.agent_id.is_none()).find_map(|it| {
        let t = it.text.as_deref()?.trim();
        (!t.is_empty() && !t.starts_with('[') && !t.starts_with('/')).then(|| cut(t, 60).0)
    })
}

pub async fn run(app: Shared) {
    tokio::time::sleep(Duration::from_secs(20)).await;
    loop {
        sweep(&app).await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn sweep(app: &Shared) {
    let now = now_ms();
    // (id, since, tid) of open ones; deadlines handled under the lock
    let mut changed = false;
    let todo: Vec<(String, i64, String)> = {
        let mut inner = app.inner.lock().unwrap();
        let mut out = vec![];
        for s in inner.surfaces.iter_mut() {
            let Some(d) = s.get("decision").filter(|d| d.is_object()) else {
                // a legacy card (attention + reply chips) goes stale the same way
                if let (Some(_), Some(tid)) = (decision_of(s), s["tid"].as_str()) {
                    out.push((s["id"].as_str().unwrap_or("").to_string(), s["ts"].as_i64().unwrap_or(now), tid.to_string()));
                }
                continue;
            };
            if bucket(d, now) != "open" && bucket(d, now) != "later" {
                continue;
            }
            if d["deadline"].as_i64().is_some_and(|t| t < now) {
                s["decision"]["state"] = json!("stale");
                s["decision"]["reason"] = json!("its deadline passed");
                changed = true;
                continue;
            }
            if let Some(tid) = s["tid"].as_str() {
                let since = d["keepTs"].as_i64().unwrap_or(0).max(s["ts"].as_i64().unwrap_or(now));
                out.push((s["id"].as_str().unwrap_or("").to_string(), since, tid.to_string()));
            }
        }
        if changed {
            crate::save_surfaces(&inner.surfaces);
        }
        out
    };
    for (id, since, tid) in todo {
        let Some(t) = crate::transcript(app, &tid) else { continue };
        let hit = tokio::task::spawn_blocking(move || {
            let mut t = t.lock().unwrap();
            t.refresh();
            wrote_after(&t.items, since)
        })
        .await
        .ok()
        .flatten();
        if let Some(words) = hit {
            edit(app, &id, |_, d| {
                d["state"] = json!("stale");
                d["reason"] = json!(format!("you wrote in its chat after this: “{words}”"));
            });
            changed = true;
        }
    }
    if changed {
        crate::republish(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, d: Value) -> Value {
        json!({"id": id, "ts": 1000, "session": "s1", "pane": "w1:p1", "title": "T", "level": "attention", "replies": [], "decision": d})
    }

    #[test]
    fn parse_defaults_and_errors() {
        let d = parse(&json!({"kind": "yesno"}), &[]).unwrap();
        assert_eq!(d["options"][0]["label"], "Yes");
        assert_eq!(d["options"][1]["id"], "b");
        assert!(parse(&json!({"kind": "vote"}), &[]).is_err());
        assert!(parse(&json!({"kind": "pick"}), &[]).is_err());
        let d = parse(&json!({"kind": "rank", "options": [{"id": "x", "label": "One"}, "Two"]}), &[]).unwrap();
        assert_eq!(d["options"][1]["id"], "b");
        assert!(parse(&json!({"options": [{"id": "x", "label": "A"}, {"id": "x", "label": "B"}]}), &[]).is_err());
        assert_eq!(parse(&json!({}), &["ok".into(), "later".into()]).unwrap()["options"][1]["label"], "later");
    }

    #[test]
    fn answer_text_reads_well() {
        let d = parse(&json!({"kind": "rank", "options": ["Find", "Recent", "Jump"]}), &[]).unwrap();
        assert_eq!(answer_text("Pick", &d, &["c".into(), "a".into()], ""), "[Decision: Pick] The user ranked: 1. Jump [c], 2. Find [a]");
        assert_eq!(answer_text("Pick", &d, &["b".into()], "grid"), "[Decision: Pick] The user picked: Recent [b]\nTheir note: grid");
        assert_eq!(answer_text("Pick", &d, &[], "none"), "[Decision: Pick] The user picked none of these\nTheir note: none");
    }

    #[test]
    fn supersede_same_key_same_agent() {
        let d = parse(&json!({"kind": "yesno", "key": "k"}), &[]).unwrap();
        let mut l: VecDeque<Value> = vec![card("s1", d.clone()), card("s0", parse(&json!({"kind": "yesno", "key": "other"}), &[]).unwrap())].into();
        let mut other = card("s9", d.clone());
        other["session"] = json!("s2");
        l.push_back(other);
        assert_eq!(supersede(&mut l, &card("s3", d)), 1);
        assert_eq!(l[0]["decision"]["state"], "stale");
        assert_eq!(l[1]["decision"]["state"], "open");
        assert_eq!(l[2]["decision"]["state"], "open");
    }

    #[test]
    fn buckets_and_legacy() {
        let mut l: VecDeque<Value> = VecDeque::new();
        let mut later = card("s2", parse(&json!({"kind": "yesno"}), &[]).unwrap());
        later["decision"]["state"] = json!("later");
        later["decision"]["until"] = json!(i64::MAX);
        l.push_back(later);
        l.push_back(json!({"id": "s3", "ts": 5, "level": "attention", "replies": ["A", "B"], "title": "Legacy"}));
        l.push_back(json!({"id": "s4", "ts": 5, "level": "info", "replies": ["A"], "title": "Not one"}));
        let mut stale = card("s5", parse(&json!({"kind": "yesno"}), &[]).unwrap());
        stale["decision"]["state"] = json!("stale");
        l.push_back(stale);
        let v = list(&l, 10);
        assert_eq!(v["open"].as_array().unwrap().len(), 1);
        assert_eq!(v["open"][0]["decision"]["legacy"], true);
        assert_eq!(v["later"][0]["id"], "s2");
        assert_eq!(v["stale"][0]["id"], "s5");
        assert_eq!(summary(&l)["open"], 1);
    }

    #[test]
    fn wrote_after_skips_agents_and_answers() {
        use crate::transcript::Item;
        let mk = |ts: i64, text: &str, from: Option<&str>| {
            Item { ts, k: "user", text: Some(text.into()), from: from.map(str::to_string), ..Default::default() }
        };
        let items = vec![mk(5, "old", None), mk(20, "[Decision: X] The user picked: A [a]", None), mk(30, "hi there", Some("docs"))];
        assert_eq!(wrote_after(&items, 10), None);
        let mut items = items;
        items.push(mk(40, "3, 1, 2 grid", None));
        assert_eq!(wrote_after(&items, 10).as_deref(), Some("3, 1, 2 grid"));
    }
}
