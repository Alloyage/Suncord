use crate::asr::AsrHandle;
use crate::audio::{AudioIo, AudioPump, PlaybackPump, PcmFrame, PIPELINE_CHANNELS, PIPELINE_RATE};
use crate::auth::AuthStore;
use crate::cache::CacheStore;
use crate::crypto::{self, TransportMode};
use crate::dave::DaveSession;
use crate::gateway::GatewayHandle;
use crate::opus_codec::{Decoder, Encoder, FRAME_SAMPLES, SILENCE_OPUS};
use crate::models::{encoder_name, Settings, VoiceMemberDto, VoiceSignal, VoiceStatus};
use crate::rest::DiscordRest;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Voice gateway close 4006 — session token is dead; re-fetch Opcode 4 credentials.
    RefreshSession,
    /// UDP/voice websocket ended — allow a fresh connect on the next credentials tick.
    LinkDead,
    /// Re-send Opcode 4 if voice server events never arrived.
    RetryVoiceServer { ticket: u64 },
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
        auth: AuthStore,
        rest: DiscordRest,
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
        let cmd_tx = tx.clone();
        tauri::async_runtime::spawn(async move {
            run(
                app, gateway, cache, auth, rest, audio, asr, cmd_tx, &mut rx, &mut frames,
            )
            .await;
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
    self_user_id: Option<String>,
    session_id: Option<String>,
    endpoint: Option<String>,
    token: Option<String>,
    wait_ticket: u64,
    /// Bumps on every VOICE_SERVER_UPDATE token; session must match this ticket.
    voice_ticket: u64,
    session_ticket: u64,
}

struct RtpOut {
    pcm: mpsc::Sender<Vec<f32>>,
    stop: Option<oneshot::Sender<()>>,
}

async fn run(
    app: AppHandle,
    gateway: GatewayHandle,
    cache: Arc<CacheStore>,
    auth: AuthStore,
    rest: DiscordRest,
    audio: AudioPump,
    asr: AsrHandle,
    commands_tx: mpsc::UnboundedSender<VCmd>,
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
                        let self_user_id = cache.user_id();
                        seed_call_roster(&cache, &mut members, &channel_id, self_user_id.as_deref());
                        let wait_ticket = voice_wait_ticket();
                        pending = Some(Pending {
                            guild_id: guild_id.clone(),
                            channel_id: channel_id.clone(),
                            self_user_id: self_user_id.clone(),
                            session_id: None,
                            endpoint: None,
                            token: None,
                            wait_ticket,
                            voice_ticket: 0,
                            session_ticket: 0,
                        });
                        status.phase = "joining".into();
                        status.channel_id = Some(channel_id.clone());
                        status.channel_name = cache.channel_name(&channel_id);
                        status.detail = "Connecting…".into();
                        status.encoder = encoder_name();
                        status.members = members.values().cloned().collect();
                        sync_audio(&audio, &settings, true);
                        emit_status(&app, &status);
                        // Private calls need REST /call before Discord emits VOICE_SERVER_UPDATE.
                        if guild_id.is_none() {
                            if let Some(token) = auth.token() {
                                let rest = rest.clone();
                                let call_channel = channel_id.clone();
                                tauri::async_runtime::spawn(async move {
                                    if let Err(err) = rest.join_call(&token, &call_channel).await {
                                        log::warn!("voice REST call join: {err}");
                                    }
                                });
                            }
                        }
                        request_voice_server(
                            &gateway,
                            &app,
                            &commands_tx,
                            guild_id,
                            channel_id,
                            status.muted,
                            status.deafened,
                            wait_ticket,
                            &mut status,
                        );
                    }
                    VCmd::RetryVoiceServer { ticket } => {
                        let Some(pending_ref) = pending.as_ref() else { continue; };
                        // Never re-fire Opcode 4 once any credential arrived — that rotates the
                        // voice token and can wipe a valid session_id mid-handshake.
                        if pending_ref.wait_ticket != ticket
                            || ready(pending_ref)
                            || rtp.is_some()
                            || pending_ref.endpoint.is_some()
                            || pending_ref.token.is_some()
                            || pending_ref.session_id.is_some()
                        {
                            continue;
                        }
                        status.detail = "Still waiting — requesting voice server again…".into();
                        emit_status(&app, &status);
                        request_voice_server(
                            &gateway,
                            &app,
                            &commands_tx,
                            pending_ref.guild_id.clone(),
                            pending_ref.channel_id.clone(),
                            status.muted,
                            status.deafened,
                            ticket,
                            &mut status,
                        );
                    }
                    VCmd::LinkDead => {
                        stop_rtp(&mut rtp);
                        if let Some(pending_ref) = pending.as_ref() {
                            if ready(pending_ref) {
                                status.phase = "connecting".into();
                                status.detail = "Reconnecting voice transport…".into();
                                emit_status(&app, &status);
                                let user_id = cache.user_id().unwrap_or_default();
                                if let Ok(link) = spawn_link(
                                    app.clone(),
                                    pending_ref,
                                    user_id,
                                    status.clone(),
                                    commands_tx.clone(),
                                )
                                .await
                                {
                                    rtp = Some(link);
                                }
                            }
                        }
                    }
                    VCmd::RefreshSession => {
                        // Close code 4006: drop UDP, wipe credentials, re-issue Opcode 4.
                        stop_rtp(&mut rtp);
                        let Some(pending_ref) = pending.as_mut() else {
                            continue;
                        };
                        pending_ref.session_id = None;
                        pending_ref.endpoint = None;
                        pending_ref.token = None;
                        pending_ref.voice_ticket = 0;
                        pending_ref.session_ticket = 0;
                        pending_ref.wait_ticket = voice_wait_ticket();
                        let ticket = pending_ref.wait_ticket;
                        status.phase = "joining".into();
                        status.detail = "Refreshing voice session…".into();
                        emit_status(&app, &status);
                        request_voice_server(
                            &gateway,
                            &app,
                            &commands_tx,
                            pending_ref.guild_id.clone(),
                            pending_ref.channel_id.clone(),
                            status.muted,
                            status.deafened,
                            ticket,
                            &mut status,
                        );
                    }
                    VCmd::Signal(signal) => {
                        track_member(&cache, &mut members, &pending, &signal);
                        if let Some(pending) = pending.as_mut() {
                            if pending.self_user_id.is_none() {
                                pending.self_user_id = cache.user_id();
                            }
                            let self_id = pending
                                .self_user_id
                                .clone()
                                .or_else(|| cache.user_id());
                            let rotated = apply_signal(pending, signal, self_id.as_deref());
                            if rotated {
                                stop_rtp(&mut rtp);
                            }
                            status.members = members.values().cloned().collect();
                            maybe_start_voice_link(
                                &app,
                                pending,
                                &mut rtp,
                                &mut status,
                                self_id.unwrap_or_default(),
                                commands_tx.clone(),
                            )
                            .await;
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

fn voice_guild_matches(
    pending_guild: Option<&String>,
    pending_channel: &str,
    incoming: Option<&String>,
) -> bool {
    match (pending_guild, incoming) {
        (Some(a), Some(b)) => a == b || b == pending_channel,
        // Private calls: Opcode 4 uses guild_id null; VOICE_SERVER_UPDATE uses the channel id.
        (None, Some(b)) => b == pending_channel,
        (None, None) => true,
        (Some(_), None) => true,
    }
}

fn seed_call_roster(
    cache: &CacheStore,
    members: &mut HashMap<String, VoiceMemberDto>,
    channel_id: &str,
    self_id: Option<&str>,
) {
    if let Some(self_id) = self_id {
        let profile = cache.user_lookup(self_id);
        members.insert(
            self_id.to_string(),
            VoiceMemberDto {
                id: self_id.to_string(),
                name: profile
                    .as_ref()
                    .map(|user| user.name.clone())
                    .unwrap_or_else(|| "You".into()),
                avatar: profile.and_then(|user| user.avatar),
                muted: false,
                deafened: false,
            },
        );
    }
    for recipient_id in cache.channel_recipient_ids(channel_id) {
        if self_id == Some(recipient_id.as_str()) {
            continue;
        }
        let profile = cache.user_lookup(&recipient_id);
        members.insert(
            recipient_id.clone(),
            VoiceMemberDto {
                id: recipient_id.clone(),
                name: profile
                    .as_ref()
                    .map(|user| user.name.clone())
                    .unwrap_or_else(|| {
                        format!(
                            "user {}",
                            &recipient_id[recipient_id.len().saturating_sub(4)..]
                        )
                    }),
                avatar: profile.and_then(|user| user.avatar),
                muted: false,
                deafened: false,
            },
        );
    }
}

fn apply_signal(pending: &mut Pending, signal: VoiceSignal, self_id: Option<&str>) -> bool {
    match signal {
        VoiceSignal::State {
            guild_id,
            channel_id,
            user_id,
            session_id,
            ..
        } => {
            // Leaving / switching away from this channel — ignore for credentials.
            if channel_id.as_deref() != Some(pending.channel_id.as_str()) {
                return false;
            }
            if !voice_guild_matches(
                pending.guild_id.as_ref(),
                pending.channel_id.as_str(),
                guild_id.as_ref(),
            ) {
                return false;
            }
            // Never adopt another member's session_id — that produces voice close 4006.
            let is_self = match self_id {
                Some(id) => id == user_id.as_str(),
                // If we do not know ourselves yet, still accept a session for this channel;
                // spawn_link will fail closed if user_id is empty.
                None => true,
            };
            if !is_self {
                return false;
            }
            if !session_id.is_empty() {
                pending.session_id = Some(session_id);
                pending.session_ticket = pending.voice_ticket.max(1);
                if pending.voice_ticket == 0 {
                    // State arrived first — provisional ticket until VOICE_SERVER lands.
                    pending.session_ticket = 1;
                }
                log::info!(
                    "voice session bound ticket={} channel={}",
                    pending.session_ticket,
                    pending.channel_id
                );
            }
            false
        }
        VoiceSignal::Server {
            guild_id,
            endpoint,
            token,
        } => {
            let matches = voice_guild_matches(
                pending.guild_id.as_ref(),
                pending.channel_id.as_str(),
                guild_id.as_ref(),
            );
            // First credentials for this join always win — Discord DM servers use channel id
            // as guild_id and some payloads omit it entirely.
            let accept_first = pending.token.is_none() && pending.endpoint.is_none();
            if !matches && !accept_first {
                log::warn!(
                    "voice server ignored (guild mismatch pending={:?} incoming={:?} channel={})",
                    pending.guild_id,
                    guild_id,
                    pending.channel_id
                );
                return false;
            }
            let mut rotated = false;
            if !endpoint.is_empty() {
                pending.endpoint = Some(normalize_endpoint(&endpoint));
            }
            if !token.is_empty() {
                let had_token = pending.token.as_ref().is_some_and(|existing| existing != &token);
                if had_token {
                    pending.session_id = None;
                    pending.session_ticket = 0;
                    rotated = true;
                }
                pending.voice_ticket = pending.voice_ticket.wrapping_add(1).max(1);
                pending.token = Some(token);
                if pending.session_id.is_some() {
                    pending.session_ticket = pending.voice_ticket;
                }
                log::info!(
                    "voice server bound ticket={} endpoint={}",
                    pending.voice_ticket,
                    pending.endpoint.as_deref().unwrap_or("")
                );
            }
            rotated
        }
    }
}

fn normalize_endpoint(endpoint: &str) -> String {
    endpoint
        .trim()
        .trim_start_matches("wss://")
        .trim_start_matches("ws://")
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

static VOICE_WAIT: AtomicU64 = AtomicU64::new(1);

fn voice_wait_ticket() -> u64 {
    VOICE_WAIT.fetch_add(1, Ordering::Relaxed)
}

async fn maybe_start_voice_link(
    app: &AppHandle,
    pending: &Pending,
    rtp: &mut Option<RtpOut>,
    status: &mut VoiceStatus,
    user_id: String,
    cmd: mpsc::UnboundedSender<VCmd>,
) {
    if !ready(pending) {
        let session_ok = pending.session_id.as_ref().is_some_and(|id| !id.is_empty());
        let endpoint_ok = pending.endpoint.as_ref().is_some_and(|endpoint| !endpoint.is_empty());
        let token_ok = pending.token.as_ref().is_some_and(|token| !token.is_empty());
        status.detail = match (session_ok, endpoint_ok, token_ok) {
            (false, true, true) => "Got voice server — waiting for session…".into(),
            (true, false, _) | (true, _, false) => "Got session — waiting for voice server…".into(),
            (true, true, true) => "Pairing voice credentials…".into(),
            _ => status.detail.clone(),
        };
        emit_status(app, status);
        return;
    }
    if rtp.is_some() {
        emit_status(app, status);
        return;
    }
    status.phase = "connecting".into();
    status.detail = "Opening the voice socket…".into();
    emit_status(app, status);
    match spawn_link(app.clone(), pending, user_id, status.clone(), cmd).await {
        Ok(link) => {
            *rtp = Some(link);
            status.detail = "Negotiating UDP transport…".into();
            emit_status(app, status);
        }
        Err(err) => {
            status.phase = "error".into();
            status.detail = err;
            emit_status(app, status);
        }
    }
}

fn ready(pending: &Pending) -> bool {
    pending
        .session_id
        .as_ref()
        .is_some_and(|id| !id.is_empty())
        && pending
            .endpoint
            .as_ref()
            .is_some_and(|endpoint| !endpoint.is_empty())
        && pending.token.as_ref().is_some_and(|token| !token.is_empty())
}

fn request_voice_server(
    gateway: &GatewayHandle,
    app: &AppHandle,
    cmd_tx: &mpsc::UnboundedSender<VCmd>,
    guild_id: Option<String>,
    channel_id: String,
    mute: bool,
    deaf: bool,
    ticket: u64,
    status: &mut VoiceStatus,
) {
    // Opcode 4 must never block the voice select loop.
    let gateway_join = gateway.clone();
    let app_join = app.clone();
    let guild_label = guild_id.clone().unwrap_or_else(|| "dm".into());
    log::info!("voice Opcode 4 guild={guild_label} channel={channel_id}");
    let channel_for_err = channel_id.clone();
    tauri::async_runtime::spawn(async move {
        match gateway_join
            .voice_state(guild_id, Some(channel_id), mute, deaf)
            .await
        {
            Ok(()) => {
                // Do not clobber a newer status (connecting/live) with a stale clone.
                log::info!("voice Opcode 4 sent");
            }
            Err(err) => {
                log::error!("voice Opcode 4 failed: {err}");
                let fail = VoiceStatus {
                    phase: "error".into(),
                    detail: err,
                    channel_id: Some(channel_for_err),
                    ..VoiceStatus::default()
                };
                emit_status(&app_join, &fail);
            }
        }
    });
    let cmd_later = cmd_tx.clone();
    tauri::async_runtime::spawn(async move {
        // Give VOICE_SERVER_UPDATE room to arrive before re-issuing Opcode 4.
        tokio::time::sleep(Duration::from_secs(8)).await;
        let _ = cmd_later.send(VCmd::RetryVoiceServer { ticket });
    });
    if status.phase == "joining" || status.phase == "idle" {
        status.detail = "Waiting for voice server…".into();
        emit_status(app, status);
    }
}

async fn spawn_link(
    app: AppHandle,
    pending: &Pending,
    user_id: String,
    baseline: VoiceStatus,
    cmd: mpsc::UnboundedSender<VCmd>,
) -> Result<RtpOut, String> {
    let (pcm_tx, pcm_rx) = mpsc::channel(4);
    let (stop_tx, stop_rx) = oneshot::channel();
    let args = ConnectArgs {
        app: app.clone(),
        server_id: pending
            .guild_id
            .clone()
            .unwrap_or_else(|| pending.channel_id.clone()),
        user_id,
        session_id: pending.session_id.clone().unwrap_or_default(),
        endpoint: pending.endpoint.clone().unwrap_or_default(),
        token: pending.token.clone().unwrap_or_default(),
        baseline,
    };
    // Never await the RTP loop here — the voice command select must stay live.
    tauri::async_runtime::spawn(async move {
        let app = args.app.clone();
        let mut fail = args.baseline.clone();
        match connect_voice(args, pcm_rx, stop_rx).await {
            Ok(()) => {}
            Err(err) if err == "left voice" => {}
            Err(err) if is_session_invalid(&err) => {
                log::warn!("voice session invalid (4006): refreshing credentials");
                fail.phase = "joining".into();
                fail.detail = "Refreshing voice session…".into();
                publish(&app, fail);
                let _ = cmd.send(VCmd::RefreshSession);
            }
            Err(err) => {
                // Stay in "joining" (not "error") so the call overlay stays pinned
                // while LinkDead reconnects — avoids chat flicker during transport flaps.
                log::warn!("voice: {err}");
                fail.phase = "joining".into();
                fail.detail = if is_dave_required(&err) {
                    "Negotiating voice encryption…".into()
                } else {
                    format!("Reconnecting… ({err})")
                };
                publish(&app, fail);
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = cmd.send(VCmd::LinkDead);
            }
        }
    });
    Ok(RtpOut {
        pcm: pcm_tx,
        stop: Some(stop_tx),
    })
}

fn is_session_invalid(err: &str) -> bool {
    err.contains("4006") || err.to_ascii_lowercase().contains("session no longer valid")
}

fn is_dave_required(err: &str) -> bool {
    err.contains("4017")
        || err.to_ascii_lowercase().contains("dave")
        || err.to_ascii_lowercase().contains("e2ee")
}

/// Latest mic frame, or silence if the capture pump has nothing ready.
/// `None` only when the PCM channel is permanently closed.
fn take_pcm_frame(pcm: &mut mpsc::Receiver<Vec<f32>>) -> Option<Vec<f32>> {
    let mut latest: Option<Vec<f32>> = None;
    loop {
        match pcm.try_recv() {
            Ok(samples) if samples.len() == FRAME_SAMPLES * 2 => latest = Some(samples),
            Ok(_) => {}
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                return Some(latest.unwrap_or_else(|| vec![0.0f32; FRAME_SAMPLES * 2]));
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return latest,
        }
    }
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
    baseline: VoiceStatus,
}

async fn connect_voice(
    args: ConnectArgs,
    mut pcm: mpsc::Receiver<Vec<f32>>,
    mut stop: oneshot::Receiver<()>,
) -> Result<(), String> {
    let mut dave = DaveSession::new();
    let url = voice_url(&args.endpoint);
    log::info!("voice connecting {url}");
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
    // Voice gateway v8 requires seq_ack on heartbeats; track the last numbered message.
    let seq_ack = Arc::new(AtomicU64::new(u64::MAX)); // MAX == "no seq yet" → omit seq_ack
    let hello = next_op(&mut read, &mut stop, &mut dave, &text_tx, &seq_ack, 8).await?;
    let interval = hello
        .pointer("/d/heartbeat_interval")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_f64().map(|ms| ms as u64))
        })
        .unwrap_or(41_250);
    let heartbeat_tx = text_tx.clone();
    let heartbeat_seq = seq_ack.clone();
    let heartbeat = tokio::spawn(async move {
        // Voice gateway v8: Opcode 3 must be `{ t, seq_ack? }` — a bare integer nonce
        // is rejected and the media server later closes the session (often as 4006).
        loop {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let last = heartbeat_seq.load(Ordering::Relaxed);
            let payload = if last == u64::MAX {
                json!({ "op": 3, "d": { "t": t } })
            } else {
                json!({ "op": 3, "d": { "t": t, "seq_ack": last } })
            }
            .to_string();
            if heartbeat_tx.send(payload).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(interval.max(1))).await;
        }
    });
    if args.user_id.is_empty() || args.session_id.is_empty() || args.token.is_empty() {
        return Err("Voice identify is missing user, session, or token.".into());
    }
    // Discord media servers close with 4017 when max_dave_protocol_version is 0/omitted.
    // Advertise DAVE v1 so E2EE-required channels accept the handshake.
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
        .map_err(|_| "voice socket closed before IDENTIFY".to_string())?;
    log::info!(
        "voice IDENTIFY server={} user={} session={}… dave={}",
        args.server_id,
        args.user_id,
        &args.session_id[..args.session_id.len().min(8)],
        DaveSession::VERSION
    );
    let ready = next_op(&mut read, &mut stop, &mut dave, &text_tx, &seq_ack, 2).await?;
    log::info!(
        "voice Ready ssrc={} {}:{}",
        ready["d"]["ssrc"],
        ready["d"]["ip"],
        ready["d"]["port"]
    );
    let ready: VoiceReady =
        serde_json::from_value(ready["d"].clone()).map_err(|err| err.to_string())?;
    let mode = TransportMode::choose(&ready.modes)
        .ok_or("The voice server offered no supported encryption mode.")?;
    let udp = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|err| err.to_string())?;
    udp.connect((ready.ip.as_str(), ready.port))
        .await
        .map_err(|err| format!("UDP connect {}:{} — {err}", ready.ip, ready.port))?;
    udp.send(&crypto::discovery_packet(ready.ssrc))
        .await
        .map_err(|err| err.to_string())?;
    let mut discovery = [0u8; 74];
    // Keep reading the voice WS while waiting on UDP so DAVE/seq heartbeats stay live.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::select! {
            _ = &mut stop => return Err("left voice".into()),
            result = udp.recv(&mut discovery) => {
                result.map_err(|err| err.to_string())?;
                break;
            }
            message = read.next() => {
                let message = message
                    .ok_or_else(|| "voice socket closed".to_string())?
                    .map_err(|err| format!("voice websocket error: {err}"))?;
                if let Message::Close(frame) = &message {
                    return Err(close_error(frame));
                }
                note_seq(&seq_ack, &message, &mut dave);
                for reply in dave_replies(&mut dave, message) {
                    let _ = text_tx.send(reply);
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Err("IP discovery timed out".into());
            }
        }
    }
    let (address, port) =
        crypto::parse_discovery(&discovery).ok_or("IP discovery was unreadable.")?;
    log::info!("voice IP discovery local={address}:{port} mode={}", mode.discord_name());
    let select = json!({
        "op": 1,
        "d": {
            "protocol": "udp",
            "data": {
                "address": address,
                "port": port,
                "mode": mode.discord_name()
            }
        }
    });
    text_tx
        .send(select.to_string())
        .map_err(|_| "voice socket closed before SELECT".to_string())?;
    let described = next_op(&mut read, &mut stop, &mut dave, &text_tx, &seq_ack, 4).await?;
    let described: SessionDescription =
        serde_json::from_value(described["d"].clone()).map_err(|err| err.to_string())?;
    if described.secret_key.len() != 32 {
        return Err(format!(
            "The voice key was {} bytes (expected 32).",
            described.secret_key.len()
        ));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&described.secret_key);
    dave.set_transport_key(&key);
    dave.note_version(described.dave_protocol_version);
    let mut live = args.baseline.clone();
    live.phase = "live".into();
    live.detail = format!(
        "Connected{}",
        live.channel_name
            .as_ref()
            .map(|name| format!(" to {name}"))
            .unwrap_or_default()
    );
    live.transport = mode.label().into();
    live.dave = dave.label().into();
    live.encoder = encoder_name();
    live.sample_rate = PIPELINE_RATE;
    live.channels = PIPELINE_CHANNELS;
    publish(&args.app, live);
    // Advertise speaking so the voice server routes our SSRC immediately.
    let _ = text_tx.send(
        json!({
            "op": 5,
            "d": { "speaking": 1, "delay": 0, "ssrc": ready.ssrc }
        })
        .to_string(),
    );
    let mut encoder = Encoder::new()?;
    let mut decoder = Decoder::new()?;
    let playback = PlaybackPump::start(String::new());
    let mut sequence = 0u16;
    let mut timestamp = 0u32;
    let mut nonce = 0u32;
    let mut inbound = [0u8; 4096];
    // Always emit 20 ms RTP (mic PCM or silence) so the SFU keeps the socket alive
    // even when no input device is available.
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            received = udp.recv(&mut inbound) => {
                if let Ok(size) = received {
                    if let Ok(payload) = crypto::open(mode, &key, &inbound[..size]) {
                        let opus = dave.open_media(&payload);
                        if !opus.is_empty() {
                            if let Ok(samples) = decoder.decode(opus) {
                                playback.push(samples);
                            }
                        }
                    }
                }
            }
            _ = tick.tick() => {
                let frame = match take_pcm_frame(&mut pcm) {
                    Some(frame) => frame,
                    None => break, // channel closed
                };
                let opus = if frame.iter().all(|sample| *sample == 0.0) {
                    SILENCE_OPUS.to_vec()
                } else {
                    match encoder.encode(&frame) {
                        Ok(packet) => packet,
                        Err(_) => continue,
                    }
                };
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
                    Some(Ok(Message::Close(frame))) => {
                        heartbeat.abort();
                        playback.stop();
                        drop(text_tx);
                        let _ = writer.await;
                        return Err(close_error(&frame));
                    }
                    Some(Ok(incoming)) => {
                        note_seq(&seq_ack, &incoming, &mut dave);
                        for reply in dave_replies(&mut dave, incoming) {
                            let _ = text_tx.send(reply);
                        }
                    }
                    Some(Err(err)) => {
                        heartbeat.abort();
                        playback.stop();
                        drop(text_tx);
                        let _ = writer.await;
                        return Err(format!("voice websocket error: {err}"));
                    }
                    None => break,
                }
            }
        }
    }
    playback.stop();
    heartbeat.abort();
    drop(text_tx);
    let _ = writer.await;
    Ok(())
}

fn close_error(frame: &Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>) -> String {
    let code = frame
        .as_ref()
        .map(|close| u16::from(close.code))
        .unwrap_or(0);
    let reason = frame
        .as_ref()
        .map(|close| close.reason.to_string())
        .unwrap_or_default();
    if code == 4006 {
        return format!("voice session invalid (4006) {reason}");
    }
    if code == 4017 {
        return format!("voice socket closed (4017 E2EE/DAVE protocol required) {reason}");
    }
    format!("voice socket closed ({code} {reason})")
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

fn note_seq(seq_ack: &AtomicU64, message: &Message, dave: &mut DaveSession) {
    match message {
        Message::Text(text) => {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                if let Some(seq) = value.get("seq").and_then(|seq| seq.as_u64()) {
                    seq_ack.store(seq, Ordering::Relaxed);
                }
            }
        }
        Message::Binary(bytes) => {
            if bytes.len() >= 3 && (21..=31).contains(&bytes[2]) {
                let seq = u16::from_be_bytes([bytes[0], bytes[1]]) as u64;
                seq_ack.store(seq, Ordering::Relaxed);
                dave.seq_ack = seq as u16;
            }
        }
        _ => {}
    }
}

async fn next_op(
    read: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    stop: &mut oneshot::Receiver<()>,
    dave: &mut DaveSession,
    outbound: &mpsc::UnboundedSender<String>,
    seq_ack: &AtomicU64,
    op: u64,
) -> Result<serde_json::Value, String> {
    loop {
        let message = tokio::select! {
            _ = &mut *stop => return Err("left voice".into()),
            message = read.next() => message
                .ok_or_else(|| "voice socket closed".to_string())?
                .map_err(|err| format!("voice websocket error: {err}"))?,
        };
        if let Message::Close(frame) = &message {
            return Err(close_error(frame));
        }
        note_seq(seq_ack, &message, dave);
        // Answer DAVE/MLS opcodes at any handshake stage (Ready through Session Description).
        for reply in dave_replies(dave, message.clone()) {
            let _ = outbound.send(reply);
        }
        let Message::Text(text) = message else { continue; };
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
        let got = value.get("op").and_then(|value| value.as_u64());
        if got == Some(op) {
            return Ok(value);
        }
        if let Some(got) = got {
            log::debug!("voice skipping op={got} while waiting for op={op}");
        }
    }
}

fn voice_url(endpoint: &str) -> String {
    // VOICE_SERVER_UPDATE endpoint looks like `host.discord.media:443`.
    let cleaned = normalize_endpoint(endpoint);
    let host = cleaned.split('/').next().unwrap_or(cleaned.as_str());
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) if port.parse::<u16>().is_ok() => (name, port),
        _ => (host, "443"),
    };
    if port == "443" {
        format!("wss://{name}/?v=8")
    } else {
        format!("wss://{name}:{port}/?v=8")
    }
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

