use serenity::all::CreateEmbed;

use crate::llm::channel::VOICE_DICTIONARY_MAX_ENTRIES;

use super::super::shared::{Context, Error, preview_text};
use super::support::{require_vc_guild, send_vc_embed, send_vc_error, speak_vc_system_message};

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
#[poise::command(slash_command, prefix_command)]
pub async fn vc_dict(
    ctx: Context<'_>,
    #[description = "変換前の語句"] source: String,
    #[description = "読み上げ時の置換語句"] target: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_dict").await? else {
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

/// このテキストチャンネルの読み上げ辞書エントリを削除します
#[poise::command(slash_command, prefix_command)]
pub async fn vc_dict_delete(
    ctx: Context<'_>,
    #[description = "削除する変換前の語句"]
    #[autocomplete = "autocomplete_vc_dict_source"]
    source: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_dict_delete").await? else {
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
#[poise::command(slash_command, prefix_command)]
pub async fn vc_dict_user(
    ctx: Context<'_>,
    #[description = "変換前の語句"] source: String,
    #[description = "読み上げ時の置換語句"] target: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_dict_user").await? else {
        return Ok(());
    };

    let user_id = ctx.author().id;
    let (count, updated) = match ctx.data().user_contexts.set_voice_dictionary_entry(
        user_id,
        source.clone(),
        target.clone(),
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

/// ユーザーごとの読み上げ辞書エントリを削除します
#[poise::command(slash_command, prefix_command)]
pub async fn vc_dict_user_delete(
    ctx: Context<'_>,
    #[description = "削除する変換前の語句"]
    #[autocomplete = "autocomplete_vc_dict_user_source"]
    source: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_dict_user_delete").await? else {
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
