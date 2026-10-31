use super::models::{lenient, number_field, ChannelDto, GuildAccess, GuildDto, RoleDto};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
pub struct WireRole {
    id: String,
    #[serde(default, deserialize_with = "lenient")]
    name: String,
    #[serde(default, deserialize_with = "lenient")]
    color: u32,
    #[serde(default, deserialize_with = "lenient")]
    position: i64,
    #[serde(deserialize_with = "number_field")]
    permissions: u64,
}

pub fn member_id(member: &Value) -> Option<&str> {
    member
        .pointer("/user/id")
        .or_else(|| member.get("user_id"))?
        .as_str()
}

pub fn member_roles(member: &Value) -> Option<Vec<String>> {
    member
        .get("roles")?
        .as_array()?
        .iter()
        .map(|id| Some(id.as_str()?.to_string()))
        .collect()
}

impl WireRole {
    pub fn insert(self, access: &mut GuildAccess) {
        access.roles.insert(self.id.clone(), self.permissions);
        access.role_details.insert(
            self.id.clone(),
            RoleDto {
                id: self.id,
                name: self.name,
                color: self.color,
                position: self.position,
            },
        );
    }
}

/// `member` is the signed-in user's guild member object.
pub fn guild_access(owner_id: String, roles: Vec<WireRole>, member: Option<&Value>) -> GuildAccess {
    let mut access = GuildAccess {
        owner_id,
        roles: Default::default(),
        role_details: Default::default(),
        member_roles: member.and_then(member_roles),
        communication_disabled_until: member.and_then(member_timeout),
        timeout_known: member
            .is_some_and(|member| member.get("communication_disabled_until").is_some()),
    };
    for role in roles {
        role.insert(&mut access);
    }
    access
}

pub fn member_timeout(member: &Value) -> Option<String> {
    member
        .get("communication_disabled_until")
        .filter(|value| !value.is_null())
        .map(|value| value.as_str().unwrap_or_default().to_string())
}

/// None means Discord has not supplied enough permission data yet.
pub fn effective_permissions(
    channel: &ChannelDto,
    guild: &GuildDto,
    user: &str,
    channels: &[ChannelDto],
) -> Option<u64> {
    let mut permissions = guild_permissions(guild, user)?;
    if permissions == u64::MAX {
        return Some(permissions);
    }
    let access = guild.access.as_ref()?;
    let roles = access.member_roles.as_ref()?;
    // Private thread membership is separate from parent channel permissions.
    let private_thread = channel.kind == 12 && !channel.thread_joined;
    let channel = if matches!(channel.kind, 10..=12) {
        channels
            .iter()
            .find(|parent| Some(&parent.id) == channel.parent_id.as_ref())?
    } else {
        channel
    };
    if let Some(overwrite) = channel
        .overwrites
        .iter()
        .find(|o| o.kind == 0 && o.id == guild.id)
    {
        permissions = (permissions & !overwrite.deny) | overwrite.allow;
    }
    let (mut deny, mut allow) = (0, 0);
    for overwrite in channel
        .overwrites
        .iter()
        .filter(|o| o.kind == 0 && roles.contains(&o.id))
    {
        deny |= overwrite.deny;
        allow |= overwrite.allow;
    }
    permissions = (permissions & !deny) | allow;
    if let Some(overwrite) = channel
        .overwrites
        .iter()
        .find(|o| o.kind == 1 && o.id == user)
    {
        permissions = (permissions & !overwrite.deny) | overwrite.allow;
    }
    Some(if private_thread && permissions & (1 << 34) == 0 {
        0
    } else {
        permissions
    })
}

fn guild_permissions(guild: &GuildDto, user: &str) -> Option<u64> {
    let access = guild.access.as_ref()?;
    if access.owner_id == user {
        return Some(u64::MAX);
    }
    let mut permissions = *access.roles.get(&guild.id)?;
    for role in access.member_roles.as_ref()? {
        permissions |= access.roles.get(role)?;
    }
    Some(if permissions & 8 != 0 {
        u64::MAX
    } else {
        permissions
    })
}

fn member_timed_out(guild: &GuildDto, user: &str) -> bool {
    // Owners and administrators are exempt from member timeouts.
    guild_permissions(guild, user) != Some(u64::MAX)
        && guild.access.as_ref().is_some_and(|access| {
            access
                .communication_disabled_until
                .as_ref()
                .is_some_and(|until| {
                    chrono::DateTime::parse_from_rfc3339(until)
                        .map_or(true, |until| until > chrono::Utc::now())
                })
        })
}

pub fn can_view(
    channel: &ChannelDto,
    guild: &GuildDto,
    user: &str,
    channels: &[ChannelDto],
) -> Option<bool> {
    effective_permissions(channel, guild, user, channels).map(|bits| bits & (1 << 10) != 0)
}

pub fn can_read_history(
    channel: &ChannelDto,
    guild: &GuildDto,
    user: &str,
    channels: &[ChannelDto],
) -> Option<bool> {
    effective_permissions(channel, guild, user, channels)
        .map(|bits| bits & ((1 << 10) | (1 << 16)) == ((1 << 10) | (1 << 16)))
}

pub fn can_send(
    channel: &ChannelDto,
    guild: &GuildDto,
    user: &str,
    channels: &[ChannelDto],
) -> Option<bool> {
    if channel.thread_archived && channel.thread_locked {
        return Some(false);
    }
    let required = (1 << 10)
        | if matches!(channel.kind, 10..=12) {
            1 << 38
        } else {
            1 << 11
        };
    effective_permissions(channel, guild, user, channels)
        .map(|bits| !member_timed_out(guild, user) && bits & required == required)
}
