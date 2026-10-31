use crate::discord::{
    cache::{Access, MemberListEntry},
    models::{ChatMessageDto, ReactionEmojiDto, Snapshot},
    rest::{MessageEdit, MessageMedia},
};
use crate::{
    compose,
    http::App,
    render::{markdown, Message},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Preferences {
    pub avatar_roundness: f32,
    pub show_inaccessible_channels: bool,
    pub notifications: bool,
    pub channel_panel_width: f32,
    pub side_panel_width: f32,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            avatar_roundness: 100.0,
            show_inaccessible_channels: false,
            notifications: true,
            channel_panel_width: 252.0,
            side_panel_width: 260.0,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "camelCase")]
pub enum Action {
    Preferences {
        value: Option<Preferences>,
    },
    View {
        channel: String,
        #[serde(rename = "clientId")]
        client_id: String,
        navigation: u64,
        #[serde(rename = "subscribedIndex", default)]
        subscribed_index: usize,
    },
    Messages {
        channel: String,
        #[serde(rename = "clientId")]
        client_id: String,
        navigation: u64,
    },
    Send {
        channel: String,
        content: String,
        #[serde(default)]
        files: Vec<String>,
        nonce: String,
        reply: Option<String>,
        sticker: Option<String>,
    },
    History {
        channel: String,
        #[serde(rename = "clientId")]
        client_id: String,
        navigation: u64,
        before: Option<String>,
        after: Option<String>,
        around: Option<String>,
    },
    RefreshMessage {
        channel: String,
        message: String,
    },
    PrepareSticker {
        channel: String,
        sticker: String,
    },
    Profile {
        user: String,
        guild: Option<String>,
        #[serde(default)]
        cached: bool,
    },
    Friends {
        user: String,
    },
    Members {
        channel: String,
        #[serde(default)]
        index: usize,
        #[serde(rename = "subscribedIndex", default)]
        subscribed_index: usize,
    },
    SearchMembers {
        channel: String,
        query: String,
    },
    Role {
        channel: String,
        role: String,
    },
    Panel {
        kind: String,
        channel: Option<String>,
        cursor: Option<String>,
    },
    Read {
        channel: String,
    },
    PinDm {
        channel: String,
    },
    Edit {
        channel: String,
        message: String,
        content: String,
        retained: Vec<String>,
        #[serde(default)]
        files: Vec<String>,
    },
    Delete {
        channel: String,
        message: String,
    },
    Reaction {
        channel: String,
        message: String,
        emoji: ReactionEmojiDto,
        #[serde(default)]
        remove: bool,
    },
    Reactors {
        channel: String,
        message: String,
        emoji: ReactionEmojiDto,
    },
    Gifs {
        query: String,
    },
    Favorite {
        kind: String,
        key: String,
        index: Option<usize>,
    },
    EmojiUsed {
        key: String,
    },
    Invite {
        channel: String,
    },
    Ring {
        channel: String,
    },
    Reconnect,
}

fn require(app: &App, channel: &str, access: Access) -> Result<(), String> {
    if !app.cache.can(channel, access) {
        return Err("Channel is unavailable".into());
    }
    Ok(())
}
fn message(app: &App, channel: &str, id: &str, own: bool) -> Result<ChatMessageDto, String> {
    require(app, channel, Access::Read)?;
    let user = app.cache.user_id();
    app.cache
        .read(|saved| {
            saved
                .messages
                .get(channel)?
                .iter()
                .find(|m| m.id == id && (!own || Some(&m.author_id) == user.as_ref()))
                .cloned()
        })
        .ok_or_else(|| "Message is unavailable".into())
}
fn rows(messages: Vec<ChatMessageDto>) -> Value {
    tokio::task::block_in_place(|| {
        json!(messages.into_iter().map(Message::from).collect::<Vec<_>>())
    })
}
fn valid_draft(content: &str, files: usize, sticker: bool) -> bool {
    content.chars().count() <= 2000
        && files <= 10
        && (!content.trim().is_empty() || files > 0 || sticker)
        && !(sticker && files > 0)
}

/// Explains missing access from the channel's View Channel overrides.
fn channel_details(saved: &Snapshot, id: &str) -> String {
    let Some(channel) = saved.channels.iter().find(|row| row.id == id) else {
        return String::new();
    };
    let Some(access) = channel
        .guild_id
        .as_ref()
        .and_then(|id| saved.guilds.iter().find(|guild| &guild.id == id))
        .and_then(|guild| guild.access.as_ref())
    else {
        return "Channel permissions have not been supplied yet.".into();
    };
    let mut allowed = Vec::new();
    let mut denied = Vec::new();
    for overwrite in &channel.overwrites {
        let name = if overwrite.kind == 0 {
            access
                .role_details
                .get(&overwrite.id)
                .map(|role| format!("@{}", role.name))
                .unwrap_or_else(|| format!("Role {}", overwrite.id))
        } else {
            saved
                .users
                .get(&overwrite.id)
                .map(|user| user.name.clone())
                .unwrap_or_else(|| format!("Member {}", overwrite.id))
        };
        if overwrite.allow & (1 << 10) != 0 {
            allowed.push(name.clone());
        }
        if overwrite.deny & (1 << 10) != 0 {
            denied.push(name);
        }
    }
    allowed.sort();
    denied.sort();
    let mut details = "You do not currently have confirmed access to this channel.\n\nChannel overrides for View Channel".to_string();
    if !allowed.is_empty() {
        details.push_str(&format!("\nAllowed: {}", allowed.join(", ")));
    }
    if !denied.is_empty() {
        details.push_str(&format!("\nDenied: {}", denied.join(", ")));
    }
    details.push_str("\n\nServer permissions, combined roles and member overrides also affect access. Ask a server moderator for access.");
    if channel.kind == 12 {
        details.push_str("\nPrivate threads also require membership.");
    }
    details
}

impl Action {
    pub fn cache_only(&self) -> bool {
        matches!(
            self,
            Self::View { .. }
                | Self::Members { .. }
                | Self::SearchMembers { .. }
                | Self::PinDm { .. }
                | Self::Reconnect
        )
    }
}

/// Runs a page request. The caller holds the session gate and checked the page's account.
pub async fn apply(app: &App, action: Action) -> Result<Value, String> {
    let w = &*app.session;
    let changed = matches!(
        &action,
        Action::Messages { .. }
            | Action::Send { .. }
            | Action::History { .. }
            | Action::RefreshMessage { .. }
            | Action::Profile { cached: false, .. }
            | Action::Friends { .. }
            | Action::Read { .. }
            | Action::PinDm { .. }
            | Action::Edit { .. }
            | Action::Delete { .. }
            | Action::Reaction { .. }
            | Action::Favorite { .. }
            | Action::EmojiUsed { .. }
            | Action::Reconnect
    );
    let cache = &app.cache;
    let account = cache.user_id().unwrap_or_default();
    let result = match action {
        Action::Preferences { value } => {
            tokio::task::block_in_place(|| -> Result<Value, String> {
                let path = app.data.join("settings.json");
                let mut preferences: Preferences = std::fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default();
                if let Some(mut value) = value {
                    if !value.avatar_roundness.is_finite()
                        || !value.channel_panel_width.is_finite()
                        || !value.side_panel_width.is_finite()
                    {
                        return Err("Invalid preferences".into());
                    }
                    value.avatar_roundness = value.avatar_roundness.clamp(0.0, 100.0);
                    value.channel_panel_width = value.channel_panel_width.clamp(180.0, 420.0);
                    value.side_panel_width = value.side_panel_width.clamp(220.0, 520.0);
                    let bytes =
                        serde_json::to_vec(&value).map_err(|_| "Could not save preferences")?;
                    crate::write_file(&path, &bytes).map_err(|_| "Could not save preferences")?;
                    preferences = value;
                }
                Ok(json!(preferences))
            })?
        }
        Action::View {
            channel,
            subscribed_index,
            client_id,
            navigation,
        } => {
            w.navigate(&client_id, navigation, &channel)?;
            let _navigation = w
                .selected(&client_id, navigation, &channel)
                .ok_or("Channel selection changed while loading")?;
            let (guild, permissions, details) = cache.read(|saved| {
                let selected = saved.channels.iter().find(|item| item.id == channel);
                let guild = selected.and_then(|item| item.guild_id.clone());
                let permissions = selected
                    .zip(
                        saved
                            .guilds
                            .iter()
                            .find(|item| Some(&item.id) == guild.as_ref()),
                    )
                    .zip(saved.user.as_ref())
                    .and_then(|((item, guild), user)| {
                        crate::discord::permissions::effective_permissions(
                            item,
                            guild,
                            &user.id,
                            &saved.channels,
                        )
                    });
                (guild, permissions, channel_details(saved, &channel))
            });
            if cache.can(&channel, Access::View) {
                if let Some(guild) = &guild {
                    w.gateway
                        .subscribe_members(guild, &channel, subscribed_index.min(100_000));
                }
                cache.set_active(guild, Some(&channel));
            }
            json!({"externalEmoji":permissions.is_none_or(|p|p & (1<<18)!=0),"externalSticker":permissions.is_none_or(|p|p & (1<<37)!=0),"roles":cache.role_details(&channel),"details":details,"slowmode":cache.read(|saved| saved.channel(&channel).map_or(0, |row| row.slowmode_seconds)),"reachLatest":cache.reaches_latest(&channel)})
        }
        Action::Messages {
            channel,
            client_id,
            navigation,
        } => {
            w.navigate(&client_id, navigation, &channel)?;
            let generation = w.generation();
            require(app, &channel, Access::Read)?;
            let token = w.token()?;
            let revision = cache.message_revision(&channel);
            let expected = cache.last_message_id(&channel);
            let id = |id: &str| id.parse::<u64>().unwrap_or(0);
            let unread = cache.read(|saved| {
                let cursor = saved.last_read.get(&channel)?;
                let first = saved.messages.get(&channel).and_then(|rows| rows.front());
                (saved.unread(&channel)
                    && (first.is_none_or(|first| id(&first.id) > id(cursor))
                        || !saved.reaches_latest(&channel)))
                .then(|| cursor.clone())
            });
            let page = match unread.as_deref() {
                Some("0") => w.rest.messages(&token, &channel, None, Some("0")).await?,
                Some(cursor) => w.rest.message_context(&token, &channel, cursor).await?,
                None => w.rest.messages(&token, &channel, None, None).await?,
            };
            let latest = unread.is_none() || (unread.as_deref() == Some("0") && page.len() < 50);
            let _navigation = w
                .selected(&client_id, navigation, &channel)
                .filter(|_| generation == w.generation() && !w.closed())
                .ok_or("Channel selection changed while loading")?;
            if !cache.replace_messages_if_unchanged(&channel, page, revision, latest) {
                return Err("Messages changed while loading. Open the channel again.".into());
            }
            if latest {
                cache.confirm_latest(&channel, expected.as_deref());
            }
            let guild = cache.channel(&channel).and_then(|c| c.guild_id);
            cache.set_active(guild, Some(&channel));
            let messages = cache.messages(&channel);
            drop(_navigation);
            rows(messages)
        }
        Action::Send {
            channel,
            content,
            files,
            nonce,
            reply,
            sticker,
        } => {
            if !cache.can(&channel, Access::Send)
                || !valid_draft(&content, files.len(), sticker.is_some())
                || reply
                    .as_ref()
                    .is_some_and(|id| !crate::discord::snowflake(id))
                || nonce.is_empty()
                || nonce.len() > 25
                || !nonce.bytes().all(|b| b.is_ascii_digit())
            {
                return Err("Message cannot be sent to this channel".into());
            }
            let token = w.token()?;
            let generation = w.generation();
            let revision = cache.message_revision(&channel);
            let content =
                cache.read(|saved| compose::message_content(saved, &channel, content.trim()));
            if content.chars().count() > 2000 {
                return Err(
                    "The message exceeds 2000 characters after expanding emoji links".into(),
                );
            }
            let attachments = app.uploads.contents(&account, &files)?;
            let media = if let Some(sticker) = sticker {
                if !cache.read(|saved| {
                    saved.guilds.iter().any(|guild| {
                        guild.stickers.iter().flatten().any(|item| {
                            item.id == sticker
                                && compose::usable_sticker(saved, &channel, guild, item.available)
                        })
                    })
                }) {
                    return Err("Sticker is unavailable in this channel".into());
                }
                Some(MessageMedia::Sticker(sticker))
            } else {
                (!attachments.is_empty()).then_some(MessageMedia::Files(attachments))
            };
            let message = w
                .rest
                .send_message(&token, &channel, &content, &nonce, reply.as_deref(), media)
                .await?;
            app.uploads.remove(&files);
            if generation == w.generation() && !w.closed() {
                cache.push_message(message, Some(revision));
            }
            Value::Null
        }
        Action::History {
            channel,
            client_id,
            navigation,
            before,
            after,
            around,
        } => {
            require(app, &channel, Access::Read)?;
            let token = w.token()?;
            let generation = w.generation();
            let revision = cache.message_revision(&channel);
            let expected = cache.last_message_id(&channel);
            let page = if let Some(id) = &around {
                w.rest.message_context(&token, &channel, id).await?
            } else {
                w.rest
                    .messages(&token, &channel, before.as_deref(), after.as_deref())
                    .await?
            };
            let _navigation = w
                .selected(&client_id, navigation, &channel)
                .filter(|_| generation == w.generation() && !w.closed())
                .ok_or("Channel selection changed while loading history")?;
            let more = page.len() == 50;
            if around
                .as_ref()
                .is_some_and(|id| !page.iter().any(|row| &row.id == id))
            {
                return Err("The original message is no longer available".into());
            }
            if let Some(boundary) = before.as_ref().or(after.as_ref()) {
                if cache
                    .extend_messages(&channel, boundary, page, after.is_some(), revision)
                    .is_none()
                {
                    return Err("Messages changed while loading. Try again".into());
                }
            } else if !cache.replace_messages_if_unchanged(
                &channel,
                page,
                revision,
                around.is_none(),
            ) {
                return Err("Messages changed while loading. Try again".into());
            }
            if around.is_none() && before.is_none() && (after.is_none() || !more) {
                cache.confirm_latest(&channel, expected.as_deref());
            }
            let messages = cache.messages(&channel);
            let latest = cache.reaches_latest(&channel);
            drop(_navigation);
            json!({"rows":rows(messages),"more":more,"reachLatest":latest})
        }
        Action::RefreshMessage {
            channel,
            message: id,
        } => {
            message(app, &channel, &id, false)?;
            let revision = cache.message_revision(&channel);
            let refreshed = w.rest.message_context(&w.token()?, &channel, &id).await?;
            if !refreshed.iter().any(|row| row.id == id) {
                return Err("The original message is no longer available".into());
            }
            if !cache.refresh_media_if_unchanged(&channel, refreshed, revision) {
                return Err("Messages changed while refreshing media. Try again".into());
            }
            let message = message(app, &channel, &id, false)?;
            tokio::task::block_in_place(|| json!(Message::from(message)))
        }
        Action::PrepareSticker { channel, sticker } => {
            require(app, &channel, Access::Read)?;
            if !cache.can(&channel, Access::Send) {
                return Err("Messages cannot be sent to this channel".into());
            }
            cache.read(|saved| {
                let (guild, sticker) = saved
                    .guilds
                    .iter()
                    .find_map(|guild| {
                        guild
                            .stickers
                            .iter()
                            .flatten()
                            .find(|item| item.id == sticker)
                            .map(|item| (guild, item))
                    })
                    .ok_or("Sticker is unavailable")?;
                if compose::usable_sticker(saved, &channel, guild, sticker.available) {
                    Ok(json!({"sticker":sticker.id}))
                } else if sticker.format_type == 3 {
                    Err("This sticker needs native sticker permission".to_owned())
                } else {
                    let extension = if sticker.format_type == 4 {
                        "gif"
                    } else {
                        "png"
                    };
                    let name = sticker.name.replace(['[', ']', '(', ')'], "");
                    Ok(json!({"link":format!("[{name}](https://media.discordapp.net/stickers/{}.{extension}?size=160&lossless=true)", sticker.id)}))
                }
            })?
        }
        Action::Profile {
            user,
            guild,
            cached: false,
        } => {
            let (profile, member) = w.rest.profile(&w.token()?, &user, guild.as_deref()).await?;
            cache.remember_profile(profile);
            if let (Some(guild), Some(member)) = (&guild, member) {
                cache.update_member(
                    guild,
                    &user,
                    &serde_json::to_value(member).map_err(|_| "Unreadable member")?,
                );
            }
            Value::Null
        }
        Action::Profile { user, guild, .. } => {
            let member = guild.as_deref().and_then(|g| cache.member(g, &user));
            let mut profile = cache.user(&user);
            if let (Some(guild), Some(member), Some(profile)) = (&guild, &member, &mut profile) {
                member.apply(guild, profile);
            }
            let bio = profile
                .as_ref()
                .and_then(|profile| profile.bio.as_deref())
                .unwrap_or_default();
            let presence = cache.presence(&user);
            let custom = presence
                .custom_status
                .as_ref()
                .and_then(|status| status.text.as_deref())
                .unwrap_or_default();
            let (bio, custom) = tokio::task::block_in_place(|| (markdown(bio), markdown(custom)));
            json!({"user":profile,"member":member,"presence":presence,"bioHtml":bio,"statusHtml":custom})
        }
        Action::Friends { user } => {
            let users = if let Some(cached) = cache.mutual_friends(&user) {
                cached
            } else {
                let users = w.rest.mutual_friends(&w.token()?, &user).await?;
                cache.remember_mutual_friends(&user, users.clone());
                users
            };
            json!(users)
        }
        Action::Members {
            channel,
            index,
            subscribed_index,
        } => {
            require(app, &channel, Access::View)?;
            let guild = cache.channel(&channel).and_then(|c| c.guild_id);
            let guild = guild.as_deref();
            let mut entries = Vec::new();
            let mut total = 0;
            if let Some(guild) = guild {
                w.gateway
                    .subscribe_members(guild, &channel, subscribed_index.min(100_000));
                if let Some(list) = cache.member_list(guild) {
                    total = list.slots.len();
                    for entry in list.slots.iter().skip(index).take(100) {
                        entries.push(match entry {
                            Some(MemberListEntry::Group(id, count)) => {
                                json!({"group":id,"count":count})
                            }
                            Some(MemberListEntry::User(id)) => {
                                let member = cache.member(guild, id);
                                let mut user = cache.user(id);
                                if let (Some(member), Some(user)) = (&member, &mut user) {
                                    member.apply(guild, user);
                                }
                                json!({"user":user,"member":member,"presence":cache.presence(id)})
                            }
                            None => json!({"loading":true}),
                        });
                    }
                }
            }
            if entries.is_empty() {
                let mut users = cache.channel_users(&channel);
                users.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
                total = users.len();
                entries=users.into_iter().skip(index).take(100).map(|user|{let id=user.id.clone();json!({"user":user,"member":guild.and_then(|g|cache.member(g,&id)),"presence":cache.presence(&id)})}).collect();
            }
            json!({"entries":entries,"total":total,"index":index})
        }
        Action::SearchMembers { channel, query } => {
            require(app, &channel, Access::View)?;
            if query.chars().count() > 100 {
                return Err("Search is too long".into());
            }
            if let Some(guild) = cache.channel(&channel).and_then(|c| c.guild_id) {
                w.gateway.search_members(&guild, &query);
            }
            let needle = query.to_lowercase();
            json!(cache
                .channel_users(&channel)
                .into_iter()
                .filter(|u| u.name.to_lowercase().contains(&needle)
                    || u.username
                        .as_ref()
                        .is_some_and(|n| n.to_lowercase().contains(&needle)))
                .take(30)
                .collect::<Vec<_>>())
        }
        Action::Role { channel, role } => {
            require(app, &channel, Access::View)?;
            let guild = cache
                .channel(&channel)
                .and_then(|c| c.guild_id)
                .ok_or("Choose a server channel")?;
            let mut error = None;
            let result = match w.token() {
                Ok(token) => w.rest.role_member_ids(&token, &guild, &role).await,
                Err(error) => Err(error),
            };
            let ids = match result {
                Ok(ids) => {
                    w.gateway.request_members(
                        &guild,
                        &ids.iter().take(100).cloned().collect::<Vec<_>>(),
                    );
                    ids
                }
                Err(reason) => {
                    error = Some(reason);
                    cache.read(|saved| {
                        saved
                            .guilds
                            .iter()
                            .filter(|item| item.id == guild)
                            .flat_map(|item| &item.members)
                            .filter(|(_, member)| member.roles.contains(&role))
                            .map(|(id, _)| id.clone())
                            .collect()
                    })
                }
            };
            json!({"role":cache.role_details(&channel).into_iter().find(|r|r.id==role),"ids":ids.iter().take(100).collect::<Vec<_>>(),"total":ids.len(),"error":error})
        }

        Action::Panel {
            kind,
            channel,
            cursor,
        } => {
            if kind == "pins" {
                require(
                    app,
                    channel.as_deref().ok_or("Choose a channel")?,
                    Access::Read,
                )?;
            }
            let token = w.token()?;
            let (messages, cursor) = if kind == "pins" {
                w.rest
                    .pinned_messages(
                        &token,
                        channel.as_deref().ok_or("Choose a channel")?,
                        cursor.as_deref(),
                    )
                    .await?
            } else if kind == "inbox" {
                w.rest.recent_mentions(&token, cursor.as_deref()).await?
            } else {
                return Err("Unknown panel".into());
            };
            let messages = cache.read(|saved| {
                messages
                    .into_iter()
                    .filter(|message| saved.can(&message.channel_id, Access::Read))
                    .collect()
            });
            json!({"rows":rows(messages),"cursor":cursor})
        }
        Action::Read { channel } => {
            require(app, &channel, Access::Read)?;
            if cache.reaches_latest(&channel) {
                cache.mark_read_for_sync(&channel);
                let token = w.token()?;
                let pending = cache.take_pending_reads();
                if !pending.is_empty() {
                    if let Err(error) = w.rest.acknowledge_messages(&token, &pending).await {
                        cache.retry_reads(pending);
                        return Err(error);
                    }
                }
            }
            Value::Null
        }
        Action::PinDm { channel } => {
            if !cache.read(|saved| saved.dms.iter().any(|c| c.id == channel)) {
                return Err("Conversation is unavailable".into());
            }
            cache.toggle_pinned_dm(&channel);
            Value::Null
        }
        Action::Edit {
            channel,
            message: id,
            content,
            retained,
            files,
        } => {
            let revision = cache.message_revision(&channel);
            let original = match message(app, &channel, &id, true) {
                Ok(original) => original,
                Err(error) => {
                    require(app, &channel, Access::Read)?;
                    w.rest
                        .message_context(&w.token()?, &channel, &id)
                        .await?
                        .into_iter()
                        .find(|row| {
                            row.id == id && row.channel_id == channel && row.author_id == account
                        })
                        .ok_or(error)?
                }
            };
            if content.chars().count() > 2000
                || retained.len() + files.len() > 10
                || retained
                    .iter()
                    .any(|id| !original.attachments.iter().any(|f| f.id == *id))
                || (content.trim().is_empty() && retained.is_empty() && files.is_empty())
            {
                return Err("Invalid message edit".into());
            }
            let content = cache.read(|saved| compose::message_content(saved, &channel, &content));
            if content.chars().count() > 2000 {
                return Err(
                    "The message exceeds 2000 characters after expanding emoji links".into(),
                );
            }
            let edit = MessageEdit {
                content,
                retained_attachments: retained,
                files: app.uploads.contents(&account, &files)?,
            };
            let edited = w
                .rest
                .edit_message(&w.token()?, &channel, &id, edit)
                .await?;
            cache.replace_message(edited, revision);
            app.uploads.remove(&files);
            Value::Null
        }
        Action::Delete {
            channel,
            message: id,
        } => {
            message(app, &channel, &id, true)?;
            w.rest.delete_message(&w.token()?, &channel, &id).await?;
            cache.remove_messages(&channel, &[&id]);
            Value::Null
        }
        Action::Reaction {
            channel,
            message: id,
            emoji,
            remove,
        } => {
            message(app, &channel, &id, false)?;
            if emoji
                .name
                .as_ref()
                .is_none_or(|n| n.is_empty() || n.chars().count() > 100)
            {
                return Err("Choose an emoji".into());
            }
            w.rest
                .set_reaction(&w.token()?, &channel, &id, &emoji, remove)
                .await?;
            Value::Null
        }
        Action::Reactors {
            channel,
            message: id,
            emoji,
        } => {
            let message = message(app, &channel, &id, false)?;
            let reaction = message
                .reactions
                .into_iter()
                .find(|r| {
                    r.emoji.id == emoji.id && (emoji.id.is_some() || r.emoji.name == emoji.name)
                })
                .ok_or("Reaction is unavailable")?;
            let users = w
                .rest
                .reaction_users(&w.token()?, &channel, &id, &reaction)
                .await?;
            cache.remember_users(users.clone());
            json!(users)
        }
        Action::Gifs { query } => {
            if query.chars().count() > 100 {
                return Err("Search is too long".into());
            }
            let gifs = w.rest.gifs(&w.token()?, &query).await?;
            json!(gifs.into_iter().map(|g|json!({"id":g.id,"url":g.url,"src":g.src,"gifSrc":g.gif_src,"title":g.title})).collect::<Vec<_>>())
        }
        Action::Favorite { kind, key, index } => {
            if kind == "sticker" {
                cache.edit_favorite_sticker(&key, index);
            } else if kind == "emoji" {
                let favorites = w.rest.edit_favorite_emoji(&w.token()?, &key, index).await?;
                cache.set_favorite_emojis(favorites);
            } else {
                return Err("Unknown favorite type".into());
            }
            Value::Null
        }
        Action::EmojiUsed { key } => {
            let frequent = w.rest.record_emoji_use(&w.token()?, &key).await?;
            cache.set_frequent_emojis(frequent);
            Value::Null
        }
        Action::Invite { channel } => {
            require(app, &channel, Access::Read)?;
            json!(w.rest.invite(&w.token()?, &channel).await?)
        }
        Action::Ring { channel } => {
            if !cache.read(|saved| saved.dms.iter().any(|c| c.id == channel)) {
                return Err("Conversation is unavailable".into());
            }
            w.rest.ring(&w.token()?, &channel).await?;
            Value::Null
        }
        Action::Reconnect => {
            w.token()?;
            w.gateway.connect();
            w.update(|s| {
                s.ready = false;
                s.status = "Reconnecting".into();
            });
            Value::Null
        }
    };
    if changed {
        w.update(|_| {});
    }
    Ok(result)
}
