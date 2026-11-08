use serde_json::{Value, json};
use serenity::all::UserId;

use super::parse_u64;

pub const VOICE_DICTIONARY_MAX_ENTRIES: usize = 512;

#[derive(Clone, Debug)]
pub struct VoiceDictionaryEntry {
    pub source: String,
    pub target: String,
    pub registered_by: Option<UserId>,
    pub updated_by: Option<UserId>,
}

impl VoiceDictionaryEntry {
    pub fn from_json_value(value: &Value) -> Option<Self> {
        let source = value
            .get("source")
            .or_else(|| value.get("key"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())?
            .to_string();
        let target = value
            .get("target")
            .or_else(|| value.get("value"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())?
            .to_string();

        Some(Self {
            source,
            target,
            registered_by: value
                .get("registered_by")
                .and_then(parse_u64)
                .map(UserId::new),
            updated_by: value.get("updated_by").and_then(parse_u64).map(UserId::new),
        })
    }

    pub fn to_json_value(&self) -> Value {
        json!({
            "source": self.source,
            "target": self.target,
            "registered_by": self.registered_by.map(|user_id| user_id.get().to_string()),
            "updated_by": self.updated_by.map(|user_id| user_id.get().to_string()),
        })
    }
}

pub(crate) fn prepare_entry(source: String, target: String) -> Result<(String, String), String> {
    let source = source.trim().to_string();
    let target = target.trim().to_string();

    if source.is_empty() {
        return Err("'source' must not be empty".to_string());
    }
    if target.is_empty() {
        return Err("'target' must not be empty".to_string());
    }

    Ok((source, target))
}

pub(crate) fn prepare_source(source: &str) -> Result<String, String> {
    let source = source.trim();
    if source.is_empty() {
        Err("'source' must not be empty".to_string())
    } else {
        Ok(source.to_string())
    }
}

pub(crate) fn set_entry(
    entries: &mut Vec<VoiceDictionaryEntry>,
    source: String,
    target: String,
    registered_by: Option<UserId>,
    owner: &str,
) -> Result<(usize, bool), String> {
    let count = entries.len();
    if let Some(existing) = entries.iter_mut().find(|item| item.source == source) {
        existing.target = target;
        existing.updated_by = registered_by;
        return Ok((count, true));
    }

    if entries.len() >= VOICE_DICTIONARY_MAX_ENTRIES {
        return Err(format!(
            "voice dictionary limit reached for this {owner}: max {VOICE_DICTIONARY_MAX_ENTRIES} entries"
        ));
    }

    entries.push(VoiceDictionaryEntry {
        source,
        target,
        registered_by,
        updated_by: registered_by,
    });
    Ok((entries.len(), false))
}

pub(crate) fn remove_entry(
    entries: &mut Vec<VoiceDictionaryEntry>,
    source: &str,
) -> Result<(usize, Option<String>), String> {
    let removed_target = entries
        .iter()
        .position(|item| item.source == source)
        .map(|index| entries.remove(index).target);
    Ok((entries.len(), removed_target))
}

pub(crate) fn entries_as_pairs(entries: &[VoiceDictionaryEntry]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|item| (item.source.clone(), item.target.clone()))
        .collect()
}

pub(crate) fn entries_from_json(value: Option<&Value>) -> Vec<VoiceDictionaryEntry> {
    value
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(VoiceDictionaryEntry::from_json_value)
                .take(VOICE_DICTIONARY_MAX_ENTRIES)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn entries_to_json(entries: &[VoiceDictionaryEntry]) -> Vec<Value> {
    entries
        .iter()
        .map(VoiceDictionaryEntry::to_json_value)
        .collect()
}
