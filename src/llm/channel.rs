use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use dashmap::{DashMap, mapref::entry::Entry};
use log::{error, warn};
use serde_json::{Value, json};
use serenity::all::ChannelId;

use crate::llm::context::{LMContext, LMContextCompaction};

const CHAT_CONTEXTS_STORE_PATH: &str = "data/runtime/chat_contexts.json";
pub const VOICE_DICTIONARY_MAX_ENTRIES: usize = 512;
pub const VOICE_PARALLEL_COUNT_DEFAULT: usize = 1;
pub const VOICE_PARALLEL_COUNT_MAX: usize = 4;

/// チャンネルごとのプール
pub struct ChatContexts {
    pub contexts: DashMap<ChannelId, ChatContext>,
    pub default_system_prompt: String,
    compacting_contexts: DashMap<ChannelId, u64>,
    next_compaction_id: AtomicU64,
    store_path: PathBuf,
}

/// チャンネルごとのデータ保持
pub struct ChatContext {
    pub channel_id: ChannelId,
    pub context: LMContext,
    pub system_prompt: Option<String>,
    pub enabled: bool,
    pub voice_auto_read: bool,
    pub voice_system_read: bool,
    pub voice_parallel_count: usize,
    pub voice_dictionary: Vec<VoiceDictionaryEntry>,
}

#[derive(Clone, Debug)]
pub struct VoiceDictionaryEntry {
    pub source: String,
    pub target: String,
}

impl ChatContext {
    pub fn new(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            context: LMContext::new(),
            system_prompt: None,
            enabled: false,
            voice_auto_read: false,
            voice_system_read: true,
            voice_parallel_count: VOICE_PARALLEL_COUNT_DEFAULT,
            voice_dictionary: Vec::new(),
        }
    }

    fn should_persist(&self) -> bool {
        self.system_prompt.is_some()
            || self.enabled
            || self.voice_auto_read
            || !self.voice_system_read
            || self.voice_parallel_count != VOICE_PARALLEL_COUNT_DEFAULT
            || !self.voice_dictionary.is_empty()
    }
}

fn clamp_voice_parallel_count(value: usize) -> usize {
    value.clamp(VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX)
}

impl ChatContexts {
    pub fn new(default_system_prompt: String) -> ChatContexts {
        let mut chat_contexts = ChatContexts {
            contexts: DashMap::new(),
            default_system_prompt,
            compacting_contexts: DashMap::new(),
            next_compaction_id: AtomicU64::new(1),
            store_path: PathBuf::from(CHAT_CONTEXTS_STORE_PATH),
        };
        chat_contexts.load_from_disk();
        chat_contexts
    }

    pub fn get_or_create(&self, channel_id: ChannelId) -> LMContext {
        self.contexts
            .entry(channel_id)
            .or_insert_with(|| ChatContext::new(channel_id))
            .context
            .clone()
    }

    pub fn get_system_prompt(&self, channel_id: ChannelId) -> String {
        self.contexts
            .get(&channel_id)
            .and_then(|entry| entry.system_prompt.clone())
            .unwrap_or_else(|| self.default_system_prompt.clone())
    }

    pub fn set_system_prompt(&self, channel_id: ChannelId, system_prompt: Option<String>) {
        self.update_stored(channel_id, |entry| entry.system_prompt = system_prompt);
    }

    pub fn marge(&self, channel_id: ChannelId, other: &LMContext) {
        self.contexts
            .entry(channel_id)
            .or_insert_with(|| ChatContext::new(channel_id))
            .context
            .extend(other);
    }

    pub fn begin_compaction(
        &self,
        channel_id: ChannelId,
        token_limit: u32,
    ) -> Option<(u64, LMContextCompaction)> {
        let compaction_id = self.next_compaction_id.fetch_add(1, Ordering::Relaxed);
        match self.compacting_contexts.entry(channel_id) {
            Entry::Occupied(_) => return None,
            Entry::Vacant(entry) => {
                entry.insert(compaction_id);
            }
        }

        let compaction = self
            .contexts
            .get(&channel_id)
            .and_then(|entry| entry.context.plan_compaction(token_limit));
        if compaction.is_none() {
            self.compacting_contexts
                .remove_if(&channel_id, |_, active_id| *active_id == compaction_id);
        }
        compaction.map(|compaction| (compaction_id, compaction))
    }

    pub fn complete_compaction(
        &self,
        channel_id: ChannelId,
        compaction_id: u64,
        compaction: &LMContextCompaction,
        summary: &str,
    ) -> bool {
        let Some(active_compaction) = self.compacting_contexts.get(&channel_id) else {
            return false;
        };
        if *active_compaction != compaction_id {
            return false;
        }

        let applied = self
            .contexts
            .get_mut(&channel_id)
            .is_some_and(|mut entry| entry.context.apply_compaction(compaction, summary));
        drop(active_compaction);
        self.compacting_contexts
            .remove_if(&channel_id, |_, active_id| *active_id == compaction_id);
        applied
    }

    pub fn cancel_compaction(&self, channel_id: ChannelId, compaction_id: u64) {
        self.compacting_contexts
            .remove_if(&channel_id, |_, active_id| *active_id == compaction_id);
    }

    pub fn get_mut(&self, channel_id: ChannelId) -> Option<LMContext> {
        self.contexts
            .get(&channel_id)
            .map(|entry| entry.context.clone())
    }

    pub fn is_enabled(&self, channel_id: ChannelId) -> bool {
        self.contexts
            .get(&channel_id)
            .map(|entry| entry.enabled)
            .unwrap_or(false)
    }

    pub fn clear(&self, channel_id: ChannelId) {
        if let Some(mut entry) = self.contexts.get_mut(&channel_id) {
            entry.context.clear();
        }
    }

    pub fn set_enabled(&self, channel_id: ChannelId, enabled: bool) {
        self.update_stored(channel_id, |entry| entry.enabled = enabled);
    }

    pub fn set_voice_auto_read(&self, channel_id: ChannelId, enabled: bool) {
        self.update_stored(channel_id, |entry| entry.voice_auto_read = enabled);
    }

    pub fn is_voice_auto_read(&self, channel_id: ChannelId) -> bool {
        self.contexts
            .get(&channel_id)
            .map(|entry| entry.voice_auto_read)
            .unwrap_or(false)
    }

    pub fn set_voice_system_read(&self, channel_id: ChannelId, enabled: bool) {
        self.update_stored(channel_id, |entry| entry.voice_system_read = enabled);
    }

    pub fn is_voice_system_read(&self, channel_id: ChannelId) -> bool {
        self.contexts
            .get(&channel_id)
            .map(|entry| entry.voice_system_read)
            .unwrap_or(true)
    }

    pub fn set_voice_parallel_count(&self, channel_id: ChannelId, parallel_count: usize) -> usize {
        let normalized = clamp_voice_parallel_count(parallel_count);
        self.update_stored(channel_id, |entry| entry.voice_parallel_count = normalized);
        normalized
    }

    pub fn voice_parallel_count(&self, channel_id: ChannelId) -> usize {
        self.contexts
            .get(&channel_id)
            .map(|entry| clamp_voice_parallel_count(entry.voice_parallel_count))
            .unwrap_or(VOICE_PARALLEL_COUNT_DEFAULT)
    }

    pub fn set_voice_dictionary_entry(
        &self,
        channel_id: ChannelId,
        source: String,
        target: String,
    ) -> Result<(usize, bool), String> {
        let source = source.trim().to_string();
        let target = target.trim().to_string();

        if source.is_empty() {
            return Err("'source' must not be empty".to_string());
        }
        if target.is_empty() {
            return Err("'target' must not be empty".to_string());
        }

        let (count, updated) = {
            let mut entry = self
                .contexts
                .entry(channel_id)
                .or_insert_with(|| ChatContext::new(channel_id));

            if let Some(existing) = entry
                .voice_dictionary
                .iter_mut()
                .find(|item| item.source == source)
            {
                existing.target = target;
                (entry.voice_dictionary.len(), true)
            } else {
                if entry.voice_dictionary.len() >= VOICE_DICTIONARY_MAX_ENTRIES {
                    return Err(format!(
                        "voice dictionary limit reached for this channel: max {} entries",
                        VOICE_DICTIONARY_MAX_ENTRIES
                    ));
                }

                entry
                    .voice_dictionary
                    .push(VoiceDictionaryEntry { source, target });
                (entry.voice_dictionary.len(), false)
            }
        };

        self.save_to_disk();
        Ok((count, updated))
    }

    pub fn remove_voice_dictionary_entry(
        &self,
        channel_id: ChannelId,
        source: &str,
    ) -> Result<(usize, Option<String>), String> {
        let source = source.trim();
        if source.is_empty() {
            return Err("'source' must not be empty".to_string());
        }

        let (count, removed_target) = {
            let mut entry = self
                .contexts
                .entry(channel_id)
                .or_insert_with(|| ChatContext::new(channel_id));

            match entry
                .voice_dictionary
                .iter()
                .position(|item| item.source == source)
            {
                Some(idx) => {
                    let removed = entry.voice_dictionary.remove(idx);
                    (entry.voice_dictionary.len(), Some(removed.target))
                }
                None => (entry.voice_dictionary.len(), None),
            }
        };

        if removed_target.is_some() {
            self.save_to_disk();
        }

        Ok((count, removed_target))
    }

    pub fn voice_dictionary_entries(&self, channel_id: ChannelId) -> Vec<(String, String)> {
        self.contexts
            .get(&channel_id)
            .map(|entry| {
                entry
                    .voice_dictionary
                    .iter()
                    .map(|item| (item.source.clone(), item.target.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    pub fn voice_dictionary_count(&self, channel_id: ChannelId) -> usize {
        self.contexts
            .get(&channel_id)
            .map(|entry| entry.voice_dictionary.len())
            .unwrap_or(0)
    }

    pub fn remove_channel(&self, channel_id: ChannelId) -> bool {
        self.compacting_contexts.remove(&channel_id);
        let removed = self.contexts.remove(&channel_id).is_some();
        if removed {
            self.save_to_disk();
        }
        removed
    }

    fn update_stored(&self, channel_id: ChannelId, update: impl FnOnce(&mut ChatContext)) {
        {
            let mut entry = self
                .contexts
                .entry(channel_id)
                .or_insert_with(|| ChatContext::new(channel_id));
            update(&mut entry);
        }
        self.save_to_disk();
    }

    fn save_to_disk(&self) {
        let entries = self
            .contexts
            .iter()
            .filter_map(|entry| {
                let value = entry.value();
                if !value.should_persist() {
                    return None;
                }

                Some(json!({
                    "channel_id": value.channel_id.get().to_string(),
                    "system_prompt": value.system_prompt,
                    "enabled": value.enabled,
                    "voice_auto_read": value.voice_auto_read,
                    "voice_system_read": value.voice_system_read,
                    "voice_parallel_count": value.voice_parallel_count,
                    "voice_dictionary": value
                        .voice_dictionary
                        .iter()
                        .map(|item| json!({
                            "source": item.source,
                            "target": item.target,
                        }))
                        .collect::<Vec<Value>>(),
                }))
            })
            .collect::<Vec<Value>>();

        let doc = json!({
            "version": 1,
            "contexts": entries,
        });

        if let Some(parent) = self.store_path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            error!("failed to create chat context directory: {}", e);
            return;
        }

        let body = match serde_json::to_string_pretty(&doc) {
            Ok(v) => v,
            Err(e) => {
                error!("failed to serialize chat contexts: {}", e);
                return;
            }
        };

        if let Err(e) = fs::write(&self.store_path, body) {
            error!("failed to write chat contexts: {}", e);
        }
    }

    fn load_from_disk(&mut self) {
        let text = match fs::read_to_string(&self.store_path) {
            Ok(v) => v,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!("failed to read chat contexts file: {}", e);
                }
                return;
            }
        };

        let doc: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                warn!("failed to parse chat contexts file: {}", e);
                return;
            }
        };

        let Some(contexts) = doc.get("contexts").and_then(Value::as_array) else {
            return;
        };

        for ctx in contexts {
            let Some(channel_id_raw) = ctx.get("channel_id") else {
                continue;
            };
            let Some(channel_id_num) = parse_u64(channel_id_raw) else {
                continue;
            };

            let channel_id = ChannelId::new(channel_id_num);
            let system_prompt = ctx
                .get("system_prompt")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let enabled = ctx.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            let voice_auto_read = ctx
                .get("voice_auto_read")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let voice_system_read = ctx
                .get("voice_system_read")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let voice_parallel_count = ctx
                .get("voice_parallel_count")
                .and_then(parse_u64)
                .and_then(|v| usize::try_from(v).ok())
                .map(clamp_voice_parallel_count)
                .unwrap_or(VOICE_PARALLEL_COUNT_DEFAULT);
            let voice_dictionary = ctx
                .get("voice_dictionary")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            let source = item
                                .get("source")
                                .or_else(|| item.get("key"))
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(ToOwned::to_owned)?;

                            let target = item
                                .get("target")
                                .or_else(|| item.get("value"))
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(ToOwned::to_owned)?;

                            Some(VoiceDictionaryEntry { source, target })
                        })
                        .take(VOICE_DICTIONARY_MAX_ENTRIES)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            let mut loaded = ChatContext::new(channel_id);
            loaded.system_prompt = system_prompt;
            loaded.enabled = enabled;
            loaded.voice_auto_read = voice_auto_read;
            loaded.voice_system_read = voice_system_read;
            loaded.voice_parallel_count = voice_parallel_count;
            loaded.voice_dictionary = voice_dictionary;

            self.contexts.insert(channel_id, loaded);
        }
    }
}

fn parse_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::context::Role;

    fn test_contexts() -> ChatContexts {
        ChatContexts {
            contexts: DashMap::new(),
            default_system_prompt: "test".to_string(),
            compacting_contexts: DashMap::new(),
            next_compaction_id: AtomicU64::new(1),
            store_path: PathBuf::from("unused-in-tests.json"),
        }
    }

    fn compactable_history() -> LMContext {
        let mut context = LMContext::new();
        for text in ["old one", "old two", "recent"] {
            context.add_text(text.to_string(), Role::User);
        }
        context.record_response_total_tokens(1_000);
        context
    }

    #[test]
    fn only_one_compaction_can_run_per_channel() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(10);
        contexts.marge(channel_id, &compactable_history());

        let (compaction_id, _) = contexts.begin_compaction(channel_id, 1_000).unwrap();
        assert!(contexts.begin_compaction(channel_id, 1_000).is_none());

        contexts.cancel_compaction(channel_id, compaction_id);
        assert!(contexts.begin_compaction(channel_id, 1_000).is_some());
    }

    #[test]
    fn stale_task_cannot_unlock_a_newer_compaction() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(11);
        contexts.marge(channel_id, &compactable_history());

        let (old_id, old_compaction) = contexts.begin_compaction(channel_id, 1_000).unwrap();
        contexts.cancel_compaction(channel_id, old_id);
        let (new_id, _) = contexts.begin_compaction(channel_id, 1_000).unwrap();

        assert!(!contexts.complete_compaction(
            channel_id,
            old_id,
            &old_compaction,
            "stale summary"
        ));
        assert!(contexts.begin_compaction(channel_id, 1_000).is_none());

        contexts.cancel_compaction(channel_id, new_id);
    }
}
