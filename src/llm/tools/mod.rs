use std::collections::HashMap;

use crate::llm::tool::LMTool;

pub mod cron;
pub mod discord;
pub mod discord_reply;
pub mod file;
pub mod get_time;
pub mod latex;
pub mod modal_builder;
pub mod model_control;
pub mod typst;
pub mod voicevox;

pub(super) fn get_str_arg<'a>(
    args: &'a serde_json::Value,
    key: &'a str,
) -> Result<&'a str, String> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("Missing or invalid '{key}' parameter"))
}

pub(super) fn get_non_empty_trimmed_str_arg<'a>(
    args: &'a serde_json::Value,
    key: &'a str,
) -> Result<&'a str, String> {
    let value = get_str_arg(args, key)?.trim();
    if value.is_empty() {
        Err(format!("Missing or invalid '{key}' parameter"))
    } else {
        Ok(value)
    }
}

pub(super) fn get_opt_str_arg<'a>(args: &'a serde_json::Value, key: &'a str) -> Option<&'a str> {
    args.get(key).and_then(serde_json::Value::as_str)
}

pub(super) fn get_opt_non_empty_trimmed_str_arg<'a>(
    args: &'a serde_json::Value,
    key: &'a str,
) -> Option<&'a str> {
    get_opt_str_arg(args, key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub fn registry() -> HashMap<String, Box<dyn LMTool>> {
    let tools: Vec<Box<dyn LMTool>> = vec![
        Box::new(get_time::GetTime::new()),
        Box::new(cron::CronTool::new()),
        Box::new(discord::DiscordTool::new()),
        Box::new(discord_reply::DiscordReplyTool),
        Box::new(file::FileTool::new()),
        Box::new(typst::TypstRenderTool::new()),
        Box::new(typst::TypstPublishTool::new()),
        Box::new(latex::LatexExprRenderTool::new()),
        Box::new(modal_builder::ModalBuilderTool::new()),
        Box::new(model_control::ModelControlTool::new()),
        Box::new(voicevox::VoicevoxTool::new()),
    ];

    tools.into_iter().map(|tool| (tool.name(), tool)).collect()
}
