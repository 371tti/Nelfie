use std::{
    fs,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use dashmap::{DashMap, mapref::entry::Entry};
use log::{error, warn};
use serde_json::{Value, json};
use serenity::all::{ChannelId, UserId};

use crate::{
    llm::{
        context::{LMContext, LMContextCompaction},
        parse_u64,
        voice_dictionary::{
            entries_as_pairs, entries_from_json, entries_to_json, prepare_entry, prepare_source,
            remove_entry, set_entry,
        },
    },
    voice::normalize_parallel_count,
};

const CHAT_CONTEXTS_STORE_PATH: &str = "data/runtime/chat_contexts.json";
pub use crate::llm::voice_dictionary::{VOICE_DICTIONARY_MAX_ENTRIES, VoiceDictionaryEntry};
pub use crate::voice::{VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX};

/// チャンネルごとのプール
pub struct ChatContexts {
    pub contexts: DashMap<ChannelId, ChatContext>,
    pub default_system_prompt: String,
    compacting_contexts: DashMap<ChannelId, ActiveCompaction>,
    next_compaction_id: AtomicU64,
    store_path: PathBuf,
    save_lock: Mutex<()>,
}

struct ActiveCompaction {
    id: u64,
    recheck_requested: bool,
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

impl ChatContexts {
    pub fn new(default_system_prompt: String) -> ChatContexts {
        let mut chat_contexts = ChatContexts {
            contexts: DashMap::new(),
            default_system_prompt,
            compacting_contexts: DashMap::new(),
            next_compaction_id: AtomicU64::new(1),
            store_path: PathBuf::from(CHAT_CONTEXTS_STORE_PATH),
            save_lock: Mutex::new(()),
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
        trigger_token_limit: u32,
        target_token_limit: usize,
    ) -> Option<(u64, LMContextCompaction)> {
        let compaction_id = self.next_compaction_id.fetch_add(1, Ordering::Relaxed);
        match self.compacting_contexts.entry(channel_id) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().recheck_requested = true;
                return None;
            }
            Entry::Vacant(entry) => {
                entry.insert(ActiveCompaction {
                    id: compaction_id,
                    recheck_requested: false,
                });
            }
        }

        let history = self
            .contexts
            .get(&channel_id)
            .map(|entry| entry.context.clone());
        let compaction = history
            .and_then(|history| history.plan_compaction(trigger_token_limit, target_token_limit));
        if compaction.is_none() {
            self.compacting_contexts
                .remove_if(&channel_id, |_, active| active.id == compaction_id);
        }
        compaction.map(|compaction| (compaction_id, compaction))
    }

    pub fn continue_compaction(
        &self,
        channel_id: ChannelId,
        compaction_id: u64,
        target_token_limit: usize,
    ) -> Option<LMContextCompaction> {
        let is_active = self
            .compacting_contexts
            .get(&channel_id)
            .is_some_and(|active| active.id == compaction_id);
        if !is_active {
            return None;
        }
        let history = self
            .contexts
            .get(&channel_id)
            .map(|entry| entry.context.clone());
        history.and_then(|history| history.plan_compaction_batch(target_token_limit))
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
        if active_compaction.id != compaction_id {
            return false;
        }

        self.contexts
            .get_mut(&channel_id)
            .is_some_and(|mut entry| entry.context.apply_compaction(compaction, summary))
    }

    pub fn finish_compaction(&self, channel_id: ChannelId, compaction_id: u64) -> bool {
        match self.compacting_contexts.entry(channel_id) {
            Entry::Occupied(entry) if entry.get().id == compaction_id => {
                let recheck_requested = entry.get().recheck_requested;
                entry.remove();
                recheck_requested
            }
            _ => false,
        }
    }

    pub fn cancel_compaction(&self, channel_id: ChannelId, compaction_id: u64) -> bool {
        match self.compacting_contexts.entry(channel_id) {
            Entry::Occupied(entry) if entry.get().id == compaction_id => {
                let recheck_requested = entry.get().recheck_requested;
                entry.remove();
                recheck_requested
            }
            _ => false,
        }
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
        let normalized = normalize_parallel_count(parallel_count);
        self.update_stored(channel_id, |entry| entry.voice_parallel_count = normalized);
        normalized
    }

    pub fn voice_parallel_count(&self, channel_id: ChannelId) -> usize {
        self.contexts
            .get(&channel_id)
            .map(|entry| normalize_parallel_count(entry.voice_parallel_count))
            .unwrap_or(VOICE_PARALLEL_COUNT_DEFAULT)
    }

    pub fn set_voice_dictionary_entry(
        &self,
        channel_id: ChannelId,
        source: String,
        target: String,
        registered_by: Option<UserId>,
    ) -> Result<(usize, bool), String> {
        let (source, target) = prepare_entry(source, target)?;
        let result = {
            let mut entry = self
                .contexts
                .entry(channel_id)
                .or_insert_with(|| ChatContext::new(channel_id));
            set_entry(
                &mut entry.voice_dictionary,
                source,
                target,
                registered_by,
                "channel",
            )?
        };

        self.save_to_disk();
        Ok(result)
    }

    pub fn remove_voice_dictionary_entry(
        &self,
        channel_id: ChannelId,
        source: &str,
    ) -> Result<(usize, Option<String>), String> {
        let source = prepare_source(source)?;
        let result = {
            let mut entry = self
                .contexts
                .entry(channel_id)
                .or_insert_with(|| ChatContext::new(channel_id));
            remove_entry(&mut entry.voice_dictionary, &source)?
        };
        let (count, removed_target) = result;

        if removed_target.is_some() {
            self.save_to_disk();
        }

        Ok((count, removed_target))
    }

    pub fn voice_dictionary_entries(&self, channel_id: ChannelId) -> Vec<(String, String)> {
        self.contexts
            .get(&channel_id)
            .map(|entry| entries_as_pairs(&entry.voice_dictionary))
            .unwrap_or_default()
    }

    pub fn voice_dictionary_records(&self, channel_id: ChannelId) -> Vec<VoiceDictionaryEntry> {
        self.contexts
            .get(&channel_id)
            .map(|entry| entry.voice_dictionary.clone())
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
        let _save_guard = self
            .save_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
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
                    "voice_dictionary": entries_to_json(&value.voice_dictionary),
                }))
            })
            .collect::<Vec<Value>>();

        let doc = json!({
            "version": 1,
            "contexts": entries,
        });

        let body = match serde_json::to_string_pretty(&doc) {
            Ok(v) => v,
            Err(e) => {
                error!("failed to serialize chat contexts: {}", e);
                return;
            }
        };

        if let Err(e) = crate::app::atomic_file::write(&self.store_path, body.as_bytes()) {
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
                .map(normalize_parallel_count)
                .unwrap_or(VOICE_PARALLEL_COUNT_DEFAULT);
            let voice_dictionary = entries_from_json(ctx.get("voice_dictionary"));

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
            save_lock: Mutex::new(()),
        }
    }

    fn compactable_history() -> LMContext {
        let mut context = LMContext::new();
        for text in ["old one", "old two", "recent"] {
            context.add_text(text.to_string(), Role::User);
        }
        context
    }

    #[test]
    fn only_one_compaction_can_run_per_channel() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(10);
        contexts.marge(channel_id, &compactable_history());

        let (compaction_id, _) = contexts.begin_compaction(channel_id, 1, 0).unwrap();
        assert!(contexts.begin_compaction(channel_id, 1, 0).is_none());

        contexts.cancel_compaction(channel_id, compaction_id);
        assert!(contexts.begin_compaction(channel_id, 1, 0).is_some());
    }

    #[test]
    fn stale_task_cannot_unlock_a_newer_compaction() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(11);
        contexts.marge(channel_id, &compactable_history());

        let (old_id, old_compaction) = contexts.begin_compaction(channel_id, 1, 0).unwrap();
        contexts.cancel_compaction(channel_id, old_id);
        let (new_id, _) = contexts.begin_compaction(channel_id, 1, 0).unwrap();

        assert!(!contexts.complete_compaction(
            channel_id,
            old_id,
            &old_compaction,
            "stale summary"
        ));
        assert!(contexts.begin_compaction(channel_id, 1, 0).is_none());

        contexts.cancel_compaction(channel_id, new_id);
    }
}
