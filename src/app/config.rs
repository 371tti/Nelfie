use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::llm::prompt::DEFAULT_SYSTEM_PROMPT;

const CONFIG_FILE: &str = "config.yaml";

/// Runtime configuration. Credentials are read only from environment variables.
#[derive(Clone)]
pub struct Config {
    pub discord_token: String,
    pub discord_presence_intent_enabled: bool,
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
    pub voicevox_output_sampling_rate: u32,
    pub voicevox_open_jtalk_dict_dir: String,
    pub voicevox_vvm_dir: String,
    pub voicevox_onnxruntime_filename: String,
    pub monitor_tcp_bind: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    discord_presence_intent_enabled: bool,
    system_prompt: String,
    rate_limit_window_size: u64,
    rate_limit_sec_per_cost: u64,
    cron_rate_limit_window_size: u64,
    cron_rate_limit_sec_per_run: u64,
    admin_users: Vec<u64>,
    timeout_millis: u64,
    context_compaction_token_limit: u32,
    context_summary_max_output_tokens: u32,
    voicevox_default_speaker: u32,
    voicevox_core_acceleration: String,
    voicevox_core_cpu_threads: u16,
    voicevox_output_sampling_rate: u32,
    voicevox_open_jtalk_dict_dir: String,
    voicevox_vvm_dir: String,
    voicevox_onnxruntime_filename: String,
    monitor_tcp_bind: String,
}

impl FileConfig {
    fn load_or_create() -> Self {
        let path = Path::new(CONFIG_FILE);
        if !path.exists() {
            let defaults = Self::default();
            let yaml = serde_yaml_ng::to_string(&defaults)
                .expect("failed to serialize default config.yaml");
            let contents = format!(
                "# Nelfie settings. Keep DISCORD_TOKEN and OPENAI_API_KEY in the environment.\n{yaml}"
            );

            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(contents.as_bytes()) {
                        let _ = fs::remove_file(path);
                        panic!("failed to write {}: {error}", path.display());
                    }
                    log::info!("Created {} with default settings", path.display());
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("failed to create {}: {error}", path.display()),
            }
        }

        let contents = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let config: Self = serde_yaml_ng::from_str(&contents)
            .unwrap_or_else(|error| panic!("invalid {}: {error}", path.display()));
        config
            .validate()
            .unwrap_or_else(|error| panic!("invalid {}: {error}", path.display()));
        config
    }

    fn validate(&self) -> Result<(), String> {
        if self.rate_limit_window_size == 0
            || self.rate_limit_sec_per_cost == 0
            || self.cron_rate_limit_window_size == 0
            || self.cron_rate_limit_sec_per_run == 0
        {
            return Err("rate limit values must be greater than zero".to_string());
        }
        if !(30_000..=600_000).contains(&self.timeout_millis) {
            return Err("timeout_millis must be between 30000 and 600000".to_string());
        }
        if self.context_compaction_token_limit == 0 {
            return Err("context_compaction_token_limit must be greater than zero".to_string());
        }
        if self.context_summary_max_output_tokens == 0 {
            return Err("context_summary_max_output_tokens must be greater than zero".to_string());
        }
        if !(8_000..=96_000).contains(&self.voicevox_output_sampling_rate)
            || self.voicevox_output_sampling_rate % 24_000 != 0
        {
            return Err(
                "voicevox_output_sampling_rate must be 8000–96000 and divisible by 24000"
                    .to_string(),
            );
        }
        Ok(())
    }
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            discord_presence_intent_enabled: false,
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_owned(),
            rate_limit_window_size: 16_200,
            rate_limit_sec_per_cost: 600,
            cron_rate_limit_window_size: 3_600,
            cron_rate_limit_sec_per_run: 3_600,
            admin_users: vec![855371530270408725],
            timeout_millis: 300_000,
            context_compaction_token_limit: 64_000,
            context_summary_max_output_tokens: 6_144,
            voicevox_default_speaker: 3,
            voicevox_core_acceleration: "auto".to_string(),
            voicevox_core_cpu_threads: 0,
            voicevox_output_sampling_rate: 48_000,
            voicevox_open_jtalk_dict_dir: "voicevox_core/dict/open_jtalk_dic_utf_8-1.11"
                .to_string(),
            voicevox_vvm_dir: "voicevox_core/models/vvms".to_string(),
            voicevox_onnxruntime_filename: String::new(),
            monitor_tcp_bind: "127.0.0.1:9791".to_string(),
        }
    }
}

impl Config {
    pub fn new() -> Self {
        let file = FileConfig::load_or_create();
        let discord_token = std::env::var("DISCORD_TOKEN")
            .expect("DISCORD_TOKEN must be set in the environment or .env file");
        let openai_api_key = std::env::var("OPENAI_API_KEY")
            .expect("OPENAI_API_KEY must be set in the environment or .env file");

        Self {
            discord_token,
            discord_presence_intent_enabled: file.discord_presence_intent_enabled,
            openai_api_key,
            system_prompt: file.system_prompt,
            rate_limit_window_size: file.rate_limit_window_size,
            rate_limit_sec_per_cost: file.rate_limit_sec_per_cost,
            cron_rate_limit_window_size: file.cron_rate_limit_window_size,
            cron_rate_limit_sec_per_run: file.cron_rate_limit_sec_per_run,
            admin_users: file.admin_users,
            timeout_millis: file.timeout_millis,
            context_compaction_token_limit: file.context_compaction_token_limit,
            context_summary_max_output_tokens: file.context_summary_max_output_tokens,
            voicevox_default_speaker: file.voicevox_default_speaker,
            voicevox_core_acceleration: file.voicevox_core_acceleration,
            voicevox_core_cpu_threads: file.voicevox_core_cpu_threads,
            voicevox_output_sampling_rate: file.voicevox_output_sampling_rate,
            voicevox_open_jtalk_dict_dir: file.voicevox_open_jtalk_dict_dir,
            voicevox_vvm_dir: file.voicevox_vvm_dir,
            voicevox_onnxruntime_filename: file.voicevox_onnxruntime_filename,
            monitor_tcp_bind: file.monitor_tcp_bind,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}
