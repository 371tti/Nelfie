use crate::llm::prompt::DEFAULT_SYSTEM_PROMPT;

/// 設定
/// まだserdeかいてないのでそのままinlineで記述してる
#[derive(Clone)]
pub struct Config {
    pub discord_token: String,
    pub openai_api_key: String,
    pub system_prompt: String,
    pub rate_limit_window_size: u64,
    pub rate_limit_sec_per_cost: u64,
    pub cron_rate_limit_window_size: u64,
    pub cron_rate_limit_sec_per_run: u64,
    pub admin_users: Vec<u64>,
    pub timeout_millis: u64,
    pub context_compaction_token_limit: u32,
    pub context_summary_max_output_tokens: u32,
    pub voicevox_default_speaker: u32,
    pub voicevox_core_acceleration: String,
    pub voicevox_core_cpu_threads: u16,
    pub voicevox_core_load_all_models: bool,
    pub voicevox_output_sampling_rate: u32,
    pub voicevox_preload_on_startup: bool,
    pub voicevox_open_jtalk_dict_dir: String,
    pub voicevox_vvm_dir: String,
    pub voicevox_onnxruntime_filename: String,
}

impl Config {
    pub fn new() -> Self {
        let discord_token = std::env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN must be set");
        let openai_api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set");
        let system_prompt =
            std::env::var("SYSTEM_PROMPT").unwrap_or_else(|_| DEFAULT_SYSTEM_PROMPT.to_owned());
        let context_compaction_token_limit = std::env::var("CONTEXT_COMPACTION_TOKEN_LIMIT")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(64_000);
        let context_summary_max_output_tokens = std::env::var("CONTEXT_SUMMARY_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(4_096);
        let voicevox_default_speaker = std::env::var("VOICEVOX_DEFAULT_SPEAKER")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(3);
        let voicevox_core_acceleration =
            std::env::var("VOICEVOX_CORE_ACCELERATION").unwrap_or_else(|_| "auto".to_string());
        let voicevox_core_cpu_threads = std::env::var("VOICEVOX_CORE_CPU_THREADS")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(0);
        let voicevox_core_load_all_models = std::env::var("VOICEVOX_CORE_LOAD_ALL_MODELS")
            .ok()
            .map(|v| {
                let normalized = v.trim().to_ascii_lowercase();
                matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false);
        // 24000 の倍数で、8000以上96000以下の値じゃないと怒られるっぽい (VOICEVOXの仕様)
        let voicevox_output_sampling_rate = std::env::var("VOICEVOX_OUTPUT_SAMPLING_RATE")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v >= 8_000 && *v <= 96_000 && *v % 24_000 == 0)
            .unwrap_or(48_000);
        let voicevox_preload_on_startup = std::env::var("VOICEVOX_PRELOAD_ON_STARTUP")
            .ok()
            .map(|v| {
                let normalized = v.trim().to_ascii_lowercase();
                matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(true);
        let voicevox_open_jtalk_dict_dir = std::env::var("VOICEVOX_OPEN_JTALK_DICT_DIR")
            .unwrap_or_else(|_| "voicevox_core/dict/open_jtalk_dic_utf_8-1.11".to_string());
        let voicevox_vvm_dir = std::env::var("VOICEVOX_VVM_DIR")
            .unwrap_or_else(|_| "voicevox_core/models/vvms".to_string());
        let voicevox_onnxruntime_filename =
            std::env::var("VOICEVOX_ONNXRUNTIME_FILENAME").unwrap_or_else(|_| "".to_string());

        Config {
            discord_token,
            openai_api_key,
            system_prompt,
            rate_limit_window_size: 16200,
            rate_limit_sec_per_cost: 600,
            cron_rate_limit_window_size: 3600,
            cron_rate_limit_sec_per_run: 3600,
            admin_users: vec![855371530270408725],
            timeout_millis: 100_000,
            context_compaction_token_limit,
            context_summary_max_output_tokens,
            voicevox_default_speaker,
            voicevox_core_acceleration,
            voicevox_core_cpu_threads,
            voicevox_core_load_all_models,
            voicevox_output_sampling_rate,
            voicevox_preload_on_startup,
            voicevox_open_jtalk_dict_dir,
            voicevox_vvm_dir,
            voicevox_onnxruntime_filename,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}
