use crate::asr::AsrHandle;
use crate::audio::{AudioIo, AudioPump, PcmFrame, PIPELINE_CHANNELS, PIPELINE_RATE};
use crate::cache::CacheStore;
use crate::crypto::{self, TransportMode};
use crate::dave::DaveSession;
use crate::gateway::GatewayHandle;
use crate::opus_codec::Encoder;
use crate::models::{encoder_name, Settings, VoiceMemberDto, VoiceSignal, VoiceStatus};
use std::collections::HashMap;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};

enum VCmd {
    Join {
        guild_id: Option<String>,
        channel_id: String,
    },
    Leave,
    Mute(bool),
    Deaf(bool),
    Apply(Settings),
    Signal(VoiceSignal),
}

#[derive(Clone)]
pub struct VoiceHandle {
    tx: mpsc::UnboundedSender<VCmd>,
}

impl VoiceHandle {
    pub fn start(
        app: AppHandle,
        gateway: GatewayHandle,
        cache: Arc<CacheStore>,
        mut signals: mpsc::UnboundedReceiver<VoiceSignal>,
        mut frames: mpsc::Receiver<PcmFrame>,
        audio: AudioPump,
        asr: AsrHandle,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = tx.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(signal) = signals.recv().await {
                let _ = bridge.send(VCmd::Signal(signal));
            }
        });
        tauri::async_runtime::spawn(async move {
            run(app, gateway, cache, audio, asr, &mut rx, &mut frames).await;
        });
        Self { tx }
    }

    pub fn join(&self, guild_id: Option<String>, channel_id: String) {
        let _ = self.tx.send(VCmd::Join {
            guild_id,
            channel_id,
        });
    }

    pub fn leave(&self) {
        let _ = self.tx.send(VCmd::Leave);
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.tx.send(VCmd::Mute(muted));
    }

    pub fn set_deafened(&self, deafened: bool) {
        let _ = self.tx.send(VCmd::Deaf(deafened));
    }

    pub fn apply(&self, settings: Settings) {
        let _ = self.tx.send(VCmd::Apply(settings));
    }
}

struct Pending {
    guild_id: Option<String>,
    channel_id: String,
    session_id: Option<String>,
    endpoint: Option<String>,
    token: Option<String>,
}

struct RtpOut {
    pcm: mpsc::Sender<Vec<f32>>,
    stop: Option<oneshot::Sender<()>>,
}

async fn run(
    app: AppHandle,
    gateway: GatewayHandle,
    cache: Arc<CacheStore>,
    audio: AudioPump,
    asr: AsrHandle,
    commands: &mut mpsc::UnboundedReceiver<VCmd>,
    frames: &mut mpsc::Receiver<PcmFrame>,
) {
    let mut settings = Settings::default();
    let mut status = VoiceStatus::default();
    let mut pending: Option<Pending> = None;
    let mut rtp: Option<RtpOut> = None;
    let mut members: HashMap<String, VoiceMemberDto> = HashMap::new();
    let mut asr_on = false;
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { return; };
                match command {
                    VCmd::Apply(next) => {
                        settings = next;
                        asr_on = settings.asr_enabled;
                        asr.configure(settings.asr_enabled, settings.asr_model_path.clone());
                        status.buffer_frames = settings.buffer_frames;
                        status.hardware_accel = settings.hardware_accel;
                        status.adaptive_throttle = settings.adaptive_throttle;
                        sync_audio(&audio, &settings, status.phase != "idle" || asr_on);
                        emit_status(&app, &status);
                    }
                    VCmd::Mute(muted) => {
                        status.muted = muted;
                        let _ = gateway.voice_state(pending.as_ref().and_then(|item| item.guild_id.clone()), pending.as_ref().map(|item| item.channel_id.clone()), status.muted, status.deafened).await;
                        emit_status(&app, &status);
                    }
                    VCmd::Deaf(deafened) => {
                        status.deafened = deafened;
                        let _ = gateway.voice_state(pending.as_ref().and_then(|item| item.guild_id.clone()), pending.as_ref().map(|item| item.channel_id.clone()), status.muted, status.deafened).await;
                        emit_status(&app, &status);
                    }
                    VCmd::Leave => {
                        let _ = gateway.voice_state(pending.as_ref().and_then(|item| item.guild_id.clone()), None, status.muted, status.deafened).await;
                        stop_rtp(&mut rtp);
                        pending = None;
                        members.clear();
                        status = VoiceStatus {
                            buffer_frames: settings.buffer_frames,
                            hardware_accel: settings.hardware_accel,
                            adaptive_throttle: settings.adaptive_throttle,
                            muted: status.muted,
                            deafened: status.deafened,
                            ..VoiceStatus::default()
                        };
                        sync_audio(&audio, &settings, asr_on);
                        emit_status(&app, &status);
                    }
                    VCmd::Join { guild_id, channel_id } => {
                        stop_rtp(&mut rtp);
                        members.clear();
                        pending = Some(Pending { guild_id: guild_id.clone(), channel_id: channel_id.clone(), session_id: None, endpoint: None, token: None });
                        status.phase = "joining".into();
                        status.channel_id = Some(channel_id.clone());
                        status.channel_name = cache.channel_name(&channel_id);
                        status.detail = "Connecting…".into();
                        status.encoder = encoder_name();
                        status.members.clear();
                        sync_audio(&audio, &settings, true);
                        emit_status(&app, &status);
                        // Opcode 4 must never block this loop — dispatch and move on.
                        let gateway_join = gateway.clone();
                        let mute = status.muted;
                        let deaf = status.deafened;
                        let app_join = app.clone();
                        let mut status_join = status.clone();
                        tauri::async_runtime::spawn(async move {
                            match gateway_join
                                .voice_state(guild_id, Some(channel_id), mute, deaf)
                                .await
                            {
                                Ok(()) => {
                                    status_join.detail = "Waiting for voice server…".into();
                                    emit_status(&app_join, &status_join);
                                }
                                Err(err) => {
                                    status_join.phase = "error".into();
                                    status_join.detail = err;
                                    emit_status(&app_join, &status_join);
                                }
                            }
                        });
                        status.detail = "Waiting for voice server…".into();
                        emit_status(&app, &status);
                    }
                    VCmd::Signal(signal) => {
                        track_member(&cache, &mut members, &pending, &signal);
                        if let Some(pending) = pending.as_mut() {
                            apply_signal(pending, signal, cache.user_id().as_deref());
                            status.members = members.values().cloned().collect();
                            if ready(pending) && rtp.is_none() {
                                let user_id = cache.user_id().unwrap_or_default();
                                match spawn_link(app.clone(), pending, user_id, &mut status).await {
                                    Ok(link) => {
                                        rtp = Some(link);
                                        status.detail = format!(
                                            "Connected{}",
                                            status
                                                .channel_name
                                                .as_ref()
                                                .map(|name| format!(" to {name}"))
                                                .unwrap_or_default()
                                        );
                                        emit_status(&app, &status);
                                    }
                                    Err(err) => {
                                        status.phase = "error".into();
                                        status.detail = err;
                                        emit_status(&app, &status);
                                    }
                                }
                            } else {
                                emit_status(&app, &status);
                            }
                        }
                    }
                }
            }
            frame = frames.recv() => {
                let Some(frame) = frame else { return; };
                status.level = frame.rms;
                if let Some(link) = rtp.as_ref() {
                    let mut samples = frame.samples;
                    if status.muted {
                        samples.fill(0.0);
                    }
                    let _ = link.pcm.try_send(samples);
                }
            }
        }
    }
}

fn track_member(
    cache: &CacheStore,
    members: &mut HashMap<String, VoiceMemberDto>,
    pending: &Option<Pending>,
    signal: &VoiceSignal,
) {
    let VoiceSignal::State {
        channel_id,
        user_id,
        self_mute,
        self_deaf,
        mute,
        deaf,
        ..
    } = signal
    else {
        return;
    };
    let Some(pending) = pending else {
        return;
    };
    if channel_id.as_deref() != Some(pending.channel_id.as_str()) {
        members.remove(user_id);
        return;
    }
    let profile = cache.user_lookup(user_id);
    members.insert(
        user_id.clone(),
        VoiceMemberDto {
            id: user_id.clone(),
            name: profile
                .as_ref()
                .map(|user| user.name.clone())
                .unwrap_or_else(|| format!("user {}", &user_id[user_id.len().saturating_sub(4)..])),
            avatar: profile.and_then(|user| user.avatar),
            muted: *self_mute || *mute,
            deafened: *self_deaf || *deaf,
        },
    );
}

fn apply_signal(pending: &mut Pending, signal: VoiceSignal, self_id: Option<&str>) {
    match signal {
        VoiceSignal::State {
            guild_id,
            channel_id,
            user_id,
            session_id,
            ..
        } => {
            if self_id.is_some() && self_id != Some(user_id.as_str()) {
                return;
            }
            if channel_id.as_deref() != Some(pending.channel_id.as_str()) {
                return;
            }
            if let Some(guild_id) = guild_id {
                if pending.guild_id.as_ref() != Some(&guild_id) && pending.guild_id.is_some() {
                    return;
                }
            }
            if !session_id.is_empty() {
                pending.session_id = Some(session_id);
            }
        }
        VoiceSignal::Server {
            guild_id,
            endpoint,
            token,
        } => {
            if let Some(guild_id) = guild_id.as_ref() {
                if pending.guild_id.as_ref() != Some(guild_id) && pending.guild_id.is_some() {
                    return;
                }
            }
            if !endpoint.is_empty() {
                pending.endpoint = Some(endpoint);
            }
            if !token.is_empty() {
                pending.token = Some(token);
            }
        }
    }
}

fn ready(pending: &Pending) -> bool {
    pending.session_id.is_some() && pending.endpoint.is_some() && pending.token.is_some()
}

async fn spawn_link(
    app: AppHandle,
    pending: &Pending,
    user_id: String,
    status: &mut VoiceStatus,
) -> Result<RtpOut, String> {
    let (pcm_tx, pcm_rx) = mpsc::channel(4);
    let (stop_tx, stop_rx) = oneshot::channel();
    let args = ConnectArgs {
        app,
        server_id: pending
            .guild_id
            .clone()
            .unwrap_or_else(|| pending.channel_id.clone()),
        user_id,
        session_id: pending.session_id.clone().unwrap_or_default(),
        endpoint: pending.endpoint.clone().unwrap_or_default(),
        token: pending.token.clone().unwrap_or_default(),
    };
    status.phase = "live".into();
    status.detail = "Opening the voice socket".into();
    status.sample_rate = PIPELINE_RATE;
    status.channels = PIPELINE_CHANNELS;
    tauri::async_runtime::spawn(async move {
        if let Err(err) = connect_voice(args, pcm_rx, stop_rx).await {
            log::warn!("voice: {err}");
        }
    });
    Ok(RtpOut {
        pcm: pcm_tx,
        stop: Some(stop_tx),
    })
}

fn stop_rtp(rtp: &mut Option<RtpOut>) {
    if let Some(mut link) = rtp.take() {
        if let Some(stop) = link.stop.take() {
            let _ = stop.send(());
        }
    }
}

struct ConnectArgs {
    app: AppHandle,
    server_id: String,
    user_id: String,
    session_id: String,
    endpoint: String,
    token: String,
}

async fn connect_voice(
    args: ConnectArgs,
    mut pcm: mpsc::Receiver<Vec<f32>>,
    mut stop: oneshot::Receiver<()>,
) -> Result<(), String> {
    let mut dave = DaveSession::new();
    let url = voice_url(&args.endpoint);
    let (socket, _) = connect_async(&url).await.map_err(|err| err.to_string())?;
    let (mut write, mut read) = socket.split();
    let (text_tx, mut text_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(text) = text_rx.recv().await {
            if write.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    let hello = next_op(&mut read, &mut stop, &mut dave, &text_tx, 8).await?;
    let interval = hello
        .pointer("/d/heartbeat_interval")
        .and_then(|value| value.as_u64())
        .unwrap_or(41_250);
    let heartbeat_tx = text_tx.clone();
    let heartbeat = tokio::spawn(async move {
        let mut ticks = 0u64;
        loop {
            tokio::time::sleep(Duration::from_millis(interval.max(1))).await;
            ticks += 1;
            let payload = json!({ "op": 3, "d": { "t": ticks, "seq_ack": -1 } }).to_string();
            if heartbeat_tx.send(payload).is_err() {
                break;
            }
        }
    });
    let identify = json!({
        "op": 0,
        "d": {
            "server_id": args.server_id,
            "user_id": args.user_id,
            "session_id": args.session_id,
            "token": args.token,
            "max_dave_protocol_version": DaveSession::VERSION
        }
    });
    text_tx
        .send(identify.to_string())
        .map_err(|_| "voice socket closed".to_string())?;
    let ready = next_op(&mut read, &mut stop, &mut dave, &text_tx, 2).await?;
    let ready: VoiceReady = serde_json::from_value(ready["d"].clone()).map_err(|err| err.to_string())?;
    let mode = TransportMode::choose(&ready.modes).ok_or("The voice server offered no supported encryption mode.")?;
    let udp = UdpSocket::bind("0.0.0.0:0").await.map_err(|err| err.to_string())?;
    udp.connect((ready.ip.as_str(), ready.port))
        .await
        .map_err(|err| err.to_string())?;
    udp.send(&crypto::discovery_packet(ready.ssrc))
        .await
        .map_err(|err| err.to_string())?;
    let mut discovery = [0u8; 74];
    let read_discovery = tokio::time::timeout(Duration::from_secs(5), udp.recv(&mut discovery));
    tokio::select! {
        _ = &mut stop => return Err("left voice".into()),
        result = read_discovery => {
            result.map_err(|_| "IP discovery timed out".to_string())?.map_err(|err| err.to_string())?;
        }
    }
    let (address, port) = crypto::parse_discovery(&discovery).ok_or("IP discovery was unreadable.")?;
    let select = json!({
        "op": 1,
        "d": {
            "protocol": "udp",
            "data": { "address": address, "port": port, "mode": mode.discord_name() }
        }
    });
    text_tx
        .send(select.to_string())
        .map_err(|_| "voice socket closed".to_string())?;
    let described = next_op(&mut read, &mut stop, &mut dave, &text_tx, 4).await?;
    let described: SessionDescription =
        serde_json::from_value(described["d"].clone()).map_err(|err| err.to_string())?;
    if described.secret_key.len() != 32 {
        return Err("The voice key was not 32 bytes.".into());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&described.secret_key);
    dave.set_transport_key(&key);
    dave.note_version(described.dave_protocol_version);
    publish(
        &args.app,
        VoiceStatus {
            phase: "live".into(),
            detail: String::new(),
            transport: mode.label().into(),
            dave: dave.label().into(),
            encoder: encoder_name(),
            sample_rate: PIPELINE_RATE,
            channels: PIPELINE_CHANNELS,
            ..VoiceStatus::default()
        },
    );
    let mut encoder = Encoder::new()?;
    let mut sequence = 0u16;
    let mut timestamp = 0u32;
    let mut nonce = 0u32;
    let mut inbound = [0u8; 2048];
    loop {
        tokio::select! {
            _ = &mut stop => break,
            received = udp.recv(&mut inbound) => {
                if let Ok(size) = received {
                    let _ = crypto::open(mode, &key, &inbound[..size]);
                }
            }
            frame = pcm.recv() => {
                let Some(frame) = frame else { break; };
                let Ok(opus) = encoder.encode(&frame) else { continue; };
                let payload = dave.seal_media(&opus);
                let header = crypto::rtp_header(sequence, timestamp, ready.ssrc);
                if let Ok(packet) = crypto::seal(mode, &key, &header, &payload, nonce) {
                    let _ = udp.send(&packet).await;
                }
                sequence = sequence.wrapping_add(1);
                timestamp = timestamp.wrapping_add(960);
                nonce = nonce.wrapping_add(1);
            }
            message = read.next() => {
                match message {
                    Some(Ok(incoming)) => {
                        for reply in dave_replies(&mut dave, incoming) {
                            let _ = text_tx.send(reply);
                        }
                    }
                    _ => break,
                }
            }
        }
    }
    heartbeat.abort();
    drop(text_tx);
    let _ = writer.await;
    Ok(())
}

fn dave_replies(dave: &mut DaveSession, message: Message) -> Vec<String> {
    match message {
        Message::Text(text) => serde_json::from_str(&text)
            .ok()
            .map(|value| dave.on_json(&value))
            .unwrap_or_default(),
        Message::Binary(bytes) => dave.on_binary(&bytes),
        _ => Vec::new(),
    }
}

async fn next_op(
    read: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    stop: &mut oneshot::Receiver<()>,
    dave: &mut DaveSession,
    outbound: &mpsc::UnboundedSender<String>,
    op: u64,
) -> Result<serde_json::Value, String> {
    loop {
        let message = tokio::select! {
            _ = &mut *stop => return Err("left voice".into()),
            message = read.next() => message.ok_or("voice socket closed")?.map_err(|err| err.to_string())?,
        };
        for reply in dave_replies(dave, message.clone()) {
            let _ = outbound.send(reply);
        }
        let Message::Text(text) = message else { continue; };
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
        if value.get("op").and_then(|value| value.as_u64()) == Some(op) {
            return Ok(value);
        }
    }
}

fn voice_url(endpoint: &str) -> String {
    let host = endpoint
        .trim_start_matches("wss://")
        .split(':')
        .next()
        .unwrap_or(endpoint);
    format!("wss://{host}/?v=8")
}

fn sync_audio(audio: &AudioPump, settings: &Settings, running: bool) {
    audio.configure(AudioIo {
        buffer_frames: settings.buffer_frames,
        hardware_accel: settings.hardware_accel,
        input_name: settings.input_device.clone(),
        output_name: settings.output_device.clone(),
        running,
    });
}

fn emit_status(app: &AppHandle, status: &VoiceStatus) {
    let _ = app.emit("suncord://voice", status);
}

fn publish(app: &AppHandle, status: VoiceStatus) {
    emit_status(app, &status);
}

#[derive(Deserialize)]
struct VoiceReady {
    ssrc: u32,
    ip: String,
    port: u16,
    modes: Vec<String>,
}

#[derive(Deserialize)]
struct SessionDescription {
    #[serde(default)]
    secret_key: Vec<u8>,
    #[serde(default)]
    dave_protocol_version: u8,
}

