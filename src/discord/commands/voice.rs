mod dictionary;
mod playback;
mod speaker;
mod support;

pub use dictionary::{vc_dict, vc_dict_delete, vc_dict_user, vc_dict_user_delete};
pub use playback::{vc_autoread, vc_config, vc_download, vc_join, vc_leave, vc_say};
pub use speaker::{vc_speaker, vc_status};

use super::shared::{Context, Error};

/// VC関連の音声機能を操作します。
///
/// 接続、読み上げ、辞書、話者設定、状態確認を行います。
#[poise::command(
    slash_command,
    prefix_command,
    subcommands(
        "vc_join",
        "vc_leave",
        "vc_say",
        "vc_download",
        "vc_config",
        "vc_autoread",
        "vc_dict",
        "vc_dict_delete",
        "vc_dict_user",
        "vc_dict_user_delete",
        "vc_speaker",
        "vc_status"
    )
)]
pub async fn vc(_: Context<'_>) -> Result<(), Error> {
    Ok(())
}
