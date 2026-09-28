use poise::CreateReply;
use serenity::all::{AutocompleteChoice, CreateEmbed};

use crate::voice::{
    MusicSystem,
    music::{MAX_PLAYLIST_TRACKS, MusicQueueSnapshot},
};

use super::super::shared::{Context, Error};
use super::support::{find_author_voice_channel, require_vc_guild, send_vc_error};

/// YouTube音楽の再生とキュー、コントロールパネルを操作します。
#[poise::command(
    slash_command,
    prefix_command,
    rename = "play",
    subcommands(
        "vc_play_url",
        "vc_play_open",
        "vc_play_close",
        "vc_play_queue",
        "vc_play_pop",
        "vc_play_push_front",
        "vc_play_push_back",
        "vc_play_del",
        "vc_play_clear",
        "vc_play_shuffle",
        "vc_play_volume",
        "vc_play_previous",
        "vc_play_next",
        "vc_play_toggle",
        "vc_play_stop"
    )
)]
pub async fn vc_play(_: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// YouTubeの曲またはプレイリストをキューに追加します。再生中の曲が終わると順番に再生します。
#[poise::command(slash_command, prefix_command, rename = "url")]
pub async fn vc_play_url(
    ctx: Context<'_>,
    #[description = "YouTubeまたはYouTube Musicの曲・プレイリストURL"] url: String,
) -> Result<(), Error> {
    enqueue_for_ctx(ctx, url, false).await
}

/// 再生コントロールパネルをこのチャンネルに表示します。
#[poise::command(slash_command, prefix_command, rename = "open")]
pub async fn vc_play_open(ctx: Context<'_>) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc play open").await? else {
        return Ok(());
    };
    ctx.data()
        .voice_system
        .music
        .set_panel_open(
            guild_id,
            ctx.channel_id(),
            ctx.serenity_context().http.clone(),
            true,
        )
        .await;
    ctx.say("再生コントロールパネルを開きました。").await?;
    Ok(())
}

/// 再生コントロールパネルを閉じます。
#[poise::command(slash_command, prefix_command, rename = "close")]
pub async fn vc_play_close(ctx: Context<'_>) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc play close").await? else {
        return Ok(());
    };
    ctx.data()
        .voice_system
        .music
        .set_panel_open(
            guild_id,
            ctx.channel_id(),
            ctx.serenity_context().http.clone(),
            false,
        )
        .await;
    ctx.say("再生コントロールパネルを閉じました。").await?;
    Ok(())
}

/// 現在の曲と待機キューを表示します。
#[poise::command(slash_command, prefix_command, rename = "queue")]
pub async fn vc_play_queue(ctx: Context<'_>) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc play queue").await? else {
        return Ok(());
    };
    let snapshot = ctx.data().voice_system.music.snapshot(guild_id).await;
    let embed = queue_embed(&snapshot);
    ctx.send(CreateReply::default().embed(embed)).await?;
    Ok(())
}

/// 選んだ待機曲をキューの先頭へ移し、現在の曲の次に再生します。
#[poise::command(slash_command, prefix_command, rename = "pop")]
pub async fn vc_play_pop(
    ctx: Context<'_>,
    #[description = "キューから再生する曲"]
    #[autocomplete = "autocomplete_queued_music"]
    track: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc play pop").await? else {
        return Ok(());
    };
    let Some(track_id) = track_id(&track) else {
        send_vc_error(&ctx, "曲の選択が無効です。キューから曲を選択してください。").await?;
        return Ok(());
    };
    match ctx
        .data()
        .voice_system
        .music
        .pop_queued(guild_id, track_id)
        .await
    {
        Ok(track) => {
            ctx.say(format!("「{}」を次の曲に設定しました。", track.title))
                .await?;
        }
        Err(error) => {
            ctx.say(format!("VCエラー: {error}")).await?;
        }
    }
    Ok(())
}

/// 曲をキューの先頭へ追加します。
#[poise::command(slash_command, prefix_command, rename = "push_front")]
pub async fn vc_play_push_front(
    ctx: Context<'_>,
    #[description = "YouTubeまたはYouTube Musicの曲・プレイリストURL"] url: String,
) -> Result<(), Error> {
    enqueue_for_ctx(ctx, url, true).await
}

/// 曲をキューの末尾へ追加します。
#[poise::command(slash_command, prefix_command, rename = "push_back")]
pub async fn vc_play_push_back(
    ctx: Context<'_>,
    #[description = "YouTubeまたはYouTube Musicの曲・プレイリストURL"] url: String,
) -> Result<(), Error> {
    enqueue_for_ctx(ctx, url, false).await
}

/// 選んだ待機曲をキューから削除します。
#[poise::command(slash_command, prefix_command, rename = "del")]
pub async fn vc_play_del(
    ctx: Context<'_>,
    #[description = "キューから削除する曲"]
    #[autocomplete = "autocomplete_queued_music"]
    track: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc play del").await? else {
        return Ok(());
    };
    let Some(track_id) = track_id(&track) else {
        send_vc_error(&ctx, "曲の選択が無効です。キューから曲を選択してください。").await?;
        return Ok(());
    };
    match ctx
        .data()
        .voice_system
        .music
        .remove_queued(guild_id, track_id)
        .await
    {
        Ok(track) => {
            ctx.say(format!("「{}」をキューから削除しました。", track.title))
                .await?;
        }
        Err(error) => {
            ctx.say(format!("VCエラー: {error}")).await?;
        }
    }
    Ok(())
}

/// 現在の曲を残して、待機キューを空にします。
#[poise::command(slash_command, prefix_command, rename = "clear")]
pub async fn vc_play_clear(ctx: Context<'_>) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc play clear").await? else {
        return Ok(());
    };
    let count = ctx.data().voice_system.music.clear_queue(guild_id).await;
    ctx.say(format!("待機キューから{}曲を削除しました。", count))
        .await?;
    Ok(())
}

/// 待機キューの順番をシャッフルします。
#[poise::command(slash_command, prefix_command, rename = "shuffle")]
pub async fn vc_play_shuffle(ctx: Context<'_>) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc play shuffle").await? else {
        return Ok(());
    };
    let count = ctx.data().voice_system.music.shuffle(guild_id).await;
    ctx.say(format!("待機キューの{}曲をシャッフルしました。", count))
        .await?;
    Ok(())
}

/// 再生音量を設定します（0〜100）。
#[poise::command(slash_command, prefix_command, rename = "volume")]
pub async fn vc_play_volume(
    ctx: Context<'_>,
    #[description = "音量（0〜100）"] percent: u8,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc play volume").await? else {
        return Ok(());
    };
    match ctx
        .data()
        .voice_system
        .music
        .set_volume(guild_id, percent)
        .await
    {
        Ok(()) => {
            ctx.say(format!("再生音量を{}%に設定しました。", percent))
                .await?;
        }
        Err(error) => {
            send_vc_error(&ctx, error).await?;
        }
    }
    Ok(())
}

/// ひとつ前の曲を再生します。
#[poise::command(slash_command, prefix_command, rename = "previous")]
pub async fn vc_play_previous(ctx: Context<'_>) -> Result<(), Error> {
    control_for_ctx(ctx, "previous", "前の曲へ移動しました。").await
}

/// 次の曲へ移動します。
#[poise::command(slash_command, prefix_command, rename = "next")]
pub async fn vc_play_next(ctx: Context<'_>) -> Result<(), Error> {
    control_for_ctx(ctx, "next", "次の曲へ移動しました。").await
}

/// 再生と一時停止を切り替えます。
#[poise::command(slash_command, prefix_command, rename = "toggle")]
pub async fn vc_play_toggle(ctx: Context<'_>) -> Result<(), Error> {
    control_for_ctx(ctx, "toggle", "再生状態を切り替えました。").await
}

/// 再生を停止し、現在の曲をキューの先頭に残します。
#[poise::command(slash_command, prefix_command, rename = "stop")]
pub async fn vc_play_stop(ctx: Context<'_>) -> Result<(), Error> {
    control_for_ctx(
        ctx,
        "stop",
        "再生を停止しました。再生操作でキューの先頭から再開できます。",
    )
    .await
}

async fn enqueue_for_ctx(ctx: Context<'_>, url: String, push_front: bool) -> Result<(), Error> {
    let command_name = if push_front {
        "/vc play push_front"
    } else {
        "/vc play url"
    };
    let Some(guild_id) = require_vc_guild(&ctx, command_name).await? else {
        return Ok(());
    };
    if let Err(error) = MusicSystem::validate_url(&url) {
        send_vc_error(&ctx, error).await?;
        return Ok(());
    }
    let Some(voice_channel) = find_author_voice_channel(&ctx) else {
        send_vc_error(&ctx, "先にボイスチャンネルへ参加してください。").await?;
        return Ok(());
    };
    if matches!(ctx, poise::Context::Application(_)) {
        ctx.defer().await?;
    }
    let current_channel = ctx
        .data()
        .voice_system
        .current_voice_channel_raw(guild_id)
        .await;
    if current_channel != Some(voice_channel.get()) {
        if let Err(error) = ctx
            .data()
            .voice_system
            .join_voice(guild_id, voice_channel)
            .await
        {
            ctx.say(format!("VCエラー: {error}")).await?;
            return Ok(());
        }
    }
    let http = ctx.serenity_context().http.clone();
    let requested_by = ctx.author().display_name().to_string();
    match ctx
        .data()
        .voice_system
        .music
        .enqueue_url(
            guild_id,
            ctx.channel_id(),
            http,
            &url,
            &requested_by,
            push_front,
        )
        .await
    {
        Ok(tracks) if tracks.len() == 1 => {
            let position = if push_front {
                "キューの先頭"
            } else {
                "キュー"
            };
            ctx.say(format!(
                "「{}」を{}に追加しました。 (# {})",
                tracks[0].title, position, tracks[0].id
            ))
            .await?;
        }
        Ok(tracks) => {
            let position = if push_front {
                "キューの先頭"
            } else {
                "キュー"
            };
            ctx.say(format!(
                "プレイリストから{}曲を{}に追加しました（1回の追加上限は{}曲）。",
                tracks.len(),
                position,
                MAX_PLAYLIST_TRACKS
            ))
            .await?;
        }
        Err(error) => {
            ctx.say(format!("VCエラー: {error}")).await?;
        }
    }
    Ok(())
}

async fn control_for_ctx(ctx: Context<'_>, action: &str, success: &str) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "/vc play").await? else {
        return Ok(());
    };
    match ctx
        .data()
        .voice_system
        .music
        .control(guild_id, action)
        .await
    {
        Ok(()) => {
            ctx.say(success).await?;
        }
        Err(error) => send_vc_error(&ctx, error).await?,
    }
    Ok(())
}

async fn autocomplete_queued_music(ctx: Context<'_>, partial: &str) -> Vec<AutocompleteChoice> {
    let Some(guild_id) = ctx.guild_id() else {
        return Vec::new();
    };
    let snapshot = ctx.data().voice_system.music.snapshot(guild_id).await;
    snapshot
        .queued
        .iter()
        .filter(|track| track.title.to_lowercase().contains(&partial.to_lowercase()))
        .take(25)
        .map(|track| {
            AutocompleteChoice::new(
                format!(
                    "{} · {}",
                    track.title.chars().take(75).collect::<String>(),
                    format_duration(track.duration)
                ),
                track.id.to_string(),
            )
        })
        .collect()
}

fn queue_embed(snapshot: &MusicQueueSnapshot) -> CreateEmbed {
    let current = snapshot
        .current
        .as_ref()
        .map(|track| format!("**再生中:** {}", track.title))
        .unwrap_or_else(|| {
            if snapshot.stopped {
                "**停止中**".to_string()
            } else {
                "再生中の曲はありません。".to_string()
            }
        });
    let queued = if snapshot.queued.is_empty() {
        "キューは空です。".to_string()
    } else {
        snapshot
            .queued
            .iter()
            .take(8)
            .map(|track| {
                format!(
                    "`{}` · {} ({})",
                    track.id,
                    track.title.chars().take(65).collect::<String>(),
                    format_duration(track.duration)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    CreateEmbed::new()
        .title("🎵 再生キュー")
        .description(current)
        .field("待機曲", queued, false)
        .field("音量", format!("{}%", snapshot.volume_percent), true)
        .field(
            "状態",
            if snapshot.paused {
                "一時停止"
            } else if snapshot.stopped {
                "停止"
            } else {
                "再生"
            },
            true,
        )
}

fn track_id(value: &str) -> Option<u64> {
    value.parse().ok()
}

fn format_duration(duration: Option<std::time::Duration>) -> String {
    let Some(duration) = duration else {
        return "長さ不明".to_string();
    };
    let total = duration.as_secs();
    if total >= 3600 {
        format!("{}:{:02}:{:02}", total / 3600, total / 60 % 60, total % 60)
    } else {
        format!("{:02}:{:02}", total / 60, total % 60)
    }
}
