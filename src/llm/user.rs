use std::{fs, path::PathBuf, sync::Mutex};

use dashmap::DashMap;
use log::{error, warn};
use serde_json::{Value, json};
use serenity::all::{GuildId, UserId};

use crate::llm::{
    parse_u64,
    voice_dictionary::{
        entries_as_pairs, entries_from_json, entries_to_json, remove_entry, set_entry,
        prepare_entry, prepare_source, VoiceDictionaryEntry,
    },
};
use crate::llm::models::{ImageGenerationModels, Models};

const USER_CONTEXTS_STORE_PATH: &str = "data/runtime/user_contexts.json";

/// ユーザー情報のプール
pub struct UserContexts {
    pub contexts: DashMap<UserId, UserContext>,
    pub guild_bot_contexts: DashMap<GuildId, UserContext>,
    store_path: PathBuf,
    save_lock: Mutex<()>,
}

/// ユーザー情報の構造体
#[derive(Clone)]
pub struct UserContext {
    pub user_id: UserId,
    pub main_model: Models,
    pub image_generation_model: ImageGenerationModels,
    pub rate_line: u64,
    pub cron_rate_line: u64,
    pub voice_speaker: Option<u32>,
    pub voice_speed_scale: Option<f32>,
    pub voice_pitch_scale: Option<f32>,
    pub voice_pan: Option<f32>,
    pub voice_dictionary: Vec<VoiceDictionaryEntry>,
}

impl UserContext {
    pub fn new(user_id: UserId) -> Self {
        Self::with_model(user_id, Models::USER_DEFAULT_MODEL)
    }

    fn new_system(user_id: UserId) -> Self {
        Self::with_model(user_id, Models::SYSTEM_MODEL)
    }

    fn with_model(user_id: UserId, main_model: Models) -> Self {
        Self {
            user_id,
            main_model,
            image_generation_model: ImageGenerationModels::DEFAULT,
            rate_line: 1,
            cron_rate_line: 1,
            voice_speaker: None,
            voice_speed_scale: None,
            voice_pitch_scale: None,
            voice_pan: None,
            voice_dictionary: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserContextScope {
    User(UserId),
    GuildBot {
        guild_id: GuildId,
        bot_user_id: UserId,
    },
}

impl UserContextScope {
    pub fn for_actor(user_id: UserId, guild_id: Option<GuildId>, bot_user_id: UserId) -> Self {
        if user_id == bot_user_id
            && let Some(guild_id) = guild_id
        {
            Self::GuildBot {
                guild_id,
                bot_user_id,
            }
        } else {
            Self::User(user_id)
        }
    }
}

impl UserContexts {
    pub fn new() -> UserContexts {
        let mut user_contexts = UserContexts {
            contexts: DashMap::new(),
            guild_bot_contexts: DashMap::new(),
            store_path: PathBuf::from(USER_CONTEXTS_STORE_PATH),
            save_lock: Mutex::new(()),
        };
        user_contexts.load_from_disk();
        user_contexts
    }

    pub fn get_or_create(&self, user_id: UserId) -> UserContext {
        match self.contexts.entry(user_id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                let ctx = UserContext::new(user_id);
                let out = ctx.clone();
                vacant.insert(ctx);
                out
            }
        }
    }

    pub fn get_or_create_guild_bot(&self, guild_id: GuildId, bot_user_id: UserId) -> UserContext {
        match self.guild_bot_contexts.entry(guild_id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                let ctx = UserContext::new_system(bot_user_id);
                let out = ctx.clone();
                vacant.insert(ctx);
                out
            }
        }
    }

    pub fn get_or_create_scoped(&self, scope: UserContextScope) -> UserContext {
        match scope {
            UserContextScope::User(user_id) => self.get_or_create(user_id),
            UserContextScope::GuildBot {
                guild_id,
                bot_user_id,
            } => self.get_or_create_guild_bot(guild_id, bot_user_id),
        }
    }

    pub fn get_or_create_actor(
        &self,
        user_id: UserId,
        guild_id: Option<GuildId>,
        bot_user_id: UserId,
    ) -> UserContext {
        self.get_or_create_scoped(UserContextScope::for_actor(user_id, guild_id, bot_user_id))
    }

    pub fn set_model(&self, user_id: UserId, model: Models) {
        self.contexts
            .entry(user_id)
            .or_insert_with(|| UserContext::new(user_id))
            .main_model = model;
        self.save_to_disk();
    }

    pub fn set_model_scoped(&self, scope: UserContextScope, model: Models) {
        match scope {
            UserContextScope::User(user_id) => self.set_model(user_id, model),
            UserContextScope::GuildBot {
                guild_id,
                bot_user_id,
            } => {
                self.guild_bot_contexts
                    .entry(guild_id)
                    .or_insert_with(|| UserContext::new_system(bot_user_id))
                    .main_model = model;
                self.save_to_disk();
            }
        }
    }

    pub fn set_image_generation_model_scoped(
        &self,
        scope: UserContextScope,
        model: ImageGenerationModels,
    ) {
        match scope {
            UserContextScope::User(user_id) => {
                self.contexts
                    .entry(user_id)
                    .or_insert_with(|| UserContext::new(user_id))
                    .image_generation_model = model;
            }
            UserContextScope::GuildBot {
                guild_id,
                bot_user_id,
            } => {
                self.guild_bot_contexts
                    .entry(guild_id)
                    .or_insert_with(|| UserContext::new_system(bot_user_id))
                    .image_generation_model = model;
            }
        }
        self.save_to_disk();
    }

    pub fn set_rate_line(&self, user_id: UserId, rate_line: u64) {
        let old_rate_line;
        {
            let mut entry = self
                .contexts
                .entry(user_id)
                .or_insert_with(|| UserContext::new(user_id));

            old_rate_line = entry.rate_line;
            entry.rate_line = rate_line;
        }

        if old_rate_line == 0 || rate_line == 0 {
            self.save_to_disk();
        }
    }

    pub fn set_guild_bot_rate_line(&self, guild_id: GuildId, bot_user_id: UserId, rate_line: u64) {
        let old_rate_line;
        {
            let mut entry = self
                .guild_bot_contexts
                .entry(guild_id)
                .or_insert_with(|| UserContext::new_system(bot_user_id));

            old_rate_line = entry.rate_line;
            entry.rate_line = rate_line;
        }

        if old_rate_line == 0 || rate_line == 0 {
            self.save_to_disk();
        }
    }

    pub fn set_rate_line_scoped(&self, scope: UserContextScope, rate_line: u64) {
        match scope {
            UserContextScope::User(user_id) => self.set_rate_line(user_id, rate_line),
            UserContextScope::GuildBot {
                guild_id,
                bot_user_id,
            } => self.set_guild_bot_rate_line(guild_id, bot_user_id, rate_line),
        }
    }

    pub fn set_cron_rate_line(&self, user_id: UserId, rate_line: u64) {
        let old_rate_line;
        {
            let mut entry = self
                .contexts
                .entry(user_id)
                .or_insert_with(|| UserContext::new(user_id));

            old_rate_line = entry.cron_rate_line;
            entry.cron_rate_line = rate_line;
        }

        if old_rate_line == 0 || rate_line == 0 {
            self.save_to_disk();
        }
    }

    pub fn set_guild_bot_cron_rate_line(
        &self,
        guild_id: GuildId,
        bot_user_id: UserId,
        rate_line: u64,
    ) {
        let old_rate_line;
        {
            let mut entry = self
                .guild_bot_contexts
                .entry(guild_id)
                .or_insert_with(|| UserContext::new_system(bot_user_id));

            old_rate_line = entry.cron_rate_line;
            entry.cron_rate_line = rate_line;
        }

        if old_rate_line == 0 || rate_line == 0 {
            self.save_to_disk();
        }
    }

    pub fn set_cron_rate_line_scoped(&self, scope: UserContextScope, rate_line: u64) {
        match scope {
            UserContextScope::User(user_id) => self.set_cron_rate_line(user_id, rate_line),
            UserContextScope::GuildBot {
                guild_id,
                bot_user_id,
            } => self.set_guild_bot_cron_rate_line(guild_id, bot_user_id, rate_line),
        }
    }

    pub fn set_voice_speaker(&self, user_id: UserId, speaker: Option<u32>) {
        {
            let mut entry = self
                .contexts
                .entry(user_id)
                .or_insert_with(|| UserContext::new(user_id));
            entry.voice_speaker = speaker;
        }
        self.save_to_disk();
    }

    pub fn set_voice_speed_scale(&self, user_id: UserId, speed_scale: Option<f32>) {
        {
            let mut entry = self
                .contexts
                .entry(user_id)
                .or_insert_with(|| UserContext::new(user_id));
            entry.voice_speed_scale = speed_scale;
        }
        self.save_to_disk();
    }

    pub fn set_voice_pitch_scale(&self, user_id: UserId, pitch_scale: Option<f32>) {
        {
            let mut entry = self
                .contexts
                .entry(user_id)
                .or_insert_with(|| UserContext::new(user_id));
            entry.voice_pitch_scale = pitch_scale;
        }
        self.save_to_disk();
    }

    pub fn set_voice_pan(&self, user_id: UserId, pan: Option<f32>) {
        {
            let mut entry = self
                .contexts
                .entry(user_id)
                .or_insert_with(|| UserContext::new(user_id));
            entry.voice_pan = pan;
        }
        self.save_to_disk();
    }

    pub fn set_voice_dictionary_entry(
        &self,
        user_id: UserId,
        source: String,
        target: String,
        registered_by: Option<UserId>,
    ) -> Result<(usize, bool), String> {
        let (source, target) = prepare_entry(source, target)?;
        let result = {
            let mut entry = self
                .contexts
                .entry(user_id)
                .or_insert_with(|| UserContext::new(user_id));
            set_entry(
                &mut entry.voice_dictionary,
                source,
                target,
                registered_by,
                "user",
            )?
        };

        self.save_to_disk();
        Ok(result)
    }

    pub fn remove_voice_dictionary_entry(
        &self,
        user_id: UserId,
        source: &str,
    ) -> Result<(usize, Option<String>), String> {
        let source = prepare_source(source)?;
        let result = {
            let mut entry = self
                .contexts
                .entry(user_id)
                .or_insert_with(|| UserContext::new(user_id));
            remove_entry(&mut entry.voice_dictionary, &source)?
        };
        let (count, removed_target) = result;

        if removed_target.is_some() {
            self.save_to_disk();
        }

        Ok((count, removed_target))
    }

    pub fn voice_dictionary_entries(&self, user_id: UserId) -> Vec<(String, String)> {
        self.contexts
            .get(&user_id)
            .map(|entry| entries_as_pairs(&entry.voice_dictionary))
            .unwrap_or_default()
    }

    pub fn voice_dictionary_records(&self, user_id: UserId) -> Vec<VoiceDictionaryEntry> {
        self.contexts
            .get(&user_id)
            .map(|entry| entry.voice_dictionary.clone())
            .unwrap_or_default()
    }

    pub fn voice_dictionary_entries_scoped(
        &self,
        scope: UserContextScope,
    ) -> Vec<(String, String)> {
        match scope {
            UserContextScope::User(user_id) => self.voice_dictionary_entries(user_id),
            UserContextScope::GuildBot { guild_id, .. } => self
                .guild_bot_contexts
                .get(&guild_id)
                .map(|entry| entries_as_pairs(&entry.voice_dictionary))
                .unwrap_or_default(),
        }
    }

    pub fn voice_dictionary_entries_actor(
        &self,
        user_id: UserId,
        guild_id: Option<GuildId>,
        bot_user_id: UserId,
    ) -> Vec<(String, String)> {
        self.voice_dictionary_entries_scoped(UserContextScope::for_actor(
            user_id,
            guild_id,
            bot_user_id,
        ))
    }

    pub fn voice_dictionary_count(&self, user_id: UserId) -> usize {
        self.contexts
            .get(&user_id)
            .map(|entry| entry.voice_dictionary.len())
            .unwrap_or(0)
    }

    fn save_to_disk(&self) {
        let _save_guard = self
            .save_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let user_default_model_name = Models::USER_DEFAULT_MODEL.to_string();
        let system_model_name = Models::SYSTEM_MODEL.to_string();
        let entries = self
            .contexts
            .iter()
            .filter_map(|entry| user_context_to_json(entry.value(), &user_default_model_name))
            .collect::<Vec<Value>>();
        let guild_bot_entries = self
            .guild_bot_contexts
            .iter()
            .filter_map(|entry| {
                let mut obj = user_context_to_json(entry.value(), &system_model_name)?;
                obj["guild_id"] = json!(entry.key().get().to_string());
                Some(obj)
            })
            .collect::<Vec<Value>>();

        let doc = json!({
            "version": 2,
            "contexts": entries,
            "guild_bot_contexts": guild_bot_entries,
        });

        let body = match serde_json::to_string_pretty(&doc) {
            Ok(v) => v,
            Err(e) => {
                error!("failed to serialize user contexts: {}", e);
                return;
            }
        };

        if let Err(e) = crate::app::atomic_file::write(&self.store_path, body.as_bytes()) {
            error!("failed to write user contexts: {}", e);
        }
    }

    fn load_from_disk(&mut self) {
        let text = match fs::read_to_string(&self.store_path) {
            Ok(v) => v,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!("failed to read user contexts file: {}", e);
                }
                return;
            }
        };

        let doc: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                warn!("failed to parse user contexts file: {}", e);
                return;
            }
        };

        if let Some(contexts) = doc.get("contexts").and_then(Value::as_array) {
            for ctx in contexts {
                if let Some(user_context) = user_context_from_json(ctx, Models::USER_DEFAULT_MODEL)
                {
                    self.contexts.insert(user_context.user_id, user_context);
                }
            }
        }

        if let Some(contexts) = doc.get("guild_bot_contexts").and_then(Value::as_array) {
            for ctx in contexts {
                let Some(guild_id) = ctx.get("guild_id").and_then(parse_u64).map(GuildId::new)
                else {
                    continue;
                };

                if let Some(user_context) = user_context_from_json(ctx, Models::SYSTEM_MODEL) {
                    self.guild_bot_contexts.insert(guild_id, user_context);
                }
            }
        }
    }
}

impl Default for UserContexts {
    fn default() -> Self {
        Self::new()
    }
}

fn user_context_to_json(value: &UserContext, default_model_name: &str) -> Option<Value> {
    let model_name = value.main_model.to_string();
    if model_name == default_model_name
        && value.image_generation_model == ImageGenerationModels::DEFAULT
        && value.rate_line != 0
        && value.cron_rate_line != 0
        && value.voice_speaker.is_none()
        && value.voice_speed_scale.is_none()
        && value.voice_pitch_scale.is_none()
        && value.voice_pan.is_none()
        && value.voice_dictionary.is_empty()
    {
        return None;
    }

    let mut obj = json!({
        "user_id": value.user_id.get().to_string(),
        "main_model": model_name,
        "image_generation_model": value.image_generation_model.to_string(),
    });

    if value.rate_line == 0 {
        obj["rate_line"] = json!(0u64);
    }

    if value.cron_rate_line == 0 {
        obj["cron_rate_line"] = json!(0u64);
    }

    if let Some(speaker) = value.voice_speaker {
        obj["voice_speaker"] = json!(speaker);
    }

    if let Some(speed_scale) = value.voice_speed_scale {
        obj["voice_speed_scale"] = json!(speed_scale);
    }

    if let Some(pitch_scale) = value.voice_pitch_scale {
        obj["voice_pitch_scale"] = json!(pitch_scale);
    }

    if let Some(pan) = value.voice_pan {
        obj["voice_pan"] = json!(pan);
    }

    if !value.voice_dictionary.is_empty() {
        obj["voice_dictionary"] = json!(entries_to_json(&value.voice_dictionary));
    }

    Some(obj)
}

fn user_context_from_json(ctx: &Value, default_model: Models) -> Option<UserContext> {
    let user_id = ctx.get("user_id").and_then(parse_u64).map(UserId::new)?;
    let model = ctx
        .get("main_model")
        .and_then(Value::as_str)
        .and_then(Models::from_name)
        .unwrap_or(default_model);
    let image_generation_model = ctx
        .get("image_generation_model")
        .and_then(Value::as_str)
        .and_then(ImageGenerationModels::from_name)
        .unwrap_or_default();
    let rate_line = ctx.get("rate_line").and_then(Value::as_u64).unwrap_or(1);
    let cron_rate_line = ctx
        .get("cron_rate_line")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let voice_speaker = ctx
        .get("voice_speaker")
        .and_then(parse_u64)
        .and_then(|v| u32::try_from(v).ok());
    let voice_speed_scale = ctx
        .get("voice_speed_scale")
        .and_then(Value::as_f64)
        .map(|v| v as f32);
    let voice_pitch_scale = ctx
        .get("voice_pitch_scale")
        .and_then(Value::as_f64)
        .map(|v| v as f32);
    let voice_pan = ctx
        .get("voice_pan")
        .and_then(Value::as_f64)
        .map(|v| v as f32);
    let voice_dictionary = entries_from_json(ctx.get("voice_dictionary"));

    Some(UserContext {
        user_id,
        main_model: model,
        image_generation_model,
        rate_line,
        cron_rate_line,
        voice_speaker,
        voice_speed_scale,
        voice_pitch_scale,
        voice_pan,
        voice_dictionary,
    })
}
