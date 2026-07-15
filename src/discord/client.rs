use std::sync::{Arc, OnceLock};

use serenity::{cache::Cache, http::Http};

#[derive(Clone)]
pub struct DiscordClientHandles {
    pub http: Arc<Http>,
    pub cache: Arc<Cache>,
}

#[derive(Default)]
pub struct DiscordClientContext {
    handles: OnceLock<DiscordClientHandles>,
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
}
