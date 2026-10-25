//! User settings and the structured theme format.
//!
//! Themes are plain data (design tokens): colours, sizes and toggles. They are validated and
//! clamped on load and import, so a hand-edited or shared theme can never contain anything
//! executable or produce an unusable interface.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const THEME_FORMAT: &str = "feather-theme/1";

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Base {
    #[default]
    Dark,
    Light,
    Amoled,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    Compact,
    #[default]
    Cozy,
    Comfortable,
}

/// `#rrggbb` colour.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn parse(s: &str) -> Option<Rgb> {
        let h = s.trim().strip_prefix('#')?;
        if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        Some(Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
    }
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }
    pub fn from_u32(v: u32) -> Rgb {
        Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }
    /// Relative luminance (WCAG).
    pub fn luminance(self) -> f32 {
        fn ch(c: u8) -> f32 {
            let c = c as f32 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * ch(self.0) + 0.7152 * ch(self.1) + 0.0722 * ch(self.2)
    }
    pub fn contrast(self, other: Rgb) -> f32 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
}

impl Serialize for Rgb {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex())
    }
}
impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Rgb, D::Error> {
        let s = String::deserialize(d)?;
        Rgb::parse(&s).ok_or_else(|| serde::de::Error::custom("expected #rrggbb"))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Background {
    /// The base palette's own background.
    #[default]
    Default,
    Solid {
        color: Rgb,
    },
    /// Vertical gradient behind the message view.
    Gradient {
        top: Rgb,
        bottom: Rgb,
    },
}

/// Optional per-token colour overrides on top of the base palette.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ColorOverrides {
    pub text: Option<Rgb>,
    pub muted_text: Option<Rgb>,
    pub surface: Option<Rgb>,
    pub sidebar: Option<Rgb>,
    pub rail: Option<Rgb>,
    pub mention_highlight: Option<Rgb>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub format: String,
    pub name: String,
    pub base: Base,
    pub accent: Rgb,
    /// Body text size in points.
    pub font_size: f32,
    pub density: Density,
    /// Extra vertical space between message groups, in points.
    pub message_spacing: f32,
    pub sidebar_width: f32,
    pub member_list_width: f32,
    /// 0 = square corners.
    pub corner_radius: f32,
    pub background: Background,
    pub reduced_motion: bool,
    pub blinking_cursor: bool,
    pub show_avatars: bool,
    pub colors: ColorOverrides,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            format: THEME_FORMAT.into(),
            name: "Feather Dark".into(),
            base: Base::Dark,
            accent: Rgb(0x4f, 0x8c, 0xff),
            font_size: 14.0,
            density: Density::Cozy,
            message_spacing: 8.0,
            sidebar_width: 232.0,
            member_list_width: 220.0,
            corner_radius: 6.0,
            background: Background::Default,
            reduced_motion: false,
            blinking_cursor: false,
            show_avatars: true,
            colors: ColorOverrides::default(),
        }
    }
}

impl Theme {
    pub fn preset(base: Base) -> Theme {
        let name = match base {
            Base::Dark => "Feather Dark",
            Base::Light => "Feather Light",
            Base::Amoled => "Feather AMOLED",
        };
        Theme { base, name: name.into(), ..Theme::default() }
    }

    pub fn presets() -> Vec<Theme> {
        let mut v = vec![Theme::preset(Base::Dark), Theme::preset(Base::Light), Theme::preset(Base::Amoled)];
        v.push(Theme {
            name: "Forest".into(),
            accent: Rgb(0x3b, 0xa5, 0x5d),
            background: Background::Gradient { top: Rgb(0x14, 0x1d, 0x17), bottom: Rgb(0x0d, 0x12, 0x0f) },
            ..Theme::default()
        });
        v.push(Theme {
            name: "Compact Square".into(),
            density: Density::Compact,
            corner_radius: 0.0,
            message_spacing: 2.0,
            font_size: 13.0,
            show_avatars: false,
            ..Theme::default()
        });
        v
    }

    /// Clamps every numeric token to a usable range. Never fails.
    pub fn sanitize(&mut self) {
        fn clamp(v: &mut f32, lo: f32, hi: f32, def: f32) {
            *v = if v.is_finite() { v.clamp(lo, hi) } else { def };
        }
        let d = Theme::default();
        self.format = THEME_FORMAT.into();
        self.name = self.name.chars().filter(|c| !c.is_control()).take(64).collect();
        if self.name.trim().is_empty() {
            self.name = "Custom".into();
        }
        clamp(&mut self.font_size, 10.0, 24.0, d.font_size);
        clamp(&mut self.message_spacing, 0.0, 32.0, d.message_spacing);
        clamp(&mut self.sidebar_width, 160.0, 400.0, d.sidebar_width);
        clamp(&mut self.member_list_width, 160.0, 360.0, d.member_list_width);
        clamp(&mut self.corner_radius, 0.0, 16.0, d.corner_radius);
    }

    pub fn export_json(&self) -> String {
        let mut t = self.clone();
        t.sanitize();
        serde_json::to_string_pretty(&t).expect("theme serialises")
    }

    /// Parses a shared theme. Input is size-limited and only data fields are read.
    pub fn import_json(s: &str) -> Result<Theme, String> {
        if s.len() > 64 * 1024 {
            return Err("theme file is larger than 64 KiB".into());
        }
        let v: serde_json::Value = serde_json::from_str(s).map_err(|e| format!("not valid JSON: {e}"))?;
        match v.get("format").and_then(|f| f.as_str()) {
            Some(f) if f.starts_with("feather-theme/") => {}
            _ => return Err("not a Feather theme (missing \"format\": \"feather-theme/1\")".into()),
        }
        let mut t: Theme = serde_json::from_value(v).map_err(|e| format!("invalid theme: {e}"))?;
        t.sanitize();
        Ok(t)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Intents {
    /// Privileged: required to read message text in servers (except mentions/DMs).
    pub message_content: bool,
    /// Privileged: required for the member list.
    pub guild_members: bool,
}

impl Default for Intents {
    fn default() -> Self {
        Intents { message_content: true, guild_members: false }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub theme: Theme,
    pub saved_themes: Vec<Theme>,
    pub intents: Intents,
    pub notifications: bool,
    pub notify_all_dms: bool,
    pub show_member_list: bool,
    pub image_previews: bool,
    pub confirm_links: bool,
    /// Load a system CJK/symbol font (uses extra memory).
    pub wide_fonts: bool,
    /// Messages kept on disk per channel.
    pub disk_messages_per_channel: u32,
    pub window_size: Option<(f32, f32)>,
    pub last_channel: Option<crate::model::Id>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: 1,
            theme: Theme::default(),
            saved_themes: Vec::new(),
            intents: Intents::default(),
            notifications: true,
            notify_all_dms: true,
            show_member_list: true,
            image_previews: true,
            confirm_links: true,
            wide_fonts: false,
            disk_messages_per_channel: 2000,
            window_size: None,
            last_channel: None,
        }
    }
}

#[derive(Debug)]
pub struct LoadedSettings {
    pub settings: Settings,
    /// Set when the file existed but was unreadable; it has been moved aside.
    pub recovered_from: Option<PathBuf>,
}

impl Settings {
    pub fn sanitize(&mut self) {
        self.theme.sanitize();
        self.saved_themes.truncate(50);
        self.saved_themes.iter_mut().for_each(Theme::sanitize);
        self.disk_messages_per_channel = self.disk_messages_per_channel.clamp(100, 50_000);
        if let Some((w, h)) = self.window_size {
            if !(w.is_finite() && h.is_finite()) || w < 320.0 || h < 240.0 || w > 16384.0 || h > 16384.0 {
                self.window_size = None;
            }
        }
    }

    pub fn load(path: &Path) -> LoadedSettings {
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(_) => return LoadedSettings { settings: Settings::default(), recovered_from: None },
        };
        match serde_json::from_str::<Settings>(&raw) {
            Ok(mut s) => {
                s.sanitize();
                LoadedSettings { settings: s, recovered_from: None }
            }
            Err(e) => {
                log::warn!("settings file unreadable ({e}); using defaults");
                let aside = path.with_extension("json.bad");
                let _ = std::fs::rename(path, &aside);
                LoadedSettings { settings: Settings::default(), recovered_from: Some(aside) }
            }
        }
    }

    /// Atomic write: temp file, fsync, rename. A crash mid-write never corrupts settings.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(serde_json::to_string_pretty(self).expect("settings serialise").as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_roundtrip_and_clamp() {
        let mut t = Theme::preset(Base::Amoled);
        t.font_size = 400.0;
        t.corner_radius = f32::NAN;
        let j = t.export_json();
        let back = Theme::import_json(&j).unwrap();
        assert_eq!(back.font_size, 24.0);
        assert_eq!(back.corner_radius, Theme::default().corner_radius);
        assert_eq!(back.base, Base::Amoled);
    }

    #[test]
    fn import_rejects_foreign_and_oversized() {
        assert!(Theme::import_json("{}").is_err());
        assert!(Theme::import_json("not json").is_err());
        assert!(Theme::import_json(&"x".repeat(70_000)).is_err());
        assert!(Theme::import_json(r#"{"format":"feather-theme/1","accent":"red"}"#).is_err());
        let t = Theme::import_json(r##"{"format":"feather-theme/1","accent":"#ff0000","unknown":1}"##).unwrap();
        assert_eq!(t.accent, Rgb(255, 0, 0));
    }

    #[test]
    fn settings_atomic_save_and_corrupt_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let mut s = Settings::default();
        s.theme.accent = Rgb(1, 2, 3);
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p).settings.theme.accent, Rgb(1, 2, 3));
        std::fs::write(&p, "{ broken").unwrap();
        let l = Settings::load(&p);
        assert!(l.recovered_from.is_some());
        assert_eq!(l.settings, Settings::default());
        assert!(!p.exists());
    }

    #[test]
    fn contrast_ratio() {
        assert!((Rgb(0, 0, 0).contrast(Rgb(255, 255, 255)) - 21.0).abs() < 0.01);
    }
}
