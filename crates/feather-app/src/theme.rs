//! Turns a data-only [`Theme`] into egui styling. Applied only when the theme changes, so
//! switching themes costs one style update and never re-creates windows or reloads data.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};
use feather_core::settings::{Background, Base, Density, Rgb, Theme};

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub rail: Color32,
    pub sidebar: Color32,
    pub surface: Color32,
    pub raised: Color32,
    pub hover: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    pub mention_bg: Color32,
    pub danger: Color32,
    pub online: Color32,
}

pub fn c(r: Rgb) -> Color32 {
    Color32::from_rgb(r.0, r.1, r.2)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

pub fn palette(t: &Theme) -> Palette {
    let accent = c(t.accent);
    let (rail, sidebar, surface, raised, text, muted) = match t.base {
        Base::Dark => (0x1e1f22, 0x2b2d31, 0x313338, 0x383a40, 0xdbdee1, 0x949ba4),
        Base::Light => (0xe3e5e8, 0xf2f3f5, 0xffffff, 0xebedef, 0x1f2023, 0x5c5f66),
        Base::Amoled => (0x000000, 0x0a0a0a, 0x000000, 0x141414, 0xe6e6e6, 0x8a8a8a),
    };
    let hx = |v: u32| Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
    let o = &t.colors;
    let surface_c = o.surface.map(c).unwrap_or(hx(surface));
    let text_c = o.text.map(c).unwrap_or(hx(text));
    let dark = t.base != Base::Light;
    Palette {
        rail: o.rail.map(c).unwrap_or(hx(rail)),
        sidebar: o.sidebar.map(c).unwrap_or(hx(sidebar)),
        surface: surface_c,
        raised: hx(raised),
        hover: mix(surface_c, text_c, 0.07),
        text: text_c,
        muted: o.muted_text.map(c).unwrap_or(hx(muted)),
        accent,
        accent_text: if c_luma(accent) > 0.45 { Color32::BLACK } else { Color32::WHITE },
        mention_bg: o.mention_highlight.map(c).unwrap_or_else(|| mix(surface_c, accent, if dark { 0.22 } else { 0.16 })),
        danger: if dark { Color32::from_rgb(0xf2, 0x3f, 0x43) } else { Color32::from_rgb(0xd8, 0x32, 0x3c) },
        online: Color32::from_rgb(0x23, 0xa5, 0x59),
    }
}

fn c_luma(c: Color32) -> f32 {
    Rgb(c.r(), c.g(), c.b()).luminance()
}

/// Metrics derived from density, shared by layout code.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub avatar: f32,
    pub pad_y: f32,
    pub group_gap: f32,
    pub gutter: f32,
}

pub fn metrics(t: &Theme) -> Metrics {
    let (avatar, pad_y) = match t.density {
        Density::Compact => (24.0, 1.0),
        Density::Cozy => (36.0, 2.0),
        Density::Comfortable => (44.0, 5.0),
    };
    Metrics { avatar: if t.show_avatars { avatar } else { 0.0 }, pad_y, group_gap: t.message_spacing, gutter: if t.show_avatars { avatar + 12.0 } else { 0.0 } }
}

pub fn apply(ctx: &egui::Context, t: &Theme) {
    let p = palette(t);
    let mut v = if t.base == Base::Light { egui::Visuals::light() } else { egui::Visuals::dark() };
    let r = CornerRadius::same(t.corner_radius.round() as u8);
    v.override_text_color = Some(p.text);
    v.panel_fill = p.surface;
    v.window_fill = p.sidebar;
    v.window_stroke = Stroke::new(1.0, p.raised);
    v.extreme_bg_color = p.rail;
    v.faint_bg_color = p.hover;
    v.code_bg_color = p.raised;
    v.hyperlink_color = mix(p.accent, p.text, if t.base == Base::Light { 0.0 } else { 0.35 });
    v.selection.bg_fill = mix(p.surface, p.accent, 0.45);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.window_corner_radius = r;
    v.menu_corner_radius = r;
    v.text_cursor.blink = t.blinking_cursor;
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = r;
    }
    v.widgets.noninteractive.bg_fill = p.sidebar;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.raised);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.bg_fill = p.raised;
    v.widgets.inactive.weak_bg_fill = p.raised;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.hovered.bg_fill = mix(p.raised, p.text, 0.08);
    v.widgets.hovered.weak_bg_fill = mix(p.raised, p.text, 0.08);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.active.bg_fill = p.accent;
    v.widgets.active.weak_bg_fill = p.accent;
    v.widgets.active.fg_stroke = Stroke::new(1.0, p.accent_text);
    ctx.set_visuals(v);

    ctx.style_mut(|s| {
        let fs = t.font_size;
        s.text_styles = [
            (TextStyle::Small, FontId::new(fs * 0.8, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(fs, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(fs, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(fs * 1.35, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(fs * 0.93, FontFamily::Monospace)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(6.0, 4.0);
        s.spacing.button_padding = egui::vec2(8.0, 4.0);
        s.spacing.scroll.bar_width = 8.0;
        s.spacing.scroll.floating = true;
        s.animation_time = if t.reduced_motion { 0.0 } else { 0.08 };
        s.interaction.selectable_labels = true;
    });
}

/// Colour behind the message view (solid colour or gradient top colour).
pub fn background_fill(t: &Theme, p: &Palette) -> Color32 {
    match t.background {
        Background::Default => p.surface,
        Background::Solid { color } => c(color),
        Background::Gradient { top, .. } => c(top),
    }
}

/// Paints the optional background behind the message area.
pub fn paint_background(ui: &egui::Ui, rect: egui::Rect, t: &Theme, p: &Palette) {
    match t.background {
        Background::Default => {}
        Background::Solid { color } => {
            ui.painter().rect_filled(rect, 0, c(color));
        }
        Background::Gradient { top, bottom } => {
            let mut mesh = egui::Mesh::default();
            let (a, b) = (c(top), c(bottom));
            let i = mesh.vertices.len() as u32;
            let uv = egui::pos2(0.0, 0.0);
            mesh.vertices.push(egui::epaint::Vertex { pos: rect.left_top(), uv, color: a });
            mesh.vertices.push(egui::epaint::Vertex { pos: rect.right_top(), uv, color: a });
            mesh.vertices.push(egui::epaint::Vertex { pos: rect.right_bottom(), uv, color: b });
            mesh.vertices.push(egui::epaint::Vertex { pos: rect.left_bottom(), uv, color: b });
            mesh.indices.extend_from_slice(&[i, i + 1, i + 2, i, i + 2, i + 3]);
            ui.painter().add(egui::Shape::mesh(mesh));
        }
    }
    let _ = p;
}

/// Colour for a role/username; falls back to the normal text colour.
pub fn role_color(rgb: Option<u32>, p: &Palette) -> Color32 {
    match rgb {
        Some(v) if v != 0 => Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8),
        _ => p.text,
    }
}
