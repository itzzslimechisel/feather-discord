//! Message rows: parsed markdown, exact height computation and cached text layout.
//!
//! Height and drawing share one layout pass ([`build`]) so the virtualised list can compute the
//! exact height of every row (needed for stable scrolling) without drawing anything.

use crate::theme::{self, Metrics, Palette};
use eframe::egui::{self, text::LayoutJob, Color32, FontFamily, FontId, Galley, Stroke, TextFormat, Vec2};
use feather_core::markdown::{self, Kind, Span};
use feather_core::model::*;
use std::sync::Arc;

pub struct Row {
    pub msg: Message,
    pub spans: Vec<Span>,
    /// Same author as the previous row within a few minutes: header omitted.
    pub grouped: bool,
    pub height: Option<f32>,
    pub built: Option<Built>,
    /// Layout generation (width, font, theme) the cached height/built belong to.
    pub gen: u64,
}

impl Row {
    pub fn new(msg: Message) -> Row {
        let spans = markdown::parse(&msg.content);
        Row { msg, spans, grouped: false, height: None, built: None, gen: 0 }
    }

    pub fn refresh(&mut self) {
        self.spans = markdown::parse(&self.msg.content);
        self.height = None;
        self.built = None;
    }
}

/// Everything about the surrounding world a row needs in order to render.
pub struct Env<'a> {
    pub p: Palette,
    pub m: Metrics,
    pub font_size: f32,
    pub guild: Option<&'a GuildInfo>,
    pub me: Id,
    pub fonts: &'a FontSet,
    pub show_images: bool,
}

#[derive(Clone, Copy, Default)]
pub struct FontSet {
    pub bold: bool,
}

pub enum Link {
    Url(String),
    Spoiler,
}

pub struct Built {
    pub width: f32,
    pub height: f32,
    pub items: Vec<Item>,
    /// Link and spoiler targets in the body galley, by char range.
    pub body_links: Vec<(usize, usize, Link)>,
}

pub enum Item {
    Reply { y: f32, galley: Arc<Galley> },
    Header { y: f32, name: Arc<Galley>, time: Arc<Galley>, bot: bool, name_color: Color32 },
    Body { y: f32, galley: Arc<Galley> },
    Image { y: f32, size: Vec2, url: String, full: String, w: u32, h: u32 },
    File { y: f32, label: Arc<Galley>, url: String },
    Embed { y: f32, height: f32, color: Color32, texts: Vec<(f32, Arc<Galley>, Option<String>)>, thumb: Option<(String, Vec2)> },
    Reactions { y: f32, pills: Vec<Pill> },
}

pub struct Pill {
    pub x: f32,
    pub y: f32,
    pub size: Vec2,
    pub label: Arc<Galley>,
    pub me: bool,
    pub emoji: Emoji,
}

pub const LEFT: f32 = 16.0;
pub const MAX_IMG_W: f32 = 420.0;
pub const MAX_IMG_H: f32 = 320.0;

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

pub fn fit(w: u32, h: u32, max_w: f32, max_h: f32) -> Vec2 {
    let (w, h) = (w.max(1) as f32, h.max(1) as f32);
    let s = (max_w / w).min(max_h / h).min(1.0);
    egui::vec2((w * s).max(24.0), (h * s).max(24.0))
}

/// Compact date/time label: `HH:MM` today, otherwise `YYYY-MM-DD HH:MM`, in local time.
pub fn format_time(ts: &str, now: chrono::DateTime<chrono::Local>) -> String {
    use chrono::{DateTime, Local};
    match DateTime::parse_from_rfc3339(ts) {
        Ok(t) => {
            let t = t.with_timezone(&Local);
            if t.date_naive() == now.date_naive() {
                t.format("Today at %H:%M").to_string()
            } else {
                t.format("%Y-%m-%d %H:%M").to_string()
            }
        }
        Err(_) => String::new(),
    }
}

pub fn short_time(ts: &str) -> String {
    use chrono::{DateTime, Local};
    DateTime::parse_from_rfc3339(ts).map(|t| t.with_timezone(&Local).format("%H:%M").to_string()).unwrap_or_default()
}

fn fmt(_env: &Env, size: f32, color: Color32) -> TextFormat {
    TextFormat { font_id: FontId::new(size, FontFamily::Proportional), color, line_height: Some((size * 1.32).round()), ..Default::default() }
}

/// Builds the body text layout job and the list of clickable ranges.
pub fn body_job(row: &Row, env: &Env, width: f32, revealed: bool) -> (LayoutJob, Vec<(usize, usize, Link)>) {
    let p = &env.p;
    let mut job = LayoutJob::default();
    job.wrap.max_width = width;
    let mut links = Vec::new();
    let mut chars = 0usize;
    let mut at_line_start = true;
    let mut prev_italic = false;
    let strong = mix(p.text, if p.text.r() > 128 { Color32::WHITE } else { Color32::BLACK }, 0.35);
    let hyper = mix(p.accent, p.text, 0.3);
    for sp in &row.spans {
        let st = sp.style;
        let size = match st.heading {
            1 => env.font_size * 1.6,
            2 => env.font_size * 1.35,
            3 => env.font_size * 1.15,
            _ if st.subtext => env.font_size * 0.8,
            _ => env.font_size,
        };
        let mut f = fmt(env, size, if st.subtext { p.muted } else { p.text });
        if st.bold || st.heading > 0 {
            if env.fonts.bold {
                f.font_id = FontId::new(size, FontFamily::Name("bold".into()));
            } else {
                f.color = strong;
            }
        }
        f.italics = st.italic;
        if st.underline {
            f.underline = Stroke::new(1.0, f.color);
        }
        if st.strike {
            f.strikethrough = Stroke::new(1.0, f.color);
        }
        if st.code || st.code_block {
            f.font_id = FontId::new(size * 0.93, FontFamily::Monospace);
            f.background = p.raised;
            f.expand_bg = 2.0;
        }
        if st.quote {
            f.color = mix(p.text, p.muted, 0.5);
        }
        if st.spoiler && !revealed {
            f.color = p.raised;
            f.background = mix(p.raised, p.muted, 0.55);
        }
        let mut text = sp.text.clone();
        match &sp.kind {
            Kind::Text => {}
            Kind::Link(_) => {
                f.color = hyper;
                f.underline = Stroke::new(1.0, hyper);
            }
            Kind::User(id) => {
                let name = row
                    .msg
                    .mentions
                    .iter()
                    .find(|u| u.id == *id)
                    .map(|u| u.display_name().to_string())
                    .unwrap_or_else(|| "unknown-user".into());
                text = format!("@{name}");
                f.color = p.accent;
                f.background = p.mention_bg;
            }
            Kind::Role(_) => {
                text = "@role".into();
                f.color = p.accent;
                f.background = p.mention_bg;
            }
            Kind::Everyone => {
                f.color = p.accent;
                f.background = p.mention_bg;
            }
            Kind::Channel(id) => {
                let name = env
                    .guild
                    .and_then(|g| g.channels.iter().find(|c| c.id == *id))
                    .map(|c| c.label())
                    .unwrap_or_else(|| "unknown".into());
                text = format!("#{name}");
                f.color = p.accent;
                f.background = p.mention_bg;
            }
            Kind::Emoji { name, .. } => {
                text = format!(":{name}:");
                f.color = p.muted;
            }
            Kind::Timestamp(secs, style) => {
                text = format_timestamp(*secs, *style);
                f.background = p.raised;
            }
        }
        if st.quote && at_line_start {
            let mut qf = f.clone();
            qf.color = p.accent;
            job.append("▎ ", 0.0, qf);
            chars += 2;
        }
        let n = text.chars().count();
        match &sp.kind {
            Kind::Link(u) => links.push((chars, chars + n, Link::Url(u.clone()))),
            _ if st.spoiler && !revealed => links.push((chars, chars + n, Link::Spoiler)),
            _ => {}
        }
        at_line_start = text.ends_with('\n');
        let lead = if prev_italic && !st.italic { 2.5 } else { 0.0 };
        prev_italic = st.italic;
        job.append(&text, lead, f);
        chars += n;
    }
    (job, links)
}

fn format_timestamp(secs: i64, style: char) -> String {
    use chrono::{Local, TimeZone};
    let Some(t) = Local.timestamp_opt(secs, 0).single() else { return "<invalid time>".into() };
    match style {
        't' => t.format("%H:%M").to_string(),
        'T' => t.format("%H:%M:%S").to_string(),
        'd' => t.format("%Y-%m-%d").to_string(),
        'D' => t.format("%B %-d, %Y").to_string(),
        'F' => t.format("%A, %B %-d, %Y %H:%M").to_string(),
        'R' => {
            let d = Local::now().signed_duration_since(t).num_seconds();
            let (n, future) = (d.abs(), d < 0);
            let (v, unit) = match n {
                0..=59 => (n, "second"),
                60..=3599 => (n / 60, "minute"),
                3600..=86399 => (n / 3600, "hour"),
                _ => (n / 86400, "day"),
            };
            let s = if v == 1 { "" } else { "s" };
            if future { format!("in {v} {unit}{s}") } else { format!("{v} {unit}{s} ago") }
        }
        _ => t.format("%Y-%m-%d %H:%M").to_string(),
    }
}

fn layout(ctx: &egui::Context, job: LayoutJob) -> Arc<Galley> {
    ctx.fonts_mut(|f| f.layout_job(job))
}

pub fn text_galley(ctx: &egui::Context, text: &str, size: f32, color: Color32, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(
        text.to_string(),
        TextFormat { font_id: FontId::new(size, FontFamily::Proportional), color, line_height: Some((size * 1.32).round()), ..Default::default() },
    );
    job.wrap.max_width = width;
    layout(ctx, job)
}

/// Lays out one row at `width` (the full list width).
pub fn build(ctx: &egui::Context, row: &Row, env: &Env, width: f32, revealed: bool) -> Built {
    let p = &env.p;
    let m = env.m;
    let content_x = LEFT + m.gutter;
    let cw = (width - content_x - 16.0).max(80.0);
    let mut y = m.pad_y + if row.grouped { 0.0 } else { m.group_gap };
    let mut items = Vec::new();
    let small = env.font_size * 0.85;
    let msg = &row.msg;

    if let Some(r) = &msg.reply {
        let text = format!("↪ {}  {}", r.author, r.content.replace('\n', " "));
        let mut job = LayoutJob::single_section(text, fmt(env, small, p.muted));
        job.wrap.max_width = cw;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('…');
        let g = layout(ctx, job);
        let h = g.size().y;
        items.push(Item::Reply { y, galley: g });
        y += h + 2.0;
    }

    if !row.grouped {
        let color = theme::role_color(msg.member.as_ref().and_then(|mb| env.guild.and_then(|g| g.color_for(&mb.roles))), p);
        let mut nf = fmt(env, env.font_size, color);
        if env.fonts.bold {
            nf.font_id = FontId::new(env.font_size, FontFamily::Name("bold".into()));
        }
        let name = layout(ctx, LayoutJob::single_section(msg.author_name().to_string(), nf));
        let time = layout(ctx, LayoutJob::single_section(format_time(&msg.timestamp, chrono::Local::now()), fmt(env, small, p.muted)));
        let h = name.size().y.max(time.size().y);
        items.push(Item::Header { y, name, time, bot: msg.author.bot, name_color: color });
        y += h + 1.0;
    }

    if !msg.content.is_empty() {
        let (job, _) = body_job(row, env, cw, revealed);
        let g = layout(ctx, job);
        let h = g.size().y;
        items.push(Item::Body { y, galley: g });
        y += h + 2.0;
    }
    let body_links = if msg.content.is_empty() { Vec::new() } else { body_job(row, env, cw, revealed).1 };

    for a in &msg.attachments {
        if a.is_image() && env.show_images {
            let size = fit(a.width.unwrap_or(300), a.height.unwrap_or(200), MAX_IMG_W.min(cw), MAX_IMG_H);
            let url = if a.proxy_url.is_empty() { a.url.clone() } else { a.proxy_url.clone() };
            items.push(Item::Image { y, size, url, full: a.url.clone(), w: a.width.unwrap_or(300), h: a.height.unwrap_or(200) });
            y += size.y + 4.0;
        } else {
            let text = format!("📎 {}  ({})", a.filename, human_size(a.size));
            let g = layout(ctx, LayoutJob::single_section(text, fmt(env, env.font_size, mix(p.accent, p.text, 0.3))));
            let h = g.size().y;
            items.push(Item::File { y, label: g, url: a.url.clone() });
            y += h + 4.0;
        }
    }

    for e in &msg.embeds {
        let pad = 8.0;
        let ew = cw.min(480.0) - pad * 2.0 - 4.0;
        let mut texts = Vec::new();
        let mut ey = pad;
        if let Some(a) = e.author.as_ref().filter(|a| !a.name.is_empty()) {
            let g = text_galley(ctx, &a.name, small, p.muted, ew);
            texts.push((ey, g.clone(), None));
            ey += g.size().y + 2.0;
        }
        if let Some(t) = e.title.as_deref().filter(|t| !t.is_empty()) {
            let col = if e.url.is_some() { mix(p.accent, p.text, 0.3) } else { p.text };
            let g = text_galley(ctx, t, env.font_size, col, ew);
            texts.push((ey, g.clone(), e.url.clone()));
            ey += g.size().y + 2.0;
        }
        if let Some(d) = e.description.as_deref().filter(|d| !d.is_empty()) {
            let d: String = d.chars().take(600).collect();
            let g = text_galley(ctx, &d, env.font_size * 0.93, p.text, ew);
            texts.push((ey, g.clone(), None));
            ey += g.size().y + 4.0;
        }
        for f in e.fields.iter().take(8) {
            let g = text_galley(ctx, &format!("{}\n{}", f.name, f.value.chars().take(200).collect::<String>()), env.font_size * 0.9, p.text, ew);
            texts.push((ey, g.clone(), None));
            ey += g.size().y + 4.0;
        }
        if let Some(fo) = e.footer.as_ref().filter(|f| !f.name.is_empty()) {
            let g = text_galley(ctx, &fo.name, small, p.muted, ew);
            texts.push((ey, g.clone(), None));
            ey += g.size().y + 2.0;
        }
        let thumb = if env.show_images {
            e.image.as_ref().or(e.thumbnail.as_ref()).and_then(|im| {
                let u = im.proxy_url.clone().unwrap_or_else(|| im.url.clone());
                (!u.is_empty()).then(|| (u, fit(im.width.unwrap_or(320), im.height.unwrap_or(180), ew.min(MAX_IMG_W), 220.0)))
            })
        } else {
            None
        };
        if let Some((_, sz)) = &thumb {
            ey += sz.y + 4.0;
        }
        let color = e.color.map(|c| theme::c(feather_core::settings::Rgb::from_u32(c))).unwrap_or(p.raised);
        let h = ey + pad - 2.0;
        items.push(Item::Embed { y, height: h, color, texts, thumb });
        y += h + 4.0;
    }

    if !msg.reactions.is_empty() {
        let mut pills = Vec::new();
        let (mut x, mut ry) = (0.0f32, 0.0f32);
        let ph = env.font_size * 1.32 + 6.0;
        for r in &msg.reactions {
            let name = r.emoji.name.clone().unwrap_or_default();
            let label = if r.emoji.id.is_some() { format!(":{name}: {}", r.count) } else { format!("{name} {}", r.count) };
            let g = layout(ctx, LayoutJob::single_section(label, fmt(env, small.max(11.0), if r.me { p.accent } else { p.text })));
            let w = g.size().x + 14.0;
            if x + w > cw && x > 0.0 {
                x = 0.0;
                ry += ph + 3.0;
            }
            pills.push(Pill { x, y: ry, size: egui::vec2(w, ph), label: g, me: r.me, emoji: r.emoji.clone() });
            x += w + 4.0;
        }
        items.push(Item::Reactions { y, pills });
        y += ry + ph + 4.0;
    }

    let height = (y + m.pad_y).max(m.avatar + m.pad_y * 2.0 * 0.0 + if row.grouped { 0.0 } else { m.group_gap + m.pad_y * 2.0 });
    Built { width, height, items, body_links }
}

pub fn human_size(b: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let (mut v, mut i) = (b as f64, 0);
    while v >= 1024.0 && i < 3 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

/// Decides which rows start a new visual group.
pub fn regroup(rows: &mut [Row], from: usize) {
    use chrono::DateTime;
    for i in from..rows.len() {
        let grouped = i > 0 && {
            let (a, b) = (&rows[i - 1].msg, &rows[i].msg);
            let close = match (DateTime::parse_from_rfc3339(&a.timestamp), DateTime::parse_from_rfc3339(&b.timestamp)) {
                (Ok(x), Ok(y)) => (y - x).num_seconds().abs() < 420,
                _ => false,
            };
            a.author.id == b.author.id && close && b.reply.is_none() && b.kind == 0 && a.kind == 0
        };
        if rows[i].grouped != grouped {
            rows[i].grouped = grouped;
            rows[i].height = None;
            rows[i].built = None;
        }
    }
}
