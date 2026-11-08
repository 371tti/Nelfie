pub mod channel;
pub mod client;
pub mod compaction;
pub mod context;
pub mod models;
pub mod prompt;
pub mod tool;
pub mod tools;
pub mod user;
pub mod voice_dictionary;

pub(crate) fn parse_u64(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok()))
}
