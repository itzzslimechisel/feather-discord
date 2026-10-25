//! Discord Gateway client (v10, JSON encoding, zlib-stream transport compression).
//!
//! Implements the documented lifecycle: Hello → jittered heartbeats → Identify or Resume →
//! dispatches; zombie detection (missed ACK), op 1/7/9 handling, resumable vs. fatal close codes,
//! identify spacing, the 120-events-per-60s send limit and bounded exponential reconnect backoff.
//!
//! The only timer this task arms while idle is the protocol-mandated heartbeat.

use crate::creds::Secret;
use crate::diag::backoff;
use flate2::{Decompress, FlushDecompress};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, timeout, Instant};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub mod intents {
    pub const GUILDS: u64 = 1 << 0;
    pub const GUILD_MEMBERS: u64 = 1 << 1;
    pub const GUILD_MESSAGES: u64 = 1 << 9;
    pub const GUILD_MESSAGE_REACTIONS: u64 = 1 << 10;
    pub const DIRECT_MESSAGES: u64 = 1 << 12;
    pub const DIRECT_MESSAGE_REACTIONS: u64 = 1 << 13;
    pub const MESSAGE_CONTENT: u64 = 1 << 15;
}

pub fn intents_for(s: &crate::settings::Intents) -> u64 {
    use intents::*;
    let mut i = GUILDS | GUILD_MESSAGES | GUILD_MESSAGE_REACTIONS | DIRECT_MESSAGES | DIRECT_MESSAGE_REACTIONS;
    if s.message_content {
        i |= MESSAGE_CONTENT;
    }
    if s.guild_members {
        i |= GUILD_MEMBERS;
    }
    i
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnState {
    Connecting,
    Connected,
    Resuming,
    Reconnecting { in_ms: u64, attempt: u32 },
    Offline,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Fatal {
    AuthenticationFailed,
    DisallowedIntents,
    InvalidIntents,
    Other(u16, String),
}

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fatal::AuthenticationFailed => write!(f, "Discord rejected the bot token (4004)."),
            Fatal::DisallowedIntents => write!(
                f,
                "A privileged intent is not enabled for this bot (4014). Enable it in the Developer Portal under Bot → Privileged Gateway Intents, or turn it off in Feather's connection settings."
            ),
            Fatal::InvalidIntents => write!(f, "Invalid gateway intents (4013)."),
            Fatal::Other(c, r) => write!(f, "Gateway closed the connection ({c}: {r})."),
        }
    }
}

#[derive(Debug)]
pub enum GatewayEvent {
    Dispatch { t: String, d: Box<RawValue> },
    Status(ConnState),
    Fatal(Fatal),
}

#[derive(Debug)]
pub enum GatewayCommand {
    Send(serde_json::Value),
    Shutdown,
}

#[derive(Clone)]
pub struct GatewayConfig {
    pub url: String,
    pub token: Secret,
    pub intents: u64,
    pub compress: bool,
}

#[derive(Default)]
pub struct GatewayStats {
    pub bytes_wire: AtomicU64,
    pub bytes_json: AtomicU64,
    pub dispatches: AtomicU64,
    pub heartbeats: AtomicU64,
    pub reconnects: AtomicU64,
    pub resumes: AtomicU64,
}

#[derive(Deserialize)]
struct Envelope<'a> {
    op: u8,
    #[serde(borrow)]
    d: Option<&'a RawValue>,
    s: Option<u64>,
    t: Option<String>,
}

#[derive(Deserialize)]
struct Hello {
    heartbeat_interval: u64,
}

#[derive(Deserialize)]
struct ReadyHead {
    session_id: String,
    resume_gateway_url: String,
}

/// Incremental zlib-stream decoder: one inflate context for the whole connection; a message is
/// complete when the buffered data ends with the Z_SYNC_FLUSH suffix 00 00 ff ff.
pub struct ZlibStream {
    d: Decompress,
    buf: Vec<u8>,
    out: Vec<u8>,
}

impl Default for ZlibStream {
    fn default() -> Self {
        ZlibStream { d: Decompress::new(true), buf: Vec::new(), out: Vec::with_capacity(16 * 1024) }
    }
}

const MAX_PAYLOAD: usize = 64 << 20;

impl ZlibStream {
    pub fn push(&mut self, data: &[u8]) -> Option<Result<&[u8], String>> {
        self.buf.extend_from_slice(data);
        if self.buf.len() > MAX_PAYLOAD {
            return Some(Err("compressed payload too large".into()));
        }
        if self.buf.len() < 4 || self.buf[self.buf.len() - 4..] != [0, 0, 0xff, 0xff] {
            return None;
        }
        self.out.clear();
        let mut input: &[u8] = &self.buf;
        loop {
            if self.out.capacity() - self.out.len() < 8192 {
                self.out.reserve(self.out.capacity().max(16 * 1024));
            }
            let before = self.d.total_in();
            if let Err(e) = self.d.decompress_vec(input, &mut self.out, FlushDecompress::Sync) {
                return Some(Err(e.to_string()));
            }
            input = &input[(self.d.total_in() - before) as usize..];
            if self.out.len() > MAX_PAYLOAD {
                return Some(Err("payload too large".into()));
            }
            if input.is_empty() && self.out.len() < self.out.capacity() {
                break;
            }
        }
        self.buf.clear();
        if self.buf.capacity() > 1 << 20 {
            self.buf.shrink_to(64 * 1024);
        }
        if self.out.capacity() > 4 << 20 && self.out.len() < 1 << 20 {
            // A one-off huge payload should not pin its buffer for the life of the connection.
            self.out.shrink_to(1 << 20);
        }
        Some(Ok(&self.out))
    }
}

/// Sliding-window limiter for the 120 sends / 60 s gateway rule. Heartbeats always go
/// through; other commands use only 110 of the slots so heartbeats can never be starved.
struct SendLimiter(VecDeque<Instant>);

impl SendLimiter {
    fn record(&mut self) {
        self.0.push_back(Instant::now());
    }
    fn wait_for_command(&mut self) -> Option<Duration> {
        let now = Instant::now();
        while self.0.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
            self.0.pop_front();
        }
        (self.0.len() >= 110).then(|| Duration::from_secs(60) - now.duration_since(self.0[0]))
    }
}

struct Session {
    id: String,
    resume_url: String,
    seq: Option<u64>,
}

enum Exit {
    /// Reconnect; `resume` says whether the session may be resumed.
    Reconnect { resume: bool, delay: Option<Duration> },
    Fatal(Fatal),
    Shutdown,
}

fn with_query(base: &str, compress: bool) -> String {
    let mut u = match url::Url::parse(base) {
        Ok(u) => u,
        Err(_) => return base.to_string(),
    };
    {
        let mut q = u.query_pairs_mut();
        q.clear().append_pair("v", "10").append_pair("encoding", "json");
        if compress {
            q.append_pair("compress", "zlib-stream");
        }
    }
    u.to_string()
}

pub async fn run(
    cfg: GatewayConfig,
    events: mpsc::Sender<GatewayEvent>,
    mut cmds: mpsc::UnboundedReceiver<GatewayCommand>,
    stats: Arc<GatewayStats>,
) {
    let mut session: Option<Session> = None;
    let mut attempt: u32 = 0;
    let mut last_identify: Option<Instant> = None;
    loop {
        let resuming = session.is_some();
        let _ = events.send(GatewayEvent::Status(if resuming { ConnState::Resuming } else { ConnState::Connecting })).await;
        let exit = connection(&cfg, &events, &mut cmds, &mut session, &mut last_identify, &stats, &mut attempt).await;
        match exit {
            Exit::Shutdown => {
                let _ = events.send(GatewayEvent::Status(ConnState::Offline)).await;
                return;
            }
            Exit::Fatal(f) => {
                log::warn!("gateway stopped: {f}");
                let _ = events.send(GatewayEvent::Fatal(f)).await;
                let _ = events.send(GatewayEvent::Status(ConnState::Offline)).await;
                return;
            }
            Exit::Reconnect { resume, delay } => {
                stats.reconnects.fetch_add(1, Ordering::Relaxed);
                if !resume {
                    session = None;
                }
                attempt += 1;
                let wait = delay.unwrap_or_else(|| {
                    if attempt <= 1 {
                        Duration::from_millis(250 + fastrand::u64(0..750))
                    } else {
                        backoff(attempt - 1, Duration::from_secs(1), Duration::from_secs(60))
                    }
                });
                let _ = events
                    .send(GatewayEvent::Status(ConnState::Reconnecting { in_ms: wait.as_millis() as u64, attempt }))
                    .await;
                // Wait, but stay responsive to shutdown.
                let deadline = Instant::now() + wait;
                loop {
                    tokio::select! {
                        _ = sleep_until(deadline) => break,
                        c = cmds.recv() => match c {
                            None | Some(GatewayCommand::Shutdown) => {
                                let _ = events.send(GatewayEvent::Status(ConnState::Offline)).await;
                                return;
                            }
                            Some(GatewayCommand::Send(_)) => {} // dropped while offline
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn connection(
    cfg: &GatewayConfig,
    events: &mpsc::Sender<GatewayEvent>,
    cmds: &mut mpsc::UnboundedReceiver<GatewayCommand>,
    session: &mut Option<Session>,
    last_identify: &mut Option<Instant>,
    stats: &GatewayStats,
    attempt: &mut u32,
) -> Exit {
    let base = session.as_ref().map(|s| s.resume_url.clone()).unwrap_or_else(|| cfg.url.clone());
    let url = with_query(&base, cfg.compress);
    let mut ws_cfg = WebSocketConfig::default();
    ws_cfg.max_message_size = Some(MAX_PAYLOAD);
    ws_cfg.max_frame_size = Some(MAX_PAYLOAD);
    let ws = match timeout(Duration::from_secs(15), tokio_tungstenite::connect_async_with_config(&url, Some(ws_cfg), false)).await {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => {
            log::info!("gateway connect failed: {e}");
            return Exit::Reconnect { resume: session.is_some(), delay: None };
        }
        Err(_) => {
            log::info!("gateway connect timed out");
            return Exit::Reconnect { resume: session.is_some(), delay: None };
        }
    };
    let (mut tx, mut rx) = ws.split();
    let mut z = ZlibStream::default();
    let mut limiter = SendLimiter(VecDeque::new());

    // Decodes one websocket message into a JSON text, or None when more frames are needed.
    macro_rules! decode {
        ($m:expr) => {{
            match $m {
                WsMessage::Binary(b) => {
                    stats.bytes_wire.fetch_add(b.len() as u64, Ordering::Relaxed);
                    match z.push(&b) {
                        None => None,
                        Some(Ok(out)) => Some(String::from_utf8_lossy(out).into_owned()),
                        Some(Err(e)) => {
                            log::warn!("gateway decompression failed: {e}");
                            return Exit::Reconnect { resume: false, delay: None };
                        }
                    }
                }
                WsMessage::Text(t) => {
                    stats.bytes_wire.fetch_add(t.len() as u64, Ordering::Relaxed);
                    Some(t.to_string())
                }
                WsMessage::Close(frame) => return on_close(frame.map(|f| (u16::from(f.code), f.reason.to_string())), session),
                _ => None,
            }
        }};
    }

    // 1. Hello
    let hello_deadline = Instant::now() + Duration::from_secs(20);
    let interval = loop {
        let msg = match timeout(hello_deadline.saturating_duration_since(Instant::now()), rx.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => return Exit::Reconnect { resume: session.is_some(), delay: None },
        };
        let Some(text) = decode!(msg) else { continue };
        let env: Envelope = match serde_json::from_str(&text) {
            Ok(e) => e,
            Err(_) => return Exit::Reconnect { resume: session.is_some(), delay: None },
        };
        if env.op == 10 {
            match env.d.and_then(|d| serde_json::from_str::<Hello>(d.get()).ok()) {
                Some(h) if h.heartbeat_interval > 0 => break Duration::from_millis(h.heartbeat_interval),
                _ => return Exit::Reconnect { resume: false, delay: None },
            }
        }
    };

    macro_rules! send {
        ($v:expr) => {{
            let s = $v.to_string();
            limiter.record();
            if tx.send(WsMessage::Text(s.into())).await.is_err() {
                return Exit::Reconnect { resume: session.is_some(), delay: None };
            }
        }};
    }

    // 2. Identify or Resume
    if let Some(s) = session.as_ref() {
        send!(json!({ "op": 6, "d": { "token": cfg.token.as_str(), "session_id": s.id, "seq": s.seq } }));
    } else {
        if let Some(t) = *last_identify {
            let next = t + Duration::from_millis(5500);
            if next > Instant::now() {
                tokio::time::sleep_until(next).await;
            }
        }
        *last_identify = Some(Instant::now());
        send!(json!({ "op": 2, "d": {
            "token": cfg.token.as_str(),
            "intents": cfg.intents,
            "large_threshold": 50,
            "properties": { "os": std::env::consts::OS, "browser": "feather", "device": "feather" }
        }}));
    }

    // 3. Main loop
    let mut next_beat = Instant::now() + interval.mul_f64(fastrand::f64());
    let mut acked = true;
    let mut pending: VecDeque<serde_json::Value> = VecDeque::new();
    loop {
        let cmd_wait = if pending.is_empty() { None } else { limiter.wait_for_command() };
        tokio::select! {
            _ = sleep_until(next_beat) => {
                if !acked {
                    log::info!("gateway heartbeat not acknowledged; reconnecting");
                    let _ = tx.send(WsMessage::Close(Some(CloseFrame { code: CloseCode::Library(4000), reason: "zombie".into() }))).await;
                    return Exit::Reconnect { resume: session.is_some(), delay: Some(Duration::from_millis(100)) };
                }
                stats.heartbeats.fetch_add(1, Ordering::Relaxed);
                send!(json!({ "op": 1, "d": session.as_ref().and_then(|s| s.seq) }));
                acked = false;
                next_beat = Instant::now() + interval;
            }
            _ = async { tokio::time::sleep(cmd_wait.unwrap_or_default()).await }, if !pending.is_empty() => {
                if limiter.wait_for_command().is_none() {
                    if let Some(v) = pending.pop_front() {
                        send!(v);
                    }
                }
            }
            c = cmds.recv() => match c {
                None | Some(GatewayCommand::Shutdown) => {
                    let _ = tx.send(WsMessage::Close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() }))).await;
                    return Exit::Shutdown;
                }
                Some(GatewayCommand::Send(v)) => {
                    if pending.len() < 64 {
                        pending.push_back(v);
                    }
                }
            },
            m = rx.next() => {
                let msg = match m {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        log::info!("gateway read error: {e}");
                        return Exit::Reconnect { resume: session.is_some(), delay: None };
                    }
                    None => return Exit::Reconnect { resume: session.is_some(), delay: None },
                };
                let Some(text) = decode!(msg) else { continue };
                stats.bytes_json.fetch_add(text.len() as u64, Ordering::Relaxed);
                let env: Envelope = match serde_json::from_str(&text) {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("malformed gateway payload ignored: {e}");
                        continue;
                    }
                };
                match env.op {
                    0 => {
                        if let (Some(s), Some(sess)) = (env.s, session.as_mut()) {
                            sess.seq = Some(s);
                        }
                        let t = env.t.unwrap_or_default();
                        let Some(d) = env.d else { continue };
                        if t == "READY" {
                            match serde_json::from_str::<ReadyHead>(d.get()) {
                                Ok(r) => *session = Some(Session { id: r.session_id, resume_url: r.resume_gateway_url, seq: env.s }),
                                Err(e) => log::warn!("READY without session info: {e}"),
                            }
                            *attempt = 0;
                            let _ = events.send(GatewayEvent::Status(ConnState::Connected)).await;
                        } else if t == "RESUMED" {
                            *attempt = 0;
                            stats.resumes.fetch_add(1, Ordering::Relaxed);
                            let _ = events.send(GatewayEvent::Status(ConnState::Connected)).await;
                        }
                        stats.dispatches.fetch_add(1, Ordering::Relaxed);
                        if events.send(GatewayEvent::Dispatch { t, d: d.to_owned() }).await.is_err() {
                            return Exit::Shutdown;
                        }
                    }
                    1 => {
                        send!(json!({ "op": 1, "d": session.as_ref().and_then(|s| s.seq) }));
                    }
                    7 => {
                        let _ = tx.send(WsMessage::Close(Some(CloseFrame { code: CloseCode::Library(4000), reason: "reconnect".into() }))).await;
                        return Exit::Reconnect { resume: session.is_some(), delay: Some(Duration::from_millis(50)) };
                    }
                    9 => {
                        let resumable = env.d.map(|d| d.get() == "true").unwrap_or(false);
                        let _ = tx.send(WsMessage::Close(Some(CloseFrame { code: CloseCode::Library(4000), reason: "invalid session".into() }))).await;
                        let delay = Duration::from_millis(1000 + fastrand::u64(0..4000));
                        return Exit::Reconnect { resume: resumable && session.is_some(), delay: Some(delay) };
                    }
                    11 => acked = true,
                    _ => {}
                }
            }
        }
    }
}

fn on_close(frame: Option<(u16, String)>, session: &mut Option<Session>) -> Exit {
    let (code, reason) = frame.unwrap_or((1006, String::new()));
    log::info!("gateway closed: {code} {reason}");
    match code {
        4004 => Exit::Fatal(Fatal::AuthenticationFailed),
        4013 => Exit::Fatal(Fatal::InvalidIntents),
        4014 => Exit::Fatal(Fatal::DisallowedIntents),
        4010 | 4011 | 4012 => Exit::Fatal(Fatal::Other(code, reason)),
        4007 | 4009 => {
            *session = None;
            Exit::Reconnect { resume: false, delay: None }
        }
        _ => Exit::Reconnect { resume: session.is_some(), delay: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    fn compress_stream(msgs: &[&str]) -> Vec<Vec<u8>> {
        let mut c = Compress::new(Compression::default(), true);
        msgs.iter()
            .map(|m| {
                let mut out = Vec::with_capacity(m.len() + 64);
                c.compress_vec(m.as_bytes(), &mut out, FlushCompress::Sync).unwrap();
                out
            })
            .collect()
    }

    #[test]
    fn zlib_stream_shares_context_and_handles_fragments() {
        let big = format!("{{\"op\":0,\"d\":\"{}\"}}", "x".repeat(200_000));
        let msgs = ["{\"op\":10}", big.as_str(), "{\"op\":11}"];
        let frames = compress_stream(&msgs);
        let mut z = ZlibStream::default();
        assert_eq!(z.push(&frames[0]).unwrap().unwrap(), msgs[0].as_bytes());
        // Split the second message into two websocket frames.
        let (a, b) = frames[1].split_at(frames[1].len() / 2);
        assert!(z.push(a).is_none());
        assert_eq!(z.push(b).unwrap().unwrap(), msgs[1].as_bytes());
        assert_eq!(z.push(&frames[2]).unwrap().unwrap(), msgs[2].as_bytes());
    }

    #[test]
    fn zlib_garbage_is_an_error() {
        let mut z = ZlibStream::default();
        assert!(z.push(&[1, 2, 3, 4, 0, 0, 0xff, 0xff]).unwrap().is_err());
    }

    #[test]
    fn close_codes() {
        let mut s = Some(Session { id: "a".into(), resume_url: "x".into(), seq: Some(1) });
        assert!(matches!(on_close(Some((4004, "".into())), &mut s), Exit::Fatal(Fatal::AuthenticationFailed)));
        assert!(matches!(on_close(Some((4014, "".into())), &mut s), Exit::Fatal(Fatal::DisallowedIntents)));
        assert!(matches!(on_close(Some((4000, "".into())), &mut s), Exit::Reconnect { resume: true, .. }));
        assert!(matches!(on_close(Some((4009, "".into())), &mut s), Exit::Reconnect { resume: false, .. }));
        assert!(s.is_none());
    }

    #[test]
    fn query_building() {
        assert_eq!(with_query("wss://gateway.discord.gg", true), "wss://gateway.discord.gg/?v=10&encoding=json&compress=zlib-stream");
        assert_eq!(with_query("ws://127.0.0.1:9/gateway", false), "ws://127.0.0.1:9/gateway?v=10&encoding=json");
    }

    #[test]
    fn intents_bits() {
        let s = crate::settings::Intents { message_content: false, guild_members: false };
        assert_eq!(intents_for(&s) & intents::MESSAGE_CONTENT, 0);
        let s = crate::settings::Intents { message_content: true, guild_members: true };
        assert_ne!(intents_for(&s) & intents::GUILD_MEMBERS, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn limiter_reserves_heartbeat_headroom() {
        let mut l = SendLimiter(VecDeque::new());
        for _ in 0..110 {
            l.record();
        }
        assert!(l.wait_for_command().is_some());
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(l.wait_for_command().is_none());
    }
}
