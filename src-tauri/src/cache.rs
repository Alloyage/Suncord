use crate::models::{ChannelDto, ChatMessageDto, GuildDto, GuildFolderDto, Snapshot, UserDto};
use parking_lot::RwLock;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const MESSAGE_CAP: usize = 200;
const SNAPSHOT_CAP: usize = 50;

struct Inner {
    user: Option<UserDto>,
    guilds: Vec<GuildDto>,
    channels: Vec<ChannelDto>,
    dms: Vec<ChannelDto>,
    messages: HashMap<String, VecDeque<ChatMessageDto>>,
    users: HashMap<String, UserDto>,
    folders: Vec<GuildFolderDto>,
    active_guild_id: Option<String>,
    active_channel_id: Option<String>,
}

#[derive(Clone)]
pub struct CacheStore {
    inner: Arc<RwLock<Inner>>,
    path: PathBuf,
    dirty: Arc<AtomicBool>,
}

impl CacheStore {
    pub fn load(path: PathBuf) -> Self {
        let snapshot = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Snapshot>(&bytes).ok())
            .unwrap_or_else(Snapshot::empty);
        let mut messages = HashMap::new();
        for (id, rows) in snapshot.messages {
            messages.insert(id, VecDeque::from(rows));
        }
        Self {
            inner: Arc::new(RwLock::new(Inner {
                user: snapshot.user,
                guilds: snapshot.guilds,
                channels: snapshot.channels,
                dms: sort_dms(snapshot.dms),
                messages,
                users: snapshot.users,
                folders: snapshot.folders,
                active_guild_id: snapshot.active_guild_id,
                active_channel_id: snapshot.active_channel_id,
            })),
            path,
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn user_id(&self) -> Option<String> {
        self.inner.read().user.as_ref().map(|user| user.id.clone())
    }

    pub fn set_user(&self, user: UserDto) {
        let mut inner = self.inner.write();
        inner.users.insert(user.id.clone(), user.clone());
        inner.user = Some(user);
        drop(inner);
        self.mark();
    }

    pub fn remember_users(&self, users: impl IntoIterator<Item = UserDto>) {
        let mut inner = self.inner.write();
        for user in users {
            if !user.id.is_empty() {
                inner.users.insert(user.id.clone(), user);
            }
        }
        drop(inner);
        self.mark();
    }

    pub fn set_active(&self, guild_id: Option<String>, channel_id: Option<&str>) {
        let mut inner = self.inner.write();
        inner.active_guild_id = guild_id;
        inner.active_channel_id = channel_id.map(str::to_string);
        drop(inner);
        self.mark();
    }

    pub fn replace_structure(
        &self,
        guilds: Vec<GuildDto>,
        channels: Vec<ChannelDto>,
        dms: Vec<ChannelDto>,
        folders: Vec<GuildFolderDto>,
    ) {
        let mut inner = self.inner.write();
        inner.guilds = guilds;
        inner.channels = channels;
        inner.dms = sort_dms(dms);
        inner.folders = folders;
        drop(inner);
        self.mark();
    }

    pub fn set_folders(&self, folders: Vec<GuildFolderDto>) {
        self.inner.write().folders = folders;
        self.mark();
    }

    pub fn replace_dms(&self, dms: Vec<ChannelDto>) {
        self.inner.write().dms = sort_dms(dms);
        self.mark();
    }

    pub fn channel_name(&self, channel_id: &str) -> Option<String> {
        let inner = self.inner.read();
        inner
            .channels
            .iter()
            .chain(inner.dms.iter())
            .find(|channel| channel.id == channel_id)
            .map(|channel| channel.name.clone())
    }

    pub fn user_lookup(&self, user_id: &str) -> Option<UserDto> {
        self.inner.read().users.get(user_id).cloned()
    }

    pub fn upsert_guild(&self, guild: GuildDto, channels: Vec<ChannelDto>) {
        let mut inner = self.inner.write();
        let id = guild.id.clone();
        if let Some(existing) = inner.guilds.iter_mut().find(|item| item.id == id) {
            *existing = guild;
        } else {
            inner.guilds.push(guild);
        }
        inner
            .channels
            .retain(|channel| channel.guild_id.as_deref() != Some(id.as_str()));
        inner.channels.extend(channels);
        drop(inner);
        self.mark();
    }

    pub fn upsert_channel(&self, channel: ChannelDto, dm: bool) {
        let mut inner = self.inner.write();
        let list = if dm {
            &mut inner.dms
        } else {
            &mut inner.channels
        };
        if let Some(existing) = list.iter_mut().find(|item| item.id == channel.id) {
            *existing = channel;
        } else {
            list.push(channel);
        }
        if dm {
            inner.dms = sort_dms(std::mem::take(&mut inner.dms));
        }
        drop(inner);
        self.mark();
    }

    pub fn remove_channel(&self, id: &str) {
        let mut inner = self.inner.write();
        inner.channels.retain(|channel| channel.id != id);
        inner.dms.retain(|channel| channel.id != id);
        drop(inner);
        self.mark();
    }

    pub fn messages(&self, channel_id: &str) -> Option<Vec<ChatMessageDto>> {
        self.inner
            .read()
            .messages
            .get(channel_id)
            .filter(|rows| !rows.is_empty())
            .map(|rows| rows.iter().cloned().collect())
    }

    /// `messages` is Discord's newest-first page. Stored oldest-first.
    pub fn replace_messages(&self, channel_id: &str, mut messages: Vec<ChatMessageDto>) {
        messages.reverse();
        if messages.len() > MESSAGE_CAP {
            let excess = messages.len() - MESSAGE_CAP;
            messages.drain(0..excess);
        }
        let mut inner = self.inner.write();
        for message in &messages {
            if !message.author_id.is_empty() {
                inner.users.insert(
                    message.author_id.clone(),
                    UserDto {
                        id: message.author_id.clone(),
                        name: message.author.clone(),
                        avatar: message.avatar.clone(),
                    },
                );
            }
        }
        if let Some(dm) = inner.dms.iter_mut().find(|channel| channel.id == channel_id) {
            if let Some(last) = messages.last() {
                dm.last_message_id = Some(last.id.clone());
            }
            inner.dms = sort_dms(std::mem::take(&mut inner.dms));
        }
        inner
            .messages
            .insert(channel_id.to_string(), VecDeque::from(messages));
        drop(inner);
        self.mark();
    }

    pub fn push_message(&self, message: ChatMessageDto) {
        let mut inner = self.inner.write();
        if !message.author_id.is_empty() {
            inner.users.insert(
                message.author_id.clone(),
                UserDto {
                    id: message.author_id.clone(),
                    name: message.author.clone(),
                    avatar: message.avatar.clone(),
                },
            );
        }
        if let Some(dm) = inner
            .dms
            .iter_mut()
            .find(|channel| channel.id == message.channel_id)
        {
            dm.last_message_id = Some(message.id.clone());
            inner.dms = sort_dms(std::mem::take(&mut inner.dms));
        }
        let rows = inner.messages.entry(message.channel_id.clone()).or_default();
        if let Some(existing) = rows.iter_mut().find(|item| item.id == message.id) {
            *existing = message;
        } else if message.nonce.as_ref().is_some_and(|nonce| {
            rows.iter()
                .rev()
                .any(|item| item.nonce.as_deref() == Some(nonce.as_str()))
        }) {
            let nonce = message.nonce.clone();
            if let Some(existing) = rows
                .iter_mut()
                .rev()
                .find(|item| item.nonce.as_deref() == nonce.as_deref())
            {
                *existing = message;
            }
        } else {
            rows.push_back(message);
        }
        while rows.len() > MESSAGE_CAP {
            rows.pop_front();
        }
        drop(inner);
        self.mark();
    }

    pub fn remove_messages(&self, channel_id: &str, message_ids: &[String]) {
        if message_ids.is_empty() {
            return;
        }
        let ids = message_ids
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        let mut inner = self.inner.write();
        if let Some(rows) = inner.messages.get_mut(channel_id) {
            rows.retain(|message| !ids.contains(message.id.as_str()));
        }
        drop(inner);
        self.mark();
    }

    pub fn snapshot(&self, include_messages: bool) -> Snapshot {
        let inner = self.inner.read();
        let messages = if include_messages {
            inner
                .messages
                .iter()
                .map(|(id, rows)| {
                    let kept = rows
                        .iter()
                        .rev()
                        .take(SNAPSHOT_CAP)
                        .cloned()
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    (id.clone(), kept)
                })
                .collect()
        } else {
            HashMap::new()
        };
        Snapshot {
            user: inner.user.clone(),
            guilds: inner.guilds.clone(),
            channels: inner.channels.clone(),
            dms: inner.dms.clone(),
            messages,
            users: inner.users.clone(),
            folders: inner.folders.clone(),
            active_guild_id: inner.active_guild_id.clone(),
            active_channel_id: inner.active_channel_id.clone(),
        }
    }

    pub fn flush_if_dirty(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        self.persist();
    }

    pub fn clear(&self) {
        *self.inner.write() = Inner {
            user: None,
            guilds: Vec::new(),
            channels: Vec::new(),
            dms: Vec::new(),
            messages: HashMap::new(),
            users: HashMap::new(),
            folders: Vec::new(),
            active_guild_id: None,
            active_channel_id: None,
        };
        let _ = std::fs::remove_file(&self.path);
        self.dirty.store(false, Ordering::Relaxed);
    }

    fn mark(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn persist(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let snapshot = self.snapshot(true);
        if let Ok(bytes) = serde_json::to_vec(&snapshot) {
            let _ = std::fs::write(&self.path, bytes);
        }
    }
}

fn sort_dms(mut dms: Vec<ChannelDto>) -> Vec<ChannelDto> {
    dms.sort_by(|a, b| snowflake_desc(&a.last_message_id, &b.last_message_id));
    dms
}

fn snowflake_desc(a: &Option<String>, b: &Option<String>) -> std::cmp::Ordering {
    match (parse_snowflake(a), parse_snowflake(b)) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn parse_snowflake(value: &Option<String>) -> Option<u128> {
    value.as_ref()?.parse().ok()
}
