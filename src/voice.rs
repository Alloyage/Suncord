//! Voice and stream sessions driven by the worker, and their browser playback stream.
use crate::discord::{
    gateway::GatewayHandle,
    media_signaling::{
        MediaEvent, MediaEventKind, StreamCommand, VoiceAttempt, VoiceCommand, VoiceTarget,
    },
};
use crate::{http::App, worker::Request};
use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::{
    sync::{atomic::Ordering, Arc, Mutex},
    time::Duration,
};
use suncord_media::{
    capture::{Capture, ClockTime, Codec, VideoEncoder},
    transport::{self, Credentials, Mode},
    Identity,
};
use tokio::{
    sync::{broadcast, mpsc, watch},
    task::JoinHandle,
    time::{self, Instant},
};

pub enum BrowserEvent {
    Reset,
    StreamReset,
    Frame {
        stream: bool,
        frame: transport::ReceivedFrame,
    },
    /// Whether the local user is audibly speaking.
    Speaking {
        user: u64,
        speaking: bool,
    },
}

#[derive(Deserialize, PartialEq)]
pub struct MediaScope {
    pub session: u64,
    pub call: String,
}

/// Microphone and self mute and deafen, changed without rejoining.
#[derive(Clone, Default)]
pub struct VoiceSettings {
    pub source: Option<String>,
    pub mute: bool,
    pub deaf: bool,
}

#[derive(Clone)]
pub struct ShareSettings {
    pub encoder: VideoEncoder,
    pub audio_source: Option<String>,
    pub video_source: Option<String>,
}

struct StreamControls {
    share: watch::Receiver<Option<ShareSettings>>,
    watching: watch::Receiver<Option<u64>>,
    watch_control: watch::Sender<Option<u64>>,
}

#[derive(Clone, Default, PartialEq, Serialize)]
pub struct Status {
    pub id: String,
    pub phase: String,
    pub channel_id: Option<String>,
    pub stream_phase: String,
    pub watch_phase: String,
    pub watch_error: Option<String>,
    pub watch_owner: Option<String>,
    pub stream_error: Option<String>,
    pub video_width: u32,
    pub video_height: u32,
    pub video_fps: u32,
    pub video_encoder: String,
    pub error: Option<String>,
    pub sharing: bool,
    pub video_frames: u64,
    pub video_keyframes: u64,
    pub video_plis: u64,
    pub video_nacks: u64,
    pub video_feedback: u64,
    pub video_encoded: u64,
    pub video_bitrate: u32,
    /// Discord's REMB for a shared stream, zero when absent.
    pub receiver_bitrate: u32,
    /// Voice heartbeat round trip in milliseconds, zero until measured.
    pub ping: u32,
    /// Who watches the local user's stream.
    pub viewers: Vec<String>,
}

impl Status {
    fn end_share(&mut self) {
        self.sharing = false;
        self.viewers.clear();
        self.stream_phase.clear();
    }

    fn end_watch(&mut self) {
        self.watch_phase.clear();
        self.watch_owner = None;
    }
}

/// Use inside a Tokio LocalSet. UI requests never own native handles.
pub struct Controller {
    gateway: GatewayHandle,
    user: String,
    target: VoiceTarget,
    voice: VoiceSettings,
    output: mpsc::Sender<BrowserEvent>,
    pub status: Arc<Mutex<Status>>,
    task: Option<(watch::Sender<bool>, JoinHandle<()>)>,
    voice_control: Option<watch::Sender<VoiceSettings>>,
    stream_control: Option<watch::Sender<Option<ShareSettings>>>,
    watch_control: Option<watch::Sender<Option<u64>>>,
}

impl Controller {
    pub fn new(
        gateway: GatewayHandle,
        user: String,
        target: VoiceTarget,
        voice: VoiceSettings,
        output: mpsc::Sender<BrowserEvent>,
    ) -> Self {
        Self {
            gateway,
            user,
            target,
            voice,
            output,
            status: Arc::new(Mutex::new(Status {
                phase: "Disconnected".into(),
                ..Default::default()
            })),
            task: None,
            voice_control: None,
            stream_control: None,
            watch_control: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(|(_, task)| !task.is_finished())
    }

    pub fn set_voice(&mut self, change: impl FnOnce(&mut VoiceSettings)) -> Result<(), String> {
        let mut voice = self.voice.clone();
        change(&mut voice);
        self.voice_control
            .as_ref()
            .ok_or("Join voice first")?
            .send(voice.clone())
            .map_err(|_| "The voice session ended")?;
        self.voice = voice;
        Ok(())
    }

    pub fn set_share(&mut self, settings: Option<ShareSettings>) -> Result<(), String> {
        self.stream_control
            .as_ref()
            .ok_or("Join voice first")?
            .send(settings)
            .map_err(|_| "The voice session ended".to_owned())
    }

    pub fn watch(&mut self, owner: Option<u64>) -> Result<(), String> {
        if owner.is_some_and(|owner| owner.to_string() == self.user)
            && self.status.lock().unwrap().stream_phase.is_empty()
        {
            return Err("Start sharing your screen first".into());
        }
        self.watch_control
            .as_ref()
            .ok_or("Join voice first")?
            .send(owner)
            .map_err(|_| "The voice session ended".to_owned())
    }

    pub fn start(&mut self, id: String) -> Result<(), String> {
        if self.is_running() {
            return Err("A voice session is already running".into());
        }
        let (stop, mut cancelled) = watch::channel(false);
        let gateway = self.gateway.clone();
        let target = self.target.clone();
        let user = self.user.clone();
        let (voice_control, voice) = watch::channel(self.voice.clone());
        self.voice_control = Some(voice_control);
        let output = self.output.clone();
        let (stream_control, stream) = watch::channel(None);
        self.stream_control = Some(stream_control);
        let (watch_control, watching) = watch::channel(None);
        self.watch_control = Some(watch_control.clone());
        let status = self.status.clone();
        *status.lock().unwrap() = Status {
            id,
            phase: "Joining".into(),
            channel_id: Some(self.target.channel_id.clone()),
            ..Default::default()
        };
        let task = tokio::task::spawn_local(async move {
            let result = tokio::select! {
                biased;
                _ = cancelled.changed() => Ok(()),
                _ = output.closed() => Ok(()),
                result = session(gateway, target, user, voice, output.clone(), StreamControls { share: stream, watching, watch_control }, status.clone()) => result,
            };
            let _ = output.try_send(BrowserEvent::Reset);
            let mut state = status.lock().unwrap();
            state.end_share();
            state.end_watch();
            state.phase = if result.is_ok() {
                "Disconnected"
            } else {
                "Failed"
            }
            .into();
            state.error = result.err();
        });
        self.task = Some((stop, task));
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<(), String> {
        if let Some((stop, task)) = self.task.take() {
            let mut events = self.gateway.subscribe_media();
            let _ = stop.send(true);
            let finished = task.await;
            if !self.gateway.voice_active() {
                return finished.map_err(|_| "Media worker stopped unexpectedly".into());
            }
            // Wait for Discord's own state update, including when joining was cancelled.
            let leave = async {
                loop {
                    let event = events
                        .recv()
                        .await
                        .map_err(|_| "Voice leave acknowledgement was lost".to_owned())?;
                    if !self.gateway.voice_active() {
                        return Ok::<_, String>(());
                    }
                    if let MediaEventKind::VoiceState(state) = event.kind {
                        if state.user_id == self.user && state.channel_id.is_none() {
                            return Ok(());
                        }
                    }
                }
            };
            let left = time::timeout(Duration::from_secs(5), leave)
                .await
                .unwrap_or_else(|_| Err("Voice leave was not acknowledged".to_owned()));
            if let Err(error) = left {
                let mut state = self.status.lock().unwrap();
                state.phase = "Failed".into();
                state.error.get_or_insert(error.clone());
                return Err(error);
            }
            finished.map_err(|_| "Media worker stopped unexpectedly".to_owned())?;
        }
        Ok(())
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        if let Some((stop, _)) = &self.task {
            let _ = stop.send(true);
        }
    }
}

/// Runs its closure on drop, undoing signaling state on every exit path.
struct Defer<F: FnMut()>(F);
impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

async fn session(
    gateway: GatewayHandle,
    target: VoiceTarget,
    user: String,
    mut settings: watch::Receiver<VoiceSettings>,
    output: mpsc::Sender<BrowserEvent>,
    streams: StreamControls,
    status: Arc<Mutex<Status>>,
) -> Result<(), String> {
    let StreamControls {
        share: mut stream,
        mut watching,
        watch_control,
    } = streams;
    // Fail before joining if native capture or encryption dependencies are absent.
    let identity = Identity::new(&user)?;
    let me = user.parse().unwrap_or_default();
    let new_capture = |source: &Option<String>| Capture::audio(source.as_deref(), true);
    let mut current = settings.borrow_and_update().clone();
    let (mut capture, mut input) = new_capture(&current.source)?;
    let capture_origin = Instant::now();
    let mut capture_offset = ClockTime::ZERO;
    let mut last_input_pts = ClockTime::ZERO;
    let (forward, transport_input) = mpsc::channel(4);
    let mut events = gateway.subscribe_media();
    let attempt = gateway
        .join_voice(target.clone(), (current.mute, current.deaf))
        .map_err(|e| e.to_string())?;
    let _leave = Defer(|| {
        let _ = gateway.voice_command(attempt, VoiceCommand::Leave);
    });
    let mut voice = None;
    let mut server = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while voice.is_none() || server.is_none() {
        let event = time::timeout_at(deadline, events.recv())
            .await
            .map_err(|_| "Voice join timed out")?
            .map_err(|_| "Voice signaling was lost")?;
        if event.attempt != Some(attempt) {
            continue;
        }
        match event.kind {
            MediaEventKind::Invalidated => return Err("Voice signaling was invalidated".into()),
            MediaEventKind::VoiceState(state) => {
                if state.user_id != user
                    || state.guild_id != target.guild_id
                    || state.channel_id.as_deref() != Some(&target.channel_id)
                {
                    return Err("Voice target changed".into());
                }
                voice = Some(state);
            }
            MediaEventKind::VoiceServer(allocation) => {
                if allocation.endpoint.is_none() {
                    return Err("Voice server is being reallocated".into());
                }
                server = Some(allocation);
            }
            _ => {}
        }
    }
    let voice = voice.unwrap();
    let mut server_muted = voice.mute || voice.suppress;
    let sending =
        |current: &VoiceSettings, server_muted| !(current.mute || current.deaf || server_muted);
    // The announced Speaking flag also gates sent audio.
    let (speaking, announced) = watch::channel(sending(&current, server_muted));
    let server = server.unwrap();
    let credentials = Credentials {
        identity,
        server_id: target
            .guild_id
            .clone()
            .unwrap_or_else(|| target.channel_id.clone()),
        channel_id: target.channel_id.clone(),
        user_id: user,
        session_id: voice.session_id.clone(),
        token: server.token,
        endpoint: server.endpoint.unwrap(),
    };
    status.lock().unwrap().phase = "Negotiating encryption".into();
    let (send, mut received) = mpsc::channel(8);
    let report = status.clone();
    let mode = Mode::Voice {
        input: transport_input,
        output: send,
        speaking: announced,
    };
    let transport = transport::run(credentials.clone(), mode, move |value| {
        let mut state = report.lock().unwrap();
        state.ping = value.ping;
        state.phase = if value.transport_ready {
            "Encrypted voice"
        } else {
            "Negotiating encryption"
        }
        .into();
    });
    tokio::pin!(transport);
    let streaming = async {
        loop {
            let settings = stream.borrow_and_update().clone();
            if let Some(settings) = settings {
                {
                    let mut state = status.lock().unwrap();
                    state.stream_error = None;
                    state.video_frames = 0;
                    state.video_encoded = 0;
                    state.video_bitrate = 0;
                    state.stream_phase = "Starting stream".into();
                }
                let changed = tokio::select! {
                    result = share(&gateway, attempt, &target, &credentials, &settings, &output, status.clone()) => {
                        status.lock().unwrap().stream_error = result.err();
                        false
                    },
                    changed = stream.changed() => {
                        changed.map_err(|_| "Stream controls closed")?;
                        true
                    },
                };
                status.lock().unwrap().end_share();
                watch_control.send_if_modified(|owner| {
                    if *owner == Some(me) {
                        *owner = None;
                        true
                    } else {
                        false
                    }
                });
                if changed {
                    continue;
                }
            }
            stream
                .changed()
                .await
                .map_err(|_| "Stream controls closed")?;
        }
    };
    let viewing = async {
        loop {
            let owner = *watching.borrow_and_update();
            if let Some(owner) = owner {
                {
                    let mut state = status.lock().unwrap();
                    state.watch_error = None;
                    state.watch_owner = Some(owner.to_string());
                    state.watch_phase = if owner == me {
                        "Previewing your screen"
                    } else {
                        "Connecting to stream"
                    }
                    .into();
                }
                let operation = async {
                    if owner == me {
                        std::future::pending::<Result<(), String>>().await
                    } else {
                        watch_stream(
                            &gateway,
                            attempt,
                            &target,
                            &credentials,
                            owner,
                            &output,
                            status.clone(),
                        )
                        .await
                    }
                };
                let changed = tokio::select! {
                    result = operation => {
                        status.lock().unwrap().watch_error = result.err();
                        false
                    },
                    changed = watching.changed() => {
                        changed.map_err(|_| "Watch controls closed")?;
                        true
                    },
                };
                status.lock().unwrap().end_watch();
                output
                    .send(BrowserEvent::StreamReset)
                    .await
                    .map_err(|_| "Browser playback stopped")?;
                if changed {
                    continue;
                }
            }
            watching
                .changed()
                .await
                .map_err(|_| "Watch controls closed")?;
        }
    };
    tokio::pin!(streaming, viewing);
    let mut tick = time::interval(Duration::from_millis(100));
    let (mut heard, mut shown) = (None::<Instant>, false);
    loop {
        tokio::select! {
            result = &mut transport => return result,
            result = &mut streaming => return result,
            result = &mut viewing => return result,
            changed = settings.changed() => {
                changed.map_err(|_| "Voice controls closed")?;
                let next = settings.borrow_and_update().clone();
                if next.source != current.source {
                    let replacement = new_capture(&next.source)?;
                    capture_offset = ClockTime::from_nseconds(capture_origin.elapsed().as_nanos() as u64).max(last_input_pts + ClockTime::from_mseconds(20));
                    (capture, input) = replacement;
                }
                if (next.mute, next.deaf) != (current.mute, current.deaf) {
                    gateway.voice_command(attempt, VoiceCommand::SetVoice(next.mute, next.deaf)).map_err(|e| e.to_string())?;
                }
                current = next;
                speaking.send_replace(sending(&current, server_muted));
            }
            frame = input.recv() => {
                let mut frame = frame.ok_or("Microphone capture ended")?;
                if !*speaking.borrow() { continue; }
                frame.pts += capture_offset;
                last_input_pts = frame.pts;
                if let Err(mpsc::error::TrySendError::Closed(_)) = forward.try_send(frame) {
                    return Err("Voice transport ended".into());
                }
            }
            event = events.recv() => {
                let event = event.map_err(|_| "Voice signaling was lost")?;
                if event.attempt != Some(attempt) { continue; }
                match event.kind {
                    MediaEventKind::Invalidated | MediaEventKind::VoiceServer(_) => return Err("Voice session changed, reconnect explicitly".into()),
                    MediaEventKind::VoiceState(state) => {
                        if state.session_id != voice.session_id { return Err("Disconnected because you joined from another client".into()); }
                        if state.channel_id.as_deref() != Some(&target.channel_id) { return Err("Voice session changed".into()); }
                        server_muted = state.mute || state.suppress;
                        speaking.send_replace(sending(&current, server_muted));
                    }
                    _ => {}
                }
            }
            frame = received.recv() => {
                let frame = frame.ok_or("Voice receive queue closed")?;
                // A late voice frame is worthless, so a full queue drops it.
                if let Err(mpsc::error::TrySendError::Closed(_)) = output.try_send(BrowserEvent::Frame { stream: false, frame }) {
                    return Err("Browser playback stopped".into());
                }
            }
            _ = tick.tick() => {
                if let Some(error) = capture.error() { return Err(error); }
                // Peaks above about -36 dBFS count as speech, held for 300 ms.
                if capture.peak.swap(0, Ordering::Relaxed) > 512 { heard = Some(Instant::now()); }
                let loud = *speaking.borrow() && heard.is_some_and(|at| at.elapsed() < Duration::from_millis(300));
                if loud != shown && output.try_send(BrowserEvent::Speaking { user: me, speaking: loud }).is_ok() { shown = loud; }
            }
        }
    }
}

/// Waits for this attempt's stream allocation and returns its transport credentials
/// and latest viewers.
/// A share ignores an earlier stream's late delete until its own stream exists.
async fn allocate_stream(
    events: &mut broadcast::Receiver<MediaEvent>,
    attempt: VoiceAttempt,
    key: &str,
    sharing: bool,
    voice: &Credentials,
) -> Result<(Credentials, Vec<String>), String> {
    let mut stream = None;
    let mut viewers = Vec::new();
    let mut server = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while stream.is_none() || server.is_none() {
        let event = time::timeout_at(deadline, events.recv())
            .await
            .map_err(|_| "Stream allocation timed out")?
            .map_err(|_| "Stream signaling was lost")?;
        if event.attempt != Some(attempt) {
            continue;
        }
        match event.kind {
            MediaEventKind::StreamCreate(created) if created.stream_key == key => {
                viewers = created.viewer_ids;
                stream = Some(created.rtc_server_id);
            }
            MediaEventKind::StreamUpdate(updated) if updated.stream_key == key => {
                viewers = updated.viewer_ids
            }
            MediaEventKind::StreamServer(allocation) if allocation.stream_key == key => {
                let endpoint = allocation
                    .endpoint
                    .ok_or("Stream server is being reallocated")?;
                server = Some((allocation.token, endpoint));
            }
            MediaEventKind::StreamDelete(deleted)
                if deleted.stream_key == key && (stream.is_some() || !sharing) =>
            {
                return Err("Stream allocation was cancelled".into())
            }
            MediaEventKind::Invalidated => return Err("Stream allocation was cancelled".into()),
            _ => {}
        }
    }
    let (token, endpoint) = server.unwrap();
    Ok((
        Credentials {
            server_id: stream.unwrap(),
            token,
            endpoint,
            ..voice.clone()
        },
        viewers,
    ))
}

async fn share(
    gateway: &GatewayHandle,
    attempt: VoiceAttempt,
    target: &VoiceTarget,
    voice: &Credentials,
    settings: &ShareSettings,
    output: &mpsc::Sender<BrowserEvent>,
    status: Arc<Mutex<Status>>,
) -> Result<(), String> {
    status.lock().unwrap().stream_phase = "Select a screen or window".into();
    // Get consent and start capture before publishing a Go Live session.
    // Start with H.264 until negotiation, unless a codec was explicitly selected.
    let mut codecs = settings.encoder.codecs()?;
    if let Some(value) = std::env::var_os("SUNCORD_VIDEO_CODEC") {
        let codec = match value.to_str() {
            Some("h264") => Codec::H264,
            Some("h265") => Codec::H265,
            Some("av1") => Codec::Av1,
            _ => return Err("SUNCORD_VIDEO_CODEC must be h264, h265, or av1".into()),
        };
        codecs.retain(|available| *available == codec);
    }
    let initial = if codecs.contains(&Codec::H264) {
        Codec::H264
    } else {
        *codecs
            .first()
            .ok_or("Selected video codec is unavailable")?
    };
    let (mut capture, mut input, video) =
        Capture::screen(settings.encoder, initial, settings.video_source.as_deref()).await?;
    {
        let mut state = status.lock().unwrap();
        state.video_width = video.width;
        state.video_height = video.height;
        state.video_fps = video.fps;
        state.video_encoder = video
            .encoder
            .element(video.codec)
            .unwrap_or_default()
            .into();
        state.video_bitrate = video.bitrate;
    }
    let audio = settings
        .audio_source
        .as_deref()
        .map(|source| Capture::audio(Some(source), false))
        .transpose()?;
    let (audio_capture, audio_input) = match audio {
        Some((capture, input)) => (Some(capture), Some(input)),
        None => (None, None),
    };
    status.lock().unwrap().stream_phase = "Allocating stream".into();
    let key = target
        .stream_key(&voice.user_id)
        .map_err(|e| e.to_string())?;
    let mut events = gateway.subscribe_media();
    gateway
        .stream_command(attempt, StreamCommand::Create)
        .map_err(|e| e.to_string())?;
    let _delete = Defer(|| {
        let _ = gateway.stream_command(attempt, StreamCommand::Delete);
    });
    gateway
        .stream_command(attempt, StreamCommand::SetPaused(false))
        .map_err(|e| e.to_string())?;
    let (credentials, viewers) = allocate_stream(&mut events, attempt, &key, true, voice).await?;
    status.lock().unwrap().viewers = viewers;
    let (keyframes, mut requests) = mpsc::channel(1);
    let (encoder_rate, mut requested_rate) = watch::channel(video.bitrate);
    let report = status.clone();
    let encoded = capture.frames.clone();
    let (codec, mut selected) = watch::channel(video.codec);
    let (video_send, video_input) = mpsc::channel(1);
    let me = voice
        .user_id
        .parse::<u64>()
        .map_err(|_| "Invalid local user")?;
    let preview_keyframes = keyframes.clone();
    let relay = async {
        let mut previewing = false;
        let mut codec = video.codec;
        let mut waiting = true;
        while let Some(frame) = input.recv().await {
            let selected =
                status.lock().unwrap().watch_owner.as_deref() == Some(voice.user_id.as_str());
            if selected != previewing || frame.codec != codec || frame.discontinuity {
                previewing = selected;
                codec = frame.codec;
                waiting = true;
                if previewing {
                    let _ = preview_keyframes.try_send(());
                }
            }
            if previewing && (!waiting || frame.keyframe) {
                if waiting && output.try_send(BrowserEvent::StreamReset).is_err() {
                    let _ = preview_keyframes.try_send(());
                } else {
                    let time = ((u128::from(frame.pts.nseconds()) << 32) / 1_000_000_000) as u64;
                    let received = transport::ReceivedFrame {
                        epoch: 0,
                        codec: frame.codec,
                        keyframe: frame.keyframe,
                        sender_time: Some(time),
                        user: me,
                        timestamp: 0,
                        data: frame.data.clone(),
                    };
                    waiting = output
                        .try_send(BrowserEvent::Frame {
                            stream: true,
                            frame: received,
                        })
                        .is_err();
                    if waiting {
                        let _ = preview_keyframes.try_send(());
                    }
                }
            }
            video_send
                .send(frame)
                .await
                .map_err(|_| "Stream video transport stopped")?;
        }
        Err::<(), String>("Screen capture ended".into())
    };
    tokio::pin!(relay);
    let mode = Mode::GoLive {
        settings: video,
        codecs,
        codec,
        video: video_input,
        audio: audio_input,
        keyframes,
    };
    let transport = transport::run(credentials, mode, move |value| {
        let mut state = report.lock().unwrap();
        state.sharing = value.transport_ready;
        state.stream_phase = if value.transport_ready {
            "Sharing your screen"
        } else {
            "Waiting for a viewer"
        }
        .into();
        state.video_frames = value.video_frames;
        state.video_keyframes = value.video_keyframes;
        state.video_plis = value.video_plis;
        state.video_nacks = value.video_nacks;
        state.video_feedback = value.video_feedback;
        state.video_encoded = encoded.load(Ordering::Relaxed);
        state.receiver_bitrate = value.receiver_bitrate;
        if value.video_bitrate != 0 {
            state.video_bitrate = value.video_bitrate;
            encoder_rate.send_if_modified(|rate| {
                if *rate == value.video_bitrate {
                    return false;
                }
                *rate = value.video_bitrate;
                true
            });
        }
    });
    tokio::pin!(transport);
    let mut tick = time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            result = &mut transport => return result,
            result = &mut relay => return result,
            Ok(()) = selected.changed() => {
                let codec = *selected.borrow_and_update();
                capture.set_codec(codec)?;
                status.lock().unwrap().video_encoder = video.encoder.element(codec).unwrap_or_default().into();
            }
            changed = requested_rate.changed() => {
                changed.map_err(|_| "Stream bitrate channel closed")?;
                capture.set_video_bitrate(*requested_rate.borrow_and_update())?;
            }
            request = requests.recv() => {
                request.ok_or("Stream keyframe channel closed")?;
                capture.request_keyframe()?;
            }
            event = events.recv() => {
                let event = event.map_err(|_| "Stream signaling was lost")?;
                if event.attempt != Some(attempt) { continue; }
                match event.kind {
                    MediaEventKind::StreamDelete(value) if value.stream_key == key => return Err("Stream ended".into()),
                    MediaEventKind::StreamUpdate(value) if value.stream_key == key => status.lock().unwrap().viewers = value.viewer_ids,
                    MediaEventKind::StreamServer(value) if value.stream_key == key => return Err("Stream server changed".into()),
                    MediaEventKind::Invalidated => return Err("Stream session was invalidated".into()),
                    _ => {}
                }
            }
            _ = tick.tick() => {
                if let Some(error) = capture.error().or_else(|| audio_capture.as_ref().and_then(Capture::error)) { return Err(error); }
            }
        }
    }
}

async fn watch_stream(
    gateway: &GatewayHandle,
    attempt: VoiceAttempt,
    target: &VoiceTarget,
    voice: &Credentials,
    owner: u64,
    output: &mpsc::Sender<BrowserEvent>,
    status: Arc<Mutex<Status>>,
) -> Result<(), String> {
    let key = target
        .stream_key(&owner.to_string())
        .map_err(|e| e.to_string())?;
    let mut events = gateway.subscribe_media();
    gateway
        .stream_command(attempt, StreamCommand::Watch(owner))
        .map_err(|e| e.to_string())?;
    let _stop = Defer(|| {
        let _ = gateway.stream_command(attempt, StreamCommand::StopWatching(owner));
    });
    let (credentials, _) = allocate_stream(&mut events, attempt, &key, false, voice).await?;
    let (received, mut frames) = mpsc::channel(16);
    let report = status.clone();
    let (epoch_send, mut epochs) = watch::channel(0u64);
    let mode = Mode::Watch {
        owner,
        output: received,
    };
    let transport = transport::run(credentials, mode, move |value| {
        let _ = epoch_send.send(value.receive_epoch);
        let mut state = report.lock().unwrap();
        state.watch_phase = if value.transport_ready {
            "Watching stream"
        } else {
            "Connecting to stream"
        }
        .into();
    });
    tokio::pin!(transport);
    let mut epoch = 0;
    let mut video_started = false;
    loop {
        tokio::select! {
            biased;
            result=&mut transport => return result,
            changed=epochs.changed() => {
                changed.map_err(|_| "Stream epoch channel closed")?;
                let current=*epochs.borrow_and_update();
                if current != epoch {
                    output.try_send(BrowserEvent::StreamReset).map_err(|_| "Browser playback stopped or fell behind")?;
                    epoch=current;video_started=false;
                }
            },
            frame=frames.recv() => {
                let frame=frame.ok_or("Stream receive queue closed")?;
                if frame.epoch != epoch { continue; }
                let video=frame.codec != Codec::Opus;
                if frame.sender_time.is_none() {
                    if video && video_started { return Err("Received video lost its sender clock mapping".into()); }
                    continue;
                }
                if video && !video_started {
                    if !frame.keyframe { continue; }
                    video_started=true;
                }
                if !video_started { continue; }
                output.try_send(BrowserEvent::Frame { stream: true, frame }).map_err(|_| "Browser playback stopped or fell behind")?;
            }
            event=events.recv() => {
                let event=event.map_err(|_| "Stream watch signaling was lost")?;
                if event.attempt != Some(attempt) { continue; }
                match event.kind {
                    MediaEventKind::StreamDelete(value) if value.stream_key==key => return Err("Watched stream ended".into()),
                    MediaEventKind::StreamServer(value) if value.stream_key==key => return Err("Watched stream server changed, reconnect explicitly".into()),
                    MediaEventKind::Invalidated => return Err("Stream watch was invalidated".into()),
                    _ => {}
                }
            }
        }
    }
}

/// Length-prefixed frames tolerate arbitrary HTTP chunk boundaries: length, kind,
/// flag, sending user and timestamp. Voice timestamps are RTP ticks, stream timestamps NTP.
fn frame(kind: u8, flag: u8, user: u64, time: u64, data: &[u8]) -> Bytes {
    let mut bytes = Vec::with_capacity(22 + data.len());
    bytes.extend_from_slice(&((22 + data.len()) as u32).to_be_bytes());
    bytes.extend_from_slice(&[kind, flag]);
    bytes.extend_from_slice(&user.to_be_bytes());
    bytes.extend_from_slice(&time.to_be_bytes());
    bytes.extend_from_slice(data);
    bytes.into()
}

pub async fn media(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let Some(session) = headers
        .get("x-suncord-session")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(call) = headers
        .get("x-suncord-call")
        .and_then(|value| value.to_str().ok())
        .filter(|call| !call.is_empty() && call.len() <= 64)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (output, frames) = tokio::sync::mpsc::channel(32);
    let (reply, result) = tokio::sync::oneshot::channel();
    if app
        .commands
        .try_send(Request::Attach(
            output,
            crate::http::page_account(&headers).to_owned(),
            MediaScope {
                session,
                call: call.to_owned(),
            },
            reply,
        ))
        .is_err()
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match result.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return (StatusCode::CONFLICT, error).into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    let stream = futures_util::stream::unfold(frames, |mut frames| async {
        let bytes = match frames.recv().await? {
            BrowserEvent::Reset => frame(0, 0, 0, 0, &[]),
            BrowserEvent::StreamReset => frame(4, 0, 0, 0, &[]),
            BrowserEvent::Speaking { user, speaking } => frame(5, u8::from(speaking), user, 0, &[]),
            BrowserEvent::Frame {
                stream: false,
                frame: voice,
            } => frame(
                1,
                u8::from(voice.keyframe),
                voice.user,
                u64::from(voice.timestamp),
                &voice.data,
            ),
            BrowserEvent::Frame {
                stream: true,
                frame: video,
            } => frame(
                if video.codec != Codec::Opus { 2 } else { 3 },
                u8::from(video.keyframe)
                    | match video.codec {
                        Codec::H265 => 2,
                        Codec::Av1 => 4,
                        _ => 0,
                    },
                video.user,
                video.sender_time.unwrap_or_default(),
                &video.data,
            ),
        };
        Some((Ok::<_, std::convert::Infallible>(bytes), frames))
    });
    (
        [(header::CONTENT_TYPE, "application/octet-stream")],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}
