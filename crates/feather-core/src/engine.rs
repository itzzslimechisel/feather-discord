//! The background engine: one OS thread running a single-threaded async runtime that owns the
//! gateway connection, REST client, local cache and notification logic.
//!
//! The UI talks to it with [`Command`]s and receives [`Event`]s. The engine never blocks the UI
//! thread, and the UI never touches the network or disk directly.

use crate::config::Endpoints;
use crate::creds::{self, LoadResult, Secret};
use crate::gateway::{self, ConnState, GatewayCommand, GatewayConfig, GatewayEvent, GatewayStats};
use crate::images::{self, DecodedImage};
use crate::model::*;
use crate::notify::Notifier;
use crate::permissions::{self, VIEW_CHANNEL};
use crate::rest::{Rest, RestError, Upload};
use crate::settings::Intents;
use crate::store::{is_corruption, ReadState, Store};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Semaphore};

pub const PAGE: u32 = 50;
const FLUSH_AFTER: Duration = Duration::from_millis(1500);
const FLUSH_BATCH: usize = 256;

pub enum Command {
    Login { token: Secret, remember: bool },
    Logout,
    Reconnect,
    SetIntents(Intents),
    OpenChannel { channel: Id },
    LoadOlder { channel: Id, before: Id },
    LoadNewer { channel: Id, after: Id },
    Send { channel: Id, content: String, reply_to: Option<Id>, nonce: String, files: Vec<Upload> },
    Edit { channel: Id, message: Id, content: String },
    Delete { channel: Id, message: Id },
    React { channel: Id, message: Id, emoji: Emoji, add: bool },
    OpenDm { user: Id },
    MarkRead { channel: Id, message: Id },
    LoadMembers { guild: Id },
    Search { query: String, channel: Option<Id>, seq: u64 },
    FetchImage { url: String, max_w: u32, max_h: u32 },
    Focus { focused: bool, active_channel: Option<Id> },
    SetNotify { enabled: bool, all_dms: bool },
    SetDiskLimit(u32),
    /// Benchmark/diagnostic counters are emitted as an [`Event::Stats`].
    QueryStats,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Session {
    NeedsLogin { reason: Option<String> },
    LoggingIn,
    LoggedIn { user: User, token_persisted: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKind {
    /// The newest page of a channel.
    Latest,
    Older,
    Newer,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub rest_requests: u64,
    pub rest_rate_limited: u64,
    pub rest_bytes: u64,
    pub gw_bytes_wire: u64,
    pub gw_bytes_json: u64,
    pub gw_dispatches: u64,
    pub gw_heartbeats: u64,
    pub gw_reconnects: u64,
    pub gw_resumes: u64,
    pub db_messages: u64,
    pub db_bytes: u64,
    pub db_flushes: u64,
    pub images_fetched: u64,
    pub image_bytes: u64,
}

#[derive(Debug)]
pub enum Event {
    Session(Session),
    Conn(ConnState),
    /// Cached state for instant startup (also re-sent after logout with empty contents).
    Snapshot { user: Option<User>, guilds: Vec<GuildInfo>, dms: Vec<Channel>, read: HashMap<Id, ReadState> },
    Guild(GuildInfo),
    GuildRemoved(Id),
    Dm(Channel),
    Message(Message),
    MessageUpdated(MessageUpdate),
    MessagesDeleted { channel: Id, ids: Vec<Id> },
    Reaction { channel: Id, message: Id, emoji: Emoji, add: bool, me: bool },
    ReactionsCleared { channel: Id, message: Id, emoji: Option<Emoji> },
    History { channel: Id, kind: HistoryKind, messages: Vec<Message>, from_cache: bool, reached_end: bool },
    Read { channel: Id, state: ReadState },
    SendFailed { channel: Id, nonce: String, error: String },
    Members { guild: Id, result: Result<Vec<Member>, String> },
    SearchResults { seq: u64, results: Vec<Message> },
    Image { url: String, result: Result<DecodedImage, String> },
    OpenChannel(Id),
    Toast(String),
    CacheReset,
    Stats(Stats),
}

pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub endpoints: Endpoints,
    pub intents: Intents,
    pub disk_messages_per_channel: u32,
    pub notifications: bool,
    pub notify_all_dms: bool,
}

#[derive(Clone)]
pub struct UiTx {
    tx: std::sync::mpsc::Sender<Event>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl UiTx {
    pub fn new(tx: std::sync::mpsc::Sender<Event>, wake: Arc<dyn Fn() + Send + Sync>) -> UiTx {
        UiTx { tx, wake }
    }
    pub fn send(&self, e: Event) {
        if self.tx.send(e).is_ok() {
            (self.wake)();
        }
    }
}

pub struct EngineHandle {
    pub tx: mpsc::UnboundedSender<Command>,
    pub rx: std::sync::mpsc::Receiver<Event>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EngineHandle {
    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }
    /// Asks the engine to flush and disconnect cleanly, waiting at most `max`.
    pub fn shutdown(mut self, max: Duration) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let deadline = std::time::Instant::now() + max;
            while !t.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

pub fn spawn(cfg: EngineConfig, wake: Arc<dyn Fn() + Send + Sync>) -> EngineHandle {
    let (ctx, crx) = mpsc::unbounded_channel();
    let (etx, erx) = std::sync::mpsc::channel();
    let ui = UiTx::new(etx, wake);
    let thread = std::thread::Builder::new()
        .name("feather-engine".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(2)
                .thread_keep_alive(Duration::from_secs(5))
                .build()
                .expect("tokio runtime");
            rt.block_on(Engine::new(cfg, ui).run(crx));
        })
        .expect("engine thread");
    EngineHandle { tx: ctx, rx: erx, thread: Some(thread) }
}

/// Minimal per-guild context needed to recompute channel permissions on updates.
struct GuildCtx {
    info: GuildInfo,
    owner: Id,
    roles: Vec<Role>,
    all_channels: Vec<Channel>,
    threads: Vec<Channel>,
}

enum Internal {
    Fetched { channel: Id, kind: HistoryKind, result: Result<Vec<Message>, RestError>, requested: u32 },
    LoggedIn { token: Secret, remember: bool, result: Result<(User, Option<String>), RestError> },
    DmOpened(Result<Channel, RestError>),
    ImageDone { url: String },
}

struct Engine {
    cfg: EngineConfig,
    ui: UiTx,
    store: Store,
    rest: Arc<Rest>,
    image_http: reqwest::Client,
    image_sem: Arc<Semaphore>,
    images_inflight: HashSet<String>,
    gw_tx: Option<mpsc::UnboundedSender<GatewayCommand>>,
    gw_events_tx: mpsc::Sender<GatewayEvent>,
    gw_events_rx: Option<mpsc::Receiver<GatewayEvent>>,
    gw_stats: Arc<GatewayStats>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: Option<mpsc::UnboundedReceiver<Internal>>,
    token: Option<Secret>,
    me: Option<User>,
    guilds: HashMap<Id, GuildCtx>,
    dms: HashMap<Id, Channel>,
    channel_guild: HashMap<Id, Id>,
    read: HashMap<Id, ReadState>,
    last_msg: HashMap<Id, Id>,
    /// Channels whose newest page has been fetched since the current gateway session began.
    synced: HashSet<Id>,
    fetching: HashSet<(Id, u8)>,
    reached_start: HashSet<Id>,
    pending_msgs: Vec<Message>,
    pending_reads: HashMap<Id, ReadState>,
    pending_deletes: Vec<Id>,
    inserts_since_prune: HashMap<Id, u32>,
    flush_due: Option<tokio::time::Instant>,
    focused: bool,
    active: Option<Id>,
    notifier: Notifier,
    first_live: bool,
    stats: Stats,
}

impl Engine {
    fn new(cfg: EngineConfig, ui: UiTx) -> Engine {
        let store = Store::open(&cfg.data_dir.join("cache.db"));
        let rest = Arc::new(Rest::new(&cfg.endpoints.api));
        let (gw_events_tx, gw_events_rx) = mpsc::channel(4096);
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let notifier = Notifier::new(cfg.notifications, cfg.notify_all_dms);
        Engine {
            cfg,
            ui,
            store,
            rest,
            image_http: images::client(),
            image_sem: Arc::new(Semaphore::new(4)),
            images_inflight: HashSet::new(),
            gw_tx: None,
            gw_events_tx,
            gw_events_rx: Some(gw_events_rx),
            gw_stats: Arc::new(GatewayStats::default()),
            internal_tx,
            internal_rx: Some(internal_rx),
            token: None,
            me: None,
            guilds: HashMap::new(),
            dms: HashMap::new(),
            channel_guild: HashMap::new(),
            read: HashMap::new(),
            last_msg: HashMap::new(),
            synced: HashSet::new(),
            fetching: HashSet::new(),
            reached_start: HashSet::new(),
            pending_msgs: Vec::new(),
            pending_reads: HashMap::new(),
            pending_deletes: Vec::new(),
            inserts_since_prune: HashMap::new(),
            flush_due: None,
            focused: true,
            active: None,
            notifier,
            first_live: true,
            stats: Stats::default(),
        }
    }

    async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Command>) {
        self.startup();
        let mut gw_rx = self.gw_events_rx.take().expect("gateway receiver");
        let mut internal_rx = self.internal_rx.take().expect("internal receiver");
        loop {
            let flush_at = self.flush_due;
            tokio::select! {
                c = cmds.recv() => match c {
                    None | Some(Command::Shutdown) => break,
                    Some(c) => self.command(c),
                },
                Some(e) = gw_rx.recv() => {
                    self.gateway_event(e);
                    // Drain whatever else is already queued before yielding, so a burst becomes
                    // one batch of UI work instead of many wakeups.
                    for _ in 0..256 {
                        match gw_rx.try_recv() {
                            Ok(e) => self.gateway_event(e),
                            Err(_) => break,
                        }
                    }
                }
                Some(i) = internal_rx.recv() => self.internal(i),
                _ = async { tokio::time::sleep_until(flush_at.unwrap()).await }, if flush_at.is_some() => self.flush(),
            }
            if self.pending_msgs.len() >= FLUSH_BATCH {
                self.flush();
            }
        }
        self.flush();
        if let Some(gw) = self.gw_tx.take() {
            let _ = gw.send(GatewayCommand::Shutdown);
            // Give the close frame a moment to go out.
            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                while let Some(e) = gw_rx.recv().await {
                    if matches!(e, GatewayEvent::Status(ConnState::Offline)) {
                        break;
                    }
                }
            })
            .await;
        }
    }

    fn startup(&mut self) {
        if self.store.recovered {
            self.ui.send(Event::Toast("The local cache was damaged and has been rebuilt. Nothing else was affected.".into()));
        }
        let me: Option<User> = self.store.meta_get("self_user").and_then(|j| serde_json::from_str(&j).ok());
        let guilds = self.store.load_guilds().unwrap_or_default();
        let dms = self.store.load_dms().unwrap_or_default();
        self.read = self.store.read_states().unwrap_or_default();
        for g in &guilds {
            for c in &g.channels {
                self.channel_guild.insert(c.id, g.id);
                if let Some(l) = c.last_message_id {
                    self.last_msg.insert(c.id, l);
                }
            }
        }
        for d in &dms {
            if let Some(l) = d.last_message_id {
                self.last_msg.insert(d.id, l);
            }
            self.dms.insert(d.id, d.clone());
        }
        self.me = me.clone();
        self.ui.send(Event::Snapshot { user: me, guilds, dms, read: self.read.clone() });
        crate::diag::mark("engine_snapshot_sent");

        match creds::load() {
            LoadResult::Found(token) if me_is_known(&self.me) => {
                self.ui.send(Event::Session(Session::LoggingIn));
                self.start_gateway(token, None);
            }
            LoadResult::Found(token) => {
                // Token but no cached identity (e.g. cache was reset): validate it first.
                self.ui.send(Event::Session(Session::LoggingIn));
                self.command(Command::Login { token, remember: true });
            }
            LoadResult::NotFound => self.ui.send(Event::Session(Session::NeedsLogin { reason: None })),
            LoadResult::Unavailable(e) => {
                log::info!("credential store unavailable: {e}");
                self.ui.send(Event::Session(Session::NeedsLogin {
                    reason: Some("Your system keychain is unavailable, so the token can only be kept for this session.".into()),
                }))
            }
        }
    }

    fn start_gateway(&mut self, token: Secret, url: Option<String>) {
        if let Some(old) = self.gw_tx.take() {
            let _ = old.send(GatewayCommand::Shutdown);
        }
        self.rest.set_token(Some(token.clone()));
        self.token = Some(token.clone());
        let url = url.or_else(|| self.store.meta_get("gateway_url")).filter(|u| {
            // A cached URL is only trusted if it still matches the configured environment.
            u.starts_with("wss://") && !self.cfg.endpoints.is_override || self.cfg.endpoints.is_override && crate::config::is_loopback(u)
        });
        let cfg = GatewayConfig {
            url: url.unwrap_or_else(|| self.cfg.endpoints.gateway.clone()),
            token,
            intents: gateway::intents_for(&self.cfg.intents),
            compress: true,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        self.gw_tx = Some(tx);
        tokio::spawn(gateway::run(cfg, self.gw_events_tx.clone(), rx, self.gw_stats.clone()));
    }

    fn logged_in(&self) -> bool {
        self.token.is_some()
    }

    // ---------------------------------------------------------------- commands

    fn command(&mut self, c: Command) {
        match c {
            Command::Login { token, remember } => {
                self.ui.send(Event::Session(Session::LoggingIn));
                self.rest.set_token(Some(token.clone()));
                let rest = self.rest.clone();
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    let result = match rest.me().await {
                        Ok(u) => {
                            let url = rest.gateway_url().await.ok();
                            Ok((u, url))
                        }
                        Err(e) => Err(e),
                    };
                    let _ = tx.send(Internal::LoggedIn { token, remember, result });
                });
            }
            Command::Logout => self.logout(None),
            Command::Reconnect => {
                if let Some(t) = self.token.clone() {
                    self.start_gateway(t, None);
                }
            }
            Command::SetIntents(i) => {
                self.cfg.intents = i;
                if let Some(t) = self.token.clone() {
                    self.synced.clear();
                    self.start_gateway(t, None);
                }
            }
            Command::OpenChannel { channel } => self.open_channel(channel),
            Command::LoadOlder { channel, before } => self.load_older(channel, before),
            Command::LoadNewer { channel, after } => {
                self.flush();
                let msgs = self.db(|s| s.messages_after(channel, after, PAGE)).unwrap_or_default();
                let reached_end = (msgs.len() as u32) < PAGE;
                self.ui.send(Event::History { channel, kind: HistoryKind::Newer, messages: msgs, from_cache: true, reached_end });
            }
            Command::Send { channel, content, reply_to, nonce, files } => {
                if !self.logged_in() {
                    self.ui.send(Event::SendFailed { channel, nonce, error: "Not connected".into() });
                    return;
                }
                let rest = self.rest.clone();
                let ui = self.ui.clone();
                tokio::spawn(async move {
                    // The created message arrives through the gateway (MESSAGE_CREATE with our nonce);
                    // the REST response is only used for errors.
                    if let Err(e) = rest.send_message(channel, &content, reply_to, &nonce, &files).await {
                        ui.send(Event::SendFailed { channel, nonce, error: e.to_string() });
                    }
                });
            }
            Command::Edit { channel, message, content } => {
                let (rest, ui) = (self.rest.clone(), self.ui.clone());
                tokio::spawn(async move {
                    if let Err(e) = rest.edit_message(channel, message, &content).await {
                        ui.send(Event::Toast(format!("Could not edit message: {e}")));
                    }
                });
            }
            Command::Delete { channel, message } => {
                let (rest, ui) = (self.rest.clone(), self.ui.clone());
                tokio::spawn(async move {
                    if let Err(e) = rest.delete_message(channel, message).await {
                        ui.send(Event::Toast(format!("Could not delete message: {e}")));
                    }
                });
            }
            Command::React { channel, message, emoji, add } => {
                let (rest, ui) = (self.rest.clone(), self.ui.clone());
                tokio::spawn(async move {
                    if let Err(e) = rest.react(channel, message, &emoji.api_form(), add).await {
                        ui.send(Event::Toast(format!("Could not update reaction: {e}")));
                    }
                });
            }
            Command::OpenDm { user } => {
                let (rest, tx) = (self.rest.clone(), self.internal_tx.clone());
                tokio::spawn(async move {
                    let _ = tx.send(Internal::DmOpened(rest.open_dm(user).await));
                });
            }
            Command::MarkRead { channel, message } => {
                let cur = self.read.get(&channel).copied().unwrap_or_default();
                if message > cur.last_read || cur.mentions > 0 {
                    let st = ReadState { last_read: message.max(cur.last_read), mentions: 0 };
                    self.read.insert(channel, st);
                    self.pending_reads.insert(channel, st);
                    self.schedule_flush();
                    self.ui.send(Event::Read { channel, state: st });
                }
            }
            Command::LoadMembers { guild } => {
                if !self.cfg.intents.guild_members {
                    self.ui.send(Event::Members {
                        guild,
                        result: Err("The member list needs the Server Members intent. Enable it for your bot in the Developer Portal, then turn it on in Settings → Connection.".into()),
                    });
                    return;
                }
                let (rest, ui) = (self.rest.clone(), self.ui.clone());
                tokio::spawn(async move {
                    let result = rest.guild_members(guild, 1000).await.map_err(|e| e.to_string());
                    ui.send(Event::Members { guild, result });
                });
            }
            Command::Search { query, channel, seq } => {
                self.flush();
                let results = self.db(|s| s.search(&query, channel, 100)).unwrap_or_default();
                self.ui.send(Event::SearchResults { seq, results });
            }
            Command::FetchImage { url, max_w, max_h } => self.fetch_image(url, max_w, max_h),
            Command::Focus { focused, active_channel } => {
                self.focused = focused;
                self.active = active_channel;
            }
            Command::SetNotify { enabled, all_dms } => {
                self.notifier.enabled = enabled;
                self.notifier.all_dms = all_dms;
            }
            Command::SetDiskLimit(n) => self.cfg.disk_messages_per_channel = n.clamp(100, 50_000),
            Command::QueryStats => {
                let r = self.rest.stats();
                let g = &self.gw_stats;
                let mut s = self.stats.clone();
                s.rest_requests = r.requests;
                s.rest_rate_limited = r.rate_limited;
                s.rest_bytes = r.bytes_in;
                s.gw_bytes_wire = g.bytes_wire.load(Ordering::Relaxed);
                s.gw_bytes_json = g.bytes_json.load(Ordering::Relaxed);
                s.gw_dispatches = g.dispatches.load(Ordering::Relaxed);
                s.gw_heartbeats = g.heartbeats.load(Ordering::Relaxed);
                s.gw_reconnects = g.reconnects.load(Ordering::Relaxed);
                s.gw_resumes = g.resumes.load(Ordering::Relaxed);
                s.db_messages = self.store.total_messages();
                s.db_bytes = self.store.file_size();
                self.ui.send(Event::Stats(s));
            }
            Command::Shutdown => {}
        }
    }

    fn logout(&mut self, reason: Option<String>) {
        if let Some(gw) = self.gw_tx.take() {
            let _ = gw.send(GatewayCommand::Shutdown);
        }
        creds::clear();
        self.rest.set_token(None);
        self.token = None;
        self.me = None;
        self.guilds.clear();
        self.dms.clear();
        self.channel_guild.clear();
        self.read.clear();
        self.last_msg.clear();
        self.synced.clear();
        self.reached_start.clear();
        self.pending_msgs.clear();
        self.pending_reads.clear();
        self.pending_deletes.clear();
        let _ = self.store.clear_all();
        self.ui.send(Event::Snapshot { user: None, guilds: vec![], dms: vec![], read: HashMap::new() });
        self.ui.send(Event::Conn(ConnState::Offline));
        self.ui.send(Event::Session(Session::NeedsLogin { reason }));
    }

    fn db<T>(&mut self, f: impl FnOnce(&mut Store) -> rusqlite::Result<T>) -> Option<T> {
        match f(&mut self.store) {
            Ok(v) => Some(v),
            Err(e) if is_corruption(&e) => {
                log::warn!("cache corruption detected: {e}");
                let path = self.cfg.data_dir.join("cache.db");
                self.store = Store::open_in_memory();
                std::fs::rename(&path, path.with_extension("corrupt-runtime")).ok();
                self.store = Store::open(&path);
                self.ui.send(Event::Toast("The local cache was damaged and has been rebuilt.".into()));
                self.ui.send(Event::CacheReset);
                None
            }
            Err(e) => {
                log::warn!("cache error: {e}");
                None
            }
        }
    }

    fn open_channel(&mut self, channel: Id) {
        self.active = Some(channel);
        self.flush();
        let cached = self.db(|s| s.messages_before(channel, None, PAGE)).unwrap_or_default();
        let have_cache = !cached.is_empty();
        self.ui.send(Event::History {
            channel,
            kind: HistoryKind::Latest,
            messages: cached,
            from_cache: true,
            reached_end: self.reached_start.contains(&channel),
        });
        if self.logged_in() && !self.synced.contains(&channel) {
            self.fetch(channel, HistoryKind::Latest, None, PAGE);
        } else if !have_cache && !self.logged_in() {
            log::debug!("channel {channel} has no cached messages and we are offline");
        }
    }

    fn load_older(&mut self, channel: Id, before: Id) {
        self.flush();
        let msgs = self.db(|s| s.messages_before(channel, Some(before), PAGE)).unwrap_or_default();
        let n = msgs.len() as u32;
        if n > 0 {
            let oldest = msgs[0].id;
            let reached_end = n < PAGE && self.reached_start.contains(&channel);
            self.ui.send(Event::History { channel, kind: HistoryKind::Older, messages: msgs, from_cache: true, reached_end });
            if n < PAGE && self.logged_in() && !self.reached_start.contains(&channel) {
                self.fetch(channel, HistoryKind::Older, Some(oldest), PAGE);
            }
        } else if self.logged_in() && !self.reached_start.contains(&channel) {
            self.fetch(channel, HistoryKind::Older, Some(before), PAGE);
        } else {
            self.ui.send(Event::History { channel, kind: HistoryKind::Older, messages: vec![], from_cache: true, reached_end: true });
        }
    }

    fn fetch(&mut self, channel: Id, kind: HistoryKind, before: Option<Id>, limit: u32) {
        let key = (channel, kind as u8);
        if !self.fetching.insert(key) {
            return;
        }
        let (rest, tx) = (self.rest.clone(), self.internal_tx.clone());
        tokio::spawn(async move {
            let result = rest.messages(channel, before, None, limit as u8).await;
            let _ = tx.send(Internal::Fetched { channel, kind, result, requested: limit });
        });
    }

    fn fetch_image(&mut self, url: String, max_w: u32, max_h: u32) {
        if !self.cfg.endpoints.media_allowed(&url) {
            self.ui.send(Event::Image { url, result: Err("host not allowed".into()) });
            return;
        }
        if !self.images_inflight.insert(url.clone()) {
            return;
        }
        let (http, sem, ui, tx) = (self.image_http.clone(), self.image_sem.clone(), self.ui.clone(), self.internal_tx.clone());
        tokio::spawn(async move {
            let _permit = sem.acquire_owned().await;
            let result = match images::fetch(&http, &url).await {
                Ok(bytes) => {
                    let len = bytes.len();
                    match tokio::task::spawn_blocking(move || images::decode(&bytes, max_w, max_h)).await {
                        Ok(r) => r.map(|d| (d, len)),
                        Err(e) => Err(e.to_string()),
                    }
                }
                Err(e) => Err(e),
            };
            let _ = tx.send(Internal::ImageDone { url: url.clone() });
            match result {
                Ok((img, len)) => {
                    crate::diag::stat("image_bytes", len);
                    ui.send(Event::Image { url, result: Ok(img) })
                }
                Err(e) => ui.send(Event::Image { url, result: Err(e) }),
            }
        });
    }

    fn internal(&mut self, i: Internal) {
        match i {
            Internal::ImageDone { url } => {
                self.images_inflight.remove(&url);
                self.stats.images_fetched += 1;
            }
            Internal::Fetched { channel, kind, result, requested } => {
                self.fetching.remove(&(channel, kind as u8));
                match result {
                    Ok(mut msgs) => {
                        msgs.sort_by_key(|m| m.id);
                        let reached_end = (msgs.len() as u32) < requested;
                        if kind == HistoryKind::Latest {
                            self.synced.insert(channel);
                            // If the fresh page does not overlap the cache there is a gap in the
                            // cached history; drop the stale part rather than show a hole.
                            let newest_cached = self.db(|s| s.newest_id(channel)).flatten();
                            if let (Some(nc), Some(first)) = (newest_cached, msgs.first()) {
                                if nc < first.id && !reached_end {
                                    let _ = self.db(|s| s.prune_channel(channel, 0));
                                }
                            }
                            if let Some(last) = msgs.last() {
                                self.bump_last(channel, last.id);
                            }
                        }
                        if kind == HistoryKind::Older && reached_end || kind == HistoryKind::Latest && reached_end {
                            self.reached_start.insert(channel);
                        }
                        let _ = self.db(|s| s.insert_messages(&msgs));
                        self.ui.send(Event::History { channel, kind, messages: msgs, from_cache: false, reached_end });
                    }
                    Err(RestError::Unauthorized) => self.logout(Some("The bot token is no longer valid. Please log in again.".into())),
                    Err(e) => {
                        if kind == HistoryKind::Latest {
                            // Live events still arrive; don't hammer the endpoint on each switch.
                            self.synced.insert(channel);
                        }
                        let msg = match e {
                            RestError::Forbidden(_) => "Feather's bot cannot read this channel's history (needs Read Message History).".to_string(),
                            e => format!("Could not load messages: {e}"),
                        };
                        self.ui.send(Event::Toast(msg));
                        self.ui.send(Event::History { channel, kind, messages: vec![], from_cache: false, reached_end: kind == HistoryKind::Older });
                    }
                }
            }
            Internal::LoggedIn { token, remember, result } => match result {
                Ok((user, url)) if user.bot => {
                    let known = self.me.as_ref().map(|u| u.id);
                    if known.is_some_and(|k| k != user.id) {
                        // A different bot: never show the previous bot's cached data.
                        let _ = self.store.clear_all();
                        self.guilds.clear();
                        self.dms.clear();
                        self.read.clear();
                        self.ui.send(Event::CacheReset);
                        self.ui.send(Event::Snapshot { user: None, guilds: vec![], dms: vec![], read: HashMap::new() });
                    }
                    let persisted = if remember {
                        match creds::save(&token) {
                            Ok(p) => p,
                            Err(e) => {
                                self.ui.send(Event::Toast(format!("Could not save the token to the system keychain: {e}")));
                                false
                            }
                        }
                    } else {
                        creds::clear();
                        false
                    };
                    let _ = self.store.meta_set("self_user", &serde_json::to_string(&user).unwrap_or_default());
                    if let Some(u) = &url {
                        let _ = self.store.meta_set("gateway_url", u);
                    }
                    self.me = Some(user.clone());
                    self.ui.send(Event::Session(Session::LoggedIn { user, token_persisted: persisted }));
                    self.start_gateway(token, url);
                }
                Ok(_) => {
                    self.rest.set_token(None);
                    self.ui.send(Event::Session(Session::NeedsLogin {
                        reason: Some("That token belongs to a user account. Feather only supports bot tokens: using a personal account token in a third-party client breaks Discord's Terms of Service.".into()),
                    }));
                }
                Err(RestError::Unauthorized) => {
                    self.rest.set_token(None);
                    creds::clear();
                    self.ui.send(Event::Session(Session::NeedsLogin { reason: Some("Discord rejected that token. Check it in the Developer Portal (Bot → Reset Token).".into()) }));
                }
                Err(e) => {
                    self.rest.set_token(None);
                    self.ui.send(Event::Session(Session::NeedsLogin { reason: Some(format!("Could not reach Discord: {e}")) }));
                }
            },
            Internal::DmOpened(r) => match r {
                Ok(c) => {
                    let _ = self.store.save_dm(&c);
                    self.dms.insert(c.id, c.clone());
                    let id = c.id;
                    self.ui.send(Event::Dm(c));
                    self.ui.send(Event::OpenChannel(id));
                }
                Err(e) => self.ui.send(Event::Toast(format!("Could not open a DM: {e}"))),
            },
        }
    }

    // ---------------------------------------------------------------- gateway

    fn gateway_event(&mut self, e: GatewayEvent) {
        match e {
            GatewayEvent::Status(s) => {
                if s == ConnState::Connected {
                    crate::diag::mark("gateway_connected");
                }
                self.ui.send(Event::Conn(s));
            }
            GatewayEvent::Fatal(f) => {
                self.gw_tx = None;
                if f == gateway::Fatal::AuthenticationFailed {
                    self.logout(Some("Discord rejected the saved bot token. Please log in again.".into()));
                } else {
                    self.ui.send(Event::Toast(f.to_string()));
                }
            }
            GatewayEvent::Dispatch { t, d } => {
                if let Err(err) = self.dispatch(&t, &d) {
                    log::warn!("could not handle {t}: {err}");
                }
            }
        }
    }

    fn dispatch(&mut self, t: &str, d: &RawValue) -> Result<(), serde_json::Error> {
        match t {
            "READY" => {
                #[derive(Deserialize)]
                struct UG {
                    id: Id,
                }
                #[derive(Deserialize)]
                struct Ready {
                    user: User,
                    guilds: Vec<UG>,
                }
                let r: Ready = serde_json::from_str(d.get())?;
                self.synced.clear();
                let ids: HashSet<Id> = r.guilds.iter().map(|g| g.id).collect();
                // Guilds the bot left while offline.
                for gone in self.store.load_guilds().unwrap_or_default().into_iter().filter(|g| !ids.contains(&g.id)) {
                    let _ = self.store.remove_guild(gone.id);
                    self.ui.send(Event::GuildRemoved(gone.id));
                }
                let _ = self.store.meta_set("self_user", &serde_json::to_string(&r.user).unwrap_or_default());
                self.me = Some(r.user.clone());
                self.ui.send(Event::Session(Session::LoggedIn { user: r.user, token_persisted: matches!(creds::load(), LoadResult::Found(_)) }));
            }
            "GUILD_CREATE" => {
                let g: Guild = serde_json::from_str(d.get())?;
                if g.unavailable {
                    return Ok(());
                }
                self.upsert_guild(g);
            }
            "GUILD_UPDATE" => {
                let g: Guild = serde_json::from_str(d.get())?;
                if let Some(ctx) = self.guilds.get_mut(&g.id) {
                    ctx.info.name = g.name;
                    ctx.info.icon = g.icon;
                    ctx.owner = g.owner_id;
                    if !g.roles.is_empty() {
                        ctx.roles = g.roles;
                    }
                    self.recompute(g.id);
                }
            }
            "GUILD_DELETE" => {
                let g: Guild = serde_json::from_str(d.get())?;
                if !g.unavailable {
                    self.guilds.remove(&g.id);
                    let _ = self.store.remove_guild(g.id);
                    self.ui.send(Event::GuildRemoved(g.id));
                }
            }
            "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => {
                #[derive(Deserialize)]
                struct R {
                    guild_id: Id,
                    role: Role,
                }
                let r: R = serde_json::from_str(d.get())?;
                if let Some(ctx) = self.guilds.get_mut(&r.guild_id) {
                    ctx.roles.retain(|x| x.id != r.role.id);
                    ctx.roles.push(r.role);
                    self.recompute(r.guild_id);
                }
            }
            "GUILD_ROLE_DELETE" => {
                #[derive(Deserialize)]
                struct R {
                    guild_id: Id,
                    role_id: Id,
                }
                let r: R = serde_json::from_str(d.get())?;
                if let Some(ctx) = self.guilds.get_mut(&r.guild_id) {
                    ctx.roles.retain(|x| x.id != r.role_id);
                    ctx.info.my_roles.retain(|x| *x != r.role_id);
                    self.recompute(r.guild_id);
                }
            }
            "GUILD_MEMBER_UPDATE" => {
                #[derive(Deserialize)]
                struct M {
                    guild_id: Id,
                    user: User,
                    roles: Vec<Id>,
                }
                let m: M = serde_json::from_str(d.get())?;
                if self.me.as_ref().is_some_and(|u| u.id == m.user.id) {
                    if let Some(ctx) = self.guilds.get_mut(&m.guild_id) {
                        ctx.info.my_roles = m.roles;
                        self.recompute(m.guild_id);
                    }
                }
            }
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "THREAD_CREATE" | "THREAD_UPDATE" => {
                let c: Channel = serde_json::from_str(d.get())?;
                let is_thread = t.starts_with("THREAD");
                match c.guild_id {
                    Some(gid) => {
                        if let Some(ctx) = self.guilds.get_mut(&gid) {
                            let list = if is_thread { &mut ctx.threads } else { &mut ctx.all_channels };
                            list.retain(|x| x.id != c.id);
                            list.push(c);
                            self.recompute(gid);
                        }
                    }
                    None if c.is_dm() => self.upsert_dm(c),
                    None => {}
                }
            }
            "CHANNEL_DELETE" | "THREAD_DELETE" => {
                let c: Channel = serde_json::from_str(d.get())?;
                if let Some(gid) = c.guild_id {
                    if let Some(ctx) = self.guilds.get_mut(&gid) {
                        ctx.all_channels.retain(|x| x.id != c.id);
                        ctx.threads.retain(|x| x.id != c.id);
                        self.recompute(gid);
                    }
                }
            }
            "THREAD_LIST_SYNC" => {
                #[derive(Deserialize)]
                struct S {
                    guild_id: Id,
                    #[serde(default)]
                    threads: Vec<Channel>,
                }
                let s: S = serde_json::from_str(d.get())?;
                if let Some(ctx) = self.guilds.get_mut(&s.guild_id) {
                    for th in s.threads {
                        ctx.threads.retain(|x| x.id != th.id);
                        ctx.threads.push(th);
                    }
                    self.recompute(s.guild_id);
                }
            }
            "MESSAGE_CREATE" => {
                let m: Message = serde_json::from_str(d.get())?;
                self.on_message(m.compact());
            }
            "MESSAGE_UPDATE" => {
                let u: MessageUpdate = serde_json::from_str(d.get())?;
                if let Some(mut m) = self.pending_msgs.iter().rev().find(|m| m.id == u.id).cloned().or_else(|| self.db(|s| s.get_message(u.id)).flatten()) {
                    u.apply(&mut m);
                    self.queue_store(m);
                }
                self.ui.send(Event::MessageUpdated(u));
            }
            "MESSAGE_DELETE" => {
                #[derive(Deserialize)]
                struct D {
                    id: Id,
                    channel_id: Id,
                }
                let x: D = serde_json::from_str(d.get())?;
                self.delete_messages(x.channel_id, vec![x.id]);
            }
            "MESSAGE_DELETE_BULK" => {
                #[derive(Deserialize)]
                struct D {
                    ids: Vec<Id>,
                    channel_id: Id,
                }
                let x: D = serde_json::from_str(d.get())?;
                self.delete_messages(x.channel_id, x.ids);
            }
            "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" => {
                #[derive(Deserialize)]
                struct R {
                    user_id: Id,
                    channel_id: Id,
                    message_id: Id,
                    emoji: Emoji,
                }
                let r: R = serde_json::from_str(d.get())?;
                let add = t == "MESSAGE_REACTION_ADD";
                let me = self.me.as_ref().is_some_and(|u| u.id == r.user_id);
                self.update_cached(r.message_id, |m| apply_reaction(m, &r.emoji, add, me));
                self.ui.send(Event::Reaction { channel: r.channel_id, message: r.message_id, emoji: r.emoji, add, me });
            }
            "MESSAGE_REACTION_REMOVE_ALL" | "MESSAGE_REACTION_REMOVE_EMOJI" => {
                #[derive(Deserialize)]
                struct R {
                    channel_id: Id,
                    message_id: Id,
                    #[serde(default)]
                    emoji: Option<Emoji>,
                }
                let r: R = serde_json::from_str(d.get())?;
                let emoji = r.emoji.clone();
                self.update_cached(r.message_id, |m| match &emoji {
                    Some(e) => m.reactions.retain(|x| !x.emoji.same(e)),
                    None => m.reactions.clear(),
                });
                self.ui.send(Event::ReactionsCleared { channel: r.channel_id, message: r.message_id, emoji: r.emoji });
            }
            _ => {} // Not deserialised at all.
        }
        Ok(())
    }

    fn update_cached(&mut self, id: Id, f: impl FnOnce(&mut Message)) {
        if let Some(mut m) = self.pending_msgs.iter().rev().find(|m| m.id == id).cloned().or_else(|| self.db(|s| s.get_message(id)).flatten()) {
            f(&mut m);
            self.queue_store(m);
        }
    }

    fn delete_messages(&mut self, channel: Id, ids: Vec<Id>) {
        self.pending_msgs.retain(|m| !ids.contains(&m.id));
        self.pending_deletes.extend(ids.iter().copied());
        self.schedule_flush();
        self.ui.send(Event::MessagesDeleted { channel, ids });
    }

    fn upsert_guild(&mut self, mut g: Guild) {
        let Some(me) = self.me.as_ref().map(|u| u.id) else { return };
        let my_roles = g
            .members
            .iter()
            .find(|m| m.user.as_ref().is_some_and(|u| u.id == me))
            .map(|m| m.roles.clone())
            .unwrap_or_default();
        for c in g.channels.iter_mut().chain(g.threads.iter_mut()) {
            c.guild_id = Some(g.id);
        }
        let ctx = GuildCtx {
            info: GuildInfo { id: g.id, name: g.name, icon: g.icon, member_count: g.member_count, my_roles, ..Default::default() },
            owner: g.owner_id,
            roles: g.roles,
            all_channels: g.channels,
            threads: g.threads,
        };
        self.guilds.insert(g.id, ctx);
        self.recompute(g.id);
    }

    /// Recomputes visible channels for a guild, persists and publishes it.
    fn recompute(&mut self, gid: Id) {
        let Some(me) = self.me.as_ref().map(|u| u.id) else { return };
        let Some(ctx) = self.guilds.get_mut(&gid) else { return };
        let mut g = Guild {
            id: gid,
            owner_id: ctx.owner,
            roles: ctx.roles.clone(),
            channels: ctx.all_channels.clone(),
            threads: ctx.threads.clone(),
            members: vec![Member { user: Some(User { id: me, ..Default::default() }), nick: None, roles: ctx.info.my_roles.clone() }],
            ..Default::default()
        };
        permissions::annotate(&mut g, me);
        let mut visible: Vec<Channel> = g
            .channels
            .into_iter()
            .chain(g.threads)
            .filter(|c| c.kind == channel_type::GUILD_CATEGORY || (c.is_text_like() && permissions::has(c.my_permissions, VIEW_CHANNEL)))
            .collect();
        sort_channels(&mut visible);
        // Hide categories with no visible children.
        let parents: HashSet<Id> = visible.iter().filter_map(|c| c.parent_id).collect();
        visible.retain(|c| c.kind != channel_type::GUILD_CATEGORY || parents.contains(&c.id));
        let mut roles = ctx.roles.clone();
        roles.sort_by_key(|r| std::cmp::Reverse(r.position));
        ctx.info.role_colors = roles.iter().filter(|r| r.color != 0).map(|r| (r.id, r.color, r.position)).collect();
        ctx.info.channels = visible;
        let info = ctx.info.clone();
        let mut new_reads = Vec::new();
        for c in &info.channels {
            self.channel_guild.insert(c.id, gid);
            if let Some(l) = c.last_message_id {
                let cur = self.last_msg.entry(c.id).or_insert(l);
                *cur = (*cur).max(l);
                // First time we see a channel: treat existing history as read.
                if !self.read.contains_key(&c.id) {
                    let st = ReadState { last_read: l, mentions: 0 };
                    self.read.insert(c.id, st);
                    new_reads.push((c.id, st));
                }
            }
        }
        if !new_reads.is_empty() {
            let _ = self.db(|s| s.set_read_states(&new_reads));
            for (c, st) in new_reads {
                self.ui.send(Event::Read { channel: c, state: st });
            }
        }
        let _ = self.db(|s| s.save_guild(&info));
        self.ui.send(Event::Guild(info));
    }

    fn upsert_dm(&mut self, c: Channel) {
        let _ = self.db(|s| s.save_dm(&c));
        self.dms.insert(c.id, c.clone());
        self.ui.send(Event::Dm(c));
    }

    fn bump_last(&mut self, channel: Id, id: Id) -> bool {
        let e = self.last_msg.entry(channel).or_insert(Id(0));
        if id > *e {
            *e = id;
            true
        } else {
            false
        }
    }

    fn on_message(&mut self, m: Message) {
        let me = self.me.as_ref().map(|u| u.id).unwrap_or_default();
        let channel = m.channel_id;
        // DMs: bots do not receive CHANNEL_CREATE for DMs, so create the entry from the message.
        if m.guild_id.is_none() && !self.dms.contains_key(&channel) {
            let other = if m.author.id == me { m.mentions.first().cloned() } else { Some(m.author.clone()) };
            let c = Channel {
                id: channel,
                kind: channel_type::DM,
                recipients: other.into_iter().collect(),
                last_message_id: Some(m.id),
                ..Default::default()
            };
            self.upsert_dm(c);
        }
        let is_new = self.bump_last(channel, m.id);
        if is_new && m.author.id != me {
            let viewing = self.focused && self.active == Some(channel);
            let dm = m.guild_id.is_none();
            let my_roles = m.guild_id.and_then(|g| self.guilds.get(&g)).map(|g| g.info.my_roles.clone()).unwrap_or_default();
            let mentioned = m.mentions_user(me) || m.mention_roles.iter().any(|r| my_roles.contains(r));
            if mentioned || dm {
                if !viewing {
                    let st = self.read.entry(channel).or_default();
                    st.mentions += 1;
                    let st = *st;
                    self.pending_reads.insert(channel, st);
                    self.ui.send(Event::Read { channel, state: st });
                }
                if !viewing && (mentioned || self.notifier.all_dms) {
                    let place = match m.guild_id.and_then(|g| self.guilds.get(&g)) {
                        Some(g) => {
                            let ch = g.info.channels.iter().find(|c| c.id == channel).map(|c| c.label()).unwrap_or_default();
                            format!("{} (#{ch}, {})", m.author_name(), g.info.name)
                        }
                        None => m.author_name().to_string(),
                    };
                    let body = if m.content.is_empty() { "sent an attachment".to_string() } else { m.content.clone() };
                    self.notifier.show(&place, &body);
                }
            }
        }
        if self.first_live {
            self.first_live = false;
            crate::diag::mark("engine_first_live_message");
        }
        self.queue_store(m.clone());
        self.ui.send(Event::Message(m));
    }

    fn queue_store(&mut self, m: Message) {
        *self.inserts_since_prune.entry(m.channel_id).or_default() += 1;
        self.pending_msgs.push(m);
        self.schedule_flush();
    }

    fn schedule_flush(&mut self) {
        if self.flush_due.is_none() {
            self.flush_due = Some(tokio::time::Instant::now() + FLUSH_AFTER);
        }
    }

    /// Writes buffered messages/read states in one transaction each; prunes channels that grew.
    fn flush(&mut self) {
        self.flush_due = None;
        if self.pending_msgs.is_empty() && self.pending_reads.is_empty() && self.pending_deletes.is_empty() {
            return;
        }
        self.stats.db_flushes += 1;
        let msgs = std::mem::take(&mut self.pending_msgs);
        let _ = self.db(|s| s.insert_messages(&msgs));
        let dels = std::mem::take(&mut self.pending_deletes);
        if !dels.is_empty() {
            let _ = self.db(|s| s.delete_messages(&dels));
        }
        let reads: Vec<(Id, ReadState)> = self.pending_reads.drain().collect();
        if !reads.is_empty() {
            let _ = self.db(|s| s.set_read_states(&reads));
        }
        let limit = self.cfg.disk_messages_per_channel;
        let due: Vec<Id> = self.inserts_since_prune.iter().filter(|(_, n)| **n >= limit / 10).map(|(c, _)| *c).collect();
        for c in due {
            self.inserts_since_prune.remove(&c);
            let _ = self.db(|s| s.prune_channel(c, limit));
            self.reached_start.remove(&c);
        }
    }
}

fn me_is_known(me: &Option<User>) -> bool {
    me.as_ref().is_some_and(|u| u.bot && u.id.is_set())
}

pub fn apply_reaction(m: &mut Message, emoji: &Emoji, add: bool, me: bool) {
    match m.reactions.iter_mut().position(|r| r.emoji.same(emoji)) {
        Some(i) => {
            let r = &mut m.reactions[i];
            if add {
                r.count += 1;
                r.me |= me;
            } else {
                r.count = r.count.saturating_sub(1);
                if me {
                    r.me = false;
                }
                if r.count == 0 {
                    m.reactions.remove(i);
                }
            }
        }
        None if add => m.reactions.push(Reaction { count: 1, me, emoji: emoji.clone() }),
        None => {}
    }
}

/// Discord's sidebar order: uncategorised first, then categories by position, children by
/// position (text before voice), threads under their parent.
pub fn sort_channels(v: &mut [Channel]) {
    use channel_type::*;
    struct Info {
        kind: u8,
        position: i32,
        parent: Option<Id>,
    }
    let info: HashMap<Id, Info> = v.iter().map(|c| (c.id, Info { kind: c.kind, position: c.position, parent: c.parent_id })).collect();
    let is_thread = |k: u8| matches!(k, PUBLIC_THREAD | PRIVATE_THREAD | ANNOUNCEMENT_THREAD);
    let key = |c: &Channel| -> (i32, Id, u8, u8, i32, Id, u8, Id) {
        if c.kind == GUILD_CATEGORY {
            return (c.position, c.id, 0, 0, 0, Id(0), 0, Id(0));
        }
        // Threads sort directly after their parent channel.
        let (base_id, thread) = match c.parent_id.filter(|_| is_thread(c.kind)) {
            Some(p) if info.contains_key(&p) => (p, 1),
            _ => (c.id, 0),
        };
        let b = &info[&base_id];
        let (cat_pos, cat_id) = b.parent.and_then(|p| info.get(&p).map(|i| (i.position, p))).unwrap_or((-1, Id(0)));
        let voice = matches!(b.kind, GUILD_VOICE | GUILD_STAGE_VOICE) as u8;
        (cat_pos, cat_id, 1, voice, b.position, base_id, thread, c.id)
    };
    v.sort_by_cached_key(key);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(id: u64, kind: u8, pos: i32, parent: Option<u64>) -> Channel {
        Channel { id: Id(id), kind, position: pos, parent_id: parent.map(Id), name: Some(format!("c{id}")), ..Default::default() }
    }

    #[test]
    fn sidebar_order() {
        use channel_type::*;
        let mut v = vec![
            ch(10, GUILD_CATEGORY, 1, None),
            ch(11, GUILD_TEXT, 0, Some(10)),
            ch(12, GUILD_VOICE, 0, Some(10)),
            ch(20, GUILD_CATEGORY, 0, None),
            ch(21, GUILD_TEXT, 5, Some(20)),
            ch(22, GUILD_TEXT, 1, Some(20)),
            ch(1, GUILD_TEXT, 9, None),
            ch(30, PUBLIC_THREAD, 0, Some(22)),
        ];
        sort_channels(&mut v);
        let ids: Vec<u64> = v.iter().map(|c| c.id.0).collect();
        assert_eq!(ids, vec![1, 20, 22, 30, 21, 10, 11, 12]);
    }

    #[test]
    fn reactions_apply() {
        let mut m = Message::default();
        let e = Emoji { name: Some("👍".into()), ..Default::default() };
        apply_reaction(&mut m, &e, true, false);
        apply_reaction(&mut m, &e, true, true);
        assert_eq!((m.reactions[0].count, m.reactions[0].me), (2, true));
        apply_reaction(&mut m, &e, false, true);
        assert_eq!((m.reactions[0].count, m.reactions[0].me), (1, false));
        apply_reaction(&mut m, &e, false, false);
        assert!(m.reactions.is_empty());
    }
}
