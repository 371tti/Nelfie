use std::{io, sync::Arc, time::Duration};

use log::{debug, info, warn};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::timeout,
};

use crate::app::context::NelfieContext;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REQUEST_BYTES: usize = 64;
const MAX_CONCURRENT_CLIENTS: usize = 16;

pub struct TcpMonitorServer {
    listener: TcpListener,
}

impl TcpMonitorServer {
    pub async fn bind(address: &str) -> io::Result<Self> {
        let listener = TcpListener::bind(address).await?;
        info!(
            "TCP monitoring server listening on {}",
            listener.local_addr()?
        );
        Ok(Self { listener })
    }

    pub async fn run(self, context: NelfieContext, mut shutdown: watch::Receiver<bool>) {
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_CLIENTS));
        let mut clients = JoinSet::new();

        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                Some(result) = clients.join_next(), if !clients.is_empty() => {
                    if let Err(error) = result {
                        warn!("TCP monitoring client task failed: {error}");
                    }
                }
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                                debug!("TCP monitoring client rejected at capacity: peer={peer}");
                                drop(stream);
                                continue;
                            };
                            let context = context.clone();
                            clients.spawn(async move {
                                let _permit = permit;
                                if let Err(error) = handle_client(&context, stream).await {
                                    debug!("TCP monitoring request failed: peer={peer} error={error}");
                                }
                            });
                        }
                        Err(error) => {
                            warn!("TCP monitoring accept failed: {error}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        }

        clients.abort_all();
        while clients.join_next().await.is_some() {}
        info!("TCP monitoring server stopped");
    }
}

async fn handle_client(context: &NelfieContext, mut stream: TcpStream) -> io::Result<()> {
    let request = match timeout(REQUEST_TIMEOUT, read_request(&mut stream)).await {
        Ok(result) => result?,
        Err(_) => return Ok(()),
    };

    let response = match request {
        MonitorRequest::Metrics => render_metrics(context).await,
        MonitorRequest::Invalid => {
            "status.code 2\nmonitor.command.unit boolean\nmonitor.command.valid 0\n\n".into()
        }
        MonitorRequest::Empty => return Ok(()),
    };

    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

enum MonitorRequest {
    Metrics,
    Invalid,
    Empty,
}

async fn read_request(stream: &mut TcpStream) -> io::Result<MonitorRequest> {
    let mut request = Vec::with_capacity(MAX_REQUEST_BYTES);
    let mut buffer = [0_u8; 32];

    loop {
        let bytes_read = stream.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }

        let newline = buffer[..bytes_read].iter().position(|byte| *byte == b'\n');
        let bytes_to_append = newline.map_or(bytes_read, |index| index + 1);
        if request.len() + bytes_to_append > MAX_REQUEST_BYTES {
            return Ok(MonitorRequest::Invalid);
        }
        request.extend_from_slice(&buffer[..bytes_to_append]);

        if let Some(index) = newline {
            if index + 1 != bytes_read {
                return Ok(MonitorRequest::Invalid);
            }
            break;
        }
    }

    if request.is_empty() {
        return Ok(MonitorRequest::Empty);
    }

    let command = request
        .strip_suffix(b"\r\n")
        .or_else(|| request.strip_suffix(b"\n"))
        .unwrap_or(&request);
    if command == b"METRICS" {
        Ok(MonitorRequest::Metrics)
    } else {
        Ok(MonitorRequest::Invalid)
    }
}

async fn render_metrics(context: &NelfieContext) -> String {
    let status_code = context.monitoring.status_code();
    let gateway_ready = u8::from(context.monitoring.gateway_ready());
    let gateway_reconnecting = u8::from(context.monitoring.gateway_reconnecting());
    let voicevox_ready = u8::from(context.monitoring.voicevox_ready());
    let gateway_latency_max_ms = if context.monitoring.gateway_ready() {
        context.discord_client.gateway_latency_max_ms().await
    } else {
        None
    };
    let guild_count = context
        .discord_client
        .try_open()
        .map(|handles| handles.cache.guild_count())
        .unwrap_or_default();
    let connected_voice_guilds = context.voice_system.connected_voice_guild_ids().len();
    let uptime_seconds = context.monitoring.uptime().as_secs();

    let mut metrics = format!(
        "status.code {status_code}\n\
         nelfie.uptime.unit s\n\
         nelfie.uptime.seconds {uptime_seconds}\n\
         discord.gateway.unit boolean\n\
         discord.gateway.ready {gateway_ready}\n\
         discord.gateway.reconnecting {gateway_reconnecting}\n"
    );
    if let Some(latency_ms) = gateway_latency_max_ms {
        metrics.push_str(&format!(
            "discord.gateway.heartbeat_rtt.unit ms\n\
             discord.gateway.heartbeat_rtt.max {latency_ms}\n\
             response.unit ms\n\
             response.rtt {latency_ms}\n"
        ));
    }
    metrics.push_str(&format!(
        "discord.guilds.unit guilds\n\
         discord.guilds.count {guild_count}\n\
         nelfie.voice.unit guilds\n\
         nelfie.voice.connected_guilds {connected_voice_guilds}\n\
         voicevox.core.unit boolean\n\
         voicevox.core.ready {voicevox_ready}\n\n"
    ));
    metrics
}
