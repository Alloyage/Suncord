use crate::models::Note;
use crate::state;
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::window::Color;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use zeroize::{Zeroize, Zeroizing};

const SERVICE: &str = "Suncord";
const ACCOUNT: &str = "discord-token";
const LOGIN_URL: &str = "https://discord.com/login";
const DESKTOP_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";
const WEBVIEW_ARGS: &str =
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required";
const LOGIN_BG: Color = Color(0x11, 0x11, 0x11, 0xff);

struct AuthInner {
    token: Mutex<Option<Zeroizing<String>>>,
    inflight: AtomicBool,
}

#[derive(Clone)]
pub struct AuthStore {
    inner: Arc<AuthInner>,
}

impl AuthStore {
    pub fn load() -> Self {
        let token = read_keyring().and_then(|raw| normalize_token(&raw).map(Zeroizing::new));
        Self {
            inner: Arc::new(AuthInner {
                token: Mutex::new(token),
                inflight: AtomicBool::new(false),
            }),
        }
    }

    pub fn token(&self) -> Option<Zeroizing<String>> {
        self.inner
            .token
            .lock()
            .as_ref()
            .map(|value| Zeroizing::new(value.to_string()))
    }

    pub fn set_token(&self, token: &str) -> Result<(), String> {
        write_keyring(token)?;
        *self.inner.token.lock() = Some(Zeroizing::new(token.to_string()));
        Ok(())
    }

    pub fn clear(&self) -> Result<(), String> {
        // Drop the in-memory token first so nothing can reconnect mid-clear.
        if let Some(mut previous) = self.inner.token.lock().take() {
            previous.zeroize();
        }
        self.inner.inflight.store(false, Ordering::Release);
        delete_keyring()?;
        // Second pass in case Credential Manager lagged on the first delete.
        let _ = delete_keyring();
        if read_keyring().is_some() {
            return Err("The OS keychain still holds a session token.".into());
        }
        Ok(())
    }
}

pub fn normalize_token(raw: &str) -> Option<String> {
    let mut token = raw.trim().trim_matches('"').trim().to_string();
    if let Some(rest) = token
        .strip_prefix("Bot ")
        .or_else(|| token.strip_prefix("Bearer "))
    {
        token = rest.trim().to_string();
    }
    if !(50..=200).contains(&token.len()) {
        return None;
    }
    if !token
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-')
    {
        return None;
    }
    Some(token)
}

pub fn open_login(app: &AppHandle) -> Result<(), String> {
    let login_url: url::Url = LOGIN_URL
        .parse()
        .map_err(|err| format!("login url: {err}"))?;

    // Always rebuild so a prior Discord session in WebView2 cannot auto-capture again.
    destroy_login_window(app);
    let data_dir = login_data_dir(app)?;
    let _ = std::fs::create_dir_all(&data_dir);

    let script = include_str!("auth_hook.js");
    let app_handle = app.clone();

    let window = WebviewWindowBuilder::new(app, "login", WebviewUrl::External(login_url))
        .title("Sign in")
        .inner_size(520.0, 780.0)
        .resizable(true)
        .center()
        .focused(true)
        .visible(true)
        .background_color(LOGIN_BG)
        .user_agent(DESKTOP_UA)
        .additional_browser_args(WEBVIEW_ARGS)
        .data_directory(data_dir)
        .initialization_script(script)
        .on_navigation(move |url| {
            if url.scheme() == "suncord-auth" {
                if let Some(token) = token_from_callback(url) {
                    let app_handle = app_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(err) = state::complete_login(&app_handle, token).await {
                            log::warn!("login was not stored");
                            let _ = app_handle.emit(
                                "suncord://error",
                                Note {
                                    message: err,
                                },
                            );
                            if let Some(window) = app_handle.get_webview_window("login") {
                                let _ = window.eval("window.__suncordRetry = true;");
                            }
                        }
                    });
                }
                return false;
            }
            if url.scheme() == "about" || url.scheme() == "data" {
                return true;
            }
            matches!(url.host_str(), Some(host) if login_host_allowed(host))
        })
        .build()
        .map_err(|err| err.to_string())?;

    let closer = app.clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            let _ = closer.emit("suncord://login-closed", ());
        }
    });

    Ok(())
}

pub fn wipe_login_session(app: &AppHandle) {
    destroy_login_window(app);
    if let Ok(dir) = login_data_dir(app) {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn destroy_login_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("login") {
        let _ = window.destroy();
    }
}

fn login_data_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|err| err.to_string())?
        .join("login-webview"))
}

fn token_from_callback(url: &url::Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned())
}

pub fn login_host_allowed(host: &str) -> bool {
    const ROOTS: &[&str] = &[
        "discord.com",
        "discordapp.com",
        "discord.gg",
        "discord.media",
        "discordapp.net",
        "hcaptcha.com",
        "recaptcha.net",
        "google.com",
        "gstatic.com",
        "googleapis.com",
        "cloudflare.com",
        "cf-assets.com",
    ];
    ROOTS
        .iter()
        .any(|root| host == *root || host_suffix(host, root))
}

fn host_suffix(host: &str, root: &str) -> bool {
    host.strip_suffix(root)
        .is_some_and(|prefix| prefix.ends_with('.'))
}

pub(crate) fn mark_begin(auth: &AuthStore) -> bool {
    auth.inner.inflight.swap(true, Ordering::AcqRel)
}

pub(crate) fn mark_end(auth: &AuthStore) {
    auth.inner.inflight.store(false, Ordering::Release);
}

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, ACCOUNT).map_err(|err| err.to_string())
}

fn read_keyring() -> Option<String> {
    let entry = entry().ok()?;
    match entry.get_password() {
        Ok(value) => Some(value),
        Err(keyring::Error::NoEntry) => None,
        Err(err) => {
            log::warn!("credential store unavailable: {err}");
            None
        }
    }
}

fn write_keyring(token: &str) -> Result<(), String> {
    entry()?
        .set_password(token)
        .map_err(|err| format!("could not store the session in the OS keychain: {err}"))
}

fn delete_keyring() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(format!("could not remove the session from the OS keychain: {err}")),
    }
}

#[derive(Clone, Serialize)]
struct AuthEvent {
    #[serde(rename = "signedIn")]
    signed_in: bool,
}

pub fn emit_auth(app: &AppHandle, signed_in: bool) {
    let _ = app.emit("suncord://auth", AuthEvent { signed_in });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_normal_token_and_strips_quotes() {
        let raw = format!("\"{}\"", "a".repeat(60));
        let token = normalize_token(&raw).unwrap();
        assert_eq!(token.len(), 60);
    }

    #[test]
    fn rejects_short_or_dirty_tokens() {
        assert!(normalize_token("short").is_none());
        assert!(normalize_token(&format!("{} {}", "a".repeat(60), "no")).is_none());
    }

    #[test]
    fn allows_discord_and_captcha_hosts() {
        assert!(login_host_allowed("discord.com"));
        assert!(login_host_allowed("status.discord.com"));
        assert!(login_host_allowed("newassets.hcaptcha.com"));
        assert!(!login_host_allowed("evil.example"));
    }
}
