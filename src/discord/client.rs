use std::sync::{Arc, OnceLock};

use serenity::{cache::Cache, gateway::ShardManager, http::Http};

#[derive(Clone)]
pub struct DiscordClientHandles {
    pub http: Arc<Http>,
    pub cache: Arc<Cache>,
}

#[derive(Default)]
pub struct DiscordClientContext {
    handles: OnceLock<DiscordClientHandles>,
    shard_manager: OnceLock<Arc<ShardManager>>,
}

impl DiscordClientContext {
    pub fn install(&self, handles: DiscordClientHandles) {
        self.handles
            .set(handles)
            .unwrap_or_else(|_| panic!("Discord client context is already initialized"));
    }

    pub fn open(&self) -> DiscordClientHandles {
        self.handles
            .get()
            .expect("Discord client context is not initialized")
            .clone()
    }

    pub fn try_open(&self) -> Option<DiscordClientHandles> {
        self.handles.get().cloned()
    }

    pub fn install_shard_manager(&self, shard_manager: Arc<ShardManager>) {
        self.shard_manager
            .set(shard_manager)
            .unwrap_or_else(|_| panic!("Discord shard manager is already initialized"));
    }

    pub async fn gateway_latency_max_ms(&self) -> Option<u64> {
        let shard_manager = self.shard_manager.get()?;
        let runners = shard_manager.runners.lock().await;
        runners
            .values()
            .filter_map(|runner| runner.latency)
            .map(|latency| latency.as_millis().min(u64::MAX as u128) as u64)
            .max()
    }
}
