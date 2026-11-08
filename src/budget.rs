//! Global core budget broker: cross-worktree, cross-agent, no taskset/systemd.
//!
//! Design: cooperative user-space broker backed by file-locked JSON.
//! - `active.json` = currently running runs (pid, cores, ETA)
//! - `queue.json`  = runs waiting for cores
//! - `budget.lock` = fs2 exclusive lock file guarding both.
//! Admission rule is a simple sum: `sum(active.cores) + request.cores <= budget`
//! => admit, else queue/reject/replace. ETA comes from stored
//! `estimated_finish` of active runs + queue ahead. Stale entries (dead pid) are
//! reaped on every lock. Windows liveness falls back to heartbeat mtime because
//! `pid_exists` there is best-effort (see util.rs).
//!
//! The broker NEVER uses taskset, cgroup quotas, or systemd. Instead it caps
//! *cooperative* parallelism by setting env vars (`CARGO_BUILD_JOBS`,
//! `JOINT_FUZZ_THREADS`, `RAYON_NUM_THREADS`, …) to the granted cores in the
//! executor, and by refusing to launch more than budget cores of children at once.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::util::pid_exists;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveRun {
    pub id: String,
    pub pid: i32,
    pub test: String,
    pub cores: usize,
    pub started_at: DateTime<Utc>,
    pub estimated_finish: DateTime<Utc>,
    pub worktree: Option<String>,
    pub cmd: Option<String>,
    pub justification_words: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedRun {
    pub id: String,
    pub test: String,
    pub cores: usize,
    pub enqueued_at: DateTime<Utc>,
    pub worktree: Option<String>,
    pub priority: i32,
}

#[derive(Debug, Clone)]
pub enum Admission {
    Admitted {
        active_cores: usize,
    },
    Queued {
        position: usize,
        eta_start_secs: f64,
        eta_finish_secs: f64,
    },
    Rejected {
        active_cores: usize,
        budget_cores: usize,
        eta_start_secs: f64,
        eta_finish_secs: f64,
    },
}

pub struct Broker {
    active_path: PathBuf,
    queue_path: PathBuf,
    lock_path: PathBuf,
    budget_cores: usize,
}

impl Broker {
    #[must_use]
    pub fn new(state_dir: PathBuf, budget_cores: usize) -> Self {
        Self {
            active_path: state_dir.join("active.json"),
            queue_path: state_dir.join("queue.json"),
            lock_path: state_dir.join("budget.lock"),
            budget_cores: budget_cores.max(1),
        }
    }

    fn ensure_parent(path: &PathBuf) -> std::io::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        Ok(())
    }

    pub fn lock(&self) -> std::io::Result<std::fs::File> {
        Self::ensure_parent(&self.lock_path)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.lock_path)?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn load_active_unlocked(&self) -> Vec<ActiveRun> {
        let Ok(text) = std::fs::read_to_string(&self.active_path) else {
            return Vec::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn load_queue_unlocked(&self) -> Vec<QueuedRun> {
        let Ok(text) = std::fs::read_to_string(&self.queue_path) else {
            return Vec::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn save_active_unlocked(&self, runs: &[ActiveRun]) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(runs).map_err(std::io::Error::other)?;
        std::fs::write(&self.active_path, text)?;
        Ok(())
    }

    fn save_queue_unlocked(&self, q: &[QueuedRun]) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(q).map_err(std::io::Error::other)?;
        std::fs::write(&self.queue_path, text)?;
        Ok(())
    }

    /// Drop entries whose pid no longer exists (unix). On Windows we keep them
    /// (pid_exists is best-effort); heartbeat pruning happens via `touch`.
    fn reap(&self, active: Vec<ActiveRun>) -> Vec<ActiveRun> {
        #[cfg(unix)]
        {
            active.into_iter().filter(|r| pid_exists(r.pid)).collect()
        }
        #[cfg(not(unix))]
        {
            active
        }
    }

    fn active_cores(&self, active: &[ActiveRun]) -> usize {
        active.iter().map(|r| r.cores).sum()
    }

    fn eta(&self, active: &[ActiveRun], queue: &[QueuedRun], extra_cores: usize) -> (f64, f64) {
        let now = Utc::now();
        let start_secs = active
            .iter()
            .map(|r| (r.estimated_finish - now).num_seconds().max(0) as f64)
            .fold(0f64, f64::max);
        // finish = start + this run's own estimate (caller fills estimated_finish after admit;
        // here approximate with max remaining).
        let finish_secs = start_secs + extra_cores as f64; // caller replaces with real estimate
        let _ = queue;
        (start_secs, finish_secs)
    }

    pub fn active_cores_now(&self) -> Result<usize, std::io::Error> {
        let _lock = self.lock()?;
        let active = self.reap(self.load_active_unlocked());
        Ok(self.active_cores(&active))
    }

    pub fn list_active(&self) -> Result<Vec<ActiveRun>, std::io::Error> {
        let _lock = self.lock()?;
        let active = self.reap(self.load_active_unlocked());
        self.save_active_unlocked(&active)?;
        Ok(active)
    }

    pub fn list_queue(&self) -> Result<Vec<QueuedRun>, std::io::Error> {
        let _lock = self.lock()?;
        Ok(self.load_queue_unlocked())
    }

    /// Try to admit `cores` for `test`. On success the run is in active.json.
    /// `est_secs` is the estimate effective secs used for ETA.
    pub fn try_admit(
        &self,
        id: &str,
        pid: i32,
        test: &str,
        cores: usize,
        est_secs: f64,
        worktree: Option<String>,
        justification_words: usize,
        queue_mode: bool,
    ) -> Result<Admission, std::io::Error> {
        let _lock = self.lock()?;
        let mut active = self.reap(self.load_active_unlocked());
        let mut queue = self.load_queue_unlocked();
        let used = self.active_cores(&active);
        let now = Utc::now();
        if used + cores <= self.budget_cores {
            let est_finish = now + chrono::Duration::milliseconds((est_secs * 1000.0) as i64);
            active.push(ActiveRun {
                id: id.to_string(),
                pid,
                test: test.to_string(),
                cores,
                started_at: now,
                estimated_finish: est_finish,
                worktree,
                cmd: None,
                justification_words,
            });
            self.save_active_unlocked(&active)?;
            // Remove self from queue if present (promotion).
            queue.retain(|q| q.id != id);
            self.save_queue_unlocked(&queue)?;
            return Ok(Admission::Admitted {
                active_cores: used + cores,
            });
        }
        // Over budget.
        let (eta_start, _) = self.eta(&active, &queue, cores);
        let eta_finish = eta_start + est_secs;
        if queue_mode {
            let pos = queue.len();
            queue.push(QueuedRun {
                id: id.to_string(),
                test: test.to_string(),
                cores,
                enqueued_at: now,
                worktree,
                priority: 0,
            });
            self.save_queue_unlocked(&queue)?;
            Ok(Admission::Queued {
                position: pos,
                eta_start_secs: eta_start,
                eta_finish_secs: eta_finish,
            })
        } else {
            Ok(Admission::Rejected {
                active_cores: used,
                budget_cores: self.budget_cores,
                eta_start_secs: eta_start,
                eta_finish_secs: eta_finish,
            })
        }
    }

    pub fn release(&self, id: &str) -> Result<(), std::io::Error> {
        let _lock = self.lock()?;
        let mut active = self.reap(self.load_active_unlocked());
        active.retain(|r| r.id != id);
        self.save_active_unlocked(&active)?;
        let mut queue = self.load_queue_unlocked();
        queue.retain(|q| q.id != id);
        self.save_queue_unlocked(&queue)?;
        Ok(())
    }

    /// Heartbeat: bump file mtime so Windows pruning can expire stale entries.
    pub fn heartbeat(&self, id: &str) -> Result<(), std::io::Error> {
        let _lock = self.lock()?;
        let _ = id;
        // Re-write active to refresh mtime (coarse heartbeat).
        let active = self.load_active_unlocked();
        self.save_active_unlocked(&active)?;
        Ok(())
    }

    /// Cancel a run: SIGTERM (unix) / TerminateProcess (windows) then SIGKILL.
    /// Returns true if the entry existed.
    pub fn cancel(&self, id: &str) -> Result<bool, std::io::Error> {
        let (pid_opt, existed) = {
            let _lock = self.lock()?;
            let mut active = self.reap(self.load_active_unlocked());
            let pos = active.iter().position(|r| r.id == id);
            let pid_opt = pos.map(|i| active[i].pid);
            if let Some(i) = pos {
                active.remove(i);
            }
            self.save_active_unlocked(&active)?;
            let mut queue = self.load_queue_unlocked();
            let len_before = queue.len();
            queue.retain(|q| q.id != id);
            let q_existed = queue.len() != len_before;
            if q_existed {
                self.save_queue_unlocked(&queue)?;
            }
            (pid_opt, pid_opt.is_some() || q_existed)
        };
        let Some(pid) = pid_opt else {
            return Ok(existed);
        };
        terminate_pid(pid, Duration::from_secs(5));
        Ok(true)
    }

    /// Cancel the oldest (or lowest-priority) active run to free cores for `need_cores`.
    /// Returns the victim id, if any.
    pub fn replace_victim(&self, need_cores: usize) -> Result<Option<String>, std::io::Error> {
        let victim = {
            let _lock = self.lock()?;
            let active = self.reap(self.load_active_unlocked());
            if active.is_empty() {
                None
            } else {
                // Oldest started_at first.
                let mut sorted = active.clone();
                sorted.sort_by_key(|r| r.started_at);
                // Prefer a victim that alone frees enough; else oldest.
                let victim = sorted
                    .iter()
                    .find(|r| r.cores >= need_cores)
                    .or(sorted.first())
                    .map(|r| r.id.clone());
                victim
            }
        };
        if let Some(ref vid) = victim {
            let _ = self.cancel(vid);
        }
        Ok(victim)
    }
}

/// Cross-platform terminate: SIGTERM, wait grace, SIGKILL (unix) or kill (windows).
pub fn terminate_pid(pid: i32, grace: Duration) {
    if pid <= 0 {
        return;
    }
    #[cfg(unix)]
    {
        unsafe {
            let _ = libc::kill(pid, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + grace;
        while std::time::Instant::now() < deadline {
            if !pid_exists(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        unsafe {
            let _ = libc::kill(pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        // No extra dep: shell out to `taskkill /PID /T /F` (best effort).
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
        let _ = grace;
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, grace);
    }
}

/// Poll until `id` is admitted, re-trying every 500ms. Prints ETA updates.
/// Returns Ok(()) when admitted (caller must have enqueued first in queue mode).
pub fn wait_until_admitted(
    broker: &Broker,
    id: &str,
    pid: i32,
    test: &str,
    cores: usize,
    est_secs: f64,
    worktree: Option<String>,
    justification_words: usize,
    verbose: bool,
) -> Result<(), std::io::Error> {
    loop {
        match broker.try_admit(
            id,
            pid,
            test,
            cores,
            est_secs,
            worktree.clone(),
            justification_words,
            true,
        )? {
            Admission::Admitted { .. } => return Ok(()),
            Admission::Queued {
                position,
                eta_start_secs,
                ..
            } => {
                if verbose {
                    eprintln!(
                        "[fuzz-next] queued #{position} for {test}: starts in ~{eta_start_secs:.0}s (est {est_secs:.0}s, {cores} cores)"
                    );
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Admission::Rejected { .. } => {
                // In wait loop we always pass queue_mode=true, so Rejected shouldn't happen.
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// Summary line for `status`.
#[must_use]
pub fn format_eta(secs: f64) -> String {
    if secs < 90.0 {
        format!("{secs:.0}s")
    } else if secs < 3600.0 {
        format!("{:.1}m", secs / 60.0)
    } else {
        format!("{:.1}h", secs / 3600.0)
    }
}

#[cfg(test)]
mod tests_budget {
    use super::*;

    fn broker(dir: &tempfile::TempDir) -> Broker {
        Broker::new(dir.path().to_path_buf(), 12)
    }

    #[test]
    fn admits_until_budget_then_queues() {
        let dir = tempfile::tempdir().unwrap();
        let b = broker(&dir);
        let a = b
            .try_admit("r1", 111, "native_fuzz", 8, 60.0, None, 200, true)
            .unwrap();
        assert!(matches!(a, Admission::Admitted { .. }));
        let q = b
            .try_admit("r2", 222, "post_match", 8, 60.0, None, 200, true)
            .unwrap();
        assert!(matches!(q, Admission::Queued { .. }));
        // Reject mode when not queueing.
        let r = b
            .try_admit("r3", 333, "ticker", 8, 60.0, None, 200, false)
            .unwrap();
        assert!(matches!(r, Admission::Rejected { .. }));
        assert_eq!(b.active_cores_now().unwrap(), 8);
    }

    #[test]
    fn release_frees_cores() {
        let dir = tempfile::tempdir().unwrap();
        let b = broker(&dir);
        b.try_admit("r1", 111, "a", 4, 60.0, None, 200, true)
            .unwrap();
        b.release("r1").unwrap();
        assert_eq!(b.active_cores_now().unwrap(), 0);
    }

    #[test]
    fn replace_victim_picks_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let b = broker(&dir);
        b.try_admit("old", 111, "a", 4, 60.0, None, 200, true)
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));
        b.try_admit("new", 222, "b", 4, 60.0, None, 200, true)
            .unwrap();
        // Release/re-admit path keeps deterministic ordering by started_at (10ms gap).
        let v = b.replace_victim(4).unwrap();
        assert_eq!(v.as_deref(), Some("old"));
    }
}
