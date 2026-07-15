use std::collections::HashMap;

use log::warn;
use serenity::all::{ChannelId, Message, MessageId};

use crate::llm::context::{DiscordImageSource, LMContext};

pub(super) struct MessageImageAttachment {
    filename: String,
    content_type: Option<String>,
    size: u32,
    pub(super) source: Option<DiscordImageSource>,
}

impl MessageImageAttachment {
    pub(super) fn to_context_json(&self) -> serde_json::Value {
        serde_json::json!({
            "filename": self.filename,
            "content_type": self.content_type,
            "size": self.size,
            "url": self.source.as_ref().map(|source| source.url.as_str()),
            "url_captured_at_unix": self.source.as_ref().map(|source| source.captured_at_unix),
            "url_expires_at_unix": self.source.as_ref().and_then(|source| discord_attachment_expiry_unix(&source.url)),
            "status": if self.source.is_some() { "url_stored_refreshable" } else { "unsupported" },
        })
    }
}

pub(super) fn message_image_attachments(msg: &Message) -> Vec<MessageImageAttachment> {
    let captured_at_unix = chrono::Utc::now().timestamp().max(0) as u64;

    msg.attachments
        .iter()
        .filter_map(|attachment| {
            let content_type = attachment
                .content_type
                .as_deref()
                .and_then(normalize_supported_image_content_type)
                .or_else(|| supported_image_content_type_from_filename(&attachment.filename));
            let source = content_type.and_then(|_| {
                if attachment.url.is_empty() {
                    None
                } else {
                    Some(DiscordImageSource {
                        channel_id: msg.channel_id.get(),
                        message_id: msg.id.get(),
                        attachment_id: attachment.id.get(),
                        captured_at_unix,
                        url: attachment.url.clone(),
                    })
                }
            });

            if content_type.is_none() && source.is_none() {
                return None;
            }

            Some(MessageImageAttachment {
                filename: attachment.filename.clone(),
                content_type: content_type.map(str::to_string),
                size: attachment.size,
                source,
            })
        })
        .collect()
}

fn normalize_supported_image_content_type(content_type: &str) -> Option<&'static str> {
    let media_type = content_type.split(';').next()?.trim();
    match media_type {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/webp" => Some("image/webp"),
        "image/gif" => Some("image/gif"),
        _ => None,
    }
}

fn supported_image_content_type_from_filename(filename: &str) -> Option<&'static str> {
    let extension = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

const DISCORD_ATTACHMENT_URL_EXPIRY_MARGIN_SECS: u64 = 10 * 60;
const DISCORD_ATTACHMENT_URL_FALLBACK_REFRESH_SECS: u64 = 60 * 60;

fn discord_attachment_expiry_unix(url: &str) -> Option<u64> {
    let url = reqwest::Url::parse(url).ok()?;
    for (key, value) in url.query_pairs() {
        if key == "ex" {
            return u64::from_str_radix(value.as_ref(), 16).ok();
        }
    }
    None
}

fn should_refresh_discord_image_source(source: &DiscordImageSource, now: u64) -> bool {
    match discord_attachment_expiry_unix(&source.url) {
        Some(expires_at) => {
            now >= expires_at.saturating_sub(DISCORD_ATTACHMENT_URL_EXPIRY_MARGIN_SECS)
        }
        None => {
            now.saturating_sub(source.captured_at_unix)
                >= DISCORD_ATTACHMENT_URL_FALLBACK_REFRESH_SECS
        }
    }
}

pub(super) async fn refresh_discord_image_urls_for_api(
    ctx: &serenity::client::Context,
    lm_context: &mut LMContext,
) {
    lm_context.prune_discord_image_sources_to_present_urls();

    let now = chrono::Utc::now().timestamp().max(0) as u64;
    let stale_sources = lm_context
        .discord_image_sources
        .iter()
        .filter(|source| should_refresh_discord_image_source(source, now))
        .cloned()
        .collect::<Vec<_>>();

    if stale_sources.is_empty() {
        return;
    }

    let mut fetched_messages = HashMap::<(u64, u64), Option<Message>>::new();

    for source in stale_sources {
        let key = (source.channel_id, source.message_id);
        if let std::collections::hash_map::Entry::Vacant(entry) = fetched_messages.entry(key) {
            let fetched = ChannelId::new(source.channel_id)
                .message(&ctx.http, MessageId::new(source.message_id))
                .await;
            match fetched {
                Ok(message) => {
                    entry.insert(Some(message));
                }
                Err(e) => {
                    warn!(
                        "failed to refresh Discord attachment URL from channel {} message {}: {}",
                        source.channel_id, source.message_id, e
                    );
                    entry.insert(None);
                }
            }
        }

        let Some(Some(message)) = fetched_messages.get(&key) else {
            lm_context.remove_image_url(&source.url);
            continue;
        };

        let Some(attachment) = message
            .attachments
            .iter()
            .find(|attachment| attachment.id.get() == source.attachment_id)
        else {
            warn!(
                "Discord attachment {} no longer exists on message {}",
                source.attachment_id, source.message_id
            );
            lm_context.remove_image_url(&source.url);
            continue;
        };

        if attachment.url.is_empty() {
            warn!(
                "Discord attachment {} on message {} has empty URL",
                source.attachment_id, source.message_id
            );
            lm_context.remove_image_url(&source.url);
            continue;
        }

        let old_url = source.url.clone();
        let new_url = attachment.url.clone();
        lm_context.replace_image_url(&old_url, &new_url);

        if let Some(current_source) = lm_context.discord_image_sources.iter_mut().find(|item| {
            item.channel_id == source.channel_id
                && item.message_id == source.message_id
                && item.attachment_id == source.attachment_id
        }) {
            current_source.url = new_url;
            current_source.captured_at_unix = now;
        }
    }

    lm_context.prune_discord_image_sources_to_present_urls();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lm_context_stores_refreshable_discord_image_sources() {
        let mut context = LMContext::new();
        context.add_user_text_with_discord_images(
            "image".to_string(),
            vec![DiscordImageSource {
                channel_id: 1,
                message_id: 2,
                attachment_id: 3,
                captured_at_unix: 4,
                url: "https://cdn.discordapp.example/image.png".to_string(),
            }],
        );

        assert_eq!(context.discord_image_sources.len(), 1);
        assert_eq!(context.discord_image_sources[0].attachment_id, 3);
    }

    #[test]
    fn content_type_normalization_accepts_supported_images() {
        assert_eq!(
            normalize_supported_image_content_type("image/png"),
            Some("image/png")
        );
        assert_eq!(
            normalize_supported_image_content_type("image/jpg; charset=binary"),
            Some("image/jpeg")
        );
        assert_eq!(
            supported_image_content_type_from_filename("PHOTO.WEBP"),
            Some("image/webp")
        );
        assert_eq!(supported_image_content_type_from_filename("note.txt"), None);
        assert_eq!(
            normalize_supported_image_content_type("image/svg+xml"),
            None
        );
    }

    #[test]
    fn signed_url_expiry_is_parsed_as_hex_unix_timestamp() {
        assert_eq!(
            discord_attachment_expiry_unix(
                "https://cdn.discordapp.com/attachments/1/2/image.png?ex=65d903de&is=65c68ede&hm=abc"
            ),
            Some(0x65d903de)
        );
    }

    #[test]
    fn signed_url_refreshes_at_ten_minute_margin() {
        let source = DiscordImageSource {
            channel_id: 1,
            message_id: 2,
            attachment_id: 3,
            captured_at_unix: 100,
            url: "https://cdn.discordapp.com/attachments/1/2/image.png?ex=1000&is=0100&hm=abc"
                .to_string(),
        };

        assert!(!should_refresh_discord_image_source(&source, 0x1000 - 601));
        assert!(should_refresh_discord_image_source(&source, 0x1000 - 600));
    }

    #[test]
    fn unsigned_url_uses_capture_time_fallback() {
        let source = DiscordImageSource {
            channel_id: 1,
            message_id: 2,
            attachment_id: 3,
            captured_at_unix: 100,
            url: "https://example.com/image.png".to_string(),
        };

        assert!(!should_refresh_discord_image_source(&source, 100 + 3_599));
        assert!(should_refresh_discord_image_source(&source, 100 + 3_600));
    }
}
