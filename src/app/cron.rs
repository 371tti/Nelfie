use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, TimeZone, Timelike, Utc};
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
    discord::events::{CronResponseRequest, schedule_cron_response},
};

const CRON_JOBS_STORE_PATH: &str = "data/runtime/cron_jobs.json";
const CRON_TICK_SECONDS: u64 = 30;
const MAX_LOOKAHEAD_MINUTES: i64 = 60 * 24 * 366 * 5;

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
    store_path: Arc<PathBuf>,
    started: Arc<AtomicBool>,
    task_handle: Arc<RwLock<Option<AbortHandle>>>,
    seq: Arc<AtomicU64>,
}

#[derive(Serialize, Deserialize)]
struct CronJobsStore {
    version: u32,
    jobs: Vec<CronJob>,
}

impl CronScheduler {
    pub fn new() -> Self {
        let scheduler = Self {
            jobs: Arc::new(DashMap::new()),
            store_path: Arc::new(PathBuf::from(CRON_JOBS_STORE_PATH)),
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
                Err(e) => {
                    warn!(
                        "cron job {} has invalid schedule '{}': {}",
                        id, entry.schedule, e
                    );
                    return None;
                }
            };

            let next_run_at = match parsed.next_after(now) {
                Ok(next) => next.timestamp(),
                Err(e) => {
                    warn!("cron job {} cannot compute next run: {}", id, e);
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

        let doc = CronJobsStore { version: 1, jobs };
        if let Some(parent) = self.store_path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            error!("failed to create cron jobs directory: {}", e);
            return;
        }

        let body = match serde_json::to_string_pretty(&doc) {
            Ok(body) => body,
            Err(e) => {
                error!("failed to serialize cron jobs: {}", e);
                return;
            }
        };

        if let Err(e) = fs::write(self.store_path.as_ref(), body) {
            error!("failed to write cron jobs: {}", e);
        }
    }

    fn load_from_disk(&self) {
        let text = match fs::read_to_string(self.store_path.as_ref()) {
            Ok(text) => text,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!("failed to read cron jobs file: {}", e);
                }
                return;
            }
        };

        let store: CronJobsStore = match serde_json::from_str(&text) {
            Ok(store) => store,
            Err(e) => {
                warn!("failed to parse cron jobs file: {}", e);
                return;
            }
        };

        let now = Utc::now();
        let mut changed = false;
        for mut job in store.jobs {
            let schedule = match normalize_cron_expression(&job.schedule) {
                Ok(schedule) => schedule,
                Err(e) => {
                    warn!(
                        "skipping cron job {} with invalid expression: {}",
                        job.id, e
                    );
                    changed = true;
                    continue;
                }
            };

            let parsed = match CronSchedule::parse(&schedule) {
                Ok(parsed) => parsed,
                Err(e) => {
                    warn!("skipping cron job {} with invalid schedule: {}", job.id, e);
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
                    Err(e) => {
                        warn!("skipping cron job {} without future run: {}", job.id, e);
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

#[derive(Clone, Debug)]
struct CronSchedule {
    minutes: CronField,
    hours: CronField,
    days: CronField,
    months: CronField,
    weekdays: CronField,
}

impl CronSchedule {
    fn parse(expr: &str) -> Result<Self, String> {
        let parts = expr.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 5 {
            return Err("cron expression must have 5 fields".to_string());
        }

        Ok(Self {
            minutes: CronField::parse(parts[0], 0, 59, &[])?,
            hours: CronField::parse(parts[1], 0, 23, &[])?,
            days: CronField::parse(parts[2], 1, 31, &[])?,
            months: CronField::parse(parts[3], 1, 12, MONTH_NAMES)?,
            weekdays: CronField::parse(parts[4], 0, 7, WEEKDAY_NAMES)?,
        })
    }

    fn next_after(&self, after: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
        let mut candidate = after
            .with_timezone(&Local)
            .with_second(0)
            .and_then(|v| v.with_nanosecond(0))
            .ok_or_else(|| "failed to truncate timestamp".to_string())?
            + ChronoDuration::minutes(1);

        for _ in 0..MAX_LOOKAHEAD_MINUTES {
            if self.matches(&candidate) {
                return Ok(candidate.with_timezone(&Utc));
            }
            candidate += ChronoDuration::minutes(1);
        }

        Err("cron expression has no matching time in the next 5 years".to_string())
    }

    fn matches<Tz: TimeZone>(&self, dt: &DateTime<Tz>) -> bool {
        if !self.minutes.matches(dt.minute()) {
            return false;
        }
        if !self.hours.matches(dt.hour()) {
            return false;
        }
        if !self.months.matches(dt.month()) {
            return false;
        }

        let day_matches = self.days.matches(dt.day());
        let weekday_matches = self.weekdays.matches(dt.weekday().num_days_from_sunday());

        if self.days.restricted && self.weekdays.restricted {
            day_matches || weekday_matches
        } else {
            day_matches && weekday_matches
        }
    }
}

#[derive(Clone, Debug)]
struct CronField {
    allowed: Vec<bool>,
    restricted: bool,
}

impl CronField {
    fn parse(
        input: &str,
        min: u32,
        max: u32,
        names: &[(&'static str, u32)],
    ) -> Result<Self, String> {
        let mut allowed = vec![false; (max + 1) as usize];
        let mut restricted = false;

        for raw_part in input.split(',') {
            let part = raw_part.trim();
            if part.is_empty() {
                return Err(format!("empty cron field part in '{}'", input));
            }

            let (base, step) = match part.split_once('/') {
                Some((base, step)) => {
                    let step = step
                        .parse::<u32>()
                        .map_err(|_| format!("invalid step '{}'", step))?;
                    if step == 0 {
                        return Err("cron step must be greater than 0".to_string());
                    }
                    (base, step)
                }
                None => (part, 1),
            };

            let is_wildcard = base == "*" || base == "?";
            let (start, end) = if is_wildcard {
                (min, max)
            } else if let Some((start, end)) = base.split_once('-') {
                (
                    parse_cron_value(start, min, max, names)?,
                    parse_cron_value(end, min, max, names)?,
                )
            } else {
                let value = parse_cron_value(base, min, max, names)?;
                (value, value)
            };

            if start > end {
                return Err(format!(
                    "cron range start {} is greater than {}",
                    start, end
                ));
            }

            restricted |= !is_wildcard || step > 1;

            let mut value = start;
            while value <= end {
                set_allowed(&mut allowed, min, max, value)?;
                match value.checked_add(step) {
                    Some(next) => value = next,
                    None => break,
                }
            }
        }

        if !allowed.iter().any(|v| *v) {
            return Err(format!("cron field '{}' allows no values", input));
        }

        Ok(Self {
            allowed,
            restricted,
        })
    }

    fn matches(&self, value: u32) -> bool {
        self.allowed.get(value as usize).copied().unwrap_or(false)
    }
}

pub fn normalize_cron_expression(input: &str) -> Result<String, String> {
    let parts = input.split_whitespace().collect::<Vec<_>>();
    match parts.len() {
        5 => {
            let normalized = parts.join(" ");
            CronSchedule::parse(&normalized)?;
            Ok(normalized)
        }
        6 if parts[0] == "0" => {
            let normalized = parts[1..].join(" ");
            CronSchedule::parse(&normalized)?;
            Ok(normalized)
        }
        6 => Err("seconds field is only supported when it is exactly 0".to_string()),
        _ => {
            Err("cron expression must be 5 fields, or 6 fields with leading 0 seconds".to_string())
        }
    }
}

fn set_allowed(allowed: &mut [bool], min: u32, max: u32, value: u32) -> Result<(), String> {
    if value < min || value > max {
        return Err(format!(
            "cron value {} is outside allowed range {}..={}",
            value, min, max
        ));
    }

    if max == 7 && value == 7 {
        allowed[0] = true;
    }

    allowed[value as usize] = true;
    Ok(())
}

fn parse_cron_value(
    raw: &str,
    min: u32,
    max: u32,
    names: &[(&'static str, u32)],
) -> Result<u32, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty cron value".to_string());
    }

    let value = parse_named_value(raw, names).or_else(|| raw.parse::<u32>().ok());
    let Some(value) = value else {
        return Err(format!("invalid cron value '{}'", raw));
    };

    if value < min || value > max {
        return Err(format!(
            "cron value {} is outside allowed range {}..={}",
            value, min, max
        ));
    }

    Ok(value)
}

fn parse_named_value(raw: &str, names: &[(&'static str, u32)]) -> Option<u32> {
    names
        .iter()
        .find_map(|(name, value)| name.eq_ignore_ascii_case(raw).then_some(*value))
}

const MONTH_NAMES: &[(&str, u32)] = &[
    ("JAN", 1),
    ("FEB", 2),
    ("MAR", 3),
    ("APR", 4),
    ("MAY", 5),
    ("JUN", 6),
    ("JUL", 7),
    ("AUG", 8),
    ("SEP", 9),
    ("OCT", 10),
    ("NOV", 11),
    ("DEC", 12),
];

const WEEKDAY_NAMES: &[(&str, u32)] = &[
    ("SUN", 0),
    ("MON", 1),
    ("TUE", 2),
    ("WED", 3),
    ("THU", 4),
    ("FRI", 5),
    ("SAT", 6),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_five_fields_and_leading_zero_seconds() {
        assert_eq!(
            normalize_cron_expression("*/30 * * * *").unwrap(),
            "*/30 * * * *"
        );
        assert_eq!(
            normalize_cron_expression("0 */10 * * * *").unwrap(),
            "*/10 * * * *"
        );
    }

    #[test]
    fn normalize_rejects_nonzero_seconds() {
        assert!(normalize_cron_expression("*/30 * * * * *").is_err());
    }

    #[test]
    fn field_parser_supports_steps_ranges_and_names() {
        let schedule = CronSchedule::parse("*/15 9-17 * JAN MON-FRI").unwrap();

        assert!(schedule.minutes.matches(0));
        assert!(schedule.minutes.matches(45));
        assert!(!schedule.minutes.matches(46));
        assert!(schedule.hours.matches(9));
        assert!(schedule.hours.matches(17));
        assert!(!schedule.hours.matches(18));
        assert!(schedule.months.matches(1));
        assert!(!schedule.months.matches(2));
        assert!(schedule.weekdays.matches(1));
        assert!(schedule.weekdays.matches(5));
        assert!(!schedule.weekdays.matches(6));
    }

    #[test]
    fn weekday_seven_matches_sunday_zero() {
        let schedule = CronSchedule::parse("0 0 * * 7").unwrap();

        assert!(schedule.weekdays.matches(0));
    }
}
