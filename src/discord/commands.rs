use crate::voice::{SpeakOptions, apply_tts_dictionary, voice_catalog};
use std::time::Instant;

use log::{error, info, warn};
use poise::CreateReply;
use serenity::all::{
    AutocompleteChoice, ChannelId, CreateAttachment, CreateEmbed, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, GuildId, InteractionResponseFlags,
    MessageFlags, ResolvedValue, User, UserId,
};

use crate::{
    app::config::Models,
    app::context::NelfieContext,
    llm::channel::{
        VOICE_DICTIONARY_MAX_ENTRIES, VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX,
    },
    llm::tools::latex::LatexExprRenderTool,
};

// エラー型 dyn
type Error = Box<dyn std::error::Error + Send + Sync>;

// 毎回書くのがだるいので type alias
type Context<'a> = poise::Context<'a, NelfieContext, Error>;

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
        ctx.say("完了: システムプロンプトをデフォルトに戻しました。")
            .await?;
    } else {
        ob_ctx
            .chat_contexts
            .set_system_prompt(channel_id, Some(system_prompt.clone()));
        ctx.say(format!(
            "完了: システムプロンプトを更新しました。\n```{}```",
            system_prompt
        ))
        .await?;
    }

    Ok(())
}

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
        ob_ctx.user_contexts.get_or_create(target_user_id).rate_line
            + cost * ob_ctx.config.rate_limit_sec_per_cost
    };

    ob_ctx
        .user_contexts
        .set_rate_line(target_user_id, new_rate_line);

    let reply = if new_rate_line == 0 {
        format!(
            "完了: ユーザー `{}` のレート制限を **unlimit** に設定しました。",
            target_user_id
                .to_user(ctx.http())
                .await
                .map(|u| u.display_name().to_string())
                .unwrap_or_else(|_| "Null".to_string())
        )
    } else {
        format!(
            "完了: ユーザー `{}` の rate_line を **{}** に設定しました。",
            target_user_id
                .to_user(ctx.http())
                .await
                .map(|u| u.display_name().to_string())
                .unwrap_or_else(|_| "Null".to_string()),
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

/// clear context
#[poise::command(slash_command, prefix_command)]
pub async fn clear(ctx: Context<'_>) -> Result<(), Error> {
    let channel_id = ctx.channel_id();

    let ob_ctx = ctx.data();

    ob_ctx.chat_contexts.clear(channel_id);

    info!("Cleared chat context for channel {}", channel_id);

    ctx.say("完了: チャットコンテキストをクリアしました。")
        .await?;

    Ok(())
}

/// to enable nelfie bot
#[poise::command(slash_command, prefix_command)]
pub async fn enable(ctx: Context<'_>) -> Result<(), Error> {
    let channel_id = ctx.channel_id();

    let ob_ctx = ctx.data();

    if ob_ctx.chat_contexts.is_enabled(channel_id) {
        ctx.say("情報: このチャンネルのチャットコンテキストは既に有効です。")
            .await?;
        Ok(())
    } else {
        ob_ctx.chat_contexts.set_enabled(channel_id, true);
        ob_ctx.chat_contexts.get_or_create(channel_id);
        ctx.say("完了: このチャンネルのチャットコンテキストを有効化しました。")
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
        ctx.say("情報: このチャンネルのチャットコンテキストは既に無効です。")
            .await?;
        Ok(())
    } else {
        ob_ctx.chat_contexts.set_enabled(channel_id, false);
        ctx.say("完了: このチャンネルのチャットコンテキストを無効化しました。")
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
    let model = ob_ctx
        .user_contexts
        .get_or_create(user_id)
        .main_model
        .clone();
    ctx.say(format!("現在のモデル: `{}`", model)).await?;
    Ok(())
}

#[poise::command(slash_command, prefix_command)]
pub async fn list(ctx: Context<'_>) -> Result<(), Error> {
    let models = Models::list();

    let mut s = String::from("**利用可能なモデル:**\n");
    for m in models {
        s.push_str(&format!("- `{}`\n", m));
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
    ob_ctx.user_contexts.set_model(user_id, model.clone());

    ctx.say(format!("完了: モデルを `{}` に変更しました。", model))
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
    let ob_ctx = ctx.data();

    // レンダリング実行（ヘッドレスブラウザ経由）
    let png_bytes = match LatexExprRenderTool::render(&expr, ob_ctx).await {
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

fn find_author_voice_channel(ctx: &Context<'_>) -> Option<ChannelId> {
    let guild = ctx.guild()?;
    guild
        .voice_states
        .get(&ctx.author().id)
        .and_then(|state| state.channel_id)
}

/// VCに接続します(VC関連の機能が有効になります)
#[poise::command(slash_command, prefix_command)]
pub async fn vc_join(
    ctx: Context<'_>,
    #[description = "Enable auto-read in this text channel (default: true)"] auto_read: Option<
        bool,
    >,
) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        send_vc_embed(
            &ctx,
            vc_error_embed("vc_join はサーバーチャンネル内でのみ使用できます。"),
        )
        .await?;
        return Ok(());
    };

    let Some(voice_channel) = find_author_voice_channel(&ctx) else {
        send_vc_embed(
            &ctx,
            vc_error_embed("先にボイスチャンネルへ参加してください。"),
        )
        .await?;
        return Ok(());
    };

    let ob_ctx = ctx.data();
    ob_ctx
        .voice_system
        .join_voice(guild_id, voice_channel)
        .await?;

    let auto_read = auto_read.unwrap_or(true);
    ob_ctx
        .chat_contexts
        .set_voice_auto_read(ctx.channel_id(), auto_read);
    ob_ctx
        .voice_system
        .set_auto_read(guild_id, auto_read, Some(ctx.channel_id()));

    let system_read = ob_ctx.chat_contexts.is_voice_system_read(ctx.channel_id());

    let embed = CreateEmbed::new()
        .title("VC接続")
        .description(format!("VC <#{}> に接続しました。", voice_channel.get()))
        .field(
            "auto_read",
            format!(
                "{}（対象テキストチャンネル: <#{}>）",
                auto_read,
                ctx.channel_id().get()
            ),
            false,
        )
        .field("system_read", system_read.to_string(), true)
        .field("TTS", "VOICEVOX", true)
        .field("VVM", "voicevox_vvm", false);

    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(
        &ctx,
        guild_id,
        format!(
            "ボイスチャンネルに接続しました。自動読み上げは{}です。",
            if auto_read { "有効" } else { "無効" }
        ),
    )
    .await;

    Ok(())
}

/// VCから切断します(VC関連の機能が無効になります)
#[poise::command(slash_command, prefix_command)]
pub async fn vc_leave(ctx: Context<'_>) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        send_vc_embed(
            &ctx,
            vc_error_embed("vc_leave はサーバーチャンネル内でのみ使用できます。"),
        )
        .await?;
        return Ok(());
    };

    let ob_ctx = ctx.data();
    let channel_id = ctx.channel_id();
    let system_read = ob_ctx.chat_contexts.is_voice_system_read(channel_id);

    if system_read {
        speak_vc_system_message(&ctx, guild_id, "ボイスチャンネルから切断します。").await;
    }

    ob_ctx.voice_system.leave_voice(guild_id).await?;
    ob_ctx.chat_contexts.set_voice_auto_read(channel_id, false);
    ob_ctx
        .voice_system
        .set_auto_read(guild_id, false, Some(channel_id));

    let embed = CreateEmbed::new()
        .title("VC切断")
        .description("VCから切断しました。")
        .field("auto_read", "false（このチャンネル）", false)
        .field("system_read", system_read.to_string(), true);

    send_vc_embed(&ctx, embed).await?;
    Ok(())
}

/// テキストをVCで読み上げます
#[poise::command(slash_command, prefix_command)]
pub async fn vc_say(
    ctx: Context<'_>,
    #[description = "Text to read in VC"]
    #[rest]
    text: String,
) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        send_vc_embed(
            &ctx,
            vc_error_embed("vc_say はサーバーチャンネル内でのみ使用できます。"),
        )
        .await?;
        return Ok(());
    };

    if text.trim().is_empty() {
        send_vc_embed(&ctx, vc_error_embed("読み上げるテキストが空です。")).await?;
        return Ok(());
    }

    let dictionary = ctx
        .data()
        .chat_contexts
        .voice_dictionary_entries(ctx.channel_id());
    let text = apply_tts_dictionary(&text, &dictionary);
    let preview = preview_text(&text, 120);
    let channel_id = ctx.channel_id();
    let parallel_count = ctx.data().chat_contexts.voice_parallel_count(channel_id);

    let speaker = ctx.data().user_contexts.get_or_create(ctx.author().id);

    if let Err(e) = ctx
        .data()
        .voice_system
        .speak(
            guild_id,
            text,
            SpeakOptions {
                speaker: speaker.voice_speaker,
                speed_scale: speaker.voice_speed_scale,
                pitch_scale: speaker.voice_pitch_scale,
                pan: speaker.voice_pan,
                channel_id,
                parallel_count,
            },
        )
        .await
    {
        send_vc_embed(
            &ctx,
            vc_error_embed(format!("読み上げキューへの追加に失敗しました: {e}")),
        )
        .await?;
        return Ok(());
    }

    let embed = CreateEmbed::new()
        .title("VC読み上げ")
        .description(if parallel_count > 1 {
            "読み上げジョブを追加しました（並列再生）。"
        } else {
            "読み上げキューに追加しました。"
        })
        .field("parallel_count", parallel_count.to_string(), true)
        .field("text", preview, false);
    send_vc_embed(&ctx, embed).await?;
    Ok(())
}

/// VC関連設定（システム読み上げ / 自動読み上げ / 並列数）を更新します
#[poise::command(slash_command, prefix_command)]
pub async fn vc_config(
    ctx: Context<'_>,
    #[description = "Enable system read for VC command and join/leave announcements (None: keep current)"]
    system_read: Option<bool>,
    #[description = "Enable auto-read for this text channel (None: keep current)"]
    auto_read: Option<bool>,
    #[description = "Read parallel count for this text channel (1..4, None: keep current)"]
    parallel_count: Option<u8>,
) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        send_vc_embed(
            &ctx,
            vc_error_embed("vc_config はサーバーチャンネル内でのみ使用できます。"),
        )
        .await?;
        return Ok(());
    };

    let channel_id = ctx.channel_id();
    let ob_ctx = ctx.data();
    let changed = system_read.is_some() || auto_read.is_some() || parallel_count.is_some();

    let current_system_read = ob_ctx.chat_contexts.is_voice_system_read(channel_id);
    let current_auto_read = ob_ctx.chat_contexts.is_voice_auto_read(channel_id);
    let next_system_read = system_read.unwrap_or(current_system_read);
    let next_auto_read = auto_read.unwrap_or(current_auto_read);

    if let Some(value) = auto_read {
        ob_ctx.chat_contexts.set_voice_auto_read(channel_id, value);
        ob_ctx
            .voice_system
            .set_auto_read(guild_id, value, Some(channel_id));
    }

    let parallel_count = match parallel_count {
        Some(value) => {
            let value = usize::from(value);
            if !(VOICE_PARALLEL_COUNT_DEFAULT..=VOICE_PARALLEL_COUNT_MAX).contains(&value) {
                send_vc_embed(
                    &ctx,
                    vc_error_embed(format!(
                        "parallel_count は {}〜{} の範囲で指定してください。",
                        VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX
                    )),
                )
                .await?;
                return Ok(());
            }

            ob_ctx
                .chat_contexts
                .set_voice_parallel_count(channel_id, value)
        }
        None => ob_ctx.chat_contexts.voice_parallel_count(channel_id),
    };
    ob_ctx
        .voice_system
        .set_channel_parallel_count(channel_id, parallel_count);

    if system_read.is_some() {
        ob_ctx
            .chat_contexts
            .set_voice_system_read(channel_id, next_system_read);
    }

    let system_read = next_system_read;
    let auto_read = next_auto_read;

    let queue_mode = build_vc_mode_label(
        parallel_count,
        ob_ctx.voice_system.sequential_queue_capacity(),
    );

    let embed = CreateEmbed::new()
        .title("VC設定")
        .description(if changed {
            "VC関連設定を更新しました。"
        } else {
            "現在のVC関連設定です。"
        })
        .field("channel", format!("<#{}>", channel_id.get()), true)
        .field("system_read", system_read.to_string(), true)
        .field("auto_read", auto_read.to_string(), true)
        .field(
            "parallel_count(this_channel)",
            parallel_count.to_string(),
            true,
        )
        .field(
            "parallel_count_range",
            format!(
                "{}..={}",
                VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX
            ),
            true,
        )
        .field("mode", queue_mode, false);
    send_vc_embed(&ctx, embed).await?;

    if system_read {
        speak_vc_system_message(
            &ctx,
            guild_id,
            build_vc_config_voice_message(changed, system_read, auto_read, parallel_count),
        )
        .await;
    }

    Ok(())
}

/// このテキストチャンネルでの自動読み上げを有効/無効にします
#[poise::command(slash_command, prefix_command)]
pub async fn vc_autoread(
    ctx: Context<'_>,
    #[description = "Enable auto-read for this text channel"] enabled: bool,
) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        send_vc_embed(
            &ctx,
            vc_error_embed("vc_autoread はサーバーチャンネル内でのみ使用できます。"),
        )
        .await?;
        return Ok(());
    };

    let channel_id = ctx.channel_id();
    let ob_ctx = ctx.data();

    ob_ctx
        .chat_contexts
        .set_voice_auto_read(channel_id, enabled);
    ob_ctx
        .voice_system
        .set_auto_read(guild_id, enabled, Some(channel_id));

    let system_read = ob_ctx.chat_contexts.is_voice_system_read(channel_id);

    let embed = CreateEmbed::new()
        .title("VC自動読み上げ設定")
        .description("このチャンネルの自動読み上げ設定を更新しました。")
        .field("channel", format!("<#{}>", channel_id.get()), true)
        .field("auto_read", enabled.to_string(), true)
        .field("system_read", system_read.to_string(), true);
    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(
        &ctx,
        guild_id,
        format!(
            "このチャンネルの自動読み上げを{}に設定しました。",
            if enabled { "有効" } else { "無効" }
        ),
    )
    .await;

    Ok(())
}

/// このテキストチャンネルの読み上げ辞書を登録/更新します
#[poise::command(slash_command, prefix_command)]
pub async fn vc_dict(
    ctx: Context<'_>,
    #[description = "変換前の語句"] source: String,
    #[description = "読み上げ時の置換語句"] target: String,
) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        send_vc_embed(
            &ctx,
            vc_error_embed("vc_dict はサーバーチャンネル内でのみ使用できます。"),
        )
        .await?;
        return Ok(());
    };

    let channel_id = ctx.channel_id();
    let (count, updated) = match ctx.data().chat_contexts.set_voice_dictionary_entry(
        channel_id,
        source.clone(),
        target.clone(),
    ) {
        Ok(v) => v,
        Err(e) => {
            send_vc_embed(&ctx, vc_error_embed(format!("辞書設定に失敗しました: {e}"))).await?;
            return Ok(());
        }
    };

    let action = if updated { "更新" } else { "登録" };
    let embed = CreateEmbed::new()
        .title("VC辞書設定")
        .description(format!("読み上げ辞書を{}しました。", action))
        .field("channel", format!("<#{}>", channel_id.get()), true)
        .field(
            "entry_count",
            format!("{count}/{VOICE_DICTIONARY_MAX_ENTRIES}"),
            true,
        )
        .field("source", preview_text(source.trim(), 120), false)
        .field("target", preview_text(target.trim(), 120), false);
    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(
        &ctx,
        guild_id,
        format!("読み上げ辞書を{}しました。", action),
    )
    .await;

    Ok(())
}

/// このテキストチャンネルの読み上げ辞書エントリを削除します
#[poise::command(slash_command, prefix_command)]
pub async fn vc_dict_delete(
    ctx: Context<'_>,
    #[description = "削除する変換前の語句"]
    #[autocomplete = "autocomplete_vc_dict_source"]
    source: String,
) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        send_vc_embed(
            &ctx,
            vc_error_embed("vc_dict_delete はサーバーチャンネル内でのみ使用できます。"),
        )
        .await?;
        return Ok(());
    };

    let channel_id = ctx.channel_id();
    let (count, removed_target) = match ctx
        .data()
        .chat_contexts
        .remove_voice_dictionary_entry(channel_id, &source)
    {
        Ok(v) => v,
        Err(e) => {
            send_vc_embed(&ctx, vc_error_embed(format!("辞書削除に失敗しました: {e}"))).await?;
            return Ok(());
        }
    };

    let Some(removed_target) = removed_target else {
        send_vc_embed(
            &ctx,
            vc_error_embed(format!(
                "指定した語句は辞書に存在しません: {}",
                preview_text(source.trim(), 120)
            )),
        )
        .await?;
        return Ok(());
    };

    let embed = CreateEmbed::new()
        .title("VC辞書削除")
        .description("読み上げ辞書を削除しました。")
        .field("channel", format!("<#{}>", channel_id.get()), true)
        .field(
            "entry_count",
            format!("{count}/{VOICE_DICTIONARY_MAX_ENTRIES}"),
            true,
        )
        .field("source", preview_text(source.trim(), 120), false)
        .field("target", preview_text(&removed_target, 120), false);
    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(&ctx, guild_id, "読み上げ辞書を削除しました。").await;

    Ok(())
}

async fn autocomplete_vc_dict_source(ctx: Context<'_>, partial: &str) -> Vec<String> {
    let partial = partial.trim().to_lowercase();

    let mut out = ctx
        .data()
        .chat_contexts
        .voice_dictionary_entries(ctx.channel_id())
        .into_iter()
        .map(|(source, _)| source)
        .filter(|source| {
            if partial.is_empty() {
                return true;
            }

            let source_lc = source.to_lowercase();
            source_lc.starts_with(&partial) || source_lc.contains(&partial)
        })
        .collect::<Vec<_>>();

    out.sort();
    out.dedup();
    out.truncate(25);
    out
}

/// VOICEVOXの話者とスタイルを設定します（ユーザーごと）
#[poise::command(slash_command, prefix_command)]
pub async fn vc_speaker(
    ctx: Context<'_>,
    #[description = "VOICEVOX話者名"]
    #[autocomplete = "autocomplete_vc_speaker"]
    speaker: String,
    #[description = "スタイル名"]
    #[autocomplete = "autocomplete_vc_style"]
    style: String,
    #[description = "話速 (0.5〜2.0, 省略時は現状維持)"] speed: Option<f32>,
    #[description = "音高 (-1.0〜1.0, 省略時は現状維持)"] pitch: Option<f32>,
    #[description = "左右pan (-1.0=左, 0.0=中央, 1.0=右, 省略時は現状維持)"] pan: Option<f32>,
) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        send_vc_embed(
            &ctx,
            vc_error_embed("vc_speaker はサーバーチャンネル内でのみ使用できます。"),
        )
        .await?;
        return Ok(());
    };

    let Some(style_id) = voice_catalog::find_style_id(&speaker, &style) else {
        let styles = voice_catalog::styles_for_speaker(&speaker);
        if styles.is_empty() {
            let speaker_preview = voice_catalog::speaker_names()
                .into_iter()
                .take(20)
                .collect::<Vec<_>>()
                .join(", ");
            send_vc_embed(
                &ctx,
                vc_error_embed(format!(
                    "話者 '{}' は見つかりません。候補（先頭20件）: {}",
                    speaker, speaker_preview
                )),
            )
            .await?;
            return Ok(());
        }

        send_vc_embed(
            &ctx,
            vc_error_embed(format!(
                "話者 '{}' にスタイル '{}' はありません。候補: {}",
                speaker,
                style,
                styles.join(", ")
            )),
        )
        .await?;
        return Ok(());
    };

    let speed = match speed {
        Some(v) if !(0.5..=2.0).contains(&v) => {
            send_vc_embed(
                &ctx,
                vc_error_embed("speed は 0.5〜2.0 の範囲で指定してください。"),
            )
            .await?;
            return Ok(());
        }
        Some(v) => Some(v),
        None => None,
    };

    let pitch = match pitch {
        Some(v) if !(-1.0..=1.0).contains(&v) => {
            send_vc_embed(
                &ctx,
                vc_error_embed("pitch は -1.0〜1.0 の範囲で指定してください。"),
            )
            .await?;
            return Ok(());
        }
        Some(v) => Some(v),
        None => None,
    };

    let pan = match pan {
        Some(v) if !(-1.0..=1.0).contains(&v) => {
            send_vc_embed(
                &ctx,
                vc_error_embed("pan は -1.0〜1.0 の範囲で指定してください。"),
            )
            .await?;
            return Ok(());
        }
        Some(v) => Some(v),
        None => None,
    };

    let user_id = ctx.author().id;
    ctx.data()
        .user_contexts
        .set_voice_speaker(user_id, Some(style_id));
    if let Some(speed) = speed {
        ctx.data()
            .user_contexts
            .set_voice_speed_scale(user_id, Some(speed));
    }
    if let Some(pitch) = pitch {
        ctx.data()
            .user_contexts
            .set_voice_pitch_scale(user_id, Some(pitch));
    }
    if let Some(pan) = pan {
        ctx.data().user_contexts.set_voice_pan(user_id, Some(pan));
    }

    let speed_text = speed
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "(unchanged)".to_string());
    let pitch_text = pitch
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "(unchanged)".to_string());
    let pan_text = pan
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "(unchanged)".to_string());

    let embed = CreateEmbed::new()
        .title("VC話者設定")
        .description("話者設定を更新しました。")
        .field(
            "speaker",
            format!("VOICEVOX:{} / {} (id={})", speaker, style, style_id),
            false,
        )
        .field("speed", speed_text, true)
        .field("pitch", pitch_text, true)
        .field("pan", pan_text, true);
    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(&ctx, guild_id, "話者設定を更新しました").await;

    Ok(())
}

async fn autocomplete_vc_speaker(_ctx: Context<'_>, partial: &str) -> Vec<String> {
    voice_catalog::suggest_speakers(partial, 25)
}

fn selected_vc_speaker_from_ctx(ctx: Context<'_>) -> Option<String> {
    let poise::Context::Application(app_ctx) = ctx else {
        return None;
    };

    app_ctx.args.iter().find_map(|option| {
        if option.name != "speaker" {
            return None;
        }

        match option.value {
            ResolvedValue::String(value) => Some(value.to_string()),
            ResolvedValue::Autocomplete { value, .. } => Some(value.to_string()),
            _ => None,
        }
    })
}

async fn autocomplete_vc_style(ctx: Context<'_>, partial: &str) -> Vec<AutocompleteChoice> {
    let speaker = selected_vc_speaker_from_ctx(ctx);
    voice_catalog::suggest_styles(partial, speaker.as_deref(), 25)
        .into_iter()
        .map(|entry| {
            AutocompleteChoice::new(
                format!(
                    "{} / {} (id={}, {})",
                    entry.speaker_name, entry.style_name, entry.style_id, entry.vvm_file
                ),
                entry.style_name,
            )
        })
        .collect::<Vec<_>>()
}

/// VOICEVOXの話者設定と状態を取得します（ユーザーごと）
#[poise::command(slash_command, prefix_command)]
pub async fn vc_status(ctx: Context<'_>) -> Result<(), Error> {
    let Some(guild_id) = ctx.guild_id() else {
        send_vc_embed(
            &ctx,
            vc_error_embed("vc_status はサーバーチャンネル内でのみ使用できます。"),
        )
        .await?;
        return Ok(());
    };

    let ob_ctx = ctx.data();
    let channel_id = ctx.channel_id();
    let guild_voice_cfg = ob_ctx.voice_system.config(guild_id);
    let parallel_count = ob_ctx.chat_contexts.voice_parallel_count(channel_id);
    let voice_channel = ob_ctx
        .voice_system
        .current_voice_channel_raw(guild_id)
        .await;
    let auto_read = ob_ctx.chat_contexts.is_voice_auto_read(channel_id);
    let system_read = ob_ctx.chat_contexts.is_voice_system_read(channel_id);
    let dict_entries = ob_ctx.chat_contexts.voice_dictionary_count(channel_id);
    let user_voice = ob_ctx.user_contexts.get_or_create(ctx.author().id);
    let user_speaker = user_voice.voice_speaker;
    let speaker_text = user_speaker
        .map(|id| {
            let speaker_name = voice_catalog::speaker_name_for_id(id)
                .unwrap_or_else(|| "(unknown speaker)".to_string());
            let style_name = voice_catalog::style_name_for_id(id)
                .unwrap_or_else(|| "(unknown style)".to_string());
            format!("{} / {} ({})", speaker_name, style_name, id)
        })
        .unwrap_or_else(|| format!("{} (guild default)", guild_voice_cfg.speaker));
    let speed_text = user_voice
        .voice_speed_scale
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "1.00 (default)".to_string());
    let pitch_text = user_voice
        .voice_pitch_scale
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "0.00 (default)".to_string());
    let pan_text = user_voice
        .voice_pan
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "0.00 (default)".to_string());

    let current_vc = voice_channel
        .map(|id| format!("<#{}>", id))
        .unwrap_or_else(|| "(not connected)".to_string());
    let last_error = ob_ctx
        .voice_system
        .last_error(guild_id)
        .unwrap_or_else(|| "(none)".to_string());

    let embed = CreateEmbed::new()
        .title("VCステータス")
        .field("connected", current_vc, true)
        .field("auto_read(this_channel)", auto_read.to_string(), true)
        .field("system_read(this_channel)", system_read.to_string(), true)
        .field(
            "parallel_count(this_channel)",
            parallel_count.to_string(),
            true,
        )
        .field(
            "parallel_count_range",
            format!(
                "{}..={}",
                VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX
            ),
            true,
        )
        .field(
            "mode(this_channel)",
            if parallel_count > 1 {
                format!("parallel(count={parallel_count})")
            } else {
                "sequential".to_string()
            },
            true,
        )
        .field(
            "sequential_queue_limit",
            ob_ctx.voice_system.sequential_queue_capacity().to_string(),
            true,
        )
        .field(
            "tts_dict_entries(this_channel)",
            format!("{dict_entries}/{VOICE_DICTIONARY_MAX_ENTRIES}"),
            true,
        )
        .field(
            "speaker(guild default)",
            guild_voice_cfg.speaker.to_string(),
            true,
        )
        .field("speaker(user)", speaker_text, false)
        .field(
            "speed/pitch/pan(user)",
            format!("{speed_text} / {pitch_text} / {pan_text}"),
            false,
        )
        .field("voicevox_core", ob_ctx.voice_system.core_summary(), false)
        .field(
            "acceleration/cpu_threads/load_all_models",
            format!(
                "{} / {} / {}",
                ob_ctx.config.voicevox_core_acceleration,
                ob_ctx.config.voicevox_core_cpu_threads,
                ob_ctx.config.voicevox_core_load_all_models
            ),
            false,
        )
        .field("last_error", last_error, false);
    send_vc_embed(&ctx, embed).await?;
    speak_vc_system_message(&ctx, guild_id, "VCステータスを表示しました").await;

    Ok(())
}

pub fn log_err(context: &str, err: &(dyn std::error::Error + Send + Sync)) {
    error!("[{context}] {err:#?}");

    let mut src = err.source();
    while let Some(s) = src {
        error!("  caused by: {s:?}");
        src = s.source();
    }
    error!("error trace end");
}

async fn send_vc_embed(ctx: &Context<'_>, embed: CreateEmbed) -> Result<(), Error> {
    match ctx {
        poise::Context::Application(app_ctx) => {
            app_ctx
                .interaction
                .create_response(
                    app_ctx.serenity_context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .embed(embed)
                            .flags(InteractionResponseFlags::SUPPRESS_NOTIFICATIONS),
                    ),
                )
                .await?;
            app_ctx
                .has_sent_initial_response
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        poise::Context::Prefix(prefix_ctx) => {
            prefix_ctx
                .msg
                .channel_id
                .send_message(
                    prefix_ctx.serenity_context,
                    CreateMessage::new()
                        .embed(embed)
                        .flags(MessageFlags::SUPPRESS_NOTIFICATIONS),
                )
                .await?;
        }
    }

    Ok(())
}

fn vc_error_embed(message: impl Into<String>) -> CreateEmbed {
    CreateEmbed::new().title("VCエラー").description(message)
}

fn preview_text(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }

    if out.is_empty() {
        "(empty)".to_string()
    } else {
        out
    }
}

async fn speak_vc_system_message(ctx: &Context<'_>, guild_id: GuildId, text: impl Into<String>) {
    let ob_ctx = ctx.data();
    let channel_id = ctx.channel_id();

    if !ob_ctx.chat_contexts.is_voice_system_read(channel_id) {
        return;
    }

    let dictionary = ob_ctx.chat_contexts.voice_dictionary_entries(channel_id);
    let text = apply_tts_dictionary(&text.into(), &dictionary);
    let user_voice = ob_ctx.user_contexts.get_or_create(ctx.author().id);
    let parallel_count = ob_ctx.chat_contexts.voice_parallel_count(channel_id);

    if let Err(e) = ob_ctx
        .voice_system
        .speak(
            guild_id,
            text,
            SpeakOptions {
                speaker: user_voice.voice_speaker,
                speed_scale: user_voice.voice_speed_scale,
                pitch_scale: user_voice.voice_pitch_scale,
                pan: user_voice.voice_pan,
                channel_id,
                parallel_count,
            },
        )
        .await
    {
        warn!("failed to enqueue VC system message: {}", e);
    }
}

fn bool_enabled_label(value: bool) -> &'static str {
    if value { "有効" } else { "無効" }
}

fn build_vc_mode_label(parallel_count: usize, sequential_capacity: usize) -> String {
    if parallel_count > 1 {
        format!("parallel(count={parallel_count})")
    } else {
        format!("sequential(queue <= {sequential_capacity})")
    }
}

fn build_vc_config_voice_message(
    is_updating: bool,
    system_read: bool,
    auto_read: bool,
    parallel_count: usize,
) -> String {
    let action = if is_updating { "VC設定を更新しました。" } else { "現在のVC設定です。" };
    format!(
        "{}システム読み上げは{}。自動読み上げは{}。読み上げモードは{}です。",
        action,
        bool_enabled_label(system_read),
        bool_enabled_label(auto_read),
        read_mode_label(parallel_count),
    )
}

fn read_mode_label(parallel_count: usize) -> String {
    if parallel_count > 1 {
        format!("並列 {}", parallel_count)
    } else {
        "逐次".to_string()
    }
}