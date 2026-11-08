mod core_runtime;
mod system;
mod text;
mod types;
pub mod voice_catalog;

pub use self::system::VoiceSystem;
pub use self::text::apply_tts_dictionaries;
pub use self::text::apply_tts_dictionary;
pub use self::text::build_tts_text_from_message;
pub use self::types::{
    GuildVoiceConfig, SpeakOptions, VoiceCoreConfig, VOICE_PARALLEL_COUNT_DEFAULT,
    VOICE_PARALLEL_COUNT_MAX,
};
pub(crate) use self::types::normalize_parallel_count;
