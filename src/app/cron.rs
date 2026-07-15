use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serenity::all::{ChannelId, GuildId, UserId};
use tokio::{
    task::AbortHandle,
    time::{MissedTickBehavior, interval},
};

use crate::{
    app::context::NelfieContext,
    discord::responses::{CronResponseRequest, schedule_cron_response},
};

mod schedule;
mod store;

use schedule::CronSchedule;
pub use schedule::normalize_cron_expression;
use store::CronJobStore;

const CRON_TICK_SECONDS: u64 = 30;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub guild_id: u64,
    pub channel_id: u64,
    pub schedule: String,
    pub prompt: String,
    pub created_by: u64,
    pub created_at: i64,
    pub last_run_at: Option<i64>,
    pub next_run_at: i64,
}

impl CronJob {
    pub fn guild_id(&self) -> GuildId {
        GuildId::new(self.guild_id)
    }

    pub fn channel_id(&self) -> ChannelId {
        ChannelId::new(self.channel_id)
    }

    pub fn created_by(&self) -> UserId {
        UserId::new(self.created_by)
    }
}

#[derive(Clone)]
pub struct CronScheduler {
    jobs: Arc<DashMap<String, CronJob>>,
    store: Arc<CronJobStore>,
    started: Arc<AtomicBool>,
    task_handle: Arc<RwLock<Option<AbortHandle>>>,
    seq: Arc<AtomicU64>,
}

impl CronScheduler {
    pub fn new() -> Self {
        let scheduler = Self {
            jobs: Arc::new(DashMap::new()),
            store: Arc::new(CronJobStore::new()),
            started: Arc::new(AtomicBool::new(false)),
            task_handle: Arc::new(RwLock::new(None)),
            seq: Arc::new(AtomicU64::new(1)),
        };
        scheduler.load_from_disk();
        scheduler
    }

    pub fn register(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        created_by: UserId,
        schedule: String,
        prompt: String,
    ) -> Result<CronJob, String> {
        let schedule = normalize_cron_expression(&schedule)?;
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return Err("prompt must not be empty".to_string());
        }

        let parsed = CronSchedule::parse(&schedule)?;
        let now = Utc::now();
        let next_run_at = parsed.next_after(now)?.timestamp();
        let id = format!(
            "cron-{}-{}-{}",
            guild_id.get(),
            now.timestamp_millis(),
            self.seq.fetch_add(1, Ordering::Relaxed)
        );

        let job = CronJob {
            id: id.clone(),
            guild_id: guild_id.get(),
            channel_id: channel_id.get(),
            schedule,
            prompt,
            created_by: created_by.get(),
            created_at: now.timestamp(),
            last_run_at: None,
            next_run_at,
        };

        self.jobs.insert(id, job.clone());
        self.save_to_disk();
        Ok(job)
    }

    pub fn get_job(&self, id: &str) -> Option<CronJob> {
        self.jobs.get(id).map(|entry| entry.value().clone())
    }

    pub fn jobs_for_channel(&self, guild_id: GuildId, channel_id: ChannelId) -> Vec<CronJob> {
        let mut jobs = self
            .jobs
            .iter()
            .filter(|entry| {
                entry.guild_id == guild_id.get() && entry.channel_id == channel_id.get()
            })
            .map(|entry| entry.value().clone())
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| (job.created_at, job.id.clone()));
        jobs
    }

    pub fn jobs_for_guild(&self, guild_id: GuildId) -> Vec<CronJob> {
        let mut jobs = self
            .jobs
            .iter()
            .filter(|entry| entry.guild_id == guild_id.get())
            .map(|entry| entry.value().clone())
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| (job.channel_id, job.created_at, job.id.clone()));
        jobs
    }

    pub fn delete_job(&self, id: &str, guild_id: Option<GuildId>) -> Result<CronJob, String> {
        let id = id.trim();
        if id.is_empty() {
            return Err("cron id must not be empty".to_string());
        }

        if let Some(guild_id) = guild_id {
            let Some(existing) = self.jobs.get(id) else {
                return Err(format!("cron '{id}' is not registered"));
            };

            if existing.guild_id != guild_id.get() {
                return Err(format!("cron '{id}' is not registered in this guild"));
            }
        }

        let Some((_, removed)) = self.jobs.remove(id) else {
            return Err(format!("cron '{id}' is not registered"));
        };

        self.save_to_disk();
        Ok(removed)
    }

    pub fn start(&self, ctx: serenity::client::Context, ob_ctx: NelfieContext) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }

        let scheduler = self.clone();
        let task = tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(CRON_TICK_SECONDS));
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

            loop {
                tick.tick().await;
                scheduler.run_due(&ctx, &ob_ctx).await;
            }
        });

        if let Ok(mut handle) = self.task_handle.write() {
            *handle = Some(task.abort_handle());
        }

        info!("Cron scheduler started");
    }

    pub fn stop(&self) {
        if let Ok(mut handle) = self.task_handle.write()
            && let Some(handle) = handle.take()
        {
            handle.abort();
        }
        self.started.store(false, Ordering::SeqCst);
    }

    pub fn dispatch_job(
        &self,
        ctx: serenity::client::Context,
        ob_ctx: NelfieContext,
        job: CronJob,
        manual: bool,
    ) {
        if manual {
            self.touch_last_run(&job.id);
        }

        let guild_id = job.guild_id();
        let channel_id = job.channel_id();
        schedule_cron_response(
            &ctx,
            &ob_ctx,
            CronResponseRequest {
                cron_id: job.id,
                schedule: job.schedule,
                prompt: job.prompt,
                manual,
                guild_id,
                channel_id,
            },
        );
    }

    async fn run_due(&self, ctx: &serenity::client::Context, ob_ctx: &NelfieContext) {
        let now = Utc::now();
        let due_ids = self
            .jobs
            .iter()
            .filter(|entry| entry.next_run_at <= now.timestamp())
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();

        for id in due_ids {
            let Some(job) = self.mark_due_job_started(&id, now) else {
                continue;
            };

            self.dispatch_job(ctx.clone(), ob_ctx.clone(), job, false);
        }
    }

    fn mark_due_job_started(&self, id: &str, now: DateTime<Utc>) -> Option<CronJob> {
        let job = {
            let mut entry = self.jobs.get_mut(id)?;
            if entry.next_run_at > now.timestamp() {
                return None;
            }

            let parsed = match CronSchedule::parse(&entry.schedule) {
                Ok(parsed) => parsed,
                Err(error) => {
                    warn!(
                        "cron job {} has invalid schedule '{}': {}",
                        id, entry.schedule, error
                    );
                    return None;
                }
            };

            let next_run_at = match parsed.next_after(now) {
                Ok(next) => next.timestamp(),
                Err(error) => {
                    warn!("cron job {id} cannot compute next run: {error}");
                    return None;
                }
            };

            entry.last_run_at = Some(now.timestamp());
            entry.next_run_at = next_run_at;
            entry.clone()
        };

        self.save_to_disk();
        Some(job)
    }

    fn touch_last_run(&self, id: &str) {
        let touched = {
            let Some(mut entry) = self.jobs.get_mut(id) else {
                return;
            };
            entry.last_run_at = Some(Utc::now().timestamp());
            true
        };

        if touched {
            self.save_to_disk();
        }
    }

    fn save_to_disk(&self) {
        let mut jobs = self
            .jobs
            .iter()
            .map(|entry| entry.value().clone())
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| (job.guild_id, job.channel_id, job.created_at, job.id.clone()));

        if let Err(error) = self.store.save(jobs) {
            error!("{error}");
        }
    }

    fn load_from_disk(&self) {
        let jobs = match self.store.load() {
            Ok(jobs) => jobs,
            Err(error) => {
                warn!("{error}");
                return;
            }
        };

        let now = Utc::now();
        let mut changed = false;
        for mut job in jobs {
            let schedule = match normalize_cron_expression(&job.schedule) {
                Ok(schedule) => schedule,
                Err(error) => {
                    warn!(
                        "skipping cron job {} with invalid expression: {}",
                        job.id, error
                    );
                    changed = true;
                    continue;
                }
            };

            let parsed = match CronSchedule::parse(&schedule) {
                Ok(parsed) => parsed,
                Err(error) => {
                    warn!(
                        "skipping cron job {} with invalid schedule: {}",
                        job.id, error
                    );
                    changed = true;
                    continue;
                }
            };

            if schedule != job.schedule {
                job.schedule = schedule;
                changed = true;
            }

            if job.next_run_at <= now.timestamp() {
                match parsed.next_after(now) {
                    Ok(next) => {
                        job.next_run_at = next.timestamp();
                        changed = true;
                    }
                    Err(error) => {
                        warn!("skipping cron job {} without future run: {}", job.id, error);
                        changed = true;
                        continue;
                    }
                }
            }

            self.jobs.insert(job.id.clone(), job);
        }

        if changed {
            self.save_to_disk();
        }
    }
}

impl Default for CronScheduler {
    fn default() -> Self {
        Self::new()
    }
}
