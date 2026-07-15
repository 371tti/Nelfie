use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use dashmap::DashMap;
use log::info;
use serenity::all::ChannelId;
use tokio::task::AbortHandle;

use crate::{
    app::{config::Config, cron::CronScheduler},
    discord::client::DiscordClientContext,
    llm::channel::ChatContexts,
    llm::tools,
    llm::user::UserContexts,
    llm::{client::LMClient, tool::LMTool},
    voice::{VoiceCoreConfig, VoiceSystem},
};

/// 全体共有コンテキスト
/// Arcで実装されてるのでcloneは単に参照カウントの増加
#[derive(Clone)]
pub struct NelfieContext {
    pub lm_client: Arc<LMClient>,
    pub config: Arc<Config>,
    pub chat_contexts: Arc<ChatContexts>,
    pub user_contexts: Arc<UserContexts>,
    pub voice_system: Arc<VoiceSystem>,
    pub tools: Arc<HashMap<String, Box<dyn LMTool>>>,
    pub discord_client: Arc<DiscordClientContext>,
    pub cron_scheduler: Arc<CronScheduler>,
    pub responding_channels: Arc<DashMap<ChannelId, bool>>,
    pub active_responses: Arc<DashMap<ChannelId, ActiveResponse>>,
    pub response_seq: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct ActiveResponse {
    pub request_id: u64,
    pub abort_handle: AbortHandle,
}

impl NelfieContext {
    pub async fn new() -> NelfieContext {
        let config = Config::new();
        let voice_system = VoiceSystem::new(
            config.voicevox_default_speaker,
            VoiceCoreConfig {
                acceleration_mode: config.voicevox_core_acceleration.clone(),
                cpu_threads: config.voicevox_core_cpu_threads,
                load_all_models: config.voicevox_core_load_all_models,
                output_sampling_rate: config.voicevox_output_sampling_rate,
                open_jtalk_dict_dir: config.voicevox_open_jtalk_dict_dir.clone(),
                vvm_dir: config.voicevox_vvm_dir.clone(),
                onnxruntime_filename: config.voicevox_onnxruntime_filename.clone(),
            },
        );

        let openai_config = OpenAIConfig::new().with_api_key(config.openai_api_key.clone());
        let lm_client = LMClient::new(OpenAIClient::with_config(openai_config));
        let tools = tools::registry();

        NelfieContext {
            lm_client: Arc::new(lm_client),
            config: Arc::new(config.clone()),
            chat_contexts: Arc::new(ChatContexts::new(config.system_prompt.clone())),
            user_contexts: Arc::new(UserContexts::new()),
            voice_system: Arc::new(voice_system),
            tools: Arc::new(tools),
            discord_client: Arc::new(DiscordClientContext::default()),
            cron_scheduler: Arc::new(CronScheduler::new()),
            responding_channels: Arc::new(DashMap::new()),
            active_responses: Arc::new(DashMap::new()),
            response_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    pub async fn initialize_before_bot_start(&self) -> Result<(), String> {
        self.voice_system.initialize_on_startup().await
    }

    pub async fn shutdown(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for active in self.active_responses.iter() {
            active.abort_handle.abort();
        }
        self.active_responses.clear();
        self.responding_channels.clear();
        self.cron_scheduler.stop();
        self.voice_system.clear_all();

        self.response_seq.store(1, Ordering::Relaxed);
        info!("Shutting down NelfieContext...");
        Ok(())
    }
}
