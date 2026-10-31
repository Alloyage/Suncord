use super::models::{
    avatar_url, cdn_image, channel_icon_url, display_name, guild_icon_url, items, lenient,
    AttachmentDto, ChannelDto, ChatMessageDto, EmbedDto, ForwardedMessageDto, GuildDto,
    GuildMemberDto, Lenient, MessageReferenceDto, PermissionOverwrite, PollDto, ReactionDto,
    ReactionEmojiDto, ReplyDto, StickerDto, UserDto,
};
use parking_lot::Mutex;
use reqwest::header::AUTHORIZATION;
use serde::{de::IgnoredAny, Deserialize};
use serde_json::{json, Value};
use std::collections::{hash_map::RandomState, HashMap};
use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Mutex as AsyncMutex, Notify};
use zeroize::Zeroizing;

const API: &str = "https://discord.com/api/v10";

#[derive(Clone)]
pub struct DiscordRest {
    http: reqwest::Client,
    requests: Arc<Mutex<RequestState>>,
    dispatch: Arc<AsyncMutex<()>>,
    paused: Arc<Notify>,
    token_keys: RandomState,
}

#[derive(Clone, Deserialize)]
pub struct GifDto {
    pub id: String,
    pub url: String,
    pub src: String,
    #[serde(default)]
    pub gif_src: Option<String>,
    #[serde(default)]
    pub title: String,
}

pub enum MessageMedia {
    Sticker(String),
    /// File names and contents.
    Files(Vec<(String, Vec<u8>)>),
}

pub struct MessageEdit {
    pub content: String,
    pub retained_attachments: Vec<String>,
    pub files: Vec<(String, Vec<u8>)>,
}

type PauseHandler = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct RequestState {
    cooldowns: HashMap<(u64, String), Instant>,
    buckets: HashMap<(u64, reqwest::Method, String), String>,
    paused: HashMap<u64, String>,
    handlers: HashMap<u64, PauseHandler>,
    denied: HashMap<(u64, reqwest::Method, String), String>,
}

impl DiscordRest {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            http: super::http_client().map_err(|err| err.to_string())?,
            requests: Arc::default(),
            dispatch: Arc::default(),
            paused: Arc::default(),
            token_keys: RandomState::new(),
        })
    }

    pub fn register_pause_handler(&self, token: &str, handler: Arc<dyn Fn(&str) + Send + Sync>) {
        let key = self.token_keys.hash_one(token);
        let reason = {
            let mut state = self.requests.lock();
            state.handlers.insert(key, handler.clone());
            state.paused.get(&key).cloned()
        };
        if let Some(reason) = reason {
            handler(&reason);
        }
    }

    fn record_pause(&self, key: u64, reason: String) {
        let handler = {
            let mut state = self.requests.lock();
            state.paused.insert(key, reason.clone());
            state.handlers.get(&key).cloned()
        };
        if let Some(handler) = handler {
            handler(&reason);
        }
        self.paused.notify_waiters();
    }

    pub fn resume(&self, token: &str) {
        let key = self.token_keys.hash_one(token);
        let mut state = self.requests.lock();
        state.paused.remove(&key);
        state.denied.retain(|(account, _, _), _| *account != key);
    }

    pub fn pause(&self, token: &str, reason: String) {
        self.record_pause(self.token_keys.hash_one(token), reason);
    }

    pub async fn wait_until_paused(&self, token: &str) -> String {
        loop {
            let notified = self.paused.notified();
            if let Some(reason) = self.paused_reason(token) {
                return reason;
            }
            notified.await;
        }
    }

    pub fn paused_reason(&self, token: &str) -> Option<String> {
        self.requests
            .lock()
            .paused
            .get(&self.token_keys.hash_one(token))
            .cloned()
    }

    pub async fn validate_session(&self, token: &Zeroizing<String>) -> Result<UserDto, String> {
        let value = self.get(token, "/users/@me").await?;
        parse_user(&value).ok_or_else(|| "Discord returned an unreadable account.".into())
    }

    pub async fn profile(
        &self,
        token: &Zeroizing<String>,
        id: &str,
        guild: Option<&str>,
    ) -> Result<(UserDto, Option<GuildMemberDto>), String> {
        #[derive(Default, Deserialize)]
        struct Extras {
            #[serde(default, deserialize_with = "lenient")]
            bio: Option<String>,
            #[serde(default, deserialize_with = "lenient")]
            banner: Option<String>,
            #[serde(default, deserialize_with = "lenient")]
            accent_color: Option<u32>,
            #[serde(default, deserialize_with = "lenient")]
            theme_colors: Option<[u32; 2]>,
        }
        #[derive(Deserialize)]
        struct Profile {
            user: WireUser,
            #[serde(default, deserialize_with = "lenient")]
            user_profile: Option<Extras>,
            #[serde(default, deserialize_with = "lenient")]
            guild_member: Option<GuildMemberDto>,
            #[serde(default, deserialize_with = "lenient")]
            guild_member_profile: Option<Extras>,
            #[serde(default, deserialize_with = "lenient")]
            mutual_guilds: Option<Vec<Lenient<WithId>>>,
            #[serde(default, deserialize_with = "lenient")]
            mutual_friends_count: Option<u32>,
        }
        if !super::snowflake(id) {
            return Err("Invalid user ID.".into());
        }
        let mut path =
            format!("/users/{id}/profile?with_mutual_guilds=true&with_mutual_friends_count=true");
        if let Some(guild) = guild.filter(|id| super::snowflake(id)) {
            path.push_str(&format!("&guild_id={guild}"));
        }
        let profile = Profile::deserialize(&self.get(token, &path).await?)
            .map_err(|_| "Discord returned an unreadable profile.")?;
        let mut wire = profile.user;
        let extras = profile.user_profile.unwrap_or_default();
        wire.bio = extras.bio.or(wire.bio);
        wire.banner = extras.banner.or(wire.banner);
        wire.accent_color = extras.accent_color.or(wire.accent_color);
        wire.theme_colors = extras.theme_colors.or(wire.theme_colors);
        let mut user = UserDto::from(wire);
        user.mutual_guilds = profile.mutual_guilds.map(|guilds| {
            guilds
                .into_iter()
                .filter_map(|guild| Some(guild.0?.id))
                .collect()
        });
        user.mutual_friends_count = profile.mutual_friends_count;
        let member = profile.guild_member.map(|mut member| {
            if let Some(extras) = profile.guild_member_profile {
                member.bio = extras.bio.or(member.bio);
                member.banner = extras.banner.or(member.banner);
                member.theme_colors = extras.theme_colors.or(member.theme_colors);
            }
            member
        });
        Ok((user, member))
    }

    /// Rings every other recipient, as the official client does when starting a call.
    pub async fn ring(&self, token: &Zeroizing<String>, channel: &str) -> Result<(), String> {
        self.post(
            token,
            &format!("/channels/{channel}/call/ring"),
            json!({"recipients": null}),
        )
        .await?;
        Ok(())
    }

    pub async fn invite(&self, token: &Zeroizing<String>, channel: &str) -> Result<String, String> {
        let value = self
            .post(
                token,
                &format!("/channels/{channel}/invites"),
                json!({"max_age":86400,"max_uses":0,"temporary":false,"unique":false}),
            )
            .await?;
        value["code"]
            .as_str()
            .filter(|code| {
                !code.is_empty()
                    && code
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
            .map(|code| format!("https://discord.gg/{code}"))
            .ok_or("Discord returned an unreadable invite".into())
    }

    pub async fn mutual_friends(
        &self,
        token: &Zeroizing<String>,
        user: &str,
    ) -> Result<Vec<UserDto>, String> {
        let value = self
            .get(token, &format!("/users/{user}/relationships"))
            .await?;
        Ok(value
            .as_array()
            .ok_or("Unreadable mutual friends")?
            .iter()
            .filter_map(|value| parse_user(value.get("user").unwrap_or(value)))
            .collect())
    }

    pub async fn acknowledge_messages(
        &self,
        token: &Zeroizing<String>,
        messages: &[(String, String)],
    ) -> Result<(), String> {
        let read_states: Vec<_> = messages.iter().map(|(channel, message)| {
            json!({"channel_id": channel, "message_id": message, "read_state_type": 0})
        }).collect();
        self.post(
            token,
            "/read-states/ack-bulk",
            json!({"read_states": read_states}),
        )
        .await?;
        Ok(())
    }

    pub async fn dm_channels(&self, token: &Zeroizing<String>) -> Result<Vec<ChannelDto>, String> {
        let value = self.get(token, "/users/@me/channels").await?;
        let rows: Vec<Lenient<WireChannel>> = Deserialize::deserialize(&value)
            .map_err(|_| "Discord returned an unreadable conversation list.")?;
        Ok(rows
            .into_iter()
            .filter_map(|row| Some(row.0?.into_dto(None).0))
            .collect())
    }

    /// Legacy JSON user settings — still the reliable source for `guild_folders`.
    pub async fn user_settings(&self, token: &Zeroizing<String>) -> Result<Value, String> {
        self.get(token, "/users/@me/settings").await
    }

    /// Favorite and frequently used emoji from Discord's frecency settings.
    pub async fn emoji_settings(
        &self,
        token: &Zeroizing<String>,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        let bytes = self.frecency_settings(token).await?;
        Ok((
            favorite_list(&bytes)?,
            rank_frecency(&emoji_frecency(&bytes)?),
        ))
    }

    pub async fn edit_favorite_emoji(
        &self,
        token: &Zeroizing<String>,
        key: &str,
        index: Option<usize>,
    ) -> Result<Vec<String>, String> {
        let bytes = self.frecency_settings(token).await?;
        let mut favorites = favorite_list(&bytes)?;
        if index.is_none() && favorites.len() >= 250 && !favorites.iter().any(|name| name == key) {
            return Err("Discord allows up to 250 favorite emoji.".into());
        }
        edit_favorites(&mut favorites, key, index);
        let mut encoded = Vec::new();
        for name in &favorites {
            proto_bytes(&mut encoded, 1, name.as_bytes());
        }
        self.patch_frecency(token, &bytes, 5, &encoded).await?;
        Ok(favorites)
    }

    /// Records an emoji use the way Discord's client does and returns the new frequent order.
    pub async fn record_emoji_use(
        &self,
        token: &Zeroizing<String>,
        key: &str,
    ) -> Result<Vec<String>, String> {
        let bytes = self.frecency_settings(token).await?;
        let mut entries = emoji_frecency(&bytes)?;
        let at = entries
            .iter()
            .position(|(name, _)| name == key)
            .unwrap_or_else(|| {
                entries.push((key.to_owned(), Vec::new()));
                entries.len() - 1
            });
        let mut item = parse_frecency(&entries[at].1).ok_or("Unreadable emoji frecency")?;
        item.total += 1;
        item.recent.push(now_millis());
        let excess = item.recent.len().saturating_sub(10);
        item.recent.drain(..excess);
        entries[at].1 = encode_frecency(&item);
        let mut encoded = Vec::new();
        for (name, value) in &entries {
            let mut entry = Vec::new();
            proto_bytes(&mut entry, 1, name.as_bytes());
            proto_bytes(&mut entry, 2, value);
            proto_bytes(&mut encoded, 1, &entry);
        }
        self.patch_frecency(token, &bytes, 6, &encoded).await?;
        Ok(rank_frecency(&entries))
    }

    async fn frecency_settings(&self, token: &Zeroizing<String>) -> Result<Vec<u8>, String> {
        use base64::Engine;
        let value = self.get(token, "/users/@me/settings-proto/2").await?;
        base64::engine::general_purpose::STANDARD
            .decode(value["settings"].as_str().ok_or("Missing emoji settings")?)
            .map_err(|_| "Unreadable emoji settings".into())
    }

    /// Replaces one top-level field of the frecency settings.
    async fn patch_frecency(
        &self,
        token: &Zeroizing<String>,
        current: &[u8],
        field: u64,
        value: &[u8],
    ) -> Result<(), String> {
        use base64::Engine;
        let mut settings = Vec::new();
        proto_bytes(&mut settings, field, value);
        let mut body =
            json!({"settings": base64::engine::general_purpose::STANDARD.encode(settings)});
        if let Some(mut versions) =
            proto_strings(current, 1).and_then(|groups| groups.first().copied())
        {
            while !versions.is_empty() {
                let tag = proto_varint(&mut versions).ok_or("Unreadable settings version")?;
                if tag & 7 != 0 {
                    return Err("Unreadable settings version".into());
                }
                let version = proto_varint(&mut versions).ok_or("Unreadable settings version")?;
                if tag >> 3 == 3 {
                    body["required_data_version"] = version.into();
                }
            }
        }
        let result = self
            .send(
                token,
                reqwest::Method::PATCH,
                "/users/@me/settings-proto/2",
                Some(body),
            )
            .await?;
        if result["out_of_date"].as_bool() == Some(true) {
            return Err("Emoji settings changed in another client. Try again.".into());
        }
        Ok(())
    }

    pub async fn gifs(
        &self,
        token: &Zeroizing<String>,
        query: &str,
    ) -> Result<Vec<GifDto>, String> {
        let endpoint = if query.is_empty() {
            "/gifs/trending-gifs"
        } else {
            "/gifs/search"
        };
        let mut url = reqwest::Url::parse(&format!("https://discord.com{endpoint}")).unwrap();
        url.query_pairs_mut()
            .append_pair("media_format", "gif")
            .append_pair("locale", "en-US")
            .append_pair("limit", "24");
        if !query.is_empty() {
            url.query_pairs_mut().append_pair("q", query);
        }
        let value = self
            .get(
                token,
                &format!("{}?{}", url.path(), url.query().unwrap_or_default()),
            )
            .await?;
        serde_json::from_value(value).map_err(|_| "Discord returned unreadable GIF results".into())
    }

    /// Partial guild list with name + icon — fills READY unavailable stubs.
    pub async fn user_guilds(&self, token: &Zeroizing<String>) -> Result<Vec<GuildDto>, String> {
        #[derive(Deserialize)]
        struct PartialGuild {
            id: String,
            #[serde(default, deserialize_with = "lenient")]
            name: String,
            #[serde(default, deserialize_with = "lenient")]
            icon: Option<String>,
            #[serde(default, deserialize_with = "lenient")]
            banner: Option<String>,
        }
        let value = self
            .get(token, "/users/@me/guilds?with_counts=false")
            .await?;
        let rows: Vec<Lenient<PartialGuild>> = Deserialize::deserialize(&value)
            .map_err(|_| "Discord returned an unreadable guild list.")?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.0)
            .map(|row| GuildDto {
                banner: row
                    .banner
                    .map(|hash| cdn_image(&format!("banners/{}", row.id), &hash, 600)),
                icon: guild_icon_url(&row.id, &row.icon),
                id: row.id,
                name: row.name,
                ..Default::default()
            })
            .collect())
    }

    pub async fn recent_mentions(
        &self,
        token: &Zeroizing<String>,
        before: Option<&str>,
    ) -> Result<(Vec<ChatMessageDto>, Option<String>), String> {
        if before.is_some_and(|id| !super::snowflake(id)) {
            return Err("Invalid message cursor.".into());
        }
        let mut path = "/users/@me/mentions?limit=25&roles=true&everyone=true".to_owned();
        if let Some(before) = before {
            path.push_str(&format!("&before={before}"));
        }
        let rows = parse_messages(&self.get(token, &path).await?)?;
        let cursor = (rows.len() == 25)
            .then(|| rows.last().map(|row| row.id.clone()))
            .flatten();
        Ok((rows, cursor))
    }

    pub async fn pinned_messages(
        &self,
        token: &Zeroizing<String>,
        channel: &str,
        before: Option<&str>,
    ) -> Result<(Vec<ChatMessageDto>, Option<String>), String> {
        #[derive(Deserialize)]
        struct Pin {
            message: WireMessage,
            #[serde(default)]
            pinned_at: Option<String>,
        }
        #[derive(Deserialize)]
        struct Pins {
            items: Vec<Pin>,
            #[serde(default)]
            has_more: bool,
        }
        if !super::snowflake(channel) {
            return Err("Invalid channel.".into());
        }
        let mut url = reqwest::Url::parse(&format!(
            "https://discord.com/channels/{channel}/messages/pins"
        ))
        .unwrap();
        url.query_pairs_mut().append_pair("limit", "50");
        if let Some(before) = before {
            url.query_pairs_mut().append_pair("before", before);
        }
        let value = self
            .get(token, &format!("{}?{}", url.path(), url.query().unwrap()))
            .await?;
        let mut pins = Pins::deserialize(&value).map_err(|_| "Unreadable pinned messages")?;
        let cursor = pins
            .has_more
            .then(|| pins.items.last_mut().and_then(|pin| pin.pinned_at.take()))
            .flatten();
        Ok((
            pins.items
                .into_iter()
                .map(|pin| pin.message.into())
                .collect(),
            cursor,
        ))
    }

    /// Fetches 50 messages before or after a message, or the latest ones.
    pub async fn messages(
        &self,
        token: &Zeroizing<String>,
        channel_id: &str,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<Vec<ChatMessageDto>, String> {
        if std::iter::once(channel_id)
            .chain(before)
            .chain(after)
            .any(|id| !super::snowflake(id))
        {
            return Err("Invalid message location.".into());
        }
        let mut path = format!("/channels/{channel_id}/messages?limit=50");
        if let Some(before) = before {
            path.push_str(&format!("&before={before}"));
        }
        if let Some(after) = after {
            path.push_str(&format!("&after={after}"));
        }
        parse_messages(&self.get(token, &path).await?)
    }

    pub async fn message_context(
        &self,
        token: &Zeroizing<String>,
        channel: &str,
        message: &str,
    ) -> Result<Vec<ChatMessageDto>, String> {
        if [channel, message].iter().any(|id| !super::snowflake(id)) {
            return Err("Invalid message location.".into());
        }
        let value = self
            .get(
                token,
                &format!("/channels/{channel}/messages?around={message}&limit=50"),
            )
            .await?;
        parse_messages(&value)
    }

    pub async fn role_member_ids(
        &self,
        token: &Zeroizing<String>,
        guild: &str,
        role: &str,
    ) -> Result<Vec<String>, String> {
        if [guild, role].iter().any(|id| !super::snowflake(id)) {
            return Err("Invalid role.".into());
        }
        let value = self
            .get(token, &format!("/guilds/{guild}/roles/{role}/member-ids"))
            .await?;
        serde_json::from_value(value)
            .map_err(|_| "Discord returned an unreadable member list.".into())
    }

    pub async fn send_message(
        &self,
        token: &Zeroizing<String>,
        channel_id: &str,
        content: &str,
        nonce: &str,
        reply: Option<&str>,
        media: Option<MessageMedia>,
    ) -> Result<ChatMessageDto, String> {
        let mut body = json!({ "content": content, "nonce": nonce, "enforce_nonce": true });
        match media {
            Some(MessageMedia::Sticker(id)) => body["sticker_ids"] = json!([id]),
            Some(MessageMedia::Files(files)) => {
                let mut uploaded = Vec::new();
                for (index, (name, bytes)) in files.into_iter().enumerate() {
                    uploaded.push(
                        self.upload_file(token, channel_id, name, bytes, index)
                            .await?,
                    );
                }
                body["attachments"] = json!(uploaded);
            }
            None => {}
        }
        if let Some(reply) = reply {
            body["message_reference"] = json!({ "message_id": reply, "channel_id": channel_id, "fail_if_not_exists": true });
            body["allowed_mentions"] =
                json!({ "parse": ["users", "roles", "everyone"], "replied_user": false });
        }
        let value = self
            .post(token, &format!("/channels/{channel_id}/messages"), body)
            .await?;
        message_from_value(&value).ok_or_else(|| "Discord returned an unreadable message.".into())
    }

    pub async fn delete_message(
        &self,
        token: &Zeroizing<String>,
        channel: &str,
        message: &str,
    ) -> Result<(), String> {
        self.send(
            token,
            reqwest::Method::DELETE,
            &format!("/channels/{channel}/messages/{message}"),
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn edit_message(
        &self,
        token: &Zeroizing<String>,
        channel: &str,
        message: &str,
        edit: MessageEdit,
    ) -> Result<ChatMessageDto, String> {
        let mut attachments: Vec<_> = edit
            .retained_attachments
            .iter()
            .map(|id| json!({"id": id}))
            .collect();
        for (index, (filename, bytes)) in edit.files.into_iter().enumerate() {
            attachments.push(
                self.upload_file(token, channel, filename, bytes, index)
                    .await?,
            );
        }
        let value = self.send(token, reqwest::Method::PATCH,
            &format!("/channels/{channel}/messages/{message}"),
            Some(json!({ "content": edit.content, "attachments": attachments, "allowed_mentions": { "parse": [] } })),
        ).await?;
        message_from_value(&value).ok_or_else(|| "Discord returned an unreadable message.".into())
    }

    async fn upload_file(
        &self,
        token: &Zeroizing<String>,
        channel: &str,
        filename: String,
        bytes: Vec<u8>,
        index: usize,
    ) -> Result<Value, String> {
        let slot = self.post(token, &format!("/channels/{channel}/attachments"), json!({
                "files": [{"id": index.to_string(), "filename": filename, "file_size": bytes.len()}]
            })).await?;
        let slot = slot
            .pointer("/attachments/0")
            .ok_or("Discord returned no upload slot")?;
        let url = slot["upload_url"]
            .as_str()
            .ok_or("Discord returned no upload URL")?;
        let uploaded = slot["upload_filename"]
            .as_str()
            .ok_or("Discord returned no upload filename")?;
        let url = reqwest::Url::parse(url).map_err(|_| "Discord returned an invalid upload URL")?;
        if url.scheme() != "https" {
            return Err("Discord returned an insecure upload URL".into());
        }
        // Signed upload URLs never receive the account token.
        self.http
            .put(url)
            .header("Content-Type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .map_err(|_| "Could not upload the attachment. The message was not sent or edited.")?
            .error_for_status()
            .map_err(|_| {
                "Discord rejected the attachment upload. The message was not sent or edited."
            })?;
        Ok(json!({"id": index.to_string(), "filename": filename, "uploaded_filename": uploaded}))
    }

    pub async fn set_reaction(
        &self,
        token: &Zeroizing<String>,
        channel: &str,
        message: &str,
        emoji: &ReactionEmojiDto,
        remove: bool,
    ) -> Result<(), String> {
        if [channel, message].iter().any(|id| !super::snowflake(id)) {
            return Err("Invalid reaction location.".into());
        }
        let name = emoji
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
            .ok_or("Missing emoji name.")?;
        let emoji = emoji_segment(name, emoji.id.as_deref());
        self.send(
            token,
            if remove {
                reqwest::Method::DELETE
            } else {
                reqwest::Method::PUT
            },
            &format!("/channels/{channel}/messages/{message}/reactions/{emoji}/@me"),
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn reaction_users(
        &self,
        token: &Zeroizing<String>,
        channel: &str,
        message: &str,
        reaction: &ReactionDto,
    ) -> Result<Vec<UserDto>, String> {
        if [channel, message].iter().any(|id| !super::snowflake(id)) {
            return Err("Invalid reaction location.".into());
        }
        let emoji = emoji_segment(
            reaction.emoji.name.as_deref().unwrap_or("_"),
            reaction.emoji.id.as_deref(),
        );
        let mut users: Vec<UserDto> = Vec::new();
        for kind in 0..=1 {
            let count = reaction
                .count_details
                .as_ref()
                .map(|counts| {
                    if kind == 0 {
                        counts.normal
                    } else {
                        counts.burst
                    }
                })
                .unwrap_or(if kind == 0 { reaction.count } else { 0 });
            if count == 0 || users.len() >= super::models::REACTION_AVATAR_COUNT {
                continue;
            }
            let limit = super::models::REACTION_AVATAR_COUNT;
            let value = self.get(token, &format!("/channels/{channel}/messages/{message}/reactions/{emoji}?limit={limit}&type={kind}")).await?;
            for user in value
                .as_array()
                .ok_or("Discord returned unreadable reaction users.")?
                .iter()
                .filter_map(parse_user)
            {
                if !users.iter().any(|old| old.id == user.id)
                    && users.len() < super::models::REACTION_AVATAR_COUNT
                {
                    users.push(user);
                }
            }
        }
        Ok(users)
    }

    async fn get(&self, token: &Zeroizing<String>, path: &str) -> Result<Value, String> {
        self.send(token, reqwest::Method::GET, path, None).await
    }

    async fn post(
        &self,
        token: &Zeroizing<String>,
        path: &str,
        body: Value,
    ) -> Result<Value, String> {
        self.send(token, reqwest::Method::POST, path, Some(body))
            .await
    }

    async fn send(
        &self,
        token: &Zeroizing<String>,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        // Queueing is cancellable. After dispatch, the worker finishes response
        // classification even if the caller navigates away or signs out.
        let dispatch = loop {
            self.wait_for_cooldown(token, &method, path).await?;
            let dispatch = self.dispatch.clone().lock_owned().await;
            if self.cooldown_wait(token, &method, path)?.is_none() {
                break dispatch;
            }
        };
        let rest = self.clone();
        let token = token.clone();
        let path = path.to_string();
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let _dispatch = dispatch;
            if !sender.is_closed() {
                let result = rest.dispatch_request(&token, method, &path, body).await;
                let _ = sender.send(result);
            }
        });
        receiver
            .await
            .map_err(|_| "Discord request handling stopped unexpectedly.".to_owned())?
    }

    async fn dispatch_request(
        &self,
        token: &Zeroizing<String>,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let key = self.token_keys.hash_one(token.as_str());
        let route = (
            key,
            method.clone(),
            path.split('?').next().unwrap_or(path).to_string(),
        );
        {
            let state = self.requests.lock();
            if let Some(reason) = state.paused.get(&key).or_else(|| state.denied.get(&route)) {
                return Err(reason.clone());
            }
        }
        let writing = method != reqwest::Method::GET;
        let message_write = (method == reqwest::Method::POST && path.ends_with("/messages"))
            || (method == reqwest::Method::PATCH && path.contains("/messages/"));
        let mut request = self
            .http
            .request(method.clone(), format!("{API}{path}"))
            .header(AUTHORIZATION, token.as_str());
        if let Some(body) = &body {
            request = request.json(body);
        }
        eprintln!(
            "discord request {} {} at {}",
            method,
            endpoint_label(path),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );
        let response = match request.send().await {
            Ok(response) => response,
            Err(_) => {
                let reason = if message_write {
                        "Could not confirm whether Discord received your message. Requests stopped. Check official Discord before sending again."
                    } else {
                        "Could not reach Discord."
                    }.to_string();
                if message_write {
                    self.record_pause(key, reason.clone());
                }
                return Err(reason);
            }
        };
        let status = response.status();
        let headers = response.headers().clone();
        let body_type = match headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or_default().trim())
        {
            Some("application/json") => "JSON",
            Some("text/html") => "HTML",
            None => "unspecified content type",
            _ => "other content type",
        };
        let bytes = response.bytes().await;
        let parsed = bytes
            .as_ref()
            .ok()
            .map(|bytes| serde_json::from_slice::<Value>(bytes));
        let value = parsed.as_ref().and_then(|parsed| parsed.as_ref().ok());
        let code = value.and_then(|v| v.get("code")).and_then(Value::as_u64);
        // Only fixed endpoint labels and status codes, never tokens, IDs,
        // message content, response bodies, or challenge credentials.
        eprintln!(
            "discord {} {}: HTTP {}, code {:?}",
            method,
            endpoint_label(path),
            status.as_u16(),
            code
        );
        let exhausted = header_number(&headers, "x-ratelimit-remaining") == Some(0.0);
        let bucket = headers
            .get("x-ratelimit-bucket")
            .and_then(|v| v.to_str().ok())
            .map(|bucket| {
                let mut parts = path.trim_start_matches('/').split('/');
                let scope = parts.next().unwrap_or_default();
                let major = if matches!(scope, "channels" | "guilds" | "webhooks") {
                    parts.next().unwrap_or_default()
                } else {
                    ""
                };
                format!("{bucket}:{scope}:{major}")
            })
            .unwrap_or_else(|| route.2.clone());
        self.requests.lock().buckets.insert(
            (key, method.clone(), rate_limit_route(path)),
            bucket.clone(),
        );
        if status.as_u16() == 429 || exhausted {
            let wait = header_number(&headers, "retry-after")
                .into_iter()
                .chain(header_number(&headers, "x-ratelimit-reset-after"))
                .chain(
                    value
                        .and_then(|v| v.get("retry_after"))
                        .and_then(Value::as_f64),
                )
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                .reduce(f64::max)
                .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
                .and_then(|wait| wait.checked_add(Duration::from_millis(250)))
                .and_then(|wait| Instant::now().checked_add(wait));
            match wait {
                Some(until) => {
                    let mut state = self.requests.lock();
                    let global = headers
                        .get("x-ratelimit-global")
                        .is_some_and(|v| v == "true")
                        || headers
                            .get("x-ratelimit-scope")
                            .is_some_and(|v| v == "global")
                        || value.and_then(|v| v["global"].as_bool()) == Some(true);
                    let slot = state
                        .cooldowns
                        .entry((key, if global { "*".into() } else { bucket }))
                        .or_insert(until);
                    *slot = (*slot).max(until);
                }
                None => {
                    self.record_pause(key, "Discord omitted a usable rate-limit reset. Requests stopped. Check official Discord.".into());
                }
            }
        }
        if !status.is_success() {
            let restricted = value.is_some_and(|v| {
                    v.get("captcha_key").is_some() || v.get("captcha_sitekey").is_some()
                }) || matches!(code, Some(40001 | 40002 | 40003 | 40004 | 40012 | 50014))
                    // Unrecognized write failures may represent a new account
                    // restriction. Only known resource/permission failures are
                    // confined to the route instead of pausing the account.
                    || (writing && status.as_u16() != 429 && !matches!(code, Some(10003 | 10008 | 50001 | 50007 | 50013)));
            let reason = if restricted {
                format!("Discord rejected this action or requires account verification (HTTP {}, code {:?}). Requests stopped. Open official Discord.", status.as_u16(), code)
            } else if status.as_u16() == 429 {
                "Discord rate limited this request. No automatic retry was made. Wait before trying again.".into()
            } else {
                format!("Discord refused the request (HTTP {}, code {:?}). No automatic retry was made.", status.as_u16(), code)
            };
            if restricted || status.as_u16() == 401 || (status.as_u16() == 403 && value.is_none()) {
                self.record_pause(key, reason.clone());
            }
            if matches!(status.as_u16(), 400 | 403 | 404) {
                self.requests.lock().denied.insert(route, reason.clone());
            }
            if status.as_u16() == 401 {
                return Err(
                    "Discord rejected this session. Check official Discord before resuming.".into(),
                );
            }
            if writing && status.is_server_error() {
                self.record_pause(key, "Discord could not confirm message delivery. Requests stopped. Check official Discord before sending again.".into());
            }
            return Err(reason);
        }
        if status.as_u16() == 204 && !message_write {
            return Ok(Value::Null);
        }
        if !writing
            && (status.as_u16() == 204
                || headers
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    == Some("0"))
        {
            return Ok(Value::Null);
        }
        if !writing && bytes.as_ref().is_ok_and(|bytes| bytes.is_empty()) {
            return Ok(Value::Null);
        }
        match parsed {
            // Other operations validate their own response data. A read-state
            // acknowledgement or upload slot is not a message object.
            Some(Ok(value)) if !message_write || WireMessage::deserialize(&value).is_ok() => {
                Ok(value)
            }
            _ => {
                let detail = match (&bytes, &parsed) {
                    (Err(_), _) => "the response body could not be read".to_string(),
                    (Ok(bytes), _) if bytes.is_empty() => "the response body was empty".into(),
                    (_, Some(Err(error))) => format!(
                        "the response was invalid JSON ({:?}, line {}, column {})",
                        error.classify(),
                        error.line(),
                        error.column()
                    ),
                    _ => "the response did not match the message format".into(),
                };
                let reason = format!(
                    "Discord {} response (HTTP {}, {body_type}, {} bytes): {detail}.",
                    endpoint_label(path),
                    status.as_u16(),
                    bytes.as_ref().map_or(0, |bytes| bytes.len())
                );
                eprintln!("discord {method}: {reason}");
                let reason = if message_write {
                    format!("{reason} Message delivery is uncertain. Requests stopped. Check official Discord before sending again.")
                } else {
                    reason
                };
                if message_write {
                    self.record_pause(key, reason.clone());
                }
                Err(reason)
            }
        }
    }

    async fn wait_for_cooldown(
        &self,
        token: &str,
        method: &reqwest::Method,
        path: &str,
    ) -> Result<(), String> {
        loop {
            let wait = self.cooldown_wait(token, method, path)?;
            let Some(wait) = wait else { return Ok(()) };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {},
                reason = self.wait_until_paused(token) => return Err(reason),
            }
        }
    }

    fn cooldown_wait(
        &self,
        token: &str,
        method: &reqwest::Method,
        path: &str,
    ) -> Result<Option<Duration>, String> {
        let key = self.token_keys.hash_one(token);
        let route = (
            key,
            method.clone(),
            path.split('?').next().unwrap_or(path).to_owned(),
        );
        let mut state = self.requests.lock();
        if let Some(reason) = state.paused.get(&key).or_else(|| state.denied.get(&route)) {
            return Err(reason.clone());
        }
        let now = Instant::now();
        state.cooldowns.retain(|_, until| *until > now);
        let bucket = state
            .buckets
            .get(&(key, method.clone(), rate_limit_route(path)))
            .unwrap_or(&route.2);
        Ok(state
            .cooldowns
            .get(&(key, bucket.clone()))
            .into_iter()
            .chain(state.cooldowns.get(&(key, "*".into())))
            .max()
            .and_then(|until| until.checked_duration_since(now)))
    }
}

fn rate_limit_route(path: &str) -> String {
    let mut previous = "";
    path.split('?')
        .next()
        .unwrap_or(path)
        .split('/')
        .map(|part| {
            let parameter = previous == "reactions"
                || (matches!(previous, "users" | "messages" | "roles") && super::snowflake(part));
            previous = part;
            if parameter {
                ":id"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn header_number(headers: &reqwest::header::HeaderMap, name: &str) -> Option<f64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

fn endpoint_label(path: &str) -> &'static str {
    match path.split('?').next().unwrap_or(path) {
        "/users/@me" => "account",
        "/users/@me/channels" => "dm-list",
        "/users/@me/guilds" => "guild-list",
        "/users/@me/settings" => "settings",
        "/users/@me/settings-proto/2" => "emoji-favorites",
        "/gifs/search" | "/gifs/trending-gifs" => "gif-search",
        "/read-states/ack-bulk" => "read-state",
        path if path.ends_with("/attachments") => "attachment-upload",
        path if path.starts_with("/users/") => "user-profile",
        path if path.contains("/reactions/") => "reactions",
        _ => "messages",
    }
}

/// A reaction emoji as one encoded path segment.
fn emoji_segment(name: &str, id: Option<&str>) -> String {
    let emoji = id.map_or_else(|| name.to_owned(), |id| format!("{name}:{id}"));
    let mut url = reqwest::Url::parse("https://discord.com").unwrap();
    url.path_segments_mut().unwrap().push(&emoji);
    url.path()[1..].to_owned()
}

#[derive(Deserialize)]
struct WithId {
    id: String,
}

#[derive(Deserialize)]
struct WireUser {
    id: String,
    username: String,
    #[serde(default)]
    global_name: Option<String>,
    #[serde(default)]
    avatar: Option<String>,
    #[serde(default)]
    discriminator: Option<String>,
    #[serde(default)]
    bio: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    banner: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    accent_color: Option<u32>,
    #[serde(default, deserialize_with = "lenient")]
    theme_colors: Option<[u32; 2]>,
    #[serde(default, deserialize_with = "lenient")]
    premium_type: Option<u8>,
}

impl From<WireUser> for UserDto {
    fn from(wire: WireUser) -> Self {
        Self {
            avatar: avatar_url(&wire.id, &wire.avatar, wire.discriminator.as_deref()),
            name: display_name(&wire.global_name, &wire.username),
            username: Some(wire.username),
            bio: wire.bio,
            banner: wire
                .banner
                .map(|hash| cdn_image(&format!("banners/{}", wire.id), &hash, 600)),
            accent_color: wire.accent_color,
            theme_colors: wire.theme_colors,
            premium_type: wire.premium_type,
            id: wire.id,
            ..Default::default()
        }
    }
}

pub fn parse_user(value: &Value) -> Option<UserDto> {
    WireUser::deserialize(value).ok().map(Into::into)
}

#[derive(Deserialize)]
struct WireReply {
    #[serde(default, deserialize_with = "lenient")]
    author: Option<WireUser>,
    #[serde(default, deserialize_with = "lenient")]
    content: String,
    #[serde(default, deserialize_with = "lenient")]
    attachments: Vec<IgnoredAny>,
}

#[derive(Deserialize)]
struct WireSnapshot {
    message: ForwardedMessageDto,
}

/// Fields other than the identity, author and text are optional, so a malformed
/// poll or embed loses only that field instead of the whole message.
#[derive(Deserialize)]
struct WireMessage {
    id: String,
    channel_id: String,
    #[serde(default, deserialize_with = "lenient")]
    guild_id: Option<String>,
    #[serde(default)]
    content: String,
    #[serde(default)]
    timestamp: String,
    author: WireUser,
    #[serde(default, deserialize_with = "items")]
    attachments: Vec<AttachmentDto>,
    #[serde(default, deserialize_with = "items")]
    embeds: Vec<EmbedDto>,
    #[serde(default, deserialize_with = "items")]
    message_snapshots: Vec<WireSnapshot>,
    #[serde(default, deserialize_with = "lenient")]
    poll: Option<PollDto>,
    #[serde(default, deserialize_with = "lenient")]
    sticker_items: Vec<StickerDto>,
    #[serde(default, deserialize_with = "lenient")]
    message_reference: Option<MessageReferenceDto>,
    #[serde(default, deserialize_with = "lenient")]
    edited_timestamp: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    member: Option<GuildMemberDto>,
    #[serde(default, deserialize_with = "lenient")]
    reactions: Vec<ReactionDto>,
    #[serde(default, deserialize_with = "lenient")]
    referenced_message: Option<WireReply>,
    #[serde(default, deserialize_with = "items")]
    mentions: Vec<WireUser>,
    #[serde(default, deserialize_with = "lenient")]
    mention_roles: Vec<String>,
    #[serde(default, deserialize_with = "lenient")]
    mention_everyone: bool,
    #[serde(default, deserialize_with = "lenient")]
    thread: Option<WireChannel>,
    #[serde(default, rename = "type", deserialize_with = "lenient")]
    kind: u8,
    #[serde(default, deserialize_with = "lenient")]
    call: Option<WireCall>,
}

#[derive(Deserialize)]
struct WireCall {
    #[serde(default)]
    ended_timestamp: Option<String>,
}

/// The line Discord shows in place of a system message, after the author name.
fn system_text(wire: &WireMessage) -> Option<String> {
    let target = || {
        wire.mentions.first().map_or_else(
            || "someone".into(),
            |user| {
                user.global_name
                    .clone()
                    .unwrap_or_else(|| user.username.clone())
            },
        )
    };
    Some(match wire.kind {
        1 => format!("added {} to the group.", target()),
        2 if wire
            .mentions
            .first()
            .is_none_or(|user| user.id == wire.author.id) =>
        {
            "left the group.".into()
        }
        2 => format!("removed {} from the group.", target()),
        3 => call_text(
            &wire.timestamp,
            wire.call
                .as_ref()
                .and_then(|call| call.ended_timestamp.as_deref()),
        ),
        4 => format!("changed the channel name: {}", wire.content),
        5 => "changed the channel icon.".into(),
        6 => "pinned a message to this channel.".into(),
        7 => "joined the server.".into(),
        8..=11 => "boosted the server.".into(),
        _ => return None,
    })
}

fn call_text(started: &str, ended: Option<&str>) -> String {
    let minutes = ended.and_then(|ended| {
        let at = |time: &str| chrono::DateTime::parse_from_rfc3339(time).ok();
        Some(((at(ended)? - at(started)?).num_seconds() + 59).max(60) / 60)
    });
    minutes.map_or_else(
        || "started a call.".into(),
        |minutes| {
            format!(
                "started a call that lasted {minutes} minute{}.",
                if minutes == 1 { "" } else { "s" }
            )
        },
    )
}

impl From<WireMessage> for ChatMessageDto {
    fn from(wire: WireMessage) -> Self {
        let system = system_text(&wire);
        let call = wire.kind == 3;
        let author = UserDto::from(wire.author);
        Self {
            poll: wire.poll,
            stickers: wire.sticker_items,
            reference: wire.message_reference,
            edited_timestamp: wire.edited_timestamp,
            member: wire.member,
            reactions: wire.reactions,
            reply: wire.referenced_message.map(|reply| ReplyDto {
                attachment: !reply.attachments.is_empty(),
                author: reply
                    .author
                    .map_or_else(|| "unknown".into(), |author| UserDto::from(author).name),
                content: reply.content,
            }),
            id: wire.id,
            channel_id: wire.channel_id,
            author: author.name,
            avatar: author.avatar,
            author_id: author.id,
            mentions: wire.mentions.into_iter().map(Into::into).collect(),
            mention_roles: wire.mention_roles,
            mention_everyone: wire.mention_everyone,
            thread: wire.thread.map(|thread| thread.into_dto(wire.guild_id).0),
            content: wire.content,
            timestamp: wire.timestamp,
            attachments: wire.attachments,
            embeds: wire.embeds,
            forwarded: wire
                .message_snapshots
                .into_iter()
                .map(|snapshot| snapshot.message)
                .collect(),
            system,
            call,
        }
    }
}

pub fn message_from_value(value: &Value) -> Option<ChatMessageDto> {
    WireMessage::deserialize(value).ok().map(Into::into)
}

fn parse_messages(value: &Value) -> Result<Vec<ChatMessageDto>, String> {
    Ok(Vec::<WireMessage>::deserialize(value)
        .map_err(|_| "Discord returned unreadable message history.")?
        .into_iter()
        .map(Into::into)
        .collect())
}

/// Keeps poll results and reactor avatars that Discord omits from message updates.
pub fn carry_over(message: &mut ChatMessageDto, old: &ChatMessageDto) {
    carry_poll(&mut message.poll, &old.poll);
    carry_reactions(&mut message.reactions, &old.reactions);
}

fn carry_poll(poll: &mut Option<PollDto>, old: &Option<PollDto>) {
    if let (Some(poll), Some(old)) = (poll, old) {
        if poll.results.is_none() {
            poll.results.clone_from(&old.results);
        }
    }
}

fn carry_reactions(reactions: &mut [ReactionDto], old: &[ReactionDto]) {
    for reaction in reactions {
        if let Some(old) = old.iter().find(|old| old.emoji == reaction.emoji) {
            reaction.users.clone_from(&old.users);
        }
    }
}

pub fn patch_message(message: &mut ChatMessageDto, value: &Value) {
    if let Some(Ok(mut poll)) = value.get("poll").map(Option::<PollDto>::deserialize) {
        carry_poll(&mut poll, &message.poll);
        message.poll = poll;
    }
    if let Some(Ok(stickers)) = value.get("sticker_items").map(Vec::deserialize) {
        message.stickers = stickers;
    }
    if let Some(edited) = value.get("edited_timestamp") {
        message.edited_timestamp = edited.as_str().map(str::to_owned);
    }
    if let Some(member) = value.get("member") {
        message.member.get_or_insert_default().update(member);
    }
    if let Some(Ok(mut reactions)) = value.get("reactions").map(Vec::<ReactionDto>::deserialize) {
        carry_reactions(&mut reactions, &message.reactions);
        message.reactions = reactions;
    }
    if let Some(Ok(rows)) = value.get("message_snapshots").map(items::<_, WireSnapshot>) {
        message.forwarded = rows.into_iter().map(|row| row.message).collect();
    }
    if let Some(content) = value.get("content").and_then(|v| v.as_str()) {
        message.content = content.to_string();
    }
    if let Some(timestamp) = value.get("timestamp").and_then(|v| v.as_str()) {
        message.timestamp = timestamp.to_string();
    }
    if let Some(call) = value.get("call").filter(|_| message.call) {
        message.system = Some(call_text(
            &message.timestamp,
            call["ended_timestamp"].as_str(),
        ));
    }
    if let Some(user) = value.get("author").and_then(parse_user) {
        message.author_id = user.id;
        message.author = user.name;
        message.avatar = user.avatar;
    }
    if let Some(thread) = value.get("thread") {
        message.thread =
            parse_channel(thread, value["guild_id"].as_str()).map(|(thread, _)| thread);
    }
    if let Some(Ok(rows)) = value.get("mentions").map(items::<_, WireUser>) {
        message.mentions = rows.into_iter().map(Into::into).collect();
    }
    if let Some(Ok(rows)) = value.get("attachments").map(items) {
        message.attachments = rows;
    }
    if let Some(Ok(rows)) = value.get("embeds").map(items) {
        message.embeds = rows;
    }
}

#[derive(Default, Deserialize)]
struct ThreadMetadata {
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    locked: bool,
}

#[derive(Deserialize)]
pub struct WireChannel {
    id: String,
    #[serde(rename = "type")]
    kind: i32,
    #[serde(default)]
    guild_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    last_message_id: Option<String>,
    #[serde(default, deserialize_with = "items")]
    recipients: Vec<WireUser>,
    #[serde(default)]
    recipient_ids: Vec<String>,
    #[serde(default)]
    member: Option<IgnoredAny>,
    #[serde(default, deserialize_with = "lenient")]
    thread_metadata: ThreadMetadata,
    #[serde(default, deserialize_with = "lenient")]
    rate_limit_per_user: u64,
    #[serde(default, deserialize_with = "lenient")]
    position: i64,
    #[serde(default, deserialize_with = "items")]
    permission_overwrites: Vec<PermissionOverwrite>,
    #[serde(default, deserialize_with = "lenient")]
    parent_id: Option<String>,
}

impl WireChannel {
    /// Returns the channel and its recipients. `guild` fills a missing guild ID.
    pub fn into_dto(self, guild: Option<String>) -> (ChannelDto, Vec<UserDto>) {
        let users: Vec<UserDto> = self.recipients.into_iter().map(Into::into).collect();
        let name = self.name.filter(|name| !name.is_empty());
        // 1 = DM, 3 = group DM. Prefer recipient display names/avatars over raw group labels.
        let (name, icon) = match self.kind {
            1 => (
                users
                    .first()
                    .map(|person| person.name.clone())
                    .filter(|name| !name.is_empty())
                    .or(name)
                    .unwrap_or_else(|| "Direct message".into()),
                users.first().and_then(|person| person.avatar.clone()),
            ),
            3 => (
                name.unwrap_or_else(|| {
                    if users.is_empty() {
                        "Group chat".into()
                    } else {
                        users
                            .iter()
                            .map(|person| person.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                }),
                channel_icon_url(&self.id, &self.icon)
                    .or_else(|| users.first().and_then(|person| person.avatar.clone())),
            ),
            _ => (name.unwrap_or_else(|| "channel".into()), None),
        };
        let mut recipient_ids = self.recipient_ids;
        for user in &users {
            if !recipient_ids.contains(&user.id) {
                recipient_ids.push(user.id.clone());
            }
        }
        let channel = ChannelDto {
            thread_joined: self.member.is_some(),
            thread_archived: self.thread_metadata.archived,
            thread_locked: self.thread_metadata.locked,
            slowmode_seconds: self.rate_limit_per_user,
            position: self.position,
            overwrites: self.permission_overwrites,
            parent_id: self.parent_id,
            id: self.id,
            guild_id: self.guild_id.or(guild),
            name,
            kind: self.kind,
            icon,
            last_message_id: self.last_message_id,
            recipient_ids,
        };
        (channel, users)
    }
}

pub fn parse_channel(value: &Value, guild: Option<&str>) -> Option<(ChannelDto, Vec<UserDto>)> {
    let channel = WireChannel::deserialize(value).ok()?;
    Some(channel.into_dto(guild.map(str::to_owned)))
}

/// Toggles `key` without an index, otherwise moves it to that position.
pub fn edit_favorites(favorites: &mut Vec<String>, key: &str, index: Option<usize>) {
    match (favorites.iter().position(|row| row == key), index) {
        (Some(at), None) => {
            favorites.remove(at);
        }
        (None, None) => favorites.push(key.to_owned()),
        (Some(at), Some(index)) => {
            let row = favorites.remove(at);
            favorites.insert(index.min(favorites.len()), row);
        }
        (None, Some(_)) => {}
    }
}

// Read length-delimited fields while skipping the other protobuf wire types.

fn proto_varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Some(value);
        }
    }
    None
}

enum Proto<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

fn proto_fields(mut bytes: &[u8]) -> Option<Vec<(u64, Proto<'_>)>> {
    let mut fields = Vec::new();
    while !bytes.is_empty() {
        let tag = proto_varint(&mut bytes)?;
        if tag >> 3 == 0 {
            return None;
        }
        let value = match tag & 7 {
            0 => Proto::Varint(proto_varint(&mut bytes)?),
            1 => {
                bytes = bytes.get(8..)?;
                Proto::Fixed
            }
            5 => {
                bytes = bytes.get(4..)?;
                Proto::Fixed
            }
            2 => {
                let length = usize::try_from(proto_varint(&mut bytes)?).ok()?;
                let value = bytes.get(..length)?;
                bytes = &bytes[length..];
                Proto::Bytes(value)
            }
            _ => return None,
        };
        fields.push((tag >> 3, value));
    }
    Some(fields)
}

fn proto_strings(bytes: &[u8], field: u64) -> Option<Vec<&[u8]>> {
    Some(
        proto_fields(bytes)?
            .into_iter()
            .filter_map(|(tag, value)| match value {
                Proto::Bytes(value) if tag == field => Some(value),
                _ => None,
            })
            .collect(),
    )
}

fn push_varint(output: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        output.push((n as u8 & 127) | 128);
        n >>= 7;
    }
    output.push(n as u8);
}

fn proto_bytes(output: &mut Vec<u8>, field: u64, value: &[u8]) {
    push_varint(output, field << 3 | 2);
    push_varint(output, value.len() as u64);
    output.extend(value);
}

fn proto_utf8(value: &[u8]) -> Result<String, String> {
    std::str::from_utf8(value)
        .map(str::to_owned)
        .map_err(|_| "Unreadable emoji settings".into())
}

/// FrecencyUserSettings.favorite_emojis (5), FavoriteEmojis.emojis (1).
fn favorite_list(settings: &[u8]) -> Result<Vec<String>, String> {
    let groups = proto_strings(settings, 5).ok_or("Unreadable emoji favorites")?;
    let mut favorites = Vec::new();
    for group in groups {
        for name in proto_strings(group, 1).ok_or("Unreadable emoji favorites")? {
            favorites.push(proto_utf8(name)?);
        }
    }
    Ok(favorites)
}

/// FrecencyUserSettings.emoji_frecency (6) maps names to raw FrecencyItem bytes.
fn emoji_frecency(settings: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let unreadable = || "Unreadable emoji frecency".to_owned();
    let mut entries = Vec::new();
    for map in proto_strings(settings, 6).ok_or_else(unreadable)? {
        for entry in proto_strings(map, 1).ok_or_else(unreadable)? {
            let key = proto_strings(entry, 1).ok_or_else(unreadable)?;
            let value = proto_strings(entry, 2).ok_or_else(unreadable)?;
            entries.push((
                proto_utf8(key.first().ok_or_else(unreadable)?)?,
                value.first().map_or_else(Vec::new, |value| value.to_vec()),
            ));
        }
    }
    Ok(entries)
}

#[derive(Default)]
struct Frecency {
    total: u64,
    recent: Vec<u64>,
}

/// FrecencyItem: total_uses (1), recent_uses (2), frecency (3).
fn parse_frecency(bytes: &[u8]) -> Option<Frecency> {
    let mut item = Frecency::default();
    for (field, value) in proto_fields(bytes)? {
        match (field, value) {
            (1, Proto::Varint(total)) => item.total = total,
            (2, Proto::Varint(used)) => item.recent.push(used),
            (2, Proto::Bytes(mut packed)) => {
                while !packed.is_empty() {
                    item.recent.push(proto_varint(&mut packed)?);
                }
            }
            _ => {}
        }
    }
    Some(item)
}

fn encode_frecency(item: &Frecency) -> Vec<u8> {
    let mut output = Vec::new();
    push_varint(&mut output, 1 << 3);
    push_varint(&mut output, item.total);
    let mut recent = Vec::new();
    for used in &item.recent {
        push_varint(&mut recent, *used);
    }
    proto_bytes(&mut output, 2, &recent);
    push_varint(&mut output, 3 << 3);
    push_varint(&mut output, frecency_score(item, now_millis()));
    output
}

/// Matches Discord's client, which weights recent uses by age.
fn frecency_score(item: &Frecency, now: u64) -> u64 {
    if item.recent.is_empty() {
        return 0;
    }
    let bonus: u64 = item
        .recent
        .iter()
        .map(|used| {
            let days = now.saturating_sub(*used) as f64 / 86_400_000.0;
            match days {
                ..=3.0 => 100,
                ..=15.0 => 70,
                ..=30.0 => 50,
                ..=45.0 => 30,
                ..=80.0 => 10,
                _ => 0,
            }
        })
        .sum();
    (item.total * bonus).div_ceil(item.recent.len() as u64)
}

fn rank_frecency(entries: &[(String, Vec<u8>)]) -> Vec<String> {
    let now = now_millis();
    let mut ranked: Vec<_> = entries
        .iter()
        .filter_map(|(name, value)| Some((frecency_score(&parse_frecency(value)?, now), name)))
        .filter(|(score, _)| *score > 0)
        .collect();
    ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    ranked.into_iter().map(|(_, name)| name.clone()).collect()
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_millis() as u64)
}
