//! The worker owns account, sign-in, media and gateway changes and runs them one at a time.
use crate::{
    discord::{
        auth::AuthStore,
        cache::CacheStore,
        gateway::{Events, GatewayEvent, GatewayHandle},
        login,
        media_signaling::VoiceTarget,
        models::UserDto,
        rest::DiscordRest,
    },
    files::Uploads,
    session::{Session, State},
    voice::{BrowserEvent, Controller, MediaScope, ShareSettings, Status, VoiceSettings},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use suncord_media::capture::{Capture, Codec, VideoEncoder};
use tokio::sync::{mpsc, oneshot, OwnedRwLockWriteGuard};
use zeroize::Zeroizing;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Action {
    Connect {
        account: String,
    },
    Disconnect,
    Login,
    CancelLogin,
    Resume {
        account: String,
    },
    ForgetAccount {
        account: String,
    },
    Sources,
    Join {
        channel: String,
        source: Option<String>,
        mute: bool,
        deaf: bool,
    },
    Microphone {
        source: Option<String>,
    },
    Mute {
        mute: bool,
        deaf: bool,
    },
    Share {
        audio_source: Option<String>,
        video_source: Option<String>,
    },
    Watch {
        owner: String,
    },
    StopStream,
    StopWatching,
    Stop,
}

impl Action {
    pub fn changes_account(&self) -> bool {
        matches!(
            self,
            Self::Connect { .. }
                | Self::Disconnect
                | Self::Login
                | Self::CancelLogin
                | Self::Resume { .. }
                | Self::ForgetAccount { .. }
        )
    }

    fn controls_media(&self) -> bool {
        !self.changes_account() && !matches!(self, Self::Sources)
    }
}

#[derive(Deserialize)]
pub struct Command {
    #[serde(flatten)]
    pub action: Action,
    pub media: Option<MediaScope>,
}

async fn check_audio_source(source: &Option<String>, monitor: bool) -> Result<(), String> {
    if let Some(id) = source {
        let sources = tokio::task::spawn_blocking(Capture::audio_sources)
            .await
            .map_err(|_| "Could not list audio sources")??;
        if !sources
            .iter()
            .any(|device| device.id == *id && device.monitor == monitor)
        {
            return Err(if monitor {
                "Choose an output monitor for stream audio"
            } else {
                "Choose a microphone input for voice"
            }
            .into());
        }
    }
    Ok(())
}

/// Account changes hold the session gate exclusively.
type Exclusive = OwnedRwLockWriteGuard<()>;

pub enum Request {
    Action(
        Command,
        String,
        oneshot::Sender<Result<Value, String>>,
        Option<Exclusive>,
    ),
    Shutdown(oneshot::Sender<()>, Exclusive),
    Attach(
        mpsc::Sender<BrowserEvent>,
        String,
        MediaScope,
        oneshot::Sender<Result<(), String>>,
    ),
}

pub fn start(
    cache: CacheStore,
    data: PathBuf,
    account: Option<String>,
    uploads: Arc<Uploads>,
) -> Result<(mpsc::Sender<Request>, Arc<Session>), String> {
    let auth = AuthStore::new(data.join("credentials"));
    let account = account.or_else(|| auth.active_account());
    let saved_account = account
        .as_ref()
        .filter(|id| auth.accounts().iter().any(|user| &user.id == *id));
    if let Some(id) = saved_account {
        cache.switch_account(data.join("accounts").join(id).join("snapshot.json"), id);
        if cache.user_id().as_deref() != Some(id.as_str()) {
            cache.clear();
        }
    }
    let rest = DiscordRest::new()?;
    let state = Arc::new(Mutex::new(State {
        accounts: auth.accounts(),
        account: saved_account.cloned(),
        status: if saved_account.is_some() {
            "Restoring session"
        } else {
            "Choose an account or sign in"
        }
        .into(),
        ..Default::default()
    }));
    let (commands, receiver) = mpsc::channel(8);
    let events = Arc::new(Events::default());
    let gateway = GatewayHandle::start(events.clone(), auth.clone(), cache.clone(), rest.clone());
    let session = Arc::new(Session {
        auth,
        rest,
        gateway,
        state,
        gate: Arc::new(tokio::sync::RwLock::new(())),
        changes: tokio::sync::watch::channel(true).0,
        remote: tokio::sync::Semaphore::new(8),
        navigation: Mutex::new(std::collections::HashMap::new()),
    });
    let worker = Worker {
        cache,
        data,
        session: session.clone(),
        uploads,
        media: None,
        output: None,
        login: None,
        flush: None,
    };
    tokio::task::spawn_local(worker.run(receiver, events, account));
    Ok((commands, session))
}

struct Worker {
    cache: CacheStore,
    data: PathBuf,
    session: Arc<Session>,
    uploads: Arc<Uploads>,
    media: Option<Controller>,
    output: Option<Playback>,
    login: Option<tokio::task::JoinHandle<Result<Zeroizing<String>, String>>>,
    flush: Option<tokio::task::JoinHandle<()>>,
}

struct Playback {
    scope: MediaScope,
    frames: mpsc::Sender<BrowserEvent>,
}

impl Worker {
    fn matches_account(&self, account: &str) -> bool {
        self.cache.user_id().map_or_else(
            || {
                account.is_empty()
                    || self.session.state.lock().unwrap().account.as_deref() == Some(account)
            },
            |current| current == account,
        )
    }

    fn matches_media(&self, action: &Action, scope: &MediaScope) -> bool {
        scope.session == self.session.generation()
            && match &self.output {
                Some(output) => output.scope == *scope,
                None => matches!(action, Action::Stop) && self.media.is_none(),
            }
    }
    fn pause_path(&self, account: &str) -> PathBuf {
        self.data
            .join("accounts")
            .join(account)
            .join("network-paused")
    }
    fn register(&self, account: &str, token: &str) -> Result<(), String> {
        let path = self.pause_path(account);
        let saved = match std::fs::read_to_string(&path) {
            Ok(reason) => Some(reason),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => Some("Could not read the account's network pause".into()),
        };
        self.session.rest.register_pause_handler(
            token,
            Arc::new(move |reason| {
                if std::fs::create_dir_all(path.parent().unwrap())
                    .and_then(|_| std::fs::write(&path, reason))
                    .is_err()
                {
                    eprintln!("could not persist Discord network pause");
                }
            }),
        );
        saved.map_or(Ok(()), Err)
    }
    async fn stop_media(&mut self) -> Result<(), String> {
        let result = if let Some(mut media) = self.media.take() {
            media.stop().await
        } else {
            Ok(())
        };
        self.output.take();
        self.session
            .activity(|state| state.media = Status::default());
        result
    }
    async fn disconnect(&mut self) -> Result<(), String> {
        let result = self.stop_media().await;
        self.session.gateway.disconnect();
        self.cache.clear_notices();
        self.session.auth.use_token("");
        if let Some(flush) = self.flush.take() {
            let _ = flush.await;
        }
        let cache = self.cache.clone();
        let _ = tokio::task::spawn_blocking(move || cache.flush_if_dirty()).await;
        self.session.update(|s| {
            s.connected = false;
            s.ready = false;
            s.account = None;
            s.status = "Disconnected".into();
            s.typing.clear();
        });
        result
    }
    async fn connect(&mut self, account: String) -> Result<(), String> {
        if self.login.is_some() {
            return Err("Finish or cancel sign-in first".into());
        }
        if !self.session.auth.accounts().iter().any(|u| u.id == account) {
            return Err("Unknown saved account".into());
        }
        self.disconnect().await?;
        self.session.update(|s| s.account = Some(account.clone()));
        let auth = self.session.auth.clone();
        let id = account.clone();
        let token = tokio::task::spawn_blocking(move || auth.read_account_token(&id))
            .await
            .map_err(|_| "Could not read the saved sign-in")??
            .ok_or("Sign in to this account again")?;
        self.register(&account, &token)?;
        let user = self.session.rest.validate_session(&token).await?;
        if user.id != account {
            return Err("Saved credential belongs to a different account".into());
        }
        self.activate(user, token).await
    }
    async fn activate(&mut self, user: UserDto, token: Zeroizing<String>) -> Result<(), String> {
        let auth = self.session.auth.clone();
        let saved = user.clone();
        tokio::task::spawn_blocking(move || auth.select_account(&saved))
            .await
            .map_err(|_| "Could not select account")??;
        let cache = self.cache.clone();
        let path = self
            .data
            .join("accounts")
            .join(&user.id)
            .join("snapshot.json");
        let id = user.id.clone();
        tokio::task::spawn_blocking(move || {
            cache.switch_account(path, &id);
            if cache.user_id().as_deref() != Some(id.as_str()) {
                cache.clear();
            }
        })
        .await
        .map_err(|_| "Could not load cached messages")?;
        self.uploads.retain(|account| account == user.id);
        self.cache.set_user(user.clone());
        self.session.auth.use_token(&token);
        self.session.update(|s| {
            s.account = Some(user.id);
            s.accounts = self.session.auth.accounts();
            s.connected = true;
            s.ready = false;
            s.status = "Connecting".into();
        });
        self.session.gateway.connect();
        Ok(())
    }
    async fn action(&mut self, action: Action) -> Result<Value, String> {
        match action {
            Action::ForgetAccount { account: id } => {
                if !self.session.auth.accounts().iter().any(|u| u.id == id) {
                    return Err("Unknown saved account".into());
                }
                if self.session.state.lock().unwrap().account.as_ref() == Some(&id)
                    || self.cache.user_id().as_ref() == Some(&id)
                {
                    self.cancel_login();
                    self.disconnect().await?;
                }
                if self.cache.user_id().as_ref() == Some(&id) {
                    self.cache.clear();
                }
                self.uploads.retain(|account| account != id);
                let snapshot = self.data.join("accounts").join(&id).join("snapshot.json");
                match std::fs::remove_file(snapshot) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err("Could not remove the account's cached messages".into()),
                }
                let auth = self.session.auth.clone();
                tokio::task::spawn_blocking(move || auth.remove_account(&id))
                    .await
                    .map_err(|_| "Could not remove account")??;
                self.session
                    .update(|s| s.accounts = self.session.auth.accounts());
            }
            Action::Connect { account } => self.connect(account).await?,
            Action::Disconnect => {
                self.cancel_login();
                self.disconnect().await?;
            }
            Action::Login => {
                if self.login.is_some() {
                    return Err("Sign-in is already running".into());
                }
                self.disconnect().await?;
                let session = self.session.clone();
                self.login = Some(tokio::task::spawn_local(async move {
                    login::login(move |url| {
                        session.activity(|s| {
                            s.login_url = url;
                            s.status = "Finish signing in on discord.com".into();
                        });
                    })
                    .await
                }));
            }
            Action::CancelLogin => self.cancel_login(),
            Action::Resume { account } => {
                if !self.session.auth.accounts().iter().any(|u| u.id == account) {
                    return Err("Unknown saved account".into());
                }
                match std::fs::remove_file(self.pause_path(&account)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err("Could not clear the network pause".into()),
                }
                let auth = self.session.auth.clone();
                let id = account.clone();
                if let Some(token) =
                    tokio::task::spawn_blocking(move || auth.read_account_token(&id))
                        .await
                        .map_err(|_| "Could not read sign-in")??
                {
                    self.session.rest.resume(&token);
                }
                self.connect(account).await?;
            }
            Action::Sources => {
                let sources = tokio::task::spawn_blocking(Capture::audio_sources)
                    .await
                    .map_err(|_| "Could not list audio sources")??;
                return Ok(json!({
                    "audio": sources,
                    "screens": Capture::screen_sources(),
                }));
            }
            Action::Stop => self.stop_media().await?,
            Action::Microphone { source } => {
                check_audio_source(&source, false).await?;
                self.media
                    .as_mut()
                    .ok_or("Join voice first")?
                    .set_voice(|voice| voice.source = source)?;
            }
            Action::Mute { mute, deaf } => self
                .media
                .as_mut()
                .ok_or("Join voice first")?
                .set_voice(|voice| {
                    voice.mute = mute;
                    voice.deaf = deaf;
                })?,
            Action::StopStream => self
                .media
                .as_mut()
                .ok_or("Join voice first")?
                .set_share(None)?,
            Action::StopWatching => self.media.as_mut().ok_or("Join voice first")?.watch(None)?,
            Action::Share {
                audio_source,
                video_source,
            } => {
                check_audio_source(&audio_source, true).await?;
                let encoder = if VideoEncoder::Nvenc.available(Codec::H264)? {
                    VideoEncoder::Nvenc
                } else {
                    VideoEncoder::X264
                };
                if !encoder.available(Codec::H264)? {
                    return Err("No H.264 encoder is available".into());
                }
                self.media
                    .as_mut()
                    .ok_or("Join voice first")?
                    .set_share(Some(ShareSettings {
                        encoder,
                        audio_source,
                        video_source,
                    }))?;
            }
            Action::Watch { owner } => {
                let owner = owner.parse::<u64>().map_err(|_| "Invalid stream owner")?;
                if owner == 0 {
                    return Err("Invalid stream owner".into());
                }
                self.media
                    .as_mut()
                    .ok_or("Join voice first")?
                    .watch(Some(owner))?;
            }
            Action::Join {
                channel,
                source,
                mute,
                deaf,
            } => {
                if !self.session.state.lock().unwrap().ready {
                    return Err("Wait for the account to connect".into());
                }
                check_audio_source(&source, false).await?;
                let target = self.cache.channel(&channel);
                // Discord excludes stage channels from DAVE, which calls require.
                if target.as_ref().is_some_and(|c| c.kind == 13) {
                    return Err("Stage channels are not supported yet".into());
                }
                let target = target
                    .filter(|c| matches!(c.kind, 1..=3))
                    .ok_or("Choose a voice channel or conversation")?;
                if self.media.as_ref().is_some_and(|media| {
                    media.is_running()
                        && media.status.lock().unwrap().channel_id.as_ref() == Some(&channel)
                }) {
                    return Ok(Value::Null);
                }
                if self.media.is_some() {
                    return Err("Open new playback before joining another call".into());
                }
                let playback = self
                    .output
                    .as_ref()
                    .filter(|output| !output.frames.is_closed())
                    .ok_or("Open browser playback first")?;
                let mut media = Controller::new(
                    self.session.gateway.clone(),
                    self.cache.user_id().ok_or("Not signed in")?,
                    VoiceTarget {
                        guild_id: target.guild_id,
                        channel_id: channel,
                    },
                    VoiceSettings { source, mute, deaf },
                    playback.frames.clone(),
                );
                media.start(playback.scope.call.clone())?;
                let status = media.status.lock().unwrap().clone();
                self.media = Some(media);
                self.session.activity(|state| state.media = status);
            }
        }
        Ok(Value::Null)
    }
    fn cancel_login(&mut self) {
        if let Some(login) = self.login.take() {
            login.abort();
        }
        self.session.update(|s| {
            s.login_url = None;
            s.status = "Disconnected".into();
        });
    }
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Request>,
        events: Arc<Events>,
        account: Option<String>,
    ) {
        if let Some(account) = account {
            let gate = self.session.gate.clone();
            let _guard = gate.write().await;
            if let Err(error) = self.connect(account).await {
                self.session.update(|s| s.status = error);
            }
        }
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut last_flush = std::time::Instant::now();
        let mut typing = std::collections::HashMap::new();
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Request::Action(command, account, reply, _guard)) => {
                        let result = if !self.matches_account(&account) {
                            Err("The active account changed".into())
                        } else if command.action.controls_media() && !command.media.as_ref().is_some_and(|scope| self.matches_media(&command.action, scope)) {
                            Err("The active call changed".into())
                        } else {
                            self.action(command.action).await
                        };
                        if let Err(error) = &result { self.session.activity(|state| state.status = error.clone()); }
                        let _ = reply.send(result);
                    }
                    Some(Request::Shutdown(reply, _guard)) => { self.cancel_login(); let _ = self.disconnect().await; self.session.changes.send_replace(false); let _ = reply.send(()); return; }
                    Some(Request::Attach(output, account, scope, reply)) => {
                        let result = if !self.matches_account(&account) || scope.session != self.session.generation() {
                            Err("The active account changed".into())
                        } else if reply.is_closed() || output.is_closed() {
                            Err("Browser playback closed".into())
                        } else if self.output.as_ref().is_some_and(|output| !output.frames.is_closed()) {
                            Err("Another tab owns media playback".into())
                        } else {
                            let result = self.stop_media().await;
                            if result.is_ok() {
                                let _ = output.try_send(BrowserEvent::Reset);
                                self.output = Some(Playback { scope, frames: output });
                            }
                            result
                        };
                        let _ = reply.send(result);
                    }
                    None => break,
                },
                _ = events.ready() => {
                    let Some((generation, event)) = events.recv() else { continue; };
                    if generation != self.session.gateway.generation() { continue; }
                    let activity = matches!(&event, GatewayEvent::Typing { .. } | GatewayEvent::Status(_) | GatewayEvent::Disconnected | GatewayEvent::Paused { .. });
                    let change = |s: &mut State| match event {
                        GatewayEvent::Status(status) => s.status = status,
                        GatewayEvent::Ready => { s.ready = true; s.status = "Connected".into(); },
                        GatewayEvent::Disconnected => { s.ready = false; s.status = "Reconnecting".into(); },
                        GatewayEvent::Paused { reason, .. } => { s.ready = false; s.connected = false; s.status = reason; },
                        GatewayEvent::Typing { channel, user } => {
                            typing.insert((channel.clone(), user.clone()), std::time::Instant::now());
                            let users = s.typing.entry(channel).or_default();
                            if !users.contains(&user) { users.push(user); }
                        },
                        GatewayEvent::MessageCreated { channel, user } => {
                            typing.remove(&(channel.clone(), user.clone()));
                            if let Some(users) = s.typing.get_mut(&channel) { users.retain(|u| *u != user); }
                        },
                        _ => {},
                    };
                    if activity { self.session.activity(change); } else { self.session.update(change); }
                },
                _ = tick.tick() => {
                    typing.retain(|_, started| started.elapsed() < Duration::from_secs(10));
                    let expired = {
                        let mut state = self.session.state.lock().unwrap();
                        let before: usize = state.typing.values().map(Vec::len).sum();
                        state.typing.retain(|channel, users| {
                            users.retain(|user| typing.contains_key(&(channel.clone(), user.clone())));
                            !users.is_empty()
                        });
                        before != state.typing.values().map(Vec::len).sum::<usize>()
                    };
                    if expired { self.session.activity(|_| {}); }
                    if self.output.as_ref().is_some_and(|output| output.frames.is_closed()) {
                        // A call that already ended keeps its outcome on screen after playback closes.
                        let ended = self.media.as_ref().filter(|media| !media.is_running()).map(|media| media.status.lock().unwrap().clone());
                        let _ = self.stop_media().await;
                        if let Some(status) = ended { self.session.activity(|state| state.media = status); }
                    }
                    if let Some(media) = &self.media {
                        let status = media.status.lock().unwrap().clone();
                        if self.session.state.lock().unwrap().media != status {
                            self.session.activity(|state| {
                                if let Some(error) = &status.error { state.status.clone_from(error); }
                                state.media = status;
                            });
                        }
                    }
                    if self.login.as_ref().is_some_and(|t| t.is_finished()) {
                        let Ok(_guard) = self.session.gate.clone().try_write_owned() else { continue; };
                        let result = self.login.take().unwrap().await.map_err(|_| "Sign-in worker stopped".into()).and_then(|r| r);
                        let result = match result { Ok(token) => self.finish_login(token).await, Err(error) => Err(error) };
                        self.session.update(|s| { s.login_url = None; if let Err(error) = result { s.status = error; } });
                    }
                    if last_flush.elapsed() >= Duration::from_secs(5) && self.flush.as_ref().is_none_or(|task| task.is_finished()) {
                        let cache = self.cache.clone();
                        self.flush = Some(tokio::task::spawn_blocking(move || cache.flush_if_dirty()));
                        last_flush = std::time::Instant::now();
                    }
                },
            }
        }
        let gate = self.session.gate.clone();
        let _guard = gate.write().await;
        self.cancel_login();
        let _ = self.disconnect().await;
        self.session.changes.send_replace(false);
    }
    async fn finish_login(&mut self, token: Zeroizing<String>) -> Result<(), String> {
        // Respect saved account pauses even when the browser hands back its token.
        for user in self.session.auth.accounts() {
            let auth = self.session.auth.clone();
            let id = user.id.clone();
            let saved = tokio::task::spawn_blocking(move || auth.read_account_token(&id))
                .await
                .map_err(|_| "Could not check saved sign-in")??;
            if saved
                .as_ref()
                .is_some_and(|saved| saved.as_str() == token.as_str())
            {
                self.register(&user.id, &token)?;
            }
        }
        let user = self.session.rest.validate_session(&token).await?;
        self.register(&user.id, &token)?;
        let auth = self.session.auth.clone();
        let saved = user.clone();
        let credential = Zeroizing::new(token.to_string());
        tokio::task::spawn_blocking(move || auth.save_account(&saved, &credential))
            .await
            .map_err(|_| "Could not save sign-in")??;
        self.activate(user, token).await
    }
}
