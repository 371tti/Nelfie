use std::str::FromStr;

use serde_json::json;
use serenity::all::{
    Builder, ChannelId, ChannelType, CreateMessage, CreateThread, EditMessage, GetMessages,
    GuildChannel, GuildId, Message, MessageId, ReactionType,
};
use serenity::model::guild::Guild;

use crate::llm::client::{LMTool, Role};

pub struct DiscordTool;

impl DiscordTool {
    pub fn new() -> DiscordTool {
        DiscordTool {}
    }

    fn get_str_arg<'a>(args: &'a serde_json::Value, key: &'a str) -> Result<&'a str, String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("Missing or invalid '{key}' parameter"))
    }

    fn get_opt_bool_arg(args: &serde_json::Value, key: &str) -> Option<bool> {
        args.get(key).and_then(|v| v.as_bool())
    }

    fn get_opt_str_arg<'a>(args: &'a serde_json::Value, key: &'a str) -> Option<&'a str> {
        args.get(key).and_then(|v| v.as_str())
    }

    fn parse_guild_id(args: &serde_json::Value) -> Result<GuildId, String> {
        let guild_id_str = Self::get_str_arg(args, "guild_id")?;
        GuildId::from_str(guild_id_str).map_err(|e| format!("Invalid 'guild_id': {e}"))
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
        "Interact with Discord: add/remove reactions, create threads, send/edit/fetch/search messages, and inspect channels/voice presence. Important: when you use send_message, the text is already posted to Discord, so do not repeat the same body again in a normal assistant reply; only send a short confirmation if needed.".to_string()
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
                        "edit_message",
                        "fetch_message",
                        "search_messages",
                        "list_channels",
                        "list_voice_channels",
                        "list_voice_members"
                    ]
                },
                "guild_id": {
                    "type": "string",
                    "description": "ID of the target guild. Required for: list_channels, list_voice_channels, list_voice_members."
                },
                "channel_id": {
                    "type": "string",
                    "description": "ID of the target channel. Required for message/reaction/thread operations. Optional filter for list_voice_members."
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
                    "description": "Message content. Used by: send_message, edit_message. For send_message, this body is already published to Discord immediately, so do not repeat it again in a separate assistant message."
                },
                "reply_to": {
                    "type": "string",
                    "description": "Message ID to reply to. Optional. Used by: send_message."
                },
                "query": {
                    "type": "string",
                    "description": "Keyword to search in message content. Used by: search_messages."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max number of recent messages to scan (1–100). Defaults to 50. Used by: search_messages."
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
            // --------------------
            // Reaction: add
            // --------------------
            "add_reaction" => {
                let channel_id_str = Self::get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let message_id_str = Self::get_str_arg(&args, "message_id")?;
                let reaction = Self::get_str_arg(&args, "reaction")?;

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
                let channel_id_str = Self::get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let message_id_str = Self::get_str_arg(&args, "message_id")?;
                let reaction = Self::get_str_arg(&args, "reaction")?;

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
                let channel_id_str = Self::get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let name = Self::get_str_arg(&args, "name")?;

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
                let channel_id_str = Self::get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let content = Self::get_str_arg(&args, "content")?;

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
            // Edit message
            // --------------------
            "edit_message" => {
                let channel_id_str = Self::get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let message_id_str = Self::get_str_arg(&args, "message_id")?;
                let content = Self::get_str_arg(&args, "content")?;

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
                let channel_id_str = Self::get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let message_id_str = Self::get_str_arg(&args, "message_id")?;
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
                let channel_id_str = Self::get_str_arg(&args, "channel_id")?;
                let channel_id = ChannelId::from_str(channel_id_str)
                    .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                let query = Self::get_str_arg(&args, "query")?;

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
                let filter = Self::get_opt_str_arg(&args, "channel_type").unwrap_or("all");
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

                let filter_channel_id = match Self::get_opt_str_arg(&args, "channel_id") {
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
                 send_message, edit_message, fetch_message, search_messages, \
                 list_channels, list_voice_channels, list_voice_members."
            )),
        }
    }
}
