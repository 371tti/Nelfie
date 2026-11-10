use std::{
    fs,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use dashmap::{DashMap, mapref::entry::Entry};
use log::{error, warn};
use serde_json::{Value, json};
use serenity::all::{ChannelId, UserId};

use crate::{
    app::rate_limit::advance_rate_limit_bucket,
    llm::{
        context::{LMContext, LMContextCompaction, Role},
        parse_u64,
        voice_dictionary::{
            entries_as_pairs, entries_from_json, entries_to_json, prepare_entry, prepare_source,
            remove_entry, set_entry,
        },
    },
    voice::normalize_parallel_count,
};

const CHAT_CONTEXTS_STORE_PATH: &str = "data/runtime/chat_contexts.json";
const AUTO_REPLAY_JUDGE_INTERVAL_SECS: u64 = 10;
const AUTO_REPLAY_SLEEP_INITIAL_SECS: u64 = 60;
const AUTO_REPLAY_SLEEP_MAX_SECS: u64 = 32 * 60;
pub use crate::llm::voice_dictionary::{VOICE_DICTIONARY_MAX_ENTRIES, VoiceDictionaryEntry};
pub use crate::voice::{VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ChatResponseMode {
    #[default]
    Mention,
    AutoReplay,
}

impl ChatResponseMode {
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "mention" => Some(Self::Mention),
            "auto-reply" | "auto-replay" => Some(Self::AutoReplay),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::AutoReplay => "auto-reply",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoReplayDecision {
    Respond,
    Ignore,
    Sleep,
}

impl AutoReplayDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Respond => "respond",
            Self::Ignore => "ignore",
            Self::Sleep => "sleep",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PreviousAutoReplayJudgeResult {
    pub decision: AutoReplayDecision,
    pub message_timestamp: String,
    pub judged_at: String,
}

#[derive(Debug, Clone)]
pub struct AutoReplayJudgeToken {
    request_id: u64,
    mode_generation: u64,
    message_revision: u64,
    previous_result: Option<PreviousAutoReplayJudgeResult>,
}

impl AutoReplayJudgeToken {
    pub fn previous_result(&self) -> Option<&PreviousAutoReplayJudgeResult> {
        self.previous_result.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InferenceSlot {
    AutoReplayJudge(u64),
    AutoReplayResponse(u64),
    Response(u64),
}

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
    pub response_mode: ChatResponseMode,
    pub voice_auto_read: bool,
    pub voice_system_read: bool,
    pub voice_parallel_count: usize,
    pub voice_dictionary: Vec<VoiceDictionaryEntry>,
    auto_replay_judge_rate_line: u64,
    auto_replay_sleep_until: Option<Instant>,
    auto_replay_next_sleep_secs: u64,
    inference_slot: Option<InferenceSlot>,
    auto_replay_judge_in_flight: Option<u64>,
    previous_auto_replay_judge_result: Option<PreviousAutoReplayJudgeResult>,
    auto_replay_mode_generation: u64,
    next_auto_replay_request_id: u64,
    incoming_message_revision: u64,
    last_message_minute: Option<i64>,
}

impl ChatContext {
    pub fn new(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            context: LMContext::new(),
            system_prompt: None,
            enabled: false,
            response_mode: ChatResponseMode::Mention,
            voice_auto_read: false,
            voice_system_read: true,
            voice_parallel_count: VOICE_PARALLEL_COUNT_DEFAULT,
            voice_dictionary: Vec::new(),
            auto_replay_judge_rate_line: 1,
            auto_replay_sleep_until: None,
            auto_replay_next_sleep_secs: AUTO_REPLAY_SLEEP_INITIAL_SECS,
            inference_slot: None,
            auto_replay_judge_in_flight: None,
            previous_auto_replay_judge_result: None,
            auto_replay_mode_generation: 0,
            next_auto_replay_request_id: 1,
            incoming_message_revision: 0,
            last_message_minute: None,
        }
    }

    fn should_persist(&self) -> bool {
        self.system_prompt.is_some()
            || self.enabled
            || self.response_mode != ChatResponseMode::Mention
            || self.voice_auto_read
            || !self.voice_system_read
            || self.voice_parallel_count != VOICE_PARALLEL_COUNT_DEFAULT
            || !self.voice_dictionary.is_empty()
    }

    fn reset_auto_replay_runtime(&mut self) {
        self.auto_replay_judge_rate_line = 1;
        self.auto_replay_sleep_until = None;
        self.auto_replay_next_sleep_secs = AUTO_REPLAY_SLEEP_INITIAL_SECS;
        if matches!(
            self.inference_slot,
            Some(InferenceSlot::AutoReplayJudge(_) | InferenceSlot::AutoReplayResponse(_))
        ) {
            self.inference_slot = None;
        }
        self.previous_auto_replay_judge_result = None;
        self.auto_replay_mode_generation = self.auto_replay_mode_generation.wrapping_add(1);
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

    pub fn set_response_mode(&self, channel_id: ChannelId, mode: ChatResponseMode) -> bool {
        let changed = {
            let mut entry = self
                .contexts
                .entry(channel_id)
                .or_insert_with(|| ChatContext::new(channel_id));
            let changed = !entry.enabled || entry.response_mode != mode;
            if entry.response_mode != mode || !entry.enabled {
                entry.response_mode = mode;
                entry.reset_auto_replay_runtime();
            }
            entry.enabled = true;
            changed
        };

        if changed {
            self.save_to_disk();
        }
        changed
    }

    pub fn is_auto_replay_enabled(&self, channel_id: ChannelId) -> bool {
        self.contexts
            .get(&channel_id)
            .is_some_and(|entry| entry.response_mode == ChatResponseMode::AutoReplay)
    }

    pub fn record_incoming_message(&self, channel_id: ChannelId) -> u64 {
        let mut entry = self
            .contexts
            .entry(channel_id)
            .or_insert_with(|| ChatContext::new(channel_id));
        entry.incoming_message_revision = entry.incoming_message_revision.wrapping_add(1);
        entry.incoming_message_revision
    }

    pub fn begin_auto_replay_judge(
        &self,
        channel_id: ChannelId,
        now_epoch_secs: u64,
        message_revision: u64,
        now: Instant,
    ) -> Option<AutoReplayJudgeToken> {
        let mut entry = self
            .contexts
            .entry(channel_id)
            .or_insert_with(|| ChatContext::new(channel_id));

        if !entry.enabled
            || entry.response_mode != ChatResponseMode::AutoReplay
            || message_revision != entry.incoming_message_revision
            || entry.auto_replay_judge_in_flight.is_some()
            || entry.inference_slot.is_some()
        {
            return None;
        }
        if entry
            .auto_replay_sleep_until
            .is_some_and(|sleep_until| sleep_until > now)
        {
            return None;
        }
        entry.auto_replay_sleep_until = None;

        let next_rate_line = advance_rate_limit_bucket(
            entry.auto_replay_judge_rate_line,
            now_epoch_secs,
            AUTO_REPLAY_JUDGE_INTERVAL_SECS,
            AUTO_REPLAY_JUDGE_INTERVAL_SECS,
        )
        .ok()?;
        entry.auto_replay_judge_rate_line = next_rate_line;

        let request_id = entry.next_auto_replay_request_id;
        entry.next_auto_replay_request_id = request_id.wrapping_add(1).max(1);
        entry.auto_replay_judge_in_flight = Some(request_id);
        entry.inference_slot = Some(InferenceSlot::AutoReplayJudge(request_id));
        Some(AutoReplayJudgeToken {
            request_id,
            mode_generation: entry.auto_replay_mode_generation,
            message_revision: entry.incoming_message_revision,
            previous_result: entry.previous_auto_replay_judge_result.clone(),
        })
    }

    pub fn invalidate_auto_replay_judge(&self, channel_id: ChannelId) {
        if let Some(mut entry) = self.contexts.get_mut(&channel_id)
            && matches!(
                entry.inference_slot,
                Some(InferenceSlot::AutoReplayJudge(_) | InferenceSlot::AutoReplayResponse(_))
            )
        {
            entry.inference_slot = None;
            entry.auto_replay_mode_generation = entry.auto_replay_mode_generation.wrapping_add(1);
        }
    }

    pub fn begin_response_inference(
        &self,
        channel_id: ChannelId,
        request_id: u64,
        judge_reservation: Option<&AutoReplayJudgeToken>,
    ) -> bool {
        let mut entry = self
            .contexts
            .entry(channel_id)
            .or_insert_with(|| ChatContext::new(channel_id));

        if let Some(token) = judge_reservation {
            if entry.inference_slot != Some(InferenceSlot::AutoReplayResponse(token.request_id))
                || token.mode_generation != entry.auto_replay_mode_generation
                || token.message_revision != entry.incoming_message_revision
                || !entry.enabled
                || entry.response_mode != ChatResponseMode::AutoReplay
            {
                if entry.inference_slot == Some(InferenceSlot::AutoReplayResponse(token.request_id))
                {
                    entry.inference_slot = None;
                }
                return false;
            }
        } else if matches!(
            entry.inference_slot,
            Some(InferenceSlot::AutoReplayJudge(_) | InferenceSlot::AutoReplayResponse(_))
        ) {
            entry.auto_replay_mode_generation = entry.auto_replay_mode_generation.wrapping_add(1);
        }

        entry.inference_slot = Some(InferenceSlot::Response(request_id));
        true
    }

    pub fn finish_response_inference(&self, channel_id: ChannelId, request_id: u64) -> bool {
        let Some(mut entry) = self.contexts.get_mut(&channel_id) else {
            return false;
        };
        if entry.inference_slot != Some(InferenceSlot::Response(request_id)) {
            return false;
        }
        entry.inference_slot = None;
        true
    }

    pub fn finish_auto_replay_judge(
        &self,
        channel_id: ChannelId,
        token: &AutoReplayJudgeToken,
        decision: Option<AutoReplayDecision>,
        now: Instant,
        message_timestamp: &str,
        judged_at: &str,
    ) -> bool {
        let Some(mut entry) = self.contexts.get_mut(&channel_id) else {
            return false;
        };
        if entry.auto_replay_judge_in_flight != Some(token.request_id) {
            return false;
        }
        entry.auto_replay_judge_in_flight = None;

        if entry.inference_slot != Some(InferenceSlot::AutoReplayJudge(token.request_id)) {
            return false;
        }

        if token.mode_generation != entry.auto_replay_mode_generation
            || token.message_revision != entry.incoming_message_revision
            || !entry.enabled
            || entry.response_mode != ChatResponseMode::AutoReplay
        {
            entry.inference_slot = None;
            return false;
        }

        if let Some(decision) = decision {
            entry.previous_auto_replay_judge_result = Some(PreviousAutoReplayJudgeResult {
                decision,
                message_timestamp: message_timestamp.to_string(),
                judged_at: judged_at.to_string(),
            });
        }

        match decision {
            Some(AutoReplayDecision::Respond) => {
                entry.auto_replay_sleep_until = None;
                entry.auto_replay_next_sleep_secs = AUTO_REPLAY_SLEEP_INITIAL_SECS;
                entry.inference_slot = Some(InferenceSlot::AutoReplayResponse(token.request_id));
                true
            }
            Some(AutoReplayDecision::Ignore) => {
                entry.inference_slot = None;
                if entry
                    .auto_replay_sleep_until
                    .is_some_and(|sleep_until| sleep_until <= now)
                {
                    entry.auto_replay_sleep_until = None;
                }
                false
            }
            Some(AutoReplayDecision::Sleep) => {
                entry.inference_slot = None;
                let sleep_secs = entry
                    .auto_replay_next_sleep_secs
                    .clamp(AUTO_REPLAY_SLEEP_INITIAL_SECS, AUTO_REPLAY_SLEEP_MAX_SECS);
                entry.auto_replay_sleep_until = Some(now + Duration::from_secs(sleep_secs));
                entry.auto_replay_next_sleep_secs =
                    sleep_secs.saturating_mul(2).min(AUTO_REPLAY_SLEEP_MAX_SECS);
                false
            }
            None => {
                entry.inference_slot = None;
                false
            }
        }
    }

    pub fn marge(&self, channel_id: ChannelId, other: &LMContext) {
        self.contexts
            .entry(channel_id)
            .or_insert_with(|| ChatContext::new(channel_id))
            .context
            .extend(other);
    }

    pub fn marge_discord_message(
        &self,
        channel_id: ChannelId,
        timestamp_minute: i64,
        time_marker: &str,
        message: &LMContext,
    ) {
        let mut entry = self
            .contexts
            .entry(channel_id)
            .or_insert_with(|| ChatContext::new(channel_id));
        if entry.last_message_minute != Some(timestamp_minute) {
            entry.context.add_text(time_marker.to_string(), Role::User);
            entry.last_message_minute = Some(timestamp_minute);
        }
        entry.context.extend(message);
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
            entry.last_message_minute = None;
        }
    }

    pub fn set_enabled(&self, channel_id: ChannelId, enabled: bool) {
        self.update_stored(channel_id, |entry| {
            entry.enabled = enabled;
            if !enabled {
                entry.response_mode = ChatResponseMode::Mention;
                entry.reset_auto_replay_runtime();
            }
        });
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
                    "response_mode": value.response_mode.as_str(),
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
            let response_mode = ctx
                .get("response_mode")
                .and_then(Value::as_str)
                .and_then(ChatResponseMode::from_name)
                .unwrap_or_default();
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
            loaded.response_mode = response_mode;
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

    fn enable_auto_replay(contexts: &ChatContexts, channel_id: ChannelId) {
        let mut entry = ChatContext::new(channel_id);
        entry.enabled = true;
        entry.response_mode = ChatResponseMode::AutoReplay;
        contexts.contexts.insert(channel_id, entry);
    }

    #[test]
    fn response_mode_persists_and_legacy_entries_default_to_mention() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat_contexts.json");
        let channel_id = ChannelId::new(20);

        let mut contexts = test_contexts();
        contexts.store_path = path.clone();
        assert!(contexts.set_response_mode(channel_id, ChatResponseMode::AutoReplay));
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["contexts"][0]["response_mode"], "auto-reply");
        assert_eq!(
            ChatResponseMode::from_name("auto-reply"),
            Some(ChatResponseMode::AutoReplay)
        );
        assert_eq!(
            ChatResponseMode::from_name("auto-replay"),
            Some(ChatResponseMode::AutoReplay)
        );

        let mut loaded = test_contexts();
        loaded.store_path = path.clone();
        loaded.load_from_disk();
        assert!(loaded.is_enabled(channel_id));
        assert!(loaded.is_auto_replay_enabled(channel_id));

        std::fs::write(
            &path,
            r#"{"contexts":[{"channel_id":"21","enabled":true}]}"#,
        )
        .unwrap();
        let mut legacy = test_contexts();
        legacy.store_path = path;
        legacy.load_from_disk();
        assert!(legacy.is_enabled(ChannelId::new(21)));
        assert!(!legacy.is_auto_replay_enabled(ChannelId::new(21)));
    }

    #[test]
    fn judge_rate_limit_is_per_channel_and_allows_again_after_ten_seconds() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(30);
        enable_auto_replay(&contexts, channel_id);
        let now = Instant::now();

        let token = contexts
            .begin_auto_replay_judge(channel_id, 1_000, 0, now)
            .unwrap();
        assert!(!contexts.finish_auto_replay_judge(
            channel_id,
            &token,
            Some(AutoReplayDecision::Ignore),
            now,
            "message-at-1000",
            "judge-at-1000"
        ));
        assert!(
            contexts
                .begin_auto_replay_judge(channel_id, 1_000, 0, now)
                .is_none()
        );

        let other_channel = ChannelId::new(31);
        enable_auto_replay(&contexts, other_channel);
        assert!(
            contexts
                .begin_auto_replay_judge(other_channel, 1_000, 0, now)
                .is_some()
        );
        let next = contexts
            .begin_auto_replay_judge(channel_id, 1_010, 0, now + Duration::from_secs(10))
            .unwrap();
        let previous = next.previous_result().unwrap();
        assert_eq!(previous.decision, AutoReplayDecision::Ignore);
        assert_eq!(previous.message_timestamp, "message-at-1000");
        assert_eq!(previous.judged_at, "judge-at-1000");
    }

    #[test]
    fn sleep_backoff_grows_to_thirty_two_minutes_and_respond_resets_it() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(32);
        enable_auto_replay(&contexts, channel_id);
        let start = Instant::now();
        let start_epoch = 10_000;

        for (offset, sleep_secs, expected_next_sleep) in [
            (0, 60, 120),
            (60, 120, 240),
            (180, 240, 480),
            (420, 480, 960),
            (900, 960, 1_920),
            (1_860, 1_920, 1_920),
        ] {
            let now = start + Duration::from_secs(offset);
            let token = contexts
                .begin_auto_replay_judge(channel_id, start_epoch + offset, 0, now)
                .unwrap();
            assert!(!contexts.finish_auto_replay_judge(
                channel_id,
                &token,
                Some(AutoReplayDecision::Sleep),
                now,
                "message-at-sleep",
                "judge-at-sleep"
            ));
            let entry = contexts.contexts.get(&channel_id).unwrap();
            assert_eq!(entry.auto_replay_next_sleep_secs, expected_next_sleep);
            assert_eq!(
                entry.auto_replay_sleep_until.unwrap().duration_since(now),
                Duration::from_secs(sleep_secs)
            );
            drop(entry);
            assert!(
                contexts
                    .begin_auto_replay_judge(
                        channel_id,
                        start_epoch + offset + 1,
                        0,
                        now + Duration::from_secs(1),
                    )
                    .is_none()
            );
        }

        let respond_at = start + Duration::from_secs(3_780);
        let token = contexts
            .begin_auto_replay_judge(channel_id, start_epoch + 3_780, 0, respond_at)
            .unwrap();
        assert!(contexts.finish_auto_replay_judge(
            channel_id,
            &token,
            Some(AutoReplayDecision::Respond),
            respond_at,
            "message-at-respond",
            "judge-at-respond"
        ));
        assert_eq!(
            contexts
                .contexts
                .get(&channel_id)
                .unwrap()
                .auto_replay_next_sleep_secs,
            AUTO_REPLAY_SLEEP_INITIAL_SECS
        );
        assert!(contexts.begin_response_inference(channel_id, 99, Some(&token)));
        assert!(contexts.finish_response_inference(channel_id, 99));

        let next_judge_at = respond_at + Duration::from_secs(10);
        let token = contexts
            .begin_auto_replay_judge(channel_id, start_epoch + 3_790, 0, next_judge_at)
            .unwrap();
        contexts.finish_auto_replay_judge(
            channel_id,
            &token,
            Some(AutoReplayDecision::Sleep),
            next_judge_at,
            "message-after-respond",
            "judge-after-respond",
        );
        assert_eq!(
            contexts
                .contexts
                .get(&channel_id)
                .unwrap()
                .auto_replay_sleep_until
                .unwrap()
                .duration_since(next_judge_at),
            Duration::from_secs(AUTO_REPLAY_SLEEP_INITIAL_SECS)
        );
    }

    #[test]
    fn leaving_auto_reply_resets_sleep_stage_and_previous_result() {
        let directory = tempfile::tempdir().unwrap();
        let mut contexts = test_contexts();
        contexts.store_path = directory.path().join("chat_contexts.json");
        let channel_id = ChannelId::new(39);
        contexts.set_response_mode(channel_id, ChatResponseMode::AutoReplay);
        let now = Instant::now();
        let token = contexts
            .begin_auto_replay_judge(channel_id, 7_000, 0, now)
            .unwrap();
        contexts.finish_auto_replay_judge(
            channel_id,
            &token,
            Some(AutoReplayDecision::Sleep),
            now,
            "message-at-7000",
            "judge-at-7000",
        );

        contexts.set_response_mode(channel_id, ChatResponseMode::Mention);
        contexts.set_response_mode(channel_id, ChatResponseMode::AutoReplay);
        let next = contexts
            .begin_auto_replay_judge(channel_id, 7_001, 0, now + Duration::from_secs(1))
            .unwrap();

        assert!(next.previous_result().is_none());
        let entry = contexts.contexts.get(&channel_id).unwrap();
        assert_eq!(
            entry.auto_replay_next_sleep_secs,
            AUTO_REPLAY_SLEEP_INITIAL_SECS
        );
        assert!(entry.auto_replay_sleep_until.is_none());
    }

    #[test]
    fn invalidated_judge_cannot_schedule_a_reply() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(33);
        enable_auto_replay(&contexts, channel_id);
        let now = Instant::now();
        let token = contexts
            .begin_auto_replay_judge(channel_id, 2_000, 0, now)
            .unwrap();

        contexts.invalidate_auto_replay_judge(channel_id);
        assert!(!contexts.finish_auto_replay_judge(
            channel_id,
            &token,
            Some(AutoReplayDecision::Respond),
            now,
            "message-at-2000",
            "judge-at-2000"
        ));
    }

    #[test]
    fn inference_slot_blocks_judge_and_old_response_completion_cannot_clear_new_slot() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(34);
        enable_auto_replay(&contexts, channel_id);
        let now = Instant::now();

        assert!(contexts.begin_response_inference(channel_id, 100, None));
        assert!(
            contexts
                .begin_auto_replay_judge(channel_id, 3_000, 0, now)
                .is_none()
        );
        assert!(contexts.begin_response_inference(channel_id, 101, None));
        assert!(!contexts.finish_response_inference(channel_id, 100));
        assert!(
            contexts
                .begin_auto_replay_judge(channel_id, 3_000, 0, now)
                .is_none()
        );
        assert!(contexts.finish_response_inference(channel_id, 101));
        assert!(
            contexts
                .begin_auto_replay_judge(channel_id, 3_000, 0, now)
                .is_some()
        );
    }

    #[test]
    fn judge_respond_reserves_the_channel_until_its_reply_starts() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(35);
        enable_auto_replay(&contexts, channel_id);
        let now = Instant::now();
        let token = contexts
            .begin_auto_replay_judge(channel_id, 4_000, 0, now)
            .unwrap();

        assert!(contexts.finish_auto_replay_judge(
            channel_id,
            &token,
            Some(AutoReplayDecision::Respond),
            now,
            "message-at-4000",
            "judge-at-4000"
        ));
        assert!(
            contexts
                .begin_auto_replay_judge(channel_id, 4_010, 0, now + Duration::from_secs(10))
                .is_none()
        );
        assert!(contexts.begin_response_inference(channel_id, 200, Some(&token)));
        assert!(contexts.finish_response_inference(channel_id, 200));
    }

    #[test]
    fn stale_respond_reservation_is_released_when_a_new_post_arrives() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(40);
        enable_auto_replay(&contexts, channel_id);
        let now = Instant::now();
        let token = contexts
            .begin_auto_replay_judge(channel_id, 4_500, 0, now)
            .unwrap();
        assert!(contexts.finish_auto_replay_judge(
            channel_id,
            &token,
            Some(AutoReplayDecision::Respond),
            now,
            "message-at-4500",
            "judge-at-4500"
        ));

        let revision = contexts.record_incoming_message(channel_id);
        assert!(!contexts.begin_response_inference(channel_id, 250, Some(&token)));
        assert!(
            contexts
                .begin_auto_replay_judge(channel_id, 4_510, revision, now + Duration::from_secs(10))
                .is_some()
        );
    }

    #[test]
    fn stale_judge_result_is_discarded_after_a_new_message_arrives() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(36);
        enable_auto_replay(&contexts, channel_id);
        let now = Instant::now();
        let token = contexts
            .begin_auto_replay_judge(channel_id, 5_000, 0, now)
            .unwrap();

        contexts.record_incoming_message(channel_id);
        assert!(!contexts.finish_auto_replay_judge(
            channel_id,
            &token,
            Some(AutoReplayDecision::Respond),
            now,
            "message-at-5000",
            "judge-at-5000"
        ));
        let next = contexts
            .begin_auto_replay_judge(channel_id, 5_010, 1, now + Duration::from_secs(10))
            .unwrap();
        assert!(next.previous_result().is_none());
    }

    #[test]
    fn delayed_message_handler_cannot_start_a_judge_for_an_older_post() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(38);
        enable_auto_replay(&contexts, channel_id);
        let first_revision = contexts.record_incoming_message(channel_id);
        let latest_revision = contexts.record_incoming_message(channel_id);
        let now = Instant::now();

        assert!(
            contexts
                .begin_auto_replay_judge(channel_id, 5_500, first_revision, now)
                .is_none()
        );
        assert!(
            contexts
                .begin_auto_replay_judge(channel_id, 5_500, latest_revision, now)
                .is_some()
        );
    }

    #[test]
    fn preempted_judge_stays_busy_until_its_api_call_finishes() {
        let contexts = test_contexts();
        let channel_id = ChannelId::new(37);
        enable_auto_replay(&contexts, channel_id);
        let now = Instant::now();
        let token = contexts
            .begin_auto_replay_judge(channel_id, 6_000, 0, now)
            .unwrap();

        contexts.invalidate_auto_replay_judge(channel_id);
        assert!(contexts.begin_response_inference(channel_id, 300, None));
        assert!(contexts.finish_response_inference(channel_id, 300));
        assert!(
            contexts
                .begin_auto_replay_judge(channel_id, 6_010, 0, now + Duration::from_secs(10))
                .is_none()
        );
        assert!(!contexts.finish_auto_replay_judge(
            channel_id,
            &token,
            Some(AutoReplayDecision::Respond),
            now + Duration::from_secs(10),
            "message-at-6000",
            "judge-at-6010"
        ));
        assert!(
            contexts
                .begin_auto_replay_judge(channel_id, 6_010, 0, now + Duration::from_secs(10))
                .is_some()
        );
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
