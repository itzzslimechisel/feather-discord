//! Discord data model: only the fields Feather actually uses.
//!
//! Every struct tolerates missing fields (`#[serde(default)]`) because gateway payloads are
//! frequently partial, and unknown fields are ignored so new API additions never break parsing.

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Discord epoch (2015-01-01T00:00:00Z) in Unix milliseconds.
pub const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// A Discord snowflake. Serialised as a string, as the API does.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Id(pub u64);

impl Id {
    pub fn timestamp_ms(self) -> u64 {
        (self.0 >> 22) + DISCORD_EPOCH_MS
    }
    /// Smallest snowflake that could have been created at `unix_ms`.
    pub fn from_timestamp_ms(unix_ms: u64) -> Id {
        Id(unix_ms.saturating_sub(DISCORD_EPOCH_MS) << 22)
    }
    pub fn is_set(self) -> bool {
        self.0 != 0
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Id, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = Id;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a snowflake string or integer")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Id, E> {
                v.parse().map(Id).map_err(E::custom)
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Id, E> {
                Ok(Id(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Id, E> {
                u64::try_from(v).map(Id).map_err(E::custom)
            }
        }
        d.deserialize_any(V)
    }
}

/// Deserialises `null` as the type's default (Discord sends explicit nulls for many fields).
pub fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Permission bitfields arrive as decimal strings.
pub fn perm_string<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum P {
        S(String),
        N(u64),
    }
    match Option::<P>::deserialize(d)? {
        None => Ok(0),
        Some(P::N(n)) => Ok(n),
        Some(P::S(s)) => s.parse().map_err(de::Error::custom),
    }
}

fn ser_perm<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(v)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct User {
    pub id: Id,
    pub username: String,
    #[serde(deserialize_with = "null_default")]
    pub global_name: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub avatar: Option<String>,
    pub bot: bool,
}

impl User {
    pub fn display_name(&self) -> &str {
        self.global_name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&self.username)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Role {
    pub id: Id,
    pub name: String,
    pub color: u32,
    pub position: i32,
    #[serde(deserialize_with = "perm_string", serialize_with = "ser_perm")]
    pub permissions: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Member {
    #[serde(deserialize_with = "null_default")]
    pub user: Option<User>,
    #[serde(deserialize_with = "null_default")]
    pub nick: Option<String>,
    pub roles: Vec<Id>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Overwrite {
    pub id: Id,
    /// 0 = role, 1 = member
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(deserialize_with = "perm_string", serialize_with = "ser_perm")]
    pub allow: u64,
    #[serde(deserialize_with = "perm_string", serialize_with = "ser_perm")]
    pub deny: u64,
}

pub mod channel_type {
    pub const GUILD_TEXT: u8 = 0;
    pub const DM: u8 = 1;
    pub const GUILD_VOICE: u8 = 2;
    pub const GROUP_DM: u8 = 3;
    pub const GUILD_CATEGORY: u8 = 4;
    pub const GUILD_ANNOUNCEMENT: u8 = 5;
    pub const ANNOUNCEMENT_THREAD: u8 = 10;
    pub const PUBLIC_THREAD: u8 = 11;
    pub const PRIVATE_THREAD: u8 = 12;
    pub const GUILD_STAGE_VOICE: u8 = 13;
    pub const GUILD_FORUM: u8 = 15;
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Channel {
    pub id: Id,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(deserialize_with = "null_default")]
    pub guild_id: Option<Id>,
    #[serde(deserialize_with = "null_default")]
    pub name: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub topic: Option<String>,
    pub position: i32,
    #[serde(deserialize_with = "null_default")]
    pub parent_id: Option<Id>,
    #[serde(deserialize_with = "null_default")]
    pub last_message_id: Option<Id>,
    pub permission_overwrites: Vec<Overwrite>,
    pub recipients: Vec<User>,
    pub nsfw: bool,
    /// Computed locally from roles and overwrites; not part of the API payload.
    #[serde(rename = "feather_perms")]
    pub my_permissions: u64,
}

impl Channel {
    /// Channels Feather can show a message view for.
    pub fn is_text_like(&self) -> bool {
        use channel_type::*;
        matches!(
            self.kind,
            GUILD_TEXT | DM | GROUP_DM | GUILD_ANNOUNCEMENT | ANNOUNCEMENT_THREAD | PUBLIC_THREAD | PRIVATE_THREAD
        ) || (self.kind == GUILD_VOICE) // voice channels have text chat
    }
    pub fn is_dm(&self) -> bool {
        self.kind == channel_type::DM || self.kind == channel_type::GROUP_DM
    }
    pub fn label(&self) -> String {
        if let Some(n) = self.name.as_deref().filter(|n| !n.is_empty()) {
            return n.to_string();
        }
        if !self.recipients.is_empty() {
            return self.recipients.iter().map(|u| u.display_name()).collect::<Vec<_>>().join(", ");
        }
        format!("channel {}", self.id)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Attachment {
    pub id: Id,
    pub filename: String,
    pub size: u64,
    pub url: String,
    pub proxy_url: String,
    #[serde(deserialize_with = "null_default")]
    pub content_type: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub width: Option<u32>,
    #[serde(deserialize_with = "null_default")]
    pub height: Option<u32>,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        let ct_image = self.content_type.as_deref().is_some_and(|c| c.starts_with("image/"));
        let ext = self.filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        (ct_image || matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")) && self.width.is_some()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EmbedMedia {
    pub url: String,
    #[serde(deserialize_with = "null_default")]
    pub proxy_url: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub width: Option<u32>,
    #[serde(deserialize_with = "null_default")]
    pub height: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NamedText {
    #[serde(alias = "text")]
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Embed {
    #[serde(deserialize_with = "null_default")]
    pub title: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub description: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub url: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub color: Option<u32>,
    #[serde(deserialize_with = "null_default")]
    pub author: Option<NamedText>,
    #[serde(deserialize_with = "null_default")]
    pub footer: Option<NamedText>,
    #[serde(deserialize_with = "null_default")]
    pub image: Option<EmbedMedia>,
    #[serde(deserialize_with = "null_default")]
    pub thumbnail: Option<EmbedMedia>,
    pub fields: Vec<EmbedField>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(default)]
pub struct Emoji {
    #[serde(deserialize_with = "null_default")]
    pub id: Option<Id>,
    #[serde(deserialize_with = "null_default")]
    pub name: Option<String>,
    pub animated: bool,
}

impl Emoji {
    /// Form used in reaction URLs: `name:id` for custom emoji, the literal character otherwise.
    pub fn api_form(&self) -> String {
        match self.id {
            Some(id) => format!("{}:{}", self.name.as_deref().unwrap_or("_"), id),
            None => self.name.clone().unwrap_or_default(),
        }
    }
    pub fn same(&self, other: &Emoji) -> bool {
        match (self.id, other.id) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.name == other.name,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Reaction {
    pub count: u32,
    pub me: bool,
    pub emoji: Emoji,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MessageRef {
    #[serde(deserialize_with = "null_default")]
    pub message_id: Option<Id>,
    #[serde(deserialize_with = "null_default")]
    pub channel_id: Option<Id>,
}

/// Short summary of a replied-to message, kept instead of the full nested message to bound memory.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ReplyPreview {
    pub id: Id,
    pub author: String,
    pub content: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Message {
    pub id: Id,
    pub channel_id: Id,
    #[serde(deserialize_with = "null_default")]
    pub guild_id: Option<Id>,
    pub author: User,
    #[serde(deserialize_with = "null_default")]
    pub member: Option<Member>,
    pub content: String,
    pub timestamp: String,
    #[serde(deserialize_with = "null_default")]
    pub edited_timestamp: Option<String>,
    pub attachments: Vec<Attachment>,
    pub embeds: Vec<Embed>,
    pub reactions: Vec<Reaction>,
    pub mentions: Vec<User>,
    pub mention_roles: Vec<Id>,
    pub mention_everyone: bool,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(deserialize_with = "null_default")]
    pub message_reference: Option<MessageRef>,
    #[serde(skip_serializing, deserialize_with = "null_default")]
    pub referenced_message: Option<Box<Message>>,
    /// Compact form of `referenced_message` that is persisted.
    #[serde(deserialize_with = "null_default")]
    pub reply: Option<ReplyPreview>,
    #[serde(deserialize_with = "null_default")]
    pub nonce: Option<serde_json::Value>,
}

impl Message {
    /// Replaces the nested referenced message with a bounded preview.
    pub fn compact(mut self) -> Message {
        if let Some(r) = self.referenced_message.take() {
            let mut content: String = r.content.chars().take(140).collect();
            if content.is_empty() && !r.attachments.is_empty() {
                content = "[attachment]".into();
            }
            self.reply = Some(ReplyPreview { id: r.id, author: r.author_name().to_string(), content });
        }
        self.nonce = None;
        self
    }

    pub fn author_name(&self) -> &str {
        self.member
            .as_ref()
            .and_then(|m| m.nick.as_deref())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.author.display_name())
    }

    pub fn mentions_user(&self, user: Id) -> bool {
        self.mention_everyone || self.mentions.iter().any(|u| u.id == user)
    }
}

/// Partial message from MESSAGE_UPDATE: any field may be absent.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct MessageUpdate {
    pub id: Id,
    pub channel_id: Id,
    pub content: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub edited_timestamp: Option<String>,
    pub attachments: Option<Vec<Attachment>>,
    pub embeds: Option<Vec<Embed>>,
}

impl MessageUpdate {
    pub fn apply(&self, m: &mut Message) {
        if let Some(c) = &self.content {
            m.content.clone_from(c);
        }
        if self.edited_timestamp.is_some() {
            m.edited_timestamp.clone_from(&self.edited_timestamp);
        }
        if let Some(a) = &self.attachments {
            m.attachments.clone_from(a);
        }
        if let Some(e) = &self.embeds {
            m.embeds.clone_from(e);
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Guild {
    pub id: Id,
    pub name: String,
    #[serde(deserialize_with = "null_default")]
    pub icon: Option<String>,
    pub owner_id: Id,
    pub roles: Vec<Role>,
    pub channels: Vec<Channel>,
    pub threads: Vec<Channel>,
    pub members: Vec<Member>,
    pub member_count: u32,
    pub unavailable: bool,
}

/// What Feather keeps about a guild after GUILD_CREATE: no member list, no role table.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GuildInfo {
    pub id: Id,
    pub name: String,
    pub icon: Option<String>,
    pub member_count: u32,
    /// Visible channels only, sorted for display.
    pub channels: Vec<Channel>,
    /// Role colours for author names (id, colour, position), largest position first.
    pub role_colors: Vec<(Id, u32, i32)>,
    /// Roles the bot holds, so role mentions can be detected.
    pub my_roles: Vec<Id>,
}

impl GuildInfo {
    pub fn color_for(&self, roles: &[Id]) -> Option<u32> {
        self.role_colors.iter().find(|(id, c, _)| *c != 0 && roles.contains(id)).map(|(_, c, _)| *c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_roundtrip_and_timestamp() {
        let id: Id = serde_json::from_str("\"175928847299117063\"").unwrap();
        assert_eq!(id.0, 175928847299117063);
        assert_eq!(id.timestamp_ms(), 1462015105796);
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"175928847299117063\"");
        let n: Id = serde_json::from_str("42").unwrap();
        assert_eq!(n, Id(42));
    }

    #[test]
    fn message_tolerates_nulls_and_compacts_reply() {
        let j = r#"{"id":"2","channel_id":"1","author":{"id":"9","username":"a","global_name":null,"avatar":null},
            "content":"hi","timestamp":"2024-01-01T00:00:00Z","edited_timestamp":null,"member":null,
            "referenced_message":{"id":"1","channel_id":"1","author":{"id":"8","username":"b"},"content":"orig"},
            "unknown_field":{"x":1}}"#;
        let m: Message = serde_json::from_str(j).unwrap();
        let m = m.compact();
        assert!(m.referenced_message.is_none());
        let r = m.reply.unwrap();
        assert_eq!((r.id, r.author.as_str(), r.content.as_str()), (Id(1), "b", "orig"));
    }

    #[test]
    fn permissions_parse_from_string() {
        let r: Role = serde_json::from_str(r#"{"id":"1","permissions":"2048"}"#).unwrap();
        assert_eq!(r.permissions, 2048);
    }
}
