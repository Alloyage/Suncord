use crate::auth::AuthStore;
use crate::cache::CacheStore;
use crate::models::{
    channel_icon_url, display_name, guild_icon_url, avatar_url, ChannelDto, GuildDto, HistoryEvent,
    MessageDeleteEvent, Note, StatusEvent, UserDto, VoiceSignal,
};
use crate::rest::{message_from_value, user_from_value, DiscordRest};
use crate::zlib_stream::ZlibStream;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig, Message},
};

/// Discord user-gateway capability flags (not bot intents).
/// Bit 9 (USER_SETTINGS_PROTO) is intentionally cleared so READY still includes
/// JSON `user_settings.guild_folders` instead of opaque protobuf blobs.
const USER_CAPABILITIES: u64 = 16381 & !(1 << 9);
const DESKTOP_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";
const CLIENT_BUILD: u64 = 280_231;

enum Outbound {
    Text(String),
    Pong(Vec<u8>),
}

enum Command {
    Connect,
    Disconnect,
}

struct Session {
    id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
}

struct Gate {
    app: AppHandle,
    auth: AuthStore,
    cache: Arc<CacheStore>,
    rest: DiscordRest,
    voice_tx: mpsc::UnboundedSender<VoiceSignal>,
    generation: AtomicU64,
    outbound: Mutex<Option<mpsc::UnboundedSender<Outbound>>>,
    session: Mutex<Session>,
    snapshot_pending: AtomicBool,
}

#[derive(Clone)]
pub struct GatewayHandle {
    cmd: mpsc::UnboundedSender<Command>,
    gate: Arc<Gate>,
}

impl GatewayHandle {
    pub fn start(
        app: AppHandle,
        auth: AuthStore,
        cache: Arc<CacheStore>,
        rest: DiscordRest,
        voice_tx: mpsc::UnboundedSender<VoiceSignal>,
    ) -> Self {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let gate = Arc::new(Gate {
            app,
            auth,
            cache,
            rest,
            voice_tx,
            generation: AtomicU64::new(0),
            outbound: Mutex::new(None),
            session: Mutex::new(Session {
                id: None,
                resume_url: None,
                seq: None,
            }),
            snapshot_pending: AtomicBool::new(false),
        });
        let worker = gate.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    Command::Connect => {
                        let generation = worker.generation.fetch_add(1, Ordering::AcqRel) + 1;
                        let gate = worker.clone();
                        tauri::async_runtime::spawn(async move {
                            run_session(gate, generation).await;
                        });
                    }
                    Command::Disconnect => {
                        worker.generation.fetch_add(1, Ordering::AcqRel);
                        *worker.outbound.lock() = None;
                    }
                }
            }
        });
        Self { cmd: cmd_tx, gate }
    }

    pub fn connect(&self) {
        let _ = self.cmd.send(Command::Connect);
    }

    pub fn disconnect(&self) {
        let _ = self.cmd.send(Command::Disconnect);
    }

    pub async fn voice_state(
        &self,
        guild_id: Option<String>,
        channel_id: Option<String>,
        self_mute: bool,
        self_deaf: bool,
    ) -> Result<(), String> {
        let payload = json!({
            "op": 4,
            "d": {
                "guild_id": guild_id,
                "channel_id": channel_id,
                "self_mute": self_mute,
                "self_deaf": self_deaf,
            }
        })
        .to_string();
        // Opcode 4 must leave the UI path quickly — brief retry only.
        for _ in 0..8 {
            if let Some(tx) = self.gate.outbound.lock().as_ref() {
                tx.send(Outbound::Text(payload))
                    .map_err(|_| "The gateway socket closed.".to_string())?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Err("The gateway is not connected.".into())
    }
}

async fn run_session(gate: Arc<Gate>, generation: u64) {
    let mut delay = 1u64;
    loop {
        if gate.generation.load(Ordering::Acquire) != generation {
            return;
        }
        if gate.auth.token().is_none() {
            emit_status(&gate.app, "offline", "Signed out");
            return;
        }
        emit_status(&gate.app, "connecting", "Opening the gateway");
        match connect_once(gate.clone(), generation).await {
            Ok(Stop::AuthFailed) => {
                let _ = gate.auth.clear();
                crate::auth::emit_auth(&gate.app, false);
                emit_error(&gate.app, "Discord rejected the saved session.");
                emit_status(&gate.app, "offline", "Session rejected");
                return;
            }
            Ok(Stop::Stopped) => return,
            Ok(Stop::Reconnect) | Err(_) => {
                if gate.generation.load(Ordering::Acquire) != generation {
                    return;
                }
                emit_status(&gate.app, "reconnecting", "Reconnecting");
                tokio::time::sleep(Duration::from_secs(delay)).await;
                delay = (delay * 2).min(30);
            }
        }
    }
}

enum Stop {
    Reconnect,
    AuthFailed,
    Stopped,
}

async fn connect_once(gate: Arc<Gate>, generation: u64) -> Result<Stop, String> {
    let resume = {
        let session = gate.session.lock();
        session.id.is_some()
    };
    let url = {
        let session = gate.session.lock();
        gateway_url(session.resume_url.as_deref())
    };
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|err| err.to_string())?;
    {
        let headers = request.headers_mut();
        headers.insert(
            "User-Agent",
            DESKTOP_UA.parse().map_err(|err| format!("{err}"))?,
        );
        headers.insert(
            "Origin",
            "https://discord.com"
                .parse()
                .map_err(|err| format!("{err}"))?,
        );
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(64 * 1024 * 1024))
        .max_frame_size(Some(64 * 1024 * 1024));
    log::info!("gateway connecting {url}");
    let (socket, _) = connect_async_with_config(request, Some(config), false)
        .await
        .map_err(|err| err.to_string())?;
    let (mut write, mut read) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    *gate.outbound.lock() = Some(out_tx.clone());
    let writer = tokio::spawn(async move {
        while let Some(outbound) = out_rx.recv().await {
            let message = match outbound {
                Outbound::Text(text) => Message::Text(text.into()),
                Outbound::Pong(payload) => Message::Pong(payload.into()),
            };
            if write.send(message).await.is_err() {
                break;
            }
        }
    });
    let missed = Arc::new(Notify::new());
    let acked = Arc::new(AtomicBool::new(true));
    let mut zlib = ZlibStream::new();
    let mut heartbeat: Option<tokio::task::JoinHandle<()>> = None;
    let mut stop = Stop::Reconnect;

    loop {
        if gate.generation.load(Ordering::Acquire) != generation {
            stop = Stop::Stopped;
            break;
        }
        let incoming = tokio::select! {
            _ = missed.notified() => {
                log::warn!("gateway heartbeat missed");
                stop = Stop::Reconnect;
                break;
            }
            message = read.next() => message,
        };
        let Some(message) = incoming else {
            log::warn!("gateway socket ended");
            break;
        };
        let message = match message {
            Ok(message) => message,
            Err(err) => {
                log::warn!("gateway read error: {err}");
                break;
            }
        };
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => match zlib.push(&bytes) {
                Ok(Some(decoded)) => String::from_utf8(decoded).unwrap_or_default(),
                Ok(None) => continue, // still waiting on 00 00 FF FF
                Err(err) => {
                    log::warn!("gateway zlib error: {err}");
                    continue;
                }
            },
            Message::Ping(payload) => {
                let _ = out_tx.send(Outbound::Pong(payload.to_vec()));
                continue;
            }
            Message::Close(frame) => {
                let code = frame.as_ref().map(|frame| u16::from(frame.code)).unwrap_or(0);
                let reason = frame
                    .as_ref()
                    .map(|frame| frame.reason.to_string())
                    .unwrap_or_default();
                log::warn!("gateway closed: code={code} reason={reason}");
                if code == 4004 {
                    stop = Stop::AuthFailed;
                } else if code == 4001 || code == 4003 || code == 4010 || code == 4011 {
                    // Bad identify / encoding — clear resume so the next try is a fresh IDENTIFY.
                    let mut session = gate.session.lock();
                    session.id = None;
                    session.resume_url = None;
                    session.seq = None;
                }
                break;
            }
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(err) => {
                log::warn!("gateway json error: {err}");
                continue;
            }
        };
        let op = value.get("op").and_then(|op| op.as_u64()).unwrap_or(99);
        if let Some(seq) = value.get("s").and_then(|seq| seq.as_u64()) {
            gate.session.lock().seq = Some(seq);
        }
        match op {
            10 => {
                let interval = value
                    .pointer("/d/heartbeat_interval")
                    .and_then(|interval| interval.as_u64())
                    .unwrap_or(41_250);
                log::info!("gateway HELLO heartbeat={interval}ms");
                if let Some(task) = heartbeat.take() {
                    task.abort();
                }
                heartbeat = Some(spawn_heartbeat(
                    gate.clone(),
                    out_tx.clone(),
                    generation,
                    interval,
                    missed.clone(),
                    acked.clone(),
                ));
                if resume {
                    send_resume(&out_tx, &gate);
                } else {
                    send_identify(&out_tx, &gate);
                }
            }
            11 => {
                acked.store(true, Ordering::Release);
            }
            7 => {
                log::info!("gateway requested reconnect (op 7)");
                break;
            }
            9 => {
                let resumable = value.get("d").and_then(|flag| flag.as_bool()).unwrap_or(false);
                log::warn!("gateway invalid session resumable={resumable}");
                if !resumable {
                    let mut session = gate.session.lock();
                    session.id = None;
                    session.resume_url = None;
                }
                break;
            }
            0 => dispatch(&gate, &value),
            _ => {}
        }
    }

    if let Some(task) = heartbeat {
        task.abort();
    }
    *gate.outbound.lock() = None;
    drop(out_tx);
    let _ = writer.await;
    Ok(stop)
}

fn spawn_heartbeat(
    gate: Arc<Gate>,
    outbound: mpsc::UnboundedSender<Outbound>,
    generation: u64,
    interval: u64,
    missed: Arc<Notify>,
    acked: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let jitter = interval * (now_millis() % 1000) / 1000;
        tokio::time::sleep(Duration::from_millis(jitter.max(1))).await;
        let mut waiting = false;
        loop {
            if gate.generation.load(Ordering::Acquire) != generation {
                return;
            }
            if waiting && !acked.load(Ordering::Acquire) {
                missed.notify_waiters();
                return;
            }
            let seq = gate.session.lock().seq;
            let payload = json!({ "op": 1, "d": seq }).to_string();
            if outbound.send(Outbound::Text(payload)).is_err() {
                missed.notify_waiters();
                return;
            }
            acked.store(false, Ordering::Release);
            waiting = true;
            tokio::time::sleep(Duration::from_millis(interval.max(1))).await;
        }
    })
}

fn send_identify(outbound: &mpsc::UnboundedSender<Outbound>, gate: &Gate) {
    let Some(token) = gate.auth.token() else {
        return;
    };
    // User accounts must IDENTIFY with capabilities + browser properties.
    // Bot-style `intents` never receive READY for a normal Discord login token.
    let os = match std::env::consts::OS {
        "macos" => "Mac OS X",
        "linux" => "Linux",
        _ => "Windows",
    };
    let payload = json!({
        "op": 2,
        "d": {
            "token": token.as_str(),
            "capabilities": USER_CAPABILITIES,
            "properties": {
                "os": os,
                "browser": "Chrome",
                "device": "",
                "system_locale": "en-US",
                "browser_user_agent": DESKTOP_UA,
                "browser_version": "122.0.0.0",
                "os_version": "10",
                "referrer": "",
                "referring_domain": "",
                "referrer_current": "",
                "referring_domain_current": "",
                "release_channel": "stable",
                "client_build_number": CLIENT_BUILD,
                "client_event_source": serde_json::Value::Null
            },
            "presence": {
                "status": "online",
                "since": 0,
                "activities": [],
                "afk": false
            },
            "compress": false,
            "client_state": {
                "guild_versions": {}
            }
        }
    });
    log::info!("gateway IDENTIFY (user capabilities={USER_CAPABILITIES})");
    let _ = outbound.send(Outbound::Text(payload.to_string()));
}

fn send_resume(outbound: &mpsc::UnboundedSender<Outbound>, gate: &Gate) {
    let Some(token) = gate.auth.token() else {
        return;
    };
    let session = gate.session.lock();
    let Some(id) = session.id.clone() else {
        drop(session);
        send_identify(outbound, gate);
        return;
    };
    let payload = json!({
        "op": 6,
        "d": {
            "token": token.as_str(),
            "session_id": id,
            "seq": session.seq.unwrap_or(0),
        }
    });
    let _ = outbound.send(Outbound::Text(payload.to_string()));
}

fn dispatch(gate: &Arc<Gate>, value: &serde_json::Value) {
    let kind = value.get("t").and_then(|kind| kind.as_str()).unwrap_or("");
    let data = value.get("d").cloned().unwrap_or(serde_json::Value::Null);
    if kind == "READY" || kind == "RESUMED" {
        log::info!("gateway dispatch {kind}");
    }
    match kind {
        "READY" => on_ready(gate, &data),
        "READY_SUPPLEMENTAL" => on_ready_supplemental(gate, &data),
        "RESUMED" => emit_status(&gate.app, "ready", "Resumed"),
        "GUILD_CREATE" => on_guild(gate, &data),
        "MESSAGE_CREATE" | "MESSAGE_UPDATE" => {
            if let Some(message) = message_from_value(&data) {
                gate.cache.push_message(message.clone());
                let _ = gate.app.emit("suncord://message", &message);
            }
        }
        "MESSAGE_DELETE" => on_message_delete(gate, &data),
        "MESSAGE_DELETE_BULK" => on_message_delete_bulk(gate, &data),
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" => {
            if let Some((channel, users)) = channel_from_value_with_users(&data, None) {
                let dm = channel.guild_id.is_none();
                gate.cache.remember_users(users);
                gate.cache.upsert_channel(channel, dm);
                schedule_snapshot(gate);
            }
        }
        "CHANNEL_DELETE" => {
            if let Some(id) = data.get("id").and_then(|id| id.as_str()) {
                gate.cache.remove_channel(id);
                schedule_snapshot(gate);
            }
        }
        "VOICE_STATE_UPDATE" => {
            if let Some(signal) = parse_voice_state(&data) {
                let _ = gate.voice_tx.send(signal);
            } else {
                log::warn!("VOICE_STATE_UPDATE dropped (unreadable payload)");
            }
        }
        "VOICE_SERVER_UPDATE" => {
            if let Some(signal) = parse_voice_server(&data) {
                let _ = gate.voice_tx.send(signal);
            } else {
                log::warn!("VOICE_SERVER_UPDATE dropped (unreadable payload)");
            }
        }
        _ => {}
    }
}

fn on_message_delete(gate: &Arc<Gate>, data: &serde_json::Value) {
    let Some(channel_id) = data.get("channel_id").and_then(|id| id.as_str()) else {
        return;
    };
    let Some(message_id) = data.get("id").and_then(|id| id.as_str()) else {
        return;
    };
    let event = MessageDeleteEvent {
        channel_id: channel_id.to_string(),
        message_ids: vec![message_id.to_string()],
    };
    gate.cache.remove_messages(channel_id, &event.message_ids);
    let _ = gate.app.emit("suncord://message-delete", event);
}

fn on_message_delete_bulk(gate: &Arc<Gate>, data: &serde_json::Value) {
    let Some(channel_id) = data.get("channel_id").and_then(|id| id.as_str()) else {
        return;
    };
    let message_ids = data
        .get("ids")
        .and_then(|ids| ids.as_array())
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    if message_ids.is_empty() {
        return;
    }
    gate.cache.remove_messages(channel_id, &message_ids);
    let _ = gate.app.emit(
        "suncord://message-delete",
        MessageDeleteEvent {
            channel_id: channel_id.to_string(),
            message_ids,
        },
    );
}

fn on_ready(gate: &Arc<Gate>, data: &serde_json::Value) {
    #[derive(Deserialize)]
    struct Ready {
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        resume_gateway_url: Option<String>,
        #[serde(default)]
        user: Option<serde_json::Value>,
        #[serde(default)]
        guilds: Vec<serde_json::Value>,
        #[serde(default)]
        private_channels: Vec<serde_json::Value>,
        #[serde(default)]
        user_settings: Option<serde_json::Value>,
    }
    let Ok(ready) = serde_json::from_value::<Ready>(data.clone()) else {
        return;
    };
    {
        let mut session = gate.session.lock();
        session.id = ready.session_id;
        session.resume_url = ready.resume_gateway_url;
    }
    if let Some(user) = ready.user.as_ref() {
        gate.cache.set_user(user_from_value(user));
    }
    let mut guilds = Vec::new();
    let mut channels = Vec::new();
    for guild in ready.guilds {
        let Some(parsed) = guild_from_value(&guild) else {
            continue;
        };
        channels.extend(parsed.1);
        guilds.push(parsed.0);
    }
    guilds = order_guilds(guilds, ready.user_settings.as_ref());
    let mut users = Vec::new();
    let dms = ready
        .private_channels
        .iter()
        .filter_map(|channel| {
            let (dto, recipients) = channel_from_value_with_users(channel, None)?;
            users.extend(recipients);
            Some(dto)
        })
        .collect();
    let folders = parse_guild_folders(
        ready.user_settings.as_ref(),
        data.get("user_guild_settings"),
        data.get("guild_folders"),
        &guilds,
    );
    gate.cache.remember_users(users);
    gate.cache
        .replace_structure(guilds, channels, dms, folders);
    emit_status(&gate.app, "ready", "Live");
    schedule_snapshot(gate);
    // READY guilds are often `{id, unavailable:true}` stubs — hydrate names/icons via REST.
    hydrate_guild_meta(gate);
    // Folder layout may be missing until settings hydrate (or proto-era READY).
    hydrate_guild_folders(gate);
    if let Some(channel_id) = gate
        .cache
        .snapshot(false)
        .active_channel_id
        .filter(|id| !id.is_empty())
    {
        let rest = gate.rest.clone();
        let cache = gate.cache.clone();
        let auth = gate.auth.clone();
        let app = gate.app.clone();
        tauri::async_runtime::spawn(async move {
            let Some(token) = auth.token() else {
                return;
            };
            if let Ok(messages) = rest.messages(&token, &channel_id).await {
                cache.replace_messages(&channel_id, messages.clone());
                let _ = app.emit(
                    "suncord://history",
                    HistoryEvent {
                        channel_id,
                        messages,
                    },
                );
            }
        });
    }
}

fn on_guild(gate: &Arc<Gate>, data: &serde_json::Value) {
    let Some((guild, channels)) = guild_from_value(data) else {
        return;
    };
    // Skip pure unavailable stubs; wait for the real GUILD_CREATE / REST hydrate.
    if guild.name.is_empty() && guild.icon.is_none() && channels.is_empty() {
        return;
    }
    gate.cache.upsert_guild(guild, channels);
    schedule_snapshot(gate);
}

fn hydrate_guild_meta(gate: &Arc<Gate>) {
    let rest = gate.rest.clone();
    let cache = gate.cache.clone();
    let auth = gate.auth.clone();
    let app = gate.app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(token) = auth.token() else {
            return;
        };
        match rest.user_guilds(&token).await {
            Ok(guilds) if !guilds.is_empty() => {
                cache.merge_guild_meta(guilds);
                let _ = app.emit("suncord://snapshot", cache.snapshot(false));
            }
            Ok(_) => {}
            Err(err) => log::warn!("guild hydrate: {err}"),
        }
    });
}

fn hydrate_guild_folders(gate: &Arc<Gate>) {
    let rest = gate.rest.clone();
    let cache = gate.cache.clone();
    let auth = gate.auth.clone();
    let app = gate.app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(token) = auth.token() else {
            return;
        };
        let Ok(settings) = rest.user_settings(&token).await else {
            return;
        };
        let guilds = cache.snapshot(false).guilds;
        let folders = parse_guild_folders(Some(&settings), None, settings.get("guild_folders"), &guilds);
        if folders.iter().any(|folder| !folder.id.starts_with("loose-")) {
            cache.set_folders(folders);
            let _ = app.emit("suncord://snapshot", cache.snapshot(false));
        }
    });
}

fn schedule_snapshot(gate: &Arc<Gate>) {
    if gate.snapshot_pending.swap(true, Ordering::AcqRel) {
        return;
    }
    let gate = Arc::clone(gate);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        gate.snapshot_pending.store(false, Ordering::Release);
        let _ = gate.app.emit("suncord://snapshot", gate.cache.snapshot(false));
    });
}

#[derive(Deserialize)]
struct WireGuild {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    unavailable: bool,
    #[serde(default)]
    channels: Vec<serde_json::Value>,
}

fn guild_from_value(value: &serde_json::Value) -> Option<(GuildDto, Vec<ChannelDto>)> {
    let wire: WireGuild = serde_json::from_value(value.clone()).ok()?;
    if wire.unavailable && wire.name.is_none() {
        return Some((
            GuildDto {
                id: wire.id,
                name: String::new(),
                icon: None,
            },
            Vec::new(),
        ));
    }
    let guild_id = wire.id.clone();
    let channels = wire
        .channels
        .iter()
        .filter_map(|channel| channel_from_value(channel, Some(guild_id.clone())))
        .collect();
    Some((
        GuildDto {
            icon: guild_icon_url(&wire.id, &wire.icon),
            name: wire.name.unwrap_or_default(),
            id: wire.id,
        },
        channels,
    ))
}

#[derive(Deserialize)]
struct WireChannel {
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
    position: Option<i32>,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    last_message_id: Option<String>,
    #[serde(default)]
    recipients: Vec<WireRecipient>,
}

#[derive(Deserialize)]
struct WireRecipient {
    #[serde(default, deserialize_with = "deserialize_snowflake")]
    id: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    global_name: Option<String>,
    #[serde(default)]
    avatar: Option<String>,
}

fn channel_from_value(value: &serde_json::Value, guild_hint: Option<String>) -> Option<ChannelDto> {
    channel_from_value_with_users(value, guild_hint).map(|(channel, _)| channel)
}

fn channel_from_value_with_users(
    value: &serde_json::Value,
    guild_hint: Option<String>,
) -> Option<(ChannelDto, Vec<UserDto>)> {
    let wire: WireChannel = serde_json::from_value(value.clone()).ok()?;
    let guild_id = wire.guild_id.or(guild_hint);
    let users = wire
        .recipients
        .iter()
        .filter(|person| !person.id.is_empty())
        .map(|person| UserDto {
            id: person.id.clone(),
            name: display_name(&person.global_name, &person.username),
            avatar: avatar_url(&person.id, &person.avatar),
        })
        .collect::<Vec<_>>();

    // 1 = DM, 3 = group DM. Prefer recipient display names/avatars over raw group labels.
    let (name, icon) = match wire.kind {
        1 => {
            let person = wire.recipients.first();
            let name = person
                .map(|person| display_name(&person.global_name, &person.username))
                .filter(|name| !name.is_empty())
                .or_else(|| wire.name.filter(|name| !name.is_empty()))
                .unwrap_or_else(|| "Direct message".into());
            let icon = person.and_then(|person| avatar_url(&person.id, &person.avatar));
            (name, icon)
        }
        3 => {
            let name = wire
                .name
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| {
                    if users.is_empty() {
                        "Group chat".into()
                    } else {
                        users
                            .iter()
                            .map(|person| person.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                });
            let icon = channel_icon_url(&wire.id, &wire.icon).or_else(|| {
                users
                    .first()
                    .and_then(|person| person.avatar.clone())
            });
            (name, icon)
        }
        _ => {
            let name = wire
                .name
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "channel".into());
            (name, None)
        }
    };

    let recipient_ids = users.iter().map(|person| person.id.clone()).collect();
    Some((
        ChannelDto {
            id: wire.id,
            guild_id,
            name,
            kind: wire.kind,
            parent_id: wire.parent_id,
            position: wire.position.unwrap_or(0),
            icon,
            last_message_id: wire.last_message_id,
            recipient_ids,
        },
        users,
    ))
}

fn on_ready_supplemental(gate: &Arc<Gate>, data: &serde_json::Value) {
    let folders = parse_guild_folders(
        data.get("user_settings"),
        data.get("user_guild_settings"),
        data.get("guild_folders"),
        &gate.cache.snapshot(false).guilds,
    );
    if !folders.is_empty() {
        gate.cache.set_folders(folders);
        schedule_snapshot(gate);
    }
}

fn order_guilds(guilds: Vec<GuildDto>, user_settings: Option<&serde_json::Value>) -> Vec<GuildDto> {
    let positions = guild_positions(user_settings, None, None);
    if positions.is_empty() {
        return guilds;
    }
    let mut pending = guilds
        .into_iter()
        .map(|guild| (guild.id.clone(), guild))
        .collect::<std::collections::HashMap<_, _>>();
    let mut ordered = Vec::with_capacity(pending.len());
    for id in positions {
        if let Some(guild) = pending.remove(&id) {
            ordered.push(guild);
        }
    }
    ordered.extend(pending.into_values());
    ordered
}

fn guild_positions(
    user_settings: Option<&serde_json::Value>,
    user_guild_settings: Option<&serde_json::Value>,
    root_folders: Option<&serde_json::Value>,
) -> Vec<String> {
    parse_guild_folders(user_settings, user_guild_settings, root_folders, &[])
        .into_iter()
        .flat_map(|folder| folder.guild_ids)
        .collect()
}

fn parse_guild_folders(
    user_settings: Option<&serde_json::Value>,
    user_guild_settings: Option<&serde_json::Value>,
    root_folders: Option<&serde_json::Value>,
    guilds: &[crate::models::GuildDto],
) -> Vec<crate::models::GuildFolderDto> {
    let mut raw_folders = Vec::new();
    for source in [
        user_settings.and_then(|value| value.get("guild_folders")),
        user_guild_settings.and_then(|value| value.get("guild_folders")),
        root_folders,
    ] {
        if let Some(folders) = source.and_then(|value| value.as_array()) {
            raw_folders = folders.clone();
            break;
        }
    }

    let mut folders = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if !raw_folders.is_empty() {
        for (index, folder) in raw_folders.iter().enumerate() {
            let guild_ids = folder
                .get("guild_ids")
                .and_then(|value| value.as_array())
                .map(|rows| {
                    rows.iter()
                        .filter_map(snowflake_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if guild_ids.is_empty() {
                continue;
            }
            for id in &guild_ids {
                seen.insert(id.clone());
            }
            let id = folder
                .get("id")
                .and_then(|value| {
                    value
                        .as_i64()
                        .map(|number| number.to_string())
                        .or_else(|| value.as_str().map(str::to_string))
                })
                .unwrap_or_else(|| format!("folder-{index}"));
            let name = folder
                .get("name")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .filter(|name| !name.is_empty());
            // Discord uses null id/name for unguilded top-level entries (single guilds).
            if name.is_none() && folder.get("id").map(|value| value.is_null()).unwrap_or(true) && guild_ids.len() == 1
            {
                folders.push(crate::models::GuildFolderDto {
                    id: format!("loose-{}", guild_ids[0]),
                    name: None,
                    color: None,
                    guild_ids,
                });
                continue;
            }
            folders.push(crate::models::GuildFolderDto {
                id,
                name,
                color: folder.get("color").and_then(|value| value.as_u64()).map(|value| value as u32),
                guild_ids,
            });
        }
    }

    if folders.is_empty() {
        if let Some(settings) = user_settings {
            if let Some(positions) = settings
                .get("guild_positions")
                .and_then(|value| value.as_array())
            {
                for guild_id in positions {
                    if let Some(id) = guild_id.as_str() {
                        seen.insert(id.to_string());
                        folders.push(crate::models::GuildFolderDto {
                            id: format!("loose-{id}"),
                            name: None,
                            color: None,
                            guild_ids: vec![id.to_string()],
                        });
                    }
                }
            }
        }
    }

    for guild in guilds {
        if seen.insert(guild.id.clone()) {
            folders.push(crate::models::GuildFolderDto {
                id: format!("loose-{}", guild.id),
                name: None,
                color: None,
                guild_ids: vec![guild.id.clone()],
            });
        }
    }
    folders
}

fn parse_voice_state(data: &serde_json::Value) -> Option<VoiceSignal> {
    let user_id = data.get("user_id").and_then(snowflake_string)?;
    let session_id = data
        .get("session_id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    Some(VoiceSignal::State {
        guild_id: data.get("guild_id").and_then(snowflake_string),
        channel_id: data.get("channel_id").and_then(snowflake_string),
        user_id,
        session_id,
        self_mute: json_bool(data.get("self_mute")),
        self_deaf: json_bool(data.get("self_deaf")),
        mute: json_bool(data.get("mute")),
        deaf: json_bool(data.get("deaf")),
    })
}

fn parse_voice_server(data: &serde_json::Value) -> Option<VoiceSignal> {
    let endpoint = data
        .get("endpoint")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let token = data
        .get("token")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    if endpoint.is_empty() && token.is_empty() {
        return None;
    }
    Some(VoiceSignal::Server {
        guild_id: data.get("guild_id").and_then(snowflake_string),
        endpoint,
        token,
    })
}

fn json_bool(value: Option<&serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::Number(number)) => number.as_u64() == Some(1),
        _ => false,
    }
}

fn deserialize_snowflake<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    snowflake_string(&value).ok_or_else(|| serde::de::Error::custom("expected snowflake"))
}

fn snowflake_string(value: &serde_json::Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
        .or_else(|| value.as_i64().map(|n| n.to_string()))
        .filter(|id| !id.is_empty() && id != "null")
}

fn gateway_url(resume: Option<&str>) -> String {
    let base = resume.unwrap_or("wss://gateway.discord.gg");
    // User clients still speak gateway v9 with zlib-stream; v10 bot framing hangs IDENTIFY.
    format!(
        "{}/?v=9&encoding=json&compress=zlib-stream",
        base.trim_end_matches('/')
    )
}

fn emit_status(app: &AppHandle, phase: &str, detail: &str) {
    let _ = app.emit(
        "suncord://status",
        StatusEvent {
            phase: phase.into(),
            detail: detail.into(),
        },
    );
}

fn emit_error(app: &AppHandle, message: &str) {
    let _ = app.emit(
        "suncord://error",
        Note {
            message: message.into(),
        },
    );
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis() as u64
}

