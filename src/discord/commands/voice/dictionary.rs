use serenity::all::{
    ButtonStyle, ComponentInteractionCollector, CreateActionRow, CreateButton, CreateEmbed,
    CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
    EditInteractionResponse, EditMessage, InteractionResponseFlags, MessageFlags,
};

use crate::llm::channel::VOICE_DICTIONARY_MAX_ENTRIES;
use crate::llm::channel::VoiceDictionaryEntry;

use super::super::shared::{Context, Error, preview_text};
use super::support::{require_vc_guild, send_vc_embed, send_vc_error, speak_vc_system_message};

const VOICE_DICTIONARY_PAGE_SIZE: usize = 10;
const VOICE_DICTIONARY_PAGE_TIMEOUT_SECS: u64 = 14 * 60;

async fn send_dictionary_list(
    ctx: &Context<'_>,
    mut entries: Vec<VoiceDictionaryEntry>,
    title: &str,
    empty_hint: &str,
    private: bool,
) -> Result<(), Error> {
    entries.sort_by(|left, right| left.source.cmp(&right.source));
    let page_count = entries.len().max(1).div_ceil(VOICE_DICTIONARY_PAGE_SIZE);
    let context_id = ctx.id();
    let previous_button_id = format!("voice-dictionary-{context_id}-previous");
    let next_button_id = format!("voice-dictionary-{context_id}-next");
    let initial_components =
        dictionary_page_buttons(&previous_button_id, &next_button_id, 1, page_count, false);
    let initial_embed = build_dictionary_page(title, empty_hint, &entries, 1, page_count);
    let message_id = match ctx {
        poise::Context::Application(app_ctx) => {
            let response_flags = if private {
                InteractionResponseFlags::EPHEMERAL
            } else {
                InteractionResponseFlags::SUPPRESS_NOTIFICATIONS
            };
            app_ctx
                .interaction
                .create_response(
                    app_ctx.serenity_context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .embed(initial_embed)
                            .components(initial_components)
                            .flags(response_flags),
                    ),
                )
                .await?;
            app_ctx
                .has_sent_initial_response
                .store(true, std::sync::atomic::Ordering::SeqCst);
            app_ctx
                .interaction
                .get_response(app_ctx.serenity_context.http.as_ref())
                .await?
                .id
        }
        poise::Context::Prefix(prefix_ctx) => {
            let message_flags = MessageFlags::SUPPRESS_NOTIFICATIONS;
            prefix_ctx
                .msg
                .channel_id
                .send_message(
                    prefix_ctx.serenity_context,
                    CreateMessage::new()
                        .embed(initial_embed)
                        .components(initial_components)
                        .flags(message_flags),
                )
                .await?
                .id
        }
    };

    if page_count == 1 {
        return Ok(());
    }

    let expires_at = tokio::time::Instant::now()
        + std::time::Duration::from_secs(VOICE_DICTIONARY_PAGE_TIMEOUT_SECS);
    let mut page = 1_usize;
    loop {
        let remaining = expires_at.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Some(interaction) = ComponentInteractionCollector::new(&ctx.serenity_context().shard)
            .message_id(message_id)
            .custom_ids(vec![previous_button_id.clone(), next_button_id.clone()])
            .timeout(remaining)
            .await
        else {
            break;
        };

        page = match interaction.data.custom_id.as_str() {
            id if id == previous_button_id.as_str() => page.saturating_sub(1).max(1),
            id if id == next_button_id.as_str() => (page + 1).min(page_count),
            _ => page,
        };
        interaction
            .create_response(
                ctx.serenity_context(),
                CreateInteractionResponse::UpdateMessage(
                    CreateInteractionResponseMessage::new()
                        .embed(build_dictionary_page(
                            title, empty_hint, &entries, page, page_count,
                        ))
                        .components(dictionary_page_buttons(
                            &previous_button_id,
                            &next_button_id,
                            page,
                            page_count,
                            false,
                        )),
                ),
            )
            .await?;
    }

    let disabled_components =
        dictionary_page_buttons(&previous_button_id, &next_button_id, page, page_count, true);
    match ctx {
        poise::Context::Application(app_ctx) => {
            app_ctx
                .interaction
                .edit_response(
                    app_ctx.serenity_context,
                    EditInteractionResponse::new().components(disabled_components),
                )
                .await?;
        }
        poise::Context::Prefix(prefix_ctx) => {
            prefix_ctx
                .msg
                .channel_id
                .edit_message(
                    prefix_ctx.serenity_context,
                    message_id,
                    EditMessage::new().components(disabled_components),
                )
                .await?;
        }
    }

    Ok(())
}

fn build_dictionary_page(
    title: &str,
    empty_hint: &str,
    entries: &[VoiceDictionaryEntry],
    page: usize,
    page_count: usize,
) -> CreateEmbed {
    let first_entry = (page - 1) * VOICE_DICTIONARY_PAGE_SIZE;
    let page_entries = entries
        .iter()
        .skip(first_entry)
        .take(VOICE_DICTIONARY_PAGE_SIZE)
        .collect::<Vec<_>>();
    let description = if entries.is_empty() {
        empty_hint.to_string()
    } else {
        format!(
            "登録数: {}/{VOICE_DICTIONARY_MAX_ENTRIES}　・　表示: {}〜{}件",
            entries.len(),
            first_entry + 1,
            first_entry + page_entries.len()
        )
    };
    let mut embed = CreateEmbed::new()
        .title(title)
        .description(description)
        .footer(CreateEmbedFooter::new(format!(
            "ページ {page}/{page_count}"
        )));

    for (index, entry) in page_entries.iter().enumerate() {
        embed = embed.field(
            format!(
                "{:02}　{}",
                first_entry + index + 1,
                preview_dictionary_value(&entry.source)
            ),
            format!(
                "置換後: {}\n{}",
                preview_dictionary_value(&entry.target),
                dictionary_attribution(entry)
            ),
            false,
        );
    }

    embed
}

fn dictionary_page_buttons(
    previous_button_id: &str,
    next_button_id: &str,
    page: usize,
    page_count: usize,
    disabled: bool,
) -> Vec<CreateActionRow> {
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new(previous_button_id)
            .label("◀ 前へ")
            .style(ButtonStyle::Secondary)
            .disabled(disabled || page == 1),
        CreateButton::new(next_button_id)
            .label("次へ ▶")
            .style(ButtonStyle::Secondary)
            .disabled(disabled || page == page_count),
    ])]
}

fn preview_dictionary_value(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_control() || character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let value = preview_text(value.trim(), 80);
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '~' | '|' | '>' | '[' | ']' | '(' | ')' | '<'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.replace('@', "@\u{200b}")
}

fn dictionary_attribution(entry: &VoiceDictionaryEntry) -> String {
    match (entry.registered_by, entry.updated_by) {
        (Some(registered_by), Some(updated_by)) if registered_by != updated_by => format!(
            "登録者 <@{}>・更新者 <@{}>",
            registered_by.get(),
            updated_by.get()
        ),
        (Some(registered_by), _) => format!("登録者 <@{}>", registered_by.get()),
        (None, Some(updated_by)) => {
            format!("登録者不明・更新者 <@{}>", updated_by.get())
        }
        (None, None) => "登録者不明".to_string(),
    }
}

fn autocomplete_dictionary_sources(entries: Vec<(String, String)>, partial: &str) -> Vec<String> {
    let partial = partial.trim().to_lowercase();

    let mut out = entries
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

fn build_vc_dictionary_embed(
    title: &str,
    description: impl Into<String>,
    scope_name: &str,
    scope_value: String,
    count: usize,
    source: &str,
    target: &str,
) -> CreateEmbed {
    CreateEmbed::new()
        .title(title)
        .description(description)
        .field(scope_name, scope_value, true)
        .field(
            "entry_count",
            format!("{count}/{VOICE_DICTIONARY_MAX_ENTRIES}"),
            true,
        )
        .field("source", preview_text(source.trim(), 120), false)
        .field("target", preview_text(target.trim(), 120), false)
}

/// このテキストチャンネルの読み上げ辞書を登録/更新します
#[poise::command(slash_command, prefix_command, rename = "dict")]
pub async fn vc_dict(
    ctx: Context<'_>,
    #[description = "変換前の語句"] source: String,
    #[description = "読み上げ時の置換語句"] target: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc dict").await? else {
        return Ok(());
    };

    let channel_id = ctx.channel_id();
    let (count, updated) = match ctx.data().chat_contexts.set_voice_dictionary_entry(
        channel_id,
        source.clone(),
        target.clone(),
        Some(ctx.author().id),
    ) {
        Ok(v) => v,
        Err(e) => {
            send_vc_error(&ctx, format!("辞書設定に失敗しました: {e}")).await?;
            return Ok(());
        }
    };

    let action = if updated { "更新" } else { "登録" };
    let embed = build_vc_dictionary_embed(
        "VC辞書設定",
        format!("読み上げ辞書を{}しました。", action),
        "channel",
        format!("<#{}>", channel_id.get()),
        count,
        &source,
        &target,
    );
    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(
        &ctx,
        guild_id,
        format!("読み上げ辞書を{}しました。", action),
    )
    .await;

    Ok(())
}

/// このテキストチャンネルの読み上げ辞書を一覧表示します
#[poise::command(slash_command, prefix_command, rename = "dict-list")]
pub async fn vc_dict_list(ctx: Context<'_>) -> Result<(), Error> {
    if require_vc_guild(&ctx, "/vc dict-list").await?.is_none() {
        return Ok(());
    }

    send_dictionary_list(
        &ctx,
        ctx.data()
            .chat_contexts
            .voice_dictionary_records(ctx.channel_id()),
        "読み上げ辞書一覧",
        "読み上げ辞書はまだ登録されていません。",
        false,
    )
    .await
}

/// このテキストチャンネルの読み上げ辞書エントリを削除します
#[poise::command(slash_command, prefix_command, rename = "dict-delete")]
pub async fn vc_dict_delete(
    ctx: Context<'_>,
    #[description = "削除する変換前の語句"]
    #[autocomplete = "autocomplete_vc_dict_source"]
    source: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc dict-delete").await? else {
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
            send_vc_error(&ctx, format!("辞書削除に失敗しました: {e}")).await?;
            return Ok(());
        }
    };

    let Some(removed_target) = removed_target else {
        send_vc_error(
            &ctx,
            format!(
                "指定した語句は辞書に存在しません: {}",
                preview_text(source.trim(), 120)
            ),
        )
        .await?;
        return Ok(());
    };

    let embed = build_vc_dictionary_embed(
        "VC辞書削除",
        "読み上げ辞書を削除しました。",
        "channel",
        format!("<#{}>", channel_id.get()),
        count,
        &source,
        &removed_target,
    );
    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(&ctx, guild_id, "読み上げ辞書を削除しました。").await;

    Ok(())
}

/// ユーザーごとの読み上げ辞書を登録/更新します
#[poise::command(slash_command, prefix_command, rename = "dict-user")]
pub async fn vc_dict_user(
    ctx: Context<'_>,
    #[description = "変換前の語句"] source: String,
    #[description = "読み上げ時の置換語句"] target: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc dict-user").await? else {
        return Ok(());
    };

    let user_id = ctx.author().id;
    let (count, updated) = match ctx.data().user_contexts.set_voice_dictionary_entry(
        user_id,
        source.clone(),
        target.clone(),
        Some(user_id),
    ) {
        Ok(v) => v,
        Err(e) => {
            send_vc_error(&ctx, format!("辞書設定に失敗しました: {e}")).await?;
            return Ok(());
        }
    };

    let action = if updated { "更新" } else { "登録" };
    let embed = build_vc_dictionary_embed(
        "VCユーザー辞書設定",
        format!("ユーザー辞書を{}しました。", action),
        "user",
        format!("<@{}>", user_id.get()),
        count,
        &source,
        &target,
    );
    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(
        &ctx,
        guild_id,
        format!("ユーザー辞書を{}しました。", action),
    )
    .await;

    Ok(())
}

/// 自分用の読み上げ辞書を一覧表示します
#[poise::command(slash_command, prefix_command, rename = "dict-user-list")]
pub async fn vc_dict_user_list(ctx: Context<'_>) -> Result<(), Error> {
    if require_vc_guild(&ctx, "/vc dict-user-list")
        .await?
        .is_none()
    {
        return Ok(());
    }

    let user_id = ctx.author().id;
    send_dictionary_list(
        &ctx,
        ctx.data().user_contexts.voice_dictionary_records(user_id),
        "自分の読み上げ辞書一覧",
        "自分用の読み上げ辞書はまだ登録されていません。",
        true,
    )
    .await
}

/// ユーザーごとの読み上げ辞書エントリを削除します
#[poise::command(slash_command, prefix_command, rename = "dict-user-delete")]
pub async fn vc_dict_user_delete(
    ctx: Context<'_>,
    #[description = "削除する変換前の語句"]
    #[autocomplete = "autocomplete_vc_dict_user_source"]
    source: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc dict-user-delete").await? else {
        return Ok(());
    };

    let user_id = ctx.author().id;
    let (count, removed_target) = match ctx
        .data()
        .user_contexts
        .remove_voice_dictionary_entry(user_id, &source)
    {
        Ok(v) => v,
        Err(e) => {
            send_vc_error(&ctx, format!("辞書削除に失敗しました: {e}")).await?;
            return Ok(());
        }
    };

    let Some(removed_target) = removed_target else {
        send_vc_error(
            &ctx,
            format!(
                "指定した語句は辞書に存在しません: {}",
                preview_text(source.trim(), 120)
            ),
        )
        .await?;
        return Ok(());
    };

    let embed = build_vc_dictionary_embed(
        "VCユーザー辞書削除",
        "ユーザー辞書を削除しました。",
        "user",
        format!("<@{}>", user_id.get()),
        count,
        &source,
        &removed_target,
    );
    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(&ctx, guild_id, "ユーザー辞書を削除しました。").await;

    Ok(())
}

async fn autocomplete_vc_dict_source(ctx: Context<'_>, partial: &str) -> Vec<String> {
    autocomplete_dictionary_sources(
        ctx.data()
            .chat_contexts
            .voice_dictionary_entries(ctx.channel_id()),
        partial,
    )
}

async fn autocomplete_vc_dict_user_source(ctx: Context<'_>, partial: &str) -> Vec<String> {
    autocomplete_dictionary_sources(
        ctx.data()
            .user_contexts
            .voice_dictionary_entries(ctx.author().id),
        partial,
    )
}
