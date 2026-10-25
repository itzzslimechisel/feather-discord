//! Bot token storage in the operating system's credential store
//! (Windows Credential Manager, macOS Keychain, Secret Service on Linux).
//!
//! The token is never written to Feather's own files. When no credential store is available
//! the token is kept in memory for the current session only and the user is told so.

use zeroize::Zeroizing;

const SERVICE: &str = "dev.feather.client";
const ACCOUNT: &str = "discord-bot-token";

pub type Secret = Zeroizing<String>;

#[derive(Debug, PartialEq)]
pub enum LoadResult {
    Found(Secret),
    NotFound,
    /// The credential store exists but could not be used (locked, missing daemon, ...).
    Unavailable(String),
}

fn entry() -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, ACCOUNT)
}

pub fn load() -> LoadResult {
    if std::env::var_os("FEATHER_NO_KEYRING").is_some() {
        return LoadResult::Unavailable("disabled by FEATHER_NO_KEYRING".into());
    }
    match entry().and_then(|e| e.get_password()) {
        Ok(t) => LoadResult::Found(Zeroizing::new(t)),
        Err(keyring::Error::NoEntry) => LoadResult::NotFound,
        Err(e) => LoadResult::Unavailable(e.to_string()),
    }
}

/// Returns Ok(true) if persisted, Ok(false) if no store is available (session-only).
pub fn save(token: &str) -> Result<bool, String> {
    if std::env::var_os("FEATHER_NO_KEYRING").is_some() {
        return Ok(false);
    }
    match entry().and_then(|e| e.set_password(token)) {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoStorageAccess(_)) | Err(keyring::Error::PlatformFailure(_)) => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}

pub fn clear() {
    if std::env::var_os("FEATHER_NO_KEYRING").is_some() {
        return;
    }
    if let Ok(e) = entry() {
        let _ = e.delete_credential();
    }
}

/// Basic shape check before a token is ever sent anywhere. Bot tokens are three
/// dot-separated URL-safe base64 segments. Prefixes such as "Bot " are stripped.
pub fn normalise(input: &str) -> Option<Secret> {
    let t = input.trim();
    let t = t.strip_prefix("Bot ").unwrap_or(t).trim();
    let parts: Vec<&str> = t.split('.').collect();
    let ok = parts.len() == 3
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    ok.then(|| Zeroizing::new(t.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalise_strips_prefix_and_rejects_garbage() {
        assert_eq!(normalise(" Bot aaa.bbb.ccc ").as_deref().map(|s| s.as_str()), Some("aaa.bbb.ccc"));
        assert!(normalise("not a token").is_none());
        assert!(normalise("a.b").is_none());
        assert!(normalise("a.b.c<script>").is_none());
    }
}
