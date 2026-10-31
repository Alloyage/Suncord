use crate::discord::models::{CustomEmojiDto, GuildDto, Snapshot};

fn channel_guild<'a>(snapshot: &'a Snapshot, channel: &str) -> Option<&'a str> {
    snapshot
        .channels
        .iter()
        .find(|row| row.id == channel)
        .and_then(|row| row.guild_id.as_deref())
}

fn permission(snapshot: &Snapshot, channel: &str, bit: u8) -> bool {
    let Some(row) = snapshot.channels.iter().find(|row| row.id == channel) else {
        return snapshot.dms.iter().any(|row| row.id == channel);
    };
    let Some(guild) = snapshot
        .guilds
        .iter()
        .find(|guild| Some(&guild.id) == row.guild_id.as_ref())
    else {
        return false;
    };
    snapshot
        .user
        .as_ref()
        .and_then(|user| {
            crate::discord::permissions::effective_permissions(
                row,
                guild,
                &user.id,
                &snapshot.channels,
            )
        })
        .is_some_and(|bits| bits & (1 << bit) != 0)
}

fn premium(snapshot: &Snapshot) -> u8 {
    snapshot
        .user
        .as_ref()
        .and_then(|user| user.premium_type)
        .unwrap_or(0)
}

fn usable_emoji(
    snapshot: &Snapshot,
    channel: &str,
    guild: &GuildDto,
    emoji: &CustomEmojiDto,
) -> bool {
    let role_allowed = emoji.roles.is_empty()
        || guild
            .access
            .as_ref()
            .and_then(|access| access.member_roles.as_ref())
            .is_some_and(|roles| emoji.roles.iter().any(|role| roles.contains(role)));
    emoji.available
        && role_allowed
        && if channel_guild(snapshot, channel) == Some(&guild.id) {
            !emoji.animated || premium(snapshot) > 0
        } else {
            premium(snapshot) > 0 && permission(snapshot, channel, 18)
        }
}

pub fn usable_sticker(
    snapshot: &Snapshot,
    channel: &str,
    guild: &GuildDto,
    available: bool,
) -> bool {
    available
        && (channel_guild(snapshot, channel) == Some(&guild.id)
            || (premium(snapshot) > 1 && permission(snapshot, channel, 37)))
}

pub fn message_content(snapshot: &Snapshot, channel: &str, source: &str) -> String {
    let mut output = String::new();
    let mut copied = 0;
    let code: Vec<_> = pulldown_cmark::Parser::new(source)
        .into_offset_iter()
        .filter_map(|(event, range)| {
            matches!(
                event,
                pulldown_cmark::Event::Code(_)
                    | pulldown_cmark::Event::Start(pulldown_cmark::Tag::CodeBlock(_))
            )
            .then_some(range)
        })
        .collect();
    for (start, _) in source.match_indices('<') {
        if start < copied || code.iter().any(|range| range.contains(&start)) {
            continue;
        }
        let Some(end) = source[start..].find('>').map(|end| start + end + 1) else {
            continue;
        };
        let token = &source[start + 1..end - 1];
        let animated = token.starts_with("a:");
        let Some((name, id)) = token
            .strip_prefix("a:")
            .or_else(|| token.strip_prefix(':'))
            .and_then(|token| token.split_once(':'))
            .filter(|(name, id)| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
                    && !id.is_empty()
                    && id.bytes().all(|ch| ch.is_ascii_digit())
            })
        else {
            continue;
        };
        let native = snapshot
            .guilds
            .iter()
            .find_map(|guild| {
                guild
                    .emojis
                    .iter()
                    .flatten()
                    .find(|emoji| emoji.id == id)
                    .map(|emoji| usable_emoji(snapshot, channel, guild, emoji))
            })
            .unwrap_or(premium(snapshot) > 0 && permission(snapshot, channel, 18));
        if native {
            continue;
        }
        output.push_str(&source[copied..start]);
        if !output.is_empty() && !output.ends_with(char::is_whitespace) {
            output.push(' ');
        }
        output.push_str(&format!(
            "[{name}](https://cdn.discordapp.com/emojis/{id}.{}?size=48&name={name}&lossless=true)",
            if animated { "gif" } else { "webp" }
        ));
        if source[end..]
            .chars()
            .next()
            .is_some_and(|ch| !ch.is_whitespace())
        {
            output.push(' ');
        }
        copied = end;
    }
    output.push_str(&source[copied..]);
    output
}
