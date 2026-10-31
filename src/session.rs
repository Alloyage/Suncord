//! Session state shared by the worker and HTTP request tasks.
use crate::{
    discord::{auth::AuthStore, gateway::GatewayHandle, models::UserDto, rest::DiscordRest},
    voice::Status,
};
use serde::Serialize;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use zeroize::Zeroizing;

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub snapshot_revision: u64,
    pub connected: bool,
    pub ready: bool,
    pub status: String,
    pub login_url: Option<String>,
    pub accounts: Vec<UserDto>,
    pub account: Option<String>,
    pub media: Status,
    pub typing: std::collections::HashMap<String, Vec<String>>,
}

pub type Pages = std::collections::HashMap<String, (u64, String, std::time::Instant)>;

pub struct Session {
    pub auth: AuthStore,
    pub rest: DiscordRest,
    pub gateway: GatewayHandle,
    pub state: Arc<Mutex<State>>,
    pub gate: Arc<tokio::sync::RwLock<()>>,
    pub remote: tokio::sync::Semaphore,
    pub navigation: Mutex<Pages>,
    /// Notifies pages of state changes. False once the session shut down.
    pub changes: tokio::sync::watch::Sender<bool>,
}

impl Session {
    pub fn navigate(&self, client: &str, revision: u64, channel: &str) -> Result<(), String> {
        if client.is_empty() || client.len() > 64 {
            return Err("Invalid page identifier".into());
        }
        let mut pages = self.navigation.lock().unwrap();
        pages.retain(|_, (_, _, updated)| updated.elapsed() < Duration::from_secs(600));
        if !pages.contains_key(client) && pages.len() >= 128 {
            return Err("Too many open pages".into());
        }
        if pages.get(client).is_some_and(|(current, selected, _)| {
            revision < *current || revision == *current && selected != channel
        }) {
            return Err("Channel selection changed while loading".into());
        }
        pages.insert(
            client.to_owned(),
            (revision, channel.to_owned(), std::time::Instant::now()),
        );
        Ok(())
    }
    /// Holds the page's navigation lock so the request commits before another selection.
    pub fn selected(
        &self,
        client: &str,
        revision: u64,
        channel: &str,
    ) -> Option<std::sync::MutexGuard<'_, Pages>> {
        let pages = self.navigation.lock().unwrap();
        pages
            .get(client)
            .is_some_and(|(current, selected, _)| *current == revision && selected == channel)
            .then_some(pages)
    }
    pub fn closed(&self) -> bool {
        !*self.changes.borrow()
    }
    fn notify(&self) {
        self.changes.send_if_modified(|open| *open);
    }
    pub fn generation(&self) -> u64 {
        self.gateway.generation()
    }
    /// Changes state that requires the page to reload the snapshot.
    pub fn update(&self, change: impl FnOnce(&mut State)) {
        let mut state = self.state.lock().unwrap();
        change(&mut state);
        state.snapshot_revision = state.snapshot_revision.wrapping_add(1);
        self.notify();
    }
    pub fn activity(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state.lock().unwrap());
        self.notify();
    }
    pub fn token(&self) -> Result<Zeroizing<String>, String> {
        if !self.state.lock().unwrap().connected {
            return Err("Connect an account first".into());
        }
        self.auth
            .token()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| "Connect an account first".into())
    }
}
