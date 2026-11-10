use std::str::FromStr;

use serde_json::{Value, json};
use serenity::all::{CreateAllowedMentions, CreateMessage, MessageId, UserId};

use crate::{
    app::context::NelfieContext,
    llm::tool::{LMTool, ToolExecutionContext},
};

const DISCORD_MESSAGE_LIMIT: usize = 2_000;

pub struct DiscordReplyTool;

impl DiscordReplyTool {
    fn reply_to(args: &Value, context: &ToolExecutionContext) -> Result<Option<MessageId>, String> {
        let Some(value) = args.get("reply_to") else {
            return Ok(context.reply_to_message_id);
        };
        let value = value
            .as_str()
            .ok_or_else(|| "'reply_to' must be a message ID, 'request', or 'none'.".to_string())?;

        match value {
            "none" => Ok(None),
            "request" => context
                .reply_to_message_id
                .map(Some)
                .ok_or_else(|| "There is no source message to reply to in this request.".into()),
            message_id => MessageId::from_str(message_id)
                .map(Some)
                .map_err(|error| format!("Invalid 'reply_to' message ID: {error}")),
        }
    }

    fn mention_user(
        args: &Value,
        context: &ToolExecutionContext,
    ) -> Result<Option<UserId>, String> {
        let Some(value) = args.get("mention") else {
            return Ok(None);
        };
        let value = value.as_str().ok_or_else(|| {
            "'mention' must be 'request_author' or a Discord user ID.".to_string()
        })?;

        match value {
            "request_author" => context
                .request_author_id
                .map(Some)
                .ok_or_else(|| "There is no request author to mention.".into()),
            user_id => UserId::from_str(user_id)
                .map(Some)
                .map_err(|error| format!("Invalid 'mention' user ID: {error}")),
        }
    }

    async fn send(
        args: Value,
        ob_context: NelfieContext,
        context: ToolExecutionContext,
    ) -> Result<String, String> {
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| "Missing or invalid 'content' parameter.".to_string())?;
        if content.trim().is_empty() {
            return Err("'content' must not be empty.".into());
        }

        let reply_to = Self::reply_to(&args, &context)?;
        let mention = Self::mention_user(&args, &context)?;
        let content = if let Some(user_id) = mention {
            let mention_text = format!("<@{}>", user_id.get());
            if content.trim_start().starts_with(&mention_text) {
                content.to_string()
            } else {
                format!("{mention_text} {content}")
            }
        } else {
            content.to_string()
        };
        if content.chars().count() > DISCORD_MESSAGE_LIMIT {
            return Err(format!(
                "The message exceeds Discord's {DISCORD_MESSAGE_LIMIT}-character limit."
            ));
        }

        let allowed_users = mention.into_iter().collect::<Vec<_>>();
        let allowed_mentions = CreateAllowedMentions::new()
            .users(allowed_users)
            .all_users(false)
            .all_roles(false)
            .everyone(false)
            .replied_user(false);
        let mut builder = CreateMessage::new()
            .content(&content)
            .allowed_mentions(allowed_mentions);
        if let Some(reply_to) = reply_to {
            builder = builder.reference_message((context.channel_id, reply_to));
        }

        let discord_client = ob_context.discord_client.open();
        let message = context
            .channel_id
            .send_message(&discord_client.http, builder)
            .await
            .map_err(|error| format!("Failed to send Discord reply: {error}"))?;

        Ok(json!({
            "status": "ok",
            "channel_id": context.channel_id.to_string(),
            "message_id": message.id.to_string(),
            "content": content,
            "reply_to": reply_to.map(|id| id.to_string()),
            "mention_user_id": mention.map(|id| id.to_string()),
            "note": "already_posted_to_discord_do_not_repeat_body"
        })
        .to_string())
    }
}

#[async_trait::async_trait]
impl LMTool for DiscordReplyTool {
    fn name(&self) -> String {
        "discord-reply".into()
    }

    fn description(&self) -> String {
        "Post one finished message immediately to the current Discord channel. Normal assistant text is sent automatically; use this only when a reply reference, a targeted user ping, or an explicitly requested separate post is needed. For one intended reply, make exactly one call: do not split it across calls, repeat it, or make parallel discord-reply calls. If the user explicitly asks for multiple distinct posts, make one call per post sequentially and wait for each result. Omit reply_to to reply to the request message; use 'none' for a standalone post. Only the selected user can be pinged. Use discord-tool for other channels or Discord actions.".into()
    }

    fn json_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": DISCORD_MESSAGE_LIMIT,
                    "description": "The content of this single Discord message."
                },
                "reply_to": {
                    "type": "string",
                    "description": "Reply target: a Discord message ID, 'request' for the message that triggered this response, or 'none' for a standalone post. Omit to use the request message by default. A returned message_id can chain the next call."
                },
                "mention": {
                    "type": "string",
                    "description": "Optional user to ping: 'request_author' for the user who triggered this response, or a known Discord user ID. Omit for no ping."
                }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: Value, _ob_context: NelfieContext) -> Result<String, String> {
        Err("discord-reply requires the current response context.".into())
    }

    async fn execute_with_context(
        &self,
        args: Value,
        ob_context: NelfieContext,
        context: ToolExecutionContext,
    ) -> Result<String, String> {
        Self::send(args, ob_context, context).await
    }
}
