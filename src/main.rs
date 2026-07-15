use log::{error, info};
use nelfie::app::context::NelfieContext;
use nelfie::discord::bot;

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    nelfie::logging::init();
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    // コンテキスト初期化
    let ob_ctx = NelfieContext::new().await;
    if let Err(e) = ob_ctx.initialize_before_bot_start().await {
        error!("startup failed: component=voicevox error={e}");
        return;
    }

    if let Err(e) = bot::start(ob_ctx.clone()).await {
        error!("startup failed: component=discord error={e}");
        return;
    }

    info!("Nelfie started; waiting for Ctrl-C");
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!("shutdown signal listener failed: {e}");
    }
    info!("Shutdown requested");

    if let Err(e) = ob_ctx.shutdown().await {
        error!("shutdown failed: {e}");
    }
    info!("Shutdown complete");
}
