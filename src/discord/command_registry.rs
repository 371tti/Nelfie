use crate::{app::context::NelfieContext, discord::commands};

pub type CommandError = Box<dyn std::error::Error + Send + Sync>;

pub fn all() -> Vec<poise::Command<NelfieContext, CommandError>> {
    vec![
        commands::ping(),
        commands::enable(),
        commands::clear(),
        commands::disable(),
        commands::model(),
        commands::tex_expr(),
        commands::rate_config(),
        commands::rate_status(),
        commands::cron(),
        commands::cron_test(),
        commands::del_cron(),
        commands::set_system_prompt(),
        commands::vc_join(),
        commands::vc_leave(),
        commands::vc_say(),
        commands::vc_download(),
        commands::vc_config(),
        commands::vc_autoread(),
        commands::vc_dict(),
        commands::vc_dict_delete(),
        commands::vc_dict_user(),
        commands::vc_dict_user_delete(),
        commands::vc_speaker(),
        commands::vc_status(),
    ]
}
