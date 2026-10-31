use super::models::{
    ChannelDto, ChatMessageDto, GuildAccess, GuildDto, GuildFolderDto, PresenceDto, Snapshot,
    UserDto,
};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const MESSAGE_CAP: usize = 200;
const SNAPSHOT_CAP: usize = 50;
const NOTICE_CAP: usize = 20;

#[derive(Clone, Serialize)]
pub struct Notice {
    pub id: String,
    pub channel: String,
    pub author: String,
    pub content: String,
}

#[derive(Clone, Default)]
pub struct MemberList {
    pub id: String,
    pub slots: Vec<Option<MemberListEntry>>,
    pub groups: Vec<(String, usize)>,
}
#[derive(Clone)]
pub enum MemberListEntry {
    Group(String, usize),
    User(String),
}

/// What the signed-in user may do in a channel. Viewing differs from reading
/// only for forums, which have no message history of their own.
#[derive(Clone, Copy, PartialEq)]
pub enum Access {
    View,
    Read,
    Send,
}

/// Someone connected to a voice channel. Self and server mute stay distinct.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceUser {
    pub id: String,
    #[serde(skip)]
    channel: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    mute: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    deaf: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    server_mute: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    server_deaf: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    video: bool,
}

impl VoiceUser {
    fn parse(value: &serde_json::Value) -> Option<Self> {
        let flag = |keys: &[&str]| keys.iter().any(|key| value[*key].as_bool() == Some(true));
        Some(Self {
            id: value["user_id"].as_str()?.to_owned(),
            channel: value["channel_id"].as_str()?.to_owned(),
            mute: flag(&["self_mute"]),
            deaf: flag(&["self_deaf"]),
            server_mute: flag(&["mute", "suppress"]),
            server_deaf: flag(&["deaf"]),
            stream: flag(&["self_stream"]),
            video: flag(&["self_video"]),
        })
    }
}

/// The saved snapshot plus runtime-only state.
#[derive(Default)]
struct Inner {
    saved: Snapshot,
    member_lists: HashMap<String, MemberList>,
    presences: HashMap<String, PresenceDto>,
    /// Voice states keyed by guild and user.
    voice: HashMap<(String, String), VoiceUser>,
    /// Ringing users of each active private call, keyed by channel.
    calls: HashMap<String, Vec<String>>,
    revisions: HashMap<String, MessageRevision>,
    live_tail: Option<LiveTail>,
    pending_reads: HashMap<String, String>,
    notices: VecDeque<Notice>,
}

struct LiveTail {
    after: String,
    history: u64,
    messages: VecDeque<ChatMessageDto>,
}

#[derive(Clone, Copy, Default)]
pub struct MessageRevision {
    content: u64,
    history: u64,
}

impl MessageRevision {
    fn invalidate(&mut self) {
        self.content += 1;
        self.history += 1;
    }
}

impl Deref for Inner {
    type Target = Snapshot;
    fn deref(&self) -> &Snapshot {
        &self.saved
    }
}

impl DerefMut for Inner {
    fn deref_mut(&mut self) -> &mut Snapshot {
        &mut self.saved
    }
}

#[derive(Clone, Default)]
pub struct CacheStore {
    inner: Arc<RwLock<Inner>>,
    path: Arc<Mutex<Option<PathBuf>>>,
    dirty: Arc<AtomicBool>,
}

impl CacheStore {
    /// Reads the saved state without copying it.
    pub fn read<T>(&self, read: impl FnOnce(&Snapshot) -> T) -> T {
        read(&self.inner.read())
    }

    pub fn notices(&self) -> Vec<Notice> {
        let mut inner = self.inner.write();
        let Inner { saved, notices, .. } = &mut *inner;
        notices.retain(|notice| {
            saved.can(&notice.channel, Access::Read)
                && saved.message_unread(&notice.channel, &notice.id)
        });
        notices.iter().cloned().collect()
    }

    pub fn clear_notices(&self) {
        self.inner.write().notices.clear();
    }

    pub fn reaction_event(&self, data: &serde_json::Value, kind: &str) -> Option<()> {
        let channel = data["channel_id"].as_str()?;
        let id = data["message_id"].as_str()?;
        let mut inner = self.inner.write();
        inner
            .revisions
            .entry(channel.to_owned())
            .or_default()
            .invalidate();
        let user_id = data["user_id"].as_str().unwrap_or_default();
        let own = inner.user.as_ref().is_some_and(|user| user_id == user.id);
        let burst = data["burst"].as_bool().unwrap_or(false) || data["type"].as_u64() == Some(1);
        let user = data
            .pointer("/member/user")
            .and_then(super::rest::parse_user)
            .or_else(|| inner.users.get(user_id).cloned());
        if let Some(user) = &user {
            inner
                .users
                .entry(user.id.clone())
                .or_insert_with(|| user.clone());
        }
        let message = inner
            .messages
            .get_mut(channel)?
            .iter_mut()
            .find(|message| message.id == id)?;
        if kind == "MESSAGE_REACTION_REMOVE_ALL" {
            message.reactions.clear();
        } else {
            let emoji = super::models::ReactionEmojiDto::deserialize(data.get("emoji")?).ok()?;
            let position = message.reactions.iter().position(|reaction| {
                if emoji.id.is_some() {
                    reaction.emoji.id == emoji.id
                } else {
                    reaction.emoji.id.is_none() && reaction.emoji.name == emoji.name
                }
            });
            if let Some(index) = position {
                let reaction = &mut message.reactions[index];
                let details = reaction
                    .count_details
                    .get_or_insert(super::models::ReactionCounts {
                        normal: reaction.count,
                        burst: 0,
                    });
                let count = if burst {
                    &mut details.burst
                } else {
                    &mut details.normal
                };
                let added = kind == "MESSAGE_REACTION_ADD";
                if added {
                    *count = count.saturating_add(1);
                    reaction.count = reaction.count.saturating_add(1);
                    if let Some(user) =
                        user.filter(|user| !reaction.users.iter().any(|row| row.id == user.id))
                    {
                        if reaction.users.len() < super::models::REACTION_AVATAR_COUNT {
                            reaction.users.push(user);
                        }
                    }
                } else {
                    *count = count.saturating_sub(1);
                    reaction.count = if kind == "MESSAGE_REACTION_REMOVE_EMOJI" {
                        0
                    } else {
                        reaction.count.saturating_sub(1)
                    };
                    reaction.users.retain(|user| user.id != user_id);
                }
                if own && !burst {
                    reaction.me = added;
                }
                if reaction.count == 0 {
                    message.reactions.remove(index);
                }
            } else if kind == "MESSAGE_REACTION_ADD" {
                message.reactions.push(super::models::ReactionDto {
                    users: user.into_iter().collect(),
                    count_details: Some(super::models::ReactionCounts {
                        normal: u32::from(!burst),
                        burst: u32::from(burst),
                    }),
                    emoji,
                    count: 1,
                    me: own && !burst,
                });
            }
        }
        drop(inner);
        self.mark();
        Some(())
    }

    pub fn presence(&self, id: &str) -> PresenceDto {
        let mut presence = self
            .inner
            .read()
            .presences
            .get(id)
            .cloned()
            .unwrap_or_default();
        if presence
            .custom_status
            .as_ref()
            .and_then(|status| status.expires_at.as_deref())
            .and_then(|expires| chrono::DateTime::parse_from_rfc3339(expires).ok())
            .is_some_and(|expires| expires <= chrono::Utc::now())
        {
            presence.custom_status = None;
        }
        presence
    }

    pub fn update_presence(&self, id: &str, value: &serde_json::Value) {
        if id.is_empty() {
            return;
        }
        let mut inner = self.inner.write();
        let presence = inner.presences.entry(id.to_owned()).or_default();
        if let Some(status) = value
            .get("status")
            .and_then(|v| v.as_str())
            .filter(|status| matches!(*status, "online" | "idle" | "dnd" | "offline" | "invisible"))
        {
            presence.status = if status == "invisible" {
                "offline"
            } else {
                status
            }
            .to_owned();
        }
        let custom = value.get("custom_status").or_else(|| {
            value
                .get("activities")
                .and_then(|activities| activities.as_array())
                .map(|activities| {
                    activities
                        .iter()
                        .find(|activity| activity["type"] == 4)
                        .unwrap_or(&serde_json::Value::Null)
                })
        });
        if let Some(custom) = custom {
            presence.custom_status = super::models::CustomStatusDto::deserialize(custom)
                .ok()
                .map(|mut status| {
                    if status.emoji.is_none()
                        && (custom.get("emoji_id").is_some() || custom.get("emoji_name").is_some())
                    {
                        status.emoji = Some(super::models::ReactionEmojiDto {
                            id: custom
                                .get("emoji_id")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned),
                            name: custom
                                .get("emoji_name")
                                .and_then(|v| v.as_str())
                                .map(str::to_owned),
                        });
                    }
                    status
                })
                .filter(|status| {
                    status.text.as_ref().is_some_and(|text| !text.is_empty())
                        || status.emoji.as_ref().is_some_and(|emoji| {
                            emoji.id.is_some()
                                || emoji.name.as_ref().is_some_and(|name| !name.is_empty())
                        })
                });
        }
    }

    pub fn clear_live(&self) {
        let mut inner = self.inner.write();
        inner.presences.clear();
        inner.voice.clear();
        inner.calls.clear();
        inner.live_tail = None;
    }

    /// Dropping a tail also invalidates loads that would have needed its arrivals.
    pub fn clear_live_tail(&self) {
        let mut inner = self.inner.write();
        if inner.live_tail.take().is_some() {
            if let Some(channel) = inner.active_channel_id.clone() {
                inner.revisions.entry(channel).or_default().history += 1;
            }
        }
    }

    /// Applies one voice state, or replaces a guild's states when given a list.
    /// Returns the users that are not cached yet.
    pub fn update_voice(&self, guild: &str, states: &serde_json::Value) -> Vec<String> {
        let mut inner = self.inner.write();
        let states = match states.as_array() {
            Some(states) => {
                inner.voice.retain(|(id, _), _| id != guild);
                states.as_slice()
            }
            None => {
                if let Some(user) = states["user_id"].as_str() {
                    inner.voice.remove(&(guild.to_owned(), user.to_owned()));
                }
                std::slice::from_ref(states)
            }
        };
        let mut missing = Vec::new();
        for user in states.iter().filter_map(VoiceUser::parse) {
            if !inner.users.contains_key(&user.id) {
                missing.push(user.id.clone());
            }
            inner
                .voice
                .insert((guild.to_owned(), user.id.clone()), user);
        }
        missing
    }

    /// Records a private call and who it rings, or removes it.
    pub fn update_call(&self, channel: &str, ringing: Option<Vec<String>>) {
        let mut inner = self.inner.write();
        match ringing {
            Some(ringing) => {
                inner.calls.insert(channel.to_owned(), ringing);
            }
            // An ended call keeps no participants.
            None => {
                inner.calls.remove(channel);
                inner
                    .voice
                    .retain(|(guild, _), user| !guild.is_empty() || user.channel != channel);
            }
        }
    }

    pub fn calls(&self) -> HashMap<String, Vec<String>> {
        self.inner.read().calls.clone()
    }

    /// Voice participants grouped by channel, in a stable order so equal snapshots serialize equally.
    pub fn voice(&self) -> BTreeMap<String, Vec<VoiceUser>> {
        let mut channels: BTreeMap<String, Vec<VoiceUser>> = BTreeMap::new();
        for user in self.inner.read().voice.values() {
            channels
                .entry(user.channel.clone())
                .or_default()
                .push(user.clone());
        }
        channels
    }

    pub fn can(&self, channel: &str, access: Access) -> bool {
        self.inner.read().can(channel, access)
    }

    fn load(path: &Path) -> Inner {
        let mut saved = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Snapshot>(&bytes).ok())
            .unwrap_or_default();
        sort_dms(&mut saved.dms);
        Inner {
            saved,
            ..Default::default()
        }
    }

    pub fn user_id(&self) -> Option<String> {
        self.inner.read().user.as_ref().map(|user| user.id.clone())
    }

    pub fn channel(&self, id: &str) -> Option<ChannelDto> {
        self.inner.read().channel(id).cloned()
    }

    pub fn switch_account(&self, path: PathBuf, user_id: &str) {
        self.flush_if_dirty();
        let mut saved_path = self.path.lock();
        if saved_path.as_ref() == Some(&path) {
            return;
        }
        if self.user_id().as_deref() != Some(user_id) {
            let mut loaded = Self::load(&path);
            if loaded.user.as_ref().map(|user| user.id.as_str()) != Some(user_id) {
                loaded = Inner::default();
            }
            *self.inner.write() = loaded;
        }
        *saved_path = Some(path);
        self.mark();
    }

    pub fn edit_favorite_sticker(&self, id: &str, index: Option<usize>) {
        super::rest::edit_favorites(&mut self.inner.write().favorite_stickers, id, index);
        self.mark();
    }

    pub fn set_favorite_emojis(&self, emojis: Vec<String>) {
        self.inner.write().favorite_emojis = emojis;
        self.mark();
    }

    pub fn set_frequent_emojis(&self, emojis: Vec<String>) {
        self.inner.write().frequent_emojis = emojis;
        self.mark();
    }

    pub fn set_user(&self, mut user: UserDto) {
        let mut inner = self.inner.write();
        if let Some(previous) = inner.users.get(&user.id).or_else(|| {
            inner
                .user
                .as_ref()
                .filter(|previous| previous.id == user.id)
        }) {
            user.retain_profile(previous);
        }
        inner.users.insert(user.id.clone(), user.clone());
        inner.user = Some(user);
        resolve_dm_names(&mut inner);
        drop(inner);
        self.mark();
    }

    pub fn mutual_friends(&self, id: &str) -> Option<Vec<UserDto>> {
        let inner = self.inner.read();
        inner
            .mutual_friends
            .get(id)?
            .iter()
            .map(|id| inner.users.get(id).cloned())
            .collect()
    }

    pub fn remember_mutual_friends(&self, id: &str, users: Vec<UserDto>) {
        let ids = users.iter().map(|user| user.id.clone()).collect();
        self.remember_users(users);
        self.inner.write().mutual_friends.insert(id.to_owned(), ids);
        self.mark();
    }

    pub fn remember_users(&self, users: impl IntoIterator<Item = UserDto>) {
        let mut inner = self.inner.write();
        for mut user in users {
            if !user.id.is_empty() && !user.name.is_empty() {
                if let Some(previous) = inner.users.get(&user.id) {
                    user.retain_profile(previous);
                }
                inner.users.insert(user.id.clone(), user);
            }
        }
        resolve_dm_names(&mut inner);
        drop(inner);
        self.mark();
    }

    pub fn remember_profile(&self, user: UserDto) {
        if user.id.is_empty() || user.name.is_empty() {
            return;
        }
        let mut inner = self.inner.write();
        inner.users.insert(user.id.clone(), user);
        resolve_dm_names(&mut inner);
        drop(inner);
        self.mark();
    }

    pub fn update_read_state(&self, channel: &str, message: &str, mentions: u32, reset: bool) {
        let mut inner = self.inner.write();
        if reset {
            inner.pending_reads.remove(channel);
        }
        let previous = inner.last_read.entry(channel.to_owned()).or_default();
        if reset || message.parse::<u64>().unwrap_or(0) >= previous.parse::<u64>().unwrap_or(0) {
            *previous = message.to_owned();
            inner.mention_counts.insert(channel.to_owned(), mentions);
            drop(inner);
            self.mark();
        }
    }

    pub fn mark_read(&self, channel: &str) {
        if let Some(latest) = self.last_message_id(channel) {
            self.update_read_state(channel, &latest, 0, false);
        }
    }

    pub fn mark_read_for_sync(&self, channel: &str) {
        let last_read = |cache: &Self| cache.inner.read().last_read.get(channel).cloned();
        let before = last_read(self);
        self.mark_read(channel);
        if let Some(message) = last_read(self).filter(|message| Some(message) != before.as_ref()) {
            self.inner
                .write()
                .pending_reads
                .insert(channel.to_owned(), message);
        }
    }

    pub fn take_pending_reads(&self) -> Vec<(String, String)> {
        self.inner.write().pending_reads.drain().collect()
    }

    pub fn retry_reads(&self, messages: Vec<(String, String)>) {
        let mut inner = self.inner.write();
        for (channel, message) in messages {
            if inner.last_read.get(&channel) == Some(&message) {
                inner.pending_reads.entry(channel).or_insert(message);
            }
        }
    }

    pub fn user(&self, id: &str) -> Option<UserDto> {
        let inner = self.inner.read();
        inner
            .users
            .get(id)
            .cloned()
            .or_else(|| inner.user.as_ref().filter(|user| user.id == id).cloned())
    }

    pub fn toggle_pinned_dm(&self, id: &str) {
        let mut inner = self.inner.write();
        if !inner.dms.iter().any(|row| row.id == id) {
            return;
        }
        if let Some(index) = inner.pinned_dms.iter().position(|row| row == id) {
            inner.pinned_dms.remove(index);
        } else {
            inner.pinned_dms.push(id.to_owned());
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn channel_users(&self, channel: &str) -> Vec<UserDto> {
        let inner = self.inner.read();
        let Some(channel_row) = inner.channel(channel) else {
            return Vec::new();
        };
        let guild = inner.guild_of(channel_row);
        let mut ids: HashSet<_> = channel_row.recipient_ids.iter().cloned().collect();
        if let Some(guild) = guild {
            ids.extend(guild.members.keys().cloned());
        }
        if let Some(messages) = inner.messages.get(channel) {
            ids.extend(messages.iter().map(|row| row.author_id.clone()));
            ids.extend(
                messages
                    .iter()
                    .flat_map(|row| row.mentions.iter().map(|user| user.id.clone())),
            );
        }
        if let Some(user) = &inner.user {
            ids.insert(user.id.clone());
        }
        ids.into_iter()
            .filter_map(|id| {
                let mut user = inner
                    .users
                    .get(&id)
                    .or_else(|| inner.user.as_ref().filter(|user| user.id == id))?
                    .clone();
                if let Some(guild) = guild {
                    if let Some(member) = guild.members.get(&id) {
                        member.apply(&guild.id, &mut user);
                    }
                }
                Some(user)
            })
            .collect()
    }

    pub fn member_list(&self, guild: &str) -> Option<MemberList> {
        self.inner.read().member_lists.get(guild).cloned()
    }

    pub fn update_member_list(&self, data: &serde_json::Value) {
        let Some(guild) = data["guild_id"].as_str() else {
            return;
        };
        let mut inner = self.inner.write();
        let list = inner.member_lists.entry(guild.to_owned()).or_default();
        let id = data["id"].as_str().unwrap_or_default();
        if list.id != id {
            *list = MemberList {
                id: id.to_owned(),
                ..Default::default()
            };
        }
        if let Some(groups) = data["groups"].as_array() {
            list.groups = groups
                .iter()
                .filter_map(|group| {
                    Some((
                        group["id"].as_str()?.to_owned(),
                        group["count"].as_u64()? as usize,
                    ))
                })
                .collect();
        }
        let count: usize = list.groups.iter().map(|(_, count)| count + 1).sum();
        if count > 0 {
            list.slots.resize(list.slots.len().max(count), None);
        }
        let entry = |item: &serde_json::Value| {
            if let Some(group) = item.get("group") {
                Some(MemberListEntry::Group(
                    group["id"].as_str()?.to_owned(),
                    group["count"]
                        .as_u64()
                        .map(|count| count as usize)
                        .or_else(|| {
                            list.groups
                                .iter()
                                .find(|(id, _)| Some(id.as_str()) == group["id"].as_str())
                                .map(|(_, count)| *count)
                        })
                        .unwrap_or(0),
                ))
            } else {
                Some(MemberListEntry::User(
                    item.pointer("/member/user/id")?.as_str()?.to_owned(),
                ))
            }
        };
        for op in data["ops"].as_array().into_iter().flatten() {
            let index = op["index"].as_u64().unwrap_or(0) as usize;
            match op["op"].as_str().unwrap_or_default() {
                "SYNC" => {
                    let start = op["range"][0].as_u64().unwrap_or(0) as usize;
                    for (offset, item) in op["items"].as_array().into_iter().flatten().enumerate() {
                        if let Some(slot) = list.slots.get_mut(start + offset) {
                            *slot = entry(item);
                        }
                    }
                }
                "INSERT" if index <= list.slots.len() => {
                    list.slots.insert(index, entry(&op["item"]))
                }
                "UPDATE" => {
                    if let Some(slot) = list.slots.get_mut(index) {
                        *slot = entry(&op["item"]);
                    }
                }
                "DELETE" if index < list.slots.len() => {
                    list.slots.remove(index);
                }
                "INVALIDATE" => {
                    let start = op["range"][0].as_u64().unwrap_or(0) as usize;
                    let end = op["range"][1].as_u64().unwrap_or(0) as usize;
                    for i in start..=end.min(list.slots.len().saturating_sub(1)) {
                        if let Some(slot) = list.slots.get_mut(i) {
                            *slot = None;
                        }
                    }
                }
                _ => {}
            }
        }
        if data["groups"].is_array() {
            list.slots.resize(count, None);
        }
        let mut index = 0;
        for (id, count) in &list.groups {
            if let Some(slot) = list.slots.get_mut(index) {
                *slot = Some(MemberListEntry::Group(id.clone(), *count));
            }
            index += count + 1;
        }
    }

    pub fn role_details(&self, channel: &str) -> Vec<super::models::RoleDto> {
        let inner = self.inner.read();
        inner
            .channel_access(channel)
            .map(|access| access.role_details.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn member(&self, guild: &str, user: &str) -> Option<super::models::GuildMemberDto> {
        self.inner
            .read()
            .guilds
            .iter()
            .find(|row| row.id == guild)?
            .members
            .get(user)
            .cloned()
    }

    pub fn update_member(&self, guild: &str, user: &str, value: &serde_json::Value) {
        let mut inner = self.inner.write();
        if let Some(guild) = inner.guilds.iter_mut().find(|row| row.id == guild) {
            guild
                .members
                .entry(user.to_owned())
                .or_default()
                .update(value);
        }
        drop(inner);
        self.mark();
    }

    pub fn set_active(&self, guild_id: Option<String>, channel_id: Option<&str>) {
        let mut inner = self.inner.write();
        if let Some(previous) = inner.active_channel_id.clone() {
            let previous_guild = inner.active_guild_id.clone().unwrap_or_default();
            inner.visited_channels.insert(previous_guild, previous);
        }
        let key = guild_id.clone().unwrap_or_default();
        if let Some(channel) = channel_id {
            inner
                .visited_channels
                .insert(key.clone(), channel.to_owned());
        }
        let channel_id = channel_id
            .map(str::to_owned)
            .or_else(|| inner.visited_channels.get(&key).cloned());
        if inner.active_channel_id != channel_id {
            inner.live_tail = None;
        }
        inner.active_channel_id = channel_id;
        inner.active_guild_id = guild_id;
        drop(inner);
        self.mark();
    }

    pub fn replace_structure(
        &self,
        guilds: Vec<GuildDto>,
        channels: HashMap<String, Vec<ChannelDto>>,
        dms: Vec<ChannelDto>,
    ) {
        let mut inner = self.inner.write();
        // READY often ships unavailable stubs. Keep richer name/icon from cache.
        inner.member_lists.clear();
        let previous = std::mem::take(&mut inner.guilds);
        inner.guilds = guilds
            .into_iter()
            .map(|mut guild| {
                if let Some(old) = previous.iter().find(|item| item.id == guild.id) {
                    guild.banner = guild.banner.or_else(|| old.banner.clone());
                    guild.emojis = guild.emojis.or_else(|| old.emojis.clone());
                    guild.stickers = guild.stickers.or_else(|| old.stickers.clone());
                    for (id, member) in &old.members {
                        guild
                            .members
                            .entry(id.clone())
                            .or_insert_with(|| member.clone());
                    }
                    if guild.name.is_empty() && !old.name.is_empty() {
                        guild.name = old.name.clone();
                    }
                    if guild.icon.is_none() && old.icon.is_some() {
                        guild.icon = old.icon.clone();
                    }
                    if guild.access.is_none() {
                        guild.access = old.access.clone();
                    } else if let (Some(access), Some(previous)) = (&mut guild.access, &old.access)
                    {
                        access.retain_missing(previous);
                    }
                }
                guild
            })
            .collect();
        let guilds: HashSet<_> = inner.guilds.iter().map(|guild| guild.id.as_str()).collect();
        let channel_ids: HashSet<_> = channels
            .values()
            .flatten()
            .map(|channel| channel.id.as_str())
            .collect();
        let dm_ids: HashSet<_> = dms.iter().map(|channel| channel.id.as_str()).collect();
        let removed = inner
            .channels
            .iter()
            .filter(|channel| {
                channel.guild_id.as_deref().is_none_or(|guild| {
                    !guilds.contains(guild)
                        || (channels.contains_key(guild)
                            && !channel_ids.contains(channel.id.as_str()))
                })
            })
            .chain(
                inner
                    .dms
                    .iter()
                    .filter(|channel| !dm_ids.contains(channel.id.as_str())),
            )
            .map(|channel| channel.id.clone())
            .collect();
        if inner
            .active_guild_id
            .as_deref()
            .is_some_and(|id| !guilds.contains(id))
        {
            inner.active_guild_id = None;
        }
        inner.remove_channels(&removed);
        // Keep channels only for guilds whose READY stubs omitted the list.
        inner.channels.retain(|channel| {
            !channel
                .guild_id
                .as_ref()
                .is_some_and(|guild| channels.contains_key(guild))
        });
        inner.channels.extend(channels.into_values().flatten());
        inner.dms = dms;
        sort_dms(&mut inner.dms);
        resolve_dm_names(&mut inner);
        drop(inner);
        self.mark();
    }

    pub fn merge_guild_meta(&self, updates: Vec<GuildDto>) {
        let mut inner = self.inner.write();
        for update in updates {
            if let Some(existing) = inner.guilds.iter_mut().find(|item| item.id == update.id) {
                if !update.name.is_empty() {
                    existing.name = update.name;
                }
                existing.icon = update.icon;
                existing.banner = update.banner.or_else(|| existing.banner.take());
            }
        }
        drop(inner);
        self.mark();
    }

    pub fn set_folders(&self, folders: Vec<GuildFolderDto>) {
        self.inner.write().folders = folders;
        self.mark();
    }

    pub fn order_guilds(&self, positions: &[String]) {
        if positions.is_empty() {
            return;
        }
        sort_guilds(&mut self.inner.write().guilds, positions);
        self.mark();
    }

    pub fn replace_dms_if_unchanged(&self, dms: Vec<ChannelDto>, expected: &[ChannelDto]) -> bool {
        let mut inner = self.inner.write();
        if inner.dms != expected {
            return false;
        }
        let ids: HashSet<_> = dms.iter().map(|channel| channel.id.as_str()).collect();
        let removed = inner
            .dms
            .iter()
            .filter(|channel| !ids.contains(channel.id.as_str()))
            .map(|channel| channel.id.clone())
            .collect();
        inner.remove_channels(&removed);
        inner.dms = dms;
        sort_dms(&mut inner.dms);
        resolve_dm_names(&mut inner);
        drop(inner);
        self.mark();
        true
    }

    pub fn upsert_guild(
        &self,
        guild: GuildDto,
        channels: Option<Vec<ChannelDto>>,
        icon_known: bool,
        banner_known: bool,
    ) {
        let mut inner = self.inner.write();
        let id = guild.id.clone();
        if let Some(existing) = inner.guilds.iter_mut().find(|item| item.id == id) {
            if guild.emojis.is_some() {
                existing.emojis = guild.emojis;
            }
            if guild.stickers.is_some() {
                existing.stickers = guild.stickers;
            }
            existing.members.extend(guild.members);
            if banner_known {
                existing.banner = guild.banner;
            }
            if !guild.name.is_empty() {
                existing.name = guild.name;
            }
            if icon_known {
                existing.icon = guild.icon;
            }
            if let Some(mut access) = guild.access {
                if let Some(old) = existing.access.as_ref() {
                    access.retain_missing(old);
                }
                existing.access = Some(access);
            }
        } else {
            inner.guilds.push(guild);
        }
        if let Some(channels) = channels {
            let ids: HashSet<_> = channels.iter().map(|channel| channel.id.as_str()).collect();
            let removed = inner
                .channels
                .iter()
                .filter(|channel| {
                    channel.guild_id.as_deref() == Some(id.as_str())
                        && !ids.contains(channel.id.as_str())
                })
                .map(|channel| channel.id.clone())
                .collect();
            inner.remove_channels(&removed);
            inner
                .channels
                .retain(|channel| channel.guild_id.as_deref() != Some(id.as_str()));
            inner.channels.extend(channels);
        }
        drop(inner);
        self.mark();
    }

    pub fn update_expressions(&self, data: &serde_json::Value, stickers: bool) {
        let mut inner = self.inner.write();
        if let Some(guild) = inner
            .guilds
            .iter_mut()
            .find(|guild| data["guild_id"].as_str() == Some(&guild.id))
        {
            if stickers {
                if let Some(Ok(rows)) = data.get("stickers").map(super::models::items) {
                    guild.stickers = Some(rows);
                }
            } else if let Some(Ok(rows)) = data.get("emojis").map(super::models::items) {
                guild.emojis = Some(rows);
            }
        }
        drop(inner);
        self.mark();
    }

    pub fn remove_guild(&self, id: &str) {
        let mut inner = self.inner.write();
        let channels: HashSet<_> = inner
            .channels
            .iter()
            .filter(|channel| channel.guild_id.as_deref() == Some(id))
            .map(|channel| channel.id.clone())
            .collect();
        inner.guilds.retain(|guild| guild.id != id);
        inner.voice.retain(|(guild, _), _| guild != id);
        inner.remove_channels(&channels);
        for folder in &mut inner.folders {
            folder.guild_ids.retain(|guild| guild != id);
        }
        if inner.active_guild_id.as_deref() == Some(id) {
            inner.active_guild_id = None;
            inner.active_channel_id = None;
            inner.live_tail = None;
        }
        drop(inner);
        self.mark();
    }

    pub fn update_access(&self, data: &serde_json::Value, event: &str) {
        let mut inner = self.inner.write();
        let Some(guild_id) = data
            .get("guild_id")
            .or_else(|| {
                if event == "GUILD_UPDATE" {
                    data.get("id")
                } else {
                    None
                }
            })
            .and_then(|v| v.as_str())
        else {
            return;
        };
        let user = inner.user.as_ref().map(|u| u.id.clone());
        let Some(access) = inner
            .guilds
            .iter_mut()
            .find(|g| g.id == guild_id)
            .and_then(|g| g.access.as_mut())
        else {
            return;
        };
        if event == "GUILD_UPDATE" {
            if let Some(owner) = data.get("owner_id").and_then(|v| v.as_str()) {
                access.owner_id = owner.to_string();
            }
        } else if event == "GUILD_MEMBER_UPDATE" {
            if super::permissions::member_id(data) != user.as_deref() {
                return;
            }
            access.member_roles = super::permissions::member_roles(data);
            if data.get("communication_disabled_until").is_some() {
                access.communication_disabled_until = super::permissions::member_timeout(data);
                access.timeout_known = true;
            }
        } else if event == "GUILD_ROLE_DELETE" {
            if let Some(id) = data.get("role_id").and_then(|v| v.as_str()) {
                access.roles.remove(id);
                access.role_details.remove(id);
            }
            if let Some(roles) = &mut access.member_roles {
                roles.retain(|id| access.roles.contains_key(id));
            }
        } else if let Some(Ok(role)) = data
            .get("role")
            .map(super::permissions::WireRole::deserialize)
        {
            role.insert(access);
        }
        drop(inner);
        self.mark();
    }

    pub fn sync_threads(&self, guild: &str, parents: Option<&[String]>, threads: Vec<ChannelDto>) {
        let mut inner = self.inner.write();
        let removed: HashSet<_> = inner
            .channels
            .iter()
            .filter(|channel| {
                matches!(channel.kind, 10..=12)
                    && !channel.thread_archived
                    && channel.guild_id.as_deref() == Some(guild)
                    && parents.is_none_or(|parents| {
                        channel
                            .parent_id
                            .as_ref()
                            .is_some_and(|parent| parents.contains(parent))
                    })
                    && !threads.iter().any(|thread| thread.id == channel.id)
            })
            .map(|channel| channel.id.clone())
            .collect();
        inner.remove_channels(&removed);
        for thread in threads {
            if let Some(existing) = inner.channels.iter_mut().find(|row| row.id == thread.id) {
                *existing = thread;
            } else {
                inner.channels.push(thread);
            }
        }
        drop(inner);
        self.mark();
    }

    pub fn upsert_channel(&self, mut channel: ChannelDto, dm: bool) {
        let mut inner = self.inner.write();
        let list = if dm {
            &mut inner.dms
        } else {
            &mut inner.channels
        };
        if let Some(existing) = list.iter_mut().find(|item| item.id == channel.id) {
            if matches!(channel.kind, 10..=12) {
                channel.thread_joined |= existing.thread_joined;
            }
            *existing = channel;
        } else {
            list.push(channel);
        }
        if dm {
            sort_dms(&mut inner.dms);
        }
        resolve_dm_names(&mut inner);
        drop(inner);
        self.mark();
    }

    pub fn thread_membership(&self, id: &str, joined: bool) {
        let mut inner = self.inner.write();
        if let Some(channel) = inner.channels.iter_mut().find(|row| row.id == id) {
            channel.thread_joined = joined;
        }
        drop(inner);
        self.mark();
    }

    pub fn remove_channel(&self, id: &str) {
        let mut inner = self.inner.write();
        inner.remove_channels(&HashSet::from([id.to_owned()]));
        drop(inner);
        self.mark();
    }

    pub fn messages(&self, channel: &str) -> Vec<ChatMessageDto> {
        self.inner
            .read()
            .messages
            .get(channel)
            .map(|rows| rows.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn message_revision(&self, channel_id: &str) -> MessageRevision {
        self.inner
            .read()
            .revisions
            .get(channel_id)
            .copied()
            .unwrap_or_default()
    }

    pub fn reaches_latest(&self, channel: &str) -> bool {
        self.inner.read().reaches_latest(channel)
    }

    pub fn last_message_id(&self, channel: &str) -> Option<String> {
        let inner = self.inner.read();
        inner
            .channel(channel)
            .and_then(|row| row.last_message_id.clone())
    }

    pub fn update_last_messages(&self, guild: &str, channels: &serde_json::Value) {
        let mut inner = self.inner.write();
        let mut changed = false;
        for update in channels.as_array().into_iter().flatten() {
            let (Some(id), Some(message)) =
                (update["id"].as_str(), update["last_message_id"].as_str())
            else {
                continue;
            };
            if let Some(channel) = inner
                .channels
                .iter_mut()
                .find(|channel| channel.id == id && channel.guild_id.as_deref() == Some(guild))
            {
                changed |= advance_message_id(&mut channel.last_message_id, message);
            }
        }
        drop(inner);
        if changed {
            self.mark();
        }
    }

    /// Discord keeps `last_message_id` after the newest message is deleted.
    /// A fetch that reached the newest message corrects it, unless a message arrived meanwhile.
    pub fn confirm_latest(&self, channel: &str, expected: Option<&str>) {
        let mut inner = self.inner.write();
        let saved = &mut inner.saved;
        let newest = saved
            .messages
            .get(channel)
            .and_then(|rows| rows.back())
            .map(|row| row.id.clone());
        let Some(row) = saved
            .channels
            .iter_mut()
            .chain(&mut saved.dms)
            .find(|row| row.id == channel)
            .filter(|row| {
                row.last_message_id.as_deref() == expected && row.last_message_id != newest
            })
        else {
            return;
        };
        row.last_message_id = newest;
        drop(inner);
        self.mark();
    }

    pub fn extend_messages(
        &self,
        channel: &str,
        before: &str,
        mut messages: Vec<ChatMessageDto>,
        newer: bool,
        revision: MessageRevision,
    ) -> Option<usize> {
        let mut inner = self.inner.write();
        if revision.history
            != inner
                .revisions
                .get(channel)
                .copied()
                .unwrap_or_default()
                .history
            || inner.active_channel_id.as_deref() != Some(channel)
            || inner
                .messages
                .get(channel)
                .and_then(|rows| if newer { rows.back() } else { rows.front() })
                .is_none_or(|message| message.id != before)
        {
            return None;
        }
        let boundary = before.parse::<u64>().unwrap_or(0);
        messages.retain(|message| {
            message.channel_id == channel
                && message.id.parse::<u64>().is_ok_and(|id| {
                    if newer {
                        id > boundary
                    } else {
                        id < boundary
                    }
                })
        });
        messages.sort_by_key(|message| message.id.parse::<u64>().unwrap_or(0));
        if !newer {
            messages.reverse();
        }
        messages.dedup_by(|a, b| a.id == b.id);
        let count = messages.len();
        if count == 0 {
            return Some(0);
        }
        inner
            .revisions
            .entry(channel.to_owned())
            .or_default()
            .invalidate();
        for message in &messages {
            remember_message(&mut inner, message);
        }
        let rows = inner.messages.get_mut(channel).unwrap();
        for message in messages {
            if newer {
                rows.push_back(message);
            } else {
                rows.push_front(message);
            }
        }
        // Evict the opposite end so history remains contiguous in either direction.
        if newer {
            rows.drain(..rows.len().saturating_sub(MESSAGE_CAP));
        } else {
            rows.truncate(MESSAGE_CAP);
        }
        drop(inner);
        self.mark();
        Some(count)
    }

    pub fn refresh_media_if_unchanged(
        &self,
        channel: &str,
        messages: Vec<ChatMessageDto>,
        revision: MessageRevision,
    ) -> bool {
        let mut inner = self.inner.write();
        if revision.history
            != inner
                .revisions
                .get(channel)
                .copied()
                .unwrap_or_default()
                .history
        {
            return false;
        }
        let Some(rows) = inner.messages.get_mut(channel) else {
            return false;
        };
        for message in messages
            .into_iter()
            .filter(|message| message.channel_id == channel)
        {
            if let Some(row) = rows.iter_mut().find(|row| row.id == message.id) {
                row.attachments = message.attachments;
                row.embeds = message.embeds;
                for (old, new) in row.forwarded.iter_mut().zip(message.forwarded) {
                    old.attachments = new.attachments;
                    old.embeds = new.embeds;
                }
            }
        }
        inner
            .revisions
            .entry(channel.to_owned())
            .or_default()
            .invalidate();
        drop(inner);
        self.mark();
        true
    }

    pub fn replace_messages_if_unchanged(
        &self,
        channel_id: &str,
        mut messages: Vec<ChatMessageDto>,
        revision: MessageRevision,
        latest: bool,
    ) -> bool {
        messages.sort_by_key(|message| message.id.parse::<u64>().unwrap_or(0));
        let mut inner = self.inner.write();
        let current = inner.revisions.get(channel_id).copied().unwrap_or_default();
        if revision.history != current.history {
            return false;
        }
        if latest && inner.active_channel_id.as_deref() == Some(channel_id) {
            if let Some(tail) = inner.live_tail.as_ref().filter(|tail| {
                tail.history == current.history
                    && tail.messages.back().map(|row| row.id.as_str())
                        == inner
                            .channel(channel_id)
                            .and_then(|channel| channel.last_message_id.as_deref())
            }) {
                let newest = messages.last().map_or(0, |message| id_number(&message.id));
                if newest >= id_number(&tail.after) {
                    messages.extend(
                        tail.messages
                            .iter()
                            .filter(|row| id_number(&row.id) > newest)
                            .cloned(),
                    );
                } else if tail.messages.len() == MESSAGE_CAP {
                    // The live tail itself is a complete latest window.
                    messages = tail.messages.iter().cloned().collect();
                } else {
                    return false;
                }
            }
        }
        if let Some(rows) = inner.messages.get(channel_id) {
            if latest && revision.content != current.content {
                let newest = messages.last().map_or(0, |message| id_number(&message.id));
                if let Some(index) = rows.iter().position(|row| id_number(&row.id) == newest) {
                    messages.extend(rows.iter().skip(index + 1).cloned());
                } else if rows.len() == MESSAGE_CAP
                    && inner.reaches_latest(channel_id)
                    && rows.front().is_some_and(|row| id_number(&row.id) > newest)
                {
                    // Live traffic evicted the entire REST page.
                    messages = rows.iter().cloned().collect();
                } else if rows.back().is_some_and(|row| id_number(&row.id) > newest) {
                    // Without a shared boundary, continuity is unknown.
                    return false;
                }
            }
            for message in &mut messages {
                if let Some(old) = rows.iter().find(|row| row.id == message.id) {
                    super::rest::carry_over(message, old);
                }
            }
        }
        if messages.len() > MESSAGE_CAP {
            let excess = messages.len() - MESSAGE_CAP;
            messages.drain(0..excess);
        }
        inner
            .revisions
            .entry(channel_id.to_string())
            .or_default()
            .invalidate();
        for message in &messages {
            remember_message(&mut inner, message);
        }
        if let Some(dm) = inner
            .dms
            .iter_mut()
            .find(|channel| channel.id == channel_id)
        {
            if let Some(last) = messages.last() {
                advance_message_id(&mut dm.last_message_id, &last.id);
            }
            sort_dms(&mut inner.dms);
        }
        inner
            .messages
            .insert(channel_id.to_string(), VecDeque::from(messages));
        if inner.active_channel_id.as_deref() == Some(channel_id) {
            inner.live_tail = None;
        }
        drop(inner);
        self.mark();
        true
    }

    /// Adds or replaces a message. With a revision, nothing changes if the channel
    /// changed since. Messages without one are live arrivals that may count as mentions.
    pub fn push_message(&self, message: ChatMessageDto, revision: Option<MessageRevision>) -> bool {
        let mut inner = self.inner.write();
        if revision.is_some_and(|revision| {
            revision.history
                != inner
                    .revisions
                    .get(&message.channel_id)
                    .copied()
                    .unwrap_or_default()
                    .history
        }) {
            return false;
        }
        let newest = inner
            .messages
            .get(&message.channel_id)
            .and_then(|rows| rows.back())
            .map(|last| id_number(&last.id));
        let newer = newest.is_none_or(|newest| id_number(&message.id) > newest);
        let beyond_context =
            newest.is_some() && newer && !inner.reaches_latest(&message.channel_id);
        let current = inner
            .revisions
            .entry(message.channel_id.clone())
            .or_default();
        current.content += 1;
        if !newer {
            current.history += 1;
        }
        let history = current.history;
        let previous = if beyond_context {
            inner
                .channel(&message.channel_id)
                .and_then(|channel| channel.last_message_id.clone())
        } else {
            None
        };
        if revision.is_none()
            && !inner.notices.iter().any(|notice| notice.id == message.id)
            && !inner
                .messages
                .get(&message.channel_id)
                .is_some_and(|rows| rows.iter().any(|row| row.id == message.id))
            && message.id.parse::<u64>().unwrap_or(0)
                > inner
                    .last_read
                    .get(&message.channel_id)
                    .and_then(|id| id.parse::<u64>().ok())
                    .unwrap_or(0)
            && inner
                .user
                .as_ref()
                .is_some_and(|user| user.id != message.author_id)
            && (inner.dms.iter().any(|dm| dm.id == message.channel_id)
                || inner.mentions_user(&message))
        {
            let count = inner
                .mention_counts
                .entry(message.channel_id.clone())
                .or_default();
            *count = count.saturating_add(1);
            inner.notices.push_back(Notice {
                id: message.id.clone(),
                channel: message.channel_id.clone(),
                author: message.author.clone(),
                content: message.content.chars().take(180).collect(),
            });
            if inner.notices.len() > NOTICE_CAP {
                inner.notices.pop_front();
            }
        }
        remember_message(&mut inner, &message);
        if let Some(channel) = inner
            .channels
            .iter_mut()
            .find(|row| row.id == message.channel_id)
        {
            advance_message_id(&mut channel.last_message_id, &message.id);
        }
        let dm_changed = inner
            .dms
            .iter_mut()
            .find(|row| row.id == message.channel_id)
            .is_some_and(|dm| advance_message_id(&mut dm.last_message_id, &message.id));
        if dm_changed {
            sort_dms(&mut inner.dms);
        }
        if inner
            .user
            .as_ref()
            .is_some_and(|user| user.id == message.author_id)
        {
            inner.mention_counts.remove(&message.channel_id);
            let read = inner
                .last_read
                .entry(message.channel_id.clone())
                .or_default();
            if message.id.parse::<u64>().unwrap_or(0) > read.parse::<u64>().unwrap_or(0) {
                *read = message.id.clone();
            }
        }
        // A live message must not bridge a gap in a historical page.
        if beyond_context {
            if inner.active_channel_id.as_deref() == Some(message.channel_id.as_str()) {
                if let Some(previous) = previous.filter(|previous| {
                    revision.is_none() && id_number(&message.id) > id_number(previous)
                }) {
                    let tail = inner.live_tail.get_or_insert_with(|| LiveTail {
                        after: previous.clone(),
                        history,
                        messages: VecDeque::new(),
                    });
                    if tail.history != history
                        || tail
                            .messages
                            .back()
                            .map_or(tail.after.as_str(), |row| &row.id)
                            != previous
                    {
                        tail.after = previous;
                        tail.history = history;
                        tail.messages.clear();
                    }
                    tail.messages.push_back(message);
                    if tail.messages.len() > MESSAGE_CAP {
                        tail.after = tail.messages.pop_front().unwrap().id;
                    }
                } else if inner.live_tail.as_ref().is_some_and(|tail| {
                    id_number(&message.id) > id_number(&tail.after)
                        && !tail.messages.iter().any(|row| row.id == message.id)
                }) {
                    inner.live_tail = None;
                }
            }
            drop(inner);
            self.mark();
            return true;
        }
        let rows = inner
            .messages
            .entry(message.channel_id.clone())
            .or_default();
        if let Some(existing) = rows.iter_mut().find(|item| item.id == message.id) {
            let mut message = message;
            super::rest::carry_over(&mut message, existing);
            *existing = message;
        } else {
            let index = rows.partition_point(|row| id_number(&row.id) < id_number(&message.id));
            rows.insert(index, message);
        }
        while rows.len() > MESSAGE_CAP {
            rows.pop_front();
        }
        drop(inner);
        self.mark();
        true
    }

    pub fn replace_message(
        &self,
        mut message: ChatMessageDto,
        revision: MessageRevision,
    ) -> Option<()> {
        let mut inner = self.inner.write();
        let current = inner
            .revisions
            .entry(message.channel_id.clone())
            .or_default();
        let unchanged = current.history == revision.history;
        // Successful edits invalidate history even outside the cached page.
        current.invalidate();
        if !unchanged {
            return None;
        }
        let existing = inner
            .messages
            .get_mut(&message.channel_id)?
            .iter_mut()
            .find(|row| row.id == message.id)?;
        super::rest::carry_over(&mut message, existing);
        *existing = message;
        drop(inner);
        self.mark();
        Some(())
    }

    pub fn patch_message(&self, data: &serde_json::Value) -> Option<()> {
        let channel = data.get("channel_id")?.as_str()?;
        let id = data.get("id")?.as_str()?;
        let mut inner = self.inner.write();
        // Invalidate history even when this message is not cached yet.
        inner
            .revisions
            .entry(channel.to_string())
            .or_default()
            .invalidate();
        let message = inner
            .messages
            .get_mut(channel)?
            .iter_mut()
            .find(|row| row.id == id)?;
        super::rest::patch_message(message, data);
        drop(inner);
        self.mark();
        Some(())
    }

    pub fn update_poll_vote(&self, data: &serde_json::Value, added: bool) -> Option<()> {
        let channel = data["channel_id"].as_str()?;
        let id = data["message_id"].as_str()?;
        let answer = u32::try_from(data["answer_id"].as_u64()?).ok()?;
        let mut inner = self.inner.write();
        let own = inner
            .user
            .as_ref()
            .is_some_and(|user| data["user_id"].as_str() == Some(&user.id));
        let poll = inner
            .messages
            .get_mut(channel)?
            .iter_mut()
            .find(|message| message.id == id)?
            .poll
            .as_mut()?;
        let results = poll.results.as_mut()?;
        if results.is_finalized || !poll.answers.iter().any(|row| row.answer_id == answer) {
            return None;
        }
        let index = results
            .answer_counts
            .iter()
            .position(|row| row.id == answer)
            .unwrap_or_else(|| {
                results.answer_counts.push(super::models::PollCountDto {
                    id: answer,
                    count: 0,
                    me_voted: false,
                });
                results.answer_counts.len() - 1
            });
        let count = &mut results.answer_counts[index];
        count.count = if added {
            count.count.saturating_add(1)
        } else {
            count.count.saturating_sub(1)
        };
        if own {
            count.me_voted = added;
        }
        inner
            .revisions
            .entry(channel.to_owned())
            .or_default()
            .invalidate();
        drop(inner);
        self.mark();
        Some(())
    }

    pub fn remove_messages(&self, channel_id: &str, message_ids: &[&str]) {
        if message_ids.is_empty() {
            return;
        }
        let mut inner = self.inner.write();
        inner
            .revisions
            .entry(channel_id.to_string())
            .or_default()
            .invalidate();
        inner.notices.retain(|notice| {
            notice.channel != channel_id || !message_ids.contains(&notice.id.as_str())
        });
        if let Some(rows) = inner.messages.get_mut(channel_id) {
            rows.retain(|message| !message_ids.contains(&message.id.as_str()));
        }
        drop(inner);
        self.mark();
    }

    /// The channel's revision and messages when `account` is signed in and may read it.
    pub fn readable_messages(
        &self,
        channel: &str,
        account: &str,
    ) -> Option<(u64, Vec<ChatMessageDto>)> {
        let inner = self.inner.read();
        if inner.user.as_ref().map(|user| user.id.as_str()) != Some(account)
            || !inner.can(channel, Access::Read)
        {
            return None;
        }
        Some((
            inner
                .revisions
                .get(channel)
                .copied()
                .unwrap_or_default()
                .content,
            inner
                .messages
                .get(channel)
                .map(|rows| rows.iter().cloned().collect())
                .unwrap_or_default(),
        ))
    }

    pub fn flush_if_dirty(&self) {
        let path = self.path.lock();
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let Some(path) = &*path else { return };
        if let Err(error) = self.persist(path) {
            self.mark();
            eprintln!("could not save cached messages: {error}");
        }
    }

    pub fn clear(&self) {
        let path = self.path.lock();
        *self.inner.write() = Inner::default();
        if let Some(path) = &*path {
            let _ = std::fs::remove_file(path);
        }
        self.dirty.store(false, Ordering::Relaxed);
    }

    fn mark(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Serializes in place under the read lock, keeping each channel's newest messages.
    fn persist(&self, path: &Path) -> std::io::Result<()> {
        struct Recent<'a>(&'a HashMap<String, VecDeque<ChatMessageDto>>);
        impl Serialize for Recent<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_map(self.0.iter().map(|(id, rows)| {
                    (
                        id,
                        rows.range(rows.len().saturating_sub(SNAPSHOT_CAP)..)
                            .collect::<Vec<_>>(),
                    )
                }))
            }
        }
        #[derive(Serialize)]
        struct Saved<'a> {
            #[serde(flatten)]
            snapshot: &'a Snapshot,
            messages: Recent<'a>,
        }
        let bytes = {
            let inner = self.inner.read();
            serde_json::to_vec(&Saved {
                snapshot: &inner.saved,
                messages: Recent(&inner.messages),
            })?
        };
        crate::write_file(path, &bytes)
    }
}

pub fn sort_guilds(guilds: &mut [GuildDto], positions: &[String]) {
    let mut order = HashMap::with_capacity(positions.len());
    for (index, id) in positions.iter().enumerate() {
        order.entry(id.as_str()).or_insert(index);
    }
    guilds.sort_by_key(|guild| order.get(guild.id.as_str()).copied().unwrap_or(usize::MAX));
}

fn remember_message(inner: &mut Inner, message: &ChatMessageDto) {
    if let Some(thread) = &message.thread {
        if !inner.channels.iter().any(|row| row.id == thread.id) {
            inner.channels.push(thread.clone());
        }
    }
    for user in &message.mentions {
        inner
            .users
            .entry(user.id.clone())
            .or_insert_with(|| user.clone());
    }
    if !message.author_id.is_empty() {
        inner
            .users
            .entry(message.author_id.clone())
            .and_modify(|user| {
                user.name.clone_from(&message.author);
                user.avatar.clone_from(&message.avatar);
            })
            .or_insert_with(|| UserDto {
                id: message.author_id.clone(),
                name: message.author.clone(),
                avatar: message.avatar.clone(),
                ..Default::default()
            });
    }
}

fn resolve_dm_names(saved: &mut Snapshot) {
    let own_id = saved.user.as_ref().map(|user| user.id.as_str());
    for channel in &mut saved.dms {
        let peers = channel
            .recipient_ids
            .iter()
            .filter(|id| Some(id.as_str()) != own_id)
            .filter_map(|id| saved.users.get(id))
            .filter(|user| !user.name.is_empty())
            .collect::<Vec<_>>();
        if channel.kind == 1 && (channel.name.is_empty() || channel.name == "Direct message") {
            if let Some(user) = peers.first() {
                channel.name = user.name.clone();
                channel.icon = user.avatar.clone();
            }
        } else if channel.kind == 3
            && (channel.name.is_empty() || channel.name == "Group chat")
            && !peers.is_empty()
        {
            channel.name = peers
                .iter()
                .map(|user| user.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
        }
    }
}

fn sort_dms(dms: &mut [ChannelDto]) {
    dms.sort_by_key(|dm| {
        std::cmp::Reverse(
            dm.last_message_id
                .as_ref()
                .and_then(|id| id.parse::<u128>().ok()),
        )
    });
}

fn id_number(id: &str) -> u64 {
    id.parse().unwrap_or(0)
}

impl Snapshot {
    pub fn channel(&self, id: &str) -> Option<&ChannelDto> {
        self.channels
            .iter()
            .chain(&self.dms)
            .find(|row| row.id == id)
    }

    fn guild_of(&self, channel: &ChannelDto) -> Option<&GuildDto> {
        let id = channel.guild_id.as_ref()?;
        self.guilds.iter().find(|guild| &guild.id == id)
    }

    fn channel_access(&self, id: &str) -> Option<&GuildAccess> {
        self.guild_of(self.channel(id)?)?.access.as_ref()
    }

    pub fn can(&self, id: &str, access: Access) -> bool {
        self.channel(id)
            .is_some_and(|channel| self.allows(channel, self.guild_of(channel), access))
    }

    /// Channels allowing `access`, with each guild looked up once.
    pub fn allowed(&self, access: Access) -> Vec<&str> {
        let guilds: HashMap<_, _> = self
            .guilds
            .iter()
            .map(|guild| (guild.id.as_str(), guild))
            .collect();
        self.channels
            .iter()
            .chain(&self.dms)
            .filter(|channel| {
                let guild = channel
                    .guild_id
                    .as_deref()
                    .and_then(|id| guilds.get(id).copied());
                self.allows(channel, guild, access)
            })
            .map(|channel| channel.id.as_str())
            .collect()
    }

    fn allows(&self, channel: &ChannelDto, guild: Option<&GuildDto>, access: Access) -> bool {
        let messages = matches!(channel.kind, 0 | 1 | 2 | 3 | 5 | 10 | 11 | 12 | 13);
        if channel.guild_id.is_none() {
            // Never initiate an empty DM. Use official Discord to establish it.
            return messages
                && (access != Access::Send
                    || self
                        .messages
                        .get(&channel.id)
                        .is_some_and(|rows| !rows.is_empty()));
        }
        let (Some(guild), Some(user)) = (guild, &self.user) else {
            return false;
        };
        let channels = &self.channels;
        let allowed = match access {
            Access::View if matches!(channel.kind, 15 | 16) => {
                super::permissions::can_view(channel, guild, &user.id, channels)
            }
            _ if !messages => return false,
            Access::Send => super::permissions::can_send(channel, guild, &user.id, channels),
            _ => super::permissions::can_read_history(channel, guild, &user.id, channels),
        };
        allowed == Some(true)
    }

    pub fn mentions_user(&self, message: &ChatMessageDto) -> bool {
        let Some(user) = &self.user else {
            return false;
        };
        if message.mention_everyone || message.mentions.iter().any(|mention| mention.id == user.id)
        {
            return true;
        }
        self.channel_access(&message.channel_id)
            .and_then(|access| access.member_roles.as_ref())
            .is_some_and(|roles| {
                message
                    .mention_roles
                    .iter()
                    .any(|role| roles.contains(role))
            })
    }

    pub fn message_unread(&self, channel: &str, message: &str) -> bool {
        self.channel(channel).is_some()
            && self
                .last_read
                .get(channel)
                .is_none_or(|read| id_number(message) > id_number(read))
    }

    pub fn unread(&self, channel: &str) -> bool {
        let Some(last) = self
            .channel(channel)
            .and_then(|row| row.last_message_id.as_deref())
        else {
            return false;
        };
        self.last_read
            .get(channel)
            .is_some_and(|read| id_number(last) > id_number(read))
    }

    pub fn reaches_latest(&self, channel: &str) -> bool {
        let latest = self
            .channel(channel)
            .and_then(|row| row.last_message_id.as_deref());
        let cached = self
            .messages
            .get(channel)
            .and_then(|rows| rows.back())
            .map(|row| id_number(&row.id));
        latest.is_none_or(|latest| cached.is_some_and(|cached| cached >= id_number(latest)))
    }
}

impl Inner {
    fn remove_channels(&mut self, ids: &HashSet<String>) {
        if ids.is_empty() {
            return;
        }
        self.notices.retain(|notice| !ids.contains(&notice.channel));
        let saved = &mut self.saved;
        saved.channels.retain(|channel| !ids.contains(&channel.id));
        saved.dms.retain(|channel| !ids.contains(&channel.id));
        saved.pinned_dms.retain(|id| !ids.contains(id));
        for id in ids {
            saved.messages.remove(id);
            saved.last_read.remove(id);
            saved.mention_counts.remove(id);
            self.pending_reads.remove(id);
            self.revisions.entry(id.clone()).or_default().invalidate();
        }
        saved
            .visited_channels
            .retain(|_, channel| !ids.contains(channel));
        self.voice.retain(|_, user| !ids.contains(&user.channel));
        self.calls.retain(|id, _| !ids.contains(id));
        if saved
            .active_channel_id
            .as_ref()
            .is_some_and(|id| ids.contains(id))
        {
            saved.active_channel_id = None;
            self.live_tail = None;
        }
    }
}

fn advance_message_id(previous: &mut Option<String>, message: &str) -> bool {
    if id_number(message) > previous.as_deref().map_or(0, id_number) {
        *previous = Some(message.to_owned());
        true
    } else {
        false
    }
}
