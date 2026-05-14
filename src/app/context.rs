use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use async_openai::{Client as OpenAIClient, config::OpenAIConfig};
use dashmap::DashMap;
use log::info;
use serenity::{
    Client as DiscordClient,
    all::{ChannelId, GatewayIntents},
};
use songbird::SerenityInit;
use tokio::task::AbortHandle;

use crate::{
    app::config::Config,
    discord::commands::{
        clear, disable, enable, model, ping, rate_config, set_system_prompt, tex_expr, vc_autoread,
        vc_config, vc_dict, vc_dict_delete, vc_dict_user, vc_dict_user_delete, vc_download,
        vc_join, vc_leave, vc_say, vc_speaker, vc_status,
    },
    discord::events::event_handler,
    llm::channel::ChatContexts,
    llm::client::{LMClient, LMTool},
    llm::tools,
    llm::user::UserContexts,
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
    pub discord_client: Arc<DiscordContextWrapper>,
    pub pending_modals: Arc<DashMap<String, tools::modal_builder::PendingModalSpec>>,
    pub responding_channels: Arc<DashMap<ChannelId, bool>>,
    pub active_responses: Arc<DashMap<ChannelId, ActiveResponse>>,
    pub response_seq: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct ActiveResponse {
    pub request_id: u64,
    pub abort_handle: AbortHandle,
}

/// DiscordContext を全体共有するための頭の悪いラッパー
pub struct DiscordContextWrapper {
    pub inner: RwLock<Option<Arc<DisabledContextWrapperInner>>>,
}

impl DiscordContextWrapper {
    pub fn open(&self) -> Arc<DisabledContextWrapperInner> {
        self.inner
            .read()
            .expect("RWlock")
            .clone()
            .expect("inisializing")
            .clone()
    }
    pub fn lazy() -> DiscordContextWrapper {
        DiscordContextWrapper {
            inner: RwLock::new(None),
        }
    }
    pub fn set(&self, ctx: Arc<DisabledContextWrapperInner>) {
        let mut w = self.inner.write().expect("RWlock");
        *w = Some(ctx);
    }
}

// 上のinner
pub struct DisabledContextWrapperInner {
    pub http: Arc<serenity::http::Http>,
    pub cache: Arc<serenity::cache::Cache>,
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

        // ツールの定義
        let openai_config = OpenAIConfig::new().with_api_key(config.openai_api_key.clone());
        let lm_client = LMClient::new(OpenAIClient::with_config(openai_config));
        let tools: HashMap<String, Box<dyn LMTool>> = vec![
            Box::new(tools::get_time::GetTime::new()) as Box<dyn LMTool>,
            Box::new(tools::discord::DiscordTool::new()) as Box<dyn LMTool>,
            Box::new(tools::latex::LatexExprRenderTool::new()) as Box<dyn LMTool>,
            Box::new(tools::modal_builder::ModalBuilderTool::new()) as Box<dyn LMTool>,
            Box::new(tools::voicevox::VoicevoxTool::new()) as Box<dyn LMTool>,
        ]
        .into_iter()
        .map(|tool| (tool.name(), tool))
        .collect();

        NelfieContext {
            lm_client: Arc::new(lm_client),
            config: Arc::new(config.clone()),
            chat_contexts: Arc::new(ChatContexts::new(config.system_prompt.clone())),
            user_contexts: Arc::new(UserContexts::new()),
            voice_system: Arc::new(voice_system),
            tools: Arc::new(tools),
            discord_client: Arc::new(DiscordContextWrapper::lazy()),
            pending_modals: Arc::new(DashMap::new()),
            responding_channels: Arc::new(DashMap::new()),
            active_responses: Arc::new(DashMap::new()),
            response_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    pub async fn initialize_before_bot_start(&self) -> Result<(), String> {
        self.voice_system.initialize_on_startup().await
    }

    pub async fn start_discord(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("Starting Discord bot...");

        let ob_ctx = self.clone();
        let framework = poise::Framework::builder()
            .options(poise::FrameworkOptions {
                commands: vec![
                    ping(),
                    enable(),
                    clear(),
                    disable(),
                    model(),
                    tex_expr(),
                    rate_config(),
                    set_system_prompt(),
                    vc_join(),
                    vc_leave(),
                    vc_say(),
                    vc_download(),
                    vc_config(),
                    vc_autoread(),
                    vc_dict(),
                    vc_dict_delete(),
                    vc_dict_user(),
                    vc_dict_user_delete(),
                    vc_speaker(),
                    vc_status(),
                ],
                prefix_options: poise::PrefixFrameworkOptions {
                    prefix: Some("!".into()),
                    ..Default::default()
                },
                event_handler: |ctx, event, framework, data| {
                    Box::pin(event_handler(ctx, event, framework, data))
                },
                ..Default::default()
            })
            .setup(move |ctx, _ready, framework| {
                let ob_ctx = ob_ctx.clone();
                Box::pin(async move {
                    ob_ctx
                        .discord_client
                        .set(Arc::new(DisabledContextWrapperInner {
                            http: ctx.http.clone(),
                            cache: ctx.cache.clone(),
                        }));

                    if let Some(songbird_manager) = songbird::get(ctx).await {
                        ob_ctx.voice_system.set_songbird(songbird_manager);
                    }

                    poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                    println!("Bot is ready!");
                    Ok(ob_ctx)
                })
            })
            .build();

        let intents = GatewayIntents::GUILDS
            | GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::GUILD_MESSAGE_REACTIONS
            | GatewayIntents::GUILD_VOICE_STATES
            | GatewayIntents::MESSAGE_CONTENT;

        let discord_client = DiscordClient::builder(self.config.discord_token.clone(), intents)
            .register_songbird()
            .framework(framework);

        tokio::spawn(async move {
            let mut c = discord_client.await.expect("Error creating client");
            c.start().await.expect("Error starting client");
        });

        Ok(())
    }

    pub async fn shutdown(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for active in self.active_responses.iter() {
            active.abort_handle.abort();
        }
        self.active_responses.clear();
        self.responding_channels.clear();
        self.pending_modals.clear();
        self.voice_system.clear_all();

        self.response_seq.store(1, Ordering::Relaxed);
        info!("Shutting down NelfieContext...");
        Ok(())
    }
}
