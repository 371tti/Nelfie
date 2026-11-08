use std::{
    collections::{HashMap, VecDeque},
    sync::atomic::{AtomicU64, Ordering},
};

use dashmap::DashMap;
use serenity::all::{ChannelId, Message, MessageId};

const MAX_REFERENCES_PER_CHANNEL: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentRef {
    pub channel_id: u64,
    pub message_id: Option<u64>,
    pub attachment_id: u64,
    pub filename: String,
    pub content_type: Option<String>,
    pub size: Option<u64>,
    pub source_url: Option<String>,
}

impl AttachmentRef {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "channel_id": self.channel_id.to_string(),
            "message_id": self.message_id.map(|id| id.to_string()),
            "attachment_id": self.attachment_id.to_string(),
            "filename": self.filename,
            "content_type": self.content_type,
            "size": self.size,
            "source": if self.source_url.is_some() { "profile_image" } else { "message_attachment" },
        })
    }
}

pub struct AttachmentRegistry {
    by_channel: DashMap<ChannelId, VecDeque<AttachmentRef>>,
    next_profile_image_id: AtomicU64,
}

impl Default for AttachmentRegistry {
    fn default() -> Self {
        Self {
            by_channel: DashMap::new(),
            next_profile_image_id: AtomicU64::new(u64::MAX),
        }
    }
}

impl AttachmentRegistry {
    pub fn observe_message(&self, message: &Message) {
        if message.attachments.is_empty() {
            return;
        }

        let channel_id = message.channel_id;
        let channel_value = channel_id.get();
        let message_value = message.id.get();
        let mut refs = self.by_channel.entry(channel_id).or_default();

        for attachment in &message.attachments {
            refs.retain(|existing| {
                existing.message_id != Some(message_value)
                    || existing.attachment_id != attachment.id.get()
            });
            refs.push_back(AttachmentRef {
                channel_id: channel_value,
                message_id: Some(message_value),
                attachment_id: attachment.id.get(),
                filename: attachment.filename.clone(),
                content_type: attachment.content_type.clone(),
                size: Some(u64::from(attachment.size)),
                source_url: None,
            });
            while refs.len() > MAX_REFERENCES_PER_CHANNEL {
                refs.pop_front();
            }
        }
    }

    pub fn register_profile_image(
        &self,
        channel_id: ChannelId,
        filename: String,
        content_type: String,
        url: String,
    ) -> Result<AttachmentRef, String> {
        validate_discord_cdn_url(&url)?;

        if let Some(existing) = self.by_channel.get(&channel_id).and_then(|refs| {
            refs.iter()
                .find(|reference| {
                    reference.source_url.as_deref() == Some(url.as_str())
                        && reference.filename == filename
                })
                .cloned()
        }) {
            return Ok(existing);
        }

        let attachment_id = loop {
            let candidate = self
                .next_profile_image_id
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                    (next > i64::MAX as u64).then(|| next - 1)
                })
                .map_err(|_| "プロフィール画像IDを発行できません。".to_string())?;
            if self.find(candidate, None).is_none() {
                break candidate;
            }
        };
        let reference = AttachmentRef {
            channel_id: channel_id.get(),
            message_id: None,
            attachment_id,
            filename,
            content_type: Some(content_type),
            size: None,
            source_url: Some(url),
        };
        let mut refs = self.by_channel.entry(channel_id).or_default();
        refs.push_back(reference.clone());
        while refs.len() > MAX_REFERENCES_PER_CHANNEL {
            refs.pop_front();
        }
        Ok(reference)
    }

    pub fn list(&self, channel_id: ChannelId) -> Vec<AttachmentRef> {
        self.by_channel
            .get(&channel_id)
            .map(|refs| refs.iter().rev().cloned().collect())
            .unwrap_or_default()
    }

    pub fn list_all(&self) -> Vec<AttachmentRef> {
        let mut refs = Vec::new();
        for entry in self.by_channel.iter() {
            refs.extend(entry.value().iter().cloned());
        }
        refs.sort_by_key(|item| {
            (
                item.channel_id,
                item.message_id.unwrap_or_default(),
                item.attachment_id,
            )
        });
        refs.reverse();
        refs
    }

    pub fn find(&self, attachment_id: u64, channel_id: Option<ChannelId>) -> Option<AttachmentRef> {
        if let Some(channel_id) = channel_id {
            return self
                .by_channel
                .get(&channel_id)?
                .iter()
                .find(|item| item.attachment_id == attachment_id)
                .cloned();
        }

        self.by_channel.iter().find_map(|entry| {
            entry
                .value()
                .iter()
                .find(|item| item.attachment_id == attachment_id)
                .cloned()
        })
    }

    pub fn find_many(
        &self,
        attachment_ids: &[u64],
        channel_id: Option<ChannelId>,
    ) -> Result<Vec<AttachmentRef>, String> {
        let requested = attachment_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let available = if let Some(channel_id) = channel_id {
            self.list(channel_id)
        } else {
            self.list_all()
        };
        let mut found = HashMap::new();
        for item in available {
            if requested.contains(&item.attachment_id) {
                found.entry(item.attachment_id).or_insert(item);
            }
        }

        attachment_ids
            .iter()
            .map(|id| {
                found
                    .get(id)
                    .cloned()
                    .ok_or_else(|| format!("添付ID `{id}` は保持中の参照にありません。"))
            })
            .collect()
    }

    pub fn clear_channel(&self, channel_id: ChannelId) {
        self.by_channel.remove(&channel_id);
    }

    pub async fn fetch_message_attachment(
        &self,
        http: &serenity::http::Http,
        reference: &AttachmentRef,
    ) -> Result<(String, Option<String>, Vec<u8>), String> {
        const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
        let attachment = self.latest_attachment(http, reference).await?;
        if reference
            .size
            .is_some_and(|size| size > MAX_ATTACHMENT_BYTES as u64)
            || attachment.2 > MAX_ATTACHMENT_BYTES as u64
        {
            return Err(format!(
                "`{}` は25 MiBを超えているため取得できません。",
                attachment.0
            ));
        }
        let bytes = download_attachment_url(&attachment.3, MAX_ATTACHMENT_BYTES).await?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err("添付ファイルは25 MiBを超えています。".to_string());
        }
        Ok((attachment.0, attachment.1, bytes))
    }

    pub async fn latest_attachment(
        &self,
        http: &serenity::http::Http,
        reference: &AttachmentRef,
    ) -> Result<(String, Option<String>, u64, String), String> {
        if let Some(url) = &reference.source_url {
            validate_discord_cdn_url(url)?;
            return Ok((
                reference.filename.clone(),
                reference.content_type.clone(),
                reference.size.unwrap_or_default(),
                url.clone(),
            ));
        }

        let message_id = reference.message_id.ok_or_else(|| {
            format!("添付 `{}` の取得元がありません。", reference.attachment_id)
        })?;
        let channel = ChannelId::new(reference.channel_id);
        let message = channel
            .message(http, MessageId::new(message_id))
            .await
            .map_err(|error| {
                format!(
                    "チャンネル `{}` のメッセージ `{}` を取得できません（Botの権限不足、または削除済み）。: {error}",
                    reference.channel_id, message_id
                )
            })?;
        let attachment = message
            .attachments
            .iter()
            .find(|attachment| attachment.id.get() == reference.attachment_id)
            .ok_or_else(|| {
                format!(
                    "添付 `{}` はメッセージ `{}` から削除されています。",
                    reference.attachment_id, message_id
                )
            })?;
        Ok((
            attachment.filename.clone(),
            attachment.content_type.clone(),
            u64::from(attachment.size),
            attachment.url.clone(),
        ))
    }
}

fn validate_discord_cdn_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "Discord添付URLが無効です。".to_string())?;
    if parsed.scheme() != "https"
        || parsed.port().is_some()
        || parsed.username() != ""
        || parsed.password().is_some()
        || !matches!(
            parsed.host_str(),
            Some("cdn.discordapp.com" | "media.discordapp.net")
        )
    {
        return Err("Discord添付以外のURLは取得できません。".to_string());
    }
    Ok(parsed)
}

pub async fn download_attachment_url(url: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    let parsed = validate_discord_cdn_url(url)?;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|error| format!("添付取得クライアントを初期化できません: {error}"))?;
    let mut response = client
        .get(parsed)
        .send()
        .await
        .map_err(|error| format!("Discordから添付を取得できません: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Discordから添付を取得できませんでした（HTTP {}）。",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(format!("添付ファイルは{} bytesを超えています。", max_bytes));
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("添付の読み込みに失敗しました: {error}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!("添付ファイルは{} bytesを超えています。", max_bytes));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_images_are_reusable_channel_assets() {
        let registry = AttachmentRegistry::default();
        let channel_id = ChannelId::new(123);
        let reference = registry
            .register_profile_image(
                channel_id,
                "user-42-avatar.webp".to_string(),
                "image/webp".to_string(),
                "https://cdn.discordapp.com/avatars/42/avatarhash.webp?size=1024".to_string(),
            )
            .unwrap();

        assert!(reference.attachment_id > i64::MAX as u64);
        assert!(reference.message_id.is_none());
        assert!(reference.size.is_none());
        assert_eq!(registry.find(reference.attachment_id, Some(channel_id)), Some(reference.clone()));
        assert_eq!(registry.find_many(&[reference.attachment_id], None).unwrap(), vec![reference.clone()]);
        assert_eq!(registry.list(channel_id)[0].to_json()["source"], "profile_image");

        registry.clear_channel(channel_id);
        assert!(registry.find(reference.attachment_id, None).is_none());
    }

    #[test]
    fn profile_image_assets_reject_non_discord_hosts() {
        let registry = AttachmentRegistry::default();
        assert!(registry
            .register_profile_image(
                ChannelId::new(123),
                "profile.png".to_string(),
                "image/png".to_string(),
                "https://example.com/profile.png".to_string(),
            )
            .is_err());
    }
}
