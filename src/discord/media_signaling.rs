//! Native controller signaling only. These events contain credentials and must never
//! enter UI events, logs, snapshots, or browser responses. Signaling is not media readiness.
//!
//! Wire shapes: https://docs.discord.com/developers/events/gateway-events#update-voice-state
//! and https://docs.discord.food/gateway/gateway-events#create-stream

use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use zeroize::Zeroizing;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceTarget {
    /// None for a private call.
    pub guild_id: Option<String>,
    pub channel_id: String,
}

impl VoiceTarget {
    pub fn validate(&self) -> Result<(), SignalingError> {
        if !super::snowflake(&self.channel_id)
            || self
                .guild_id
                .as_deref()
                .is_some_and(|id| !super::snowflake(id))
        {
            return Err(SignalingError::InvalidIdentifier);
        }
        Ok(())
    }

    /// Deafening also mutes, like the official client.
    pub fn voice_payload(&self, (mute, deaf): (bool, bool), leave: bool) -> Value {
        json!({"op": 4, "d": {
            "guild_id": self.guild_id,
            "channel_id": if leave { None } else { Some(&self.channel_id) },
            "self_mute": mute || deaf, "self_deaf": deaf
        }})
    }

    pub fn stream_key(&self, user: &str) -> Result<String, SignalingError> {
        self.validate()?;
        if !super::snowflake(user) {
            return Err(SignalingError::InvalidIdentifier);
        }
        Ok(match &self.guild_id {
            Some(guild) => format!("guild:{guild}:{}:{user}", self.channel_id),
            None => format!("call:{}:{user}", self.channel_id),
        })
    }
}

/// A local cancellation token, not a Discord nonce or proof of media readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceAttempt {
    pub gateway_generation: u64,
    pub serial: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum VoiceCommand {
    Leave,
    /// Self mute and deafen.
    SetVoice(bool, bool),
}

#[derive(Clone, Copy, Debug)]
pub enum StreamCommand {
    Create,
    Delete,
    SetPaused(bool),
    Watch(u64),
    StopWatching(u64),
}

impl StreamCommand {
    pub fn payload(self, target: &VoiceTarget, user: &str) -> Result<Value, SignalingError> {
        let key = match self {
            Self::Watch(owner) | Self::StopWatching(owner) => {
                if owner == 0 || owner.to_string() == user {
                    return Err(SignalingError::InvalidIdentifier);
                }
                target.stream_key(&owner.to_string())?
            }
            _ => target.stream_key(user)?,
        };
        Ok(match self {
            Self::Create => json!({"op": 18, "d": {
                "type": if target.guild_id.is_some() { "guild" } else { "call" },
                "guild_id": target.guild_id, "channel_id": target.channel_id,
                "preferred_region": null
            }}),
            Self::Delete => json!({"op": 19, "d": {"stream_key": key}}),
            Self::Watch(_) => json!({"op": 20, "d": {"stream_key": key}}),
            Self::StopWatching(_) => json!({"op": 19, "d": {"stream_key": key}}),
            Self::SetPaused(paused) => {
                json!({"op": 22, "d": {"stream_key": key, "paused": paused}})
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalingError {
    GatewayUnavailable,
    InvalidIdentifier,
    StaleAttempt,
    AlreadyActive,
}

impl std::fmt::Display for SignalingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::GatewayUnavailable => "No authenticated gateway socket is available",
            Self::InvalidIdentifier => "Identifiers must be nonempty decimal strings",
            Self::StaleAttempt => "The voice attempt was cancelled or invalidated",
            Self::AlreadyActive => "Disconnect the current voice session before joining again",
        })
    }
}

impl std::error::Error for SignalingError {}

/// Subscribe before sending commands. On broadcast lag, discard all credentials
/// and cancel the attempt. There is no replay of credentials for late subscribers.
/// Server credentials are limited to the current target. Own-account voice-state
/// moves outside that target are also delivered so the controller can stop.
/// `attempt` is the local attempt at receipt, not wire-level correlation. Discord
/// replies have no join nonce, and voice server updates can omit the channel ID.
/// The controller must also check user, target and session before using credentials.
#[derive(Clone)]
pub struct MediaEvent {
    pub attempt: Option<VoiceAttempt>,
    pub kind: MediaEventKind,
}

// Deliberately no Debug or Serialize on events or credential payloads.
#[derive(Clone)]
pub enum MediaEventKind {
    Invalidated,
    VoiceState(VoiceState),
    VoiceServer(VoiceServer),
    StreamCreate(StreamCreate),
    StreamServer(StreamServer),
    StreamDelete(StreamDelete),
    StreamUpdate(StreamUpdate),
}

impl MediaEventKind {
    pub fn matches_target(
        &self,
        target: &VoiceTarget,
        user: &str,
        watched_owner: Option<u64>,
    ) -> bool {
        let matches_stream = |key: &str| {
            target.stream_key(user).is_ok_and(|own| own == key)
                || watched_owner.is_some_and(|owner| {
                    target
                        .stream_key(&owner.to_string())
                        .is_ok_and(|watched| watched == key)
                })
        };
        match self {
            // Own-account moves outside the target must also stop its native media.
            Self::VoiceState(state) => state.user_id == user,
            Self::VoiceServer(server) => {
                server.guild_id == target.guild_id
                    && match &server.channel_id {
                        Some(id) => id == &target.channel_id,
                        None => target.guild_id.is_some(),
                    }
            }
            Self::StreamCreate(stream) => matches_stream(&stream.stream_key),
            Self::StreamServer(stream) => matches_stream(&stream.stream_key),
            Self::StreamDelete(stream) => matches_stream(&stream.stream_key),
            Self::StreamUpdate(stream) => matches_stream(&stream.stream_key),
            _ => false,
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct VoiceState {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    pub user_id: String,
    #[serde(deserialize_with = "secret")]
    pub session_id: Zeroizing<String>,
    pub mute: bool,
    #[serde(default)]
    pub suppress: bool,
}

#[derive(Clone, Deserialize)]
pub struct VoiceServer {
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    #[serde(deserialize_with = "secret")]
    pub token: Zeroizing<String>,
    /// None requires tearing down transport and waiting for a new allocation.
    pub endpoint: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct StreamCreate {
    pub stream_key: String,
    pub rtc_server_id: String,
    #[serde(default)]
    pub viewer_ids: Vec<String>,
}

#[derive(Clone, Deserialize)]
pub struct StreamServer {
    pub stream_key: String,
    #[serde(deserialize_with = "secret")]
    pub token: Zeroizing<String>,
    pub endpoint: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct StreamDelete {
    pub stream_key: String,
}

#[derive(Clone, Deserialize)]
pub struct StreamUpdate {
    pub stream_key: String,
    #[serde(default)]
    pub viewer_ids: Vec<String>,
}

fn secret<'de, D: Deserializer<'de>>(d: D) -> Result<Zeroizing<String>, D::Error> {
    String::deserialize(d).map(Zeroizing::new)
}

pub fn parse_event(kind: &str, data: &Value) -> Option<MediaEventKind> {
    // Malformed credential events are dropped without logging their contents.
    Some(match kind {
        "VOICE_STATE_UPDATE" => MediaEventKind::VoiceState(Deserialize::deserialize(data).ok()?),
        "VOICE_SERVER_UPDATE" => MediaEventKind::VoiceServer(Deserialize::deserialize(data).ok()?),
        "STREAM_CREATE" => MediaEventKind::StreamCreate(Deserialize::deserialize(data).ok()?),
        "STREAM_SERVER_UPDATE" => {
            MediaEventKind::StreamServer(Deserialize::deserialize(data).ok()?)
        }
        "STREAM_DELETE" => MediaEventKind::StreamDelete(Deserialize::deserialize(data).ok()?),
        "STREAM_UPDATE" => MediaEventKind::StreamUpdate(Deserialize::deserialize(data).ok()?),
        _ => return None,
    })
}

#[derive(Default)]
pub struct MediaState {
    pub serial: u64,
    /// The attempt, its target and self mute and deafen.
    pub current: Option<(VoiceAttempt, VoiceTarget, (bool, bool))>,
    pub leaving: bool,
    pub watched_owner: Option<u64>,
    pub pending: std::collections::VecDeque<Value>,
}

impl MediaState {
    /// The current target and voice state, unless `attempt` is stale or leaving.
    pub fn attempt(
        &self,
        attempt: VoiceAttempt,
    ) -> Result<&(VoiceAttempt, VoiceTarget, (bool, bool)), SignalingError> {
        self.current
            .as_ref()
            .filter(|(current, _, _)| !self.leaving && *current == attempt)
            .ok_or(SignalingError::StaleAttempt)
    }
}
