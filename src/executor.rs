//! Executor: spawn a test command with timeouts, slow warnings, retries.
//!
//! Nextest slice: slow-timeout (SLOW marker), slow-timeout+terminate-after
//! (SIGTERM then SIGKILL after grace), global timeout, retries with backoff
//! (flaky = passed on retry), captured stdout/stderr to run dir (never piped
//! through `head` so panics are never lost).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::timings::TimingRecord;
use crate::util::RetryPolicy;

#[derive(Debug, Clone)]
pub struct ExecOutcome {
    pub rc: i32,
    pub secs: f64,
    pub status: String,
    pub flaky: bool,
    pub attempts: u32,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub tail: String,
    /// Peak RSS (MiB) of the whole process tree while running, if sampled.
    pub peak_rss_mb: Option<f64>,
}

/// Sample total RSS (MiB) for `root_pid` + all descendants via /proc (unix).
/// Returns None on Windows / if unavailable. Best-effort for budget history.
#[must_use]
pub fn sample_tree_rss_mb(root_pid: i32) -> Option<f64> {
    #[cfg(unix)]
    {
        use std::collections::HashSet;
        // Build pid->ppid map once per sample.
        let mut ppid: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
        let mut rss: std::collections::HashMap<i32, f64> = std::collections::HashMap::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return None;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(pid) = name.parse::<i32>() else {
                continue;
            };
            let status_path = entry.path().join("status");
            let Ok(text) = std::fs::read_to_string(status_path) else {
                continue;
            };
            let mut pp = None;
            let mut vmrss_kb = None;
            for line in text.lines() {
                if line.starts_with("PPid:") {
                    pp = line
                        .split_whitespace()
                        .nth(1)
                        .and_then(|s| s.parse::<i32>().ok());
                }
                if line.starts_with("VmRSS:") {
                    vmrss_kb = line
                        .split_whitespace()
                        .nth(1)
                        .and_then(|s| s.parse::<f64>().ok());
                }
            }
            if let Some(ppv) = pp {
                ppid.insert(pid, ppv);
            }
            if let Some(kb) = vmrss_kb {
                rss.insert(pid, kb / 1024.0);
            }
        }
        let mut subtree: HashSet<i32> = HashSet::new();
        subtree.insert(root_pid);
        // Expand until fixpoint (bounded by /proc size).
        loop {
            let mut added = false;
            for (pid, parent) in &ppid {
                if subtree.contains(parent) && !subtree.contains(pid) {
                    subtree.insert(*pid);
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        let total_mb: f64 = subtree
            .iter()
            .map(|p| rss.get(p).copied().unwrap_or(0.0))
            .sum();
        if total_mb > 0.0 {
            Some(total_mb)
        } else {
            None
        }
    }
    #[cfg(not(unix))]
    {
        let _ = root_pid;
        None
    }
}

/// Build the cargo-test command for a per-binary run, with cooperative thread caps.
///
/// Caps are env-based (cross-platform, no taskset/systemd): child processes
/// read `CARGO_BUILD_JOBS`, `JOINT_FUZZ_THREADS`, `RAYON_NUM_THREADS`,
/// `TOKIO_WORKER_THREADS` and `NEXTEST_TEST_THREADS` for their internal pools.
#[must_use]
pub fn build_test_command(
    test: &str,
    cores: usize,
    release: bool,
    extra_cargo_args: &[String],
    extra_test_args: &[String],
) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.arg("test");
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--test").arg(test);
    for a in extra_cargo_args {
        cmd.arg(a);
    }
    if !extra_test_args.is_empty() {
        cmd.arg("--");
        for a in extra_test_args {
            cmd.arg(a);
        }
    }
    let c = cores.max(1).to_string();
    cmd.env("CARGO_BUILD_JOBS", &c)
        .env("JOINT_FUZZ_THREADS", &c)
        .env("RAYON_NUM_THREADS", &c)
        .env("TOKIO_WORKER_THREADS", &c)
        .env("NEXTEST_TEST_THREADS", &c)
        .env("CARGO_TERM_QUIET", "false");
    cmd
}

/// Run `cmd` with timeout semantics. `run_dir` must exist; logs go there.
/// `slow_timeout` only logs SLOW (None = off). `terminate_after` kills after
/// `slow_timeout * terminate_after` of wall time (None = never terminate).
pub fn run_with_timeouts(
    mut cmd: Command,
    run_dir: &Path,
    slow_timeout: Option<Duration>,
    terminate_after: Option<u32>,
    global_timeout: Option<Duration>,
    retry: Option<RetryPolicy>,
    verbose: bool,
) -> std::io::Result<ExecOutcome> {
    let max_attempts = retry.as_ref().map(|r| r.count + 1).unwrap_or(1);
    let mut last_rc = 0i32;
    let mut last_status = "fail".to_string();
    let mut last_secs = 0.0;
    let mut flaky = false;
    let (stdout_path, stderr_path) = (run_dir.join("stdout.log"), run_dir.join("stderr.log"));

    for attempt in 1..=max_attempts {
        if attempt > 1 {
            if verbose {
                eprintln!("[fuzz-next] retry attempt {attempt}/{max_attempts}");
            }
        }
        // Fresh log handles per attempt (attempt suffix keeps history).
        let (out_p, err_p) = if attempt == 1 {
            (stdout_path.clone(), stderr_path.clone())
        } else {
            (
                run_dir.join(format!("stdout.try{attempt}.log")),
                run_dir.join(format!("stderr.try{attempt}.log")),
            )
        };
        let out_file = File::create(&out_p)?;
        let err_file = File::create(&err_p)?;
        cmd.stdout(Stdio::from(out_file));
        cmd.stderr(Stdio::from(err_file));

        // New process group on unix so terminate kills the whole cargo subtree.
        #[cfg(unix)]
        {
            use nix::unistd::{setpgid, Pid};
            use std::os::unix::process::CommandExt;
            // SAFETY: pre_exec runs in forked child before exec.
            unsafe {
                cmd.pre_exec(|| {
                    setpgid(Pid::from_raw(0), Pid::from_raw(0))?;
                    Ok(())
                });
            }
        }

        let start = Instant::now();
        let mut child = cmd.spawn()?;
        let child_pid = child.id() as i32;
        let mut slow_warned = false;
        let mut timed_out = false;
        let mut peak_rss_mb: Option<f64> = None;
        let mut last_sample = Instant::now();

        let terminate_at = global_timeout.or_else(|| {
            slow_timeout
                .zip(terminate_after)
                .map(|(st, n)| st * n.max(1))
        });

        loop {
            // Sample peak RSS every ~200ms (cheap /proc walk on unix).
            if last_sample.elapsed() >= Duration::from_millis(200) {
                if let Some(mb) = sample_tree_rss_mb(child_pid) {
                    peak_rss_mb = Some(match peak_rss_mb {
                        Some(prev) => prev.max(mb),
                        None => mb,
                    });
                }
                last_sample = Instant::now();
            }
            match child.try_wait()? {
                Some(status) => {
                    let secs = start.elapsed().as_secs_f64();
                    let rc = status.code().unwrap_or(-1);
                    last_rc = rc;
                    last_secs = secs;
                    if timed_out {
                        last_status = "timeout".to_string();
                    } else if rc == 0 {
                        last_status = "pass".to_string();
                    } else {
                        last_status = "fail".to_string();
                    }
                    if attempt > 1 && rc == 0 {
                        flaky = true;
                    }
                    break;
                }
                None => {
                    let elapsed = start.elapsed();
                    if let Some(st) = slow_timeout {
                        if !slow_warned && elapsed >= st && !timed_out {
                            if verbose {
                                eprintln!(
                                    "[fuzz-next] SLOW: still running after {} (test may be long)",
                                    fmt_dur(st)
                                );
                            }
                            slow_warned = true;
                        }
                    }
                    if let Some(ta) = terminate_at {
                        if elapsed >= ta && !timed_out {
                            if verbose {
                                eprintln!(
                                    "[fuzz-next] TIMEOUT after {} — terminating",
                                    fmt_dur(ta)
                                );
                            }
                            timed_out = true;
                            terminate_child(&mut child);
                            // Give SIGTERM a brief grace, then force.
                            std::thread::sleep(Duration::from_millis(200));
                            let _ = child.kill();
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        // One final sample at exit.
        if let Some(mb) = sample_tree_rss_mb(child_pid) {
            peak_rss_mb = Some(match peak_rss_mb {
                Some(prev) => prev.max(mb),
                None => mb,
            });
        }
        // Collect tail for failure summaries.
        let tail = read_tail(&out_p, 50) + &read_tail(&err_p, 50);
        if last_rc == 0 || attempt == max_attempts {
            return Ok(ExecOutcome {
                rc: last_rc,
                secs: last_secs,
                status: if flaky {
                    "flaky".to_string()
                } else {
                    last_status
                },
                flaky,
                attempts: attempt,
                stdout_path: out_p,
                stderr_path: err_p,
                tail,
                peak_rss_mb,
            });
        }
        // Failed but retries remain.
        if let Some(policy) = &retry {
            let delay = policy.delay_for_attempt(attempt);
            if verbose {
                eprintln!(
                    "[fuzz-next] attempt {attempt} failed (rc={last_rc}); retrying in {}",
                    fmt_dur(delay)
                );
            }
            std::thread::sleep(delay);
            // Rebuild command for next attempt (Command is consumed by spawn).
            // Caller handles this by rebuilding outside? Instead we clone via build again.
            // To keep API simple, executor expects caller to rebuild — see runner.
            // Here we break to let caller rebuild. Mark last outcome.
            let _ = tail;
        }
        // For simplicity, if retry policy present, caller (runner) loops externally
        // and passes a fresh Command each time. This loop still handles the first attempt.
        // We signal the caller by returning the failure so it can decide to retry.
        if retry.is_some() && attempt < max_attempts {
            // Continue loop with same `cmd` — but std::process::Command cannot be
            // re-spawned after spawn (it can! spawn takes &mut self). It reuses env/args,
            // only stdout/stderr were overwritten above. So loop safe.
            continue;
        }
        return Ok(ExecOutcome {
            rc: last_rc,
            secs: last_secs,
            status: last_status.clone(),
            flaky: false,
            attempts: attempt,
            stdout_path: out_p,
            stderr_path: err_p,
            tail,
            peak_rss_mb,
        });
    }
    Ok(ExecOutcome {
        rc: last_rc,
        secs: last_secs,
        status: last_status,
        flaky,
        attempts: max_attempts,
        stdout_path: stdout_path.clone(),
        stderr_path: stderr_path.clone(),
        tail: String::new(),
        peak_rss_mb: None,
    })
}

fn terminate_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        unsafe {
            let pid = child.id() as i32;
            let _ = libc::kill(pid, libc::SIGTERM);
            // Also try negative pgid targeting the process group.
            let _ = libc::kill(-pid, libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .output();
    }
}

fn read_tail(path: &Path, lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let ls: Vec<&str> = text.lines().collect();
    let start = ls.len().saturating_sub(lines);
    ls[start..].join("\n")
}

#[must_use]
pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        format!("{s:.0}s")
    } else if s < 3600.0 {
        format!("{:.1}m", s / 60.0)
    } else {
        format!("{:.1}h", s / 3600.0)
    }
}

/// Record a finished run into timings.jsonl (shared helper for runner + tests).
pub fn record_timing(store_path: &Path, rec: TimingRecord) -> std::io::Result<()> {
    crate::timings::TimingStore::new(store_path.to_path_buf()).append(&rec)
}

#[cfg(test)]
mod tests_executor {
    use super::*;

    #[test]
    fn build_command_sets_thread_caps() {
        let cmd = build_test_command("native_fuzz", 8, true, &[], &[]);
        // Inspect via debug string (contains env keys).
        let dbg = format!("{cmd:?}");
        assert!(dbg.contains("JOINT_FUZZ_THREADS"));
        assert!(dbg.contains("CARGO_BUILD_JOBS"));
    }

    #[test]
    fn run_with_timeouts_captures_output_and_returns_rc() {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo hello-out; echo hello-err >&2; exit 0");
        let out = run_with_timeouts(
            cmd,
            dir.path(),
            Some(Duration::from_secs(60)),
            None,
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(out.rc, 0);
        assert_eq!(out.status, "pass");
        let stdout = std::fs::read_to_string(&out.stdout_path).unwrap();
        assert!(stdout.contains("hello-out"));
        let stderr = std::fs::read_to_string(&out.stderr_path).unwrap();
        assert!(stderr.contains("hello-err"));
    }

    #[test]
    fn run_with_timeouts_slow_and_terminate() {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 5; echo done");
        // slow at 100ms, terminate after 1*100ms => ~100ms timeout.
        let out = run_with_timeouts(
            cmd,
            dir.path(),
            Some(Duration::from_millis(100)),
            Some(1),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(out.status, "timeout");
        assert!(out.secs < 4.0);
    }

    #[test]
    fn retries_turn_fail_into_flaky() {
        let dir = tempfile::tempdir().unwrap();
        // Fail first time, pass second (via marker file).
        let marker = dir.path().join("marker");
        let script = format!(
            "if [ -f {} ]; then echo ok; exit 0; else touch {}; echo boom; exit 1; fi",
            marker.display(),
            marker.display()
        );
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&script);
        let mut policy = RetryPolicy::new(1);
        policy.delay = Duration::from_millis(10);
        policy.max_delay = Duration::from_millis(50);
        policy.jitter = false;
        let out = run_with_timeouts(
            cmd,
            dir.path(),
            Some(Duration::from_secs(60)),
            None,
            None,
            Some(policy),
            false,
        )
        .unwrap();
        assert_eq!(out.rc, 0);
        assert!(out.flaky);
        assert_eq!(out.attempts, 2);
    }

    #[test]
    fn fmt_dur_formats() {
        assert_eq!(fmt_dur(Duration::from_secs(30)), "30s");
        assert_eq!(fmt_dur(Duration::from_secs(300)), "5.0m");
    }
}
