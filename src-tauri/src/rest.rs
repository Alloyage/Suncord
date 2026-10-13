use crate::models::{
    avatar_url, channel_icon_url, display_name, AttachmentDto, ChannelDto, ChatMessageDto, EmbedDto,
    MediaDto, UserDto,
};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use zeroize::Zeroizing;

const API: &str = "https://discord.com/api/v10";
const DESKTOP_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";

#[derive(Clone)]
pub struct DiscordRest {
    http: reqwest::Client,
}

impl DiscordRest {
    pub fn new() -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(DESKTOP_UA)
            .build()
            .map_err(|err| err.to_string())?;
        Ok(Self { http })
    }

    pub async fn current_user(&self, token: &Zeroizing<String>) -> Result<UserDto, String> {
        let value = self.get(token, "/users/@me").await?;
        Ok(user_from_value(&value))
    }

    pub async fn dm_channels(&self, token: &Zeroizing<String>) -> Result<Vec<ChannelDto>, String> {
        let value = self.get(token, "/users/@me/channels").await?;
        let rows = value.as_array().cloned().unwrap_or_default();
        Ok(rows.iter().filter_map(dm_channel_from_value).collect())
    }

    pub async fn messages(
        &self,
        token: &Zeroizing<String>,
        channel_id: &str,
    ) -> Result<Vec<ChatMessageDto>, String> {
        let value = self
            .get(token, &format!("/channels/{channel_id}/messages?limit=50"))
            .await?;
        let rows = value.as_array().cloned().unwrap_or_default();
        Ok(rows.iter().filter_map(message_from_value).collect())
    }

    pub async fn pins(
        &self,
        token: &Zeroizing<String>,
        channel_id: &str,
    ) -> Result<Vec<ChatMessageDto>, String> {
        let value = self
            .get(token, &format!("/channels/{channel_id}/pins"))
            .await?;
        let rows = value.as_array().cloned().unwrap_or_default();
        Ok(rows.iter().filter_map(message_from_value).collect())
    }

    pub async fn send_message(
        &self,
        token: &Zeroizing<String>,
        channel_id: &str,
        content: &str,
        nonce: &str,
    ) -> Result<ChatMessageDto, String> {
        let value = self
            .post(
                token,
                &format!("/channels/{channel_id}/messages"),
                json!({ "content": content, "nonce": nonce }),
            )
            .await?;
        message_from_value(&value).ok_or_else(|| "Discord returned an unreadable message.".into())
    }

    pub async fn edit_message(
        &self,
        token: &Zeroizing<String>,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<ChatMessageDto, String> {
        let value = self
            .patch(
                token,
                &format!("/channels/{channel_id}/messages/{message_id}"),
                json!({ "content": content }),
            )
            .await?;
        message_from_value(&value).ok_or_else(|| "Discord returned an unreadable message.".into())
    }

    pub async fn delete_message(
        &self,
        token: &Zeroizing<String>,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), String> {
        self.send(
            token,
            reqwest::Method::DELETE,
            &format!("/channels/{channel_id}/messages/{message_id}"),
            None,
        )
        .await
        .map(|_| ())
    }

    pub async fn add_reaction(
        &self,
        token: &Zeroizing<String>,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), String> {
        let encoded = urlencoding_emoji(emoji);
        self.send(
            token,
            reqwest::Method::PUT,
            &format!("/channels/{channel_id}/messages/{message_id}/reactions/{encoded}/@me"),
            None,
        )
        .await
        .map(|_| ())
    }

    async fn get(&self, token: &Zeroizing<String>, path: &str) -> Result<serde_json::Value, String> {
        self.send(token, reqwest::Method::GET, path, None).await
    }

    async fn post(
        &self,
        token: &Zeroizing<String>,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.send(token, reqwest::Method::POST, path, Some(body))
            .await
    }

    async fn patch(
        &self,
        token: &Zeroizing<String>,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.send(token, reqwest::Method::PATCH, path, Some(body))
            .await
    }

    async fn send(
        &self,
        token: &Zeroizing<String>,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut request = self
                .http
                .request(method.clone(), format!("{API}{path}"))
                .headers(headers(token)?);
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request.send().await.map_err(|err| err.to_string())?;
            let status = response.status();
            if status.as_u16() == 429 && attempt == 1 {
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<f64>().ok())
                    .unwrap_or(1.0);
                tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.2, 5.0))).await;
                continue;
            }
            if !status.is_success() {
                return Err(match status.as_u16() {
                    401 => "Discord rejected the saved session.".into(),
                    403 => "Discord refused that request.".into(),
                    _ => format!("Discord returned HTTP {}.", status.as_u16()),
                });
            }
            if status.as_u16() == 204
                || response
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    == Some("0")
            {
                return Ok(serde_json::Value::Null);
            }
            let bytes = response.bytes().await.map_err(|err| err.to_string())?;
            if bytes.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            return serde_json::from_slice(&bytes)
                .map_err(|_| "Discord returned an unreadable response.".into());
        }
    }
}

fn urlencoding_emoji(emoji: &str) -> String {
    emoji
        .chars()
        .map(|ch| {
            let mut buf = [0u8; 4];
            let encoded = ch.encode_utf8(&mut buf);
            encoded
                .bytes()
                .map(|byte| format!("%{byte:02X}"))
                .collect::<String>()
        })
        .collect()
}

fn headers(token: &Zeroizing<String>) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(token).map_err(|_| "session token is not a valid header")?,
    );
    headers.insert(USER_AGENT, HeaderValue::from_static(DESKTOP_UA));
    Ok(headers)
}

#[derive(Deserialize)]
struct WireUser {
    id: String,
    username: String,
    #[serde(default)]
    global_name: Option<String>,
    #[serde(default)]
    avatar: Option<String>,
}

pub fn user_from_value(value: &serde_json::Value) -> UserDto {
    let wire: WireUser = serde_json::from_value(value.clone()).unwrap_or(WireUser {
        id: String::new(),
        username: "unknown".into(),
        global_name: None,
        avatar: None,
    });
    UserDto {
        avatar: avatar_url(&wire.id, &wire.avatar),
        name: display_name(&wire.global_name, &wire.username),
        id: wire.id,
    }
}

#[derive(Deserialize)]
struct WireMessage {
    id: String,
    channel_id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    nonce: Option<serde_json::Value>,
    author: WireUser,
    #[serde(default)]
    attachments: Vec<serde_json::Value>,
    #[serde(default)]
    embeds: Vec<serde_json::Value>,
}

pub fn message_from_value(value: &serde_json::Value) -> Option<ChatMessageDto> {
    let wire: WireMessage = serde_json::from_value(value.clone()).ok()?;
    let attachments = wire
        .attachments
        .iter()
        .filter_map(attachment_from_value)
        .collect::<Vec<_>>();
    let embeds = wire
        .embeds
        .iter()
        .filter_map(embed_from_value)
        .collect::<Vec<_>>();
    let nonce = wire.nonce.and_then(|value| match value {
        serde_json::Value::String(text) => Some(text),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    });
    Some(ChatMessageDto {
        id: wire.id,
        channel_id: wire.channel_id,
        author: display_name(&wire.author.global_name, &wire.author.username),
        avatar: avatar_url(&wire.author.id, &wire.author.avatar),
        author_id: wire.author.id,
        content: wire.content,
        timestamp: wire.timestamp,
        nonce,
        pending: false,
        attachments,
        embeds,
    })
}

fn attachment_from_value(value: &serde_json::Value) -> Option<AttachmentDto> {
    #[derive(Deserialize)]
    struct WireAttachment {
        id: String,
        filename: String,
        url: String,
        #[serde(default)]
        proxy_url: Option<String>,
        #[serde(default)]
        content_type: Option<String>,
        #[serde(default)]
        width: Option<u32>,
        #[serde(default)]
        height: Option<u32>,
    }
    let wire: WireAttachment = serde_json::from_value(value.clone()).ok()?;
    Some(AttachmentDto {
        id: wire.id,
        filename: wire.filename,
        url: wire.url,
        proxy_url: wire.proxy_url,
        content_type: wire.content_type,
        width: wire.width,
        height: wire.height,
    })
}

fn embed_from_value(value: &serde_json::Value) -> Option<EmbedDto> {
    #[derive(Deserialize)]
    struct WireEmbed {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        color: Option<u32>,
        #[serde(default)]
        image: Option<serde_json::Value>,
        #[serde(default)]
        thumbnail: Option<serde_json::Value>,
        #[serde(default)]
        video: Option<serde_json::Value>,
    }
    let wire: WireEmbed = serde_json::from_value(value.clone()).ok()?;
    let image = wire.image.as_ref().and_then(media_from_value);
    let thumbnail = wire.thumbnail.as_ref().and_then(media_from_value);
    let video = wire.video.as_ref().and_then(media_from_value);
    if wire.title.is_none()
        && wire.description.is_none()
        && image.is_none()
        && thumbnail.is_none()
        && video.is_none()
    {
        return None;
    }
    Some(EmbedDto {
        title: wire.title,
        description: wire.description,
        url: wire.url,
        color: wire.color,
        image,
        thumbnail,
        video,
    })
}

fn dm_channel_from_value(value: &serde_json::Value) -> Option<ChannelDto> {
    #[derive(Deserialize)]
    struct WireRecipient {
        #[serde(default)]
        id: String,
        #[serde(default)]
        username: String,
        #[serde(default)]
        global_name: Option<String>,
        #[serde(default)]
        avatar: Option<String>,
    }
    #[derive(Deserialize)]
    struct WireChannel {
        id: String,
        #[serde(rename = "type")]
        kind: i32,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        icon: Option<String>,
        #[serde(default)]
        last_message_id: Option<String>,
        #[serde(default)]
        recipients: Vec<WireRecipient>,
    }
    let wire: WireChannel = serde_json::from_value(value.clone()).ok()?;
    let users = wire
        .recipients
        .iter()
        .map(|person| {
            (
                display_name(&person.global_name, &person.username),
                avatar_url(&person.id, &person.avatar),
            )
        })
        .collect::<Vec<_>>();
    let (name, icon) = match wire.kind {
        1 => {
            let person = users.first();
            (
                person
                    .map(|(name, _)| name.clone())
                    .filter(|name| !name.is_empty())
                    .or_else(|| wire.name.filter(|name| !name.is_empty()))
                    .unwrap_or_else(|| "Direct message".into()),
                person.and_then(|(_, icon)| icon.clone()),
            )
        }
        3 => (
            wire.name.filter(|name| !name.is_empty()).unwrap_or_else(|| {
                if users.is_empty() {
                    "Group chat".into()
                } else {
                    users
                        .iter()
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            }),
            channel_icon_url(&wire.id, &wire.icon)
                .or_else(|| users.first().and_then(|(_, icon)| icon.clone())),
        ),
        _ => (
            wire.name.unwrap_or_else(|| "channel".into()),
            None,
        ),
    };
    Some(ChannelDto {
        id: wire.id,
        guild_id: None,
        name,
        kind: wire.kind,
        parent_id: None,
        position: 0,
        icon,
        last_message_id: wire.last_message_id,
    })
}

fn media_from_value(value: &serde_json::Value) -> Option<MediaDto> {
    #[derive(Deserialize)]
    struct WireMedia {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        proxy_url: Option<String>,
        #[serde(default)]
        width: Option<u32>,
        #[serde(default)]
        height: Option<u32>,
    }
    let wire: WireMedia = serde_json::from_value(value.clone()).ok()?;
    let url = wire.url.filter(|url| !url.is_empty())?;
    Some(MediaDto {
        url,
        proxy_url: wire.proxy_url,
        width: wire.width,
        height: wire.height,
    })
}
