use super::auth::AuthStore;
use super::cache::CacheStore;
use super::media_signaling::{
    self, MediaEvent, MediaEventKind, MediaState, SignalingError, StreamCommand, VoiceAttempt,
    VoiceCommand, VoiceTarget,
};
use super::models::{cdn_image, guild_icon_url, items, ChannelDto, GuildDto, Lenient};
use super::rest::{message_from_value, parse_channel, parse_user, DiscordRest, WireChannel};
use super::zlib_stream::ZlibStream;
use super::DESKTOP_UA;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, Notify};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig, Message},
};
use zeroize::Zeroizing;

/// Discord user-gateway capability flags (not bot intents).
/// Bit 9 (USER_SETTINGS_PROTO) is intentionally cleared so READY still includes
/// JSON `user_settings.guild_folders` instead of opaque protobuf blobs.
/// Bit 13 (DEBOUNCE_MESSAGE_REACTIONS) is cleared to receive individual reactions.
/// Bit 8 (AUTH_TOKEN_REFRESH) is cleared because refreshed tokens are not handled.
/// Bit 11 (PASSIVE_GUILD_UPDATE) is cleared because it replaces VOICE_STATE_UPDATE in
/// unsubscribed guilds with periodic snapshots, which made voice presence lag.
const USER_CAPABILITIES: u64 = 16381 & !(1 << 8) & !(1 << 9) & !(1 << 11) & !(1 << 13);
const MEMBER_REQUEST_CAP: usize = 1000;
const SEND_LIMIT: usize = 100;
const MEMBER_SEND_LIMIT: usize = 80;

enum Outbound {
    Text(String),
    Pong(Vec<u8>),
}

#[derive(Clone)]
struct Outbox {
    interactive: mpsc::UnboundedSender<Outbound>,
    members: mpsc::UnboundedSender<Outbound>,
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
}

#[derive(Default)]
struct MemberRequests {
    pending: VecDeque<String>,
    in_flight: HashMap<String, Vec<String>>,
    completed: VecDeque<String>,
    query: Option<String>,
    search_nonce: Option<String>,
    retry_at: Option<Instant>,
    serial: u64,
}

impl MemberRequests {
    fn requeue(&mut self, nonce: Option<&str>) {
        if let Some(nonce) = nonce {
            if let Some(users) = self.in_flight.remove(nonce) {
                self.pending.extend(users);
            }
        } else {
            self.pending
                .extend(self.in_flight.drain().flat_map(|(_, users)| users));
        }
        if nonce.is_none() || self.search_nonce.as_deref() == nonce {
            self.search_nonce = None;
        }
    }

    fn accept(&mut self, nonce: &str) {
        if self.search_nonce.as_deref() == Some(nonce) {
            self.query = None;
            self.search_nonce = None;
        }
        if let Some(users) = self.in_flight.remove(nonce) {
            self.completed.extend(users);
            let excess = self.completed.len().saturating_sub(MEMBER_REQUEST_CAP);
            self.completed.drain(..excess);
        }
    }

    fn flush(&mut self, outbound: &Outbox, guild: &str) {
        if self.retry_at.is_some_and(|until| until > Instant::now()) {
            return;
        }
        self.retry_at = None;
        if self.query.is_some() && self.search_nonce.is_none() {
            self.serial += 1;
            let nonce = self.serial.to_string();
            if send(outbound, json!({"op": 8, "d": {"guild_id": guild, "query": self.query, "limit": 100, "nonce": nonce}})).is_err() {
                return;
            }
            self.search_nonce = Some(nonce);
        }
        while !self.pending.is_empty() {
            let users: Vec<_> = self.pending.iter().take(100).cloned().collect();
            self.serial += 1;
            let nonce = self.serial.to_string();
            if send(outbound, json!({"op": 8, "d": {"guild_id": guild, "user_ids": users, "limit": 0, "nonce": nonce}})).is_err() {
                return;
            }
            self.pending.drain(..users.len());
            self.in_flight.insert(nonce, users);
        }
    }
}

pub enum GatewayEvent {
    Status(String),
    Disconnected,
    Changed,
    Paused { reason: String },
    Ready,
    MessageCreated { channel: String, user: String },
    Typing { channel: String, user: String },
}

/// Coalesces control notifications and keeps only recent transient events.
#[derive(Default)]
pub struct Events {
    pending: Mutex<(u64, VecDeque<GatewayEvent>)>,
    available: Notify,
}

impl Events {
    fn send(&self, generation: u64, event: GatewayEvent) {
        let mut pending = self.pending.lock();
        if generation < pending.0 {
            return;
        }
        if generation != pending.0 {
            *pending = (generation, VecDeque::new());
        }
        let queue = &mut pending.1;
        let transient = |event: &GatewayEvent| {
            matches!(
                event,
                GatewayEvent::MessageCreated { .. } | GatewayEvent::Typing { .. }
            )
        };
        if transient(&event) {
            if queue.iter().filter(|event| transient(event)).count() >= 128 {
                let oldest = queue.iter().position(transient).unwrap();
                if matches!(
                    queue.remove(oldest),
                    Some(GatewayEvent::MessageCreated { .. })
                ) {
                    queue.retain(|event| !matches!(event, GatewayEvent::Changed));
                    queue.push_back(GatewayEvent::Changed);
                }
            }
        } else {
            queue.retain(|old| std::mem::discriminant(old) != std::mem::discriminant(&event));
        }
        queue.push_back(event);
        self.available.notify_one();
    }

    pub async fn ready(&self) {
        loop {
            let available = self.available.notified();
            if !self.pending.lock().1.is_empty() {
                return;
            }
            available.await;
        }
    }

    pub fn recv(&self) -> Option<(u64, GatewayEvent)> {
        let mut pending = self.pending.lock();
        pending.1.pop_front().map(|event| (pending.0, event))
    }
}

struct Gate {
    outbound: Mutex<Option<Outbox>>,
    member_requests: Mutex<HashMap<String, MemberRequests>>,
    member_subscription: Mutex<Option<(String, String, usize)>>,
    events: Arc<Events>,
    token: Option<Zeroizing<String>>,
    account: Option<String>,
    cache: CacheStore,
    rest: DiscordRest,
    generation: u64,
    media_events: broadcast::Sender<MediaEvent>,
    media: Mutex<MediaState>,
    active: Mutex<bool>,
    session: Mutex<Session>,
    settings: Mutex<Option<Value>>,
    snapshot_pending: AtomicBool,
    ready_since: Mutex<Option<Instant>>,
    hydration: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Gate {
    fn emit_media(&self, media: &MediaState, kind: MediaEventKind) {
        let _ = self.media_events.send(MediaEvent {
            attempt: media.current.as_ref().map(|(attempt, _, _)| *attempt),
            kind,
        });
    }

    /// Also unpublishes the socket, so no media command reaches a closing connection.
    fn invalidate_media(&self) {
        self.outbound.lock().take();
        let mut media = self.media.lock();
        self.emit_media(&media, MediaEventKind::Invalidated);
        media.current = None;
        media.leaving = false;
        media.watched_owner = None;
        media.pending.clear();
    }

    fn emit(&self, event: GatewayEvent) {
        self.events.send(self.generation, event);
    }

    fn cancel_hydration(&self) {
        for task in self.hydration.lock().drain(..) {
            task.abort();
        }
    }

    fn track_hydration(&self, task: tokio::task::JoinHandle<()>) {
        let mut tasks = self.hydration.lock();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    fn stop(&self) {
        *self.active.lock() = false;
        self.invalidate_media();
        self.cancel_hydration();
        self.cache.clear_live_tail();
    }

    fn pause(&self, reason: String) {
        self.stop();
        if let Some(token) = &self.token {
            self.rest.pause(token, reason.clone());
        }
        self.emit(GatewayEvent::Paused { reason });
    }
}

struct Owner {
    events: Arc<Events>,
    auth: AuthStore,
    cache: CacheStore,
    rest: DiscordRest,
    generation: AtomicU64,
    media_events: broadcast::Sender<MediaEvent>,
    active: Mutex<Option<(Arc<Gate>, tokio::task::JoinHandle<()>)>>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        if let Some((gate, task)) = self.active.get_mut().take() {
            gate.stop();
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct GatewayHandle {
    owner: Arc<Owner>,
}

fn send(outbound: &Outbox, payload: Value) -> Result<(), SignalingError> {
    let queue = if payload["op"] == 8 && payload["d"]["user_ids"].is_array() {
        &outbound.members
    } else {
        &outbound.interactive
    };
    queue
        .send(Outbound::Text(payload.to_string()))
        .map_err(|_| SignalingError::GatewayUnavailable)
}

impl GatewayHandle {
    pub fn start(
        events: Arc<Events>,
        auth: AuthStore,
        cache: CacheStore,
        rest: DiscordRest,
    ) -> Self {
        Self {
            owner: Arc::new(Owner {
                events,
                auth,
                cache,
                rest,
                generation: AtomicU64::new(0),
                media_events: broadcast::channel(64).0,
                active: Mutex::new(None),
            }),
        }
    }

    pub fn generation(&self) -> u64 {
        self.owner.generation.load(Ordering::Acquire)
    }

    pub fn connect(&self) {
        let mut active = self.owner.active.lock();
        if let Some((gate, task)) = active.take() {
            gate.stop();
            task.abort();
        }
        self.owner.cache.clear_live();
        let generation = self.owner.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let gate = Arc::new(Gate {
            outbound: Mutex::new(None),
            member_requests: Mutex::new(HashMap::new()),
            member_subscription: Mutex::new(None),
            events: self.owner.events.clone(),
            token: self.owner.auth.token(),
            account: self.owner.cache.user_id(),
            cache: self.owner.cache.clone(),
            rest: self.owner.rest.clone(),
            generation,
            media_events: self.owner.media_events.clone(),
            media: Mutex::new(MediaState::default()),
            active: Mutex::new(true),
            session: Mutex::new(Session::default()),
            settings: Mutex::new(None),
            snapshot_pending: AtomicBool::new(false),
            ready_since: Mutex::new(None),
            hydration: Mutex::new(Vec::new()),
        });
        let task = tokio::spawn(run_session(gate.clone()));
        *active = Some((gate, task));
    }

    pub fn disconnect(&self) {
        let mut active = self.owner.active.lock();
        if let Some((gate, task)) = active.take() {
            gate.stop();
            task.abort();
        }
        self.owner.cache.clear_live();
        self.owner.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Native controllers only. Subscribe before joining and treat receiver lag as invalidation.
    pub fn subscribe_media(&self) -> broadcast::Receiver<MediaEvent> {
        self.owner.media_events.subscribe()
    }

    pub fn voice_active(&self) -> bool {
        self.owner
            .active
            .lock()
            .as_ref()
            .is_some_and(|(gate, _)| gate.media.lock().current.is_some())
    }

    /// New joins require READY or RESUMED. Existing media queues controls during RESUME.
    fn media<T>(
        &self,
        command: impl FnOnce(&Gate, Option<&Outbox>, &mut MediaState) -> Result<T, SignalingError>,
    ) -> Result<T, SignalingError> {
        let active = self.owner.active.lock();
        let (gate, _) = active.as_ref().ok_or(SignalingError::GatewayUnavailable)?;
        let running = gate.active.lock();
        if !*running {
            return Err(SignalingError::GatewayUnavailable);
        }
        let outbound = gate.outbound.lock();
        let mut media = gate.media.lock();
        command(gate, outbound.as_ref(), &mut media)
    }

    /// Queues an opcode 4 request. Success means queued, not connected media.
    pub fn join_voice(
        &self,
        target: VoiceTarget,
        voice: (bool, bool),
    ) -> Result<VoiceAttempt, SignalingError> {
        target.validate()?;
        self.media(|gate, outbound, media| {
            if media.current.is_some() {
                return Err(SignalingError::AlreadyActive);
            }
            send(
                outbound.ok_or(SignalingError::GatewayUnavailable)?,
                target.voice_payload(voice, false),
            )?;
            media.serial += 1;
            let attempt = VoiceAttempt {
                gateway_generation: gate.generation,
                serial: media.serial,
            };
            media.current = Some((attempt, target, voice));
            Ok(attempt)
        })
    }

    pub fn voice_command(
        &self,
        attempt: VoiceAttempt,
        command: VoiceCommand,
    ) -> Result<(), SignalingError> {
        self.media(|gate, outbound, media| {
            let (_, target, voice) = media.attempt(attempt)?;
            let leave = matches!(command, VoiceCommand::Leave);
            let voice = match command {
                VoiceCommand::SetVoice(mute, deaf) => (mute, deaf),
                VoiceCommand::Leave => *voice,
            };
            let payload = target.voice_payload(voice, leave);
            send_media(outbound, media, payload)?;
            if leave {
                gate.emit_media(media, MediaEventKind::Invalidated);
                media.leaving = true;
                media.watched_owner = None;
            } else if let Some((_, _, current)) = media.current.as_mut() {
                *current = voice;
            }
            Ok(())
        })
    }

    pub fn stream_command(
        &self,
        attempt: VoiceAttempt,
        command: StreamCommand,
    ) -> Result<(), SignalingError> {
        self.media(|gate, outbound, media| {
            let (_, target, _) = media.attempt(attempt)?;
            let user = gate
                .cache
                .user_id()
                .ok_or(SignalingError::GatewayUnavailable)?;
            let payload = command.payload(target, &user)?;
            match command {
                StreamCommand::Watch(_) if media.watched_owner.is_some() => {
                    return Err(SignalingError::AlreadyActive);
                }
                StreamCommand::StopWatching(owner) if media.watched_owner != Some(owner) => {
                    return Err(SignalingError::StaleAttempt);
                }
                _ => {}
            }
            send_media(outbound, media, payload)?;
            match command {
                StreamCommand::Watch(owner) => media.watched_owner = Some(owner),
                StreamCommand::StopWatching(_) => media.watched_owner = None,
                _ => {}
            }
            Ok(())
        })
    }

    /// Sends a request on the authenticated socket when one is open.
    fn request(&self, send: impl FnOnce(&Gate, &Outbox)) {
        let active = self.owner.active.lock();
        let Some((gate, _)) = active.as_ref() else {
            return;
        };
        if !*gate.active.lock() {
            return;
        }
        if let Some(outbound) = gate.outbound.lock().as_ref() {
            send(gate, outbound);
        };
    }

    pub fn subscribe_members(&self, guild: &str, channel: &str, index: usize) {
        if guild.is_empty() || channel.is_empty() {
            return;
        }
        self.request(|gate, outbound| {
            let start = index / 100 * 100;
            let subscription = (guild.to_owned(), channel.to_owned(), start);
            let mut current = gate.member_subscription.lock();
            if current.as_ref() == Some(&subscription) {
                return;
            }
            if let Some((previous, _, _)) = current
                .as_ref()
                .filter(|(previous, _, _)| previous != guild)
            {
                let _ = send(outbound, json!({"op":14,"d":{"guild_id":previous,"channels":{},"typing":false,"activities":false,"threads":false}}));
            }
            let ranges = if start == 0 {
                vec![[0, 99], [100, 199]]
            } else {
                vec![[0, 99], [start, start + 99], [start + 100, start + 199]]
            };
            let mut channels = serde_json::Map::new();
            channels.insert(channel.to_owned(), json!(ranges));
            let _ = send(outbound, json!({"op":14,"d":{"guild_id":guild,"typing":true,"activities":true,"threads":true,"channels":channels}}));
            *current = Some(subscription);
        });
    }

    pub fn search_members(&self, guild: &str, query: &str) {
        if !super::snowflake(guild) || query.chars().count() > 100 {
            return;
        }
        if let Some((gate, _)) = self.owner.active.lock().as_ref() {
            if *gate.active.lock() {
                {
                    let mut requests = gate.member_requests.lock();
                    let requests = requests.entry(guild.to_owned()).or_default();
                    requests.query = (!query.is_empty()).then(|| query.to_owned());
                    requests.search_nonce = None;
                }
                gate.flush_member_requests();
            }
        }
    }

    pub fn request_members(&self, guild: &str, users: &[String]) {
        if let Some((gate, _)) = self.owner.active.lock().as_ref() {
            if *gate.active.lock() {
                gate.request_members(guild, users);
            }
        }
    }
}

impl Gate {
    fn flush_member_requests(&self) {
        if let Some(outbound) = self.outbound.lock().as_ref() {
            for (guild, requests) in self.member_requests.lock().iter_mut() {
                requests.flush(outbound, guild);
            }
        }
    }

    fn request_members(&self, guild: &str, users: &[String]) {
        if !super::snowflake(guild) {
            return;
        }
        {
            let mut requests = self.member_requests.lock();
            let requests = requests.entry(guild.to_owned()).or_default();
            let mut count =
                requests.pending.len() + requests.in_flight.values().map(Vec::len).sum::<usize>();
            for user in users {
                // Bound queued and unacknowledged IDs together.
                if count >= MEMBER_REQUEST_CAP {
                    break;
                }
                if super::snowflake(user)
                    && !requests.pending.contains(user)
                    && !requests.in_flight.values().any(|ids| ids.contains(user))
                    && !requests.completed.contains(user)
                {
                    requests.pending.push_back(user.clone());
                    count += 1;
                }
            }
        }
        self.flush_member_requests();
    }

    fn member_rate_limited(&self, data: &Value) {
        if data["opcode"].as_u64() != Some(8) {
            return;
        }
        let Some(guild) = data["meta"]["guild_id"].as_str() else {
            return;
        };
        let Some(until) = data["retry_after"]
            .as_f64()
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
            .and_then(|delay| Instant::now().checked_add(delay))
        else {
            return;
        };
        let mut requests = self.member_requests.lock();
        if let Some(requests) = requests.get_mut(guild) {
            requests.retry_at = Some(requests.retry_at.map_or(until, |old| old.max(until)));
            requests.requeue(data["meta"]["nonce"].as_str());
        }
    }

    fn update_voice(&self, guild: &str, states: &Value) {
        let missing = self.cache.update_voice(guild, states);
        self.request_members(guild, &missing);
    }
}

async fn run_session(gate: Arc<Gate>) {
    let mut delay = 5u64;
    let mut failures = 0;
    loop {
        if !*gate.active.lock() {
            return;
        }
        if gate.token.is_none() {
            emit_status(&gate, "Signed out");
            return;
        }
        emit_status(&gate, "Opening the gateway");
        match connect_once(gate.clone()).await {
            Ok(Stop::Stopped) => return,
            Ok(Stop::Paused(reason)) => {
                eprintln!("discord gateway stopped: {reason}");
                gate.pause(reason);
                return;
            }
            result @ (Ok(Stop::Reconnect | Stop::Retry) | Err(_)) => {
                if !*gate.active.lock() {
                    return;
                }
                let expected = matches!(result, Ok(Stop::Reconnect));
                if let Err(error) = result {
                    eprintln!("discord gateway connection failed: {error}");
                }
                emit_status(&gate, "Reconnecting");
                if gate
                    .ready_since
                    .lock()
                    .take()
                    .is_some_and(|ready| ready.elapsed() >= Duration::from_secs(60))
                {
                    delay = 5;
                    failures = 0;
                }
                if !expected {
                    failures += 1;
                }
                if failures >= 5 {
                    gate.stop();
                    gate.emit(GatewayEvent::Disconnected);
                    return;
                }
                let jitter = Duration::from_millis(u64::from(rand::random::<u16>()) % 1000);
                tokio::time::sleep(Duration::from_secs(delay) + jitter).await;
                delay = (delay * 2).min(60);
            }
        }
    }
}

enum Stop {
    Reconnect,
    Retry,
    Stopped,
    Paused(String),
}

async fn connect_once(gate: Arc<Gate>) -> Result<Stop, String> {
    let Some(token) = gate.token.as_ref() else {
        return Ok(Stop::Stopped);
    };
    if let Some(reason) = gate.rest.paused_reason(token) {
        return Ok(Stop::Paused(reason));
    }
    let (resume, url) = {
        let session = gate.session.lock();
        (
            session.id.is_some(),
            gateway_url(session.resume_url.as_deref())?,
        )
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
    let connection = tokio::select! {
        biased;
        reason = gate.rest.wait_until_paused(token) => return Ok(Stop::Paused(reason)),
        result = async {
            tokio::join!(
                tokio::time::timeout(Duration::from_secs(20), connect_async_with_config(request, Some(config), false)),
                tokio::time::timeout(Duration::from_secs(20), async {
                    if resume { None } else { gate.rest.user_settings(token).await.ok() }
                })
            )
        } => {
            let (connection, settings) = result;
            (connection.map_err(|_| "Gateway handshake timed out".to_string())?, settings.ok().flatten())
        }
    };
    let (connection, settings) = connection;
    if !resume {
        if let (Some(user), Some(settings)) = (gate.cache.user_id(), settings.as_ref()) {
            gate.cache.update_presence(&user, settings);
        }
        *gate.settings.lock() = settings;
    }
    let (socket, _) = match connection {
        Ok(socket) => socket,
        Err(tokio_tungstenite::tungstenite::Error::Http(response))
            if matches!(response.status().as_u16(), 401 | 403 | 429) =>
        {
            return Ok(Stop::Paused(format!(
                "Discord refused the gateway (HTTP {}). Requests stopped. Open official Discord.",
                response.status().as_u16()
            )));
        }
        Err(err) => return Err(err.to_string()),
    };
    let (mut write, mut read) = socket.split();
    let (interactive, mut out_rx) = mpsc::unbounded_channel();
    let (members, mut member_rx) = mpsc::unbounded_channel();
    let out_tx = Outbox {
        interactive,
        members,
    };
    // Heartbeats and pongs skip the send budget so a backlog never looks like a dead connection.
    let (urgent_tx, mut urgent_rx) = mpsc::unbounded_channel();
    let missed = Arc::new(Notify::new());
    let write_failed = missed.clone();
    let mut tasks = SocketTasks {
        gate: gate.clone(),
        writer: None,
        heartbeat: None,
    };
    tasks.writer = Some(tokio::spawn(async move {
        // Discord closes the gateway past 120 sends a minute, so ordinary sends keep headroom.
        let mut recent = VecDeque::new();
        loop {
            while recent.front().is_some_and(|(sent, _): &(Instant, bool)| {
                sent.elapsed() >= Duration::from_secs(60)
            }) {
                recent.pop_front();
            }
            let member_sends = recent.iter().filter(|(_, member)| *member).count();
            let wait = (recent.len() >= SEND_LIMIT || member_sends >= MEMBER_SEND_LIMIT)
                .then(|| recent[0].0 + Duration::from_secs(60));
            // Alternate ready lanes while reserving twenty sends for interactive commands.
            let member_turn = recent.back().is_some_and(|(_, member)| !member) || out_rx.is_empty();
            let (outbound, member) = tokio::select! {
                biased;
                Some(urgent) = urgent_rx.recv() => (urgent, None),
                _ = tokio::time::sleep_until(wait.unwrap_or_else(Instant::now).into()), if wait.is_some() => continue,
                Some(outbound) = member_rx.recv(), if recent.len() < SEND_LIMIT && member_sends < MEMBER_SEND_LIMIT && member_turn => {
                    (outbound, Some(true))
                }
                Some(outbound) = out_rx.recv(), if recent.len() < SEND_LIMIT => {
                    (outbound, Some(false))
                }
                else => break,
            };
            let message = match outbound {
                Outbound::Text(text) => Message::Text(text.into()),
                Outbound::Pong(payload) => Message::Pong(payload.into()),
            };
            if write.send(message).await.is_err() {
                write_failed.notify_one();
                break;
            }
            if let Some(member) = member {
                recent.push_back((Instant::now(), member));
            }
        }
    }));
    let acked = Arc::new(AtomicBool::new(true));
    let mut zlib = ZlibStream::new();
    let mut stop = Stop::Retry;
    let mut hello_received = false;
    let mut startup_deadline = tokio::time::Instant::now() + Duration::from_secs(10);

    loop {
        if !*gate.active.lock() {
            stop = Stop::Stopped;
            break;
        }
        let member_retry = gate.outbound.lock().as_ref().and_then(|_| {
            gate.member_requests
                .lock()
                .values()
                .filter_map(|requests| requests.retry_at)
                .min()
        });
        let incoming = tokio::select! {
            biased;
            reason = gate.rest.wait_until_paused(token) => {
                stop = Stop::Paused(reason);
                break;
            }
            _ = tokio::time::sleep_until(startup_deadline), if gate.ready_since.lock().is_none() => {
                eprintln!("discord gateway authentication timed out");
                break;
            }
            _ = missed.notified() => {
                eprintln!("discord gateway heartbeat missed");
                stop = Stop::Retry;
                break;
            }
            _ = async {
                if let Some(until) = member_retry {
                    tokio::time::sleep_until(until.into()).await;
                }
            }, if member_retry.is_some() => {
                gate.flush_member_requests();
                continue;
            }
            message = read.next() => message,
        };
        let Some(message) = incoming else {
            eprintln!("discord gateway socket ended");
            break;
        };
        let message = match message {
            Ok(message) => message,
            Err(err) => {
                eprintln!("discord gateway read error: {err}");
                break;
            }
        };
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => match zlib.push(&bytes) {
                Ok(Some(decoded)) => String::from_utf8(decoded).unwrap_or_default(),
                Ok(None) => continue, // still waiting on 00 00 FF FF
                Err(err) => {
                    eprintln!("discord gateway zlib error: {err}");
                    break;
                }
            },
            Message::Ping(payload) => {
                let _ = urgent_tx.send(Outbound::Pong(payload.to_vec()));
                continue;
            }
            Message::Close(frame) => {
                let code = frame
                    .as_ref()
                    .map(|frame| u16::from(frame.code))
                    .unwrap_or(0);
                let reason = frame
                    .as_ref()
                    .map(|frame| frame.reason.to_string())
                    .unwrap_or_default();
                eprintln!("discord gateway closed: code={code} reason={reason}");
                if code == 4004 {
                    stop = Stop::Paused(
                        "Discord rejected this session. Requests stopped. Open official Discord."
                            .into(),
                    );
                } else if matches!(code, 4001 | 4002 | 4005 | 4008 | 4010..=4014) {
                    stop = Stop::Paused(format!("Discord stopped the gateway (code {code}). Requests stopped. Open official Discord."));
                } else if matches!(code, 4003 | 4007 | 4009) {
                    gate.invalidate_media();
                    // Bad identify / encoding — clear resume so the next try is a fresh IDENTIFY.
                    *gate.session.lock() = Session::default();
                }
                break;
            }
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("discord gateway json error: {err}");
                continue;
            }
        };
        let active = gate.active.lock();
        if !*active {
            return Ok(Stop::Stopped);
        }
        let op = value.get("op").and_then(|op| op.as_u64()).unwrap_or(99);
        if let Some(seq) = value.get("s").and_then(|seq| seq.as_u64()) {
            gate.session.lock().seq = Some(seq);
        }
        match op {
            10 => {
                if !hello_received {
                    hello_received = true;
                    startup_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                }
                let interval = value
                    .pointer("/d/heartbeat_interval")
                    .and_then(|interval| interval.as_u64())
                    .unwrap_or(41_250);
                if let Some(task) = tasks.heartbeat.take() {
                    task.abort();
                }
                tasks.heartbeat = Some(spawn_heartbeat(
                    gate.clone(),
                    urgent_tx.clone(),
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
            1 => {
                let _ = urgent_tx.send(Outbound::Text(
                    json!({ "op": 1, "d": gate.session.lock().seq }).to_string(),
                ));
            }
            11 => {
                acked.store(true, Ordering::Release);
            }
            7 => {
                stop = Stop::Reconnect;
                break;
            }
            9 => {
                let resumable = value
                    .get("d")
                    .and_then(|flag| flag.as_bool())
                    .unwrap_or(false);
                eprintln!("discord gateway invalid session resumable={resumable}");
                if !resumable {
                    gate.invalidate_media();
                    *gate.session.lock() = Session::default();
                }
                break;
            }
            0 => {
                let ready = matches!(value["t"].as_str(), Some("READY" | "RESUMED"));
                if value["t"] == "READY" {
                    gate.invalidate_media();
                }
                if ready {
                    let mut outbound = gate.outbound.lock();
                    let mut media = gate.media.lock();
                    for payload in media.pending.drain(..) {
                        send(&out_tx, payload).map_err(|error| error.to_string())?;
                    }
                    *outbound = Some(out_tx.clone());
                }
                dispatch(&gate, value);
                if ready {
                    gate.flush_member_requests();
                }
            }
            _ => {}
        }
    }

    Ok(stop)
}

// Keep existing media controllable during RESUME, including leaving while offline.
fn send_media(
    outbound: Option<&Outbox>,
    media: &mut MediaState,
    payload: Value,
) -> Result<(), SignalingError> {
    if let Some(outbound) = outbound {
        send(outbound, payload)
    } else if media.pending.len() < 32 {
        media.pending.push_back(payload);
        Ok(())
    } else {
        Err(SignalingError::GatewayUnavailable)
    }
}

struct SocketTasks {
    gate: Arc<Gate>,
    writer: Option<tokio::task::JoinHandle<()>>,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for SocketTasks {
    fn drop(&mut self) {
        // Voice and stream transports survive a resumable user-gateway reset.
        self.gate.outbound.lock().take();
        if *self.gate.active.lock() {
            self.gate.cache.clear_live_tail();
        }
        for requests in self.gate.member_requests.lock().values_mut() {
            requests.requeue(None);
            requests.completed.clear();
        }
        *self.gate.member_subscription.lock() = None;
        self.gate.cancel_hydration();
        if let Some(task) = self.writer.take() {
            task.abort();
        }
        if let Some(task) = self.heartbeat.take() {
            task.abort();
        }
    }
}

fn spawn_heartbeat(
    gate: Arc<Gate>,
    outbound: mpsc::UnboundedSender<Outbound>,
    interval: u64,
    missed: Arc<Notify>,
    acked: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let jitter = interval * u64::from(rand::random::<u16>() % 1000) / 1000;
        tokio::time::sleep(Duration::from_millis(jitter.max(1))).await;
        let mut waiting = false;
        loop {
            if !*gate.active.lock() {
                return;
            }
            if waiting && !acked.load(Ordering::Acquire) {
                missed.notify_one();
                return;
            }
            let seq = gate.session.lock().seq;
            let payload = json!({ "op": 1, "d": seq }).to_string();
            acked.store(false, Ordering::Release);
            if outbound.send(Outbound::Text(payload)).is_err() {
                missed.notify_one();
                return;
            }
            waiting = true;
            tokio::time::sleep(Duration::from_millis(interval.max(1))).await;
        }
    })
}

fn send_identify(outbound: &Outbox, gate: &Gate) {
    let Some(token) = gate.token.as_ref() else {
        return;
    };
    // User accounts must IDENTIFY with capabilities + browser properties.
    // Bot-style `intents` never receive READY for a normal Discord login token.
    let mut payload = json!({
        "op": 2,
        "d": {
            "token": token.as_str(),
            "capabilities": USER_CAPABILITIES,
            "properties": super::client_properties(),
            "compress": false,
            "client_state": {
                "guild_versions": {}
            }
        }
    });
    if let Some(presence) = gate
        .settings
        .lock()
        .as_ref()
        .and_then(|settings| own_presence(gate, settings))
    {
        payload["d"]["presence"] = presence;
    }
    let _ = send(outbound, payload);
}

fn own_presence(gate: &Gate, settings: &Value) -> Option<Value> {
    let status = settings["status"]
        .as_str()
        .filter(|status| matches!(*status, "online" | "idle" | "dnd" | "invisible"))?;
    let mut activities = Vec::new();
    if let Some(custom) = gate
        .account
        .as_deref()
        .and_then(|user| gate.cache.presence(user).custom_status)
    {
        activities.push(json!({"name": "Custom Status", "type": 4, "state": custom.text, "emoji": custom.emoji}));
    }
    Some(json!({"status": status, "since": 0, "activities": activities, "afk": false}))
}

fn send_resume(outbound: &Outbox, gate: &Gate) {
    let Some(token) = gate.token.as_ref() else {
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
    let _ = send(outbound, payload);
}

fn dispatch(gate: &Arc<Gate>, mut value: Value) {
    let data = value.get_mut("d").map(Value::take).unwrap_or_default();
    let kind = value["t"].as_str().unwrap_or("");
    if matches!(kind, "MESSAGE_CREATE" | "MESSAGE_UPDATE") {
        if let (Some(guild), Some(user), Some(member)) = (
            data["guild_id"].as_str(),
            data["author"]["id"].as_str(),
            data.get("member"),
        ) {
            gate.cache.update_member(guild, user, member);
        }
    }
    if kind == "VOICE_STATE_UPDATE" {
        // Calls outside servers share one empty guild key.
        let guild = data["guild_id"].as_str().unwrap_or_default();
        if let Some(member) = data.get("member") {
            update_member(gate, &data["guild_id"], member);
        }
        gate.update_voice(guild, &data);
        schedule_snapshot(gate);
    }
    if let Some(event) = media_signaling::parse_event(kind, &data) {
        let session = gate.session.lock().id.clone();
        let mut media = gate.media.lock();
        if let (Some((_, target, _)), Some(user)) = (&media.current, gate.cache.user_id()) {
            if event.matches_target(target, &user, media.watched_owner) {
                // Joining from another client moves the voice session there, which also ends ours.
                let left = matches!(&event, MediaEventKind::VoiceState(state) if state.channel_id.is_none() || session.as_deref() != Some(state.session_id.as_str()));
                let clear_watch = match &event {
                    MediaEventKind::VoiceState(state) => {
                        state.guild_id != target.guild_id
                            || state.channel_id.as_ref() != Some(&target.channel_id)
                    }
                    MediaEventKind::StreamDelete(stream) => {
                        media.watched_owner.is_some_and(|owner| {
                            target
                                .stream_key(&owner.to_string())
                                .is_ok_and(|key| key == stream.stream_key)
                        })
                    }
                    _ => false,
                };
                gate.emit_media(&media, event);
                if clear_watch {
                    media.watched_owner = None;
                }
                if left {
                    media.current = None;
                    media.leaving = false;
                    media.watched_owner = None;
                    media.pending.clear();
                }
            }
        }
        return;
    }
    match kind {
        "READY" => on_ready(gate, data),
        "READY_SUPPLEMENTAL" => {
            update_presences(&gate.cache, &data["merged_presences"]);
            for (index, guild) in data["guilds"].as_array().into_iter().flatten().enumerate() {
                for member in data["merged_members"][index]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    update_member(gate, &guild["id"], member);
                }
                if let (Some(id), Some(states)) = (guild["id"].as_str(), guild.get("voice_states"))
                {
                    gate.update_voice(id, states);
                }
            }
            gate.emit(GatewayEvent::Changed);
            let positions = guild_positions(
                data.get("user_settings"),
                data.get("user_guild_settings"),
                data.get("guild_folders"),
            );
            if let Some(folders) = guild_folders(data.get("user_settings"))
                .or_else(|| guild_folders(data.get("user_guild_settings")))
                .or_else(|| guild_folders(Some(&data)))
            {
                gate.cache.set_folders(folders);
            }
            gate.cache.order_guilds(&positions);
            schedule_snapshot(gate);
        }
        "USER_SETTINGS_UPDATE" => {
            if let Some(user) = gate.cache.user_id() {
                gate.cache.update_presence(&user, &data);
                gate.emit(GatewayEvent::Changed);
            }
            if let Some(fields) = data.as_object() {
                let mut settings = gate.settings.lock();
                let saved = settings.get_or_insert_with(|| json!({}));
                for (key, value) in fields {
                    saved[key] = value.clone();
                }
                if fields.contains_key("status") || fields.contains_key("custom_status") {
                    if let (Some(presence), Some(outbound)) =
                        (own_presence(gate, saved), gate.outbound.lock().as_ref())
                    {
                        let _ = send(outbound, json!({"op": 3, "d": presence}));
                    }
                }
            }
            if let Some(folders) = guild_folders(Some(&data)) {
                gate.cache.set_folders(folders);
            }
            gate.cache
                .order_guilds(&guild_positions(Some(&data), None, None));
            schedule_snapshot(gate);
        }
        "CALL_CREATE" | "CALL_UPDATE" | "CALL_DELETE" => {
            let Some(id) = data["channel_id"].as_str() else {
                return;
            };
            let ringing = (kind != "CALL_DELETE").then(|| {
                data["ringing"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|user| user.as_str().map(str::to_owned))
                    .collect()
            });
            gate.cache.update_call(id, ringing);
            // Only CALL_CREATE lists the call's voice states.
            for state in data["voice_states"].as_array().into_iter().flatten() {
                let mut state = state.clone();
                state["channel_id"] = json!(id);
                gate.update_voice("", &state);
            }
            schedule_snapshot(gate);
        }
        // Unsubscribed guilds report their latest messages periodically.
        "CHANNEL_UNREAD_UPDATE" => {
            let Some(guild) = data["guild_id"].as_str() else {
                return;
            };
            gate.cache
                .update_last_messages(guild, &data["channel_unread_updates"]);
            schedule_snapshot(gate);
        }
        "RESUMED" => {
            *gate.ready_since.lock() = Some(Instant::now());
            emit_status(gate, "Resumed");
        }
        "PRESENCE_UPDATE" => {
            update_presences(&gate.cache, &data);
            gate.emit(GatewayEvent::Changed);
        }
        "SESSIONS_REPLACE" => {
            update_sessions(gate, &data);
            gate.emit(GatewayEvent::Changed);
        }
        "GUILD_CREATE" => on_guild(gate, &data),
        "USER_UPDATE" => {
            if data["id"].as_str() == gate.cache.user_id().as_deref() {
                if let Some(user) = parse_user(&data) {
                    gate.cache.set_user(user);
                    schedule_snapshot(gate);
                }
            }
        }
        "GUILD_DELETE" => {
            if data.get("unavailable").and_then(|value| value.as_bool()) != Some(true) {
                if let Some(id) = data.get("id").and_then(|value| value.as_str()) {
                    gate.cache.remove_guild(id);
                    gate.member_requests.lock().remove(id);
                    schedule_snapshot(gate);
                }
            }
        }
        "GUILD_UPDATE" => {
            on_guild(gate, &data);
            gate.cache.update_access(&data, kind);
        }
        "GUILD_EMOJIS_UPDATE" | "GUILD_STICKERS_UPDATE" => {
            gate.cache
                .update_expressions(&data, kind == "GUILD_STICKERS_UPDATE");
            schedule_snapshot(gate);
        }
        "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" | "GUILD_ROLE_DELETE" | "GUILD_MEMBER_UPDATE" => {
            if kind == "GUILD_MEMBER_UPDATE" {
                update_member(gate, &data["guild_id"], &data);
            }
            gate.cache.update_access(&data, kind);
            schedule_snapshot(gate);
        }
        "GUILD_MEMBER_LIST_UPDATE" => {
            for op in data["ops"].as_array().into_iter().flatten() {
                for item in op["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(op.get("item"))
                {
                    if let Some(member) = item.get("member") {
                        update_member(gate, &data["guild_id"], member);
                        if let Some(presence) = member.get("presence") {
                            if let Some(user) = member.pointer("/user/id").and_then(|v| v.as_str())
                            {
                                gate.cache.update_presence(user, presence);
                            }
                        }
                    }
                }
            }
            gate.cache.update_member_list(&data);
            schedule_snapshot(gate);
        }
        "GUILD_MEMBERS_CHUNK" => {
            if let Some(members) = data["members"].as_array() {
                for member in members {
                    update_member(gate, &data["guild_id"], member);
                }
            }
            if let (Some(guild), Some(nonce)) = (data["guild_id"].as_str(), data["nonce"].as_str())
            {
                if let Some(requests) = gate.member_requests.lock().get_mut(guild) {
                    requests.accept(nonce);
                }
            }
            schedule_snapshot(gate);
        }
        "RATE_LIMITED" => gate.member_rate_limited(&data),
        "THREAD_MEMBERS_UPDATE" => {
            if let (Some(thread), Some(user)) = (
                data.get("id").and_then(|v| v.as_str()),
                gate.cache.user_id(),
            ) {
                if data
                    .get("added_members")
                    .and_then(|v| v.as_array())
                    .is_some_and(|members| {
                        members.iter().any(|member| {
                            member.get("user_id").and_then(|v| v.as_str()) == Some(&user)
                        })
                    })
                {
                    gate.cache.thread_membership(thread, true);
                }
                if data
                    .get("removed_member_ids")
                    .and_then(|v| v.as_array())
                    .is_some_and(|members| {
                        members.iter().any(|member| member.as_str() == Some(&user))
                    })
                {
                    gate.cache.thread_membership(thread, false);
                }
                schedule_snapshot(gate);
            }
        }
        "THREAD_LIST_SYNC" => {
            #[derive(Deserialize)]
            struct Member {
                id: String,
            }
            #[derive(Deserialize)]
            struct ThreadListSync {
                guild_id: String,
                #[serde(default)]
                channel_ids: Option<Vec<String>>,
                threads: Vec<WireChannel>,
                #[serde(default, deserialize_with = "items")]
                members: Vec<Member>,
            }
            // A null filter must not turn into a full-guild sync.
            let Ok(sync) = ThreadListSync::deserialize(&data) else {
                return;
            };
            if data.get("channel_ids").is_some_and(Value::is_null) {
                return;
            }
            let members: HashSet<_> = sync.members.into_iter().map(|member| member.id).collect();
            let threads = sync
                .threads
                .into_iter()
                .map(|thread| {
                    let (mut thread, _) = thread.into_dto(Some(sync.guild_id.clone()));
                    thread.thread_joined = members.contains(&thread.id);
                    thread
                })
                .collect();
            gate.cache
                .sync_threads(&sync.guild_id, sync.channel_ids.as_deref(), threads);
            schedule_snapshot(gate);
        }
        "MESSAGE_ACK" => {
            if let (Some(channel), Some(message)) = (
                data.get("channel_id").and_then(|v| v.as_str()),
                data.get("message_id").and_then(|v| v.as_str()),
            ) {
                gate.cache.update_read_state(
                    channel,
                    message,
                    data["mention_count"]
                        .as_u64()
                        .unwrap_or(0)
                        .min(u32::MAX as u64) as u32,
                    data["manual"].as_bool().unwrap_or(false),
                );
                schedule_snapshot(gate);
            }
        }
        "TYPING_START" => {
            if let Some(user) = data.pointer("/member/user").and_then(parse_user) {
                gate.cache.remember_users([user]);
            }
            if let (Some(guild), Some(user), Some(member)) = (
                data["guild_id"].as_str(),
                data["user_id"].as_str(),
                data.get("member"),
            ) {
                gate.cache.update_member(guild, user, member);
            }
            if let (Some(channel), Some(user)) =
                (data["channel_id"].as_str(), data["user_id"].as_str())
            {
                gate.emit(GatewayEvent::Typing {
                    channel: channel.to_owned(),
                    user: user.to_owned(),
                });
            }
        }
        "MESSAGE_CREATE" => {
            if let Some(message) = message_from_value(&data) {
                let channel = message.channel_id.clone();
                let user = message.author_id.clone();
                gate.cache.push_message(message, None);
                gate.emit(GatewayEvent::MessageCreated { channel, user });
            }
        }
        "MESSAGE_UPDATE" => {
            if gate.cache.patch_message(&data).is_some() {
                gate.emit(GatewayEvent::Changed);
            }
        }
        "MESSAGE_REACTION_ADD"
        | "MESSAGE_REACTION_REMOVE"
        | "MESSAGE_REACTION_REMOVE_ALL"
        | "MESSAGE_REACTION_REMOVE_EMOJI" => {
            if gate.cache.reaction_event(&data, kind).is_some() {
                gate.emit(GatewayEvent::Changed);
            }
        }
        "MESSAGE_POLL_VOTE_ADD" | "MESSAGE_POLL_VOTE_REMOVE" => {
            if gate
                .cache
                .update_poll_vote(&data, kind == "MESSAGE_POLL_VOTE_ADD")
                .is_some()
            {
                gate.emit(GatewayEvent::Changed);
            }
        }
        "MESSAGE_DELETE" | "MESSAGE_DELETE_BULK" => {
            let ids: Vec<_> = match data.get("ids") {
                Some(ids) => ids.as_array().into_iter().flatten().collect(),
                None => data.get("id").into_iter().collect(),
            };
            let ids: Vec<_> = ids.into_iter().filter_map(Value::as_str).collect();
            if let (Some(channel), false) = (data["channel_id"].as_str(), ids.is_empty()) {
                gate.cache.remove_messages(channel, &ids);
                gate.emit(GatewayEvent::Changed);
            }
        }
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "THREAD_CREATE" | "THREAD_UPDATE" => {
            if let Some((channel, users)) = parse_channel(&data, None) {
                let dm = channel.guild_id.is_none();
                gate.cache.remember_users(users);
                gate.cache.upsert_channel(channel, dm);
                schedule_snapshot(gate);
            }
        }
        "CHANNEL_DELETE" | "THREAD_DELETE" => {
            if let Some(id) = data.get("id").and_then(|id| id.as_str()) {
                gate.cache.remove_channel(id);
                schedule_snapshot(gate);
            }
        }
        _ => {}
    }
}

fn on_ready(gate: &Arc<Gate>, data: Value) {
    #[derive(Deserialize)]
    struct Ready {
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        resume_gateway_url: Option<String>,
        #[serde(default)]
        user: Value,
        #[serde(default)]
        guilds: Vec<Value>,
        #[serde(default)]
        merged_members: Value,
        #[serde(default, deserialize_with = "items")]
        private_channels: Vec<WireChannel>,
        #[serde(default)]
        user_settings: Option<Value>,
        #[serde(default)]
        users: Value,
        #[serde(default)]
        read_state: Value,
        #[serde(default)]
        sessions: Value,
        #[serde(default)]
        merged_presences: Value,
    }
    let Ok(ready) = serde_json::from_value::<Ready>(data) else {
        return;
    };
    // A fresh session invalidates calls and voice states from the last one.
    gate.cache.clear_live();
    update_presences(&gate.cache, &ready.merged_presences);
    {
        let mut session = gate.session.lock();
        session.id = ready.session_id;
        session.resume_url = ready.resume_gateway_url;
    }
    if let Some(user) = parse_user(&ready.user) {
        gate.cache.set_user(user);
    }
    if let (Some(user), Some(settings)) = (gate.cache.user_id(), &ready.user_settings) {
        gate.cache.update_presence(&user, settings);
    }
    update_sessions(gate, &ready.sessions);
    let mut guilds = Vec::new();
    let mut channels = HashMap::new();
    let user_id = gate.cache.user_id().unwrap_or_default();
    for (index, guild) in ready.guilds.into_iter().enumerate() {
        update_presences(&gate.cache, &guild["presences"]);
        let Ok(guild) = WireGuild::deserialize(guild) else {
            continue;
        };
        let (mut guild, guild_channels) = guild.into_dto(&user_id);
        if let Some(access) = &mut guild.access {
            let members = ready.merged_members[index].as_array();
            for member in members.into_iter().flatten() {
                if let Some(id) = super::permissions::member_id(member) {
                    let mut profile = super::models::GuildMemberDto::default();
                    profile.update(member);
                    guild.members.insert(id.to_owned(), profile);
                }
            }
            let member = members.and_then(|members| {
                members
                    .iter()
                    .find(|member| super::permissions::member_id(member) == Some(user_id.as_str()))
            });
            if access.member_roles.is_none() {
                access.member_roles = member.and_then(super::permissions::member_roles);
            }
            if let Some(member) =
                member.filter(|member| member.get("communication_disabled_until").is_some())
            {
                access.communication_disabled_until = super::permissions::member_timeout(member);
                access.timeout_known = true;
            }
        }
        if let Some(guild_channels) = guild_channels {
            channels.insert(guild.id.clone(), guild_channels);
        }
        guilds.push(guild);
    }
    super::cache::sort_guilds(
        &mut guilds,
        &guild_positions(ready.user_settings.as_ref(), None, None),
    );
    gate.cache.remember_users(
        ready
            .users
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_user),
    );
    let mut users = Vec::new();
    let dms = ready
        .private_channels
        .into_iter()
        .map(|channel| {
            let (channel, recipients) = channel.into_dto(None);
            users.extend(recipients);
            channel
        })
        .collect();
    gate.cache.remember_users(users);
    gate.cache.replace_structure(guilds, channels, dms);
    if let Some(entries) = ready
        .read_state
        .get("entries")
        .unwrap_or(&ready.read_state)
        .as_array()
    {
        for entry in entries {
            if let Some(channel) = entry
                .get("id")
                .or_else(|| entry.get("channel_id"))
                .and_then(|v| v.as_str())
            {
                gate.cache.update_read_state(
                    channel,
                    entry
                        .get("last_message_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("0"),
                    entry["mention_count"]
                        .as_u64()
                        .unwrap_or(0)
                        .min(u32::MAX as u64) as u32,
                    true,
                );
            }
        }
    }
    // Establish a local baseline only when Discord supplied no read state.
    let unread: Vec<_> = gate.cache.read(|saved| {
        saved
            .channels
            .iter()
            .chain(&saved.dms)
            .filter(|channel| !saved.last_read.contains_key(&channel.id))
            .map(|channel| channel.id.clone())
            .collect()
    });
    for channel in unread {
        gate.cache.mark_read(&channel);
    }
    if let Some(folders) = guild_folders(ready.user_settings.as_ref()) {
        gate.cache.set_folders(folders);
    }
    *gate.ready_since.lock() = Some(Instant::now());
    emit_status(gate, "Live");
    gate.emit(GatewayEvent::Ready);
    // READY guilds are often `{id, unavailable:true}` stubs — hydrate names/icons via REST.
    hydrate_guild_meta(gate);
    hydrate_dms(gate);
    hydrate_emoji_settings(gate);
    // Folder layout may be missing until settings hydrate (or proto-era READY).
    hydrate_guild_order(gate);
}

fn update_presences(cache: &CacheStore, value: &Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                update_presences(cache, value);
            }
        }
        Value::Object(values) => {
            if let (Some(id), Some(_)) = (
                super::permissions::member_id(value),
                value
                    .get("status")
                    .or_else(|| value.pointer("/presence/status"))
                    .and_then(|v| v.as_str()),
            ) {
                cache.update_presence(id, value.get("presence").unwrap_or(value));
            } else {
                for value in values.values() {
                    update_presences(cache, value);
                }
            }
        }
        _ => {}
    }
}

fn update_sessions(gate: &Gate, value: &Value) {
    let (Some(user), Some(sessions)) = (gate.cache.user_id(), value.as_array()) else {
        return;
    };
    let current = gate.session.lock().id.clone();
    if let Some(session) = sessions
        .iter()
        .find(|session| session["session_id"] == "all")
        .or_else(|| {
            sessions.iter().find(|session| {
                current
                    .as_deref()
                    .is_some_and(|id| session["session_id"].as_str() == Some(id))
            })
        })
    {
        gate.cache.update_presence(&user, session);
    }
}

fn update_member(gate: &Gate, guild: &Value, member: &Value) {
    let (Some(guild), Some(id)) = (guild.as_str(), super::permissions::member_id(member)) else {
        return;
    };
    if gate.cache.user(id).is_none() {
        if let Some(user) = parse_user(&member["user"]) {
            gate.cache.remember_users([user]);
        }
    }
    gate.cache.update_member(guild, id, member);
}

fn on_guild(gate: &Arc<Gate>, data: &Value) {
    let known = |key: &str| {
        data.get("properties")
            .and_then(|properties| properties.get(key))
            .or_else(|| data.get(key))
            .is_some()
    };
    let Ok(guild) = WireGuild::deserialize(data) else {
        return;
    };
    if guild.unavailable {
        return;
    }
    let (guild, channels) = guild.into_dto(gate.cache.user_id().as_deref().unwrap_or(""));
    let icon_known = known("icon");
    // Skip pure unavailable stubs; wait for the real GUILD_CREATE / REST hydrate.
    if guild.name.is_empty()
        && guild.icon.is_none()
        && channels.as_ref().is_none_or(Vec::is_empty)
        && !icon_known
    {
        return;
    }
    gate.cache
        .upsert_guild(guild, channels, icon_known, known("banner"));
    if let (Some(id), Some(states)) = (data["id"].as_str(), data.get("voice_states")) {
        gate.update_voice(id, states);
    }
    schedule_snapshot(gate);
}

// Fetches in a tracked task and applies the result only while the gateway is still active.
fn hydrate<T, F>(
    gate: &Arc<Gate>,
    fetch: impl FnOnce(Arc<Gate>, Zeroizing<String>) -> F,
    apply: impl FnOnce(&Gate, T) + Send + 'static,
) where
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
{
    let Some(token) = gate.token.clone() else {
        return;
    };
    let fetch = fetch(gate.clone(), token);
    let task_gate = gate.clone();
    gate.track_hydration(tokio::spawn(async move {
        let gate = task_gate;
        if !*gate.active.lock() {
            return;
        }
        let Ok(value) = fetch.await else {
            return;
        };
        let active = gate.active.lock();
        if *active {
            apply(&gate, value);
        }
    }));
}

fn hydrate_emoji_settings(gate: &Arc<Gate>) {
    hydrate(
        gate,
        |gate, token| async move { gate.rest.emoji_settings(&token).await },
        |gate, (favorites, frequent)| {
            gate.cache.set_favorite_emojis(favorites);
            gate.cache.set_frequent_emojis(frequent);
            gate.emit(GatewayEvent::Changed);
        },
    );
}

fn hydrate_dms(gate: &Arc<Gate>) {
    hydrate(
        gate,
        |gate, token| async move {
            let previous = gate.cache.read(|saved| saved.dms.clone());
            Ok((previous, gate.rest.dm_channels(&token).await?))
        },
        |gate, (previous, dms)| {
            if gate.cache.replace_dms_if_unchanged(dms, &previous) {
                gate.emit(GatewayEvent::Changed);
            }
        },
    );
}

fn hydrate_guild_meta(gate: &Arc<Gate>) {
    hydrate(
        gate,
        |gate, token| async move { gate.rest.user_guilds(&token).await },
        |gate, guilds| {
            gate.cache.merge_guild_meta(guilds);
            gate.emit(GatewayEvent::Changed);
        },
    );
}

fn hydrate_guild_order(gate: &Arc<Gate>) {
    hydrate(
        gate,
        |gate, token| async move {
            let settings = gate.settings.lock().clone();
            match settings {
                Some(settings) => Ok(settings),
                None => gate.rest.user_settings(&token).await,
            }
        },
        |gate, settings| {
            if let Some(user) = gate
                .cache
                .user_id()
                .filter(|user| gate.cache.presence(user).status.is_empty())
            {
                gate.cache.update_presence(&user, &settings);
                gate.emit(GatewayEvent::Changed);
            }
            if let Some(folders) = guild_folders(Some(&settings)) {
                gate.cache.set_folders(folders);
            }
            let positions = guild_positions(Some(&settings), None, None);
            gate.cache.order_guilds(&positions);
            gate.emit(GatewayEvent::Changed);
        },
    );
}

fn schedule_snapshot(gate: &Arc<Gate>) {
    if gate.snapshot_pending.swap(true, Ordering::AcqRel) {
        return;
    }
    let gate = gate.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let active = gate.active.lock();
        if !*active {
            return;
        }
        gate.snapshot_pending.store(false, Ordering::Release);
        gate.emit(GatewayEvent::Changed);
    });
}

/// User gateway guilds can nest these fields in `properties`.
#[derive(Default, Deserialize)]
struct GuildProperties {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    banner: Option<String>,
    #[serde(default)]
    owner_id: Option<String>,
}

#[derive(Deserialize)]
struct WireGuild {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    banner: Option<String>,
    #[serde(default)]
    owner_id: Option<String>,
    #[serde(default)]
    properties: Option<GuildProperties>,
    #[serde(default)]
    unavailable: bool,
    #[serde(default)]
    emojis: Option<Vec<Lenient<super::models::CustomEmojiDto>>>,
    #[serde(default)]
    stickers: Option<Vec<Lenient<super::models::StickerDto>>>,
    #[serde(default)]
    channels: Option<Vec<Lenient<WireChannel>>>,
    #[serde(default, deserialize_with = "items")]
    threads: Vec<WireChannel>,
    #[serde(default, deserialize_with = "crate::discord::models::lenient")]
    roles: Option<Vec<super::permissions::WireRole>>,
    #[serde(default, deserialize_with = "crate::discord::models::lenient")]
    members: Vec<Value>,
    #[serde(default)]
    member: Option<Value>,
}

impl WireGuild {
    fn into_dto(self, user: &str) -> (GuildDto, Option<Vec<ChannelDto>>) {
        let properties = self.properties.unwrap_or_default();
        let name = properties.name.or(self.name);
        if self.unavailable && name.is_none() {
            return (
                GuildDto {
                    id: self.id,
                    ..Default::default()
                },
                None,
            );
        }
        let member = self.member.as_ref().or_else(|| {
            self.members
                .iter()
                .find(|member| super::permissions::member_id(member) == Some(user))
        });
        let access = properties
            .owner_id
            .or(self.owner_id)
            .zip(self.roles)
            .map(|(owner, roles)| super::permissions::guild_access(owner, roles, member));
        let members = self
            .members
            .iter()
            .filter_map(|member| {
                let id = super::permissions::member_id(member)?;
                let mut profile = super::models::GuildMemberDto::default();
                profile.update(member);
                Some((id.to_owned(), profile))
            })
            .collect();
        let channels = readable(self.channels).map(|channels| {
            channels
                .into_iter()
                .chain(self.threads)
                .map(|channel| channel.into_dto(Some(self.id.clone())).0)
                .collect()
        });
        let icon = properties.icon.or(self.icon);
        let banner = properties.banner.or(self.banner);
        (
            GuildDto {
                emojis: readable(self.emojis),
                stickers: readable(self.stickers),
                members,
                banner: banner.map(|hash| cdn_image(&format!("banners/{}", self.id), &hash, 600)),
                access,
                icon: guild_icon_url(&self.id, &icon),
                name: name.unwrap_or_default(),
                id: self.id,
            },
            channels,
        )
    }
}

fn readable<T>(rows: Option<Vec<Lenient<T>>>) -> Option<Vec<T>> {
    rows.map(|rows| rows.into_iter().filter_map(|row| row.0).collect())
}

fn guild_folders(settings: Option<&Value>) -> Option<Vec<super::models::GuildFolderDto>> {
    #[derive(Deserialize)]
    struct Folder {
        #[serde(deserialize_with = "crate::discord::models::number_field")]
        id: String,
        #[serde(default, deserialize_with = "crate::discord::models::lenient")]
        name: Option<String>,
        #[serde(default, deserialize_with = "crate::discord::models::lenient")]
        color: Option<u32>,
        guild_ids: Vec<Value>,
    }
    let folders: Vec<Lenient<Folder>> =
        Deserialize::deserialize(settings?.get("guild_folders")?).ok()?;
    Some(
        folders
            .into_iter()
            .filter_map(|folder| folder.0)
            .map(|folder| super::models::GuildFolderDto {
                id: folder.id,
                name: folder.name.unwrap_or_else(|| "Server folder".into()),
                color: folder.color,
                guild_ids: snowflakes(&folder.guild_ids),
            })
            .collect(),
    )
}

fn guild_positions(
    user_settings: Option<&Value>,
    user_guild_settings: Option<&Value>,
    root_folders: Option<&Value>,
) -> Vec<String> {
    let folders = [
        user_settings.and_then(|v| v.get("guild_folders")),
        user_guild_settings.and_then(|v| v.get("guild_folders")),
        root_folders,
    ]
    .into_iter()
    .flatten()
    .find_map(|value| value.as_array());
    let positions: Vec<_> = folders
        .into_iter()
        .flatten()
        .filter_map(|folder| folder.get("guild_ids").and_then(|v| v.as_array()))
        .flat_map(|ids| snowflakes(ids))
        .collect();
    if !positions.is_empty() {
        return positions;
    }
    user_settings
        .and_then(|v| v.get("guild_positions"))
        .and_then(|v| v.as_array())
        .map_or_else(Vec::new, |ids| snowflakes(ids))
}

fn snowflakes(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(super::models::number::<String>)
        .collect()
}

fn gateway_url(resume: Option<&str>) -> Result<String, String> {
    let base = resume.unwrap_or("wss://gateway.discord.gg");
    let mut url = reqwest::Url::parse(base).map_err(|_| "Invalid gateway URL")?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "wss"
        || !(host == "gateway.discord.gg" || host.ends_with(".discord.gg"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
    {
        return Err("Discord returned an untrusted gateway URL".into());
    }
    // User clients still speak gateway v9 with zlib-stream; v10 bot framing hangs IDENTIFY.
    url.set_path("/");
    url.set_query(Some("v=9&encoding=json&compress=zlib-stream"));
    url.set_fragment(None);
    Ok(url.into())
}

fn emit_status(gate: &Gate, detail: &str) {
    gate.emit(GatewayEvent::Status(detail.to_string()));
}
