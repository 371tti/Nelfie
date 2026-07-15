use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::CronJob;

const CRON_JOBS_STORE_PATH: &str = "data/runtime/cron_jobs.json";

pub(super) struct CronJobStore {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct CronJobsDocument {
    version: u32,
    jobs: Vec<CronJob>,
}

impl CronJobStore {
    pub(super) fn new() -> Self {
        Self {
            path: PathBuf::from(CRON_JOBS_STORE_PATH),
        }
    }

    pub(super) fn load(&self) -> Result<Vec<CronJob>, String> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("failed to read cron jobs file: {error}")),
        };

        serde_json::from_str::<CronJobsDocument>(&text)
            .map(|document| document.jobs)
            .map_err(|error| format!("failed to parse cron jobs file: {error}"))
    }

    pub(super) fn save(&self, jobs: Vec<CronJob>) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create cron jobs directory: {error}"))?;
        }

        let document = CronJobsDocument { version: 1, jobs };
        let body = serde_json::to_string_pretty(&document)
            .map_err(|error| format!("failed to serialize cron jobs: {error}"))?;

        fs::write(&self.path, body).map_err(|error| format!("failed to write cron jobs: {error}"))
    }
}
