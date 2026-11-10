use std::time::Instant;

use log::{error, info};
use poise::CreateReply;
use serenity::all::{Attachment, CreateAttachment, CreateEmbed};

use crate::llm::{
    channel::ChatResponseMode,
    models::{ImageGenerationModels, Models},
    tools::latex::LatexExprRenderTool,
    user::UserContextScope,
};

use super::shared::{Context, Error, preview_text};

#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
enum EnableMode {
    #[name = "mention"]
    Mention,
    #[name = "auto-reply"]
    AutoReplay,
}

impl EnableMode {
    fn response_mode(self) -> ChatResponseMode {
        match self {
            Self::Mention => ChatResponseMode::Mention,
            Self::AutoReplay => ChatResponseMode::AutoReplay,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::AutoReplay => "auto-reply",
        }
    }

    fn is_authorized_for(self, user_id: u64, admin_users: &[u64]) -> bool {
        matches!(self, Self::Mention) || admin_users.contains(&user_id)
    }
}

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

/// Show Nelfie's version and runtime information.
#[poise::command(slash_command, prefix_command)]
pub async fn status(ctx: Context<'_>) -> Result<(), Error> {
    let bot_user_id = ctx.serenity_context().cache.current_user().id;
    let runtime = format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH);
    let embed = CreateEmbed::new()
        .title("Nelfie Status")
        .field("Version", format!("v{}", env!("CARGO_PKG_VERSION")), true)
        .field("Runtime", runtime, true)
        .field("Bot User ID", bot_user_id.to_string(), true);

    ctx.send(CreateReply::default().embed(embed)).await?;
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
    ob_ctx.attachment_registry.clear_channel(channel_id);

    info!("Cleared chat context for channel {}", channel_id);

    ctx.say("info: チャットコンテキストをクリアしました。")
        .await?;

    Ok(())
}

/// to enable nelfie bot
#[poise::command(slash_command, prefix_command)]
pub async fn enable(
    ctx: Context<'_>,
    #[description = "Response mode (default: mention)"] mode: Option<EnableMode>,
) -> Result<(), Error> {
    let channel_id = ctx.channel_id();
    let ob_ctx = ctx.data();
    let mode = mode.unwrap_or(EnableMode::Mention);
    let response_mode = mode.response_mode();
    let mode_name = mode.as_str();

    if !mode.is_authorized_for(ctx.author().id.get(), &ob_ctx.config.admin_users) {
        ctx.say("エラー: /enable mode:auto-reply を実行する権限がありません。")
            .await?;
        return Ok(());
    }

    ob_ctx
        .chat_contexts
        .set_response_mode(channel_id, response_mode);
    ob_ctx.chat_contexts.get_or_create(channel_id);
    ctx.say(format!(
        "info: このチャンネルのチャットコンテキストを有効化しました。モード: `{}`",
        mode_name
    ))
    .await?;
    info!(
        "Enabled chat context for channel {} with mode {}",
        channel_id, mode_name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::EnableMode;

    #[test]
    fn only_admins_can_select_auto_reply_mode() {
        assert!(EnableMode::Mention.is_authorized_for(123, &[]));
        assert!(!EnableMode::AutoReplay.is_authorized_for(123, &[]));
        assert!(EnableMode::AutoReplay.is_authorized_for(123, &[123]));
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
#[poise::command(
    slash_command,
    prefix_command,
    subcommands("get", "set", "image_set", "list")
)]
pub async fn model(_: Context<'_>) -> Result<(), Error> {
    Ok(()) // ここはメインでは使わない
}

#[poise::command(slash_command, prefix_command)]
pub async fn get(ctx: Context<'_>) -> Result<(), Error> {
    let ob_ctx = ctx.data();
    let user_id = ctx.author().id;
    let settings = ob_ctx.user_contexts.get_or_create(user_id);
    ctx.say(format!(
        "Mainline: `{}` / cost **x{}**\nImage generation: `{}`",
        settings.main_model,
        settings.main_model.rate_cost(),
        settings.image_generation_model,
    ))
    .await?;
    Ok(())
}

#[poise::command(slash_command, prefix_command)]
pub async fn list(ctx: Context<'_>) -> Result<(), Error> {
    let models = Models::list();

    let mut s = String::from("**Mainline LLM models:**\n");
    for m in models {
        s.push_str(&format!("- `{}` / cost **x{}**\n", m, m.rate_cost()));
    }
    s.push_str("\n**Image generation models:**\n");
    for model in ImageGenerationModels::list() {
        s.push_str(&format!("- `{model}`\n"));
    }

    ctx.say(s).await?;
    Ok(())
}

#[poise::command(slash_command, prefix_command, rename = "image-set")]
pub async fn image_set(
    ctx: Context<'_>,
    #[description = "Choose an image generation model"]
    #[autocomplete = "autocomplete_image_model_name"]
    model_name: String,
) -> Result<(), Error> {
    let model = ImageGenerationModels::from_name(&model_name)
        .ok_or_else(|| format!("利用できない画像生成モデルです: `{model_name}`"))?;
    let ob_ctx = ctx.data();
    ob_ctx
        .user_contexts
        .set_image_generation_model_scoped(UserContextScope::User(ctx.author().id), model);
    ctx.say(format!("画像生成モデルを `{model}` に変更しました。"))
        .await?;
    Ok(())
}

async fn autocomplete_image_model_name(_ctx: Context<'_>, partial: &str) -> Vec<String> {
    ImageGenerationModels::list()
        .into_iter()
        .map(|model| model.to_string())
        .filter(|model| model.starts_with(partial))
        .collect()
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
    let model = Models::from_name(&model_name)
        .ok_or_else(|| format!("利用できないモデルです: `{model_name}`"))?;
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
#[poise::command(slash_command, prefix_command, rename = "tex")]
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

/// Render Typst source as a PDF.
#[poise::command(slash_command, rename = "typst")]
pub async fn typst_pdf(
    ctx: Context<'_>,
    #[description = "Typst source text"] source: Option<String>,
    #[description = "Upload a .typ source file"] source_file: Option<Attachment>,
    #[description = "Upload one image, font, or project asset"] asset_file: Option<Attachment>,
    #[description = "Comma-separated Discord attachment IDs for images, fonts, or source assets"]
    asset_ids: Option<String>,
    #[description = "Output format: png (default), svg, or pdf"] format: Option<String>,
) -> Result<(), Error> {
    if source.is_some() == source_file.is_some() {
        ctx.say("`source` または `source_file` のどちらか一つを指定してください。")
            .await?;
        return Ok(());
    }
    let asset_ids = match parse_attachment_ids(asset_ids.as_deref()) {
        Ok(asset_ids) => asset_ids,
        Err(error) => {
            ctx.say(preview_text(&error, 1500)).await?;
            return Ok(());
        }
    };
    let format = match format
        .as_deref()
        .map(crate::typst_render::TypstOutputFormat::parse)
        .transpose()
    {
        Ok(format) => format.unwrap_or_default(),
        Err(error) => {
            ctx.say(preview_text(&error, 1500)).await?;
            return Ok(());
        }
    };
    ctx.defer().await?;

    let (source_filename, source) = if let Some(source) = source {
        if source.trim().is_empty() {
            ctx.send(CreateReply::default().content("Typstソースが空です。"))
                .await?;
            return Ok(());
        }
        ("main.typ".to_string(), source)
    } else {
        let attachment = source_file.expect("validated source attachment");
        if u64::from(attachment.size) > 25 * 1024 * 1024 {
            ctx.send(CreateReply::default().content("ソースファイルは25 MiBまでです。"))
                .await?;
            return Ok(());
        }
        let bytes = match crate::discord::attachments::download_attachment_url(
            &attachment.url,
            25 * 1024 * 1024,
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(error) => {
                ctx.send(CreateReply::default().content(preview_text(&error, 1500)))
                    .await?;
                return Ok(());
            }
        };
        match crate::typst_render::source_from_asset(crate::typst_render::TypstAsset {
            filename: attachment.filename,
            content_type: attachment.content_type,
            bytes,
        }) {
            Ok(source) => source,
            Err(error) => {
                ctx.send(CreateReply::default().content(preview_text(&error, 1500)))
                    .await?;
                return Ok(());
            }
        }
    };

    let mut assets =
        match crate::typst_render::load_registry_assets(ctx.data(), &asset_ids, None).await {
            Ok(assets) => assets,
            Err(error) => {
                ctx.send(CreateReply::default().content(preview_text(&error, 1500)))
                    .await?;
                return Ok(());
            }
        };
    if let Some(attachment) = asset_file {
        if u64::from(attachment.size) > 25 * 1024 * 1024 {
            ctx.send(CreateReply::default().content("素材ファイルは25 MiBまでです。"))
                .await?;
            return Ok(());
        }
        let bytes = match crate::discord::attachments::download_attachment_url(
            &attachment.url,
            25 * 1024 * 1024,
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(error) => {
                ctx.send(CreateReply::default().content(preview_text(&error, 1500)))
                    .await?;
                return Ok(());
            }
        };
        assets.push(crate::typst_render::TypstAsset {
            filename: attachment.filename,
            content_type: attachment.content_type,
            bytes,
        });
    }
    let rendered = crate::typst_render::render(crate::typst_render::TypstRenderInput {
        source_filename,
        source,
        assets,
        format,
    })
    .await;
    match rendered {
        Ok(output) => match output.content {
            crate::typst_render::TypstRenderContent::Pdf(pdf) => {
                let reply = CreateReply::default()
                    .content(if output.warnings.is_empty() {
                        "Typst PDFを生成しました。".to_string()
                    } else {
                        preview_text(
                            &format!("Typst PDFを生成しました。\n{}", output.warnings.join("\n")),
                            1800,
                        )
                    })
                    .attachment(CreateAttachment::bytes(pdf, "document.pdf"));
                ctx.send(reply).await?;
            }
            crate::typst_render::TypstRenderContent::Pages(pages) => {
                let page_count = pages.len();
                crate::discord::message_delivery::send_typst_pages(
                    ctx.http(),
                    ctx.channel_id(),
                    format.extension(),
                    pages,
                )
                .await?;
                let summary = if output.warnings.is_empty() {
                    format!(
                        "Typst {0} を {page_count} ページ送信しました。",
                        format.extension()
                    )
                } else {
                    preview_text(
                        &format!(
                            "Typst {} を {page_count} ページ送信しました。\n{}",
                            format.extension(),
                            output.warnings.join("\n")
                        ),
                        1800,
                    )
                };
                ctx.say(summary).await?;
            }
        },
        Err(error) => {
            ctx.send(CreateReply::default().content(preview_text(&error, 1800)))
                .await?;
        }
    }
    Ok(())
}

fn parse_attachment_ids(value: Option<&str>) -> Result<Vec<u64>, String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|error| format!("添付ID `{value}` が不正です: {error}"))
        })
        .collect()
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
