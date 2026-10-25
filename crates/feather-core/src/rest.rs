//! Discord REST client with rate-limit-aware scheduling.
//!
//! * Per-route buckets are tracked from `X-RateLimit-Remaining` / `X-RateLimit-Reset-After`, and a
//!   request waits locally instead of being sent into a known-exhausted bucket.
//! * 429 responses honour `retry_after` (and the global flag) and are retried a bounded number
//!   of times. 5xx/network errors are retried only for idempotent requests.
//! * The bot token is only ever attached to requests for the configured API base.

use crate::creds::Secret;
use crate::model::{Channel, Id, Member, Message, User};
use reqwest::header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
const MAX_ATTEMPTS: u32 = 4;

#[derive(Debug, Clone, PartialEq)]
pub enum RestError {
    /// 401: token revoked or invalid.
    Unauthorized,
    /// 403 with Discord's error message (usually a missing permission or intent).
    Forbidden(String),
    NotFound,
    RateLimited,
    Http(u16, String),
    Network(String),
    Decode(String),
    TooLarge,
}

impl std::fmt::Display for RestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestError::Unauthorized => write!(f, "the bot token was rejected (401)"),
            RestError::Forbidden(m) => write!(f, "missing permission: {m}"),
            RestError::NotFound => write!(f, "not found"),
            RestError::RateLimited => write!(f, "rate limited, try again shortly"),
            RestError::Http(s, m) => write!(f, "HTTP {s}: {m}"),
            RestError::Network(m) => write!(f, "network error: {m}"),
            RestError::Decode(m) => write!(f, "unexpected response: {m}"),
            RestError::TooLarge => write!(f, "file is larger than {} MiB", MAX_UPLOAD_BYTES / 1024 / 1024),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    remaining: u32,
    reset_at: Instant,
}

#[derive(Default)]
struct Limits {
    buckets: HashMap<String, Bucket>,
    global_until: Option<Instant>,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct RestStats {
    pub requests: u64,
    pub rate_limited: u64,
    pub bytes_in: u64,
}

pub struct Rest {
    http: reqwest::Client,
    base: String,
    token: RwLock<Option<Secret>>,
    limits: Mutex<Limits>,
    stats: Mutex<RestStats>,
}

pub struct Upload {
    pub filename: String,
    pub bytes: Vec<u8>,
}

impl Rest {
    pub fn new(api_base: &str) -> Rest {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(2)
            .build()
            .expect("http client");
        Rest {
            http,
            base: api_base.trim_end_matches('/').to_string(),
            token: RwLock::new(None),
            limits: Mutex::new(Limits::default()),
            stats: Mutex::new(RestStats::default()),
        }
    }

    pub fn set_token(&self, t: Option<Secret>) {
        *self.token.write().unwrap() = t;
    }

    pub fn stats(&self) -> RestStats {
        *self.stats.lock().unwrap()
    }

    fn auth(&self) -> Option<String> {
        self.token.read().unwrap().as_ref().map(|t| format!("Bot {}", t.as_str()))
    }

    /// Time to wait before a request on `key` may be sent, reserving a slot if none.
    fn reserve(&self, key: &str) -> Option<Duration> {
        let mut l = self.limits.lock().unwrap();
        let now = Instant::now();
        if let Some(g) = l.global_until {
            if g > now {
                return Some(g - now);
            }
            l.global_until = None;
        }
        if let Some(b) = l.buckets.get_mut(key) {
            if b.reset_at <= now {
                l.buckets.remove(key);
            } else if b.remaining == 0 {
                return Some(b.reset_at - now);
            } else {
                b.remaining -= 1;
            }
        }
        None
    }

    fn update_limits(&self, key: &str, h: &HeaderMap) {
        let num = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
        if let (Some(rem), Some(after)) = (num("x-ratelimit-remaining"), num("x-ratelimit-reset-after")) {
            let mut l = self.limits.lock().unwrap();
            l.buckets.insert(
                key.to_string(),
                Bucket { remaining: rem.max(0.0) as u32, reset_at: Instant::now() + Duration::from_secs_f64(after.clamp(0.0, 3600.0)) },
            );
            // Bound the table: drop expired entries when it grows.
            if l.buckets.len() > 256 {
                let now = Instant::now();
                l.buckets.retain(|_, b| b.reset_at > now);
            }
        }
    }

    async fn exec<F>(&self, key: &str, idempotent: bool, build: F) -> Result<reqwest::Response, RestError>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let auth = self.auth().ok_or(RestError::Unauthorized)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            while let Some(wait) = self.reserve(key) {
                log::debug!("rate limit: waiting {wait:?} for {key}");
                tokio::time::sleep(wait.min(Duration::from_secs(60))).await;
            }
            self.stats.lock().unwrap().requests += 1;
            let resp = build()
                .header(AUTHORIZATION, &auth)
                .header(USER_AGENT, concat!("DiscordBot (https://github.com/feather-client/feather, ", env!("CARGO_PKG_VERSION"), ")"))
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) if idempotent && attempt < 3 => {
                    log::info!("request {key} failed ({e}); retrying");
                    tokio::time::sleep(crate::diag::backoff(attempt, Duration::from_millis(500), Duration::from_secs(8))).await;
                    continue;
                }
                Err(e) => return Err(RestError::Network(without_url(e))),
            };
            self.update_limits(key, resp.headers());
            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                self.stats.lock().unwrap().rate_limited += 1;
                let global = resp.headers().get("x-ratelimit-global").is_some();
                let header_after = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                let after = body["retry_after"].as_f64().or(header_after).unwrap_or(1.0).clamp(0.0, 300.0);
                let until = Instant::now() + Duration::from_secs_f64(after);
                {
                    let mut l = self.limits.lock().unwrap();
                    if global || body["global"].as_bool() == Some(true) {
                        l.global_until = Some(until);
                    } else {
                        l.buckets.insert(key.to_string(), Bucket { remaining: 0, reset_at: until });
                    }
                }
                if attempt >= MAX_ATTEMPTS {
                    return Err(RestError::RateLimited);
                }
                continue;
            }
            if status.is_server_error() && idempotent && attempt < 3 {
                tokio::time::sleep(crate::diag::backoff(attempt, Duration::from_millis(500), Duration::from_secs(8))).await;
                continue;
            }
            return match status {
                s if s.is_success() => Ok(resp),
                StatusCode::UNAUTHORIZED => Err(RestError::Unauthorized),
                StatusCode::FORBIDDEN => Err(RestError::Forbidden(error_message(resp).await)),
                StatusCode::NOT_FOUND => Err(RestError::NotFound),
                StatusCode::PAYLOAD_TOO_LARGE => Err(RestError::TooLarge),
                s => Err(RestError::Http(s.as_u16(), error_message(resp).await)),
            };
        }
    }

    async fn json<T: DeserializeOwned>(&self, resp: reqwest::Response) -> Result<T, RestError> {
        let bytes = resp.bytes().await.map_err(|e| RestError::Network(without_url(e)))?;
        self.stats.lock().unwrap().bytes_in += bytes.len() as u64;
        serde_json::from_slice(&bytes).map_err(|e| RestError::Decode(e.to_string()))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub async fn me(&self) -> Result<User, RestError> {
        let r = self.exec("GET /users/@me", true, || self.http.get(self.url("/users/@me"))).await?;
        self.json(r).await
    }

    /// Recommended gateway URL for bots.
    pub async fn gateway_url(&self) -> Result<String, RestError> {
        let r = self.exec("GET /gateway/bot", true, || self.http.get(self.url("/gateway/bot"))).await?;
        let v: serde_json::Value = self.json(r).await?;
        v["url"].as_str().map(str::to_string).ok_or_else(|| RestError::Decode("no url".into()))
    }

    /// History page. `before`/`after` are exclusive; at most 100 per request.
    pub async fn messages(&self, channel: Id, before: Option<Id>, after: Option<Id>, limit: u8) -> Result<Vec<Message>, RestError> {
        let mut path = format!("/channels/{channel}/messages?limit={}", limit.clamp(1, 100));
        if let Some(b) = before {
            path.push_str(&format!("&before={b}"));
        } else if let Some(a) = after {
            path.push_str(&format!("&after={a}"));
        }
        let key = format!("GET /channels/{channel}/messages");
        let r = self.exec(&key, true, || self.http.get(self.url(&path))).await?;
        let v: Vec<Message> = self.json(r).await?;
        Ok(v.into_iter().map(Message::compact).collect())
    }

    pub async fn send_message(
        &self,
        channel: Id,
        content: &str,
        reply_to: Option<Id>,
        nonce: &str,
        files: &[Upload],
    ) -> Result<Message, RestError> {
        if files.iter().any(|f| f.bytes.len() > MAX_UPLOAD_BYTES) {
            return Err(RestError::TooLarge);
        }
        let mut payload = json!({ "content": content, "nonce": nonce, "enforce_nonce": true,
            "allowed_mentions": { "parse": ["users", "roles", "everyone"], "replied_user": true } });
        if let Some(r) = reply_to {
            payload["message_reference"] = json!({ "message_id": r, "fail_if_not_exists": false });
        }
        if !files.is_empty() {
            payload["attachments"] =
                files.iter().enumerate().map(|(i, f)| json!({ "id": i, "filename": f.filename })).collect();
        }
        let key = format!("POST /channels/{channel}/messages");
        let url = self.url(&format!("/channels/{channel}/messages"));
        let r = self
            .exec(&key, false, || {
                if files.is_empty() {
                    self.http.post(&url).json(&payload)
                } else {
                    let mut form = reqwest::multipart::Form::new().text("payload_json", payload.to_string());
                    for (i, f) in files.iter().enumerate() {
                        let part = reqwest::multipart::Part::bytes(f.bytes.clone()).file_name(f.filename.clone());
                        form = form.part(format!("files[{i}]"), part);
                    }
                    self.http.post(&url).multipart(form)
                }
            })
            .await?;
        Ok(self.json::<Message>(r).await?.compact())
    }

    pub async fn edit_message(&self, channel: Id, id: Id, content: &str) -> Result<Message, RestError> {
        let key = format!("PATCH /channels/{channel}/messages");
        let url = self.url(&format!("/channels/{channel}/messages/{id}"));
        let body = json!({ "content": content });
        let r = self.exec(&key, false, || self.http.patch(&url).json(&body)).await?;
        Ok(self.json::<Message>(r).await?.compact())
    }

    pub async fn delete_message(&self, channel: Id, id: Id) -> Result<(), RestError> {
        let key = format!("DELETE /channels/{channel}/messages");
        let url = self.url(&format!("/channels/{channel}/messages/{id}"));
        self.exec(&key, true, || self.http.delete(&url)).await.map(|_| ())
    }

    pub async fn react(&self, channel: Id, id: Id, emoji_api_form: &str, add: bool) -> Result<(), RestError> {
        let e = url::form_urlencoded::byte_serialize(emoji_api_form.as_bytes()).collect::<String>();
        let url = self.url(&format!("/channels/{channel}/messages/{id}/reactions/{e}/@me"));
        let key = format!("REACT /channels/{channel}");
        let m = if add { Method::PUT } else { Method::DELETE };
        self.exec(&key, true, || self.http.request(m.clone(), &url).header(CONTENT_TYPE, "application/json")).await.map(|_| ())
    }

    pub async fn open_dm(&self, user: Id) -> Result<Channel, RestError> {
        let url = self.url("/users/@me/channels");
        let body = json!({ "recipient_id": user });
        let r = self.exec("POST /users/@me/channels", true, || self.http.post(&url).json(&body)).await?;
        self.json(r).await
    }

    /// Requires the privileged Server Members intent; returns Forbidden otherwise.
    pub async fn guild_members(&self, guild: Id, limit: u16) -> Result<Vec<Member>, RestError> {
        let url = self.url(&format!("/guilds/{guild}/members?limit={}", limit.clamp(1, 1000)));
        let key = format!("GET /guilds/{guild}/members");
        let r = self.exec(&key, true, || self.http.get(&url)).await?;
        self.json(r).await
    }
}

async fn error_message(resp: reqwest::Response) -> String {
    let v: serde_json::Value = resp.json().await.unwrap_or_default();
    v["message"].as_str().unwrap_or("request failed").chars().take(200).collect()
}

/// reqwest errors include the URL; strip it so query strings never reach logs or the UI.
fn without_url(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_reservation_blocks_when_exhausted() {
        let r = Rest::new("http://127.0.0.1:1");
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining", "1".parse().unwrap());
        h.insert("x-ratelimit-reset-after", "5".parse().unwrap());
        r.update_limits("k", &h);
        assert!(r.reserve("k").is_none(), "one slot left");
        let w = r.reserve("k").expect("exhausted");
        assert!(w > Duration::from_secs(4) && w <= Duration::from_secs(5));
        assert!(r.reserve("other").is_none());
    }

    #[test]
    fn global_limit_blocks_everything() {
        let r = Rest::new("http://127.0.0.1:1");
        r.limits.lock().unwrap().global_until = Some(Instant::now() + Duration::from_secs(2));
        assert!(r.reserve("a").is_some());
        assert!(r.reserve("b").is_some());
    }
}
