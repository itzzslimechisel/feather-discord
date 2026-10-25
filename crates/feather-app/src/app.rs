//! Application state and the event/command plumbing between the UI and the engine.

use crate::imgcache::ImageCache;
use crate::rows::{self, FontSet, Row};
use crate::theme;
use eframe::egui;
use feather_core::config::Endpoints;
use feather_core::diag;
use feather_core::engine::{Command, EngineHandle, Event, HistoryKind, Session};
use feather_core::gateway::ConnState;
use feather_core::model::*;
use feather_core::permissions;
use feather_core::settings::{Settings, Theme};
use feather_core::store::ReadState;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Maximum messages held in memory for the open channel.
pub const MAX_ROWS: usize = 400;
pub const IMAGE_BUDGET: usize = 48 << 20;

pub struct ChannelView {
    pub channel: Id,
    pub rows: Vec<Row>,
    pub reached_start: bool,
    pub has_newer: bool,
    pub loading_older: bool,
    pub loading_newer: bool,
    pub got_network: bool,
    pub snap_bottom: bool,
    pub at_bottom: bool,
    pub anchor: Option<(Id, f32)>,
    pub restore_anchor: bool,
    pub marked: Id,
    pub revealed: HashSet<Id>,
    pub scroll_y: f32,
    pub tops: Vec<f32>,
    pub layout_gen: u64,
    pub layout_width: f32,
    pub jump_to: Option<Id>,
}

impl ChannelView {
    pub fn new(channel: Id) -> ChannelView {
        ChannelView {
            channel,
            rows: Vec::new(),
            reached_start: false,
            has_newer: false,
            loading_older: false,
            loading_newer: false,
            got_network: false,
            snap_bottom: true,
            at_bottom: true,
            anchor: None,
            restore_anchor: false,
            marked: Id(0),
            revealed: HashSet::new(),
            scroll_y: 0.0,
            tops: Vec::new(),
            layout_gen: 0,
            layout_width: 0.0,
            jump_to: None,
        }
    }
}

pub struct Toast {
    pub text: String,
    pub until: Instant,
}

#[derive(Default)]
pub struct LoginForm {
    pub token: String,
    pub remember: bool,
    pub error: Option<String>,
    pub busy: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsTab {
    Appearance,
    Connection,
    Notifications,
    Storage,
    About,
}

pub struct App {
    pub ctx: egui::Context,
    pub engine: Option<EngineHandle>,
    pub endpoints: Endpoints,
    pub settings: Settings,
    pub settings_path: PathBuf,
    pub settings_dirty: Option<Instant>,
    pub recovered_settings: bool,
    pub theme_applied: Option<Theme>,
    pub theme_gen: u64,
    pub palette: theme::Palette,
    pub fonts: FontSet,

    pub session: Session,
    pub conn: ConnState,
    pub me: Option<User>,
    pub guilds: Vec<GuildInfo>,
    pub dms: Vec<Channel>,
    pub read: HashMap<Id, ReadState>,
    pub last_msg: HashMap<Id, Id>,
    pub sel_guild: Option<Id>,
    pub view: Option<ChannelView>,
    pub members: HashMap<Id, Result<Vec<Member>, String>>,
    pub collapsed: HashSet<Id>,
    pub last_in_guild: HashMap<Option<Id>, Id>,

    pub login: LoginForm,
    pub toasts: Vec<Toast>,
    pub images: ImageCache,

    pub composer: String,
    pub composer_focus: bool,
    pub reply_to: Option<(Id, String)>,
    pub editing: Option<Id>,
    pub attachments: Vec<(String, Vec<u8>)>,
    pub nonce_counter: u64,

    pub settings_open: bool,
    pub settings_tab: SettingsTab,
    pub theme_json: String,
    pub theme_json_error: Option<String>,
    pub switcher: Option<(String, usize)>,
    pub search_open: bool,
    pub search_query: String,
    pub search_seq: u64,
    pub search_results: Vec<Message>,
    pub pending_link: Option<String>,
    pub ctx_menu: Option<(Id, egui::Pos2)>,
    pub show_stats: bool,
    pub stats: Option<feather_core::engine::Stats>,

    pub focused: bool,
    pub first_frame_done: bool,
    pub frames: u64,
    pub connected_once: bool,
    pub live_seen: bool,
    pub window_size: Option<egui::Vec2>,
    pub bench: crate::bench::Bench,
}

impl App {
    pub fn send(&self, c: Command) {
        if let Some(e) = &self.engine {
            e.send(c);
        }
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::info!("toast: {text}");
        self.toasts.push(Toast { text, until: Instant::now() + Duration::from_secs(6) });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
        self.ctx.request_repaint_after(Duration::from_secs(6));
    }

    pub fn mark_settings_dirty(&mut self) {
        self.settings_dirty = Some(Instant::now());
        self.ctx.request_repaint_after(Duration::from_millis(900));
    }

    pub fn bump_layout(&mut self) {
        self.theme_gen += 1;
    }

    pub fn my_id(&self) -> Id {
        self.me.as_ref().map(|u| u.id).unwrap_or_default()
    }

    pub fn guild(&self, id: Id) -> Option<&GuildInfo> {
        self.guilds.iter().find(|g| g.id == id)
    }

    pub fn find_channel(&self, id: Id) -> Option<(&Channel, Option<&GuildInfo>)> {
        for g in &self.guilds {
            if let Some(c) = g.channels.iter().find(|c| c.id == id) {
                return Some((c, Some(g)));
            }
        }
        self.dms.iter().find(|c| c.id == id).map(|c| (c, None))
    }

    pub fn is_unread(&self, ch: Id) -> bool {
        match (self.last_msg.get(&ch), self.read.get(&ch)) {
            (Some(last), Some(r)) => *last > r.last_read,
            _ => false,
        }
    }

    pub fn mentions(&self, ch: Id) -> u32 {
        self.read.get(&ch).map(|r| r.mentions).unwrap_or(0)
    }

    pub fn guild_unread(&self, g: &GuildInfo) -> (bool, u32) {
        let mut unread = false;
        let mut m = 0;
        for c in &g.channels {
            if c.kind != channel_type::GUILD_CATEGORY {
                unread |= self.is_unread(c.id);
                m += self.mentions(c.id);
            }
        }
        (unread, m)
    }

    pub fn can_send(&self, ch: Id) -> bool {
        match self.find_channel(ch) {
            Some((c, Some(_))) => permissions::has(c.my_permissions, permissions::SEND_MESSAGES) || c.my_permissions == permissions::ALL,
            Some((_, None)) => true,
            None => false,
        }
    }

    pub fn perms(&self, ch: Id) -> u64 {
        self.find_channel(ch).map(|(c, g)| if g.is_some() { c.my_permissions } else { permissions::ALL }).unwrap_or(0)
    }

    // ------------------------------------------------------------ navigation

    pub fn open_channel(&mut self, ch: Id) {
        if self.view.as_ref().is_some_and(|v| v.channel == ch) {
            return;
        }
        self.view = Some(ChannelView::new(ch));
        self.reply_to = None;
        self.editing = None;
        self.composer_focus = true;
        if let Some((c, g)) = self.find_channel(ch) {
            let gid = g.map(|g| g.id);
            let _ = c;
            self.sel_guild = gid;
        }
        self.last_in_guild.insert(self.sel_guild, ch);
        self.settings.last_channel = Some(ch);
        self.mark_settings_dirty();
        self.send(Command::OpenChannel { channel: ch });
        self.send(Command::Focus { focused: self.focused, active_channel: Some(ch) });
        if let Some(g) = self.sel_guild {
            if self.settings.intents.guild_members && self.settings.show_member_list && !self.members.contains_key(&g) {
                self.send(Command::LoadMembers { guild: g });
            }
        }
    }

    /// Flat list of channels in sidebar order: used for Alt+Up/Down and the quick switcher.
    pub fn nav_list(&self) -> Vec<Id> {
        match self.sel_guild.and_then(|g| self.guild(g)) {
            Some(g) => g.channels.iter().filter(|c| c.kind != channel_type::GUILD_CATEGORY).map(|c| c.id).collect(),
            None => self.dm_order().iter().map(|c| c.id).collect(),
        }
    }

    pub fn dm_order(&self) -> Vec<&Channel> {
        let mut v: Vec<&Channel> = self.dms.iter().collect();
        v.sort_by_key(|c| std::cmp::Reverse(self.last_msg.get(&c.id).copied().unwrap_or_default()));
        v
    }

    // ------------------------------------------------------------ events

    pub fn pump_events(&mut self) {
        let mut n = 0;
        loop {
            let ev = match self.engine.as_ref().map(|e| e.rx.try_recv()) {
                Some(Ok(ev)) => ev,
                _ => break,
            };
            self.handle(ev);
            n += 1;
            if n >= 512 {
                self.ctx.request_repaint();
                break;
            }
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Snapshot { user, guilds, dms, read } => {
                self.me = user;
                self.guilds = guilds;
                self.guilds.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                self.dms = dms;
                self.read = read;
                self.last_msg.clear();
                for g in &self.guilds {
                    for c in &g.channels {
                        if let Some(l) = c.last_message_id {
                            self.last_msg.insert(c.id, l);
                        }
                    }
                }
                for d in &self.dms {
                    if let Some(l) = d.last_message_id {
                        self.last_msg.insert(d.id, l);
                    }
                }
                if self.guilds.is_empty() && self.dms.is_empty() {
                    self.view = None;
                    self.sel_guild = None;
                }
                diag::mark("ui_snapshot_applied");
                self.restore_last_channel();
            }
            Event::Session(s) => {
                if let Session::LoggedIn { user, .. } = &s {
                    self.me = Some(user.clone());
                    self.login = LoginForm { remember: true, ..Default::default() };
                }
                if let Session::NeedsLogin { reason } = &s {
                    self.login.busy = false;
                    self.login.error = reason.clone();
                }
                self.session = s;
            }
            Event::Conn(c) => {
                if c == ConnState::Connected {
                    self.connected_once = true;
                    diag::mark("ui_gateway_connected");
                }
                self.conn = c;
            }
            Event::Guild(g) => {
                for c in &g.channels {
                    if let Some(l) = c.last_message_id {
                        let e = self.last_msg.entry(c.id).or_insert(l);
                        *e = (*e).max(l);
                    }
                }
                match self.guilds.iter_mut().find(|x| x.id == g.id) {
                    Some(x) => *x = g,
                    None => {
                        self.guilds.push(g);
                        self.guilds.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                    }
                }
                self.restore_last_channel();
                self.bump_layout();
            }
            Event::GuildRemoved(id) => {
                self.guilds.retain(|g| g.id != id);
                if self.sel_guild == Some(id) {
                    self.sel_guild = None;
                    self.view = None;
                }
            }
            Event::Dm(c) => {
                if let Some(l) = c.last_message_id {
                    let e = self.last_msg.entry(c.id).or_insert(l);
                    *e = (*e).max(l);
                }
                match self.dms.iter_mut().find(|x| x.id == c.id) {
                    Some(x) => *x = c,
                    None => self.dms.push(c),
                }
            }
            Event::Message(m) => self.on_message(m),
            Event::MessageUpdated(u) => {
                if let Some(v) = self.view.as_mut().filter(|v| v.channel == u.channel_id) {
                    if let Some(r) = v.rows.iter_mut().find(|r| r.msg.id == u.id) {
                        u.apply(&mut r.msg);
                        r.refresh();
                    }
                }
            }
            Event::MessagesDeleted { channel, ids } => {
                if let Some(v) = self.view.as_mut().filter(|v| v.channel == channel) {
                    v.rows.retain(|r| !ids.contains(&r.msg.id));
                    rows::regroup(&mut v.rows, 0);
                    v.layout_gen += 1;
                }
            }
            Event::Reaction { channel, message, emoji, add, me } => {
                if let Some(v) = self.view.as_mut().filter(|v| v.channel == channel) {
                    if let Some(r) = v.rows.iter_mut().find(|r| r.msg.id == message) {
                        feather_core::engine::apply_reaction(&mut r.msg, &emoji, add, me);
                        r.height = None;
                        r.built = None;
                    }
                }
            }
            Event::ReactionsCleared { channel, message, emoji } => {
                if let Some(v) = self.view.as_mut().filter(|v| v.channel == channel) {
                    if let Some(r) = v.rows.iter_mut().find(|r| r.msg.id == message) {
                        match &emoji {
                            Some(e) => r.msg.reactions.retain(|x| !x.emoji.same(e)),
                            None => r.msg.reactions.clear(),
                        }
                        r.height = None;
                        r.built = None;
                    }
                }
            }
            Event::History { channel, kind, messages, from_cache, reached_end } => self.on_history(channel, kind, messages, from_cache, reached_end),
            Event::Read { channel, state } => {
                self.read.insert(channel, state);
            }
            Event::SendFailed { channel: _, nonce: _, error } => {
                self.toast(format!("Message not sent: {error}"));
            }
            Event::Members { guild, result } => {
                self.members.insert(guild, result);
            }
            Event::SearchResults { seq, results } => {
                if seq == self.search_seq {
                    self.search_results = results;
                }
            }
            Event::Image { url, result } => {
                let ctx = self.ctx.clone();
                self.images.insert(&ctx, url, result);
                if let Some(v) = self.view.as_mut() {
                    // Image sizes are known from message metadata, so layout does not change.
                    let _ = v;
                }
            }
            Event::OpenChannel(id) => {
                self.sel_guild = None;
                self.open_channel(id);
            }
            Event::Toast(t) => self.toast(t),
            Event::CacheReset => {
                self.view = None;
                self.images.clear();
            }
            Event::Stats(s) => self.stats = Some(s),
        }
    }

    fn restore_last_channel(&mut self) {
        if self.view.is_some() {
            return;
        }
        if let Some(ch) = self.settings.last_channel {
            if self.find_channel(ch).is_some() {
                self.open_channel(ch);
                return;
            }
        }
        // First available text channel.
        if let Some(g) = self.guilds.first() {
            if let Some(c) = g.channels.iter().find(|c| c.kind != channel_type::GUILD_CATEGORY) {
                let id = c.id;
                self.sel_guild = Some(g.id);
                self.open_channel(id);
            }
        }
    }

    fn on_message(&mut self, m: Message) {
        if self.connected_once && !self.live_seen {
            self.live_seen = true;
            diag::mark("ui_first_live_message");
        }
        let ch = m.channel_id;
        let e = self.last_msg.entry(ch).or_insert(m.id);
        *e = (*e).max(m.id);
        let focused = self.focused;
        let Some(v) = self.view.as_mut().filter(|v| v.channel == ch) else { return };
        if v.has_newer {
            return; // a gap exists; the missing range is fetched when scrolling down
        }
        if v.rows.iter().any(|r| r.msg.id == m.id) {
            return;
        }
        let was_bottom = v.at_bottom;
        let idx = v.rows.len();
        v.rows.push(Row::new(m));
        rows::regroup(&mut v.rows, idx);
        if was_bottom && v.rows.len() > MAX_ROWS {
            let drop = v.rows.len() - MAX_ROWS;
            v.rows.drain(..drop);
            v.reached_start = false;
            rows::regroup(&mut v.rows, 0);
            v.layout_gen += 1;
        }
        if was_bottom {
            v.snap_bottom = true;
        }
        let _ = focused;
    }

    fn on_history(&mut self, channel: Id, kind: HistoryKind, messages: Vec<Message>, from_cache: bool, reached_end: bool) {
        let Some(v) = self.view.as_mut().filter(|v| v.channel == channel) else { return };
        let nonempty = !messages.is_empty();
        match kind {
            HistoryKind::Latest => {
                if from_cache {
                    if v.rows.is_empty() {
                        v.rows = messages.into_iter().map(Row::new).collect();
                        v.reached_start = reached_end;
                        v.snap_bottom = true;
                        rows::regroup(&mut v.rows, 0);
                        if nonempty {
                            diag::mark("ui_cached_content");
                        }
                    }
                } else {
                    v.got_network = true;
                    let empty_before = v.rows.is_empty();
                    merge(&mut v.rows, messages);
                    if empty_before {
                        v.snap_bottom = true;
                    }
                    if reached_end {
                        v.reached_start = true;
                    }
                    if empty_before {
                        diag::mark("ui_network_content");
                    }
                }
            }
            HistoryKind::Older => {
                v.loading_older = false;
                if nonempty {
                    v.restore_anchor = true;
                    merge(&mut v.rows, messages);
                }
                if reached_end {
                    v.reached_start = true;
                }
                if v.rows.len() > MAX_ROWS {
                    let drop = v.rows.len() - MAX_ROWS;
                    v.rows.truncate(MAX_ROWS);
                    v.has_newer = true;
                    let _ = drop;
                }
            }
            HistoryKind::Newer => {
                v.loading_newer = false;
                merge(&mut v.rows, messages);
                if reached_end {
                    v.has_newer = false;
                }
                if v.rows.len() > MAX_ROWS {
                    let drop = v.rows.len() - MAX_ROWS;
                    v.restore_anchor = true;
                    v.rows.drain(..drop);
                    v.reached_start = false;
                }
            }
        }
        let n = v.rows.len();
        rows::regroup(&mut v.rows, 0);
        v.layout_gen += 1;
        let _ = n;
    }
}

/// Inserts messages into a sorted row list, replacing duplicates.
pub fn merge(rows: &mut Vec<Row>, msgs: Vec<Message>) {
    if msgs.is_empty() {
        return;
    }
    let mut incoming: Vec<Message> = msgs;
    incoming.sort_by_key(|m| m.id);
    incoming.dedup_by_key(|m| m.id);
    let mut out: Vec<Row> = Vec::with_capacity(rows.len() + incoming.len());
    let mut old = std::mem::take(rows).into_iter().peekable();
    let mut new = incoming.into_iter().peekable();
    loop {
        match (old.peek(), new.peek()) {
            (Some(a), Some(b)) if a.msg.id == b.id => {
                let b = new.next().unwrap();
                let mut a = old.next().unwrap();
                if a.msg != b {
                    a.msg = b;
                    a.refresh();
                }
                out.push(a);
            }
            (Some(a), Some(b)) if a.msg.id < b.id => out.push(old.next().unwrap()),
            (Some(_), Some(_)) => out.push(Row::new(new.next().unwrap())),
            (Some(_), None) => out.push(old.next().unwrap()),
            (None, Some(_)) => out.push(Row::new(new.next().unwrap())),
            (None, None) => break,
        }
    }
    *rows = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: u64, c: &str) -> Message {
        Message { id: Id(id), content: c.into(), ..Default::default() }
    }

    #[test]
    fn merge_sorts_dedups_and_updates() {
        let mut rows: Vec<Row> = vec![Row::new(m(2, "b")), Row::new(m(4, "d"))];
        merge(&mut rows, vec![m(3, "c"), m(1, "a"), m(4, "D"), m(3, "c")]);
        let ids: Vec<u64> = rows.iter().map(|r| r.msg.id.0).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        assert_eq!(rows[3].msg.content, "D");
    }
}
