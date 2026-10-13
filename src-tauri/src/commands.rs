use crate::audio::list_devices;
use crate::capture::list_sources;
use crate::models::{
    AudioDeviceDto, Bootstrap, ChatMessageDto, Settings, SettingsPatch, SourceDto,
};
use crate::state::{self, AppState};
use tauri::{AppHandle, State, WebviewWindow};

#[tauri::command]
pub async fn bootstrap(app: AppHandle) -> Bootstrap {
    state::bootstrap(&app).await
}

#[tauri::command]
pub async fn auth_start_login(app: AppHandle) -> Result<(), String> {
    // WebView2 creation must not run on the command/UI wait path or Windows deadlocks.
    tauri::async_runtime::spawn_blocking(move || crate::auth::open_login(&app))
        .await
        .map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn auth_capture_token(
    app: AppHandle,
    window: WebviewWindow,
    token: String,
) -> Result<(), String> {
    if window.label() != "login" {
        return Err("rejected".into());
    }
    let url = window.url().map_err(|err| err.to_string())?;
    let host = url.host_str().unwrap_or("");
    if !(host == "discord.com" || host.ends_with(".discord.com")) {
        return Err("rejected".into());
    }
    state::complete_login(&app, token).await
}

#[tauri::command]
pub fn auth_logout(app: AppHandle) -> Result<(), String> {
    state::logout(&app)
}

#[tauri::command]
pub async fn select_channel(
    state: State<'_, AppState>,
    channel_id: String,
    guild_id: Option<String>,
) -> Result<Vec<ChatMessageDto>, String> {
    state.cache.set_active(guild_id, Some(&channel_id));
    let token = state.auth.token().ok_or("Sign in before opening a channel.")?;
    // Selection is always authoritative: resolve gateway gaps, deletes, and missed edits.
    // Discord returns newest-first; cache + UI expect oldest-first chronological order.
    let messages = state.rest.messages(&token, &channel_id).await?;
    state.cache.replace_messages(&channel_id, messages);
    Ok(state.cache.messages(&channel_id).unwrap_or_default())
}

#[tauri::command]
pub async fn send_message(
    state: State<'_, AppState>,
    channel_id: String,
    content: String,
    nonce: String,
) -> Result<ChatMessageDto, String> {
    let content = content.trim().to_string();
    if content.is_empty() {
        return Err("Message is empty.".into());
    }
    if content.len() > 2000 {
        return Err("Discord messages stop at 2000 characters.".into());
    }
    let token = state.auth.token().ok_or("Sign in before sending.")?;
    // POST https://discord.com/api/v10/channels/{channel_id}/messages
    let message = state
        .rest
        .send_message(&token, &channel_id, &content, &nonce)
        .await?;
    state.cache.push_message(message.clone());
    Ok(message)
}

#[tauri::command]
pub async fn refresh_dms(state: State<'_, AppState>) -> Result<Vec<crate::models::ChannelDto>, String> {
    let token = state.auth.token().ok_or("Sign in before loading DMs.")?;
    let dms = state.rest.dm_channels(&token).await?;
    state.cache.replace_dms(dms.clone());
    Ok(dms)
}

#[tauri::command]
pub async fn channel_pins(
    state: State<'_, AppState>,
    channel_id: String,
) -> Result<Vec<ChatMessageDto>, String> {
    let token = state.auth.token().ok_or("Sign in before loading pins.")?;
    let mut pins = state.rest.pins(&token, &channel_id).await?;
    pins.reverse();
    Ok(pins)
}

#[tauri::command]
pub fn open_external(app: AppHandle, url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(|_| "That link is not a valid URL.".to_string())?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("Only http(s) links can be opened.".into());
    }
    use tauri_plugin_shell::ShellExt;
    // Plugin open is non-blocking; keep the UI thread free.
    app.shell()
        .open(parsed.as_str(), None)
        .map_err(|err| format!("Could not open the link: {err}"))
}

#[tauri::command]
pub async fn edit_message(
    state: State<'_, AppState>,
    channel_id: String,
    message_id: String,
    content: String,
) -> Result<ChatMessageDto, String> {
    let content = content.trim().to_string();
    if content.is_empty() {
        return Err("Message is empty.".into());
    }
    if content.len() > 2000 {
        return Err("Discord messages stop at 2000 characters.".into());
    }
    let token = state.auth.token().ok_or("Sign in before editing.")?;
    let message = state
        .rest
        .edit_message(&token, &channel_id, &message_id, &content)
        .await?;
    state.cache.push_message(message.clone());
    Ok(message)
}

#[tauri::command]
pub async fn delete_message(
    state: State<'_, AppState>,
    channel_id: String,
    message_id: String,
) -> Result<(), String> {
    let token = state.auth.token().ok_or("Sign in before deleting.")?;
    state
        .rest
        .delete_message(&token, &channel_id, &message_id)
        .await?;
    state
        .cache
        .remove_messages(&channel_id, &[message_id.clone()]);
    Ok(())
}

#[tauri::command]
pub async fn add_reaction(
    state: State<'_, AppState>,
    channel_id: String,
    message_id: String,
    emoji: String,
) -> Result<(), String> {
    let emoji = emoji.trim().to_string();
    if emoji.is_empty() {
        return Err("Pick an emoji first.".into());
    }
    let token = state.auth.token().ok_or("Sign in before reacting.")?;
    state
        .rest
        .add_reaction(&token, &channel_id, &message_id, &emoji)
        .await
}

#[tauri::command]
pub fn voice_join(state: State<'_, AppState>, channel_id: String, guild_id: Option<String>) {
    state.voice.join(guild_id, channel_id);
}

#[tauri::command]
pub fn voice_leave(state: State<'_, AppState>) {
    state.voice.leave();
}

#[tauri::command]
pub fn voice_set_muted(state: State<'_, AppState>, muted: bool) {
    state.voice.set_muted(muted);
}

#[tauri::command]
pub fn voice_set_deafened(state: State<'_, AppState>, deafened: bool) {
    state.voice.set_deafened(deafened);
}

#[tauri::command]
pub async fn voice_devices() -> Vec<AudioDeviceDto> {
    tauri::async_runtime::spawn_blocking(list_devices)
        .await
        .unwrap_or_default()
}

#[tauri::command]
pub fn settings_update(state: State<'_, AppState>, patch: SettingsPatch) -> Settings {
    let settings = state.settings.apply_patch(patch);
    state.voice.apply(settings.clone());
    state
        .capture
        .apply(settings.capture_fps, settings.hw_preference.clone());
    state
        .asr
        .configure(settings.asr_enabled, settings.asr_model_path.clone());
    settings
}

#[tauri::command]
pub async fn capture_sources() -> Vec<SourceDto> {
    tauri::async_runtime::spawn_blocking(list_sources)
        .await
        .unwrap_or_default()
}

#[tauri::command]
pub fn capture_start(state: State<'_, AppState>, id: String) {
    let settings = state.settings.get();
    state
        .capture
        .start_source(id, settings.capture_fps, settings.hw_preference);
}

#[tauri::command]
pub fn capture_stop(state: State<'_, AppState>) {
    state.capture.stop();
}

#[tauri::command]
pub fn capture_set_preview(state: State<'_, AppState>, enabled: bool) {
    state.capture.set_preview(enabled);
}
