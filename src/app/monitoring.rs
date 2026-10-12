use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct MonitoringState {
    started_at: Instant,
    voicevox_ready: AtomicBool,
    discord_ready: AtomicBool,
    discord_ever_ready: AtomicBool,
    discord_reconnecting: AtomicBool,
}

impl MonitoringState {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            voicevox_ready: AtomicBool::new(false),
            discord_ready: AtomicBool::new(false),
            discord_ever_ready: AtomicBool::new(false),
            discord_reconnecting: AtomicBool::new(false),
        }
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn set_voicevox_ready(&self, ready: bool) {
        self.voicevox_ready.store(ready, Ordering::Relaxed);
    }

    pub fn mark_gateway_ready(&self) {
        self.discord_ever_ready.store(true, Ordering::Relaxed);
        self.discord_ready.store(true, Ordering::Relaxed);
        self.discord_reconnecting.store(false, Ordering::Relaxed);
    }

    pub fn mark_gateway_reconnecting(&self) {
        self.discord_ready.store(false, Ordering::Relaxed);
        if self.discord_ever_ready.load(Ordering::Relaxed) {
            self.discord_reconnecting.store(true, Ordering::Relaxed);
        }
    }

    pub fn mark_gateway_disconnected(&self) {
        self.discord_ready.store(false, Ordering::Relaxed);
        self.discord_reconnecting.store(false, Ordering::Relaxed);
    }

    pub fn gateway_ready(&self) -> bool {
        self.discord_ready.load(Ordering::Relaxed)
    }

    pub fn gateway_reconnecting(&self) -> bool {
        self.discord_reconnecting.load(Ordering::Relaxed)
    }

    pub fn voicevox_ready(&self) -> bool {
        self.voicevox_ready.load(Ordering::Relaxed)
    }

    pub fn status_code(&self) -> u8 {
        if !self.voicevox_ready() {
            2
        } else if self.gateway_ready() {
            0
        } else if self.gateway_reconnecting() {
            1
        } else {
            2
        }
    }
}

impl Default for MonitoringState {
    fn default() -> Self {
        Self::new()
    }
}
