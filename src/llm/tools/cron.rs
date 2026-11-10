use std::str::FromStr;

use serde_json::json;
use serenity::all::{ChannelId, GuildId};

use crate::{app::context::NelfieContext, llm::tool::LMTool};

use super::{get_opt_non_empty_trimmed_str_arg, get_non_empty_trimmed_str_arg};

pub struct CronTool;

impl CronTool {
    pub fn new() -> Self {
        Self
    }

    fn parse_guild_id(args: &serde_json::Value) -> Result<GuildId, String> {
        let guild_id = get_non_empty_trimmed_str_arg(args, "guild_id")?;
        GuildId::from_str(guild_id).map_err(|e| format!("Invalid 'guild_id': {e}"))
    }

    fn parse_channel_id(args: &serde_json::Value) -> Result<ChannelId, String> {
        let channel_id = get_non_empty_trimmed_str_arg(args, "channel_id")?;
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
        "Manage persistent scheduled prompts for Discord. Use create only when the user asks for a scheduled or recurring action; it stores a prompt for the LLM to run and post on that schedule. Use list to inspect jobs and delete with the job ID to remove one. Schedules use server local time. Use the current guild and channel unless the user explicitly names another destination.".to_string()
    }

    fn json_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "description": "Choose one job action: create a scheduled prompt, list jobs, or delete a job by ID.",
                    "enum": ["create", "list", "delete"]
                },
                "guild_id": {
                    "type": "string",
                    "description": "Discord server containing the target channel. Required for create, list, and delete."
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
                    "description": "Instruction the LLM will run automatically when the schedule fires. Used only by create."
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
        let operation = get_non_empty_trimmed_str_arg(&args, "operation")?;

        match operation {
            "create" => {
                let guild_id = Self::parse_guild_id(&args)?;
                let channel_id = Self::parse_channel_id(&args)?;
                let schedule = get_non_empty_trimmed_str_arg(&args, "schedule")?.to_string();
                let prompt = get_non_empty_trimmed_str_arg(&args, "prompt")?.to_string();
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

                let mut jobs = if let Some(channel_id) = get_opt_non_empty_trimmed_str_arg(&args, "channel_id")
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
                let id = get_non_empty_trimmed_str_arg(&args, "id")?;
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
