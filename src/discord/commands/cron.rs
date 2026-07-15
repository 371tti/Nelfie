use poise::CreateReply;
use serenity::all::{AutocompleteChoice, CreateEmbed};

use super::shared::{Context, Error, preview_text};

/// Register a scheduled LLM prompt in this channel.
#[poise::command(slash_command, prefix_command)]
pub async fn cron(
    ctx: Context<'_>,
    #[description = "Cron expression, for example: */30 * * * *"] schedule: String,
    #[description = "Prompt to send automatically"]
    #[rest]
    prompt: String,
) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        ctx.say("エラー: cron はサーバーチャンネル内でのみ登録できます。")
            .await?;
        return Ok(());
    };

    let ob_ctx = ctx.data();
    let job = match ob_ctx.cron_scheduler.register(
        guild_id,
        ctx.channel_id(),
        ctx.author().id,
        schedule,
        prompt,
    ) {
        Ok(job) => job,
        Err(e) => {
            ctx.say(format!("エラー: cron 登録に失敗しました: {e}"))
                .await?;
            return Ok(());
        }
    };

    let embed = CreateEmbed::new()
        .title("cron登録")
        .description("定期実行プロンプトを登録しました。")
        .field("id", format!("`{}`", job.id), false)
        .field("channel", format!("<#{}>", job.channel_id), true)
        .field("schedule", format!("`{}`", job.schedule), true)
        .field(
            "next_run",
            format!("<t:{}:F> / <t:{}:R>", job.next_run_at, job.next_run_at),
            false,
        )
        .field("prompt", preview_text(&job.prompt, 240), false);

    ctx.send(CreateReply::default().embed(embed)).await?;
    Ok(())
}

/// Run an already registered cron job immediately.
#[poise::command(slash_command, prefix_command)]
pub async fn cron_test(
    ctx: Context<'_>,
    #[description = "Cron ID. If omitted, runs the only cron registered in this channel."]
    #[autocomplete = "autocomplete_cron_id"]
    id: Option<String>,
) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        ctx.say("エラー: cron_test はサーバーチャンネル内でのみ使用できます。")
            .await?;
        return Ok(());
    };

    let ob_ctx = ctx.data();
    let job = if let Some(id) = id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        let Some(job) = ob_ctx.cron_scheduler.get_job(id) else {
            ctx.say(format!("エラー: cron `{id}` は登録されていません。"))
                .await?;
            return Ok(());
        };

        if job.guild_id != guild_id.get() {
            ctx.say(format!(
                "エラー: cron `{id}` はこのサーバーの登録ではありません。"
            ))
            .await?;
            return Ok(());
        }

        job
    } else {
        let jobs = ob_ctx
            .cron_scheduler
            .jobs_for_channel(guild_id, ctx.channel_id());
        match jobs.len() {
            0 => {
                ctx.say("エラー: このチャンネルには登録済み cron がありません。")
                    .await?;
                return Ok(());
            }
            1 => jobs[0].clone(),
            _ => {
                let list = jobs
                    .iter()
                    .take(10)
                    .map(|job| {
                        format!(
                            "- `{}` `{}` {}",
                            job.id,
                            job.schedule,
                            preview_text(&job.prompt, 80)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                ctx.say(format!(
                    "エラー: このチャンネルには複数の cron があります。id を指定してください。\n{}",
                    list
                ))
                .await?;
                return Ok(());
            }
        }
    };

    ob_ctx.cron_scheduler.dispatch_job(
        ctx.serenity_context().clone(),
        ob_ctx.clone(),
        job.clone(),
        true,
    );

    ctx.say(format!(
        "info: cron_test を実行キューに入れました: `{}`",
        job.id
    ))
    .await?;
    Ok(())
}

/// Delete a registered cron job.
#[poise::command(slash_command, prefix_command)]
pub async fn del_cron(
    ctx: Context<'_>,
    #[description = "Cron ID to delete"]
    #[autocomplete = "autocomplete_cron_id"]
    id: String,
) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        ctx.say("エラー: del_cron はサーバーチャンネル内でのみ使用できます。")
            .await?;
        return Ok(());
    };

    let ob_ctx = ctx.data();
    let job = match ob_ctx.cron_scheduler.delete_job(&id, Some(guild_id)) {
        Ok(job) => job,
        Err(e) => {
            ctx.say(format!("エラー: cron 削除に失敗しました: {e}"))
                .await?;
            return Ok(());
        }
    };

    let embed = CreateEmbed::new()
        .title("cron削除")
        .description("定期実行プロンプトを削除しました。")
        .field("id", format!("`{}`", job.id), false)
        .field("channel", format!("<#{}>", job.channel_id), true)
        .field("schedule", format!("`{}`", job.schedule), true)
        .field("prompt", preview_text(&job.prompt, 240), false);

    ctx.send(CreateReply::default().embed(embed)).await?;
    Ok(())
}

async fn autocomplete_cron_id(ctx: Context<'_>, partial: &str) -> Vec<AutocompleteChoice> {
    let Some(guild_id) = ctx.guild_id() else {
        return Vec::new();
    };

    let partial = partial.trim().to_lowercase();
    ctx.data()
        .cron_scheduler
        .jobs_for_guild(guild_id)
        .into_iter()
        .filter(|job| {
            if partial.is_empty() {
                return true;
            }

            let haystack = format!(
                "{} {} {} {}",
                job.id, job.channel_id, job.schedule, job.prompt
            )
            .to_lowercase();
            haystack.contains(&partial)
        })
        .take(25)
        .map(|job| AutocompleteChoice::new(format_cron_autocomplete_label(&job), job.id))
        .collect()
}

fn format_cron_autocomplete_label(job: &crate::app::cron::CronJob) -> String {
    let label = format!(
        "{} | #{} | {} | {}",
        job.id,
        job.channel_id,
        job.schedule,
        preview_text(&job.prompt, 48)
    );
    preview_text(&label, 99)
}
