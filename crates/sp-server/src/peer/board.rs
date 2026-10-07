//! #229: the jobs this node runs now, listed in its catalog while they run.
//! In memory on purpose: a crash takes its announcements with it, so a peer
//! never waits on a job that died (it would wait the full 2 h otherwise).
//! A QUEUED job is not on the board: the catalog reads those from the rows
//! (lane 3).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use super::kind::Job;
use super::wire::{CatalogJob, JobState, ms_to_rfc3339, now_ms};

/// `(youtube id, job)` → `(started ms, live guards)`.
type Running = HashMap<(String, Job), (i64, usize)>;

#[derive(Default)]
pub struct JobBoard {
    running: Mutex<Running>,
}

/// The announcement of one running job; dropping it ends the announcement.
#[must_use = "a job is announced only while its guard lives"]
pub struct JobGuard {
    board: Arc<JobBoard>,
    key: (String, Job),
}

impl JobBoard {
    /// Announce `job` for `youtube_id` until the returned guard drops. A
    /// second guard of the same job keeps the first one's start.
    pub fn announce(self: &Arc<Self>, youtube_id: &str, job: Job) -> JobGuard {
        let key = (youtube_id.to_string(), job);
        let mut running = self.lock();
        running.entry(key.clone()).or_insert((now_ms(), 0)).1 += 1;
        JobGuard {
            board: Arc::clone(self),
            key,
        }
    }

    /// Every running job as catalog entries of `node`, one per kind it makes,
    /// sorted by YouTube id, then kind.
    pub fn snapshot(&self, node: &str) -> Vec<CatalogJob> {
        let mut jobs: Vec<CatalogJob> = self
            .lock()
            .iter()
            .flat_map(|((youtube_id, job), (started, _))| {
                job.makes().iter().map(move |kind| CatalogJob {
                    youtube_id: youtube_id.clone(),
                    kind: *kind,
                    node: node.to_string(),
                    state: JobState::Running,
                    started_at: Some(ms_to_rfc3339(*started)),
                })
            })
            .collect();
        jobs.sort_by(|a, b| {
            (a.youtube_id.as_str(), a.kind.as_str()).cmp(&(b.youtube_id.as_str(), b.kind.as_str()))
        });
        jobs
    }

    /// The map, even after a panic elsewhere poisoned the lock: nothing that
    /// changes the map can panic half-way, so it stays consistent.
    fn lock(&self) -> MutexGuard<'_, Running> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        let mut running = self.board.lock();
        if let Some(entry) = running.get_mut(&self.key) {
            entry.1 -= 1;
            if entry.1 == 0 {
                running.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
#[path = "board_tests.rs"]
mod tests;
