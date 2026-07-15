use poise::CreateReply;
use serenity::all::{CreateEmbed, User, UserId};

use crate::llm::user::UserContextScope;

use super::shared::{Context, Error};

/// only admin user
#[poise::command(slash_command, prefix_command)]
pub async fn rate_config(
    ctx: Context<'_>,

    #[description = "Target user"] target_user: User, // ← ここが Discord のユーザー選択になる

    #[description = "consumption cost value: 'unlimit' or a number"]
    #[autocomplete = "autocomplete_rate_limit"]
    limit: String,
) -> Result<(), Error> {
    let ob_ctx = ctx.data();

    let caller_id_u64 = ctx.author().id.get();
    if !ob_ctx.config.admin_users.contains(&caller_id_u64) {
        ctx.say("エラー: /rate_config を実行する権限がありません。")
            .await?;
        return Ok(());
    }

    let target_user_id: UserId = target_user.id;
    let bot_user_id = ctx.serenity_context().cache.current_user().id;
    let target_scope = UserContextScope::for_actor(target_user_id, ctx.guild_id(), bot_user_id);

    let new_rate_line: u64 = if limit.eq_ignore_ascii_case("unlimit") {
        0
    } else if limit.eq_ignore_ascii_case("reset") {
        1
    } else {
        let cost = match limit.parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                ctx.say(
                    "エラー: limit は 'unlimit' / 'reset' / 数値 のいずれかを指定してください。",
                )
                .await?;
                return Ok(());
            }
        };
        ob_ctx
            .user_contexts
            .get_or_create_scoped(target_scope)
            .rate_line
            + cost * ob_ctx.config.rate_limit_sec_per_cost
    };

    ob_ctx
        .user_contexts
        .set_rate_line_scoped(target_scope, new_rate_line);

    let scope_label = match target_scope {
        UserContextScope::User(_) => "user".to_string(),
        UserContextScope::GuildBot { guild_id, .. } => {
            format!("guild bot scope `{}`", guild_id.get())
        }
    };

    let reply = if new_rate_line == 0 {
        format!(
            "info: ユーザー `{}` ({}) のレート制限を **unlimit** に設定しました。",
            target_user_id
                .to_user(ctx.http())
                .await
                .map(|u| u.display_name().to_string())
                .unwrap_or_else(|_| "Null".to_string()),
            scope_label
        )
    } else {
        format!(
            "info: ユーザー `{}` ({}) の rate_line を **{}** に設定しました。",
            target_user_id
                .to_user(ctx.http())
                .await
                .map(|u| u.display_name().to_string())
                .unwrap_or_else(|_| "Null".to_string()),
            scope_label,
            new_rate_line
        )
    };

    ctx.say(reply).await?;
    Ok(())
}

/// `/rate_config` の第2引数 `limit` 用のオートコンプリート
async fn autocomplete_rate_limit(_ctx: Context<'_>, partial: &str) -> Vec<String> {
    let base_candidates = [
        "unlimit", "reset", "1", "2", "3", "5", "10", "30", "60", "120", "300", "600", "1800",
        "3600",
    ];

    let p = partial.to_lowercase();

    let mut out: Vec<String> = base_candidates
        .iter()
        .filter(|v| v.to_lowercase().starts_with(&p))
        .map(|v| v.to_string())
        .collect();

    out.sort();
    out.dedup();
    out.truncate(20);
    out
}

#[derive(Debug)]
struct RateBucketStatus {
    rate_line: u64,
    remaining_units: Option<u64>,
    capacity_units: u64,
    recovery_units_per_hour: f64,
    fully_recovers_at: Option<u64>,
}

impl RateBucketStatus {
    fn calculate(rate_line: u64, now: u64, window_size: u64, seconds_per_unit: u64) -> Self {
        let capacity_units = window_size / seconds_per_unit;
        let recovery_units_per_hour = 3_600.0 / seconds_per_unit as f64;

        if rate_line == 0 {
            return Self {
                rate_line,
                remaining_units: None,
                capacity_units,
                recovery_units_per_hour,
                fully_recovers_at: None,
            };
        }

        let effective_line = rate_line.max(now);
        let available_seconds = now
            .saturating_add(window_size)
            .saturating_sub(effective_line);

        Self {
            rate_line,
            remaining_units: Some(available_seconds / seconds_per_unit),
            capacity_units,
            recovery_units_per_hour,
            fully_recovers_at: (rate_line > now).then_some(rate_line),
        }
    }

    fn render(&self) -> String {
        let remaining = match self.remaining_units {
            Some(remaining) => format!("x{remaining} / x{}", self.capacity_units),
            None => "unlimited".to_string(),
        };
        let rate_line = match self.fully_recovers_at {
            Some(timestamp) => format!("{} (<t:{}:R> に全回復)", self.rate_line, timestamp),
            None if self.rate_line == 0 => "0 (unlimited)".to_string(),
            None => format!("{} (全回復済み)", self.rate_line),
        };

        format!(
            "残り: **{remaining}**\n毎時回復: **{} / 時間**\nrate_line: {rate_line}",
            format_multiplier(self.recovery_units_per_hour)
        )
    }
}

fn format_multiplier(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("x{}", value as u64)
    } else {
        format!("x{value:.2}")
    }
}

/// Show the current general and cron rate-limit buckets.
#[poise::command(slash_command, prefix_command)]
pub async fn rate_status(
    ctx: Context<'_>,
    #[description = "Target user (default: yourself)"] target_user: Option<User>,
) -> Result<(), Error> {
    let ob_ctx = ctx.data();
    let target_user_id = target_user
        .as_ref()
        .map(|user| user.id)
        .unwrap_or(ctx.author().id);
    let bot_user_id = ctx.serenity_context().cache.current_user().id;
    let target_scope = UserContextScope::for_actor(target_user_id, ctx.guild_id(), bot_user_id);
    let user_context = ob_ctx.user_contexts.get_or_create_scoped(target_scope);
    let now = chrono::Utc::now().timestamp().max(0) as u64;

    let general = RateBucketStatus::calculate(
        user_context.rate_line,
        now,
        ob_ctx.config.rate_limit_window_size,
        ob_ctx.config.rate_limit_sec_per_cost,
    );
    let cron = RateBucketStatus::calculate(
        user_context.cron_rate_line,
        now,
        ob_ctx.config.cron_rate_limit_window_size,
        ob_ctx.config.cron_rate_limit_sec_per_run,
    );

    let scope_label = match target_scope {
        UserContextScope::User(_) => "user".to_string(),
        UserContextScope::GuildBot { guild_id, .. } => {
            format!("guild bot / guild {}", guild_id.get())
        }
    };
    let model_cost = user_context.main_model.rate_cost();
    let remaining_calls = match general.remaining_units {
        Some(units) => format!("あと **{}回**", units / model_cost),
        None => "**unlimited**".to_string(),
    };
    let display_name = target_user
        .map(|user| user.display_name().to_string())
        .unwrap_or_else(|| ctx.author().display_name().to_string());

    let embed = CreateEmbed::new()
        .title("レート状態")
        .field(
            "target",
            format!(
                "{} ({})\nscope: {}",
                display_name,
                target_user_id.get(),
                scope_label
            ),
            false,
        )
        .field(
            "model",
            format!(
                "{} / cost **x{}**\n現在の残量では {}",
                user_context.main_model, model_cost, remaining_calls
            ),
            false,
        )
        .field("general", general.render(), true)
        .field("cron", cron.render(), true);

    ctx.send(CreateReply::default().embed(embed)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_bucket_reports_capacity_remaining_and_hourly_recovery() {
        let status = RateBucketStatus::calculate(1_000, 1_000, 16_200, 600);

        assert_eq!(status.remaining_units, Some(27));
        assert_eq!(status.capacity_units, 27);
        assert_eq!(status.recovery_units_per_hour, 6.0);
        assert_eq!(status.fully_recovers_at, None);
    }

    #[test]
    fn rate_bucket_accounts_for_future_rate_line() {
        let status = RateBucketStatus::calculate(1_600, 1_000, 16_200, 600);

        assert_eq!(status.remaining_units, Some(26));
        assert_eq!(status.fully_recovers_at, Some(1_600));
    }

    #[test]
    fn zero_rate_line_is_unlimited() {
        let status = RateBucketStatus::calculate(0, 1_000, 16_200, 600);

        assert_eq!(status.remaining_units, None);
        assert!(status.render().contains("unlimited"));
    }
}
