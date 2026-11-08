use crate::{app::context::NelfieContext, discord::commands};

pub type CommandError = Box<dyn std::error::Error + Send + Sync>;

pub fn all() -> Vec<poise::Command<NelfieContext, CommandError>> {
    vec![
        commands::ping(),
        commands::status(),
        commands::enable(),
        commands::clear(),
        commands::disable(),
        commands::model(),
        commands::tex_expr(),
        commands::typst_pdf(),
        commands::rate(),
        commands::cron(),
        commands::set_system_prompt(),
        commands::vc(),
    ]
}
