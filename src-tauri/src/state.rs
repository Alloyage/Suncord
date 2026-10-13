use crate::asr::AsrHandle;
use crate::auth::{self, AuthStore};
use crate::cache::CacheStore;
use crate::capture::CaptureHandle;
use crate::gateway::GatewayHandle;
use crate::models::{Bootstrap, Snapshot};
use crate::rest::DiscordRest;
use crate::settings::SettingsStore;
use crate::voice::VoiceHandle;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;
use zeroize::Zeroize;

pub struct AppState {
    pub auth: AuthStore,
    pub cache: CacheStore,
    pub settings: SettingsStore,
    pub rest: DiscordRest,
    pub gateway: GatewayHandle,
    pub voice: VoiceHandle,
    pub capture: CaptureHandle,
    pub asr: AsrHandle,
}

impl AppState {
    pub fn boot(app: &AppHandle) -> Result<Self, String> {
        let dir = app.path().app_data_dir().map_err(|err| err.to_string())?;
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        let settings = SettingsStore::load(dir.join("settings.json"));
        let auth = AuthStore::load();
        let cache = CacheStore::load(dir.join("snapshot.json"));
        let rest = DiscordRest::new()?;
        let (voice_tx, voice_rx) = mpsc::unbounded_channel();
        let gateway = GatewayHandle::start(
            app.clone(),
            auth.clone(),
            Arc::new(cache.clone()),
            rest.clone(),
            voice_tx,
        );
        let asr = AsrHandle::start(app.clone());
        let (frame_tx, frame_rx) = mpsc::channel(4);
        let audio = crate::audio::AudioPump::start(frame_tx, asr.audio_sender());
        let voice = VoiceHandle::start(
            app.clone(),
            gateway.clone(),
            Arc::new(cache.clone()),
            auth.clone(),
            rest.clone(),
            voice_rx,
            frame_rx,
            audio,
            asr.clone(),
        );
        let capture = CaptureHandle::start(app.clone());
        let current = settings.get();
        voice.apply(current.clone());
        asr.configure(current.asr_enabled, current.asr_model_path.clone());
        capture.apply(current.capture_fps, current.hw_preference.clone());
        // Gateway connects only after bootstrap validates the keychain token.
        let flusher = cache.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                flusher.flush_if_dirty();
            }
        });
        Ok(Self {
            auth,
            cache,
            settings,
            rest,
            gateway,
            voice,
            capture,
            asr,
        })
    }

    pub fn bootstrap_offline(&self) -> Bootstrap {
        Bootstrap {
            signed_in: false,
            snapshot: Snapshot::empty(),
            settings: self.settings.get(),
            phase: "offline".into(),
        }
    }
}

pub async fn bootstrap(app: &AppHandle) -> Bootstrap {
    let state = app.state::<AppState>();
    let Some(token) = state.auth.token() else {
        auth::wipe_login_session(app);
        return state.bootstrap_offline();
    };
    match state.rest.current_user(&token).await {
        Ok(user) => {
            state.cache.set_user(user);
            state.gateway.connect();
            Bootstrap {
                signed_in: true,
                snapshot: state.cache.snapshot(true),
                settings: state.settings.get(),
                phase: "connecting".into(),
            }
        }
        Err(err) => {
            log::warn!("saved session rejected: {err}");
            let _ = state.auth.clear();
            state.cache.clear();
            state.gateway.disconnect();
            auth::wipe_login_session(app);
            state.bootstrap_offline()
        }
    }
}

pub async fn complete_login(app: &AppHandle, mut raw: String) -> Result<(), String> {
    let Some(token) = auth::normalize_token(&raw) else {
        raw.zeroize();
        return Err("The sign-in token was rejected.".into());
    };
    raw.zeroize();
    let state = app.state::<AppState>();
    if auth::mark_begin(&state.auth) {
        return Ok(());
    }
    let auth = state.auth.clone();
    let cache = state.cache.clone();
    let gateway = state.gateway.clone();
    let rest = state.rest.clone();
    drop(state);
    let verified = async {
        let secret = zeroize::Zeroizing::new(token);
        let user = rest.current_user(&secret).await?;
        auth.set_token(&secret)?;
        cache.set_user(user);
        Ok::<(), String>(())
    }
    .await;
    auth::mark_end(&auth);
    let () = verified?;
    auth::emit_auth(app, true);
    let _ = app.emit("suncord://snapshot", cache.snapshot(true));
    // Close the sign-in webview first; no status toast on success.
    let app_handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(window) = app_handle.get_webview_window("login") {
            let _ = window.close();
        }
    });
    gateway.connect();
    Ok(())
}

pub fn logout(app: &AppHandle) -> Result<(), String> {
    // Close + wipe the Discord login profile so Sign in cannot auto-reuse a cached token.
    auth::wipe_login_session(app);

    let state = app.state::<AppState>();
    state.gateway.disconnect();
    state.voice.leave();
    state.capture.stop();
    state.auth.clear()?;
    state.cache.clear();
    let empty = state.cache.snapshot(true);
    drop(state);

    auth::emit_auth(app, false);
    let _ = app.emit("suncord://snapshot", empty);
    let _ = app.emit(
        "suncord://status",
        crate::models::StatusEvent {
            phase: "offline".into(),
            detail: String::new(),
        },
    );
    Ok(())
}
