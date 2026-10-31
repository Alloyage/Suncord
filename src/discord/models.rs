use serde::{de::Error, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

/// Deserializes to `None` instead of failing, so one malformed field loses only itself.
pub struct Lenient<T>(pub Option<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Lenient<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(T::deserialize(Value::deserialize(deserializer)?).ok()))
    }
}

pub fn lenient<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Lenient::deserialize(deserializer)?.0.unwrap_or_default())
}

/// Keeps the readable items of a list. `null` is an empty list.
pub fn items<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Vec<T>, D::Error> {
    Ok(Option::<Vec<Lenient<T>>>::deserialize(deserializer)?
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| item.0)
        .collect())
}

/// Discord sends some IDs and permission bits as strings and others as numbers.
pub fn number<T: std::str::FromStr>(value: &Value) -> Option<T> {
    match value {
        Value::String(text) if !text.is_empty() => text.parse().ok(),
        Value::Number(number) => number.to_string().parse().ok(),
        _ => None,
    }
}

pub fn number_field<'de, D: Deserializer<'de>, T: std::str::FromStr>(
    deserializer: D,
) -> Result<T, D::Error> {
    number(&Value::deserialize(deserializer)?).ok_or_else(|| D::Error::custom("expected a number"))
}

#[derive(Clone, Default, Serialize)]
pub struct PresenceDto {
    pub status: String,
    pub custom_status: Option<CustomStatusDto>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct CustomStatusDto {
    #[serde(default, alias = "state")]
    pub text: Option<String>,
    pub emoji: Option<ReactionEmojiDto>,
    pub expires_at: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserDto {
    #[serde(default)]
    pub premium_type: Option<u8>,
    pub id: String,
    pub name: String,
    pub avatar: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub bio: Option<String>,
    #[serde(default)]
    pub banner: Option<String>,
    #[serde(default)]
    pub accent_color: Option<u32>,
    #[serde(default)]
    pub theme_colors: Option<[u32; 2]>,
    #[serde(default)]
    pub mutual_guilds: Option<Vec<String>>,
    #[serde(default)]
    pub mutual_friends_count: Option<u32>,
}

impl UserDto {
    pub fn retain_profile(&mut self, previous: &Self) {
        self.premium_type = self.premium_type.or(previous.premium_type);
        self.bio = self.bio.take().or_else(|| previous.bio.clone());
        self.banner = self.banner.take().or_else(|| previous.banner.clone());
        self.accent_color = self.accent_color.or(previous.accent_color);
        self.theme_colors = self.theme_colors.or(previous.theme_colors);
        self.mutual_friends_count = self.mutual_friends_count.or(previous.mutual_friends_count);
        self.mutual_guilds = self
            .mutual_guilds
            .take()
            .or_else(|| previous.mutual_guilds.clone());
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuildDto {
    #[serde(default)]
    pub emojis: Option<Vec<CustomEmojiDto>>,
    #[serde(default)]
    pub stickers: Option<Vec<StickerDto>>,
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    #[serde(default)]
    pub access: Option<GuildAccess>,
    #[serde(default)]
    pub members: HashMap<String, GuildMemberDto>,
    #[serde(default)]
    pub banner: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct CustomEmojiDto {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub animated: bool,
    #[serde(default = "available_expression")]
    pub available: bool,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct StickerDto {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub tags: String,
    pub format_type: u8,
    #[serde(default = "available_expression")]
    pub available: bool,
}

fn available_expression() -> bool {
    true
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct GuildMemberDto {
    #[serde(default)]
    pub theme_colors: Option<[u32; 2]>,
    pub nick: Option<String>,
    pub avatar: Option<String>,
    pub banner: Option<String>,
    pub bio: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
}

impl GuildMemberDto {
    pub fn update(&mut self, value: &serde_json::Value) {
        for (key, field) in [
            ("nick", &mut self.nick),
            ("avatar", &mut self.avatar),
            ("banner", &mut self.banner),
            ("bio", &mut self.bio),
        ] {
            if let Some(value) = value.get(key) {
                *field = value.as_str().map(str::to_owned);
            }
        }
        if let Some(colors) = value.get("theme_colors") {
            self.theme_colors = Deserialize::deserialize(colors).ok();
        }
        if let Some(roles) = value.get("roles").and_then(|v| v.as_array()) {
            self.roles = roles
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
        }
    }

    pub fn apply(&self, guild: &str, user: &mut UserDto) {
        if let Some(nick) = self.nick.as_ref().filter(|nick| !nick.is_empty()) {
            user.name.clone_from(nick);
        }
        for (hash, field, kind, size) in [
            (&self.avatar, &mut user.avatar, "avatars", 128),
            (&self.banner, &mut user.banner, "banners", 600),
        ] {
            if let Some(hash) = hash.as_ref().filter(|hash| !hash.is_empty()) {
                *field = Some(cdn_image(
                    &format!("guilds/{guild}/users/{}/{kind}", user.id),
                    hash,
                    size,
                ));
            }
        }
        if self.theme_colors.is_some() {
            user.theme_colors = self.theme_colors;
        }
        if self.bio.as_ref().is_some_and(|bio| !bio.is_empty()) {
            user.bio.clone_from(&self.bio);
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct RoleDto {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub color: u32,
    #[serde(default)]
    pub position: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GuildAccess {
    pub owner_id: String,
    pub roles: HashMap<String, u64>,
    #[serde(default)]
    pub role_details: HashMap<String, RoleDto>,
    pub member_roles: Option<Vec<String>>,
    #[serde(default)]
    pub communication_disabled_until: Option<String>,
    #[serde(default)]
    pub timeout_known: bool,
}

impl GuildAccess {
    pub fn retain_missing(&mut self, old: &Self) {
        if self.member_roles.is_none() {
            self.member_roles.clone_from(&old.member_roles);
        }
        if !self.timeout_known {
            self.communication_disabled_until
                .clone_from(&old.communication_disabled_until);
            self.timeout_known = old.timeout_known;
        }
    }
}

/// Also reads Discord's `type` key and string permission bits.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionOverwrite {
    pub id: String,
    #[serde(alias = "type", deserialize_with = "number_field")]
    pub kind: u64,
    #[serde(deserialize_with = "number_field")]
    pub allow: u64,
    #[serde(deserialize_with = "number_field")]
    pub deny: u64,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelDto {
    pub id: String,
    pub guild_id: Option<String>,
    pub name: String,
    pub kind: i32,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub last_message_id: Option<String>,
    /// Peers used to resolve conversation names and avatars.
    #[serde(default)]
    pub recipient_ids: Vec<String>,
    #[serde(default)]
    pub overwrites: Vec<PermissionOverwrite>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub position: i64,
    #[serde(default)]
    pub slowmode_seconds: u64,
    #[serde(default)]
    pub thread_joined: bool,
    #[serde(default)]
    pub thread_archived: bool,
    #[serde(default)]
    pub thread_locked: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDto {
    #[serde(default)]
    pub id: String,
    pub filename: String,
    pub url: String,
    #[serde(default, alias = "proxy_url")]
    pub proxy_url: Option<String>,
    #[serde(default, alias = "content_type")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct EmbedMediaDto {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct EmbedAuthorDto {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub proxy_icon_url: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct EmbedFooterDto {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub proxy_icon_url: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct EmbedFieldDto {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub inline: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct EmbedDto {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub color: Option<u32>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub author: Option<EmbedAuthorDto>,
    #[serde(default)]
    pub provider: Option<EmbedAuthorDto>,
    #[serde(default)]
    pub footer: Option<EmbedFooterDto>,
    #[serde(default)]
    pub fields: Vec<EmbedFieldDto>,
    #[serde(default)]
    pub image: Option<EmbedMediaDto>,
    #[serde(default)]
    pub thumbnail: Option<EmbedMediaDto>,
    #[serde(default)]
    pub video: Option<EmbedMediaDto>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ForwardedMessageDto {
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<AttachmentDto>,
    #[serde(default)]
    pub embeds: Vec<EmbedDto>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MessageReferenceDto {
    pub message_id: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PollDto {
    pub question: PollMediaDto,
    pub answers: Vec<PollAnswerDto>,
    pub expiry: Option<String>,
    #[serde(default)]
    pub allow_multiselect: bool,
    pub results: Option<PollResultsDto>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PollMediaDto {
    #[serde(default)]
    pub text: String,
    pub emoji: Option<ReactionEmojiDto>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PollAnswerDto {
    pub answer_id: u32,
    pub poll_media: PollMediaDto,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PollResultsDto {
    #[serde(default)]
    pub is_finalized: bool,
    #[serde(default)]
    pub answer_counts: Vec<PollCountDto>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PollCountDto {
    pub id: u32,
    pub count: u32,
    #[serde(default)]
    pub me_voted: bool,
}

/// Empty fields are omitted to keep message payloads small.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessageDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll: Option<PollDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stickers: Vec<StickerDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<MessageReferenceDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<GuildMemberDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<ReactionDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<ReplyDto>,
    pub id: String,
    pub channel_id: String,
    pub author_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<UserDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mention_roles: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mention_everyone: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ChannelDto>,
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    pub content: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embeds: Vec<EmbedDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forwarded: Vec<ForwardedMessageDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// A call message, whose `system` text updates when the call ends.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub call: bool,
}

pub const REACTION_AVATAR_COUNT: usize = 4;

#[derive(Clone, Serialize, Deserialize)]
pub struct ReactionDto {
    #[serde(default)]
    pub users: Vec<UserDto>,
    #[serde(default)]
    pub count_details: Option<ReactionCounts>,
    pub emoji: ReactionEmojiDto,
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub me: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ReactionCounts {
    pub normal: u32,
    pub burst: u32,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionEmojiDto {
    pub id: Option<String>,
    pub name: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ReplyDto {
    #[serde(default)]
    pub attachment: bool,
    pub author: String,
    pub content: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuildFolderDto {
    pub id: String,
    pub name: String,
    pub guild_ids: Vec<String>,
    pub color: Option<u32>,
}

/// The cached account state. Messages are persisted separately and never sent to the page.
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    #[serde(default)]
    pub mutual_friends: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub pinned_dms: Vec<String>,
    #[serde(default)]
    pub favorite_emojis: Vec<String>,
    #[serde(default)]
    pub favorite_stickers: Vec<String>,
    #[serde(default)]
    pub frequent_emojis: Vec<String>,
    pub user: Option<UserDto>,
    pub guilds: Vec<GuildDto>,
    #[serde(default)]
    pub folders: Vec<GuildFolderDto>,
    pub channels: Vec<ChannelDto>,
    pub dms: Vec<ChannelDto>,
    #[serde(default, skip_serializing)]
    pub messages: HashMap<String, VecDeque<ChatMessageDto>>,
    #[serde(default)]
    pub users: HashMap<String, UserDto>,
    #[serde(default)]
    pub last_read: HashMap<String, String>,
    #[serde(default)]
    pub mention_counts: HashMap<String, u32>,
    pub active_guild_id: Option<String>,
    pub active_channel_id: Option<String>,
    #[serde(default)]
    pub visited_channels: HashMap<String, String>,
}

pub fn channel_icon_url(channel_id: &str, icon: &Option<String>) -> Option<String> {
    let hash = icon.as_ref()?;
    Some(format!(
        "https://cdn.discordapp.com/channel-icons/{channel_id}/{hash}.webp?size=64"
    ))
}

pub fn avatar_url(
    user_id: &str,
    avatar: &Option<String>,
    discriminator: Option<&str>,
) -> Option<String> {
    let Some(hash) = avatar
        .as_deref()
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
    else {
        let index = match discriminator
            .filter(|value| *value != "0")
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(discriminator) => discriminator % 5,
            None => (user_id.parse::<u64>().ok()? >> 22) % 6,
        };
        return Some(format!(
            "https://cdn.discordapp.com/embed/avatars/{index}.png"
        ));
    };
    Some(cdn_image(&format!("avatars/{user_id}"), hash, 128))
}

pub fn guild_icon_url(guild_id: &str, icon: &Option<String>) -> Option<String> {
    let hash = icon.as_ref()?.trim();
    if hash.is_empty() {
        return None;
    }
    Some(cdn_image(&format!("icons/{guild_id}"), hash, 128))
}

pub fn cdn_image(path: &str, hash: &str, size: u32) -> String {
    let ext = if hash.starts_with("a_") {
        "gif"
    } else {
        "webp"
    };
    format!("https://cdn.discordapp.com/{path}/{hash}.{ext}?size={size}")
}

pub fn display_name(global: &Option<String>, username: &str) -> String {
    global
        .as_ref()
        .filter(|name| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| username.to_string())
}
