//! Discord v8 UDP voice and bounded video transport. Call on a current-thread Tokio
//! runtime (or LocalSet). Native DAVE handles never cross a thread boundary.
//! Supply Capture::audio(None, false)'s bounded receiver for synthetic audio.
//! No microphone, desktop, pipeline URL, or reconnect is implicit.
use crate::{
    capture::{self, Codec},
    dave,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::{
        mpsc::{self, error::TrySendError},
        watch,
    },
    time::{self, Instant},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
    MaybeTlsStream, WebSocketStream,
};
use zeroize::Zeroizing;
mod av1;
mod h264;
mod h265;
mod mls;
mod pacer;
mod rate;
mod receive;
mod rtp;
use mls::{discover_group, Mls};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const USERS: usize = 256;
const DEADLINE: Duration = Duration::from_secs(30);

// Deliberately neither Debug nor Serialize. Never include credentials or remote
// payloads/native MLS error strings in errors, callbacks, or logs.
#[derive(Clone)]
pub struct Credentials {
    /// Clone the same identity for all connections belonging to this call.
    pub identity: dave::Identity,
    pub server_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub session_id: Zeroizing<String>,
    pub token: Zeroizing<String>,
    pub endpoint: String,
}

/// Each mode owns only the bounded queues it uses. Keep capacities at most 64.
pub enum Mode {
    /// `speaking` is the announced Speaking flag. Muted callers clear it and stop input.
    Voice {
        input: mpsc::Receiver<capture::Frame>,
        output: mpsc::Sender<ReceivedFrame>,
        speaking: watch::Receiver<bool>,
    },
    /// The caller must force a capture keyframe for each bounded request notification.
    /// `codecs` lists what capture can encode. `codec` starts as the codec capture
    /// produces and changes to Discord's choice, which capture must then switch to.
    /// Video frames in any other codec are dropped.
    GoLive {
        settings: capture::VideoSettings,
        codecs: Vec<Codec>,
        codec: watch::Sender<Codec>,
        video: mpsc::Receiver<capture::Frame>,
        audio: Option<mpsc::Receiver<capture::Frame>>,
        keyframes: mpsc::Sender<()>,
    },
    Watch {
        owner: u64,
        output: mpsc::Sender<ReceivedFrame>,
    },
}

impl Mode {
    fn video(&self) -> bool {
        !matches!(self, Self::Voice { .. })
    }

    fn output(&self) -> Option<&mpsc::Sender<ReceivedFrame>> {
        match self {
            Self::Voice { output, .. } | Self::Watch { output, .. } => Some(output),
            Self::GoLive { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Status {
    /// True only when UDP transport and DAVE keys are both usable.
    pub transport_ready: bool,
    pub video_frames: u64,
    /// Keyframes accepted by the packet queue, and feedback received from Discord.
    pub video_keyframes: u64,
    pub video_plis: u64,
    pub video_nacks: u64,
    pub video_feedback: u64,
    pub video_bitrate: u32,
    /// Discord's latest REMB for the stream, zero when absent.
    pub receiver_bitrate: u32,
    pub receive_epoch: u64,
    /// Voice gateway heartbeat round trip in milliseconds.
    pub ping: u32,
}

/// A complete decrypted encoded frame, ready for browser decoding.
/// The bounded output queue drops frames on overflow, never blocking heartbeats.
pub struct ReceivedFrame {
    pub epoch: u64,
    pub codec: Codec,
    pub keyframe: bool,
    /// Sender NTP time in unsigned 32.32 fixed point, when recently mapped by RTCP.
    pub sender_time: Option<u64>,
    /// The sending user, mapped from its SSRC by Speaking events.
    pub user: u64,
    /// Wrapping RTP ticks at the codec's clock rate.
    pub timestamp: u32,
    pub data: Vec<u8>,
}

/// Cancel by dropping the future. Errors are sanitized.
/// The caller owns capture and drops it when this returns.
pub async fn run(
    credentials: Credentials,
    mode: Mode,
    mut report: impl FnMut(Status),
) -> Result<(), String> {
    let mut last = Status::default();
    report(last);
    let result = match Call::connect(credentials, mode, &mut |status| {
        last = status;
        report(status);
    })
    .await
    {
        Ok(call) => call.run().await,
        Err(error) => Err(error),
    };
    last.transport_ready = false;
    report(last);
    result
}

async fn send(ws: &mut Socket, message: Message) -> Result<(), String> {
    time::timeout(Duration::from_secs(5), ws.send(message))
        .await
        .map_err(|_| "Voice websocket write timed out")?
        .map_err(|_| "Voice websocket write failed".into())
}
async fn event(ws: &mut Socket, op: u8, data: Value) -> Result<(), String> {
    send(
        ws,
        Message::Text(json!({"op":op,"d":data}).to_string().into()),
    )
    .await
}
async fn binary(ws: &mut Socket, op: u8, bytes: Vec<u8>) -> Result<(), String> {
    if bytes.len() > 1024 * 1024 {
        return Err("DAVE output exceeded limit".into());
    }
    let mut payload = Vec::with_capacity(bytes.len() + 1);
    payload.push(op);
    payload.extend(bytes);
    send(ws, Message::Binary(payload.into())).await
}
fn number(v: &Value, key: &str) -> Result<u64, String> {
    v[key]
        .as_u64()
        .ok_or_else(|| "Malformed voice control message".into())
}
fn int<T: TryFrom<u64>>(v: &Value, key: &str) -> Result<T, String> {
    T::try_from(number(v, key)?).map_err(|_| "Voice control integer out of range".into())
}
fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::Opus => "opus",
        Codec::H264 => "H264",
        Codec::H265 => "H265",
        Codec::Av1 => "AV1",
    }
}
fn valid_id(text: &str) -> bool {
    text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit()) && text.parse::<u64>().is_ok()
}
fn id(v: &Value) -> Result<String, String> {
    v.as_str()
        .filter(|text| valid_id(text))
        .map(str::to_owned)
        .ok_or_else(|| "Malformed voice user ID".into())
}
fn version(v: &Value, key: &str, max: u16) -> Result<u16, String> {
    let value = int(v, key)?;
    if value == 0 || value > max {
        return Err("Unsupported DAVE version, plaintext forbidden".into());
    }
    Ok(value)
}
fn endpoint(value: &str) -> Result<String, String> {
    // Gateway supplies a host[:port], never an arbitrary application URL.
    let (host, port) = value
        .split_once(':')
        .map_or((value, None), |(h, p)| (h, Some(p)));
    if host.is_empty()
        || host.len() > 253
        || !host.ends_with(".discord.media")
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        || port.is_some_and(|p| p.parse::<u16>().ok().filter(|p| *p != 0).is_none())
    {
        return Err("Invalid Discord voice endpoint".into());
    }
    Ok(format!("wss://{value}/?v=8"))
}

async fn next_frame(mode: &mut Mode) -> Option<capture::Frame> {
    match mode {
        Mode::Voice { input, .. }
        | Mode::GoLive {
            video: input,
            audio: None,
            ..
        } => input.recv().await,
        Mode::GoLive {
            video,
            audio: Some(audio),
            ..
        } => tokio::select! { frame = video.recv() => frame, frame = audio.recv() => frame },
        Mode::Watch { .. } => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

async fn receive(socket: Option<&UdpSocket>, buffer: &mut [u8]) -> std::io::Result<usize> {
    match socket {
        Some(socket) => socket.recv(buffer).await,
        None => std::future::pending().await,
    }
}

// Audio and video have independent RTP sequence numbers, clocks and reports.
struct Sender {
    sequence: u16,
    origin: u32,
    timestamp: u32,
    last_pts: Option<u64>,
    last_media: Instant,
    packets: u64,
    octets: u64,
}
impl Sender {
    fn new() -> Self {
        let origin = rand::random();
        Self {
            sequence: rand::random(),
            origin,
            timestamp: origin,
            last_pts: None,
            last_media: Instant::now(),
            packets: 0,
            octets: 0,
        }
    }

    /// Repairs pass no timestamp and leave the media clock mapping alone.
    fn sent(&mut self, timestamp: Option<u32>, octets: usize) {
        if let Some(timestamp) = timestamp {
            self.timestamp = timestamp;
            self.last_media = Instant::now();
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.packets = self.packets.saturating_add(1);
        self.octets = self.octets.saturating_add(octets as u64);
    }
}

struct ReceiverReport {
    ntp: u64,
    timestamp: u32,
    received: Instant,
}

impl ReceiverReport {
    fn time(&self, timestamp: u32, clock: u32) -> Option<u64> {
        let clock = i64::from(clock);
        let delta = i64::from(timestamp.wrapping_sub(self.timestamp) as i32);
        if self.received.elapsed() > Duration::from_secs(10) || delta.abs() > clock * 10 {
            return None;
        }
        Some(self.ntp.wrapping_add((delta * (1i64 << 32) / clock) as u64))
    }
}

fn watch_sources(
    d: &Value,
    local: u32,
    local_video: Option<(u32, u32)>,
) -> Result<Option<(u32, u32, u32)>, String> {
    let audio = int(d, "audio_ssrc")?;
    if audio == 0 || audio == local || local_video.is_some_and(|(v, r)| audio == v || audio == r) {
        return Err("Invalid watched audio SSRC".into());
    }
    let streams = d["streams"]
        .as_array()
        .filter(|s| s.len() <= 8)
        .ok_or("Invalid watched video streams")?;
    let mut selected = None;
    let mut seen = BTreeSet::from([audio, local]);
    if let Some((v, r)) = local_video {
        seen.extend([v, r]);
    }
    for stream in streams {
        let active = match stream.get("active") {
            Some(value) => value.as_bool().ok_or("Invalid watched stream activity")?,
            None => true,
        };
        if !active {
            continue;
        }
        let quality = number(stream, "quality")?;
        let video = int(stream, "ssrc")?;
        let rtx = int(stream, "rtx_ssrc")?;
        if video == 0 || rtx == 0 || !seen.insert(video) || !seen.insert(rtx) {
            return Err("Conflicting watched video SSRCs".into());
        }
        if selected.is_none() || quality == 100 {
            selected = Some((audio, video, rtx));
        }
    }
    Ok(selected)
}

struct Call<'a> {
    mode: Mode,
    identity: dave::Identity,
    user_id: String,
    /// The watched user, only in watch mode.
    owner: Option<String>,
    max_version: u16,
    status: Status,
    on_status: &'a mut dyn FnMut(Status),
    ws: Socket,
    // Signaling and MLS.
    users: BTreeSet<String>,
    ssrc_users: BTreeMap<u32, String>,
    mls: Option<Mls>,
    external: Option<Vec<u8>>,
    // Zero initializes only a disposable key package. Unknown is represented
    // separately and never authorizes MLS processing, transitions, or media.
    group: Option<u64>,
    dave_ready: bool,
    recoveries: u8,
    seq_ack: i64,
    heartbeat: Option<Duration>,
    next_heartbeat: Instant,
    ack: Option<(u64, Instant)>,
    negotiation_timeout: Duration,
    startup: Instant,
    // UDP transport.
    udp: Option<UdpSocket>,
    cipher: Option<rtp::Cipher>,
    discovered: bool,
    ssrc: u32,
    video_ssrc: Option<(u32, u32)>,
    // Go Live sending.
    video_codec: Codec,
    /// Discord wants no stream video for now.
    paused: bool,
    pacing: Option<(pacer::Pacer, rate::Rate)>,
    audio_sender: Sender,
    video_sender: Sender,
    rtx_sender: Sender,
    transport_sequence: u16,
    need_keyframe: bool,
    media_epoch: Option<u64>,
    next_report: Instant,
    // Watching.
    watched: Option<(u32, u32, u32)>,
    video_receiver: receive::Video,
    sender_reports: BTreeMap<u32, ReceiverReport>,
    receive_epoch: Option<(Option<u64>, bool)>,
    subscribed: bool,
    pli_pending: bool,
    last_pli: Option<Instant>,
}

impl<'a> Call<'a> {
    async fn connect(
        c: Credentials,
        mode: Mode,
        on_status: &'a mut dyn FnMut(Status),
    ) -> Result<Self, String> {
        let owner = match &mode {
            Mode::Watch { owner, .. } => {
                if *owner == 0 || owner.to_string() == c.user_id {
                    return Err("Invalid watched user".into());
                }
                Some(owner.to_string())
            }
            _ => None,
        };
        let mut status = Status::default();
        let mut video_codec = Codec::H264;
        let pacing = match &mode {
            Mode::GoLive {
                settings,
                codecs,
                codec,
                ..
            } => {
                settings.validate()?;
                if codecs.is_empty() || codecs.contains(&Codec::Opus) {
                    return Err("Invalid stream video codecs".into());
                }
                video_codec = *codec.borrow();
                let now = std::time::Instant::now();
                let rate = rate::Rate::new(settings.bitrate, now);
                status.video_bitrate = rate.bitrate();
                Some((pacer::Pacer::new(rate.bitrate(), now), rate))
            }
            _ => None,
        };
        let channel = c
            .channel_id
            .parse::<u64>()
            .map_err(|_| "Invalid channel ID")?;
        if !valid_id(&c.user_id) || !valid_id(&c.server_id) {
            return Err("Malformed voice user ID".into());
        }
        if c.session_id.is_empty()
            || c.token.is_empty()
            || c.session_id.len() > 4096
            || c.token.len() > 4096
        {
            return Err("Invalid voice credentials".into());
        }
        let max_version = dave::DaveLibrary::load()
            .map_err(|_| "DAVE library unavailable")?
            .max_supported_protocol_version();
        if max_version == 0 {
            return Err("Encrypted DAVE unavailable".into());
        }
        let config = WebSocketConfig::default()
            .max_message_size(Some(1024 * 1024))
            .max_frame_size(Some(1024 * 1024));
        let (mut ws, _) = time::timeout(
            Duration::from_secs(15),
            connect_async_with_config(endpoint(&c.endpoint)?, Some(config), false),
        )
        .await
        .map_err(|_| "Voice websocket connection timed out")?
        .map_err(|_| "Voice websocket connection failed")?;
        let video = mode.video();
        let mut identify = json!({"server_id":c.server_id,"user_id":c.user_id,"session_id":c.session_id.as_str(),"token":c.token.as_str(),"max_dave_protocol_version":max_version});
        if video {
            identify["video"] = json!(true);
            identify["streams"] = json!([{"type":"screen","rid":"100","quality":100}]);
        }
        event(&mut ws, 0, identify).await?;
        // A new stream may need a viewer to supply the first public MLS context.
        let negotiation_timeout = if video {
            Duration::from_secs(90)
        } else {
            DEADLINE
        };
        let now = Instant::now();
        Ok(Self {
            mode,
            identity: c.identity,
            users: BTreeSet::from([c.user_id.clone()]),
            user_id: c.user_id,
            owner,
            max_version,
            status,
            on_status,
            ws,
            ssrc_users: BTreeMap::new(),
            mls: None,
            external: None,
            group: if video { None } else { Some(channel) },
            dave_ready: false,
            recoveries: 0,
            seq_ack: -1,
            heartbeat: None,
            next_heartbeat: now,
            ack: None,
            negotiation_timeout,
            startup: now + negotiation_timeout,
            udp: None,
            cipher: None,
            discovered: false,
            ssrc: 0,
            video_ssrc: None,
            video_codec,
            paused: false,
            pacing,
            audio_sender: Sender::new(),
            video_sender: Sender::new(),
            rtx_sender: Sender::new(),
            transport_sequence: 1,
            need_keyframe: video,
            media_epoch: None,
            next_report: now + Duration::from_secs(5),
            watched: None,
            video_receiver: receive::Video::new(),
            sender_reports: BTreeMap::new(),
            receive_epoch: None,
            subscribed: false,
            pli_pending: true,
            last_pli: None,
        })
    }

    async fn run(mut self) -> Result<(), String> {
        let mut tick = time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        let mut buffer = [0; 4096];
        loop {
            if self.owner.is_some() {
                self.watch_upkeep().await?;
            }
            self.pacer_upkeep()?;
            let deadline = self
                .pacing
                .as_ref()
                .and_then(|(pacer, _)| pacer.deadline(std::time::Instant::now()));
            tokio::select! {
                _ = sleep_until(deadline) => self.send_video().await?,
                _ = tick.tick() => self.tick().await?,
                frame = next_frame(&mut self.mode) => {
                    self.send_frame(frame.ok_or("Media input ended")?).await?;
                }
                size = receive(self.udp.as_ref(), &mut buffer) => {
                    let size = size.map_err(|_| "Voice UDP receive failed")?;
                    self.on_udp(&buffer[..size]).await?;
                }
                message = self.ws.next() => {
                    let message = message
                        .ok_or("Voice websocket ended")?
                        .map_err(|_| "Voice websocket receive failed")?;
                    self.on_message(message).await?;
                    self.update_ready().await?;
                }
            }
        }
    }

    fn report(&mut self) {
        (self.on_status)(self.status);
    }

    fn media_ready(&self) -> bool {
        self.status.transport_ready && self.mls.as_ref().is_some_and(|m| m.ready)
    }

    async fn send_sealed(&mut self, header: &[u8], data: &[u8]) -> Result<(), String> {
        let packet = self
            .cipher
            .as_mut()
            .ok_or("Transport key missing")?
            .seal(header, data)?;
        self.udp
            .as_ref()
            .ok_or("UDP missing")?
            .send(&packet)
            .await
            .map_err(|_| "Voice UDP send failed")?;
        Ok(())
    }

    fn request_keyframe(&mut self) -> Result<(), String> {
        self.need_keyframe = true;
        if let Mode::GoLive { keyframes, .. } = &self.mode {
            if let Err(TrySendError::Closed(())) = keyframes.try_send(()) {
                return Err("Keyframe request channel closed".into());
            }
        }
        Ok(())
    }

    /// Adopts Discord's send codec and asks capture to switch when it differs.
    fn select_codec(&mut self, name: Option<&str>) -> Result<(), String> {
        let Mode::GoLive { codecs, codec, .. } = &self.mode else {
            return Ok(());
        };
        let selected = codecs
            .iter()
            .copied()
            .find(|codec| Some(codec_name(*codec)) == name)
            .ok_or("Discord selected an unoffered video codec")?;
        codec.send_if_modified(|current| std::mem::replace(current, selected) != selected);
        if selected != self.video_codec {
            self.video_codec = selected;
            self.restart_video()?;
        }
        Ok(())
    }

    /// Drops queued and repairable video, then waits for a fresh keyframe.
    fn restart_video(&mut self) -> Result<(), String> {
        if let Some((pacer, _)) = &mut self.pacing {
            pacer.clear(std::time::Instant::now());
        }
        self.request_keyframe()
    }

    fn reset_receive(&mut self) {
        self.video_receiver.clear();
        self.sender_reports.clear();
        self.subscribed = false;
        self.pli_pending = true;
        self.status.receive_epoch = self.status.receive_epoch.wrapping_add(1);
        self.report();
    }

    fn deliver(&mut self, frame: ReceivedFrame) -> Result<(), String> {
        let Some(output) = self.mode.output() else {
            return Ok(());
        };
        match output.try_send(frame) {
            Err(TrySendError::Closed(_)) => return Err("Playback queue closed".into()),
            Err(TrySendError::Full(frame)) if frame.codec != Codec::Opus => {
                self.video_receiver.clear()
            }
            _ => {}
        }
        Ok(())
    }

    async fn watch_upkeep(&mut self) -> Result<(), String> {
        let ready = self.media_ready();
        let epoch = (self.mls.as_ref().map(|m| m.generation), ready);
        if self.receive_epoch != Some(epoch) {
            self.receive_epoch = Some(epoch);
            self.reset_receive();
        }
        let Some((_, video, _)) = self.watched.filter(|_| ready) else {
            return Ok(());
        };
        if !self.subscribed {
            event(
                &mut self.ws,
                15,
                json!({video.to_string():100,"pixelCounts":{video.to_string():0}}),
            )
            .await?;
            self.subscribed = true;
        }
        self.pli_pending |= self.video_receiver.take_keyframe_request();
        if self.pli_pending
            && self
                .last_pli
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
        {
            let packet = rtp::pli(self.ssrc, video);
            self.send_sealed(&packet[..8], &packet[8..]).await?;
            self.last_pli = Some(Instant::now());
            self.pli_pending = false;
        }
        Ok(())
    }

    fn pacer_upkeep(&mut self) -> Result<(), String> {
        let now = std::time::Instant::now();
        let ready = self.media_ready();
        let epoch = self.mls.as_ref().map(|m| m.generation);
        let restart = epoch != self.media_epoch || !ready;
        let Some((pacer, rate)) = &mut self.pacing else {
            return Ok(());
        };
        if restart {
            pacer.clear(now);
            rate.clear();
            self.media_epoch = epoch;
            self.need_keyframe = true;
        }
        let expired = pacer.expire(now);
        if !restart && !self.paused && pacer.can_probe() {
            if let Some(bitrate) = rate.probe(now) {
                pacer.probe(bitrate, now);
            }
        }
        if (restart && ready) || expired {
            self.request_keyframe()?;
        }
        Ok(())
    }

    async fn send_video(&mut self) -> Result<(), String> {
        // Drain earned tokens per wake, without imposing a timer wait on every packet.
        // The token bucket still limits bursts, and the batch bounds time away from audio.
        for _ in 0..64 {
            let (pacer, _) = self.pacing.as_mut().ok_or("Video pacer missing")?;
            let Some(packet) = pacer.pop(std::time::Instant::now()) else {
                break;
            };
            let (video, rtx) = self.video_ssrc.ok_or("Video SSRC missing")?;
            let repair = packet.original_sequence.is_some();
            let (sender, ssrc) = if repair {
                (&mut self.rtx_sender, rtx)
            } else {
                (&mut self.video_sender, video)
            };
            let sequence = sender.sequence;
            sender.sent(
                (!repair).then_some(packet.timestamp),
                packet.payload.len() + if repair { 2 } else { 0 },
            );
            let (header, payload) = rtp::video_packet(
                sequence,
                ssrc,
                self.video_codec,
                self.transport_sequence,
                &packet,
            );
            let transport_sequence = self.transport_sequence;
            self.transport_sequence = self.transport_sequence.wrapping_add(1);
            self.send_sealed(&header, &payload).await?;
            let frame_end = packet.marker && !repair;
            self.status.video_frames = self
                .status
                .video_frames
                .saturating_add(u64::from(frame_end));
            let now = std::time::Instant::now();
            let (pacer, rate) = self.pacing.as_mut().ok_or("Video pacer missing")?;
            // Sealing adds a 16 byte tag and 4 byte nonce.
            rate.sent(
                transport_sequence,
                header.len() + payload.len() + 20,
                packet.probe,
                now,
            );
            pacer.sent(sequence, packet, now);
        }
        Ok(())
    }

    async fn tick(&mut self) -> Result<(), String> {
        let now = Instant::now();
        if self.owner.is_some() {
            self.video_receiver.expire(std::time::Instant::now());
        }
        if self.users.len() == 1
            && self
                .mls
                .as_ref()
                .is_some_and(|m| !m.ready && m.pending.is_none())
        {
            self.startup = now + self.negotiation_timeout;
        }
        if (!self.status.transport_ready && now >= self.startup)
            || self
                .mls
                .as_ref()
                .and_then(|m| m.pending)
                .is_some_and(|p| now.duration_since(p.1) > DEADLINE)
        {
            return Err("Encrypted voice negotiation timed out".into());
        }
        if let Some(interval) = self.heartbeat {
            if self.ack.is_some_and(|(_, sent)| {
                now.duration_since(sent) >= interval + Duration::from_secs(5)
            }) {
                return Err("Voice heartbeat ACK timed out".into());
            }
            if now >= self.next_heartbeat && self.ack.is_none() {
                let nonce = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| "Invalid system clock")?
                    .as_millis() as u64;
                event(&mut self.ws, 3, json!({"t":nonce,"seq_ack":self.seq_ack})).await?;
                self.ack = Some((nonce, now));
                self.next_heartbeat = now + interval;
            }
        }
        if now >= self.next_report {
            let (video, rtx) = self.video_ssrc.unwrap_or((self.ssrc, self.ssrc));
            let reports = [
                (
                    &self.audio_sender,
                    &self.audio_sender,
                    self.ssrc,
                    Codec::Opus,
                ),
                (
                    &self.video_sender,
                    &self.video_sender,
                    video,
                    self.video_codec,
                ),
                (&self.rtx_sender, &self.video_sender, rtx, self.video_codec),
            ]
            .into_iter()
            .filter(|(sender, ..)| sender.packets != 0)
            .map(|(sender, mapping, ssrc, codec)| {
                let elapsed = now.duration_since(mapping.last_media).as_micros()
                    * u128::from(codec.clock())
                    / 1_000_000;
                rtp::sender_report(
                    ssrc,
                    mapping.timestamp.wrapping_add(elapsed as u32),
                    sender.packets,
                    sender.octets,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
            for report in reports {
                self.send_sealed(&report[..8], &report[8..]).await?;
            }
            self.next_report = now + Duration::from_secs(5);
        }
        if self.status.transport_ready {
            self.announce().await?;
        }
        if let Some((pacer, rate)) = &self.pacing {
            self.status.video_bitrate = pacer.encoder_bitrate(rate.bitrate());
        }
        self.report();
        Ok(())
    }

    /// Sends a changed Speaking flag and returns whether voice may be sent.
    async fn announce(&mut self) -> Result<bool, String> {
        let Mode::Voice { speaking, .. } = &mut self.mode else {
            return Ok(true);
        };
        // A mute during the write is announced too, before any send is allowed.
        while speaking.has_changed().unwrap_or(false) {
            let on = *speaking.borrow_and_update();
            event(
                &mut self.ws,
                5,
                json!({"speaking":u8::from(on),"delay":0,"ssrc":self.ssrc}),
            )
            .await?;
        }
        Ok(*speaking.borrow())
    }

    async fn send_frame(&mut self, frame: capture::Frame) -> Result<(), String> {
        let video = frame.codec != Codec::Opus;
        if video && matches!(self.mode, Mode::Voice { .. }) {
            return Err("Encoded frame does not match transport mode".into());
        }
        let limit = if video { 1024 * 1024 } else { 4096 };
        if frame.data.is_empty() || frame.data.len() > limit {
            return Err("Invalid encoded media frame size".into());
        }
        if !self.status.transport_ready {
            return Ok(());
        }
        // Paused, or capture is still switching to Discord's codec.
        if video && (self.paused || frame.codec != self.video_codec) {
            self.need_keyframe = true;
            return Ok(());
        }
        // Audio queued before a mute is dropped, and an unmute is announced before it sends.
        if !video && !self.announce().await? {
            return Ok(());
        }
        if video && frame.discontinuity {
            self.restart_video()?;
        }
        if video && self.need_keyframe {
            if !frame.keyframe {
                return Ok(());
            }
            self.need_keyframe = false;
        }
        let sender = if video {
            &mut self.video_sender
        } else {
            &mut self.audio_sender
        };
        let pts = frame.pts.nseconds();
        if let Some(previous) = sender.last_pts.filter(|previous| pts <= *previous) {
            return Err(format!(
                "Nonmonotonic media timestamp ({:?}: {previous} -> {pts} ns)",
                frame.codec
            ));
        }
        sender.last_pts = Some(pts);
        let timestamp = sender.origin.wrapping_add(
            (u128::from(pts) * u128::from(frame.codec.clock()) / 1_000_000_000) as u32,
        );
        let ssrc = if video {
            self.video_ssrc.ok_or("Video SSRC missing")?.0
        } else {
            self.ssrc
        };
        // WebRTC rewrites SPS restrictions before DAVE decryption. Make
        // that edit before authentication so receivers leave the SPS intact.
        let plain = if frame.codec == Codec::H264 {
            h264::normalize(&frame.data)?
        } else {
            Cow::Borrowed(frame.data.as_slice())
        };
        // Encrypt the whole frame before adding any RTP descriptors.
        let data = self
            .mls
            .as_mut()
            .ok_or("DAVE missing")?
            .encryptor
            .encrypt(ssrc, frame.codec, &plain)
            .map_err(|_| "DAVE frame encryption failed")?;
        if video {
            let limit = rtp::MTU - 54;
            let payloads = match frame.codec {
                Codec::H265 => h265::payloads(&data, limit)?,
                Codec::Av1 => av1::payloads(&data, frame.keyframe, limit)?,
                _ => h264::payloads(&data, limit)?,
            };
            let (pacer, _) = self.pacing.as_mut().ok_or("Video pacer missing")?;
            if !pacer.enqueue(
                timestamp,
                frame.keyframe,
                payloads,
                std::time::Instant::now(),
            ) {
                self.request_keyframe()?;
            } else if frame.keyframe {
                self.status.video_keyframes += 1;
            }
            return Ok(());
        }
        if data.len() > rtp::MTU - 32 {
            return Err("Opus frame exceeds transport MTU".into());
        }
        // Audio never waits behind the video pacer or its repair budget.
        let sender = &self.audio_sender;
        let header = rtp::header(sender.sequence, timestamp, ssrc, sender.packets == 0);
        self.send_sealed(&header, &data).await?;
        self.audio_sender.sent(Some(timestamp), data.len());
        Ok(())
    }

    async fn on_udp(&mut self, packet: &[u8]) -> Result<(), String> {
        if !self.discovered {
            return self.on_discovery(packet).await;
        }
        let Some(cipher) = &self.cipher else {
            return Ok(());
        };
        if packet.len() >= 8 && packet[0] >> 6 == 2 && (192..=223).contains(&packet[1]) {
            if let Some(plain) = cipher.open(packet, 8) {
                let mut rtcp = packet[..8].to_vec();
                rtcp.extend(plain);
                self.on_rtcp(&rtcp)?;
            }
            return Ok(());
        }
        let Some(clear) = rtp::clear_len(packet) else {
            return Ok(());
        };
        if self.owner.is_some() {
            let Some((audio, video, rtx)) = self.watched else {
                return Ok(());
            };
            let expected = match packet[1] & 127 {
                120 => audio,
                rtp::H264_PT => video,
                rtp::H264_RTX_PT => rtx,
                _ => return Ok(()),
            };
            if packet[8..12] != expected.to_be_bytes() {
                return Ok(());
            }
        }
        let Some(plain) = cipher.open(packet, clear) else {
            return Ok(());
        };
        match self.mode {
            Mode::Watch { .. } => self.on_watched(packet, clear, &plain),
            Mode::GoLive { .. } if matches!(packet[1] & 127, rtp::H264_PT | rtp::H264_RTX_PT) => {
                Err("Incoming stream video is unsupported by sender transport".into())
            }
            Mode::GoLive { .. } => Ok(()),
            Mode::Voice { .. } => {
                let Some(packet) = rtp::opus(&packet[..clear], &plain) else {
                    return Ok(());
                };
                let Some(user) = self.ssrc_users.get(&packet.ssrc) else {
                    return Ok(());
                };
                let Some(decryptor) = self.mls.as_mut().and_then(|m| m.decryptors.get_mut(user))
                else {
                    return Ok(());
                };
                let Ok(data) = decryptor.decrypt(Codec::Opus, packet.payload) else {
                    return Ok(());
                };
                self.deliver(ReceivedFrame {
                    user: user.parse().unwrap_or_default(),
                    timestamp: packet.timestamp,
                    data,
                    codec: Codec::Opus,
                    keyframe: false,
                    sender_time: None,
                    epoch: 0,
                })
            }
        }
    }

    async fn on_discovery(&mut self, packet: &[u8]) -> Result<(), String> {
        if packet.len() != 74
            || packet[..4] != [0, 2, 0, 70]
            || packet[4..8] != self.ssrc.to_be_bytes()
        {
            return Ok(());
        }
        let address = &packet[8..72];
        let end = address
            .iter()
            .position(|b| *b == 0)
            .ok_or("Invalid UDP discovery response")?;
        let address = std::str::from_utf8(&address[..end])
            .ok()
            .and_then(|address| address.parse::<IpAddr>().ok())
            .ok_or("Invalid UDP discovery address")?;
        let port = u16::from_be_bytes([packet[72], packet[73]]);
        if port == 0 {
            return Err("Invalid UDP discovery port".into());
        }
        let mut codecs =
            vec![json!({"name":"opus","type":"audio","priority":1000,"payload_type":120})];
        if self.mode.video() {
            let encode: &[Codec] = match &self.mode {
                Mode::GoLive { codecs, .. } => codecs,
                _ => &[],
            };
            // Discord prefers lower priorities. Playback decodes H.264 only, so
            // streams watched in Suncord fall back to it.
            for (index, codec) in [Codec::Av1, Codec::H265, Codec::H264]
                .into_iter()
                .filter(|codec| *codec == Codec::H264 || encode.contains(codec))
                .enumerate()
            {
                let (media, rtx) = rtp::payload_types(codec);
                codecs.push(json!({"name":codec_name(codec),"type":"video","priority":(index + 1) * 1000,"payload_type":media,"rtx_payload_type":rtx,"encode":encode.contains(&codec),"decode":codec == Codec::H264}));
            }
        }
        event(
            &mut self.ws,
            1,
            json!({"protocol":"udp","data":{"address":address.to_string(),"port":port,"mode":rtp::MODE},"codecs":codecs}),
        )
        .await?;
        self.discovered = true;
        Ok(())
    }

    fn on_rtcp(&mut self, rtcp: &[u8]) -> Result<(), String> {
        if self.owner.is_some() {
            let Some((audio, video, _)) = self.watched else {
                return Ok(());
            };
            let Some(feedback) = rtp::feedback(rtcp, video) else {
                return Ok(());
            };
            for sr in feedback.sender_reports {
                if sr.ssrc != audio && sr.ssrc != video {
                    continue;
                }
                if sr.ssrc == video && !self.sender_reports.contains_key(&video) {
                    // A pre-clock keyframe was not playable. Keep requesting
                    // until a keyframe with an SR mapping can be delivered.
                    self.video_receiver.clear();
                    self.pli_pending = true;
                }
                self.sender_reports.insert(
                    sr.ssrc,
                    ReceiverReport {
                        ntp: sr.ntp,
                        timestamp: sr.timestamp,
                        received: Instant::now(),
                    },
                );
            }
            return Ok(());
        }
        let Some((video, _)) = self.video_ssrc else {
            return Ok(());
        };
        let Some(feedback) = rtp::feedback(rtcp, video) else {
            return Ok(());
        };
        self.status.video_plis += u64::from(feedback.keyframe);
        self.status.video_nacks += feedback.nacks.len() as u64;
        self.status.video_feedback += feedback.transport.len() as u64;
        if feedback.keyframe
            && !self.need_keyframe
            && !self
                .pacing
                .as_ref()
                .is_some_and(|(pacer, _)| pacer.has_keyframe(std::time::Instant::now()))
        {
            // Repeated PLIs must not discard the keyframe already in flight or its repairs.
            self.request_keyframe()?;
        }
        let now = std::time::Instant::now();
        let (pacer, rate) = self.pacing.as_mut().ok_or("Video pacer missing")?;
        pacer.request(&feedback.nacks, now);
        let mut changed = feedback
            .receiver_bitrate
            .and_then(|bitrate| rate.receiver(bitrate, now));
        for report in &feedback.transport {
            changed = rate.feedback(report, now).or(changed);
        }
        self.status.receiver_bitrate = rate.receiver_bitrate(now).unwrap_or_default();
        self.set_bitrate(changed);
        Ok(())
    }

    /// Pauses the stream at quality 0 and otherwise caps its rate. Like Discord's
    /// client, a smaller wanted pixel count scales the rate, never below 0.02 bits
    /// per pixel per frame. Without one, the quality percentage scales it.
    fn on_sink_wants(&mut self, d: &Value) -> Result<(), String> {
        let (Mode::GoLive { settings, .. }, Some((video, _))) = (&self.mode, self.video_ssrc)
        else {
            return Ok(());
        };
        let settings = *settings;
        let key = video.to_string();
        let Some(quality) = d[&key].as_u64().or_else(|| d["any"].as_u64()) else {
            return Ok(());
        };
        let paused = quality == 0;
        if paused != self.paused {
            self.paused = paused;
            if paused {
                if let Some((pacer, _)) = &mut self.pacing {
                    pacer.clear(std::time::Instant::now());
                }
            } else {
                self.request_keyframe()?;
            }
        }
        let pixels = f64::from(settings.width) * f64::from(settings.height);
        let share = d["pixelCounts"][&key]
            .as_f64()
            .filter(|count| count.is_finite() && *count > 0.0)
            .map_or(quality.min(100) as f64 / 100.0, |count| {
                (count / pixels).min(1.0)
            });
        let cap =
            (f64::from(settings.bitrate) * share).max(0.02 * pixels * f64::from(settings.fps));
        let changed = self
            .pacing
            .as_mut()
            .and_then(|(_, rate)| rate.set_cap(cap as u32, std::time::Instant::now()));
        self.set_bitrate(changed);
        Ok(())
    }

    /// Applies a changed send rate to the pacer and, through status, the encoder.
    fn set_bitrate(&mut self, bitrate: Option<u32>) {
        let (Some(bitrate), Some((pacer, _))) = (bitrate, &mut self.pacing) else {
            return;
        };
        pacer.set_bitrate(bitrate, std::time::Instant::now());
        self.status.video_bitrate = pacer.encoder_bitrate(bitrate);
        self.report();
    }

    fn on_watched(&mut self, packet: &[u8], clear: usize, plain: &[u8]) -> Result<(), String> {
        let Some((audio, video, _)) = self.watched.filter(|_| self.media_ready()) else {
            return Ok(());
        };
        let marker = packet[1] & 128 != 0;
        let payload_type = packet[1] & 127;
        let Some(rtp) = rtp::media(&packet[..clear], plain) else {
            return Ok(());
        };
        let mut sequence = rtp.sequence;
        let mut payload = rtp.payload;
        let codec = if payload_type == 120 {
            Codec::Opus
        } else {
            Codec::H264
        };
        if payload_type == rtp::H264_RTX_PT {
            if payload.len() < 3 {
                return Ok(());
            }
            sequence = u16::from_be_bytes([payload[0], payload[1]]);
            payload = &payload[2..];
        }
        // Reassemble video before DAVE authenticates the whole frame.
        let assembled = if codec == Codec::H264 {
            match self.video_receiver.push(
                sequence,
                rtp.timestamp,
                marker,
                payload,
                std::time::Instant::now(),
            ) {
                Ok(Some(frame)) => Some(frame),
                Ok(None) => return Ok(()),
                Err(_) => {
                    self.video_receiver.clear();
                    return Ok(());
                }
            }
        } else {
            None
        };
        let (Some(mls), Some(owner)) = (&mut self.mls, &self.owner) else {
            return Ok(());
        };
        let Some(decryptor) = mls.decryptors.get_mut(owner) else {
            return Ok(());
        };
        let encoded = assembled
            .as_ref()
            .map_or(payload, |frame| frame.data.as_slice());
        let Ok(data) = decryptor.decrypt(codec, encoded) else {
            if codec == Codec::H264 {
                self.video_receiver.clear();
            }
            return Ok(());
        };
        let ssrc = if codec == Codec::H264 { video } else { audio };
        let timestamp = assembled
            .as_ref()
            .map_or(rtp.timestamp, |frame| frame.timestamp);
        self.deliver(ReceivedFrame {
            user: owner.parse().unwrap_or_default(),
            timestamp,
            data,
            sender_time: self
                .sender_reports
                .get(&ssrc)
                .and_then(|report| report.time(timestamp, codec.clock())),
            epoch: self.status.receive_epoch,
            codec,
            keyframe: assembled.is_some_and(|frame| frame.keyframe),
        })
    }

    async fn on_message(&mut self, message: Message) -> Result<(), String> {
        match message {
            Message::Text(text) => self.on_text(&text).await,
            Message::Binary(bytes) => self.on_binary(&bytes).await,
            Message::Ping(bytes) => send(&mut self.ws, Message::Pong(bytes)).await,
            Message::Close(_) => Err("Voice websocket closed".into()),
            _ => Ok(()),
        }
    }

    async fn on_text(&mut self, text: &str) -> Result<(), String> {
        let payload: Value = serde_json::from_str(text).map_err(|_| "Malformed voice JSON")?;
        if let Some(seq) = payload["seq"].as_u64() {
            self.seq_ack = i64::try_from(seq).map_err(|_| "Invalid voice sequence")?;
        }
        let d = &payload["d"];
        match number(&payload, "op")? {
            8 => {
                if self.heartbeat.is_some() {
                    return Err("Duplicate voice Hello".into());
                }
                let interval = d["heartbeat_interval"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 100.0 && *v <= 120_000.0)
                    .ok_or("Invalid voice heartbeat interval")?;
                self.heartbeat = Some(Duration::from_secs_f64(interval / 1000.0));
                self.next_heartbeat = Instant::now();
            }
            6 => {
                if let Some((_, sent)) = self
                    .ack
                    .filter(|(nonce, _)| d["t"].as_u64() == Some(*nonce))
                {
                    self.ack = None;
                    self.status.ping = sent.elapsed().as_millis() as u32;
                    self.report();
                }
            }
            // The codec is the one this client should send, never the watched one.
            14 => {
                if self.owner.is_some() {
                    self.video_receiver.clear();
                    self.pli_pending = true;
                } else if self.mode.video() {
                    if d["video_codec"].is_string() {
                        self.select_codec(d["video_codec"].as_str())?;
                    }
                    self.request_keyframe()?;
                }
            }
            2 => self.on_ready(d).await?,
            4 => self.on_session(d).await?,
            11 => {
                let added = d["user_ids"].as_array().ok_or("Invalid recognized users")?;
                if added.len() > USERS {
                    return Err("Recognized user limit exceeded".into());
                }
                for value in added {
                    self.users.insert(id(value)?);
                }
                if self.users.len() > USERS {
                    return Err("Recognized user limit exceeded".into());
                }
            }
            13 => {
                let user = id(&d["user_id"])?;
                if user == self.user_id {
                    return Err("Local voice user disconnected".into());
                }
                if self.owner.as_ref() == Some(&user) {
                    return Err("Watched user disconnected".into());
                }
                self.users.remove(&user);
                self.ssrc_users.retain(|_, u| *u != user);
            }
            15 => self.on_sink_wants(d)?,
            12 => {
                let Some(owner) = &self.owner else {
                    return Ok(());
                };
                if d["user_id"].as_str() != Some(owner.as_str()) {
                    return Ok(());
                }
                if !self.users.contains(owner) {
                    return Err("Unrecognized watched user".into());
                }
                let sources = watch_sources(d, self.ssrc, self.video_ssrc)?;
                if sources != self.watched {
                    self.watched = sources;
                    self.reset_receive();
                }
            }
            5 => {
                if self.owner.is_some() {
                    return Ok(());
                }
                let user = id(&d["user_id"])?;
                if !self.users.contains(&user) {
                    return Err("Unrecognized speaking user".into());
                }
                let remote = int(d, "ssrc")?;
                if remote == self.ssrc {
                    return Err("Conflicting voice SSRC".into());
                }
                self.ssrc_users.retain(|_, u| *u != user);
                if self.ssrc_users.get(&remote).is_some_and(|u| *u != user) {
                    return Err("Conflicting voice SSRC".into());
                }
                self.ssrc_users.insert(remote, user);
            }
            21 => {
                let mls = self.mls.as_ref().ok_or("DAVE transition before session")?;
                if version(d, "protocol_version", self.max_version)?
                    != mls.session.protocol_version()
                {
                    return Err("Unprepared DAVE version change".into());
                }
                if self.group.is_none() {
                    return Ok(());
                }
                let transition = int(d, "transition_id")?;
                if mls.pending.is_some() {
                    return Err("Overlapping DAVE transitions".into());
                }
                self.begin_transition(transition).await?;
            }
            22 => {
                if self.group.is_none() {
                    return Ok(());
                }
                let transition = int(d, "transition_id")?;
                self.mls
                    .as_mut()
                    .ok_or("DAVE execute before session")?
                    .execute(transition)?;
            }
            24 => {
                self.mls.as_ref().ok_or("DAVE epoch before session")?;
                let version = version(d, "protocol_version", self.max_version)?;
                if number(d, "epoch")? != 1 {
                    return Err("DAVE version migration unsupported".into());
                }
                self.new_key_package(version).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_ready(&mut self, d: &Value) -> Result<(), String> {
        if self.udp.is_some() {
            return Err("Duplicate voice Ready".into());
        }
        if !d["modes"]
            .as_array()
            .ok_or("Missing transport modes")?
            .iter()
            .any(|m| m.as_str() == Some(rtp::MODE))
        {
            return Err("AES256-GCM rtpsize unavailable".into());
        }
        self.ssrc = int(d, "ssrc")?;
        if self.ssrc == 0 {
            return Err("Invalid local SSRC".into());
        }
        if self.mode.video()
            && (self.owner.is_none() || d["streams"].as_array().is_some_and(|s| !s.is_empty()))
        {
            let streams = d["streams"]
                .as_array()
                .filter(|streams| streams.len() <= 8)
                .ok_or("Missing allocated video streams")?;
            let mut allocated = streams
                .iter()
                .filter(|stream| stream["rid"].as_str() == Some("100"));
            let stream = allocated.next().ok_or("Missing allocated screen RID")?;
            if allocated.next().is_some() {
                return Err("Duplicate allocated screen RID".into());
            }
            let video = int(stream, "ssrc")?;
            let rtx = int(stream, "rtx_ssrc")?;
            if video == 0 || rtx == 0 || video == rtx || video == self.ssrc || rtx == self.ssrc {
                return Err("Invalid allocated video SSRCs".into());
            }
            self.video_ssrc = Some((video, rtx));
        }
        let ip = d["ip"]
            .as_str()
            .ok_or("Missing voice UDP address")?
            .parse::<IpAddr>()
            .map_err(|_| "Invalid voice UDP address")?;
        let port = int(d, "port")?;
        if port == 0 || ip.is_unspecified() || ip.is_multicast() || ip.is_loopback() {
            return Err("Invalid voice UDP peer".into());
        }
        let socket = UdpSocket::bind(if ip.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })
            .await
            .map_err(|_| "Voice UDP bind failed")?;
        socket
            .connect(SocketAddr::new(ip, port))
            .await
            .map_err(|_| "Voice UDP connect failed")?;
        let mut discovery = [0; 74];
        discovery[..4].copy_from_slice(&[0, 1, 0, 70]);
        discovery[4..8].copy_from_slice(&self.ssrc.to_be_bytes());
        socket
            .send(&discovery)
            .await
            .map_err(|_| "UDP discovery send failed")?;
        self.udp = Some(socket);
        Ok(())
    }

    async fn on_session(&mut self, d: &Value) -> Result<(), String> {
        if !self.discovered || self.cipher.is_some() {
            return Err("Unexpected voice Session Description".into());
        }
        if d["mode"].as_str() != Some(rtp::MODE)
            || d["audio_codec"].as_str().is_some_and(|c| c != "opus")
        {
            return Err("Unsupported negotiated voice mode or codec".into());
        }
        if self.owner.is_none() && self.mode.video() {
            self.select_codec(d["video_codec"].as_str())?;
        }
        let key = d["secret_key"]
            .as_array()
            .filter(|a| a.len() == 32)
            .ok_or("Invalid transport key")?
            .iter()
            .map(|v| {
                v.as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or("Invalid transport key")
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.cipher = Some(rtp::Cipher::new(&key)?);
        if let Mode::GoLive { settings, .. } = &self.mode {
            let capture::VideoSettings {
                width,
                height,
                fps,
                bitrate,
                ..
            } = *settings;
            let (video, rtx) = self.video_ssrc.ok_or("Missing allocated video SSRCs")?;
            // Advertise before MLS readiness so an official viewer can join.
            event(&mut self.ws, 12, json!({"audio_ssrc":self.ssrc,"video_ssrc":video,"rtx_ssrc":rtx,"streams":[{"type":"video","rid":"100","ssrc":video,"rtx_ssrc":rtx,"active":true,"quality":100,"max_bitrate":bitrate,"max_framerate":fps,"max_resolution":{"type":"fixed","width":width,"height":height}}]})).await?;
        }
        let version = version(d, "dave_protocol_version", self.max_version)?;
        self.new_key_package(version).await
    }

    /// A fresh session clears libdave's retained roster as well as the HPKE leaf.
    /// The call's shared signing identity survives independently.
    async fn new_key_package(&mut self, version: u16) -> Result<(), String> {
        let generation = self
            .mls
            .as_ref()
            .map_or(0, |mls| mls.generation.wrapping_add(1));
        let mut mls = Mls::new(
            &self.identity,
            self.group.unwrap_or(0),
            version,
            self.external.as_deref(),
        )?;
        mls.generation = generation;
        let package = mls
            .session
            .key_package()
            .map_err(|_| "DAVE key package failed")?;
        self.mls = Some(mls);
        binary(&mut self.ws, 26, package).await
    }

    /// Invalidates a transition and restarts from a fresh key package.
    async fn recover(&mut self, transition: Option<u16>) -> Result<(), String> {
        if self.recoveries >= 3 {
            return Err("MLS recovery budget exhausted".into());
        }
        self.recoveries += 1;
        if let Some(transition) = transition {
            event(&mut self.ws, 31, json!({"transition_id":transition})).await?;
        }
        let version = self
            .mls
            .as_ref()
            .ok_or("DAVE missing")?
            .session
            .protocol_version();
        self.new_key_package(version).await
    }

    async fn begin_transition(&mut self, transition: u16) -> Result<(), String> {
        let mls = self.mls.as_mut().ok_or("DAVE missing")?;
        mls.pending = Some((transition, Instant::now()));
        if transition == 0 {
            mls.execute(0)
        } else {
            event(&mut self.ws, 23, json!({"transition_id":transition})).await
        }
    }

    async fn on_binary(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() < 4 {
            return Err("Malformed binary voice message".into());
        }
        self.seq_ack = i64::from(u16::from_be_bytes([bytes[0], bytes[1]]));
        let (op, data) = (bytes[2], &bytes[3..]);
        match op {
            25 => {
                if data.len() > 4096 || self.external.is_some() {
                    return Err("Invalid MLS external sender package".into());
                }
                self.external = Some(data.to_vec());
                if let Some(mls) = &mut self.mls {
                    mls.set_external_sender(data)?;
                }
                Ok(())
            }
            27 | 29 | 30 => self.on_mls(op, data).await,
            _ => Err("Unsupported binary voice opcode".into()),
        }
    }

    async fn on_mls(&mut self, op: u8, data: &[u8]) -> Result<(), String> {
        let mls = self.mls.as_ref().ok_or("MLS message before session")?;
        if !mls.external {
            return Err("MLS message before external sender".into());
        }
        if self.mode.video() && matches!(op, 27 | 29) {
            if let Some(candidate) = discover_group(op, data)? {
                match self.group {
                    Some(known) if known != candidate => {
                        return Err("MLS stream group changed unexpectedly".into())
                    }
                    None => {
                        self.group = Some(candidate);
                        // This message belongs to the disposable bootstrap leaf.
                        // Commit transitions need explicit invalidation, never ACK.
                        if op == 29 {
                            return self
                                .recover(Some(u16::from_be_bytes([data[0], data[1]])))
                                .await;
                        }
                        let version = mls.session.protocol_version();
                        return self.new_key_package(version).await;
                    }
                    _ => {}
                }
            }
        }
        if self.group.is_none() {
            if op == 30 {
                if data.len() < 3 {
                    return Err("Invalid bootstrap MLS welcome".into());
                }
                return self
                    .recover(Some(u16::from_be_bytes([data[0], data[1]])))
                    .await;
            }
            return Ok(());
        }
        let recognized: Vec<_> = self.users.iter().map(String::as_str).collect();
        let mls = self.mls.as_mut().ok_or("MLS message before session")?;
        if op == 27 {
            if mls.pending.is_some() {
                return Err("MLS proposals overlap transition".into());
            }
            if let Some(commit) = mls
                .session
                .process_proposals(data, &recognized)
                .map_err(|_| "MLS proposals rejected")?
            {
                binary(&mut self.ws, 28, commit).await?;
            }
            return Ok(());
        }
        if data.len() < 3 || mls.pending.is_some() {
            return Err("Invalid MLS transition".into());
        }
        let transition = u16::from_be_bytes([data[0], data[1]]);
        let result = if op == 29 {
            match mls.session.process_commit(&data[2..]) {
                Ok(Some(roster)) => Ok((roster, false)),
                Ok(None) => return Ok(()),
                Err(()) => Err(()),
            }
        } else {
            mls.session
                .process_welcome(&data[2..], &recognized)
                .map(|r| (r, true))
        };
        let Ok((roster, full)) = result else {
            event(&mut self.ws, 31, json!({"transition_id":transition})).await?;
            self.recover(None).await?;
            self.status.transport_ready = false;
            self.dave_ready = false;
            self.report();
            self.startup = Instant::now() + DEADLINE;
            return Ok(());
        };
        mls.roster(roster, full, &self.users, &self.user_id)?;
        self.begin_transition(transition).await
    }

    async fn update_ready(&mut self) -> Result<(), String> {
        let ready = self.group.is_some() && self.mls.as_ref().is_some_and(|m| m.ready);
        if !ready && self.dave_ready {
            self.startup = Instant::now() + self.negotiation_timeout;
        }
        let transport =
            ready && self.cipher.is_some() && self.discovered && self.heartbeat.is_some();
        if transport && !self.status.transport_ready {
            let speaking = match &mut self.mode {
                Mode::Voice { speaking, .. } => Some(u8::from(*speaking.borrow_and_update())),
                Mode::GoLive { audio, .. } => {
                    let audio = audio.is_some();
                    self.request_keyframe()?;
                    audio.then_some(2)
                }
                Mode::Watch { .. } => None,
            };
            if let Some(speaking) = speaking {
                event(
                    &mut self.ws,
                    5,
                    json!({"speaking":speaking,"delay":0,"ssrc":self.ssrc}),
                )
                .await?;
            }
        }
        if ready != self.dave_ready || transport != self.status.transport_ready {
            self.dave_ready = ready;
            self.status.transport_ready = transport;
            self.report();
        }
        Ok(())
    }
}
