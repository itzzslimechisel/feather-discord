//! feather-mock: a local server that speaks enough of Discord's Gateway (v10, JSON, zlib-stream)
//! and REST API for Feather's integration tests and benchmarks. It never talks to Discord.
//!
//! Usage: feather-mock [--port 7777] [--guilds 3] [--channels 8] [--history 500]
//!                     [--image-history 0] [--chatter 0] [--chatter-channels 1] [--heartbeat-ms 41250]
//!
//! Control endpoints (POST, no auth): /control/drop, /control/reconnect, /control/invalid-session,
//! /control/ratelimit?n=3, /control/emit?channel=ID&content=TEXT, /control/stats

use axum::extract::ws::{Message as Ws, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use flate2::{Compress, Compression, FlushCompress};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

const EPOCH: u64 = 1_420_070_400_000;
pub const TOKEN: &str = "MTAwMA.mock.feather-test-token-not-a-secret";
const BOT_ID: u64 = 1000;

#[derive(Clone, Debug)]
struct Opts {
    port: u16,
    guilds: u64,
    channels: u64,
    history: u64,
    image_history: u64,
    chatter: f64,
    chatter_channels: u64,
    heartbeat_ms: u64,
}

impl Opts {
    fn parse() -> Opts {
        let mut o = Opts { port: 7777, guilds: 3, channels: 8, history: 500, image_history: 0, chatter: 0.0, chatter_channels: 1, heartbeat_ms: 41250 };
        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i + 1 < args.len() {
            let v = &args[i + 1];
            match args[i].as_str() {
                "--port" => o.port = v.parse().unwrap(),
                "--guilds" => o.guilds = v.parse().unwrap(),
                "--channels" => o.channels = v.parse().unwrap(),
                "--history" => o.history = v.parse().unwrap(),
                "--image-history" => o.image_history = v.parse().unwrap(),
                "--chatter" => o.chatter = v.parse().unwrap(),
                "--chatter-channels" => o.chatter_channels = v.parse().unwrap(),
                "--heartbeat-ms" => o.heartbeat_ms = v.parse().unwrap(),
                a => panic!("unknown argument {a}"),
            }
            i += 2;
        }
        o
    }
}

#[derive(Clone, Debug)]
enum Ctl {
    Dispatch,
    Drop,
    Reconnect,
    InvalidSession,
}

#[derive(Default, Clone)]
struct SessBuf {
    seq: u64,
    buf: VecDeque<(u64, Arc<(String, Value)>)>,
}

struct Mock {
    opts: Opts,
    next_id: AtomicU64,
    /// channel id -> messages oldest first (generated lazily).
    messages: Mutex<HashMap<u64, Vec<Value>>>,
    bus: broadcast::Sender<Ctl>,
    /// Resume buffers by session id: (seq, event).
    sessions: Mutex<HashMap<String, SessBuf>>,
    ratelimit_next: AtomicU64,
    stats: Mutex<HashMap<String, u64>>,
    pngs: Mutex<HashMap<(u32, u32, u8), Arc<Vec<u8>>>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn snow(ms: u64, inc: u64) -> u64 {
    ((ms - EPOCH) << 22) | (inc & 0x3fffff)
}

fn ts(id: u64) -> String {
    let ms = (id >> 22) + EPOCH;
    let secs = ms / 1000;
    let (d, s) = (secs / 86400, secs % 86400);
    // civil-from-days (H. Hinnant)
    let z = d as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}+00:00", s / 3600, s % 3600 / 60, s % 60, ms % 1000)
}

fn guild_id(g: u64) -> u64 {
    2_000_000 + g * 1000
}
fn channel_id(g: u64, c: u64) -> u64 {
    guild_id(g) + 1 + c
}

const WORDS: &[&str] = &[
    "the", "build", "is", "green", "again", "**nice**", "who", "broke", "`main`", "lol", "can", "you", "review", "my", "PR",
    "https://example.com/docs", "_really_", "fast", "startup", "memory", "~~old~~", "new", "ship", "it", "tomorrow", "||spoiler||",
    "coffee", "deploy", "friday", "😀", "rust", "async", "gateway", "theme", "dark", "mode",
];

fn user(n: u64) -> Value {
    if n == 0 {
        return json!({ "id": BOT_ID.to_string(), "username": "featherbot", "global_name": "Feather Bot", "avatar": null, "bot": true });
    }
    json!({ "id": (5000 + n).to_string(), "username": format!("user{n}"), "global_name": format!("User {n}"),
            "avatar": if n % 3 == 0 { Value::Null } else { json!(format!("hash{n}")) }, "bot": false })
}

impl Mock {
    fn new(opts: Opts) -> Mock {
        let (bus, _) = broadcast::channel(4096);
        Mock {
            next_id: AtomicU64::new(0),
            opts,
            messages: Mutex::new(HashMap::new()),
            bus,
            sessions: Mutex::new(HashMap::new()),
            ratelimit_next: AtomicU64::new(0),
            stats: Mutex::new(HashMap::new()),
            pngs: Mutex::new(HashMap::new()),
        }
    }

    fn count(&self, k: &str) {
        *self.stats.lock().unwrap().entry(k.to_string()).or_default() += 1;
    }

    fn new_id(&self) -> u64 {
        snow(now_ms(), self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn is_image_channel(&self, ch: u64) -> bool {
        self.opts.image_history > 0 && ch == channel_id(0, 1)
    }

    fn make_message(&self, ch: u64, guild: Option<u64>, id: u64, n: u64, content: Option<String>) -> Value {
        let author = user(1 + n % 7);
        let content = content.unwrap_or_else(|| {
            let len = 3 + (n * 7919 % 23) as usize;
            let mut s: Vec<&str> = (0..len).map(|k| WORDS[((n + k as u64) * 31 % WORDS.len() as u64) as usize]).collect();
            if n % 17 == 0 {
                s.push("<@1000>");
            }
            if n % 29 == 0 {
                return format!("```rust\nfn main() {{\n    println!(\"{n}\");\n}}\n```");
            }
            s.join(" ")
        });
        let mut m = json!({
            "id": id.to_string(), "channel_id": ch.to_string(), "author": author, "content": content,
            "timestamp": ts(id), "edited_timestamp": null, "type": 0, "attachments": [], "embeds": [],
            "mentions": if content.contains("<@1000>") { json!([user(0)]) } else { json!([]) },
            "mention_roles": [], "mention_everyone": false,
        });
        if let Some(g) = guild {
            m["guild_id"] = json!(g.to_string());
        }
        if n % 11 == 0 {
            m["reactions"] = json!([{ "count": 1 + n % 4, "me": false, "emoji": { "id": null, "name": "👍" } }]);
        }
        if self.is_image_channel(ch) {
            let (w, h) = (320 + (n % 5) * 160, 240 + (n % 3) * 120);
            let port = self.opts.port;
            m["attachments"] = json!([{ "id": (id + 1).to_string(), "filename": format!("image{n}.png"), "size": w * h,
                "url": format!("http://127.0.0.1:{port}/cdn/img/{w}/{h}/{}", n % 8),
                "proxy_url": format!("http://127.0.0.1:{port}/cdn/img/{w}/{h}/{}", n % 8),
                "content_type": "image/png", "width": w, "height": h }]);
        }
        m
    }

    fn ensure_history(&self, ch: u64) {
        let mut all = self.messages.lock().unwrap();
        if all.contains_key(&ch) {
            return;
        }
        let count = if self.is_image_channel(ch) { self.opts.image_history } else { self.opts.history };
        let guild = (ch / 1000) * 1000;
        let start = now_ms() - count * 60_000 - 3_600_000;
        let v = (0..count).map(|n| self.make_message(ch, Some(guild), snow(start + n * 60_000, n), n, None)).collect();
        all.insert(ch, v);
    }

    fn guild_payload(&self, g: u64) -> Value {
        let gid = guild_id(g);
        let cat = gid + 900;
        let mut channels = vec![json!({ "id": cat.to_string(), "type": 4, "name": "Text Channels", "position": 0, "guild_id": gid.to_string(), "permission_overwrites": [] })];
        for c in 0..self.opts.channels {
            let id = channel_id(g, c);
            self.ensure_history(id);
            let last = self.messages.lock().unwrap()[&id].last().map(|m| m["id"].clone()).unwrap_or(Value::Null);
            let name = if self.is_image_channel(id) { "images".to_string() } else { ["general", "random", "dev", "help", "off-topic", "announcements", "music", "memes"][c as usize % 8].to_string() };
            let name = if c >= 8 { format!("{name}-{c}") } else { name };
            channels.push(json!({ "id": id.to_string(), "type": 0, "name": name, "position": c, "parent_id": cat.to_string(),
                "topic": format!("Topic for #{name}"), "last_message_id": last, "permission_overwrites": [], "guild_id": gid.to_string() }));
        }
        // One channel the bot cannot see.
        channels.push(json!({ "id": (gid + 950).to_string(), "type": 0, "name": "secret", "position": 99, "parent_id": cat.to_string(),
            "permission_overwrites": [{ "id": gid.to_string(), "type": 0, "allow": "0", "deny": "1024" }] }));
        json!({
            "id": gid.to_string(), "name": format!("Test Server {}", g + 1), "icon": null, "owner_id": "1",
            "member_count": 42, "unavailable": false,
            "roles": [{ "id": gid.to_string(), "name": "@everyone", "permissions": "117824", "position": 0, "color": 0 },
                      { "id": (gid + 990).to_string(), "name": "Mods", "permissions": "0", "position": 1, "color": 0x3ba55d }],
            "channels": channels, "threads": [],
            "members": [{ "user": user(0), "roles": [], "nick": null }],
        })
    }

    /// Appends the event to every session's replay buffer (so it survives a dropped connection)
    /// and wakes the connected sessions.
    fn emit(&self, t: &str, d: Value) {
        let ev = Arc::new((t.to_string(), d));
        for sb in self.sessions.lock().unwrap().values_mut() {
            sb.seq += 1;
            let seq = sb.seq;
            sb.buf.push_back((seq, ev.clone()));
            if sb.buf.len() > 2000 {
                sb.buf.pop_front();
            }
        }
        let _ = self.bus.send(Ctl::Dispatch);
    }

    fn next_seq(&self, sid: &str) -> u64 {
        let mut all = self.sessions.lock().unwrap();
        let sb = all.entry(sid.to_string()).or_default();
        sb.seq += 1;
        sb.seq
    }

    fn post_message(&self, ch: u64, content: String, author: Value, nonce: Option<Value>) -> Value {
        self.ensure_history(ch);
        let id = self.new_id();
        let guild = if ch >= 2_000_000 { Some((ch / 1000) * 1000) } else { None };
        let mut m = self.make_message(ch, guild, id, id % 1000, Some(content));
        m["author"] = author;
        m["mentions"] = if m["content"].as_str().unwrap_or("").contains("<@1000>") { json!([user(0)]) } else { json!([]) };
        if let Some(n) = nonce {
            m["nonce"] = n;
        }
        if self.is_image_channel(ch) {
            m["attachments"] = json!([]);
        }
        self.messages.lock().unwrap().entry(ch).or_default().push(m.clone());
        self.emit("MESSAGE_CREATE", m.clone());
        m
    }
}

type S = State<Arc<Mock>>;

fn authed(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bot {TOKEN}"))
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "message": msg, "code": 0 }))).into_response()
}

fn guard(s: &Mock, h: &HeaderMap, route: &str) -> Option<Response> {
    s.count(&format!("rest {route}"));
    if !authed(h) {
        return Some(err(StatusCode::UNAUTHORIZED, "401: Unauthorized"));
    }
    if s.ratelimit_next.load(Ordering::Relaxed) > 0 {
        s.ratelimit_next.fetch_sub(1, Ordering::Relaxed);
        s.count("rest 429");
        let mut r = (StatusCode::TOO_MANY_REQUESTS, Json(json!({ "message": "You are being rate limited.", "retry_after": 0.2, "global": false }))).into_response();
        r.headers_mut().insert("retry-after", "1".parse().unwrap());
        return Some(r);
    }
    None
}

fn with_limits(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert("x-ratelimit-limit", "5".parse().unwrap());
    h.insert("x-ratelimit-remaining", "4".parse().unwrap());
    h.insert("x-ratelimit-reset-after", "1.0".parse().unwrap());
    r
}

async fn me(State(s): S, h: HeaderMap) -> Response {
    if let Some(r) = guard(&s, &h, "GET /users/@me") {
        return r;
    }
    Json(user(0)).into_response()
}

async fn gateway_bot(State(s): S, h: HeaderMap) -> Response {
    if let Some(r) = guard(&s, &h, "GET /gateway/bot") {
        return r;
    }
    Json(json!({ "url": format!("ws://127.0.0.1:{}/gateway", s.opts.port), "shards": 1,
        "session_start_limit": { "total": 1000, "remaining": 999, "reset_after": 0, "max_concurrency": 1 } })).into_response()
}

async fn get_messages(State(s): S, h: HeaderMap, Path(ch): Path<u64>, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Some(r) = guard(&s, &h, "GET /channels/messages") {
        return r;
    }
    if ch % 1000 == 950 {
        return err(StatusCode::FORBIDDEN, "Missing Access");
    }
    s.ensure_history(ch);
    let limit: usize = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(50).clamp(1, 100);
    let all = s.messages.lock().unwrap();
    let v = &all[&ch];
    let idv = |m: &Value| m["id"].as_str().unwrap().parse::<u64>().unwrap();
    let page: Vec<Value> = if let Some(b) = q.get("before").and_then(|b| b.parse::<u64>().ok()) {
        let end = v.iter().position(|m| idv(m) >= b).unwrap_or(v.len());
        v[end.saturating_sub(limit)..end].iter().rev().cloned().collect()
    } else if let Some(a) = q.get("after").and_then(|a| a.parse::<u64>().ok()) {
        let start = v.iter().position(|m| idv(m) > a).unwrap_or(v.len());
        v[start..(start + limit).min(v.len())].iter().rev().cloned().collect()
    } else {
        v[v.len().saturating_sub(limit)..].iter().rev().cloned().collect()
    };
    with_limits(Json(page).into_response())
}

async fn create_message(State(s): S, h: HeaderMap, Path(ch): Path<u64>, body: axum::body::Bytes) -> Response {
    if let Some(r) = guard(&s, &h, "POST /channels/messages") {
        return r;
    }
    let ct = h.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("");
    let payload: Value = if ct.starts_with("multipart/") {
        // Pull payload_json out of the multipart body (good enough for tests).
        let text = String::from_utf8_lossy(&body);
        text.split("name=\"payload_json\"")
            .nth(1)
            .and_then(|r| r.split("\r\n\r\n").nth(1))
            .and_then(|r| r.split("\r\n--").next())
            .and_then(|j| serde_json::from_str(j).ok())
            .unwrap_or_default()
    } else {
        serde_json::from_slice(&body).unwrap_or_default()
    };
    let content = payload["content"].as_str().unwrap_or("").to_string();
    if content.is_empty() && !ct.starts_with("multipart/") {
        return err(StatusCode::BAD_REQUEST, "Cannot send an empty message");
    }
    let m = s.post_message(ch, content, user(0), payload.get("nonce").cloned());
    with_limits(Json(m).into_response())
}

async fn edit_message(State(s): S, h: HeaderMap, Path((ch, id)): Path<(u64, String)>, Json(body): Json<Value>) -> Response {
    if let Some(r) = guard(&s, &h, "PATCH /channels/messages") {
        return r;
    }
    let mut all = s.messages.lock().unwrap();
    let Some(m) = all.get_mut(&ch).and_then(|v| v.iter_mut().find(|m| m["id"] == id)) else {
        return err(StatusCode::NOT_FOUND, "Unknown Message");
    };
    m["content"] = body["content"].clone();
    m["edited_timestamp"] = json!(ts(snow(now_ms(), 0)));
    let m = m.clone();
    drop(all);
    s.emit("MESSAGE_UPDATE", m.clone());
    Json(m).into_response()
}

async fn delete_message(State(s): S, h: HeaderMap, Path((ch, id)): Path<(u64, String)>) -> Response {
    if let Some(r) = guard(&s, &h, "DELETE /channels/messages") {
        return r;
    }
    if let Some(v) = s.messages.lock().unwrap().get_mut(&ch) {
        v.retain(|m| m["id"] != id);
    }
    s.emit("MESSAGE_DELETE", json!({ "id": id, "channel_id": ch.to_string() }));
    StatusCode::NO_CONTENT.into_response()
}

async fn reaction(State(s): S, h: HeaderMap, method: axum::http::Method, Path((ch, id, emoji)): Path<(u64, String, String)>) -> Response {
    if let Some(r) = guard(&s, &h, "REACTION") {
        return r;
    }
    let t = if method == axum::http::Method::PUT { "MESSAGE_REACTION_ADD" } else { "MESSAGE_REACTION_REMOVE" };
    let e = match emoji.split_once(':') {
        Some((n, i)) => json!({ "name": n, "id": i }),
        None => json!({ "name": emoji, "id": null }),
    };
    s.emit(t, json!({ "user_id": BOT_ID.to_string(), "channel_id": ch.to_string(), "message_id": id, "emoji": e }));
    StatusCode::NO_CONTENT.into_response()
}

async fn open_dm(State(s): S, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Some(r) = guard(&s, &h, "POST /users/@me/channels") {
        return r;
    }
    let uid: u64 = body["recipient_id"].as_str().and_then(|v| v.parse().ok()).unwrap_or(5001);
    Json(json!({ "id": (900_000 + uid).to_string(), "type": 1, "recipients": [user(uid - 5000)], "last_message_id": null })).into_response()
}

async fn members(State(s): S, h: HeaderMap, Path(_g): Path<u64>) -> Response {
    if let Some(r) = guard(&s, &h, "GET /guilds/members") {
        return r;
    }
    let v: Vec<Value> = (0..40).map(|n| json!({ "user": user(n), "roles": [], "nick": null })).collect();
    Json(v).into_response()
}

fn png(s: &Mock, w: u32, h: u32, seed: u8) -> Arc<Vec<u8>> {
    let mut cache = s.pngs.lock().unwrap();
    cache
        .entry((w, h, seed))
        .or_insert_with(|| {
            let img = image::RgbaImage::from_fn(w, h, |x, y| {
                image::Rgba([(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, seed.wrapping_mul(37), 255])
            });
            let mut out = Vec::new();
            image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
            Arc::new(out)
        })
        .clone()
}

async fn cdn_img(State(s): S, Path((w, h, seed)): Path<(u32, u32, u8)>) -> Response {
    s.count("cdn image");
    let data = png(&s, w.min(2048), h.min(2048), seed);
    ([("content-type", "image/png")], data.as_ref().clone()).into_response()
}

async fn cdn_avatar(State(s): S, Path((_a, b, _c)): Path<(String, String, String)>) -> Response {
    s.count("cdn avatar");
    let seed = b.bytes().fold(0u8, |a, x| a.wrapping_add(x));
    let data = png(&s, 64, 64, seed);
    ([("content-type", "image/png")], data.as_ref().clone()).into_response()
}

async fn control(State(s): S, Path(action): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    match action.as_str() {
        "drop" => {
            let _ = s.bus.send(Ctl::Drop);
        }
        "reconnect" => {
            let _ = s.bus.send(Ctl::Reconnect);
        }
        "invalid-session" => {
            let _ = s.bus.send(Ctl::InvalidSession);
        }
        "ratelimit" => s.ratelimit_next.store(q.get("n").and_then(|n| n.parse().ok()).unwrap_or(1), Ordering::Relaxed),
        "emit" => {
            let ch: u64 = q.get("channel").and_then(|c| c.parse().ok()).unwrap_or(channel_id(0, 0));
            let content = q.get("content").cloned().unwrap_or_else(|| "hello from the mock".into());
            let m = s.post_message(ch, content, user(2), None);
            return Json(m).into_response();
        }
        "stats" => return Json(json!(*s.stats.lock().unwrap())).into_response(),
        _ => return StatusCode::NOT_FOUND.into_response(),
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn gateway(State(s): S, ws: WebSocketUpgrade, Query(q): Query<HashMap<String, String>>) -> Response {
    let compress = q.get("compress").map(|c| c == "zlib-stream").unwrap_or(false);
    ws.on_upgrade(move |socket| session(s, socket, compress))
}

struct Out {
    z: Option<Compress>,
}

impl Out {
    fn frame(&mut self, v: &Value) -> Ws {
        let text = v.to_string();
        match &mut self.z {
            None => Ws::Text(text.into()),
            Some(c) => {
                let mut out = Vec::with_capacity(text.len() / 2 + 64);
                loop {
                    let before = c.total_in();
                    let consumed_so_far = (c.total_in() - before) as usize;
                    let _ = consumed_so_far;
                    c.compress_vec(text.as_bytes(), &mut out, FlushCompress::Sync).unwrap();
                    if out.len() < out.capacity() {
                        break;
                    }
                    out.reserve(out.capacity());
                    // Data was not fully consumed: compress the remainder.
                    let done = c.total_in() - before;
                    if done as usize >= text.len() {
                        break;
                    }
                    let rest = &text.as_bytes()[done as usize..];
                    c.compress_vec(rest, &mut out, FlushCompress::Sync).unwrap();
                    break;
                }
                Ws::Binary(out.into())
            }
        }
    }
}

async fn session(s: Arc<Mock>, socket: WebSocket, compress: bool) {
    s.count("gateway connect");
    let (mut tx, mut rx) = socket.split();
    let mut out = Out { z: compress.then(|| Compress::new(Compression::fast(), true)) };
    let mut bus = s.bus.subscribe();
    if tx.send(out.frame(&json!({ "op": 10, "d": { "heartbeat_interval": s.opts.heartbeat_ms } }))).await.is_err() {
        return;
    }
    let mut seq: u64 = 0;
    let mut session_id: Option<String> = None;
    let mut chatter = if s.opts.chatter > 0.0 { Some(tokio::time::interval(Duration::from_secs_f64(1.0 / s.opts.chatter))) } else { None };
    let mut chatter_n: u64 = 0;
    loop {
        tokio::select! {
            m = rx.next() => {
                let Some(Ok(m)) = m else { return };
                let Ws::Text(t) = m else { if matches!(m, Ws::Close(_)) { return } continue };
                let v: Value = serde_json::from_str(&t).unwrap_or_default();
                match v["op"].as_u64() {
                    Some(1) => {
                        s.count("gateway heartbeat");
                        let _ = tx.send(out.frame(&json!({ "op": 11 }))).await;
                    }
                    Some(2) => {
                        s.count("gateway identify");
                        if v["d"]["token"] != TOKEN {
                            let _ = tx.send(Ws::Close(Some(axum::extract::ws::CloseFrame { code: 4004, reason: "Authentication failed.".into() }))).await;
                            return;
                        }
                        if v["d"]["intents"].as_u64().unwrap_or(0) & (1 << 1) != 0 && std::env::var_os("MOCK_DENY_MEMBERS").is_some() {
                            let _ = tx.send(Ws::Close(Some(axum::extract::ws::CloseFrame { code: 4014, reason: "Disallowed intent(s).".into() }))).await;
                            return;
                        }
                        let sid = format!("sess{}", s.new_id());
                        s.sessions.lock().unwrap().insert(sid.clone(), SessBuf::default());
                        session_id = Some(sid.clone());
                        let guilds: Vec<Value> = (0..s.opts.guilds).map(|g| json!({ "id": guild_id(g).to_string(), "unavailable": true })).collect();
                        seq = s.next_seq(&sid);
                        let ready = json!({ "op": 0, "t": "READY", "s": seq, "d": { "v": 10, "user": user(0), "guilds": guilds,
                            "session_id": sid, "resume_gateway_url": format!("ws://127.0.0.1:{}/gateway", s.opts.port), "private_channels": [] } });
                        if tx.send(out.frame(&ready)).await.is_err() { return }
                        for g in 0..s.opts.guilds {
                            seq = s.next_seq(&sid);
                            let gc = json!({ "op": 0, "t": "GUILD_CREATE", "s": seq, "d": s.guild_payload(g) });
                            if tx.send(out.frame(&gc)).await.is_err() { return }
                        }
                    }
                    Some(6) => {
                        s.count("gateway resume");
                        let sid = v["d"]["session_id"].as_str().unwrap_or("").to_string();
                        let from = v["d"]["seq"].as_u64().unwrap_or(0);
                        let buf = s.sessions.lock().unwrap().get(&sid).cloned();
                        match buf {
                            Some(sb) => {
                                session_id = Some(sid.clone());
                                seq = from;
                                for (sq, ev) in sb.buf.iter().filter(|(sq, _)| *sq > from) {
                                    seq = *sq;
                                    let _ = tx.send(out.frame(&json!({ "op": 0, "t": ev.0, "s": sq, "d": ev.1 }))).await;
                                }
                                seq = seq.max(sb.seq);
                                let rs = s.next_seq(&sid);
                                seq = seq.max(rs);
                                let _ = tx.send(out.frame(&json!({ "op": 0, "t": "RESUMED", "s": rs, "d": {} }))).await;
                            }
                            None => {
                                let _ = tx.send(out.frame(&json!({ "op": 9, "d": false }))).await;
                            }
                        }
                    }
                    _ => {}
                }
            }
            c = bus.recv() => {
                match c {
                    Ok(Ctl::Dispatch) => {
                        let Some(sid) = &session_id else { continue };
                        let pending: Vec<(u64, Arc<(String, Value)>)> = s.sessions.lock().unwrap().get(sid)
                            .map(|sb| sb.buf.iter().filter(|(sq, _)| *sq > seq).cloned().collect()).unwrap_or_default();
                        for (sq, ev) in pending {
                            seq = sq;
                            if tx.send(out.frame(&json!({ "op": 0, "t": ev.0, "s": sq, "d": ev.1 }))).await.is_err() { return }
                        }
                    }
                    Ok(Ctl::Drop) => return, // abrupt TCP close, no close frame
                    Ok(Ctl::Reconnect) => { let _ = tx.send(out.frame(&json!({ "op": 7, "d": null }))).await; }
                    Ok(Ctl::InvalidSession) => {
                        if let Some(sid) = &session_id { s.sessions.lock().unwrap().remove(sid); }
                        let _ = tx.send(out.frame(&json!({ "op": 9, "d": false }))).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
            _ = async { chatter.as_mut().unwrap().tick().await }, if chatter.is_some() && session_id.is_some() => {
                chatter_n += 1;
                let g = chatter_n % s.opts.guilds.max(1);
                let c = chatter_n / s.opts.guilds.max(1) % s.opts.chatter_channels.max(1);
                let content = format!("live message {chatter_n} {}", WORDS[(chatter_n % WORDS.len() as u64) as usize]);
                s.post_message(channel_id(g, c), content, user(1 + chatter_n % 7), None);
            }
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let opts = Opts::parse();
    let port = opts.port;
    let s = Arc::new(Mock::new(opts));
    let app = Router::new()
        .route("/gateway", get(gateway))
        .route("/api/v10/users/@me", get(me))
        .route("/api/v10/gateway/bot", get(gateway_bot))
        .route("/api/v10/channels/{ch}/messages", get(get_messages).post(create_message))
        .route("/api/v10/channels/{ch}/messages/{id}", axum::routing::patch(edit_message).delete(delete_message))
        .route("/api/v10/channels/{ch}/messages/{id}/reactions/{emoji}/@me", put(reaction).delete(reaction))
        .route("/api/v10/users/@me/channels", post(open_dm))
        .route("/api/v10/guilds/{g}/members", get(members))
        .route("/cdn/img/{w}/{h}/{seed}", get(cdn_img))
        .route("/cdn/avatars/{a}/{b}/{c}", get(cdn_avatar))
        .route("/cdn/embed/avatars/{b}", get(|s: S, Path(b): Path<String>| async move { cdn_avatar(s, Path((String::new(), b, String::new()))).await }))
        .route("/control/{action}", post(control))
        .with_state(s);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    println!("feather-mock listening on http://127.0.0.1:{port}");
    axum::serve(listener, app).await.unwrap();
}
