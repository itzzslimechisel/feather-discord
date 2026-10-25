//! All drawing: layout, panels, the virtualised message list and dialogs.

use crate::app::*;
use crate::imgcache::Lookup;
use crate::rows::{self, Env, Item, Link};
use crate::theme::{self, Palette};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Frame, Key, Margin, Modifiers, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, StrokeKind,
    TextEdit, Vec2, WidgetText,
};
use feather_core::config::Endpoints;
use feather_core::creds;
use feather_core::engine::{Command, EngineHandle, Session, Stats};
use feather_core::gateway::ConnState;
use feather_core::model::*;
use feather_core::permissions;
use feather_core::settings::{Background, Base, Density, Rgb, Settings, Theme};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub fn new_app(
    ctx: egui::Context,
    engine: EngineHandle,
    endpoints: Endpoints,
    settings: Settings,
    settings_path: PathBuf,
    recovered_settings: bool,
) -> App {
    let palette = theme::palette(&settings.theme);
    theme::apply(&ctx, &settings.theme);
    let applied = settings.theme.clone();
    let mut images = crate::imgcache::ImageCache::new(IMAGE_BUDGET);
    images.enabled = settings.image_previews;
    let mut app = App {
        ctx,
        engine: Some(engine),
        endpoints,
        settings,
        settings_path,
        settings_dirty: None,
        recovered_settings,
        theme_applied: Some(applied),
        theme_gen: 1,
        palette,
        fonts: Default::default(),
        session: Session::LoggingIn,
        conn: ConnState::Offline,
        me: None,
        guilds: Vec::new(),
        dms: Vec::new(),
        read: HashMap::new(),
        last_msg: HashMap::new(),
        sel_guild: None,
        view: None,
        members: HashMap::new(),
        collapsed: HashSet::new(),
        last_in_guild: HashMap::new(),
        login: LoginForm { remember: true, ..Default::default() },
        toasts: Vec::new(),
        images,
        composer: String::new(),
        composer_focus: false,
        reply_to: None,
        editing: None,
        attachments: Vec::new(),
        nonce_counter: 0,
        settings_open: false,
        settings_tab: SettingsTab::Appearance,
        theme_json: String::new(),
        theme_json_error: None,
        switcher: None,
        search_open: false,
        search_query: String::new(),
        search_seq: 0,
        search_results: Vec::new(),
        pending_link: None,
        ctx_menu: None,
        show_stats: false,
        stats: None,
        focused: true,
        first_frame_done: false,
        frames: 0,
        connected_once: false,
        live_seen: false,
        window_size: None,
        bench: crate::bench::Bench::from_env(),
    };
    if app.recovered_settings {
        app.toast("Your settings file was unreadable, so defaults were restored. The old file was kept as settings.json.bad.");
    }
    // Benchmarks and tests can sign in to a loopback mock without typing a token.
    if app.endpoints.is_override {
        if let Ok(t) = std::env::var("FEATHER_DEV_TOKEN") {
            if let Some(token) = creds::normalise(&t) {
                app.send(Command::Login { token, remember: false });
            }
        }
    }
    app
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let t0 = Instant::now();
        self.pump_events();
        self.sync_theme(ctx);
        self.track_window(ctx);
        self.shortcuts(ctx);
        self.bench_driver(ctx);
        self.draw(ctx);
        self.housekeeping(ctx);
        self.frames += 1;
        if self.frames == 1 {
            feather_core::diag::mark("ui_first_frame");
        }
        if self.frames == 2 {
            feather_core::diag::mark("ui_interactive");
        }
        self.bench.frame(t0.elapsed().as_secs_f32() * 1000.0);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_settings_now();
        if let Some(e) = self.engine.take() {
            e.shutdown(Duration::from_secs(2));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// frame bookkeeping

impl App {
    fn sync_theme(&mut self, ctx: &egui::Context) {
        if self.theme_applied.as_ref() != Some(&self.settings.theme) {
            let old = self.theme_applied.take();
            theme::apply(ctx, &self.settings.theme);
            self.palette = theme::palette(&self.settings.theme);
            self.theme_applied = Some(self.settings.theme.clone());
            // Only layout-affecting tokens invalidate cached row heights.
            let t = &self.settings.theme;
            let layout_changed = old.map_or(true, |o| {
                o.font_size != t.font_size || o.density != t.density || o.message_spacing != t.message_spacing || o.show_avatars != t.show_avatars
            });
            if layout_changed {
                self.bump_layout();
            }
        }
        self.images.enabled = self.settings.image_previews;
    }

    fn track_window(&mut self, ctx: &egui::Context) {
        let (focused, rect) = ctx.input(|i| (i.viewport().focused.unwrap_or(true), i.viewport().inner_rect));
        if let Some(r) = rect {
            self.window_size = Some(r.size());
        }
        if focused != self.focused {
            self.focused = focused;
            let active = self.view.as_ref().map(|v| v.channel);
            self.send(Command::Focus { focused, active_channel: active });
            if let Some(v) = self.view.as_mut() {
                v.marked = Id(0); // re-mark when focus returns
            }
        }
    }

    fn housekeeping(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.toasts.retain(|t| t.until > now);
        if let Some(t) = self.settings_dirty {
            if now.duration_since(t) > Duration::from_millis(800) {
                self.save_settings_now();
            }
        }
        if self.frames == 3 && !self.first_frame_done {
            self.first_frame_done = true;
            self.fonts.bold = crate::fonts::install_system_fonts(ctx, self.settings.wide_fonts);
            self.bump_layout();
            ctx.request_repaint();
        }
    }

    fn save_settings_now(&mut self) {
        self.settings_dirty = None;
        if let Some(s) = self.window_size {
            self.settings.window_size = Some((s.x, s.y));
        }
        self.settings.sanitize();
        if let Err(e) = self.settings.save(&self.settings_path) {
            log::warn!("could not save settings: {e}");
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        use egui::KeyboardShortcut as K;
        let cmd = Modifiers::COMMAND;
        if ctx.input_mut(|i| i.consume_shortcut(&K::new(cmd, Key::K))) {
            self.switcher = if self.switcher.is_some() { None } else { Some((String::new(), 0)) };
        }
        if ctx.input_mut(|i| i.consume_shortcut(&K::new(cmd, Key::Comma))) {
            self.settings_open = !self.settings_open;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&K::new(cmd, Key::F))) {
            self.search_open = !self.search_open;
        }
        let alt = Modifiers::ALT;
        let dir = if ctx.input_mut(|i| i.consume_shortcut(&K::new(alt, Key::ArrowUp))) {
            -1
        } else if ctx.input_mut(|i| i.consume_shortcut(&K::new(alt, Key::ArrowDown))) {
            1
        } else {
            0
        };
        if dir != 0 {
            let list = self.nav_list();
            if let Some(cur) = self.view.as_ref().and_then(|v| list.iter().position(|c| *c == v.channel)) {
                let next = (cur as i32 + dir).rem_euclid(list.len() as i32) as usize;
                self.open_channel(list[next]);
            }
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            if self.ctx_menu.take().is_some() || self.pending_link.take().is_some() {
            } else if self.switcher.take().is_some() {
            } else if self.settings_open {
                self.settings_open = false;
            } else if self.search_open {
                self.search_open = false;
            } else if self.reply_to.take().is_some() {
            } else if self.editing.take().is_some() {
                self.composer.clear();
            }
        }
        // Files dropped onto the window become attachments.
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            let Some(path) = f.path else { continue };
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
            match std::fs::metadata(&path) {
                Ok(m) if m.len() as usize > feather_core::rest::MAX_UPLOAD_BYTES => {
                    self.toast(format!("{name} is larger than 10 MiB and can't be attached."));
                }
                Ok(_) => match std::fs::read(&path) {
                    Ok(bytes) if self.attachments.len() < 10 => self.attachments.push((name, bytes)),
                    Ok(_) => self.toast("At most 10 attachments per message."),
                    Err(e) => self.toast(format!("Could not read {name}: {e}")),
                },
                Err(e) => self.toast(format!("Could not read {name}: {e}")),
            }
        }
    }

    fn bench_driver(&mut self, ctx: &egui::Context) {
        if !self.bench.on {
            return;
        }
        let chans: Vec<Id> = self
            .guilds
            .iter()
            .flat_map(|g| g.channels.iter().filter(|c| c.kind != channel_type::GUILD_CATEGORY).map(|c| c.id))
            .collect();
        if let Some(i) = self.bench.next_switch(chans.len()) {
            self.open_channel(chans[i]);
        }
        if self.bench.scenario == "switch" {
            ctx.request_repaint_after(self.bench.switch_every);
        }
        if self.bench.scenario == "scroll" {
            ctx.request_repaint();
        }
        if let Ok(ch) = std::env::var("FEATHER_BENCH_CHANNEL") {
            static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if let Ok(id) = ch.parse::<u64>() {
                if !DONE.load(std::sync::atomic::Ordering::Relaxed) && self.find_channel(Id(id)).is_some() {
                    DONE.store(true, std::sync::atomic::Ordering::Relaxed);
                    self.open_channel(Id(id));
                }
            }
        }
        if let Ok(tab) = std::env::var("FEATHER_BENCH_SETTINGS") {
            self.settings_open = true;
            self.settings_tab = match tab.as_str() {
                "connection" => SettingsTab::Connection,
                "storage" => SettingsTab::Storage,
                "about" => SettingsTab::About,
                _ => SettingsTab::Appearance,
            };
        }
        if self.bench.scenario == "stats" {
            self.send(Command::QueryStats);
        }
        if let Some(s) = self.stats.take() {
            feather_core::diag::stat("rest_requests", s.rest_requests);
            feather_core::diag::stat("rest_bytes", s.rest_bytes);
            feather_core::diag::stat("gw_bytes_wire", s.gw_bytes_wire);
            feather_core::diag::stat("gw_bytes_json", s.gw_bytes_json);
            feather_core::diag::stat("gw_dispatches", s.gw_dispatches);
            feather_core::diag::stat("db_bytes", s.db_bytes);
            feather_core::diag::stat("db_messages", s.db_messages);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// top-level layout

impl App {
    fn draw(&mut self, ctx: &egui::Context) {
        let show_login = matches!(self.session, Session::NeedsLogin { .. }) || (self.session == Session::LoggingIn && self.guilds.is_empty() && self.dms.is_empty() && !self.connected_once && self.view.is_none() && self.frames > 30);
        if show_login && !matches!(self.session, Session::LoggingIn) {
            self.login_ui(ctx);
        } else if matches!(self.session, Session::LoggingIn) && self.guilds.is_empty() && self.dms.is_empty() && self.frames > 1 {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| ui.label(RichText::new("Signing in…").color(self.palette.muted)));
            });
        } else {
            self.main_ui(ctx);
        }
        self.overlays(ctx);
    }

    fn main_ui(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        if self.conn != ConnState::Connected {
            let (text, color) = match &self.conn {
                ConnState::Connecting => ("Connecting…".to_string(), p.muted),
                ConnState::Resuming => ("Reconnecting…".to_string(), p.muted),
                ConnState::Reconnecting { attempt, .. } => (format!("Connection lost. Reconnecting (attempt {attempt}). Showing saved messages."), p.danger),
                ConnState::Offline => ("Offline. Showing saved messages.".to_string(), p.muted),
                ConnState::Connected => unreachable!(),
            };
            egui::TopBottomPanel::top("status").exact_height(22.0).frame(Frame::new().fill(p.rail).inner_margin(Margin::symmetric(10, 2))).show(ctx, |ui| {
                ui.label(RichText::new(text).small().color(color));
            });
        }
        self.rail_ui(ctx);
        self.sidebar_ui(ctx);
        if self.search_open {
            self.search_ui(ctx);
        } else if self.settings.show_member_list && self.settings.intents.guild_members && self.sel_guild.is_some() {
            self.members_ui(ctx);
        }
        if self.view.is_some() {
            self.composer_ui(ctx);
        }
        egui::CentralPanel::default().frame(Frame::new().fill(theme::background_fill(&self.settings.theme, &p))).show(ctx, |ui| {
            theme::paint_background(ui, ui.max_rect(), &self.settings.theme, &p);
            if self.view.is_some() {
                self.channel_header(ui);
                self.messages_ui(ui);
            } else {
                ui.centered_and_justified(|ui| {
                    let hint = if self.guilds.is_empty() && self.dms.is_empty() {
                        "Waiting for servers…\nInvite your bot to a server to see it here."
                    } else {
                        "Pick a channel on the left.   Ctrl+K opens the quick switcher."
                    };
                    ui.label(RichText::new(hint).color(p.muted));
                });
            }
        });
    }

    fn channel_header(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        let Some(ch) = self.view.as_ref().map(|v| v.channel) else { return };
        let (title, topic) = match self.find_channel(ch) {
            Some((c, Some(_))) => (format!("# {}", c.label()), c.topic.clone().unwrap_or_default()),
            Some((c, None)) => (format!("@ {}", c.label()), String::new()),
            None => ("channel".into(), String::new()),
        };
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new(title).strong().size(self.settings.theme.font_size * 1.1));
            if !topic.is_empty() {
                ui.separator();
                ui.add(egui::Label::new(RichText::new(topic.replace('\n', " ")).small().color(p.muted)).truncate());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("🔍").on_hover_text("Search saved messages (Ctrl+F)").clicked() {
                    self.search_open = !self.search_open;
                }
            });
        });
        ui.add_space(2.0);
        let r = ui.available_rect_before_wrap();
        ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.raised));
    }

    // ---- server rail

    fn rail_ui(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        let mut select: Option<Option<Id>> = None;
        egui::SidePanel::left("rail").exact_width(68.0).resizable(false).frame(Frame::new().fill(p.rail).inner_margin(Margin::symmetric(10, 8))).show(ctx, |ui| {
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                let dm_unread: u32 = self.dms.iter().map(|c| self.mentions(c.id)).sum();
                let dm_any = self.dms.iter().any(|c| self.is_unread(c.id));
                if self.round_button(ui, None, "DM", self.sel_guild.is_none(), dm_any, dm_unread, "Direct messages") {
                    select = Some(None);
                }
                ui.add(egui::Separator::default().spacing(2.0));
                let guilds = self.guilds.clone_light();
                for g in &guilds {
                    let (unread, m) = self.guild_unread_light(g);
                    let icon = g.icon.as_ref().map(|h| self.endpoints.guild_icon_url(g.id, h, 96));
                    let initials: String = g.name.split_whitespace().filter_map(|w| w.chars().next()).take(3).collect();
                    let label = if initials.is_empty() { "?".to_string() } else { initials };
                    if self.round_button(ui, icon, &label, self.sel_guild == Some(g.id), unread, m, &g.name) {
                        select = Some(Some(g.id));
                    }
                }
            });
        });
        if let Some(sel) = select {
            self.sel_guild = sel;
            if let Some(ch) = self.last_in_guild.get(&sel).copied().filter(|c| self.find_channel(*c).is_some()).or_else(|| match sel {
                Some(g) => self.guild(g).and_then(|g| g.channels.iter().find(|c| c.kind != channel_type::GUILD_CATEGORY).map(|c| c.id)),
                None => self.dm_order().first().map(|c| c.id),
            }) {
                self.open_channel(ch);
                self.sel_guild = sel;
            }
        }
    }

    fn guild_unread_light(&self, g: &GuildLite) -> (bool, u32) {
        let mut unread = false;
        let mut m = 0;
        for c in &g.channel_ids {
            unread |= self.is_unread(*c);
            m += self.mentions(*c);
        }
        (unread, m)
    }

    #[allow(clippy::too_many_arguments)]
    fn round_button(&mut self, ui: &mut egui::Ui, icon: Option<String>, label: &str, selected: bool, unread: bool, mentions: u32, tip: &str) -> bool {
        let p = self.palette;
        let size = 46.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
        let radius = if selected || resp.hovered() { (self.settings.theme.corner_radius * 2.0).min(16.0) } else { size / 2.0 };
        let fill = if selected { p.accent } else if resp.hovered() { p.raised } else { p.sidebar };
        ui.painter().rect_filled(rect, CornerRadius::same(radius as u8), fill);
        let mut drew = false;
        if let Some(url) = icon {
            let mut req = |c: Command| {
                if let Some(e) = &self.engine {
                    e.send(c);
                }
            };
            if let Lookup::Ready(tex, _) = self.images.get(&url, 96, 96, &mut req) {
                egui::Image::new((tex.id(), rect.size())).corner_radius(CornerRadius::same(radius as u8)).paint_at(ui, rect);
                drew = true;
            }
        }
        if !drew {
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, FontId::proportional(self.settings.theme.font_size), if selected { p.accent_text } else { p.text });
        }
        if unread || selected {
            let h = if selected { 28.0 } else { 8.0 };
            let bar = Rect::from_center_size(Pos2::new(rect.left() - 8.0, rect.center().y), Vec2::new(4.0, h));
            ui.painter().rect_filled(bar, 2, p.text);
        }
        if mentions > 0 {
            badge(ui, rect.right_bottom() - Vec2::new(2.0, 2.0), mentions, p);
        }
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tip));
        resp.on_hover_text(tip).clicked()
    }

    // ---- channel list / DM list

    fn sidebar_ui(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        let w = self.settings.theme.sidebar_width;
        let mut open: Option<Id> = None;
        let mut new_width = None;
        let resp = egui::SidePanel::left("sidebar")
            .default_width(w)
            .width_range(160.0..=400.0)
            .resizable(true)
            .frame(Frame::new().fill(p.sidebar).inner_margin(Margin::same(0)))
            .show(ctx, |ui| {
                let title = match self.sel_guild.and_then(|g| self.guild(g)) {
                    Some(g) => g.name.clone(),
                    None => "Direct Messages".to_string(),
                };
                Frame::new().inner_margin(Margin::symmetric(12, 10)).show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(title).strong()).truncate());
                });
                let r = ui.available_rect_before_wrap();
                ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.raised));
                let bottom_h = 46.0;
                let list_h = (ui.available_height() - bottom_h).max(40.0);
                ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).id_salt("chan-list").show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.add_space(4.0);
                    let cur = self.view.as_ref().map(|v| v.channel);
                    match self.sel_guild.and_then(|g| self.guild(g)).cloned() {
                        Some(g) => {
                            let mut hidden_cat = false;
                            for c in &g.channels {
                                if c.kind == channel_type::GUILD_CATEGORY {
                                    hidden_cat = self.collapsed.contains(&c.id);
                                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::click());
                                    let arrow = if hidden_cat { "▸" } else { "▾" };
                                    ui.painter().text(rect.left_center() + Vec2::new(10.0, 0.0), Align2::LEFT_CENTER, format!("{arrow} {}", c.label().to_uppercase()), FontId::proportional(self.settings.theme.font_size * 0.78), p.muted);
                                    if resp.clicked() {
                                        if !self.collapsed.remove(&c.id) {
                                            self.collapsed.insert(c.id);
                                        }
                                    }
                                    continue;
                                }
                                let is_thread = matches!(c.kind, channel_type::PUBLIC_THREAD | channel_type::PRIVATE_THREAD | channel_type::ANNOUNCEMENT_THREAD);
                                let in_hidden = c.parent_id.is_some_and(|pid| self.collapsed.contains(&pid)) && !is_thread;
                                if in_hidden && cur != Some(c.id) {
                                    continue;
                                }
                                if hidden_cat && c.parent_id.is_some() && cur != Some(c.id) {
                                    continue;
                                }
                                let glyph = match c.kind {
                                    channel_type::GUILD_VOICE | channel_type::GUILD_STAGE_VOICE => "🔊",
                                    channel_type::GUILD_ANNOUNCEMENT => "📢",
                                    _ if is_thread => "↳",
                                    _ => "#",
                                };
                                let indent = if is_thread { 24.0 } else { 8.0 };
                                if self.sidebar_item(ui, c.id, &format!("{glyph} {}", c.label()), indent, cur == Some(c.id), None) {
                                    open = Some(c.id);
                                }
                            }
                        }
                        None => {
                            let dms: Vec<Channel> = self.dm_order().into_iter().cloned().collect();
                            for c in &dms {
                                let user = c.recipients.first().cloned();
                                if self.sidebar_item(ui, c.id, &c.label(), 8.0, cur == Some(c.id), user) {
                                    open = Some(c.id);
                                }
                            }
                            if dms.is_empty() {
                                ui.add_space(8.0);
                                ui.label(RichText::new("No conversations yet.\nDMs appear here once someone messages your bot.").small().color(p.muted));
                            }
                        }
                    }
                });
                // Bottom user panel
                let r = ui.available_rect_before_wrap();
                ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.raised));
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    let name = self.me.as_ref().map(|u| u.display_name().to_string()).unwrap_or_else(|| "Not signed in".into());
                    let dot = match self.conn {
                        ConnState::Connected => p.online,
                        ConnState::Offline => p.muted,
                        _ => Color32::from_rgb(0xf0, 0xb2, 0x32),
                    };
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.0, dot);
                    ui.add(egui::Label::new(RichText::new(name).small()).truncate());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(6.0);
                        if ui.button("⚙").on_hover_text("Settings (Ctrl+,)").clicked() {
                            self.settings_open = true;
                        }
                    });
                });
            });
        let rw = resp.response.rect.width();
        if (rw - w).abs() > 1.0 && resp.response.dragged() || (resp.response.rect.width() - w).abs() > 1.0 && ctx.input(|i| i.pointer.any_released()) {
            new_width = Some(rw);
        }
        if let Some(nw) = new_width {
            self.settings.theme.sidebar_width = nw.clamp(160.0, 400.0);
            self.mark_settings_dirty();
        }
        if let Some(id) = open {
            self.open_channel(id);
        }
    }

    fn sidebar_item(&mut self, ui: &mut egui::Ui, id: Id, label: &str, indent: f32, selected: bool, avatar_of: Option<User>) -> bool {
        let p = self.palette;
        let unread = self.is_unread(id);
        let mentions = self.mentions(id);
        let h = (self.settings.theme.font_size * 1.3 + 10.0).max(if avatar_of.is_some() { 36.0 } else { 0.0 });
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width() - 8.0, h), Sense::click());
        let rect = rect.translate(Vec2::new(4.0, 0.0));
        let r = CornerRadius::same(self.settings.theme.corner_radius as u8);
        if selected {
            ui.painter().rect_filled(rect, r, p.raised);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, r, p.hover);
        }
        let mut x = rect.left() + indent;
        if let Some(u) = avatar_of {
            let a = Rect::from_min_size(Pos2::new(x, rect.center().y - 14.0), Vec2::splat(28.0));
            self.draw_avatar(ui, &u, a);
            x += 36.0;
        }
        let color = if selected || unread || mentions > 0 { p.text } else { p.muted };
        let font = FontId::proportional(self.settings.theme.font_size);
        let galley = ui.painter().layout_no_wrap(label.to_string(), font, color);
        let max_w = rect.right() - x - if mentions > 0 { 34.0 } else { 8.0 };
        let clipped = ui.painter().with_clip_rect(Rect::from_min_max(Pos2::new(x, rect.top()), Pos2::new(x + max_w, rect.bottom())));
        clipped.galley(Pos2::new(x, rect.center().y - galley.size().y / 2.0), galley, color);
        if unread && !selected {
            let bar = Rect::from_center_size(Pos2::new(rect.left() - 2.0, rect.center().y), Vec2::new(4.0, 8.0));
            ui.painter().rect_filled(bar, 2, p.text);
        }
        if mentions > 0 {
            badge(ui, Pos2::new(rect.right() - 6.0, rect.center().y + 8.0), mentions, p);
        }
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("{label}{}", if unread { ", unread" } else { "" })));
        resp.clicked()
    }

    pub fn draw_avatar(&mut self, ui: &egui::Ui, u: &User, rect: Rect) {
        let p = self.palette;
        let url = match &u.avatar {
            Some(h) => self.endpoints.avatar_url(u.id, h, 64),
            None => self.endpoints.default_avatar_url(u.id),
        };
        let radius = CornerRadius::same((rect.width() / 2.0) as u8);
        let mut req = |c: Command| {
            if let Some(e) = &self.engine {
                e.send(c);
            }
        };
        match self.images.get(&url, 64, 64, &mut req) {
            Lookup::Ready(tex, _) => egui::Image::new((tex.id(), rect.size())).corner_radius(radius).paint_at(ui, rect),
            _ => {
                ui.painter().rect_filled(rect, radius, p.raised);
                let ch = u.display_name().chars().next().unwrap_or('?').to_uppercase().to_string();
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, ch, FontId::proportional(rect.width() * 0.45), p.muted);
            }
        }
    }

    // ---- right panels

    fn members_ui(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        let Some(g) = self.sel_guild else { return };
        egui::SidePanel::right("members").exact_width(self.settings.theme.member_list_width).resizable(false).frame(Frame::new().fill(p.sidebar).inner_margin(Margin::symmetric(10, 10))).show(ctx, |ui| {
            ui.label(RichText::new("MEMBERS").small().color(p.muted));
            ui.add_space(4.0);
            match self.members.get(&g).cloned() {
                None => {
                    ui.label(RichText::new("Loading…").color(p.muted));
                }
                Some(Err(e)) => {
                    ui.label(RichText::new(e).small().color(p.muted));
                }
                Some(Ok(list)) => {
                    let mut names: Vec<(String, User)> = list
                        .iter()
                        .filter_map(|m| m.user.as_ref().map(|u| (m.nick.clone().unwrap_or_else(|| u.display_name().to_string()), u.clone())))
                        .collect();
                    names.sort_by_key(|(n, _)| n.to_lowercase());
                    let h = 32.0;
                    ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, h, names.len(), |ui, range| {
                        for (name, user) in &names[range] {
                            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
                            self.draw_avatar(ui, user, Rect::from_min_size(rect.min + Vec2::new(0.0, 2.0), Vec2::splat(26.0)));
                            ui.painter().text(rect.left_center() + Vec2::new(34.0, 0.0), Align2::LEFT_CENTER, name, FontId::proportional(self.settings.theme.font_size), p.text);
                        }
                    });
                }
            }
        });
    }

    fn search_ui(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        let mut open_ch = None;
        egui::SidePanel::right("search").default_width(320.0).resizable(true).frame(Frame::new().fill(p.sidebar).inner_margin(Margin::same(10))).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Search saved messages").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✕").clicked() {
                        self.search_open = false;
                    }
                });
            });
            ui.label(RichText::new("Searches messages stored on this computer. Discord offers bots no server-side search.").small().color(p.muted));
            let r = ui.add(TextEdit::singleline(&mut self.search_query).hint_text("Search…").desired_width(f32::INFINITY));
            if r.changed() {
                self.search_seq += 1;
                let channel = None;
                self.send(Command::Search { query: self.search_query.clone(), channel, seq: self.search_seq });
            }
            ui.add_space(6.0);
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if self.search_results.is_empty() && !self.search_query.is_empty() {
                    ui.label(RichText::new("No saved messages match.").color(p.muted));
                }
                for m in &self.search_results {
                    let place = self.find_channel(m.channel_id).map(|(c, g)| match g {
                        Some(g) => format!("{} › #{}", g.name, c.label()),
                        None => c.label(),
                    });
                    let snippet: String = m.content.chars().take(140).collect();
                    let text = format!("{}  ·  {}\n{}", place.unwrap_or_else(|| "unknown channel".into()), m.author_name(), snippet);
                    if ui.add(egui::Button::new(text).wrap().min_size(Vec2::new(ui.available_width(), 0.0)).fill(Color32::TRANSPARENT)).clicked() {
                        open_ch = Some(m.channel_id);
                    }
                }
            });
        });
        if let Some(c) = open_ch {
            self.open_channel(c);
        }
    }

    // ---- composer

    fn composer_ui(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        let Some(ch) = self.view.as_ref().map(|v| v.channel) else { return };
        let can_send = self.can_send(ch);
        let name = self.find_channel(ch).map(|(c, g)| if g.is_some() { format!("#{}", c.label()) } else { format!("@{}", c.label()) }).unwrap_or_default();
        let id = egui::Id::new("composer-edit");
        let mut do_send = false;
        egui::TopBottomPanel::bottom("composer").frame(Frame::new().fill(self.settings.theme_surface(&p)).inner_margin(Margin::symmetric(16, 10))).show(ctx, |ui| {
            if let Some((_, who)) = &self.reply_to {
                let who = who.clone();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Replying to {who}")).small().color(p.muted));
                    if ui.small_button("✕").clicked() {
                        self.reply_to = None;
                    }
                });
            }
            if self.editing.is_some() {
                ui.label(RichText::new("Editing message. Enter to save, Esc to cancel.").small().color(p.accent));
            }
            if !self.attachments.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    let mut remove = None;
                    for (i, (n, b)) in self.attachments.iter().enumerate() {
                        if ui.small_button(format!("📎 {n} ({}) ✕", rows::human_size(b.len() as u64))).clicked() {
                            remove = Some(i);
                        }
                    }
                    if let Some(i) = remove {
                        self.attachments.remove(i);
                    }
                });
            }
            let has_focus = ctx.memory(|m| m.has_focus(id));
            let enter = has_focus && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
            if has_focus && self.composer.is_empty() && self.editing.is_none() && ctx.input(|i| i.key_pressed(Key::ArrowUp)) {
                let me = self.my_id();
                if let Some(v) = self.view.as_ref() {
                    if let Some(r) = v.rows.iter().rev().find(|r| r.msg.author.id == me && r.msg.kind == 0) {
                        self.editing = Some(r.msg.id);
                        self.composer = r.msg.content.clone();
                    }
                }
            }
            let hint = if can_send { format!("Message {name}") } else { "You don't have permission to send messages here".to_string() };
            ui.add_enabled_ui(can_send, |ui| {
                let r = ui.add(
                    TextEdit::multiline(&mut self.composer)
                        .id(id)
                        .hint_text(hint)
                        .desired_rows(1)
                        .desired_width(f32::INFINITY)
                        .lock_focus(true)
                        .margin(Margin::symmetric(10, 8)),
                );
                if self.composer_focus {
                    r.request_focus();
                    self.composer_focus = false;
                }
            });
            let n = self.composer.chars().count();
            if n > 1800 {
                ui.label(RichText::new(format!("{n}/2000")).small().color(if n > 2000 { p.danger } else { p.muted }));
            }
            if enter && can_send && n <= 2000 {
                do_send = true;
            }
        });
        if do_send {
            self.submit_composer(ch);
        }
    }

    fn submit_composer(&mut self, ch: Id) {
        let text = self.composer.trim().to_string();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        if let Some(msg) = self.editing.take() {
            if !text.is_empty() {
                self.send(Command::Edit { channel: ch, message: msg, content: text });
            }
            self.composer.clear();
            return;
        }
        self.nonce_counter += 1;
        let nonce = format!("{:x}{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0), self.nonce_counter);
        let files = self.attachments.drain(..).map(|(filename, bytes)| feather_core::rest::Upload { filename, bytes }).collect();
        let reply_to = self.reply_to.take().map(|(id, _)| id);
        self.send(Command::Send { channel: ch, content: text, reply_to, nonce, files });
        self.composer.clear();
        if let Some(v) = self.view.as_mut() {
            v.snap_bottom = true;
        }
    }
}

trait ThemeExt {
    fn theme_surface(&self, p: &Palette) -> Color32;
}
impl ThemeExt for Settings {
    fn theme_surface(&self, p: &Palette) -> Color32 {
        theme::background_fill(&self.theme, p)
    }
}

/// Small owned copy of what the rail needs from a guild, so the rail can borrow `self` mutably.
pub struct GuildLite {
    pub id: Id,
    pub name: String,
    pub icon: Option<String>,
    pub channel_ids: Vec<Id>,
}

trait CloneLight {
    fn clone_light(&self) -> Vec<GuildLite>;
}
impl CloneLight for Vec<GuildInfo> {
    fn clone_light(&self) -> Vec<GuildLite> {
        self.iter()
            .map(|g| GuildLite {
                id: g.id,
                name: g.name.clone(),
                icon: g.icon.clone(),
                channel_ids: g.channels.iter().filter(|c| c.kind != channel_type::GUILD_CATEGORY).map(|c| c.id).collect(),
            })
            .collect()
    }
}

pub fn badge(ui: &egui::Ui, center: Pos2, n: u32, p: Palette) {
    let text = if n > 99 { "99+".to_string() } else { n.to_string() };
    let galley = ui.painter().layout_no_wrap(text, FontId::proportional(10.5), Color32::WHITE);
    let size = Vec2::new((galley.size().x + 8.0).max(16.0), 16.0);
    let rect = Rect::from_center_size(center - Vec2::new(size.x / 2.0 - 8.0, 0.0), size);
    ui.painter().rect_filled(rect, 8, p.danger);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

// ---------------------------------------------------------------------------------------------
// the virtualised message list

impl App {
    fn messages_ui(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        let Some(mut v) = self.view.take() else { return };
        let theme = self.settings.theme.clone();
        let metrics = theme::metrics(&theme);
        let me = self.my_id();
        let guild_info = v_guild(&self.guilds, self.sel_guild).cloned();
        let width = ui.available_width();
        let env = Env { p, m: metrics, font_size: theme.font_size, guild: guild_info.as_ref(), me, fonts: &self.fonts, show_images: self.settings.image_previews };
        let ctx = ui.ctx().clone();

        // 1. exact heights for every row (cached until width/theme/content changes)
        let gen = (self.theme_gen << 44) ^ (v.layout_gen << 20) ^ (width.round() as u64);
        let n = v.rows.len();
        let mut tops = std::mem::take(&mut v.tops);
        tops.clear();
        tops.reserve(n + 1);
        let mut y = 8.0;
        for r in v.rows.iter_mut() {
            if r.gen != gen || r.height.is_none() {
                let revealed = v.revealed.contains(&r.msg.id);
                let b = rows::build(&ctx, r, &env, width, revealed);
                r.height = Some(b.height);
                r.built = Some(b);
                r.gen = gen;
            }
            tops.push(y);
            y += r.height.unwrap();
        }
        tops.push(y);
        let total = y + 8.0;

        // 2. scroll target
        let mut area = ScrollArea::vertical().id_salt(("msgs", v.channel.0)).auto_shrink([false, false]).stick_to_bottom(true);
        if v.snap_bottom {
            area = area.vertical_scroll_offset(total);
            v.snap_bottom = false;
        } else if v.restore_anchor {
            if let Some((id, off)) = v.anchor {
                if let Some(i) = v.rows.iter().position(|r| r.msg.id == id) {
                    area = area.vertical_scroll_offset((tops[i] - off).max(0.0));
                }
            }
            v.restore_anchor = false;
        } else if self.bench.on && self.bench.scenario == "scroll" {
            let step = 14.0 * self.bench.scroll_dir;
            let mut next = v.scroll_y + step;
            if next < 0.0 {
                self.bench.scroll_dir = 1.0;
                next = 0.0;
            } else if next > total {
                self.bench.scroll_dir = -1.0;
            }
            area = area.vertical_scroll_offset(next);
        }

        if v.rows.is_empty() {
            let msg = if v.got_network {
                "No messages here yet. Say something!"
            } else if matches!(self.conn, ConnState::Connected) {
                "Loading messages…"
            } else {
                "No saved messages for this channel while offline."
            };
            ui.centered_and_justified(|ui| ui.label(RichText::new(msg).color(p.muted)));
            v.tops = tops;
            self.view = Some(v);
            return;
        }

        // 3. draw only what is visible
        let mut actions: Vec<RowAction> = Vec::new();
        let viewport_info = std::cell::Cell::new((0.0f32, 0.0f32));
        area.show_viewport(ui, |ui, viewport| {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(width, total), Sense::hover());
            let origin = rect.min;
            viewport_info.set((viewport.min.y, viewport.max.y));
            let overscan = 300.0;
            let first = tops.partition_point(|&t| t <= viewport.min.y - overscan).saturating_sub(1).min(n - 1);
            let last = tops.partition_point(|&t| t < viewport.max.y + overscan).min(n);
            for i in first..last {
                let top = tops[i];
                let h = tops[i + 1] - top;
                let row_rect = Rect::from_min_size(origin + Vec2::new(0.0, top), Vec2::new(width, h));
                if !ui.is_rect_visible(row_rect) {
                    continue;
                }
                let row = &mut v.rows[i];
                if row.built.is_none() {
                    let revealed = v.revealed.contains(&row.msg.id);
                    row.built = Some(rows::build(&ctx, row, &env, width, revealed));
                }
                let built = row.built.as_ref().unwrap();
                paint_row(ui, row_rect, &row.msg, row.grouped, built, &env, &mut self.images, self.engine.as_ref(), &self.endpoints, &mut actions, &theme);
            }
            // Free layouts for rows far outside the viewport.
            let keep_lo = tops.partition_point(|&t| t <= viewport.min.y - 2000.0).saturating_sub(1);
            let keep_hi = tops.partition_point(|&t| t < viewport.max.y + 2000.0).min(n);
            for (i, r) in v.rows.iter_mut().enumerate() {
                if (i < keep_lo || i >= keep_hi) && r.built.is_some() {
                    r.built = None;
                }
            }
        });
        let (vmin, vmax) = viewport_info.get();
        v.scroll_y = vmin;
        v.at_bottom = vmax >= total - 6.0;
        let first_vis = tops.partition_point(|&t| t <= vmin).saturating_sub(1).min(n - 1);
        v.anchor = Some((v.rows[first_vis].msg.id, tops[first_vis] - vmin));

        // 4. incremental history
        if vmin < 300.0 && !v.loading_older && !v.reached_start && !v.rows.is_empty() && self.engine.is_some() {
            v.loading_older = true;
            let before = v.rows[0].msg.id;
            if let Some(e) = &self.engine {
                e.send(Command::LoadOlder { channel: v.channel, before });
            }
        }
        if v.has_newer && vmax > total - 300.0 && !v.loading_newer {
            v.loading_newer = true;
            let after = v.rows.last().map(|r| r.msg.id).unwrap_or_default();
            if let Some(e) = &self.engine {
                e.send(Command::LoadNewer { channel: v.channel, after });
            }
        }
        // 5. unread tracking
        if v.at_bottom && self.focused && !v.has_newer {
            if let Some(last) = v.rows.last().map(|r| r.msg.id) {
                if last > v.marked {
                    v.marked = last;
                    if let Some(e) = &self.engine {
                        e.send(Command::MarkRead { channel: v.channel, message: last });
                    }
                    let st = self.read.entry(v.channel).or_default();
                    st.last_read = st.last_read.max(last);
                    st.mentions = 0;
                }
            }
        }
        v.tops = tops;

        // 6. apply deferred row actions
        for a in actions {
            match a {
                RowAction::Link(u) => self.open_link(u),
                RowAction::Spoiler(id) => {
                    v.revealed.insert(id);
                    if let Some(r) = v.rows.iter_mut().find(|r| r.msg.id == id) {
                        r.height = None;
                        r.built = None;
                    }
                }
                RowAction::Menu(id, pos) => self.ctx_menu = Some((id, pos)),
                RowAction::React(msg, emoji, add) => {
                    self.send(Command::React { channel: v.channel, message: msg, emoji, add });
                }
            }
        }
        self.view = Some(v);
    }

    pub fn open_link(&mut self, url: String) {
        if self.settings.confirm_links {
            self.pending_link = Some(url);
        } else {
            self.ctx.open_url(egui::OpenUrl::new_tab(url));
        }
    }
}

fn v_guild(guilds: &[GuildInfo], sel: Option<Id>) -> Option<&GuildInfo> {
    sel.and_then(|g| guilds.iter().find(|x| x.id == g))
}

pub enum RowAction {
    Link(String),
    Spoiler(Id),
    Menu(Id, Pos2),
    React(Id, Emoji, bool),
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    ui: &mut egui::Ui,
    rect: Rect,
    msg: &Message,
    grouped: bool,
    built: &rows::Built,
    env: &Env,
    images: &mut crate::imgcache::ImageCache,
    engine: Option<&EngineHandle>,
    endpoints: &Endpoints,
    actions: &mut Vec<RowAction>,
    theme: &Theme,
) {
    let p = &env.p;
    let hovered = ui.rect_contains_pointer(rect);
    let mentioned = msg.mentions_user(env.me) && msg.author.id != env.me;
    if mentioned {
        ui.painter().rect_filled(rect, 0, p.mention_bg);
        ui.painter().rect_filled(Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height())), 0, p.accent);
    } else if hovered {
        ui.painter().rect_filled(rect, 0, p.hover);
    }
    if hovered && ui.input(|i| i.pointer.secondary_clicked()) {
        if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
            actions.push(RowAction::Menu(msg.id, pos));
        }
    }
    let cx = rect.left() + rows::LEFT + env.m.gutter;
    let mut request = |c: Command| {
        if let Some(e) = engine {
            e.send(c);
        }
    };
    if !grouped && env.m.avatar > 0.0 {
        let a = Rect::from_min_size(Pos2::new(rect.left() + rows::LEFT, rect.top() + env.m.group_gap + env.m.pad_y + 2.0), Vec2::splat(env.m.avatar));
        let url = match &msg.author.avatar {
            Some(h) => endpoints.avatar_url(msg.author.id, h, 64),
            None => endpoints.default_avatar_url(msg.author.id),
        };
        let r = CornerRadius::same((env.m.avatar / 2.0) as u8);
        match images.get(&url, 64, 64, &mut request) {
            Lookup::Ready(tex, _) => egui::Image::new((tex.id(), a.size())).corner_radius(r).paint_at(ui, a),
            _ => {
                ui.painter().rect_filled(a, r, p.raised);
                let ch = msg.author_name().chars().next().unwrap_or('?').to_uppercase().to_string();
                ui.painter().text(a.center(), Align2::CENTER_CENTER, ch, FontId::proportional(env.m.avatar * 0.45), p.muted);
            }
        }
    } else if grouped && hovered && env.m.avatar > 0.0 {
        let t = rows::short_time(&msg.timestamp);
        ui.painter().text(Pos2::new(rect.left() + rows::LEFT + env.m.avatar / 2.0, rect.top() + env.m.pad_y + 8.0), Align2::CENTER_CENTER, t, FontId::proportional(env.font_size * 0.7), p.muted);
    }

    for item in &built.items {
        match item {
            Item::Reply { y, galley } => {
                ui.painter().galley(Pos2::new(cx, rect.top() + y), galley.clone(), p.muted);
            }
            Item::Header { y, name, time, bot, name_color } => {
                let pos = Pos2::new(cx, rect.top() + y);
                ui.painter().galley(pos, name.clone(), *name_color);
                let mut x = cx + name.size().x + 8.0;
                if *bot {
                    let g = ui.painter().layout_no_wrap("BOT".into(), FontId::proportional(env.font_size * 0.65), p.accent_text);
                    let r = Rect::from_min_size(Pos2::new(x, pos.y + 2.0), g.size() + Vec2::new(8.0, 2.0));
                    ui.painter().rect_filled(r, 3, p.accent);
                    ui.painter().galley(r.center() - g.size() / 2.0, g, p.accent_text);
                    x += r.width() + 6.0;
                }
                ui.painter().galley(Pos2::new(x, pos.y + (name.size().y - time.size().y).max(0.0) / 2.0 + 1.0), time.clone(), p.muted);
            }
            Item::Body { y, galley } => {
                let pos = Pos2::new(cx, rect.top() + y);
                let body_rect = Rect::from_min_size(pos, galley.size());
                let resp = ui.put(body_rect, egui::Label::new(WidgetText::Galley(galley.clone())).selectable(true).sense(Sense::click()));
                if !built.body_links.is_empty() {
                    if let Some(pp) = resp.hover_pos() {
                        let idx = galley.cursor_from_pos(pp - body_rect.min).index;
                        if let Some((_, _, l)) = built.body_links.iter().find(|(a, b, _)| idx >= *a && idx < *b) {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            if resp.clicked() {
                                match l {
                                    Link::Url(u) => actions.push(RowAction::Link(u.clone())),
                                    Link::Spoiler => actions.push(RowAction::Spoiler(msg.id)),
                                }
                            }
                        }
                    }
                }
            }
            Item::Image { y, size, url, full, w, h } => {
                let r = Rect::from_min_size(Pos2::new(cx, rect.top() + y), *size);
                match images.get(url, (size.x * 1.5) as u32, (size.y * 1.5) as u32, &mut request) {
                    Lookup::Ready(tex, _) => {
                        egui::Image::new((tex.id(), r.size())).corner_radius(CornerRadius::same(theme.corner_radius as u8)).paint_at(ui, r);
                    }
                    Lookup::Pending => {
                        ui.painter().rect_filled(r, CornerRadius::same(theme.corner_radius as u8), p.raised);
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, format!("Loading {w}×{h}…"), FontId::proportional(env.font_size * 0.85), p.muted);
                    }
                    Lookup::Failed => {
                        ui.painter().rect_filled(r, CornerRadius::same(theme.corner_radius as u8), p.raised);
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, "Image unavailable", FontId::proportional(env.font_size * 0.85), p.muted);
                    }
                }
                let resp = ui.interact(r, ui.id().with(("img", msg.id.0, *y as i32)), Sense::click());
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    actions.push(RowAction::Link(full.clone()));
                }
            }
            Item::File { y, label, url } => {
                let r = Rect::from_min_size(Pos2::new(cx, rect.top() + y), label.size());
                ui.painter().galley(r.min, label.clone(), p.accent);
                let resp = ui.interact(r, ui.id().with(("file", msg.id.0, *y as i32)), Sense::click());
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    actions.push(RowAction::Link(url.clone()));
                }
            }
            Item::Embed { y, height, color, texts, thumb } => {
                let w = (rect.right() - cx - 16.0).min(480.0);
                let r = Rect::from_min_size(Pos2::new(cx, rect.top() + y), Vec2::new(w, *height));
                ui.painter().rect_filled(r, CornerRadius::same(theme.corner_radius as u8), p.raised);
                ui.painter().rect_filled(Rect::from_min_size(r.min, Vec2::new(4.0, r.height())), CornerRadius { nw: theme.corner_radius as u8, sw: theme.corner_radius as u8, ne: 0, se: 0 }, *color);
                let mut last_bottom = 0.0;
                for (ty, g, link) in texts {
                    let pos = r.min + Vec2::new(14.0, *ty);
                    ui.painter().galley(pos, g.clone(), p.text);
                    last_bottom = ty + g.size().y;
                    if let Some(u) = link {
                        let tr = Rect::from_min_size(pos, g.size());
                        let resp = ui.interact(tr, ui.id().with(("embed", msg.id.0, *ty as i32)), Sense::click());
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.clicked() {
                            actions.push(RowAction::Link(u.clone()));
                        }
                    }
                }
                if let Some((url, sz)) = thumb {
                    let tr = Rect::from_min_size(r.min + Vec2::new(14.0, last_bottom + 6.0), *sz);
                    if let Lookup::Ready(tex, _) = images.get(url, (sz.x * 1.5) as u32, (sz.y * 1.5) as u32, &mut request) {
                        egui::Image::new((tex.id(), tr.size())).corner_radius(CornerRadius::same(4)).paint_at(ui, tr);
                    } else {
                        ui.painter().rect_filled(tr, 4, p.sidebar);
                    }
                }
            }
            Item::Reactions { y, pills } => {
                for pill in pills {
                    let r = Rect::from_min_size(Pos2::new(cx + pill.x, rect.top() + y + pill.y), pill.size);
                    let fill = if pill.me { p.mention_bg } else { p.raised };
                    ui.painter().rect_filled(r, 8, fill);
                    if pill.me {
                        ui.painter().rect_stroke(r, 8, Stroke::new(1.0, p.accent), StrokeKind::Inside);
                    }
                    ui.painter().galley(r.center() - pill.label.size() / 2.0, pill.label.clone(), p.text);
                    let resp = ui.interact(r, ui.id().with(("react", msg.id.0, pill.x as i32, pill.y as i32)), Sense::click());
                    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("reaction {}", pill.label.text())));
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.clicked() {
                        actions.push(RowAction::React(msg.id, pill.emoji.clone(), !pill.me));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// login, dialogs, overlays

impl App {
    fn login_ui(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.12).max(12.0));
                ui.set_max_width(520.0);
                ui.label(RichText::new("Feather").size(34.0).strong().color(p.accent));
                ui.label(RichText::new("A fast, light Discord bot-account client").color(p.muted));
                ui.add_space(18.0);
                Frame::new().fill(p.sidebar).corner_radius(self.settings.theme.corner_radius).inner_margin(Margin::same(18)).show(ui, |ui| {
                    ui.set_width(460.0);
                    ui.label("Sign in with your bot's token. Feather connects through Discord's official bot API; personal account tokens are not supported and are refused.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("1. Create an application and add a Bot:").small().color(p.muted));
                        if ui.link(RichText::new("Developer Portal").small()).clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab("https://discord.com/developers/applications"));
                        }
                    });
                    ui.label(RichText::new("2. Turn on Message Content Intent (Bot → Privileged Gateway Intents).").small().color(p.muted));
                    ui.label(RichText::new("3. Invite the bot to your server, then paste its token below.").small().color(p.muted));
                    ui.add_space(10.0);
                    let r = ui.add(TextEdit::singleline(&mut self.login.token).password(true).hint_text("Bot token").desired_width(f32::INFINITY));
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.checkbox(&mut self.login.remember, "Remember on this computer (stored in your system keychain)");
                    if let Some(e) = &self.login.error {
                        ui.add_space(4.0);
                        ui.colored_label(p.danger, e);
                    }
                    ui.add_space(8.0);
                    let busy = self.login.busy;
                    let clicked = ui.add_enabled(!busy && !self.login.token.trim().is_empty(), egui::Button::new(if busy { "Signing in…" } else { "Sign in" }).fill(p.accent)).clicked();
                    if (clicked || (enter && !busy)) && !self.login.token.trim().is_empty() {
                        match creds::normalise(&self.login.token) {
                            Some(token) => {
                                self.login.busy = true;
                                self.login.error = None;
                                let remember = self.login.remember;
                                self.login.token.clear();
                                self.send(Command::Login { token, remember });
                            }
                            None => self.login.error = Some("That doesn't look like a bot token (it should have three parts separated by dots).".into()),
                        }
                    }
                    if self.guilds.is_empty() && self.dms.is_empty() {
                        ui.add_space(6.0);
                        ui.label(RichText::new("Feather works offline too: once signed in, saved messages stay readable without a connection.").small().color(p.muted));
                    }
                });
            });
        });
    }

    fn overlays(&mut self, ctx: &egui::Context) {
        self.settings_window(ctx);
        self.switcher_ui(ctx);
        self.link_dialog(ctx);
        self.context_menu(ctx);
        self.toast_ui(ctx);
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        if self.toasts.is_empty() {
            return;
        }
        let p = self.palette;
        egui::Area::new(egui::Id::new("toasts")).anchor(Align2::RIGHT_BOTTOM, Vec2::new(-16.0, -70.0)).order(egui::Order::Tooltip).show(ctx, |ui| {
            for t in &self.toasts {
                Frame::new().fill(p.raised).stroke(Stroke::new(1.0, p.accent)).corner_radius(self.settings.theme.corner_radius).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                    ui.set_max_width(360.0);
                    ui.label(&t.text);
                });
                ui.add_space(4.0);
            }
        });
    }

    fn link_dialog(&mut self, ctx: &egui::Context) {
        let Some(url) = self.pending_link.clone() else { return };
        let host = url.split("://").nth(1).and_then(|r| r.split(['/', '?', '#']).next()).unwrap_or("").to_string();
        let suspicious = !host.is_ascii() || host.contains("xn--") || host.contains('@');
        let p = self.palette;
        let mut close = false;
        egui::Window::new("Open this link?").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.label("Feather will open this address in your web browser:");
            ui.add(egui::Label::new(RichText::new(&url).monospace()).wrap());
            if suspicious {
                ui.colored_label(p.danger, "Warning: this address contains characters that can imitate another site. Check it carefully.");
            }
            ui.horizontal(|ui| {
                if ui.button("Open").clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url.clone()));
                    close = true;
                }
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(url.clone());
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if close {
            self.pending_link = None;
        }
    }

    fn context_menu(&mut self, ctx: &egui::Context) {
        let Some((id, pos)) = self.ctx_menu else { return };
        let Some(v) = self.view.as_ref() else {
            self.ctx_menu = None;
            return;
        };
        let Some(msg) = v.rows.iter().find(|r| r.msg.id == id).map(|r| r.msg.clone()) else {
            self.ctx_menu = None;
            return;
        };
        let ch = v.channel;
        let mine = msg.author.id == self.my_id();
        let can_manage = permissions::has(self.perms(ch), permissions::MANAGE_MESSAGES);
        let can_react = permissions::has(self.perms(ch), permissions::ADD_REACTIONS);
        let mut close = false;
        let resp = egui::Area::new(egui::Id::new("msg-menu")).fixed_pos(pos).order(egui::Order::Foreground).show(ctx, |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(170.0);
                if self.can_send(ch) && ui.button("Reply").clicked() {
                    self.reply_to = Some((id, msg.author_name().to_string()));
                    self.composer_focus = true;
                    close = true;
                }
                if mine && msg.kind == 0 && ui.button("Edit").clicked() {
                    self.editing = Some(id);
                    self.composer = msg.content.clone();
                    self.composer_focus = true;
                    close = true;
                }
                if can_react {
                    ui.horizontal(|ui| {
                        for e in ["👍", "❤", "😂", "🎉", "😮", "😢"] {
                            if ui.button(e).clicked() {
                                let emoji = Emoji { id: None, name: Some(e.to_string()), animated: false };
                                self.send(Command::React { channel: ch, message: id, emoji, add: true });
                                close = true;
                            }
                        }
                    });
                }
                if ui.button("Copy text").clicked() {
                    ui.ctx().copy_text(msg.content.clone());
                    close = true;
                }
                if ui.button("Copy message ID").clicked() {
                    ui.ctx().copy_text(msg.id.to_string());
                    close = true;
                }
                if (mine || can_manage) && ui.add(egui::Button::new(RichText::new("Delete").color(self.palette.danger))).clicked() {
                    self.send(Command::Delete { channel: ch, message: id });
                    close = true;
                }
            });
        });
        let clicked_outside = ctx.input(|i| i.pointer.any_pressed()) && !resp.response.contains_pointer();
        if close || clicked_outside {
            self.ctx_menu = None;
        }
    }

    fn switcher_ui(&mut self, ctx: &egui::Context) {
        let Some((mut query, mut sel)) = self.switcher.clone() else { return };
        let p = self.palette;
        let mut items: Vec<(Id, String)> = Vec::new();
        for g in &self.guilds {
            for c in g.channels.iter().filter(|c| c.kind != channel_type::GUILD_CATEGORY) {
                items.push((c.id, format!("{} › #{}", g.name, c.label())));
            }
        }
        for c in &self.dms {
            items.push((c.id, format!("DM › {}", c.label())));
        }
        let words: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_string).collect();
        let mut hits: Vec<&(Id, String)> = items.iter().filter(|(_, l)| { let l = l.to_lowercase(); words.iter().all(|w| l.contains(w.as_str())) }).take(10).collect();
        hits.sort_by_key(|(id, l)| (!self.is_unread(*id), l.len()));
        if !hits.is_empty() {
            sel = sel.min(hits.len() - 1);
        }
        let (down, up, enter) = ctx.input_mut(|i| (i.consume_key(Modifiers::NONE, Key::ArrowDown), i.consume_key(Modifiers::NONE, Key::ArrowUp), i.consume_key(Modifiers::NONE, Key::Enter)));
        if down && !hits.is_empty() {
            sel = (sel + 1) % hits.len();
        }
        if up && !hits.is_empty() {
            sel = (sel + hits.len() - 1) % hits.len();
        }
        let mut chosen = None;
        egui::Area::new(egui::Id::new("switcher")).anchor(Align2::CENTER_TOP, Vec2::new(0.0, 80.0)).order(egui::Order::Foreground).show(ctx, |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(440.0);
                let r = ui.add(TextEdit::singleline(&mut query).hint_text("Jump to a channel or conversation…").desired_width(f32::INFINITY));
                r.request_focus();
                ui.add_space(4.0);
                for (i, (id, label)) in hits.iter().map(|h| (h.0, h.1.clone())).enumerate() {
                    let text = RichText::new(label).color(if self.is_unread(id) { p.text } else { p.muted });
                    if ui.add(egui::Button::new(text).selected(i == sel).min_size(Vec2::new(ui.available_width(), 0.0))).clicked() {
                        chosen = Some(id);
                    }
                }
                if hits.is_empty() {
                    ui.label(RichText::new("No matches").color(p.muted));
                }
            });
        });
        if enter {
            chosen = hits.get(sel).map(|h| h.0);
        }
        if let Some(id) = chosen {
            self.switcher = None;
            self.open_channel(id);
            self.composer_focus = true;
        } else {
            if query != self.switcher.as_ref().map(|s| s.0.clone()).unwrap_or_default() {
                sel = 0;
            }
            self.switcher = Some((query, sel));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// settings

impl App {
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Settings").open(&mut open).default_size([620.0, 560.0]).collapsible(false).show(ctx, |ui| {
            ui.horizontal(|ui| {
                for (t, name) in [
                    (SettingsTab::Appearance, "Appearance"),
                    (SettingsTab::Connection, "Connection"),
                    (SettingsTab::Notifications, "Notifications"),
                    (SettingsTab::Storage, "Storage"),
                    (SettingsTab::About, "About"),
                ] {
                    ui.selectable_value(&mut self.settings_tab, t, name);
                }
            });
            ui.separator();
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.settings_tab {
                SettingsTab::Appearance => self.tab_appearance(ui),
                SettingsTab::Connection => self.tab_connection(ui),
                SettingsTab::Notifications => self.tab_notifications(ui),
                SettingsTab::Storage => self.tab_storage(ui),
                SettingsTab::About => self.tab_about(ui),
            });
        });
        if !open {
            self.settings_open = false;
        }
    }

    fn tab_appearance(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        ui.spacing_mut().slider_width = 200.0;
        let mut changed = false;
        let t = &mut self.settings.theme;
        ui.label(RichText::new("Presets").strong());
        ui.horizontal_wrapped(|ui| {
            for preset in Theme::presets() {
                if ui.button(&preset.name).clicked() {
                    *t = preset;
                    changed = true;
                }
            }
        });
        ui.add_space(6.0);
        egui::Grid::new("theme-grid").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
            ui.label("Base");
            ui.horizontal(|ui| {
                for (b, n) in [(Base::Dark, "Dark"), (Base::Light, "Light"), (Base::Amoled, "AMOLED")] {
                    changed |= ui.selectable_value(&mut t.base, b, n).changed();
                }
            });
            ui.end_row();
            ui.label("Accent colour");
            let mut rgb = [t.accent.0, t.accent.1, t.accent.2];
            if ui.color_edit_button_srgb(&mut rgb).changed() {
                t.accent = Rgb(rgb[0], rgb[1], rgb[2]);
                changed = true;
            }
            ui.end_row();
            ui.label("Text size");
            changed |= ui.add(egui::Slider::new(&mut t.font_size, 10.0..=24.0).suffix(" pt")).changed();
            ui.end_row();
            ui.label("Density");
            ui.horizontal(|ui| {
                for (d, n) in [(Density::Compact, "Compact"), (Density::Cozy, "Cozy"), (Density::Comfortable, "Comfortable")] {
                    changed |= ui.selectable_value(&mut t.density, d, n).changed();
                }
            });
            ui.end_row();
            ui.label("Message spacing");
            changed |= ui.add(egui::Slider::new(&mut t.message_spacing, 0.0..=32.0)).changed();
            ui.end_row();
            ui.label("Sidebar width");
            changed |= ui.add(egui::Slider::new(&mut t.sidebar_width, 160.0..=400.0)).changed();
            ui.end_row();
            ui.label("Member list width");
            changed |= ui.add(egui::Slider::new(&mut t.member_list_width, 160.0..=360.0)).changed();
            ui.end_row();
            ui.label("Corners");
            ui.horizontal(|ui| {
                changed |= ui.add(egui::Slider::new(&mut t.corner_radius, 0.0..=16.0)).changed();
                if ui.small_button("Square").clicked() {
                    t.corner_radius = 0.0;
                    changed = true;
                }
                if ui.small_button("Round").clicked() {
                    t.corner_radius = 10.0;
                    changed = true;
                }
            });
            ui.end_row();
            ui.label("Background");
            ui.horizontal(|ui| {
                let kind = match t.background {
                    Background::Default => 0,
                    Background::Solid { .. } => 1,
                    Background::Gradient { .. } => 2,
                };
                let mut k = kind;
                ui.selectable_value(&mut k, 0, "Default");
                ui.selectable_value(&mut k, 1, "Solid");
                ui.selectable_value(&mut k, 2, "Gradient");
                if k != kind {
                    let base = theme::palette(t).surface;
                    let b = Rgb(base.r(), base.g(), base.b());
                    t.background = match k {
                        0 => Background::Default,
                        1 => Background::Solid { color: b },
                        _ => Background::Gradient { top: b, bottom: Rgb(b.0 / 2, b.1 / 2, b.2 / 2) },
                    };
                    changed = true;
                }
                match &mut t.background {
                    Background::Solid { color } => {
                        let mut a = [color.0, color.1, color.2];
                        if ui.color_edit_button_srgb(&mut a).changed() {
                            *color = Rgb(a[0], a[1], a[2]);
                            changed = true;
                        }
                    }
                    Background::Gradient { top, bottom } => {
                        let mut a = [top.0, top.1, top.2];
                        let mut b = [bottom.0, bottom.1, bottom.2];
                        if ui.color_edit_button_srgb(&mut a).changed() {
                            *top = Rgb(a[0], a[1], a[2]);
                            changed = true;
                        }
                        if ui.color_edit_button_srgb(&mut b).changed() {
                            *bottom = Rgb(b[0], b[1], b[2]);
                            changed = true;
                        }
                    }
                    Background::Default => {}
                }
            });
            ui.end_row();
        });
        ui.add_space(4.0);
        changed |= ui.checkbox(&mut t.reduced_motion, "Reduce animations").changed();
        changed |= ui.checkbox(&mut t.show_avatars, "Show avatars in messages").changed();
        changed |= ui.checkbox(&mut t.blinking_cursor, "Blinking text cursor (uses a little CPU while typing)").changed();
        if ui.checkbox(&mut self.settings.wide_fonts, "Load a system font for CJK and symbols (uses ~20 MB more memory)").changed() {
            let wide = self.settings.wide_fonts;
            self.fonts.bold = crate::fonts::install_system_fonts(ui.ctx(), wide);
            self.bump_layout();
            self.mark_settings_dirty();
        }
        ui.add_space(8.0);
        ui.label(RichText::new("Live preview").strong());
        Frame::new().fill(theme::background_fill(&self.settings.theme, &p)).stroke(Stroke::new(1.0, p.raised)).corner_radius(self.settings.theme.corner_radius).inner_margin(Margin::same(10)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Alice").color(Color32::from_rgb(0x3b, 0xa5, 0x5d)));
                ui.label(RichText::new("Today at 12:30").small().color(p.muted));
            });
            ui.label("Reading from a lighter, faster client.");
            ui.horizontal(|ui| {
                ui.label(RichText::new("@Bob").color(p.accent).background_color(p.mention_bg));
                ui.label("did you see the new theme?");
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("link").color(ui.visuals().hyperlink_color).underline());
                ui.label(RichText::new("code").monospace().background_color(p.raised));
            });
        });
        ui.add_space(8.0);
        ui.label(RichText::new("My themes").strong());
        let mut apply = None;
        let mut delete = None;
        for (i, s) in self.settings.saved_themes.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(&s.name);
                if ui.small_button("Apply").clicked() {
                    apply = Some(i);
                }
                if ui.small_button("Delete").clicked() {
                    delete = Some(i);
                }
            });
        }
        if let Some(i) = apply {
            self.settings.theme = self.settings.saved_themes[i].clone();
            changed = true;
        }
        if let Some(i) = delete {
            self.settings.saved_themes.remove(i);
            changed = true;
        }
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.add(TextEdit::singleline(&mut self.settings.theme.name).desired_width(160.0));
            if ui.button("Save current theme").clicked() {
                let cur = self.settings.theme.clone();
                self.settings.saved_themes.retain(|t| t.name != cur.name);
                if self.settings.saved_themes.len() < 50 {
                    self.settings.saved_themes.push(cur);
                }
                changed = true;
            }
        });
        ui.add_space(8.0);
        ui.label(RichText::new("Share or import").strong());
        ui.label(RichText::new("Themes are plain data (colours and sizes). Importing never runs code.").small().color(p.muted));
        ui.add(TextEdit::multiline(&mut self.theme_json).desired_rows(5).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace).hint_text("Paste a theme here to import it"));
        ui.horizontal(|ui| {
            if ui.button("Export current (copies)").clicked() {
                self.theme_json = self.settings.theme.export_json();
                ui.ctx().copy_text(self.theme_json.clone());
                self.theme_json_error = None;
            }
            if ui.button("Import").clicked() {
                match Theme::import_json(&self.theme_json) {
                    Ok(t) => {
                        self.settings.theme = t;
                        self.theme_json_error = None;
                        changed = true;
                    }
                    Err(e) => self.theme_json_error = Some(e),
                }
            }
            if ui.button("Reset to defaults").clicked() {
                self.settings.theme = Theme::default();
                changed = true;
            }
        });
        if let Some(e) = &self.theme_json_error {
            ui.colored_label(p.danger, e);
        }
        if changed {
            self.settings.theme.sanitize();
            self.mark_settings_dirty();
        }
    }

    fn tab_connection(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        match &self.me {
            Some(u) => ui.label(format!("Signed in as bot {} ({})", u.display_name(), u.id)),
            None => ui.label("Not signed in"),
        };
        ui.label(RichText::new("Feather uses Discord's official bot interface. It shows the servers your bot has joined and DMs people send to it. It cannot show your personal account's servers or DMs.").small().color(p.muted));
        ui.add_space(8.0);
        ui.label(RichText::new("Gateway intents").strong());
        let mut changed = false;
        changed |= ui.checkbox(&mut self.settings.intents.message_content, "Message Content (needed to read message text in servers)").changed();
        changed |= ui.checkbox(&mut self.settings.intents.guild_members, "Server Members (needed for the member list)").changed();
        ui.label(RichText::new("Both are privileged: enable them for your bot in the Developer Portal first, or Discord will refuse the connection (error 4014).").small().color(p.muted));
        if changed {
            let i = self.settings.intents.clone();
            self.send(Command::SetIntents(i));
            self.members.clear();
            self.mark_settings_dirty();
        }
        ui.checkbox(&mut self.settings.show_member_list, "Show member list").changed();
        ui.add_space(8.0);
        ui.checkbox(&mut self.settings.image_previews, "Load image previews");
        ui.checkbox(&mut self.settings.confirm_links, "Ask before opening links");
        ui.add_space(12.0);
        if ui.add(egui::Button::new("Sign out and forget token").fill(p.danger)).clicked() {
            self.send(Command::Logout);
            self.view = None;
            self.settings_open = false;
        }
        ui.label(RichText::new("This also deletes the saved messages on this computer.").small().color(p.muted));
    }

    fn tab_notifications(&mut self, ui: &mut egui::Ui) {
        let mut c = false;
        c |= ui.checkbox(&mut self.settings.notifications, "Desktop notifications for mentions").changed();
        c |= ui.checkbox(&mut self.settings.notify_all_dms, "Also notify for every direct message").changed();
        if c {
            self.send(Command::SetNotify { enabled: self.settings.notifications, all_dms: self.settings.notify_all_dms });
            self.mark_settings_dirty();
        }
        ui.label(RichText::new("Notifications only appear while Feather isn't showing that channel.").small().color(self.palette.muted));
    }

    fn tab_storage(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        if ui.add(egui::Slider::new(&mut self.settings.disk_messages_per_channel, 100..=20000).text("messages kept per channel on disk")).changed() {
            self.send(Command::SetDiskLimit(self.settings.disk_messages_per_channel));
            self.mark_settings_dirty();
        }
        ui.label(RichText::new(format!("Messages in memory: at most {MAX_ROWS} (the open channel only). Image cache: {:.1} MiB of {} MiB used ({} images).", self.images.bytes() as f64 / 1048576.0, IMAGE_BUDGET >> 20, self.images.len())).small().color(p.muted));
        if ui.button("Refresh storage numbers").clicked() {
            self.send(Command::QueryStats);
        }
        if let Some(s) = &self.stats {
            ui.label(format!("Saved messages: {}   Database size: {:.1} MiB", s.db_messages, s.db_bytes as f64 / 1048576.0));
            ui.label(format!("This session: {} REST requests ({:.1} KiB), gateway {:.1} KiB on the wire ({:.1} KiB decompressed), {} reconnects", s.rest_requests, s.rest_bytes as f64 / 1024.0, s.gw_bytes_wire as f64 / 1024.0, s.gw_bytes_json as f64 / 1024.0, s.gw_reconnects));
        }
        if ui.button("Clear image cache").clicked() {
            self.images.clear();
        }
        ui.checkbox(&mut self.show_stats, "Show performance overlay");
    }

    fn tab_about(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        ui.label(RichText::new(format!("Feather {}", env!("CARGO_PKG_VERSION"))).strong().size(18.0));
        ui.label("Independent project. Not affiliated with or endorsed by Discord.");
        ui.add_space(8.0);
        ui.label(RichText::new("Keyboard shortcuts").strong());
        for (k, d) in [
            ("Ctrl+K", "Quick switcher"),
            ("Alt+↑ / Alt+↓", "Previous / next channel"),
            ("Ctrl+F", "Search saved messages"),
            ("Ctrl+,", "Settings"),
            ("Enter / Shift+Enter", "Send / new line"),
            ("↑ in empty composer", "Edit your last message"),
            ("Esc", "Close, cancel reply or edit"),
            ("Right-click a message", "Reply, edit, react, copy, delete"),
        ] {
            ui.horizontal(|ui| {
                ui.label(RichText::new(k).monospace().color(p.accent));
                ui.label(d);
            });
        }
        ui.add_space(8.0);
        ui.label(RichText::new("Drop a file onto the window to attach it (up to 10 MiB each).").small().color(p.muted));
    }
}

#[allow(dead_code)]
fn _unused(_: Stats) {}
