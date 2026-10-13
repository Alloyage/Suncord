use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserDto {
    pub id: String,
    pub name: String,
    pub avatar: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuildDto {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelDto {
    pub id: String,
    pub guild_id: Option<String>,
    pub name: String,
    pub kind: i32,
    pub parent_id: Option<String>,
    pub position: i32,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub last_message_id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDto {
    pub id: String,
    pub filename: String,
    pub url: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaDto {
    pub url: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbedDto {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub color: Option<u32>,
    #[serde(default)]
    pub image: Option<MediaDto>,
    #[serde(default)]
    pub thumbnail: Option<MediaDto>,
    #[serde(default)]
    pub video: Option<MediaDto>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessageDto {
    pub id: String,
    pub channel_id: String,
    pub author_id: String,
    pub author: String,
    pub avatar: Option<String>,
    pub content: String,
    pub timestamp: String,
    pub nonce: Option<String>,
    pub pending: bool,
    #[serde(default)]
    pub attachments: Vec<AttachmentDto>,
    #[serde(default)]
    pub embeds: Vec<EmbedDto>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuildFolderDto {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color: Option<u32>,
    #[serde(default)]
    pub guild_ids: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub user: Option<UserDto>,
    pub guilds: Vec<GuildDto>,
    pub channels: Vec<ChannelDto>,
    pub dms: Vec<ChannelDto>,
    pub messages: HashMap<String, Vec<ChatMessageDto>>,
    #[serde(default)]
    pub users: HashMap<String, UserDto>,
    #[serde(default)]
    pub folders: Vec<GuildFolderDto>,
    pub active_guild_id: Option<String>,
    pub active_channel_id: Option<String>,
}

impl Snapshot {
    pub fn empty() -> Self {
        Self {
            user: None,
            guilds: Vec::new(),
            channels: Vec::new(),
            dms: Vec::new(),
            messages: HashMap::new(),
            users: HashMap::new(),
            folders: Vec::new(),
            active_guild_id: None,
            active_channel_id: None,
        }
    }
}

pub fn channel_icon_url(channel_id: &str, icon: &Option<String>) -> Option<String> {
    let hash = icon.as_ref()?;
    Some(format!(
        "https://cdn.discordapp.com/channel-icons/{channel_id}/{hash}.webp?size=64"
    ))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub buffer_frames: u32,
    pub hardware_accel: bool,
    pub adaptive_throttle: bool,
    pub capture_fps: u32,
    pub hw_preference: String,
    pub asr_enabled: bool,
    pub asr_model_path: String,
    pub input_device: String,
    pub output_device: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            buffer_frames: 480,
            hardware_accel: true,
            adaptive_throttle: false,
            capture_fps: 60,
            hw_preference: "gpu".into(),
            asr_enabled: false,
            asr_model_path: String::new(),
            input_device: String::new(),
            output_device: String::new(),
        }
    }
}

impl Settings {
    pub fn clamp(mut self) -> Self {
        self.buffer_frames = self.buffer_frames.clamp(128, 4_800);
        self.capture_fps = self.capture_fps.clamp(15, 240);
        if !matches!(self.hw_preference.as_str(), "auto" | "gpu" | "cpu") {
            self.hw_preference = "gpu".into();
        }
        self
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    pub buffer_frames: Option<u32>,
    pub hardware_accel: Option<bool>,
    pub adaptive_throttle: Option<bool>,
    pub capture_fps: Option<u32>,
    pub hw_preference: Option<String>,
    pub asr_enabled: Option<bool>,
    pub asr_model_path: Option<String>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bootstrap {
    pub signed_in: bool,
    pub snapshot: Snapshot,
    pub settings: Settings,
    pub phase: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusEvent {
    pub phase: String,
    pub detail: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    pub message: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEvent {
    pub channel_id: String,
    pub messages: Vec<ChatMessageDto>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDeleteEvent {
    pub channel_id: String,
    pub message_ids: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceMemberDto {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub avatar: Option<String>,
    pub muted: bool,
    pub deafened: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceStatus {
    pub phase: String,
    pub channel_id: Option<String>,
    #[serde(default)]
    pub channel_name: Option<String>,
    pub detail: String,
    pub muted: bool,
    pub deafened: bool,
    pub level: f32,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_frames: u32,
    pub hardware_accel: bool,
    pub adaptive_throttle: bool,
    pub transport: String,
    pub dave: String,
    pub encoder: String,
    #[serde(default)]
    pub members: Vec<VoiceMemberDto>,
}

impl Default for VoiceStatus {
    fn default() -> Self {
        Self {
            phase: "idle".into(),
            channel_id: None,
            channel_name: None,
            detail: "Not in voice".into(),
            muted: false,
            deafened: false,
            level: 0.0,
            sample_rate: 48_000,
            channels: 2,
            buffer_frames: 480,
            hardware_accel: true,
            adaptive_throttle: false,
            transport: "idle".into(),
            dave: "idle".into(),
            encoder: encoder_name(),
            members: Vec::new(),
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStats {
    pub running: bool,
    pub source: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub capture_ms: f32,
    pub dropped: u64,
    pub path: String,
    #[serde(default)]
    pub bitrate_kbps: f32,
}

/// Compressed preview frame for the WebGL monitor. Capture itself stays native RGBA.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureFrame {
    pub width: u32,
    pub height: u32,
    pub png_base64: String,
}

impl Default for CaptureStats {
    fn default() -> Self {
        Self {
            running: false,
            source: String::new(),
            width: 0,
            height: 0,
            fps: 0.0,
            capture_ms: 0.0,
            dropped: 0,
            path: "idle".into(),
            bitrate_kbps: 0.0,
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceDto {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDeviceDto {
    pub name: String,
    pub input: bool,
    pub output: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AsrStatus {
    pub enabled: bool,
    pub detail: String,
    pub caption: String,
    pub level: f32,
}

#[derive(Clone)]
pub enum VoiceSignal {
    State {
        guild_id: Option<String>,
        channel_id: Option<String>,
        user_id: String,
        session_id: String,
        self_mute: bool,
        self_deaf: bool,
        mute: bool,
        deaf: bool,
    },
    Server {
        guild_id: Option<String>,
        endpoint: String,
        token: String,
    },
}

pub fn encoder_name() -> String {
    "opus".into()
}

pub fn avatar_url(user_id: &str, avatar: &Option<String>) -> Option<String> {
    let hash = avatar.as_ref()?.trim();
    if hash.is_empty() {
        return None;
    }
    if hash.starts_with("http://") || hash.starts_with("https://") {
        return Some(hash.to_string());
    }
    let ext = if hash.starts_with("a_") { "gif" } else { "png" };
    Some(format!(
        "https://cdn.discordapp.com/avatars/{user_id}/{hash}.{ext}?size=128"
    ))
}

pub fn guild_icon_url(guild_id: &str, icon: &Option<String>) -> Option<String> {
    let mut hash = icon.as_ref()?.trim().to_string();
    if hash.is_empty() || hash == "null" || hash == "undefined" {
        return None;
    }
    // Already a CDN URL (re-hydrated snapshot) — prefer png over webp.
    if hash.starts_with("http://") || hash.starts_with("https://") {
        if let Some(idx) = hash.find(".webp") {
            hash.replace_range(idx..idx + 5, ".png");
        }
        return Some(hash);
    }
    let cleaned = hash
        .split('?')
        .next()
        .unwrap_or(&hash)
        .trim_end_matches(".png")
        .trim_end_matches(".gif")
        .trim_end_matches(".webp")
        .trim_end_matches(".jpg")
        .trim_end_matches(".jpeg")
        .to_string();
    // Discord icon hashes are hex (animated prefixes start with a_).
    let body = cleaned.strip_prefix("a_").unwrap_or(cleaned.as_str());
    if body.len() < 8 || !body.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let ext = if cleaned.starts_with("a_") { "gif" } else { "png" };
    Some(format!(
        "https://cdn.discordapp.com/icons/{guild_id}/{cleaned}.{ext}?size=128"
    ))
}

pub fn display_name(global: &Option<String>, username: &str) -> String {
    global
        .as_ref()
        .filter(|name| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| username.to_string())
}
