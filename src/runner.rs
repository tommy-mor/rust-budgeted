//! Runner orchestration: estimate → gate → budget → execute → record → report.

use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;

use crate::budget::{Admission, Broker};
use crate::config::Config;
use crate::executor::{build_test_command, fmt_dur, run_with_timeouts};
use crate::reporter::{format_human, write_junit, write_result_json, RunSummary};
use crate::timings::{
    count_words, validate_justification, validate_seen_estimate, TimingRecord, TimingStore,
};
use crate::util::{parse_duration, RetryPolicy};

pub struct RunRequest {
    pub test: String,
    pub cores: usize,
    pub mem_mb: Option<usize>,
    pub release: bool,
    pub cases: Option<u64>,
    pub justification: Option<String>,
    pub hypothesis: Option<String>,
    pub seen_estimate_secs: Option<f64>,
    pub seen_estimate_mem_mb: Option<f64>,
    pub queue_mode: bool,
    pub replace: bool,
    pub yes: bool,
    pub slow_timeout: Option<Duration>,
    pub terminate_after: Option<u32>,
    pub global_timeout: Option<Duration>,
    pub retries: u32,
    pub extra_cargo_args: Vec<String>,
    pub extra_test_args: Vec<String>,
    pub junit: Option<PathBuf>,
    pub allow_no_justification: bool,
    pub verbose: bool,
    pub worktree: Option<String>,
    pub sha: Option<String>,
}

pub struct RunResult {
    pub summary: RunSummary,
    pub run_id: String,
    pub estimate_secs: f64,
    pub estimate_mem_mb: f64,
    pub record: TimingRecord,
    pub budget_active_cores_after: usize,
    pub budget_active_mem_after: usize,
}

fn make_run_id(test: &str) -> String {
    let now = Utc::now();
    let pid = std::process::id();
    let rand_part: u32 = (rand::random::<u32>()) % 10000;
    format!(
        "{}-{}-{}-{:04}",
        now.format("%Y%m%d-%H%M%S"),
        test,
        pid,
        rand_part
    )
}

fn worktree_name() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".to_string())
}

fn git_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Full run flow. On any gate failure (justification / seen-estimate / approval)
/// returns Err(String) with human instructions and exit-code guidance (typed as Err).
pub fn run(req: RunRequest, cfg: &Config) -> Result<RunResult, String> {
    let cores = req.cores.max(1);
    let state_dir = cfg.effective_state_dir();
    let store = TimingStore::new(state_dir.join("timings.jsonl"));
    let est = store.estimate(&req.test, cores, req.cases);
    let estimate_secs = est.effective_secs;
    let estimate_mem_mb = est.effective_mem_mb;
    // Explicit --mem-mb wins over the estimate; otherwise budget the estimate.
    let mem_mb_requested = req
        .mem_mb
        .unwrap_or(estimate_mem_mb.ceil() as usize)
        .max(64);

    if req.verbose {
        eprintln!(
            "[fuzz-next] estimate for `{}` @ {} cores / {} MiB: time p50={:?} p95={:?} eff={:.0}s mem p95={:?} eff={:.0}MiB n={} {}",
            req.test,
            cores,
            mem_mb_requested,
            est.p50,
            est.p95,
            estimate_secs,
            est.mem_p95,
            estimate_mem_mb,
            est.n,
            if est.from_defaults { "(defaults)" } else { "" }
        );
        eprintln!("[fuzz-next] {}", est.notes);
        eprintln!(
            "[fuzz-next] cost: {:.0} core-s + {:.0} mem-s ({}s x {}MiB)",
            estimate_secs * cores as f64,
            est.effective_mem_seconds,
            estimate_secs,
            estimate_mem_mb,
        );
    }

    // Gate: justification + hypothesis (unless explicitly allowed away for tests/CI).
    let just_words = if req.allow_no_justification {
        0
    } else {
        match validate_justification(req.justification.as_deref(), req.hypothesis.as_deref()) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[fuzz-next] BLOCKED: {e}");
                print_estimate_and_justification_hint(&est, cores, req.cases);
                return Err(e);
            }
        }
    };

    // Gate: seen estimate must match store (proves agent ran `estimate` first).
    if !req.allow_no_justification {
        if let Err(e) = validate_seen_estimate(req.seen_estimate_secs, estimate_secs) {
            eprintln!("[fuzz-next] BLOCKED: {e}");
            print_estimate_and_justification_hint(&est, cores, req.cases);
            return Err(e);
        }
        if let Err(e) =
            crate::timings::validate_seen_estimate_mem(req.seen_estimate_mem_mb, estimate_mem_mb)
        {
            eprintln!("[fuzz-next] BLOCKED: {e}");
            print_estimate_and_justification_hint(&est, cores, req.cases);
            return Err(e);
        }
    }

    // Gates: long-run / big-mem approval. >threshold needs --yes.
    let time_over = estimate_secs > cfg.approval_threshold.as_secs_f64();
    let mem_over = estimate_mem_mb > cfg.mem_approval_threshold_mb as f64;
    if (time_over || mem_over) && !req.yes && !req.allow_no_justification {
        let mut msg = String::new();
        if time_over {
            msg.push_str(&format!(
                "estimated {:.0}s exceeds approval threshold {} ",
                estimate_secs,
                fmt_dur(cfg.approval_threshold),
            ));
        }
        if mem_over {
            msg.push_str(&format!(
                "estimated {:.0}MiB exceeds memory approval threshold {}MiB ",
                estimate_mem_mb, cfg.mem_approval_threshold_mb,
            ));
        }
        msg.push_str(&format!(
            "— rerun with --yes after reading the estimate and writing the justification. This run asks for {:.0} core-seconds + {:.0} mem-seconds ({}s x {} cores / {}MiB).",
            estimate_secs * cores as f64,
            est.effective_mem_seconds,
            estimate_secs,
            cores,
            estimate_mem_mb as usize,
        ));
        eprintln!("[fuzz-next] NEEDS-APPROVAL: {msg}");
        print_estimate_and_justification_hint(&est, cores, req.cases);
        return Err(format!("NEEDS-APPROVAL: {msg}"));
    }

    // Budget admission (cores + memory, both must fit).
    let run_id = make_run_id(&req.test);
    let broker = Broker::with_mem(state_dir.clone(), cfg.budget_cores, cfg.budget_mem_mb);
    let worktree = req.worktree.clone().unwrap_or_else(worktree_name);
    let pid = std::process::id() as i32;
    let sha = req.sha.clone().or_else(git_sha);

    // Reap / pre-check for ETA display.
    let active_cores = broker
        .active_cores_now()
        .map_err(|e| format!("broker: {e}"))?;
    let active_mem = broker
        .active_mem_now()
        .map_err(|e| format!("broker: {e}"))?;
    if req.verbose {
        eprintln!(
            "[fuzz-next] budget {active_cores}/{} cores + {active_mem}/{} MiB in use",
            cfg.budget_cores, cfg.budget_mem_mb
        );
    }

    let admission = broker
        .try_admit(
            &run_id,
            pid,
            &req.test,
            cores,
            mem_mb_requested,
            estimate_secs,
            Some(worktree.clone()),
            just_words,
            req.queue_mode,
        )
        .map_err(|e| format!("broker: {e}"))?;

    match admission {
        Admission::Admitted { .. } => {}
        Admission::Queued {
            position,
            eta_start_secs,
            eta_finish_secs,
        } => {
            eprintln!(
                "[fuzz-next] QUEUED #{position} for {}: starts in ~{}s, finishes in ~{}s ({} cores / {}MiB). Waiting…",
                req.test,
                eta_start_secs as i64,
                eta_finish_secs as i64,
                cores,
                mem_mb_requested,
            );
            if req.replace {
                if let Ok(Some(victim)) = broker.replace_victim(cores, mem_mb_requested) {
                    eprintln!("[fuzz-next] --replace: cancelled victim {victim} to free cores+mem");
                }
            }
            // Block until admitted (poll).
            crate::budget::wait_until_admitted(
                &broker,
                &run_id,
                pid,
                &req.test,
                cores,
                mem_mb_requested,
                estimate_secs,
                Some(worktree.clone()),
                just_words,
                req.verbose,
            )
            .map_err(|e| format!("wait: {e}"))?;
        }
        Admission::Rejected {
            active_cores,
            budget_cores,
            active_mem_mb,
            budget_mem_mb,
            reason,
            eta_start_secs,
            eta_finish_secs,
        } => {
            let msg = format!(
                "REJECTED ({reason}): cores {active_cores}/{budget_cores}, mem {active_mem_mb}/{budget_mem_mb} MiB in use. Need {cores} cores / {mem_mb_requested}MiB. Starts in ~{eta_start_secs:.0}s, finishes in ~{eta_finish_secs:.0}s. Rerun without --no-queue (queue mode) or with --replace to cancel a victim.",
            );
            eprintln!("[fuzz-next] {msg}");
            return Err(msg);
        }
    }

    // Ensure run dir.
    let runs_dir = state_dir.join("runs");
    std::fs::create_dir_all(&runs_dir).map_err(|e| format!("{e}"))?;
    let run_dir = runs_dir.join(&run_id);
    std::fs::create_dir_all(&run_dir).map_err(|e| format!("{e}"))?;

    // Build command + retry policy.
    let retry = if req.retries > 0 {
        Some(RetryPolicy::new(req.retries))
    } else {
        None
    };
    // Pre-build so executor retry loop can re-spawn (Command is re-spinnable).
    let cmd = build_test_command(
        &req.test,
        cores,
        req.release,
        &req.extra_cargo_args,
        &req.extra_test_args,
    );
    let slow = req.slow_timeout.or(Some(cfg.slow_timeout));
    let outcome = run_with_timeouts(
        cmd,
        &run_dir,
        slow,
        req.terminate_after,
        req.global_timeout,
        retry,
        req.verbose,
    )
    .map_err(|e| format!("exec: {e}"))?;

    let secs = outcome.secs;
    let record = TimingRecord {
        ts: Utc::now(),
        test: req.test.clone(),
        binary: Some(req.test.clone()),
        secs,
        cores,
        rc: outcome.rc,
        status: outcome.status.clone(),
        sha,
        cases: req.cases,
        worktree: Some(worktree.clone()),
        run_id: run_id.clone(),
        justification: req.justification.clone(),
        hypothesis: req.hypothesis.clone(),
        seen_estimate_secs: req.seen_estimate_secs,
        mem_mb_requested: Some(mem_mb_requested),
        peak_rss_mb: outcome.peak_rss_mb,
        seen_estimate_mem_mb: req.seen_estimate_mem_mb,
    };
    // Best-effort timing append; failure shouldn't fail the run but warn.
    if let Err(e) = store.append(&record) {
        eprintln!("[fuzz-next] WARN: failed to append timings.jsonl: {e}");
    }

    let mut summary =
        RunSummary::from_record(&record, estimate_secs, estimate_mem_mb, outcome.attempts);
    summary.stdout_path = outcome.stdout_path.to_string_lossy().to_string();
    summary.stderr_path = outcome.stderr_path.to_string_lossy().to_string();

    // Reports.
    let _ = write_result_json(&run_dir.join("result.json"), &summary, &record);
    if let Some(junit_path) = &req.junit {
        if let Err(e) = write_junit(junit_path, std::slice::from_ref(&summary)) {
            eprintln!("[fuzz-next] WARN junit: {e}");
        }
    } else {
        // Default junit next to run dir for CI.
        let _ = write_junit(&run_dir.join("junit.xml"), std::slice::from_ref(&summary));
    }

    if req.verbose || outcome.rc != 0 {
        println!("{}", format_human(&summary));
        if outcome.rc != 0 {
            println!("[fuzz-next] tail of stdout/stderr:");
            print!("{}", outcome.tail);
            if outcome.tail.len() > 4000 {
                eprintln!("[fuzz-next] full logs: {}", outcome.stdout_path.display());
            }
        }
    } else {
        println!("{}", format_human(&summary));
    }

    // Release budget (drop session entry).
    let _ = broker.release(&run_id);
    let after_cores = broker.active_cores_now().unwrap_or(0);
    let after_mem = broker.active_mem_now().unwrap_or(0);
    Ok(RunResult {
        summary,
        run_id,
        estimate_secs,
        estimate_mem_mb,
        record,
        budget_active_cores_after: after_cores,
        budget_active_mem_after: after_mem,
    })
}

fn print_estimate_and_justification_hint(
    est: &crate::timings::Estimate,
    cores: usize,
    cases: Option<u64>,
) {
    println!("--- estimate ---");
    println!("test:  {}", est.test);
    println!("cores: {cores}  cases: {cases:?}");
    println!(
        "p50: {:?}  p95: {:?}  p99: {:?}  n={}",
        est.p50, est.p95, est.p99, est.n
    );
    println!(
        "effective (p95): {:.0}s  ({})",
        est.effective_secs, est.notes
    );
    println!(
        "effective mem (p95): {:.0} MiB (mem p50={:?} p95={:?})",
        est.effective_mem_mb, est.mem_p50, est.mem_p95
    );
    println!(
        "core-seconds: {:.0} ({}s x {} cores)  mem-seconds: {:.0} ({}s x {:.0}MiB)",
        est.effective_secs * cores as f64,
        est.effective_secs,
        cores,
        est.effective_mem_seconds,
        est.effective_secs,
        est.effective_mem_mb,
    );
    println!();
    println!("--- required justification + hypothesis (200+ words combined) ---");
    println!("The run command must include:");
    println!(
        "  --justification \"<why this run deserves {:.0} core-s + {:.0} mem-s RIGHT NOW>\" \\",
        est.effective_secs * cores as f64,
        est.effective_mem_seconds,
    );
    println!(
        "  --hypothesis \"<what we expect to learn / falsify, and what decision it unlocks>\" \\"
    );
    println!("  --seen-estimate-secs {:.0} \\", est.effective_secs);
    println!("  --seen-estimate-mem-mb {:.0}", est.effective_mem_mb);
    println!();
    println!("Template questions to answer in 200+ words:");
    println!("1. What exact question does this run answer, and which alternative (cheaper test, reading code, waiting) did we rule out?");
    println!("2. Why is THIS test the right tool (what invariant / proof / soak parameter does it cover)?");
    println!("3. Why NOW — what blocked us before, what unblocks after? What decision hinges on the result?");
    println!("4. Why these CORES and this CASES count — what does the timing history say, and what breaks if we use half?");
    println!("5. What is the hypothesis and falsifier (pre-registered expectation + what output would refute it)?");
    println!("6. If it flakes/fails, what do we do next (rerun, shrink, file bug) and what's the cost of finding out late?");
    println!("7. What are we giving up by taking these cores (which other queued runs wait, ETA impact)?");
}

/// Parse a duration arg like "--slow-timeout 60s".
#[must_use]
pub fn opt_duration(v: Option<String>) -> Option<Duration> {
    v.and_then(|s| parse_duration(&s))
}

#[cfg(test)]
mod tests_runner {
    use super::*;
    #[test]
    fn run_id_unique_per_call() {
        let a = make_run_id("x");
        let b = make_run_id("x");
        assert_ne!(a, b);
        assert!(a.contains("x"));
    }
    #[test]
    fn justification_gate_rejects_short() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            state_dir: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        let req = RunRequest {
            test: "rooms".into(),
            cores: 2,
            mem_mb: None,
            release: false,
            cases: None,
            justification: Some("too short".into()),
            hypothesis: Some("also short".into()),
            seen_estimate_secs: Some(30.0),
            seen_estimate_mem_mb: Some(400.0),
            queue_mode: false,
            replace: false,
            yes: false,
            slow_timeout: None,
            terminate_after: None,
            global_timeout: None,
            retries: 0,
            extra_cargo_args: vec![],
            extra_test_args: vec![],
            junit: None,
            allow_no_justification: false,
            verbose: false,
            worktree: None,
            sha: None,
        };
        let r = run(req, &cfg);
        assert!(r.is_err());
        let e = r.err().unwrap();
        assert!(e.contains("200"));
    }
    #[test]
    fn seen_estimate_gate_rejects_missing() {
        // Long justification but no seen estimate -> blocked.
        let justification = "word ".repeat(120);
        let hypothesis = "word ".repeat(90);
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            state_dir: Some(dir.path().to_path_buf()),
            ..Config::default()
        };
        let req = RunRequest {
            test: "rooms".into(),
            cores: 2,
            mem_mb: None,
            release: false,
            cases: None,
            justification: Some(justification),
            hypothesis: Some(hypothesis),
            seen_estimate_secs: None,
            seen_estimate_mem_mb: None,
            queue_mode: false,
            replace: false,
            yes: false,
            slow_timeout: None,
            terminate_after: None,
            global_timeout: None,
            retries: 0,
            extra_cargo_args: vec![],
            extra_test_args: vec![],
            junit: None,
            allow_no_justification: false,
            verbose: false,
            worktree: None,
            sha: None,
        };
        let r = run(req, &cfg);
        assert!(r.is_err());
    }
    #[test]
    fn count_words_works_for_gate() {
        assert_eq!(count_words("a b c"), 3);
        assert_eq!(count_words("  a   b\n\nc  "), 3);
    }
}
