use std::time::Instant;

use log::{error, info};
use poise::CreateReply;
use serenity::all::CreateAttachment;

use crate::llm::{models::Models, tools::latex::LatexExprRenderTool};

use super::shared::{Context, Error};

/// ping pong..
#[poise::command(slash_command, prefix_command)]
pub async fn ping(ctx: Context<'_>) -> Result<(), Error> {
    let start = Instant::now();

    // まずメッセージ送信
    let msg = ctx.say("応答時間を計測中...").await?;

    let elapsed = start.elapsed().as_millis();

    // CreateReply を作って渡す
    msg.edit(
        ctx,
        CreateReply::default().content(format!("Pong! `{elapsed}ms`")),
    )
    .await?;

    Ok(())
}

/// only admin user
#[poise::command(slash_command, prefix_command)]
pub async fn set_system_prompt(
    ctx: Context<'_>,

    #[description = "System prompt to set (or 'reset' to default)"] system_prompt: String,
) -> Result<(), Error> {
    let ob_ctx = ctx.data();

    let caller_id_u64 = ctx.author().id.get();
    if !ob_ctx.config.admin_users.contains(&caller_id_u64) {
        ctx.say("エラー: /set_system_prompt を実行する権限がありません。")
            .await?;
        return Ok(());
    }

    let channel_id = ctx.channel_id();

    if system_prompt.eq_ignore_ascii_case("reset") {
        ob_ctx.chat_contexts.set_system_prompt(channel_id, None);
        ctx.say("info: システムプロンプトをデフォルトに戻しました。")
            .await?;
    } else {
        ob_ctx
            .chat_contexts
            .set_system_prompt(channel_id, Some(system_prompt.clone()));
        ctx.say(format!(
            "info: システムプロンプトを更新しました。\n```{}```",
            system_prompt
        ))
        .await?;
    }

    Ok(())
}

/// clear context
#[poise::command(slash_command, prefix_command)]
pub async fn clear(ctx: Context<'_>) -> Result<(), Error> {
    let channel_id = ctx.channel_id();

    let ob_ctx = ctx.data();

    ob_ctx.chat_contexts.clear(channel_id);

    info!("Cleared chat context for channel {}", channel_id);

    ctx.say("info: チャットコンテキストをクリアしました。")
        .await?;

    Ok(())
}

/// to enable nelfie bot
#[poise::command(slash_command, prefix_command)]
pub async fn enable(ctx: Context<'_>) -> Result<(), Error> {
    let channel_id = ctx.channel_id();

    let ob_ctx = ctx.data();

    if ob_ctx.chat_contexts.is_enabled(channel_id) {
        ctx.say("info: このチャンネルのチャットコンテキストは既に有効です。")
            .await?;
        Ok(())
    } else {
        ob_ctx.chat_contexts.set_enabled(channel_id, true);
        ob_ctx.chat_contexts.get_or_create(channel_id);
        ctx.say("info: このチャンネルのチャットコンテキストを有効化しました。")
            .await?;
        info!("Enabled chat context for channel {}", channel_id);
        Ok(())
    }
}

/// to disable nelfie bot
#[poise::command(slash_command, prefix_command)]
pub async fn disable(ctx: Context<'_>) -> Result<(), Error> {
    let channel_id = ctx.channel_id();

    let ob_ctx = ctx.data();

    if !ob_ctx.chat_contexts.is_enabled(channel_id) {
        ctx.say("info: このチャンネルのチャットコンテキストは既に無効です。")
            .await?;
        Ok(())
    } else {
        ob_ctx.chat_contexts.set_enabled(channel_id, false);
        ctx.say("info: このチャンネルのチャットコンテキストを無効化しました。")
            .await?;
        info!("Disabled chat context for channel {}", channel_id);
        Ok(())
    }
}

/// model config command
#[poise::command(slash_command, prefix_command, subcommands("get", "set", "list"))]
pub async fn model(_: Context<'_>) -> Result<(), Error> {
    Ok(()) // ここはメインでは使わない
}

#[poise::command(slash_command, prefix_command)]
pub async fn get(ctx: Context<'_>) -> Result<(), Error> {
    let ob_ctx = ctx.data();
    let user_id = ctx.author().id;
    let model = ob_ctx.user_contexts.get_or_create(user_id).main_model;
    ctx.say(format!(
        "現在のモデル: `{}` / cost **x{}**",
        model,
        model.rate_cost()
    ))
    .await?;
    Ok(())
}

#[poise::command(slash_command, prefix_command)]
pub async fn list(ctx: Context<'_>) -> Result<(), Error> {
    let models = Models::list();

    let mut s = String::from("**利用可能なモデル:**\n");
    for m in models {
        s.push_str(&format!("- `{}` / cost **x{}**\n", m, m.rate_cost()));
    }

    ctx.say(s).await?;
    Ok(())
}

#[poise::command(slash_command, prefix_command)]
pub async fn set(
    ctx: Context<'_>,
    #[description = "Choose a model"]
    #[autocomplete = "autocomplete_model_name"]
    model_name: String,
) -> Result<(), Error> {
    let ob_ctx = ctx.data();
    let user_id = ctx.author().id;
    let model = Models::from(model_name);
    ob_ctx.user_contexts.set_model(user_id, model);

    ctx.say(format!(
        "info: モデルを `{}`（cost x{}）に変更しました。",
        model,
        model.rate_cost()
    ))
    .await?;
    Ok(())
}

async fn autocomplete_model_name(_ctx: Context<'_>, partial: &str) -> Vec<String> {
    let models = Models::list();
    models
        .into_iter()
        .filter(|m| m.to_string().starts_with(partial))
        .map(|m| m.to_string())
        .collect()
}

/// latex expr render
#[poise::command(slash_command, prefix_command)]
pub async fn tex_expr(
    ctx: Context<'_>,
    #[description = "LaTeX expression to render"]
    #[autocomplete = "autocomplete_tex_expr"]
    expr: String,
) -> Result<(), Error> {
    let png_bytes = match LatexExprRenderTool::render(&expr) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("Failed to render LaTeX expression `{}`: {}", expr, e);
            ctx.say(format!("エラー: LaTeX のレンダリングに失敗しました: {}", e))
                .await?;
            return Ok(());
        }
    };

    let attachment = CreateAttachment::bytes(png_bytes, "tex_expr.png");

    ctx.send(CreateReply::default().attachment(attachment))
        .await?;

    Ok(())
}

async fn autocomplete_tex_expr(_ctx: Context<'_>, partial: &str) -> Vec<String> {
    // LaTeX コマンド単体候補
    const COMMANDS: &[&str] = &[
        r"\alpha",
        r"\beta",
        r"\gamma",
        r"\delta",
        r"\sin",
        r"\cos",
        r"\tan",
        r"\log",
        r"\ln",
        r"\sqrt{}",
        r"\frac{}{}",
        r"\int_0^1",
        r"\sum_{n=0}^{\infty}",
        r"\prod_{i=1}^{n}",
        r"\lim_{x \to 0}",
        r"\infty",
        r"\mathbb{R}",
        r"\mathbb{Z}",
        r"\mathbb{N}",
    ];

    // ある程度完成された数式テンプレ
    const SNIPPETS: &[&str] = &[
        r"\int_0^1 x^2 \, dx",
        r"\sum_{n=0}^{\infty} a_n x^n",
        r"\lim_{x \to 0} \frac{\sin x}{x}",
        r"e^{i\pi} + 1 = 0",
        r"a^2 + b^2 = c^2",
        r"\frac{d}{dx} f(x)",
        r"\nabla \cdot \vec{E} = \frac{\rho}{\varepsilon_0}",
    ];

    let mut candidates: Vec<String> = Vec::new();

    // まずコマンド候補
    for &c in COMMANDS {
        if partial.is_empty() || c.starts_with(partial) || c.contains(partial) {
            candidates.push(c.to_string());
        }
    }

    // つぎにテンプレ数式
    for &s in SNIPPETS {
        if partial.is_empty() || s.starts_with(partial) || s.contains(partial) {
            candidates.push(s.to_string());
        }
    }

    // ダブり削除 & 最大 20 個くらいに絞る
    candidates.sort();
    candidates.dedup();
    candidates.truncate(20);

    candidates
}
