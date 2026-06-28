use std::str::FromStr;

use serde_json::json;
use serenity::all::{ChannelId, GuildId};

use crate::{app::context::NelfieContext, llm::client::LMTool};

pub struct CronTool;

impl CronTool {
    pub fn new() -> Self {
        Self
    }

    fn get_str_arg<'a>(args: &'a serde_json::Value, key: &'a str) -> Result<&'a str, String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("Missing or invalid '{key}' parameter"))
    }

    fn get_opt_str_arg<'a>(args: &'a serde_json::Value, key: &'a str) -> Option<&'a str> {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    }

    fn parse_guild_id(args: &serde_json::Value) -> Result<GuildId, String> {
        let guild_id = Self::get_str_arg(args, "guild_id")?;
        GuildId::from_str(guild_id).map_err(|e| format!("Invalid 'guild_id': {e}"))
    }

    fn parse_channel_id(args: &serde_json::Value) -> Result<ChannelId, String> {
        let channel_id = Self::get_str_arg(args, "channel_id")?;
        ChannelId::from_str(channel_id).map_err(|e| format!("Invalid 'channel_id': {e}"))
    }

    fn job_to_json(job: crate::app::cron::CronJob) -> serde_json::Value {
        json!({
            "id": job.id,
            "guild_id": job.guild_id.to_string(),
            "channel_id": job.channel_id.to_string(),
            "schedule": job.schedule,
            "prompt": job.prompt,
            "created_by": job.created_by.to_string(),
            "created_at": job.created_at,
            "last_run_at": job.last_run_at,
            "next_run_at": job.next_run_at,
            "next_run_discord": format!("<t:{}:F> / <t:{}:R>", job.next_run_at, job.next_run_at),
        })
    }
}

impl Default for CronTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl LMTool for CronTool {
    fn name(&self) -> String {
        "cron-tool".to_string()
    }

    fn description(&self) -> String {
        "Manage persistent scheduled LLM prompts for Discord channels. Use create to register a cron schedule, list to inspect registered schedules, and delete to remove one. Use the current guild_id and channel_id from the system context unless the user clearly requests another channel.".to_string()
    }

    fn json_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "description": "Cron schedule operation.",
                    "enum": ["create", "list", "delete"]
                },
                "guild_id": {
                    "type": "string",
                    "description": "Discord guild ID. Required for create/list/delete."
                },
                "channel_id": {
                    "type": "string",
                    "description": "Discord channel ID. Required for create. Optional for list to filter to one channel."
                },
                "schedule": {
                    "type": "string",
                    "description": "Cron expression in local server time. Five fields like '*/30 * * * *', or six fields only when the leading seconds field is 0. Used by create."
                },
                "prompt": {
                    "type": "string",
                    "description": "Prompt to automatically send to the LLM when the schedule fires. Used by create."
                },
                "id": {
                    "type": "string",
                    "description": "Cron ID. Required for delete."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum schedules returned by list. Defaults to 50, max 100."
                }
            },
            "required": ["operation"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ob_ctx: NelfieContext,
    ) -> Result<String, String> {
        let operation = Self::get_str_arg(&args, "operation")?;

        match operation {
            "create" => {
                let guild_id = Self::parse_guild_id(&args)?;
                let channel_id = Self::parse_channel_id(&args)?;
                let schedule = Self::get_str_arg(&args, "schedule")?.to_string();
                let prompt = Self::get_str_arg(&args, "prompt")?.to_string();
                let bot_user_id = ob_ctx.discord_client.open().cache.current_user().id;

                let job = ob_ctx.cron_scheduler.register(
                    guild_id,
                    channel_id,
                    bot_user_id,
                    schedule,
                    prompt,
                )?;

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "job": Self::job_to_json(job),
                })
                .to_string())
            }
            "list" => {
                let guild_id = Self::parse_guild_id(&args)?;
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50)
                    .clamp(1, 100) as usize;

                let mut jobs = if let Some(channel_id) = Self::get_opt_str_arg(&args, "channel_id")
                {
                    let channel_id = ChannelId::from_str(channel_id)
                        .map_err(|e| format!("Invalid 'channel_id': {e}"))?;
                    ob_ctx.cron_scheduler.jobs_for_channel(guild_id, channel_id)
                } else {
                    ob_ctx.cron_scheduler.jobs_for_guild(guild_id)
                };

                let total_count = jobs.len();
                jobs.truncate(limit);
                let jobs = jobs.into_iter().map(Self::job_to_json).collect::<Vec<_>>();

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id.to_string(),
                    "returned_count": jobs.len(),
                    "total_count": total_count,
                    "jobs": jobs,
                })
                .to_string())
            }
            "delete" => {
                let id = Self::get_str_arg(&args, "id")?;
                let guild_id = Self::parse_guild_id(&args)?;
                let job = ob_ctx.cron_scheduler.delete_job(id, Some(guild_id))?;

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "deleted_job": Self::job_to_json(job),
                })
                .to_string())
            }
            other => Err(format!(
                "Unsupported 'operation': {other}. Use one of: create, list, delete."
            )),
        }
    }
}
