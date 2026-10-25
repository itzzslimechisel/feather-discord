//! Service endpoints. Production endpoints are fixed; a loopback-only override exists so the
//! test suite and benchmarks can run against `feather-mock` without ever sending a real token
//! to an arbitrary host.

use url::Url;

pub const API_VERSION: u8 = 10;

#[derive(Clone, Debug)]
pub struct Endpoints {
    /// REST base including version, e.g. `https://discord.com/api/v10`.
    pub api: String,
    /// Gateway URL used when no cached URL is available.
    pub gateway: String,
    /// Hosts the image loader may fetch from (never sent credentials).
    pub media_hosts: Vec<String>,
    pub cdn: String,
    pub is_override: bool,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            api: format!("https://discord.com/api/v{API_VERSION}"),
            gateway: "wss://gateway.discord.gg".into(),
            media_hosts: vec!["cdn.discordapp.com".into(), "media.discordapp.net".into()],
            cdn: "https://cdn.discordapp.com".into(),
            is_override: false,
        }
    }
}

pub fn is_loopback(u: &str) -> bool {
    Url::parse(u)
        .ok()
        .and_then(|u| u.host_str().map(|h| matches!(h, "127.0.0.1" | "localhost" | "[::1]")))
        .unwrap_or(false)
}

impl Endpoints {
    /// Reads `FEATHER_API_BASE` (e.g. `http://127.0.0.1:7777`). Non-loopback values are refused.
    pub fn from_env() -> Endpoints {
        match std::env::var("FEATHER_API_BASE") {
            Ok(base) if is_loopback(&base) => {
                let base = base.trim_end_matches('/').to_string();
                let host = Url::parse(&base).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
                let port = Url::parse(&base).ok().and_then(|u| u.port()).map(|p| format!(":{p}")).unwrap_or_default();
                log::warn!("using loopback test endpoints at {base}");
                Endpoints {
                    api: format!("{base}/api/v{API_VERSION}"),
                    gateway: format!("ws://{host}{port}/gateway"),
                    media_hosts: vec![host],
                    cdn: format!("{base}/cdn"),
                    is_override: true,
                }
            }
            Ok(base) => {
                log::error!("ignoring FEATHER_API_BASE={base}: only loopback test servers are allowed");
                Endpoints::default()
            }
            Err(_) => Endpoints::default(),
        }
    }

    pub fn avatar_url(&self, user_id: crate::model::Id, hash: &str, size: u32) -> String {
        format!("{}/avatars/{user_id}/{hash}.png?size={size}", self.cdn)
    }
    pub fn guild_icon_url(&self, guild_id: crate::model::Id, hash: &str, size: u32) -> String {
        format!("{}/icons/{guild_id}/{hash}.png?size={size}", self.cdn)
    }
    pub fn emoji_url(&self, id: crate::model::Id, size: u32) -> String {
        format!("{}/emojis/{id}.png?size={size}", self.cdn)
    }
    /// Discord's default avatars, indexed by `(user_id >> 22) % 6`.
    pub fn default_avatar_url(&self, user_id: crate::model::Id) -> String {
        format!("{}/embed/avatars/{}.png", self.cdn, (user_id.0 >> 22) % 6)
    }

    pub fn media_allowed(&self, u: &str) -> bool {
        let Ok(u) = Url::parse(u) else { return false };
        let scheme_ok = u.scheme() == "https" || (self.is_override && u.scheme() == "http");
        scheme_ok && u.host_str().is_some_and(|h| self.media_hosts.iter().any(|m| m == h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loopback_detection() {
        assert!(is_loopback("http://127.0.0.1:7777"));
        assert!(is_loopback("http://localhost:1"));
        assert!(!is_loopback("http://example.com"));
        assert!(!is_loopback("http://127.0.0.1.evil.com"));
    }
    #[test]
    fn media_allowlist() {
        let e = Endpoints::default();
        assert!(e.media_allowed("https://cdn.discordapp.com/avatars/1/a.png"));
        assert!(!e.media_allowed("http://cdn.discordapp.com/avatars/1/a.png"));
        assert!(!e.media_allowed("https://evil.example/x.png"));
        assert!(!e.media_allowed("file:///etc/passwd"));
    }
}
