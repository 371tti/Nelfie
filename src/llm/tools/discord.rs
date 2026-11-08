use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use async_openai::types::responses::{
    EasyInputContent, EasyInputMessage, ImageDetail, InputContent, InputImageContent, InputItem,
    InputTextContent, Role as InputRole,
};
use serde_json::json;
use serenity::all::{
    Builder, ChannelId, ChannelType, CreateMessage, CreateThread, EditInteractionResponse,
    EditMessage, GetMessages, GuildChannel, GuildId, Message, MessageId, OnlineStatus, ReactionType,
    RoleId, UserId,
};
use serenity::model::guild::Guild;

use crate::{
    discord::ephemeral::current_ephemeral_interaction,
    llm::{context::Role, tool::LMTool},
};

use super::{get_opt_str_arg, get_str_arg};

pub struct DiscordTool;

impl DiscordTool {
    pub fn new() -> DiscordTool {
        DiscordTool {}
    }

    fn get_opt_bool_arg(args: &serde_json::Value, key: &str) -> Option<bool> {
        args.get(key).and_then(|v| v.as_bool())
    }

    fn profile_image_file_metadata(user_id: UserId, label: &str, url: &str) -> (String, String) {
        let extension = reqwest::Url::parse(url)
            .ok()
            .and_then(|url| {
                url.path()
                    .rsplit('/')
                    .next()
                    .and_then(|filename| filename.rsplit_once('.').map(|(_, ext)| ext))
                    .map(str::to_ascii_lowercase)
            })
            .filter(|extension| {
                matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif")
            })
            .unwrap_or_else(|| "png".to_string());
        let content_type = match extension.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => "image/png",
        };
        let label = label
            .to_ascii_lowercase()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>();
        (
            format!("user-{}-{label}.{extension}", user_id.get()),
            content_type.to_string(),
        )
    }

    fn parse_guild_id(args: &serde_json::Value) -> Result<GuildId, String> {
        let guild_id_str = get_str_arg(args, "guild_id")?;
        GuildId::from_str(guild_id_str).map_err(|e| format!("Invalid 'guild_id': {e}"))
    }

    fn current_guild_id(
        cache: &serenity::cache::Cache,
        channel_id: Option<ChannelId>,
    ) -> Option<GuildId> {
        channel_id.and_then(|channel_id| cache.channel(channel_id).map(|channel| channel.guild_id))
    }

    fn resolve_guild_id(
        args: &serde_json::Value,
        cache: &serenity::cache::Cache,
        current_channel_id: Option<ChannelId>,
    ) -> Result<GuildId, String> {
        if let Some(guild_id) = get_opt_str_arg(args, "guild_id") {
            return GuildId::from_str(guild_id)
                .map_err(|error| format!("Invalid 'guild_id': {error}"));
        }

        Self::current_guild_id(cache, current_channel_id)
            .ok_or_else(|| {
                "A guild_id is required when the current channel is not in a server.".to_string()
            })
    }

    async fn user_profile_result(
        args: &serde_json::Value,
        ob_ctx: &crate::app::context::NelfieContext,
        current_channel_id: Option<ChannelId>,
    ) -> Result<crate::llm::tool::ToolExecutionResult, String> {
        let discord = ob_ctx.discord_client.open();
        let http = discord.http.clone();
        let cache = discord.cache.clone();
        let explicit_guild_id = get_opt_str_arg(args, "guild_id")
            .map(|value| {
                GuildId::from_str(value).map_err(|error| format!("Invalid 'guild_id': {error}"))
            })
            .transpose()?;
        let guild_id_was_explicit = explicit_guild_id.is_some();
        let guild_id = explicit_guild_id
            .or_else(|| Self::current_guild_id(&cache, current_channel_id));
        let user_id = match get_opt_str_arg(args, "user_id") {
            Some(user_id) => {
                if get_opt_str_arg(args, "username").is_some() {
                    return Err("Specify either 'user_id' or 'username', not both.".to_string());
                }
                UserId::from_str(user_id)
                    .map_err(|error| format!("Invalid 'user_id': {error}"))?
            }
            None => {
                let username = get_str_arg(args, "username")?;
                let guild_id = guild_id.ok_or_else(|| {
                    "Username lookup requires a server context or an explicit 'guild_id'."
                        .to_string()
                })?;
                let matches = guild_id
                    .search_members(&http, username, Some(100))
                    .await
                    .map_err(|error| format!("Failed to search Discord members: {error}"))?;
                let exact_matches = matches
                    .iter()
                    .filter(|member| {
                        member.user.name.eq_ignore_ascii_case(username)
                            || member
                                .user
                                .global_name
                                .as_deref()
                                .is_some_and(|name| name.eq_ignore_ascii_case(username))
                            || member
                                .nick
                                .as_deref()
                                .is_some_and(|name| name.eq_ignore_ascii_case(username))
                    })
                    .collect::<Vec<_>>();

                let selected = if exact_matches.len() == 1 {
                    Some(exact_matches[0])
                } else if exact_matches.is_empty() && matches.len() == 1 {
                    matches.first()
                } else {
                    None
                };
                if let Some(member) = selected {
                    member.user.id
                } else {
                    let candidates = matches
                        .iter()
                        .map(|member| {
                            format!(
                                "{} (username={}, display_name={})",
                                member.user.id,
                                member.user.name,
                                member.display_name()
                            )
                        })
                        .collect::<Vec<_>>();
                    if candidates.is_empty() {
                        return Err(format!(
                            "No server member matched username or nickname prefix '{username}'."
                        ));
                    }
                    return Err(format!(
                        "Username '{username}' is ambiguous. Matching server members: {}",
                        candidates.join(", ")
                    ));
                }
            }
        };
        let user = http
            .get_user(user_id)
            .await
            .map_err(|error| format!("Failed to fetch Discord user profile: {error}"))?;

        let avatar_url = user.face();
        let banner_url = user.banner_url();
        let avatar_decoration_url = user
            .avatar_decoration_data
            .as_ref()
            .map(|decoration| decoration.decoration_url());
        let accent_color = user
            .accent_colour
            .map(|color| format!("#{:06X}", color.0 & 0x00FF_FFFF));

        let mut image_urls = vec![("avatar".to_string(), avatar_url.clone())];
        if let Some(url) = banner_url.as_ref() {
            image_urls.push(("banner".to_string(), url.clone()));
        }
        if let Some(url) = avatar_decoration_url.as_ref() {
            image_urls.push(("avatar decoration".to_string(), url.clone()));
        }

        let guild_profile = if let Some(guild_id) = guild_id {
            match guild_id.member(&http, user_id).await {
                Ok(member) => {
                    let guild_avatar_url = member.avatar_url();
                    let guild_banner_url = member.banner_url();
                    let guild_decoration_url = member
                        .avatar_decoration_data
                        .as_ref()
                        .map(|decoration| decoration.decoration_url());

                    for (label, url) in [
                        ("server avatar", guild_avatar_url.as_ref()),
                        ("server banner", guild_banner_url.as_ref()),
                        ("server avatar decoration", guild_decoration_url.as_ref()),
                    ] {
                        if let Some(url) = url
                            && !image_urls.iter().any(|(_, existing)| existing == url)
                        {
                            image_urls.push((label.to_string(), url.clone()));
                        }
                    }

                    Some(json!({
                        "guild_id": guild_id.to_string(),
                        "nickname": member.nick,
                        "display_name": member.display_name(),
                        "joined_at": member.joined_at.as_ref().map(ToString::to_string),
                        "role_ids": member.roles.iter().map(ToString::to_string).collect::<Vec<_>>(),
                        "avatar_url": guild_avatar_url,
                        "banner_url": guild_banner_url,
                        "avatar_decoration_url": guild_decoration_url,
                    }))
                }
                Err(error) if guild_id_was_explicit => {
                    return Err(format!("Failed to fetch Discord guild member: {error}"));
                }
                Err(_) => None,
            }
        } else {
            None
        };

        let profile_image_refs = if let Some(channel_id) = current_channel_id {
            image_urls
                .iter()
                .map(|(label, url)| {
                    let (filename, content_type) =
                        Self::profile_image_file_metadata(user_id, label, url);
                    ob_ctx.attachment_registry.register_profile_image(
                        channel_id,
                        filename,
                        content_type,
                        url.clone(),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };

        let profile = json!({
            "status": "ok",
            "operation": "get_user_profile",
            "user_id": user_id.to_string(),
            "username": user.name,
            "global_name": user.global_name,
            "discriminator": user.discriminator.map(|value| value.get()),
            "bot": user.bot,
            "system": user.system,
            "created_at": user.created_at().to_string(),
            "avatar_url": avatar_url,
            "banner_url": banner_url,
            "accent_color": accent_color,
            "avatar_decoration_url": avatar_decoration_url,
            "profile_url": format!("https://discord.com/users/{user_id}"),
            "guild_profile": guild_profile,
            "profile_images": profile_image_refs.iter().map(|reference| reference.to_json()).collect::<Vec<_>>(),
        });

        let mut content = Vec::with_capacity(image_urls.len() * 2);
        for (index, (label, url)) in image_urls.into_iter().enumerate() {
            let reusable_asset = profile_image_refs.get(index).map(|reference| {
                format!(
                    "Reusable file and Typst asset ID: {} ({})",
                    reference.attachment_id, reference.filename
                )
            });
            content.push(InputContent::InputText(InputTextContent {
                text: reusable_asset.map_or_else(
                    || format!("Discord profile image for user {user_id}: {label}"),
                    |asset| format!("Discord profile image for user {user_id}: {label}. {asset}"),
                ),
            }));
            content.push(InputContent::InputImage(InputImageContent {
                detail: ImageDetail::Auto,
                file_id: None,
                image_url: Some(url),
            }));
        }

        Ok(crate::llm::tool::ToolExecutionResult {
            text: profile.to_string(),
            input_items: vec![InputItem::EasyMessage(EasyInputMessage {
                role: InputRole::User,
                content: EasyInputContent::ContentList(content),
                ..Default::default()
            })],
            input_bytes: 0,
            review_preview_id: None,
        })
    }

    async fn list_recent_users_result(
        args: &serde_json::Value,
        ob_ctx: &crate::app::context::NelfieContext,
        current_channel_id: Option<ChannelId>,
    ) -> Result<String, String> {
        let discord = ob_ctx.discord_client.open();
        let http = discord.http.clone();
        let cache = discord.cache.clone();
        let channel_id = match get_opt_str_arg(args, "channel_id") {
            Some(channel_id) => ChannelId::from_str(channel_id)
                .map_err(|error| format!("Invalid 'channel_id': {error}"))?,
            None => current_channel_id.ok_or_else(|| {
                "A channel_id is required when there is no current channel context.".to_string()
            })?,
        };
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 100) as u8;
        let include_bots = Self::get_opt_bool_arg(args, "include_bots").unwrap_or(false);
        let presence_by_user = Self::current_guild_id(&cache, Some(channel_id))
            .and_then(|guild_id| cache.guild(guild_id))
            .map(|guild| {
                guild
                    .presences
                    .iter()
                    .map(|(user_id, presence)| (*user_id, presence.status))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        let messages = channel_id
            .messages(&http, GetMessages::new().limit(limit))
            .await
            .map_err(|error| format!("Failed to fetch recent channel messages: {error}"))?;
        let scanned_message_count = messages.len();

        let mut positions = HashMap::<UserId, usize>::new();
        let mut users = Vec::<serde_json::Value>::new();
        for message in messages {
            if !include_bots && message.author.bot {
                continue;
            }

            if let Some(index) = positions.get(&message.author.id).copied() {
                let message_count = users[index]["message_count"].as_u64().unwrap_or(1);
                users[index]["message_count"] = json!(message_count + 1);
                continue;
            }

            let display_name = message
                .member
                .as_ref()
                .and_then(|member| member.nick.as_deref())
                .unwrap_or_else(|| message.author.display_name())
                .to_string();
            let status = presence_by_user
                .get(&message.author.id)
                .map(|status| status.name())
                .unwrap_or("unknown");
            positions.insert(message.author.id, users.len());
            users.push(json!({
                "user_id": message.author.id.to_string(),
                "username": message.author.name,
                "global_name": message.author.global_name,
                "display_name": display_name,
                "bot": message.author.bot,
                "online_status": status,
                "last_message_id": message.id.to_string(),
                "last_message_at": message.timestamp.to_string(),
                "message_count": 1,
            }));
        }

        Ok(json!({
            "status": "ok",
            "operation": "list_recent_users",
            "channel_id": channel_id.to_string(),
            "scanned_message_count": scanned_message_count,
            "include_bots": include_bots,
            "user_count": users.len(),
            "users": users,
        })
        .to_string())
    }

    async fn search_users_result(
        args: &serde_json::Value,
        ob_ctx: &crate::app::context::NelfieContext,
        current_channel_id: Option<ChannelId>,
    ) -> Result<String, String> {
        let query = get_str_arg(args, "query")?.trim();
        if query.is_empty() {
            return Err("The 'query' parameter must not be empty.".to_string());
        }

        let discord = ob_ctx.discord_client.open();
        let http = discord.http.clone();
        let cache = discord.cache.clone();
        let guild_id = Self::resolve_guild_id(args, &cache, current_channel_id)?;
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(25)
            .clamp(1, 100) as usize;
        let include_bots = Self::get_opt_bool_arg(args, "include_bots").unwrap_or(false);
        let online_only = Self::get_opt_bool_arg(args, "online_only").unwrap_or(false);
        if online_only && !ob_ctx.config.discord_presence_intent_enabled {
            return Err(
                "Online filtering is disabled. Set DISCORD_PRESENCE_INTENT_ENABLED=true and enable the GUILD_PRESENCES privileged intent in the Discord Developer Portal, then restart Nelfie."
                    .to_string(),
            );
        }

        let role_id = get_opt_str_arg(args, "role_id")
            .map(|value| {
                RoleId::from_str(value).map_err(|error| format!("Invalid 'role_id': {error}"))
            })
            .transpose()?;
        let channel_id = get_opt_str_arg(args, "channel_id")
            .map(|value| {
                ChannelId::from_str(value)
                    .map_err(|error| format!("Invalid 'channel_id': {error}"))
            })
            .transpose()?;
        let activity_limit = args
            .get("activity_limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 100) as u8;

        let (presence_by_user, channel_type, voice_user_ids) = {
            let guild = cache.guild(guild_id).ok_or_else(|| {
                "Guild is not available in cache. Make sure Nelfie has joined the server and finished connecting.".to_string()
            })?;
            let presence_by_user = guild
                .presences
                .iter()
                .map(|(user_id, presence)| (*user_id, presence.status))
                .collect::<HashMap<_, _>>();
            let channel_type = channel_id
                .and_then(|channel_id| guild.channels.get(&channel_id).map(|channel| channel.kind));
            let voice_user_ids = if channel_type.is_some_and(Self::is_voice_channel_type) {
                guild
                    .voice_states
                    .iter()
                    .filter(|(_, state)| state.channel_id == channel_id)
                    .map(|(user_id, _)| *user_id)
                    .collect::<HashSet<_>>()
            } else {
                HashSet::new()
            };
            (presence_by_user, channel_type, voice_user_ids)
        };

        let channel_user_ids = if let Some(channel_id) = channel_id {
            if channel_type.is_some_and(Self::is_voice_channel_type) {
                Some(voice_user_ids)
            } else {
                let messages = channel_id
                    .messages(&http, GetMessages::new().limit(activity_limit))
                    .await
                    .map_err(|error| {
                        format!("Failed to fetch recent messages for channel filter: {error}")
                    })?;
                Some(
                    messages
                        .into_iter()
                        .map(|message| message.author.id)
                        .collect::<HashSet<_>>(),
                )
            }
        } else {
            None
        };

        let remote_limit = if online_only || role_id.is_some() || channel_id.is_some() {
            1000
        } else {
            limit as u64
        };
        let members = guild_id
            .search_members(&http, query, Some(remote_limit))
            .await
            .map_err(|error| format!("Failed to search server members: {error}"))?;

        let mut users = Vec::new();
        for member in members {
            let user_id = member.user.id;
            if !include_bots && member.user.bot {
                continue;
            }
            if role_id.is_some_and(|role_id| !member.roles.contains(&role_id)) {
                continue;
            }
            if channel_user_ids
                .as_ref()
                .is_some_and(|user_ids| !user_ids.contains(&user_id))
            {
                continue;
            }

            let status = presence_by_user.get(&user_id).copied();
            let is_online = status.is_some_and(|status| {
                matches!(
                    status,
                    OnlineStatus::Online | OnlineStatus::Idle | OnlineStatus::DoNotDisturb
                )
            });
            if online_only && !is_online {
                continue;
            }

            users.push(json!({
                "user_id": user_id.to_string(),
                "username": member.user.name,
                "global_name": member.user.global_name,
                "nickname": member.nick,
                "display_name": member.display_name(),
                "bot": member.user.bot,
                "online_status": status
                    .map(|status| status.name().to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                "joined_at": member.joined_at.as_ref().map(ToString::to_string),
                "role_ids": member.roles.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "avatar_url": member.avatar_url().unwrap_or_else(|| member.user.face()),
            }));
            if users.len() >= limit {
                break;
            }
        }

        Ok(json!({
            "status": "ok",
            "operation": "search_users",
            "guild_id": guild_id.to_string(),
            "query": query,
            "match_type": "username_or_nickname_prefix",
            "filters": {
                "online_only": online_only,
                "role_id": role_id.map(|role_id| role_id.to_string()),
                "channel_id": channel_id.map(|channel_id| channel_id.to_string()),
                "activity_limit": if channel_user_ids.is_some()
                    && !channel_type.is_some_and(Self::is_voice_channel_type)
                {
                    Some(activity_limit)
                } else {
                    None
                },
                "include_bots": include_bots,
            },
            "user_count": users.len(),
            "users": users,
        })
        .to_string())
    }

    fn channel_matches_filter(channel_type: ChannelType, channel_filter: &str) -> bool {
        match channel_filter {
            "all" => true,
            "text" => channel_type == ChannelType::Text || channel_type == ChannelType::News,
            "voice" => channel_type == ChannelType::Voice,
            "stage" => channel_type == ChannelType::Stage,
            "category" => channel_type == ChannelType::Category,
            _ => false,
        }
    }

    fn channel_type_name(channel_type: ChannelType) -> &'static str {
        match channel_type {
            ChannelType::Text => "text",
            ChannelType::Voice => "voice",
            ChannelType::Category => "category",
            ChannelType::News => "news",
            ChannelType::Stage => "stage",
            ChannelType::Private => "private",
            ChannelType::PrivateThread => "private_thread",
            ChannelType::PublicThread => "public_thread",
            ChannelType::NewsThread => "news_thread",
            ChannelType::Forum => "forum",
            ChannelType::Unknown(_) => "unknown",
            _ => "unknown",
        }
    }

    fn is_voice_channel_type(channel_type: ChannelType) -> bool {
        channel_type == ChannelType::Voice || channel_type == ChannelType::Stage
    }

    fn sort_channels(channels: &mut [GuildChannel]) {
        channels.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.id.get().cmp(&b.id.get()))
        });
    }

    fn build_voice_channel_members(
        guild: &Guild,
        channel_id: ChannelId,
        include_state: bool,
    ) -> Vec<serde_json::Value> {
        guild
            .voice_states
            .iter()
            .filter(|(_, state)| state.channel_id == Some(channel_id))
            .map(|(user_id, state)| {
                let display_name = guild
                    .members
                    .get(user_id)
                    .map(|m| m.display_name().to_string())
                    .unwrap_or_else(|| "(unknown)".to_string());

                if include_state {
                    json!({
                        "user_id": user_id.to_string(),
                        "display_name": display_name,
                        "self_mute": state.self_mute,
                        "self_deaf": state.self_deaf,
                        "mute": state.mute,
                        "deaf": state.deaf,
                    })
                } else {
                    json!({
                        "user_id": user_id.to_string(),
                        "display_name": display_name,
                    })
                }
            })
            .collect()
    }

    fn build_channel_json(
        channel: GuildChannel,
        members: Vec<serde_json::Value>,
        include_parent_id: bool,
    ) -> serde_json::Value {
        let mut value = json!({
            "channel_id": channel.id.to_string(),
            "name": channel.name,
            "type": Self::channel_type_name(channel.kind),
            "position": channel.position,
            "member_count": members.len(),
            "members": members,
        });

        if include_parent_id {
            value["parent_id"] = json!(channel.parent_id.map(|v| v.to_string()));
        }

        value
    }
}

impl Default for DiscordTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl LMTool for DiscordTool {
    fn name(&self) -> String {
        "discord-tool".to_string()
    }

    fn description(&self) -> String {
        "Interact with Discord: inspect user profiles and profile images, search server members by username or nickname with online, role, and channel filters, list recent users in a channel, add/remove reactions, create threads, send/edit/fetch/search messages, send ephemeral interaction messages visible only to the current modal respondent, and inspect channels/voice presence. get_user_profile returns reusable file and Typst asset IDs for its images. Use get_user_profile with a user_id or username, search_users for server-member queries, and list_recent_users for recent channel participants. Important: when you use send_message or send_ephemeral_message, the text is already posted to Discord, so do not repeat the same body again in a normal assistant reply.".to_string()
    }

    fn json_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "description": "Discord operation to perform.",
                    "enum": [
                        "add_reaction",
                        "remove_reaction",
                        "create_thread",
                        "send_message",
                        "send_ephemeral_message",
                        "edit_message",
                        "fetch_message",
                        "search_messages",
                        "list_channels",
                        "list_voice_channels",
                        "list_voice_members",
                        "get_user_profile",
                        "search_users",
                        "list_recent_users"
                    ]
                },
                "guild_id": {
                    "type": "string",
                    "description": "ID of the target guild. Required for: list_channels, list_voice_channels, list_voice_members. Optional for search_users and username-based get_user_profile; defaults to the current server when possible."
                },
                "channel_id": {
                    "type": "string",
                    "description": "ID of the target channel. Required for message/reaction/thread operations. Optional for list_recent_users (defaults to the current channel) and search_users; search_users matches recent text-channel speakers or current voice/stage participants."
                },
                "user_id": {
                    "type": "string",
                    "description": "ID of the target user. Use for get_user_profile; it may be omitted when username is provided. Optional for send_ephemeral_message; if provided, it must match the current modal respondent. Discord only permits this operation while handling an interaction such as a modal submission."
                },
                "username": {
                    "type": "string",
                    "description": "Username or server nickname to look up with get_user_profile. Requires a server context or guild_id. A unique prefix match is accepted; ambiguous matches return candidates. Global display names are returned in results but Discord member search does not search them directly."
                },
                "message_id": {
                    "type": "string",
                    "description": "ID of the target message. Used by: add/remove_reaction, create_thread(from message), send_message(reply_to), edit_message, fetch_message."
                },
                "reaction": {
                    "type": "string",
                    "description": "Emoji for reactions. Unicode (e.g. 🫠,😱,👍,👈,🤔) or custom emoji ID. Used by: add_reaction, remove_reaction."
                },
                "name": {
                    "type": "string",
                    "description": "Name of the thread. Used by: create_thread."
                },
                "thread_type": {
                    "type": "string",
                    "description": "Type of the thread. 'public' or 'private'. Defaults to 'public'. Used by: create_thread.",
                    "enum": ["public", "private"]
                },
                "content": {
                    "type": "string",
                    "description": "Message content. Used by: send_message, send_ephemeral_message, edit_message. For send_message/send_ephemeral_message, this body is already published to Discord immediately, so do not repeat it again in a separate assistant message."
                },
                "reply_to": {
                    "type": "string",
                    "description": "Message ID to reply to. Optional. Used by: send_message."
                },
                "query": {
                    "type": "string",
                    "description": "Search text. Used as a content substring by search_messages, or as a username/nickname prefix by search_users."
                },
                "limit": {
                    "type": "integer",
                    "description": "Result limit. For search_messages and list_recent_users, max messages to scan (1–100, default 50); for search_users, max users to return (1–100, default 25)."
                },
                "online_only": {
                    "type": "boolean",
                    "description": "When true, search_users returns only users whose cached Discord presence is online, idle, or do-not-disturb. Requires GUILD_PRESENCES to be enabled."
                },
                "role_id": {
                    "type": "string",
                    "description": "Optional server role ID filter for search_users."
                },
                "include_bots": {
                    "type": "boolean",
                    "description": "Whether list_recent_users and search_users should include bot users. Defaults to false."
                },
                "activity_limit": {
                    "type": "integer",
                    "description": "How many recent messages to scan when search_users filters by a text channel (1–100, default 100)."
                },
                "channel_type": {
                    "type": "string",
                    "description": "Filter for list_channels. Defaults to 'all'.",
                    "enum": ["all", "text", "voice", "stage", "category"]
                },
                "include_members": {
                    "type": "boolean",
                    "description": "Include current member list for voice/stage channels. Used by: list_channels, list_voice_channels."
                }
            },
            "required": ["operation"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ob_ctx: crate::app::context::NelfieContext,
    ) -> Result<String, String> {
        let operation = args
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'operation' parameter".to_string())?;

        let http = ob_ctx.discord_client.open().http.clone();
        let cache = ob_ctx.discord_client.open().cache.clone();

        match operation {
            "get_user_profile" => {
                Ok(Self::user_profile_result(&args, &ob_ctx, None).await?.text)
            }
            "search_users" => Ok(Self::search_users_result(&args, &ob_ctx, None).await?),
            "list_recent_users" => {
                Ok(Self::list_recent_users_result(&args, &ob_ctx, None).await?)
            }

            // --------------------
            // Reaction: add
            // --------------------
            "add_reaction" => {
                let channel_id_str = get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let message_id_str = get_str_arg(&args, "message_id")?;
                let reaction = get_str_arg(&args, "reaction")?;

                let message_id = MessageId::from_str(message_id_str)
                    .map_err(|e| format!("Invalid 'message_id': {e}"))?;

                channel_id
                    .create_reaction(
                        http,
                        message_id,
                        ReactionType::Unicode(reaction.to_string()),
                    )
                    .await
                    .map_err(|e| format!("Failed to add reaction: {e}"))?;

                Ok(format!(
                    "Added reaction '{}' on channel_id='{}', message_id='{}'",
                    reaction, channel_id_str, message_id_str
                ))
            }

            // --------------------
            // Reaction: remove
            // --------------------
            "remove_reaction" => {
                let channel_id_str = get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let message_id_str = get_str_arg(&args, "message_id")?;
                let reaction = get_str_arg(&args, "reaction")?;

                let message_id = MessageId::from_str(message_id_str)
                    .map_err(|e| format!("Invalid 'message_id': {e}"))?;

                channel_id
                    .delete_reaction_emoji(
                        http,
                        message_id,
                        ReactionType::Unicode(reaction.to_string()),
                    )
                    .await
                    .map_err(|e| format!("Failed to remove reaction: {e}"))?;

                Ok(format!(
                    "Removed reaction '{}' on channel_id='{}', message_id='{}'",
                    reaction, channel_id_str, message_id_str
                ))
            }

            // --------------------
            // Thread: create
            // --------------------
            "create_thread" => {
                let channel_id_str = get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let name = get_str_arg(&args, "name")?;

                let message_id_str_opt = args.get("message_id").and_then(|v| v.as_str());
                let thread_type_str = args
                    .get("thread_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("public");

                let channel_type = match thread_type_str {
                    "public" => ChannelType::PublicThread,
                    "private" => ChannelType::PrivateThread,
                    other => {
                        return Err(format!(
                            "Unsupported 'thread_type': {other}. Use 'public' or 'private'."
                        ));
                    }
                };

                let message_id_opt: Option<MessageId> = match message_id_str_opt {
                    Some(s) => {
                        let mid = MessageId::from_str(s)
                            .map_err(|e| format!("Invalid 'message_id': {e}"))?;
                        Some(mid)
                    }
                    None => None,
                };

                let builder = CreateThread::new(name).kind(channel_type);

                let res = builder
                    .execute(&http, (channel_id, message_id_opt))
                    .await
                    .map_err(|e| format!("Failed to create thread: {e}"))?;

                // Chat コンテキスト移動ロジックは元のまま
                let mut context = ob_ctx.chat_contexts.get_or_create(channel_id);
                context.add_text(
                    "The context has been moved to the newly created thread. You are now inside the thread you created.".to_string(),
                    Role::System,
                );
                ob_ctx.chat_contexts.marge(res.id, &context);
                ob_ctx.chat_contexts.set_enabled(res.id, true);

                Ok(format!(
                    "Created {thread_type_str} thread '{}' in channel_id='{}' (from message_id='{}')",
                    name,
                    channel_id_str,
                    message_id_str_opt.unwrap_or("-")
                ))
            }

            // --------------------
            // Send message
            // --------------------
            "send_message" => {
                let channel_id_str = get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let content = get_str_arg(&args, "content")?;

                let reply_to_str = args.get("reply_to").and_then(|v| v.as_str());

                let mut builder = CreateMessage::new().content(content);

                if let Some(reply_id_str) = reply_to_str {
                    let reply_id = MessageId::from_str(reply_id_str)
                        .map_err(|e| format!("Invalid 'reply_to' message_id: {e}"))?;
                    builder = builder.reference_message((channel_id, reply_id));
                }

                let msg = channel_id
                    .send_message(&http, builder)
                    .await
                    .map_err(|e| format!("Failed to send message: {e}"))?;

                let result = json!({
                    "status": "ok",
                    "operation": operation,
                    "channel_id": channel_id_str,
                    "message_id": msg.id.to_string(),
                    "content_length": msg.content.chars().count(),
                    "note": "already_posted_to_discord_do_not_repeat_body",
                });

                Ok(result.to_string())
            }

            // --------------------
            // Send ephemeral interaction message to the current modal respondent
            // --------------------
            "send_ephemeral_message" => {
                let content = get_str_arg(&args, "content")?;
                let user_id = get_opt_str_arg(&args, "user_id")
                    .map(|raw| UserId::from_str(raw).map_err(|e| format!("Invalid 'user_id': {e}")))
                    .transpose()?;

                let modal_ctx = current_ephemeral_interaction().ok_or_else(|| {
                        "send_ephemeral_message is only available while responding to an interaction, such as a modal submission. Use send_message for public channel output.".to_string()
                    })?;

                if let Some(user_id) = user_id
                    && user_id != modal_ctx.respondent_user_id
                {
                    return Err(format!(
                        "send_ephemeral_message can only target the current interaction respondent '{}', got '{}'",
                        modal_ctx.respondent_user_id, user_id
                    ));
                }

                let msg = modal_ctx
                    .interaction
                    .edit_response(&http, EditInteractionResponse::new().content(content))
                    .await
                    .map_err(|e| format!("Failed to edit ephemeral interaction response: {e}"))?;

                let result = json!({
                    "status": "ok",
                    "operation": operation,
                    "visibility": "interaction_ephemeral",
                    "recipient_user_id": modal_ctx.respondent_user_id.to_string(),
                    "channel_id": msg.channel_id.to_string(),
                    "message_id": msg.id.to_string(),
                    "content_length": msg.content.chars().count(),
                    "note": "already_sent_ephemeral_interaction_response_do_not_repeat_body_publicly",
                });

                Ok(result.to_string())
            }

            // --------------------
            // Edit message
            // --------------------
            "edit_message" => {
                let channel_id_str = get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let message_id_str = get_str_arg(&args, "message_id")?;
                let content = get_str_arg(&args, "content")?;

                let message_id = MessageId::from_str(message_id_str)
                    .map_err(|e| format!("Invalid 'message_id': {e}"))?;

                let builder = EditMessage::new().content(content);

                let msg = channel_id
                    .edit_message(&http, message_id, builder)
                    .await
                    .map_err(|e| format!("Failed to edit message: {e}"))?;

                let result = json!({
                    "status": "ok",
                    "operation": operation,
                    "channel_id": channel_id_str,
                    "message_id": message_id_str,
                    "content_length": msg.content.chars().count(),
                    "note": "message_edited_in_discord",
                });

                Ok(result.to_string())
            }

            // --------------------
            // Fetch message
            // --------------------
            "fetch_message" => {
                let channel_id_str = get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let message_id_str = get_str_arg(&args, "message_id")?;
                let message_id = MessageId::from_str(message_id_str)
                    .map_err(|e| format!("Invalid 'message_id': {e}"))?;

                let msg = channel_id
                    .message(&http, message_id)
                    .await
                    .map_err(|e| format!("Failed to fetch message: {e}"))?;

                let result = json!({
                    "status": "ok",
                    "operation": operation,
                    "channel_id": channel_id_str,
                    "message_id": message_id_str,
                    "author_id": msg.author.id.to_string(),
                    "author_name": msg.author.name,
                    "content": msg.content,
                    "timestamp": msg.timestamp.to_string(),
                });

                Ok(result.to_string())
            }

            // --------------------
            // Search messages
            // --------------------
            "search_messages" => {
                let channel_id_str = get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let query = get_str_arg(&args, "query")?;

                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50)
                    .min(100) as u8;

                let messages: Vec<Message> = channel_id
                    .messages(&http, GetMessages::new().limit(limit))
                    .await
                    .map_err(|e| format!("Failed to fetch messages: {e}"))?;

                let lower_query = query.to_lowercase();
                let matched: Vec<serde_json::Value> = messages
                    .into_iter()
                    .filter(|m| m.content.to_lowercase().contains(&lower_query))
                    .map(|m| {
                        json!({
                            "message_id": m.id.to_string(),
                            "author_id": m.author.id.to_string(),
                            "author_name": m.author.name,
                            "content": m.content,
                            "timestamp": m.timestamp.to_string(),
                        })
                    })
                    .collect();

                let result = json!({
                    "status": "ok",
                    "operation": operation,
                    "channel_id": channel_id_str,
                    "query": query,
                    "matched_count": matched.len(),
                    "messages": matched,
                });

                Ok(result.to_string())
            }

            // --------------------
            // List channels in guild
            // --------------------
            "list_channels" => {
                let guild_id = Self::parse_guild_id(&args)?;
                let guild_id_str = guild_id.to_string();
                let filter = get_opt_str_arg(&args, "channel_type").unwrap_or("all");
                let include_members =
                    Self::get_opt_bool_arg(&args, "include_members").unwrap_or(false);

                let channels = guild_id
                    .channels(&http)
                    .await
                    .map_err(|e| format!("Failed to list channels: {e}"))?;

                let guild_ref = cache.guild(guild_id);

                let mut rows = channels
                    .into_values()
                    .filter(|ch| Self::channel_matches_filter(ch.kind, filter))
                    .collect::<Vec<_>>();

                Self::sort_channels(&mut rows);

                let channels_json = rows
                    .into_iter()
                    .map(|ch| {
                        let members = if include_members && Self::is_voice_channel_type(ch.kind) {
                            guild_ref
                                .as_ref()
                                .map(|g| Self::build_voice_channel_members(g, ch.id, false))
                                .unwrap_or_default()
                        } else {
                            Vec::new()
                        };

                        Self::build_channel_json(ch, members, true)
                    })
                    .collect::<Vec<_>>();

                let result = json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_str,
                    "channel_type": filter,
                    "include_members": include_members,
                    "channel_count": channels_json.len(),
                    "channels": channels_json,
                });

                Ok(result.to_string())
            }

            // --------------------
            // List voice/stage channels (+ optional members)
            // --------------------
            "list_voice_channels" => {
                let guild_id = Self::parse_guild_id(&args)?;
                let guild_id_str = guild_id.to_string();
                let include_members =
                    Self::get_opt_bool_arg(&args, "include_members").unwrap_or(true);

                let channels = guild_id
                    .channels(&http)
                    .await
                    .map_err(|e| format!("Failed to list channels: {e}"))?;

                let guild_ref = cache
                    .guild(guild_id)
                    .ok_or_else(|| "Guild is not available in cache. Try again after the bot has fully joined and received voice states.".to_string())?;

                let mut voice_channels = channels
                    .into_values()
                    .filter(|ch| Self::is_voice_channel_type(ch.kind))
                    .collect::<Vec<_>>();

                Self::sort_channels(&mut voice_channels);

                let channels_json = voice_channels
                    .into_iter()
                    .map(|ch| {
                        let members = if include_members {
                            Self::build_voice_channel_members(&guild_ref, ch.id, true)
                        } else {
                            Vec::new()
                        };

                        Self::build_channel_json(ch, members, false)
                    })
                    .collect::<Vec<_>>();

                let result = json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_str,
                    "include_members": include_members,
                    "voice_channel_count": channels_json.len(),
                    "channels": channels_json,
                });

                Ok(result.to_string())
            }

            // --------------------
            // List members currently in voice channels (all or specific)
            // --------------------
            "list_voice_members" => {
                let guild_id = Self::parse_guild_id(&args)?;
                let guild_id_str = guild_id.to_string();

                let filter_channel_id = match get_opt_str_arg(&args, "channel_id") {
                    Some(s) => Some(
                        ChannelId::from_str(s).map_err(|e| format!("Invalid 'channel_id': {e}"))?,
                    ),
                    None => None,
                };

                let guild_ref = cache
                    .guild(guild_id)
                    .ok_or_else(|| "Guild is not available in cache. Try again after the bot has fully joined and received voice states.".to_string())?;

                let mut members = guild_ref
                    .voice_states
                    .iter()
                    .filter(|(_, state)| match filter_channel_id {
                        Some(ch) => state.channel_id == Some(ch),
                        None => state.channel_id.is_some(),
                    })
                    .map(|(user_id, state)| {
                        let display_name = guild_ref
                            .members
                            .get(user_id)
                            .map(|m| m.display_name().to_string())
                            .unwrap_or_else(|| "(unknown)".to_string());

                        json!({
                            "user_id": user_id.to_string(),
                            "display_name": display_name,
                            "channel_id": state.channel_id.map(|v| v.to_string()),
                            "self_mute": state.self_mute,
                            "self_deaf": state.self_deaf,
                            "mute": state.mute,
                            "deaf": state.deaf,
                        })
                    })
                    .collect::<Vec<_>>();

                members.sort_by(|a, b| {
                    let a_channel = a.get("channel_id").and_then(|v| v.as_str()).unwrap_or("");
                    let b_channel = b.get("channel_id").and_then(|v| v.as_str()).unwrap_or("");
                    let a_name = a.get("display_name").and_then(|v| v.as_str()).unwrap_or("");
                    let b_name = b.get("display_name").and_then(|v| v.as_str()).unwrap_or("");
                    a_channel.cmp(b_channel).then_with(|| a_name.cmp(b_name))
                });

                let result = json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_str,
                    "channel_id": filter_channel_id.map(|v| v.to_string()),
                    "member_count": members.len(),
                    "members": members,
                });

                Ok(result.to_string())
            }

            other => Err(format!(
                "Unsupported 'operation': {other}. \
                 Use one of: add_reaction, remove_reaction, create_thread, \
                 send_message, send_ephemeral_message, edit_message, fetch_message, search_messages, \
                 list_channels, list_voice_channels, list_voice_members, get_user_profile, \
                 search_users, list_recent_users."
            )),
        }
    }

    async fn execute_with_context_result(
        &self,
        args: serde_json::Value,
        ob_ctx: crate::app::context::NelfieContext,
        context: crate::llm::tool::ToolExecutionContext,
    ) -> Result<crate::llm::tool::ToolExecutionResult, String> {
        match args.get("operation").and_then(|value| value.as_str()) {
            Some("get_user_profile") => {
                return Self::user_profile_result(&args, &ob_ctx, Some(context.channel_id)).await;
            }
            Some("search_users") => {
                return Self::search_users_result(&args, &ob_ctx, Some(context.channel_id))
                    .await
                    .map(crate::llm::tool::ToolExecutionResult::text);
            }
            Some("list_recent_users") => {
                return Self::list_recent_users_result(&args, &ob_ctx, Some(context.channel_id))
                    .await
                    .map(crate::llm::tool::ToolExecutionResult::text);
            }
            _ => {}
        }

        self.execute(args, ob_ctx)
            .await
            .map(crate::llm::tool::ToolExecutionResult::text)
    }
}
