#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod asr;
mod audio;
mod auth;
mod cache;
mod capture;
mod commands;
mod crypto;
mod dave;
mod gateway;
mod models;
mod opus_codec;
mod rest;
mod settings;
mod state;
mod voice;
mod zlib_stream;

use state::AppState;
use tauri::Manager;

fn main() {
    #[cfg(debug_assertions)]
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("suncord=info"))
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let state = AppState::boot(app.handle())?;
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::auth_start_login,
            commands::auth_capture_token,
            commands::auth_logout,
            commands::select_channel,
            commands::send_message,
            commands::edit_message,
            commands::delete_message,
            commands::add_reaction,
            commands::refresh_dms,
            commands::channel_pins,
            commands::open_external,
            commands::voice_join,
            commands::voice_leave,
            commands::voice_set_muted,
            commands::voice_set_deafened,
            commands::voice_devices,
            commands::settings_update,
            commands::capture_sources,
            commands::capture_start,
            commands::capture_stop,
            commands::capture_set_preview,
        ])
        .run(tauri::generate_context!())
        .expect("Suncord failed to start");
}
