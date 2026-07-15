use std::collections::HashMap;

use crate::llm::tool::LMTool;

pub mod cron;
pub mod discord;
pub mod get_time;
pub mod latex;
pub mod modal_builder;
pub mod voicevox;

pub fn registry() -> HashMap<String, Box<dyn LMTool>> {
    let tools: Vec<Box<dyn LMTool>> = vec![
        Box::new(get_time::GetTime::new()),
        Box::new(cron::CronTool::new()),
        Box::new(discord::DiscordTool::new()),
        Box::new(latex::LatexExprRenderTool::new()),
        Box::new(modal_builder::ModalBuilderTool::new()),
        Box::new(voicevox::VoicevoxTool::new()),
    ];

    tools.into_iter().map(|tool| (tool.name(), tool)).collect()
}
