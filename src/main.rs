use log::{error, info};
use nelfie::app::context::NelfieContext;
use nelfie::app::tcp_monitor::TcpMonitorServer;
use nelfie::discord::bot;
use tokio::sync::watch;

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    nelfie::logging::init();
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    // コンテキスト初期化
    let ob_ctx = NelfieContext::new().await;
    let monitor_server = match TcpMonitorServer::bind(&ob_ctx.config.monitor_tcp_bind).await {
        Ok(server) => server,
        Err(error) => {
            error!(
                "startup failed: component=tcp_monitor bind={} error={error}",
                ob_ctx.config.monitor_tcp_bind
            );
            return;
        }
    };
    let (monitor_shutdown_tx, monitor_shutdown_rx) = watch::channel(false);
    let monitor_task = tokio::spawn(monitor_server.run(ob_ctx.clone(), monitor_shutdown_rx));

    if let Err(e) = ob_ctx.initialize_before_bot_start().await {
        error!("startup failed: component=voicevox error={e}");
        let _ = monitor_shutdown_tx.send(true);
        let _ = monitor_task.await;
        return;
    }
    ob_ctx.monitoring.set_voicevox_ready(true);

    if let Err(e) = bot::start(ob_ctx.clone()).await {
        error!("startup failed: component=discord error={e}");
        let _ = monitor_shutdown_tx.send(true);
        let _ = monitor_task.await;
        return;
    }

    info!("Nelfie started; waiting for Ctrl-C");
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!("shutdown signal listener failed: {e}");
    }
    info!("Shutdown requested");
    let _ = monitor_shutdown_tx.send(true);

    if let Err(e) = ob_ctx.shutdown().await {
        error!("shutdown failed: {e}");
    }
    if let Err(e) = monitor_task.await {
        error!("TCP monitoring server task failed: {e}");
    }
    info!("Shutdown complete");
}
