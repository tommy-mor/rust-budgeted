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
use std::time::Duration;

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
    #[serde(default = "default_mem_mb")]
    pub mem_mb: usize,
    pub started_at: DateTime<Utc>,
    pub estimated_finish: DateTime<Utc>,
    pub worktree: Option<String>,
    pub cmd: Option<String>,
    pub justification_words: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedRun {
    pub id: String,
    /// Pid of the waiting process (for duplicate-id detection on retry).
    /// `None` on old queue files written before this field existed.
    #[serde(default)]
    pub pid: Option<i32>,
    pub test: String,
    pub cores: usize,
    #[serde(default = "default_mem_mb")]
    pub mem_mb: usize,
    pub enqueued_at: DateTime<Utc>,
    pub worktree: Option<String>,
    pub priority: i32,
}

fn default_mem_mb() -> usize {
    1024
}

#[derive(Debug, Clone)]
pub enum Admission {
    Admitted {
        active_cores: usize,
        active_mem_mb: usize,
    },
    Queued {
        position: usize,
        eta_start_secs: f64,
        eta_finish_secs: f64,
    },
    Rejected {
        active_cores: usize,
        budget_cores: usize,
        active_mem_mb: usize,
        budget_mem_mb: usize,
        reason: String,
        eta_start_secs: f64,
        eta_finish_secs: f64,
    },
}

pub struct Broker {
    active_path: PathBuf,
    queue_path: PathBuf,
    lock_path: PathBuf,
    budget_cores: usize,
    budget_mem_mb: usize,
}

impl Broker {
    #[must_use]
    pub fn new(state_dir: PathBuf, budget_cores: usize) -> Self {
        Self::with_mem(state_dir, budget_cores, 1_000_000)
    }

    #[must_use]
    pub fn with_mem(state_dir: PathBuf, budget_cores: usize, budget_mem_mb: usize) -> Self {
        Self {
            active_path: state_dir.join("active.json"),
            queue_path: state_dir.join("queue.json"),
            lock_path: state_dir.join("budget.lock"),
            budget_cores: budget_cores.max(1),
            budget_mem_mb: budget_mem_mb.max(256),
        }
    }

    #[must_use]
    pub fn budget_cores(&self) -> usize {
        self.budget_cores
    }

    #[must_use]
    pub fn budget_mem_mb(&self) -> usize {
        self.budget_mem_mb
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

    fn active_mem(&self, active: &[ActiveRun]) -> usize {
        active.iter().map(|r| r.mem_mb).sum()
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

    pub fn active_mem_now(&self) -> Result<usize, std::io::Error> {
        let _lock = self.lock()?;
        let active = self.reap(self.load_active_unlocked());
        Ok(self.active_mem(&active))
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

    /// Try to admit `cores` + `mem_mb` for `test`. On success the run is in active.json.
    /// `est_secs` is the estimate effective secs used for ETA.
    pub fn try_admit(
        &self,
        id: &str,
        pid: i32,
        test: &str,
        cores: usize,
        mem_mb: usize,
        est_secs: f64,
        worktree: Option<String>,
        justification_words: usize,
        queue_mode: bool,
    ) -> Result<Admission, std::io::Error> {
        let _lock = self.lock()?;
        let mut active = self.reap(self.load_active_unlocked());
        let mut queue = self.load_queue_unlocked();
        // Duplicate-id guard: the same id from the same pid is the
        // `wait_until_admitted` retry loop and must be idempotent; the same
        // id from a *different* pid is two runs colliding on `--run-id`.
        if let Some(existing) = active.iter().find(|r| r.id == id) {
            if existing.pid == pid {
                return Ok(Admission::Admitted {
                    active_cores: self.active_cores(&active),
                    active_mem_mb: self.active_mem(&active),
                });
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("duplicate run id {id}: already active (pid {})", existing.pid),
            ));
        }
        if let Some(pos) = queue.iter().position(|q| q.id == id) {
            let existing = &queue[pos];
            // Same pid (or legacy entry without a pid) is our own
            // wait_until_admitted retry: fall through to the budget check so
            // it can promote to Admitted once cores/mem free up. Only a
            // *different* pid is a real collision.
            if existing.pid.is_some() && existing.pid != Some(pid) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("duplicate run id {id}: already queued"),
                ));
            }
            let _ = pos;
        }
        let used_cores = self.active_cores(&active);
        let used_mem = self.active_mem(&active);
        let now = Utc::now();
        let cores_ok = used_cores + cores <= self.budget_cores;
        let mem_ok = used_mem + mem_mb <= self.budget_mem_mb;
        if cores_ok && mem_ok {
            let est_finish = now + chrono::Duration::milliseconds((est_secs * 1000.0) as i64);
            active.push(ActiveRun {
                id: id.to_string(),
                pid,
                test: test.to_string(),
                cores,
                mem_mb,
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
                active_cores: used_cores + cores,
                active_mem_mb: used_mem + mem_mb,
            });
        }
        // Over budget (either dimension).
        let reason = if !cores_ok && !mem_ok {
            format!(
                "cores {used_cores}+{cores}>{} and mem {used_mem}+{mem_mb}>{}",
                self.budget_cores, self.budget_mem_mb
            )
        } else if !cores_ok {
            format!("cores {used_cores}+{cores}>{}", self.budget_cores)
        } else {
            format!("mem {used_mem}+{mem_mb}>{}", self.budget_mem_mb)
        };
        let (eta_start, _) = self.eta(&active, &queue, cores);
        let eta_finish = eta_start + est_secs;
        if queue_mode {
            // Our own retry (same pid, already queued) must not push a
            // duplicate entry — just report the existing position.
            if let Some(pos) = queue.iter().position(|q| q.id == id) {
                return Ok(Admission::Queued {
                    position: pos,
                    eta_start_secs: eta_start,
                    eta_finish_secs: eta_finish,
                });
            }
            let pos = queue.len();
            queue.push(QueuedRun {
                id: id.to_string(),
                pid: Some(pid),
                test: test.to_string(),
                cores,
                mem_mb,
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
                active_cores: used_cores,
                budget_cores: self.budget_cores,
                active_mem_mb: used_mem,
                budget_mem_mb: self.budget_mem_mb,
                reason,
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

    /// Cancel the oldest (or lowest-priority) active run to free cores+mem.
    /// Returns the victim id, if any.
    pub fn replace_victim(
        &self,
        need_cores: usize,
        need_mem_mb: usize,
    ) -> Result<Option<String>, std::io::Error> {
        let victim = {
            let _lock = self.lock()?;
            let active = self.reap(self.load_active_unlocked());
            if active.is_empty() {
                None
            } else {
                // Oldest started_at first.
                let mut sorted = active.clone();
                sorted.sort_by_key(|r| r.started_at);
                // Prefer a victim that alone frees enough of both; else oldest.
                let victim = sorted
                    .iter()
                    .find(|r| r.cores >= need_cores && r.mem_mb >= need_mem_mb)
                    .or_else(|| sorted.iter().find(|r| r.mem_mb >= need_mem_mb))
                    .or_else(|| sorted.iter().find(|r| r.cores >= need_cores))
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
    mem_mb: usize,
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
            mem_mb,
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
                        "[fuzz-next] queued #{position} for {test}: starts in ~{eta_start_secs:.0}s (est {est_secs:.0}s, {cores} cores, {mem_mb} MiB)"
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

    /// A pid that survives `reap` (unix `kill(pid,0)`). Sharing the test
    /// process itself is live but must only be used when the test never
    /// cancels/kills the entry (which would SIGTERM this process).
    fn live_pid() -> i32 {
        std::process::id() as i32
    }

    /// A live placeholder pid whose death is harmless (for `replace_victim`
    /// which SIGTERMs the victim). On unix this is a throwaway `sleep`
    /// child; elsewhere a fake id (Windows reap never prunes anyway).
    fn victim_pid() -> (i32, Option<std::process::Child>) {
        #[cfg(unix)]
        {
            let child = std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .expect("spawn sleep placeholder");
            let pid = child.id() as i32;
            (pid, Some(child))
        }
        #[cfg(not(unix))]
        {
            // Windows keeps stale entries (reap is a no-op), so a fake id
            // survives long enough for victim selection, and taskkill on it
            // is a no-op that will not kill this process.
            (999_991, None)
        }
    }

    #[test]
    fn admits_until_budget_then_queues() {
        let dir = tempfile::tempdir().unwrap();
        let b = broker(&dir);
        let pid = live_pid();
        let a = b
            .try_admit("r1", pid, "native_fuzz", 8, 500, 60.0, None, 200, true)
            .unwrap();
        assert!(matches!(a, Admission::Admitted { .. }));
        let q = b
            .try_admit("r2", pid, "post_match", 8, 500, 60.0, None, 200, true)
            .unwrap();
        assert!(matches!(q, Admission::Queued { .. }));
        // Reject mode when not queueing.
        let r = b
            .try_admit("r3", pid, "ticker", 8, 500, 60.0, None, 200, false)
            .unwrap();
        assert!(matches!(r, Admission::Rejected { .. }));
        assert_eq!(b.active_cores_now().unwrap(), 8);
    }

    #[test]
    fn release_frees_cores() {
        let dir = tempfile::tempdir().unwrap();
        let b = broker(&dir);
        b.try_admit("r1", live_pid(), "a", 4, 500, 60.0, None, 200, true)
            .unwrap();
        b.release("r1").unwrap();
        assert_eq!(b.active_cores_now().unwrap(), 0);
    }

    #[test]
    fn replace_victim_picks_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let b = broker(&dir);
        let (pid_old, child_old) = victim_pid();
        let (pid_new, child_new) = victim_pid();
        b.try_admit("old", pid_old, "a", 4, 500, 60.0, None, 200, true)
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));
        b.try_admit("new", pid_new, "b", 4, 500, 60.0, None, 200, true)
            .unwrap();
        // Release/re-admit path keeps deterministic ordering by started_at (10ms gap).
        let v = b.replace_victim(4, 500).unwrap();
        assert_eq!(v.as_deref(), Some("old"));
        // Cleanup placeholders (victim already SIGTERM'd by replace_victim).
        let mut children = [child_old, child_new];
        for c in children.iter_mut().flatten() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    #[test]
    fn mem_budget_is_enforced_separately() {
        let dir = tempfile::tempdir().unwrap();
        let b = Broker::with_mem(dir.path().to_path_buf(), 12, 1000);
        let pid = live_pid();
        assert!(matches!(
            b.try_admit("m1", pid, "a", 1, 900, 60.0, None, 200, true)
                .unwrap(),
            Admission::Admitted { .. }
        ));
        // Second 900 MiB exceeds 1000 MiB cap even though cores fit.
        assert!(matches!(
            b.try_admit("m2", pid, "b", 1, 900, 60.0, None, 200, true)
                .unwrap(),
            Admission::Queued { .. }
        ));
    }
}
