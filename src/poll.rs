//! Agent-friendly wait/poll helpers.
//!
//! The problem this solves for agents: runs are long (minutes, 100s of core-s),
//! and agents need to block efficiently until *their* run finishes instead of
//! hand-rolling `while status; do sleep; done` loops over `status` + `runs`.
//!
//! `check_once` snapshots one run id from the three sources of truth
//! (timings.jsonl, active.json, queue.json). `wait_for_runs` loops that
//! snapshot until every target reaches a terminal state (Done/Cancelled),
//! an unknown-id grace period expires, or the caller-supplied timeout fires.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::budget::{format_eta, ActiveRun, Broker, QueuedRun};
use crate::timings::{TimingRecord, TimingStore};
use crate::util::parse_duration;

/// How long `wait` will poll for a never-seen run id to appear before giving
/// up with Unknown, when the caller did not set an explicit `--timeout`
/// shorter than this. 30s comfortably covers the launch race: a background
/// run enqueues within milliseconds of printing its id, while a typo'd id
/// would otherwise hang forever. The grace does not apply to `--test` mode
/// (where waiting for a future run to appear is the point).
pub const UNKNOWN_GRACE: Duration = Duration::from_secs(30);

/// Terminal / non-terminal state of one run from the caller's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollPhase {
    /// No timing record, not in active/queue. Either not launched yet,
    /// a typo'd id, or a run that disappeared while unseen.
    Unknown,
    /// In `queue.json` waiting for budget.
    Queued,
    /// In `active.json` and executing.
    Active,
    /// Finished with a timing record (pass/fail/timeout/flaky).
    Done,
    /// Was previously seen active/queued but vanished with no timing
    /// record — cancelled, killed, or crashed before recording.
    Cancelled,
}

impl PollPhase {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Queued => "queued",
            Self::Active => "active",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }
}

/// One snapshot of a run at poll time.
#[derive(Debug, Clone)]
pub struct PollCheck {
    pub run_id: String,
    pub phase: PollPhase,
    pub test: Option<String>,
    pub cores: Option<usize>,
    pub mem_mb: Option<usize>,
    pub queue_position: Option<usize>,
    /// Seconds until estimated start (queued) or estimated finish (active).
    pub remaining_secs: Option<f64>,
    pub estimated_finish: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub enqueued_at: Option<DateTime<Utc>>,
    pub pid: Option<i32>,
    pub worktree: Option<String>,
    pub record: Option<TimingRecord>,
    /// Path to `runs/<id>/` (string form, may not exist yet).
    pub run_dir: String,
}

impl PollCheck {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.phase.is_terminal()
    }

    /// Exit-code-ish pass predicate for a finished run: `rc == 0`.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.record.as_ref().is_some_and(|r| r.rc == 0)
    }
}

/// Snapshot one run id without waiting. Cheap enough to call every poll tick.
///
/// Deliberately returns `Unknown` (not `Cancelled`) when only `runs/<id>/`
/// exists: a background run creates that directory *before* it is queued, so
/// directory existence alone cannot distinguish "starting up" from
/// "cancelled". `wait_for_runs` promotes previously-seen Unknowns to
/// Cancelled once the run has been observed active/queued.
#[must_use]
pub fn check_once(broker: &Broker, store: &TimingStore, state_dir: &Path, run_id: &str) -> PollCheck {
    let run_dir = state_dir.join("runs").join(run_id);
    let base = PollCheck {
        run_id: run_id.to_string(),
        phase: PollPhase::Unknown,
        test: None,
        cores: None,
        mem_mb: None,
        queue_position: None,
        remaining_secs: None,
        estimated_finish: None,
        started_at: None,
        enqueued_at: None,
        pid: None,
        worktree: None,
        record: None,
        run_dir: run_dir.to_string_lossy().to_string(),
    };

    // 1. Finished: a timing record exists. This wins even if the active entry
    //    has not been released yet (record is appended before release).
    let all = store.load_all();
    if let Some(rec) = all.iter().find(|r| r.run_id == run_id).cloned() {
        return PollCheck {
            phase: PollPhase::Done,
            test: Some(rec.test.clone()),
            cores: Some(rec.cores),
            mem_mb: rec.mem_mb_requested,
            record: Some(rec),
            ..base
        };
    }

    // 2. Active.
    let active = broker.list_active().unwrap_or_default();
    if let Some(a) = active.iter().find(|r| r.id == run_id) {
        let remaining = (a.estimated_finish - Utc::now()).num_seconds().max(0) as f64;
        return PollCheck {
            phase: PollPhase::Active,
            test: Some(a.test.clone()),
            cores: Some(a.cores),
            mem_mb: Some(a.mem_mb),
            remaining_secs: Some(remaining),
            estimated_finish: Some(a.estimated_finish),
            started_at: Some(a.started_at),
            pid: Some(a.pid),
            worktree: a.worktree.clone(),
            ..base
        };
    }

    // 3. Queued. ETA-to-start is the max remaining among active runs,
    //    matching the broker's queue ETA model.
    let queue = broker.list_queue().unwrap_or_default();
    if let Some(pos) = queue.iter().position(|q| q.id == run_id) {
        let q = &queue[pos];
        let eta_start = active
            .iter()
            .map(|r| (r.estimated_finish - Utc::now()).num_seconds().max(0) as f64)
            .fold(0.0, f64::max);
        return PollCheck {
            phase: PollPhase::Queued,
            test: Some(q.test.clone()),
            cores: Some(q.cores),
            mem_mb: Some(q.mem_mb),
            queue_position: Some(pos),
            remaining_secs: Some(eta_start),
            enqueued_at: Some(q.enqueued_at),
            worktree: q.worktree.clone(),
            ..base
        };
    }

    base
}

/// How a `wait` finished, for exit-code mapping in `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Every target finished with `rc == 0`.
    Passed,
    /// At least one target finished with `rc != 0` (fail/timeout).
    Failed,
    /// At least one target vanished with no record after being seen.
    Cancelled,
    /// A target never appeared (typo / unknown id, or `--test` with no run
    /// ever active before the timeout). Exit 2.
    Unknown,
    /// The caller timeout fired while targets were still pending. Exit 124.
    Timeout,
}

/// Result of `wait_for_runs`.
#[derive(Debug, Clone)]
pub struct WaitResult {
    pub checks: Vec<PollCheck>,
    pub outcome: WaitOutcome,
    pub elapsed: Duration,
    pub timed_out: bool,
}

/// Parse a duration string to `Duration`, returning `fallback` on failure.
/// Used for `--interval`/`--timeout` so a malformed value never panics.
#[must_use]
pub fn resolve_duration(s: Option<&str>, fallback: Duration) -> Duration {
    s.and_then(parse_duration).unwrap_or(fallback)
}

/// Parse an interval value with the standard 2s default.
#[must_use]
pub fn resolve_interval(s: Option<&str>) -> Duration {
    resolve_duration(s, Duration::from_secs(2))
}

/// Aggregate final snapshots into an outcome. `timed_out=true` wins over
/// everything so a still-pending wait is reported as Timeout (124), not as
/// whatever the last snapshot happened to be.
#[must_use]
pub fn aggregate_outcome(checks: &[PollCheck], timed_out: bool) -> WaitOutcome {
    if timed_out {
        return WaitOutcome::Timeout;
    }
    if checks.is_empty() {
        return WaitOutcome::Unknown;
    }
    if checks.iter().any(|c| c.phase == PollPhase::Unknown) {
        return WaitOutcome::Unknown;
    }
    if checks.iter().any(|c| c.phase == PollPhase::Cancelled) {
        return WaitOutcome::Cancelled;
    }
    if checks.iter().any(|c| c.phase == PollPhase::Done && !c.passed()) {
        return WaitOutcome::Failed;
    }
    if checks.iter().all(|c| c.phase == PollPhase::Done) {
        return WaitOutcome::Passed;
    }
    // Still pending (should not happen after a finished loop) — treat as
    // Timeout so callers do not mistake it for a pass.
    WaitOutcome::Timeout
}

/// Block until every target run reaches a terminal state, the unknown grace
/// expires, or `timeout` fires.
///
/// - `run_ids`: explicit ids to wait for.
/// - `test_filter`: when `Some` and `run_ids` is empty, discover active/queued
///   runs of that test on every tick and wait for the union of everything ever
///   discovered. When `Some` and `run_ids` is non-empty, the filter is ignored
///   (explicit ids win).
/// - `interval`: sleep between snapshots.
/// - `timeout`: overall deadline; `None` waits forever (except the unknown
///   grace for explicit ids).
/// - `on_tick(checks, finished)`: called once per tick (including the final
///   tick). `checks` is empty only in `--test` mode before any run appears.
#[must_use]
pub fn wait_for_runs(
    broker: &Broker,
    store: &TimingStore,
    state_dir: &Path,
    run_ids: &[String],
    test_filter: Option<&str>,
    interval: Duration,
    timeout: Option<Duration>,
    mut on_tick: impl FnMut(&[PollCheck], bool),
) -> WaitResult {
    let start = Instant::now();
    let start_utc = Utc::now();
    let deadline = timeout.map(|t| start + t);
    // Union of everything we have ever seen active/queued/done. Drives both
    // test-mode discovery and the Unknown -> Cancelled promotion.
    let mut targets: Vec<String> = run_ids.to_vec();
    let mut seen: HashSet<String> = HashSet::new();
    let test_mode = test_filter.is_some() && run_ids.is_empty();
    let unknown_grace = if test_mode { None } else { Some(UNKNOWN_GRACE) };

    loop {
        if test_mode {
            let test = test_filter.unwrap_or_default();
            let active = broker.list_active().unwrap_or_default();
            let queue = broker.list_queue().unwrap_or_default();
            for a in active.iter().filter(|r| r.test == test) {
                if !targets.iter().any(|t| t == &a.id) {
                    targets.push(a.id.clone());
                }
            }
            for q in queue.iter().filter(|q| q.test == test) {
                if !targets.iter().any(|t| t == &q.id) {
                    targets.push(q.id.clone());
                }
            }
            // Also discover runs that already *finished* during this wait
            // (or just before it). Without this, a short run that lives
            // less than one poll interval could slip between ticks and be
            // missed entirely. The 30s lookback covers the launch race
            // where `run --background` finishes before `wait --test`'s
            // first tick; older history is ignored so a fresh `wait`
            // still waits for the *next* run, not a stale past one.
            let lookback = chrono::Duration::seconds(30);
            let all = store.load_all();
            for r in all.iter().filter(|r| r.test == test) {
                if r.ts >= start_utc - lookback {
                    if !targets.iter().any(|t| t == &r.run_id) {
                        targets.push(r.run_id.clone());
                    }
                }
            }
        }

        let mut checks: Vec<PollCheck> = targets
            .iter()
            .map(|id| check_once(broker, store, state_dir, id))
            .collect();
        // Remember anything that is not Unknown; promote unseen-then-gone
        // Unknowns to Cancelled on later ticks.
        for c in &checks {
            if c.phase != PollPhase::Unknown {
                seen.insert(c.run_id.clone());
            }
        }
        for c in &mut checks {
            if c.phase == PollPhase::Unknown && seen.contains(&c.run_id) {
                c.phase = PollPhase::Cancelled;
            }
        }

        let any_unknown_unseen = checks
            .iter()
            .any(|c| c.phase == PollPhase::Unknown && !seen.contains(&c.run_id));
        let finished =
            !targets.is_empty() && checks.iter().all(|c| c.is_terminal());
        on_tick(&checks, finished);

        if finished {
            let outcome = aggregate_outcome(&checks, false);
            return WaitResult {
                checks,
                outcome,
                elapsed: start.elapsed(),
                timed_out: false,
            };
        }

        let now = Instant::now();
        if let Some(dl) = deadline {
            if now >= dl {
                let outcome = aggregate_outcome(&checks, true);
                return WaitResult {
                    checks,
                    outcome,
                    elapsed: start.elapsed(),
                    timed_out: true,
                };
            }
        }
        // Unknown-id grace: a typo'd id never appears, so do not hang
        // forever waiting for it. Only for explicit ids, only while at
        // least one target is still unseen.
        if let Some(grace) = unknown_grace {
            let still_unknown = any_unknown_unseen
                && !test_mode
                && (checks.is_empty()
                    || checks.iter().any(|c| c.phase == PollPhase::Unknown));
            if still_unknown && start.elapsed() >= grace {
                let outcome = aggregate_outcome(&checks, false);
                // Aggregate would say Unknown only if some check is Unknown;
                // ensure that is what we report even if others are pending.
                let outcome = if outcome == WaitOutcome::Timeout {
                    WaitOutcome::Unknown
                } else {
                    outcome
                };
                return WaitResult {
                    checks,
                    outcome,
                    elapsed: start.elapsed(),
                    timed_out: false,
                };
            }
        }

        // Sleep for one interval, clamped so we wake exactly at the deadline.
        let sleep_for = match deadline {
            Some(dl) => {
                let remaining = dl.saturating_duration_since(Instant::now());
                interval.min(remaining.max(Duration::from_millis(10)))
            }
            None => interval,
        };
        std::thread::sleep(sleep_for);
    }
}

/// One-line human progress for a single tick.
#[must_use]
pub fn format_check_human(c: &PollCheck) -> String {
    match c.phase {
        PollPhase::Unknown => {
            format!("waiting for {} to appear (queued/active not seen yet)", c.run_id)
        }
        PollPhase::Queued => {
            let pos = c.queue_position.map_or_else(|| "?".to_string(), |p| format!("#{p}"));
            let eta = c
                .remaining_secs
                .map_or_else(String::new, |s| format!(", starts in ~{}", format_eta(s)));
            let res = c
                .cores
                .map_or_else(String::new, |n| format!("{n} cores"));
            let mem = c.mem_mb.map_or_else(String::new, |m| format!(" / {m}MiB"));
            format!(
                "queued {} {}{}{}",
                pos,
                res,
                mem,
                if eta.is_empty() { String::new() } else { eta }
            )
        }
        PollPhase::Active => {
            let res = c.cores.map_or_else(String::new, |n| format!("{n} cores"));
            let rem = c
                .remaining_secs
                .map_or_else(String::new, |s| format!(", {} remaining", format_eta(s)));
            format!("active {}{}", res, rem)
        }
        PollPhase::Done => format_final_human(c),
        PollPhase::Cancelled => {
            "cancelled (was active/queued, vanished with no timing record)".to_string()
        }
    }
}

/// Final human line for a finished run (mirrors `runs` output style).
#[must_use]
pub fn format_final_human(c: &PollCheck) -> String {
    let Some(rec) = c.record.as_ref() else {
        return match c.phase {
            PollPhase::Cancelled => format!("CANCELLED {} (no timing record)", c.run_id),
            PollPhase::Unknown => format!("UNKNOWN {}", c.run_id),
            PollPhase::Queued => format!("QUEUED {}", c.run_id),
            PollPhase::Active => format!("ACTIVE {}", c.run_id),
            PollPhase::Done => format!("DONE {} (no record)", c.run_id),
        };
    };
    let status = rec.status.to_uppercase();
    let peak = rec
        .peak_rss_mb
        .map_or_else(String::new, |mb| format!(" peak {mb:.0}MiB"));
    format!(
        "{} {:<16} {:.1}s x{} cores {}{} (run {})",
        status,
        rec.test,
        rec.secs,
        rec.cores,
        rec.mem_mb_requested.map_or_else(String::new, |m| format!(" {m}MiB")),
        peak,
        rec.run_id,
    )
}

/// Machine-readable JSON for one check (final `wait` output element).
#[must_use]
pub fn check_to_json(c: &PollCheck) -> serde_json::Value {
    let rec = c.record.as_ref();
    serde_json::json!({
        "run_id": c.run_id,
        "phase": c.phase.as_str(),
        "test": c.test,
        "cores": c.cores,
        "mem_mb": c.mem_mb,
        "queue_position": c.queue_position,
        "remaining_secs": c.remaining_secs,
        "estimated_finish": c.estimated_finish,
        "started_at": c.started_at,
        "enqueued_at": c.enqueued_at,
        "pid": c.pid,
        "worktree": c.worktree,
        "run_dir": c.run_dir,
        "rc": rec.map(|r| r.rc),
        "status": rec.map(|r| r.status.clone()),
        "secs": rec.map(|r| r.secs),
        "peak_rss_mb": rec.and_then(|r| r.peak_rss_mb),
        "sha": rec.and_then(|r| r.sha.clone()),
        "cases": rec.and_then(|r| r.cases),
    })
}

/// Result of waiting for the whole budget to drain (`status --poll`).
#[derive(Debug, Clone)]
pub struct IdleResult {
    pub elapsed: Duration,
    pub timed_out: bool,
    pub active: Vec<ActiveRun>,
    pub queue: Vec<QueuedRun>,
}

/// Block until `active` and `queue` are both empty (or `timeout` fires).
/// `on_tick(active, queue)` runs on every tick, including the final one.
#[must_use]
pub fn wait_until_idle(
    broker: &Broker,
    interval: Duration,
    timeout: Option<Duration>,
    mut on_tick: impl FnMut(&[ActiveRun], &[QueuedRun]),
) -> IdleResult {
    let start = Instant::now();
    let deadline = timeout.map(|t| start + t);
    loop {
        let active = broker.list_active().unwrap_or_default();
        let queue = broker.list_queue().unwrap_or_default();
        let idle = active.is_empty() && queue.is_empty();
        on_tick(&active, &queue);
        if idle {
            return IdleResult {
                elapsed: start.elapsed(),
                timed_out: false,
                active,
                queue,
            };
        }
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                return IdleResult {
                    elapsed: start.elapsed(),
                    timed_out: true,
                    active,
                    queue,
                };
            }
        }
        let sleep_for = match deadline {
            Some(dl) => {
                let remaining = dl.saturating_duration_since(Instant::now());
                interval.min(remaining.max(Duration::from_millis(10)))
            }
            None => interval,
        };
        std::thread::sleep(sleep_for);
    }
}

/// Read the stdout/stderr tail for a finished run (empty if logs are missing).
#[must_use]
pub fn read_run_tail(state_dir: &Path, run_id: &str, lines: usize) -> String {
    let dir = state_dir.join("runs").join(run_id);
    let mut out = String::new();
    for name in ["stdout.log", "stderr.log"] {
        let path = dir.join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            let ls: Vec<&str> = text.lines().collect();
            let start = ls.len().saturating_sub(lines);
            if !ls[start..].is_empty() {
                out.push_str(&format!("--- {name} ({} tail) ---\n", run_id));
                out.push_str(&ls[start..].join("\n"));
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests_poll {
    use super::*;
    use chrono::Utc;
    use crate::timings::TimingRecord;

    fn rec(run_id: &str, test: &str, secs: f64, rc: i32) -> TimingRecord {
        TimingRecord {
            ts: Utc::now(),
            test: test.to_string(),
            binary: None,
            secs,
            cores: 8,
            rc,
            status: if rc == 0 { "pass" } else { "fail" }.to_string(),
            sha: None,
            cases: None,
            worktree: None,
            run_id: run_id.to_string(),
            justification: None,
            hypothesis: None,
            seen_estimate_secs: None,
            mem_mb_requested: Some(1800),
            peak_rss_mb: Some(1500.0),
            seen_estimate_mem_mb: None,
        }
    }

    #[test]
    fn check_once_done_wins_over_active() {
        let dir = tempfile::tempdir().unwrap();
        let b = Broker::new(dir.path().to_path_buf(), 12);
        let store = TimingStore::new(dir.path().join("timings.jsonl"));
        store.append(&rec("r1", "native_fuzz", 307.0, 0)).unwrap();
        let c = check_once(&b, &store, dir.path(), "r1");
        assert_eq!(c.phase, PollPhase::Done);
        assert!(c.passed());
    }

    #[test]
    fn check_once_unknown_when_nothing_exists() {
        let dir = tempfile::tempdir().unwrap();
        let b = Broker::new(dir.path().to_path_buf(), 12);
        let store = TimingStore::new(dir.path().join("timings.jsonl"));
        let c = check_once(&b, &store, dir.path(), "nope");
        assert_eq!(c.phase, PollPhase::Unknown);
    }

    #[test]
    fn check_once_sees_active_and_queued() {
        let dir = tempfile::tempdir().unwrap();
        let b = Broker::new(dir.path().to_path_buf(), 12);
        let store = TimingStore::new(dir.path().join("timings.jsonl"));
        let pid = std::process::id() as i32;
        b.try_admit("a1", pid, "native_fuzz", 8, 500, 60.0, None, 200, true)
            .unwrap();
        let ca = check_once(&b, &store, dir.path(), "a1");
        assert_eq!(ca.phase, PollPhase::Active);
        assert_eq!(ca.test.as_deref(), Some("native_fuzz"));
        b.try_admit("q1", pid, "post_match", 8, 500, 60.0, None, 200, true)
            .unwrap();
        let cq = check_once(&b, &store, dir.path(), "q1");
        assert_eq!(cq.phase, PollPhase::Queued);
        assert_eq!(cq.queue_position, Some(0));
    }

    #[test]
    fn wait_returns_passed_for_finished_run() {
        let dir = tempfile::tempdir().unwrap();
        let b = Broker::new(dir.path().to_path_buf(), 12);
        let store = TimingStore::new(dir.path().join("timings.jsonl"));
        store.append(&rec("r1", "native_fuzz", 10.0, 0)).unwrap();
        let r = wait_for_runs(
            &b,
            &store,
            dir.path(),
            &["r1".to_string()],
            None,
            Duration::from_millis(10),
            Some(Duration::from_secs(2)),
            |_, _| {},
        );
        assert_eq!(r.outcome, WaitOutcome::Passed);
        assert!(!r.timed_out);
    }

    #[test]
    fn wait_returns_failed_for_failed_run() {
        let dir = tempfile::tempdir().unwrap();
        let b = Broker::new(dir.path().to_path_buf(), 12);
        let store = TimingStore::new(dir.path().join("timings.jsonl"));
        store.append(&rec("r1", "native_fuzz", 10.0, 1)).unwrap();
        let r = wait_for_runs(
            &b,
            &store,
            dir.path(),
            &["r1".to_string()],
            None,
            Duration::from_millis(10),
            Some(Duration::from_secs(2)),
            |_, _| {},
        );
        assert_eq!(r.outcome, WaitOutcome::Failed);
    }

    #[test]
    fn wait_times_out_for_unknown_with_short_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let b = Broker::new(dir.path().to_path_buf(), 12);
        let store = TimingStore::new(dir.path().join("timings.jsonl"));
        let r = wait_for_runs(
            &b,
            &store,
            dir.path(),
            &["ghost".to_string()],
            None,
            Duration::from_millis(10),
            Some(Duration::from_millis(50)),
            |_, _| {},
        );
        assert!(r.timed_out);
        assert_eq!(r.outcome, WaitOutcome::Timeout);
    }

    #[test]
    fn aggregate_maps_phases() {
        let done_pass = PollCheck {
            run_id: "a".into(),
            phase: PollPhase::Done,
            test: None,
            cores: None,
            mem_mb: None,
            queue_position: None,
            remaining_secs: None,
            estimated_finish: None,
            started_at: None,
            enqueued_at: None,
            pid: None,
            worktree: None,
            record: Some(rec("a", "t", 1.0, 0)),
            run_dir: String::new(),
        };
        assert_eq!(aggregate_outcome(std::slice::from_ref(&done_pass), false), WaitOutcome::Passed);
        assert_eq!(
            aggregate_outcome(std::slice::from_ref(&done_pass), true),
            WaitOutcome::Timeout
        );
        let unknown = PollCheck {
            phase: PollPhase::Unknown,
            ..done_pass.clone()
        };
        assert_eq!(aggregate_outcome(&[unknown], false), WaitOutcome::Unknown);
    }

    #[test]
    fn idle_returns_immediately_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let b = Broker::new(dir.path().to_path_buf(), 12);
        let r = wait_until_idle(&b, Duration::from_millis(10), Some(Duration::from_secs(1)), |_, _| {});
        assert!(!r.timed_out);
        assert!(r.active.is_empty());
    }
}
