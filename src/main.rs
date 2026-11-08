use std::ffi::OsString;
use std::process::{Command as ProcCommand, Stdio};
use std::time::Duration;

use cargo_fuzz_next::budget::{format_eta, Broker};
use cargo_fuzz_next::cli::{config_for, parse, run_request_from, Command};
use cargo_fuzz_next::poll::{self, PollCheck, WaitOutcome};
use cargo_fuzz_next::timings::TimingStore;

fn main() -> std::process::ExitCode {
    let cli = parse();
    match cli.command {
        Command::Run(args) => {
            let state_dir = args.state_dir.clone();
            let budget = args.budget_cores;
            let budget_mem = args.budget_mem_mb;
            let verbose = args.verbose;
            let json = args.json;
            let background = args.background;
            let background_child = args.background_child;
            let explicit_run_id = args.run_id.clone();
            let cfg = config_for(state_dir, budget, budget_mem);
            let req = run_request_from(args);
            if background && !background_child {
                // --background: validate gates synchronously, then detach.
                let run_id = explicit_run_id
                    .clone()
                    .unwrap_or_else(|| cargo_fuzz_next::runner::make_run_id(&req.test));
                if let Err(e) = cargo_fuzz_next::runner::validate_run_id(&run_id) {
                    eprintln!("[fuzz-next] {e}");
                    return 2.into();
                }
                // Fail fast on a duplicate id (already finished / active / queued).
                let state = cfg.effective_state_dir();
                let store = TimingStore::new(state.join("timings.jsonl"));
                if store
                    .load_all()
                    .iter()
                    .any(|r| r.run_id == run_id)
                {
                    eprintln!("[fuzz-next] duplicate --run-id {run_id}: already in timings history");
                    return 2.into();
                }
                let broker = Broker::with_mem(
                    state.clone(),
                    cfg.budget_cores,
                    cfg.budget_mem_mb,
                );
                let dup_active = broker
                    .list_active()
                    .unwrap_or_default()
                    .iter()
                    .any(|r| r.id == run_id);
                let dup_queue = broker
                    .list_queue()
                    .unwrap_or_default()
                    .iter()
                    .any(|q| q.id == run_id);
                if dup_active || dup_queue {
                    eprintln!("[fuzz-next] duplicate --run-id {run_id}: already active/queued");
                    return 2.into();
                }
                if let Err(e) = cargo_fuzz_next::runner::validate_preflight(&req, &cfg) {
                    eprintln!("[fuzz-next] {e}");
                    // 2 = gate, 3 = budget reject (mirrors foreground path).
                    if e.starts_with("REJECTED") {
                        return 3.into();
                    }
                    return 2.into();
                }
                match spawn_background(&cfg, &run_id) {
                    Ok(log_path) => {
                        if json {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "run_id": run_id,
                                    "test": req.test,
                                    "log": log_path.display().to_string(),
                                    "state_dir": state.display().to_string(),
                                })
                            );
                        } else {
                            println!("{run_id}");
                            eprintln!(
                                "[fuzz-next] background run {run_id} started (log: {}); wait with `cargo fuzz-next wait {run_id}` (alias `poll`)",
                                log_path.display()
                            );
                        }
                        return 0.into();
                    }
                    Err(e) => {
                        eprintln!("[fuzz-next] background spawn failed: {e}");
                        if e.contains("duplicate") {
                            return 2.into();
                        }
                        return 1.into();
                    }
                }
            }
            match cargo_fuzz_next::runner::run(req, &cfg) {
                Ok(res) => {
                    if verbose {
                        eprintln!(
                            "[fuzz-next] done {} in {:.1}s (budget now {}/{} cores, {}/{} MiB)",
                            res.run_id,
                            res.summary.secs,
                            res.budget_active_cores_after,
                            cfg.budget_cores,
                            res.budget_active_mem_after,
                            cfg.budget_mem_mb,
                        );
                    }
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "run_id": res.run_id,
                                "test": res.summary.test,
                                "secs": res.summary.secs,
                                "cores": res.summary.cores,
                                "mem_mb": res.summary.mem_mb,
                                "peak_rss_mb": res.summary.peak_rss_mb,
                                "core_seconds": res.summary.core_seconds,
                                "mem_seconds": res.summary.mem_seconds,
                                "rc": res.summary.rc,
                                "status": res.summary.status,
                                "flaky": res.summary.flaky,
                                "attempts": res.summary.attempts,
                                "estimate_secs": res.estimate_secs,
                                "estimate_mem_mb": res.estimate_mem_mb,
                                "stdout": res.summary.stdout_path,
                                "stderr": res.summary.stderr_path,
                            })
                        );
                    } else if !verbose {
                        // Human summary is printed by runner; nothing extra here.
                    }
                    if res.summary.rc == 0 {
                        0.into()
                    } else {
                        1.into()
                    }
                }
                Err(e) => {
                    eprintln!("[fuzz-next] {e}");
                    // 2 = gate (justification/estimate/approval/duplicate), 3 = budget reject.
                    if e.starts_with("NEEDS-APPROVAL")
                        || e.contains("words")
                        || e.contains("seen-estimate")
                        || e.contains("justification")
                        || e.contains("duplicate")
                    {
                        2.into()
                    } else if e.starts_with("REJECTED") {
                        3.into()
                    } else {
                        1.into()
                    }
                }
            }
        }
        Command::Estimate(args) => {
            let cfg = config_for(args.state_dir, None, None);
            let store = TimingStore::new(cfg.effective_state_dir().join("timings.jsonl"));
            let est = store.estimate(&args.test, args.cores, args.cases);
            println!("test:  {}", est.test);
            println!("cores: {}", est.cores);
            println!("cases: {:?}", args.cases);
            println!("p50: {:?}  p95: {:?}  p99: {:?}", est.p50, est.p95, est.p99);
            println!("n: {}  last: {:?}", est.n, est.last_secs);
            println!(
                "effective (p95): {:.0}s  core-seconds: {:.0}",
                est.effective_secs,
                est.effective_secs * args.cores as f64
            );
            println!(
                "effective mem (p95): {:.0} MiB (p50={:?} p95={:?})  mem-seconds: {:.0}",
                est.effective_mem_mb, est.mem_p50, est.mem_p95, est.effective_mem_seconds
            );
            println!("from_defaults: {}", est.from_defaults);
            println!("notes: {}", est.notes);
            println!();
            println!(
                "copy into `run` as: --seen-estimate-secs {:.0} --seen-estimate-mem-mb {:.0}",
                est.effective_secs, est.effective_mem_mb
            );
            0.into()
        }
        Command::Status(args) => {
            let cfg = config_for(
                args.state_dir.clone(),
                args.budget_cores,
                args.budget_mem_mb,
            );
            let broker = Broker::with_mem(
                cfg.effective_state_dir(),
                cfg.budget_cores,
                cfg.budget_mem_mb,
            );
            if args.poll {
                let interval = poll::resolve_duration(
                    args.interval.as_deref(),
                    Duration::from_secs(2),
                );
                let timeout = match args.timeout.as_ref() {
                    Some(s) => match cargo_fuzz_next::util::parse_duration(s) {
                        Some(d) => Some(d),
                        None => {
                            eprintln!("[fuzz-next] invalid --timeout {s:?}");
                            return 2.into();
                        }
                    },
                    None => None,
                };
                let budget = cfg.budget_cores;
                let budget_mem = cfg.budget_mem_mb;
                let json = args.json;
                let idle = poll::wait_until_idle(&broker, interval, timeout, |active, queue| {
                    if json {
                        // Progress ticks as JSON lines on stderr so stdout stays
                        // a single final object.
                        eprintln!(
                            "{}",
                            serde_json::json!({
                                "active": active.len(),
                                "queued": queue.len(),
                            })
                        );
                    } else {
                        let used: usize = active.iter().map(|r| r.cores).sum();
                        let used_mem: usize = active.iter().map(|r| r.mem_mb).sum();
                        eprintln!(
                            "[fuzz-next] poll status: {used}/{budget} cores + {used_mem}/{budget_mem} MiB in use ({} active, {} queued)",
                            active.len(),
                            queue.len()
                        );
                    }
                });
                if idle.timed_out {
                    eprintln!(
                        "[fuzz-next] status --poll timed out after {:.1}s ({} active, {} queued)",
                        idle.elapsed.as_secs_f64(),
                        idle.active.len(),
                        idle.queue.len()
                    );
                    return 124.into();
                }
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "active": 0,
                            "queued": 0,
                            "budget_cores": budget,
                            "budget_mem_mb": budget_mem,
                            "elapsed_secs": idle.elapsed.as_secs_f64(),
                        })
                    );
                    return 0.into();
                }
                eprintln!(
                    "[fuzz-next] idle after {:.1}s (budget now 0/{budget} cores)",
                    idle.elapsed.as_secs_f64()
                );
                return print_status(&broker, budget, budget_mem);
            }
            if args.json {
                match (broker.list_active(), broker.list_queue()) {
                    (Ok(active), Ok(queue)) => {
                        let used: usize = active.iter().map(|r| r.cores).sum();
                        let used_mem: usize = active.iter().map(|r| r.mem_mb).sum();
                        println!(
                            "{}",
                            serde_json::json!({
                                "used_cores": used,
                                "budget_cores": cfg.budget_cores,
                                "used_mem_mb": used_mem,
                                "budget_mem_mb": cfg.budget_mem_mb,
                                "active": active.iter().map(|r| serde_json::json!({
                                    "id": r.id, "test": r.test, "cores": r.cores,
                                    "mem_mb": r.mem_mb, "pid": r.pid,
                                    "started_at": r.started_at,
                                    "estimated_finish": r.estimated_finish,
                                    "worktree": r.worktree,
                                })).collect::<Vec<_>>(),
                                "queue": queue.iter().enumerate().map(|(i,q)| serde_json::json!({
                                    "id": q.id, "test": q.test, "cores": q.cores,
                                    "mem_mb": q.mem_mb, "position": i,
                                    "enqueued_at": q.enqueued_at,
                                    "worktree": q.worktree,
                                })).collect::<Vec<_>>(),
                            })
                        );
                        return 0.into();
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        eprintln!("status: {e}");
                        return 1.into();
                    }
                }
            }
            print_status(&broker, cfg.budget_cores, cfg.budget_mem_mb)
        }
        Command::Runs(args) => {
            let cfg = config_for(args.state_dir, None, None);
            let store = TimingStore::new(cfg.effective_state_dir().join("timings.jsonl"));
            let mut recs = store.load_all();
            recs.reverse();
            recs.truncate(args.last);
            if args.json {
                // Dump JSONL tail.
                for r in recs {
                    println!("{}", serde_json::to_string(&r).unwrap_or_default());
                }
            } else {
                for r in recs {
                    if let Some(t) = &args.test {
                        if &r.test != t {
                            continue;
                        }
                    }
                    println!(
                        "{} {:<16} {:>7.1}s x{} {:>6}/{:?}MiB {} {}",
                        r.ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        r.test,
                        r.secs,
                        r.cores,
                        (r.secs * r.cores as f64) as i64,
                        r.peak_rss_mb,
                        r.status,
                        r.run_id,
                    );
                }
            }
            0.into()
        }
        Command::Wait(args) => {
            if args.run_id.is_empty() && args.test.is_none() {
                eprintln!("[fuzz-next] wait needs a RUN_ID or --test (try `cargo fuzz-next wait --help`)");
                return 2.into();
            }
            if args.timeout.is_some() && args.timeout.as_deref().is_some_and(|s| cargo_fuzz_next::util::parse_duration(s).is_none()) {
                eprintln!("[fuzz-next] invalid --timeout {:?}", args.timeout);
                return 2.into();
            }
            if cargo_fuzz_next::util::parse_duration(&args.interval).is_none() {
                eprintln!("[fuzz-next] invalid --interval {:?}", args.interval);
                return 2.into();
            }
            let cfg = config_for(
                args.state_dir.clone(),
                args.budget_cores,
                args.budget_mem_mb,
            );
            let state_dir = cfg.effective_state_dir();
            let broker = Broker::with_mem(
                state_dir.clone(),
                cfg.budget_cores,
                cfg.budget_mem_mb,
            );
            let store = TimingStore::new(state_dir.join("timings.jsonl"));
            let interval = poll::resolve_interval(Some(&args.interval));
            let timeout = args.timeout.as_deref().and_then(cargo_fuzz_next::util::parse_duration);
            let verbose = args.verbose;
            let json = args.json;
            let show_tail = args.tail;
            let test_filter = args.test.clone();

            // Periodic progress to stderr (stdout stays clean for --json).
            let result = poll::wait_for_runs(
                &broker,
                &store,
                &state_dir,
                &args.run_id,
                test_filter.as_deref(),
                interval,
                timeout,
                |checks, finished| {
                    if verbose && !finished {
                        if checks.is_empty() {
                            eprintln!(
                                "[fuzz-next] wait --test {}: no active/queued run yet, waiting for one to appear…",
                                test_filter.as_deref().unwrap_or("?")
                            );
                        }
                        for c in checks {
                            eprintln!(
                                "[fuzz-next] wait {}: {}",
                                c.run_id,
                                poll::format_check_human(c)
                            );
                        }
                    }
                },
            );

            // Final output.
            match result.outcome {
                WaitOutcome::Passed => {
                    if json {
                        print_wait_json(&result.checks, &state_dir, result.elapsed);
                    } else {
                        for c in &result.checks {
                            println!("{}", poll::format_final_human(c));
                        }
                    }
                    if show_tail {
                        for c in &result.checks {
                            let tail = poll::read_run_tail(&state_dir, &c.run_id, 50);
                            if !tail.is_empty() {
                                println!("{tail}");
                            }
                        }
                    }
                    0.into()
                }
                WaitOutcome::Failed => {
                    if json {
                        print_wait_json(&result.checks, &state_dir, result.elapsed);
                    } else {
                        for c in &result.checks {
                            if c.phase == poll::PollPhase::Done {
                                println!("{}", poll::format_final_human(c));
                            }
                        }
                    }
                    // Failure tail is always shown (human mode, unless --json
                    // which already carries paths) so agents see why it failed.
                    if !json {
                        for c in &result.checks {
                            let tail = poll::read_run_tail(&state_dir, &c.run_id, 50);
                            if !tail.is_empty() {
                                println!("{tail}");
                            }
                        }
                    }
                    1.into()
                }
                WaitOutcome::Cancelled => {
                    if json {
                        print_wait_json(&result.checks, &state_dir, result.elapsed);
                    } else {
                        for c in &result.checks {
                            if c.phase == poll::PollPhase::Cancelled {
                                println!("CANCELLED {} (no timing record)", c.run_id);
                            } else if c.phase == poll::PollPhase::Done {
                                println!("{}", poll::format_final_human(c));
                            }
                        }
                    }
                    1.into()
                }
                WaitOutcome::Unknown => {
                    if json {
                        print_wait_json(&result.checks, &state_dir, result.elapsed);
                    } else {
                        for c in &result.checks {
                            eprintln!(
                                "[fuzz-next] unknown run {}: never saw it in active/queue/timings",
                                c.run_id
                            );
                        }
                        if result.checks.is_empty() {
                            eprintln!(
                                "[fuzz-next] unknown: no run of test {:?} ever appeared",
                                test_filter
                            );
                        }
                        eprintln!(
                            "[fuzz-next] hint: check the id (`cargo fuzz-next status`, `cargo fuzz-next runs`), or use `run --background --run-id <ID>` + `wait <ID>`"
                        );
                    }
                    2.into()
                }
                WaitOutcome::Timeout => {
                    if json {
                        print_wait_json(&result.checks, &state_dir, result.elapsed);
                    } else {
                        eprintln!(
                            "[fuzz-next] wait timed out after {:.1}s, still pending:",
                            result.elapsed.as_secs_f64()
                        );
                        for c in &result.checks {
                            eprintln!(
                                "  {}: {}",
                                c.run_id,
                                poll::format_check_human(c)
                            );
                        }
                        if result.checks.is_empty() {
                            eprintln!(
                                "  (no run of test {:?} appeared)",
                                test_filter
                            );
                        }
                    }
                    124.into()
                }
            }
        }
        Command::Cancel(args) => {
            let cfg = config_for(args.state_dir, args.budget_cores, None);
            let broker = Broker::with_mem(
                cfg.effective_state_dir(),
                cfg.budget_cores,
                cfg.budget_mem_mb,
            );
            match broker.cancel(&args.run_id) {
                Ok(true) => {
                    println!("cancelled {}", args.run_id);
                    0.into()
                }
                Ok(false) => {
                    eprintln!("no active run {}", args.run_id);
                    1.into()
                }
                Err(e) => {
                    eprintln!("{e}");
                    1.into()
                }
            }
        }
        Command::Clean => {
            let cfg = config_for(None, None, None);
            let broker = Broker::with_mem(
                cfg.effective_state_dir(),
                cfg.budget_cores,
                cfg.budget_mem_mb,
            );
            match broker.list_active() {
                Ok(active) => {
                    println!("active entries after reap: {}", active.len());
                    0.into()
                }
                Err(e) => {
                    eprintln!("{e}");
                    1.into()
                }
            }
        }
        Command::Doctor => {
            let cfg = config_for(None, None, None);
            let sd = cfg.effective_state_dir();
            println!("state_dir: {}", sd.display());
            println!("budget_cores: {}", cfg.budget_cores);
            println!("reserved_cores: {}", cfg.reserved_cores);
            println!("budget_mem_mb: {}", cfg.budget_mem_mb);
            println!("reserved_mem_mb: {}", cfg.reserved_mem_mb);
            println!(
                "total_physical_mem_mb: {}",
                cargo_fuzz_next::util::total_physical_mem_mb()
            );
            println!("approval_threshold: {:?}", cfg.approval_threshold);
            println!(
                "mem_approval_threshold_mb: {}",
                cfg.mem_approval_threshold_mb
            );
            println!("slow_timeout: {:?}", cfg.slow_timeout);
            println!("num_cpus: {}", cargo_fuzz_next::util::num_cpus());
            let store = TimingStore::new(sd.join("timings.jsonl"));
            println!("timings records: {}", store.load_all().len());
            for (t, n, p50, p95, last) in store.stats() {
                println!("  {t:<16} n={n} p50={p50:?} p95={p95:?} last={last:?}");
            }
            let broker = Broker::with_mem(sd.clone(), cfg.budget_cores, cfg.budget_mem_mb);
            let _ = print_status(&broker, cfg.budget_cores, cfg.budget_mem_mb);
            0.into()
        }
    }
}

fn print_wait_json(checks: &[PollCheck], state_dir: &std::path::Path, elapsed: Duration) {
    let arr: Vec<serde_json::Value> = checks
        .iter()
        .map(|c| {
            let mut v = poll::check_to_json(c);
            v["result_json"] = serde_json::json!(
                state_dir.join("runs").join(&c.run_id).join("result.json").display().to_string()
            );
            v["stdout_log"] = serde_json::json!(
                state_dir.join("runs").join(&c.run_id).join("stdout.log").display().to_string()
            );
            v["stderr_log"] = serde_json::json!(
                state_dir.join("runs").join(&c.run_id).join("stderr.log").display().to_string()
            );
            v["elapsed_secs"] = serde_json::json!(elapsed.as_secs_f64());
            v
        })
        .collect();
    if arr.len() == 1 {
        println!("{}", serde_json::to_string(&arr[0]).unwrap_or_default());
    } else {
        println!("{}", serde_json::to_string(&arr).unwrap_or_default());
    }
}

/// Spawn the detached background child for `run --background`.
///
/// Strategy: re-exec the current binary with the *original* argv (so
/// `--cargo-args`/`--` pass-through semantics are preserved verbatim),
/// inserting `--background-child` + `--run-id` immediately after `run` —
/// before any variadic `--cargo-args` could swallow them — and stripping the
/// parent's `--background`/`--run-id` flags. The child's stdout/stderr go to
/// `runs/<id>/broker.log`; its stdin is null; on unix it `setsid`s so it
/// survives the parent/terminal exiting.
fn spawn_background(cfg: &cargo_fuzz_next::config::Config, run_id: &str) -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let argv: Vec<OsString> = std::env::args_os().collect();
    let run_idx = argv.iter().position(|a| a == "run");
    let Some(ri) = run_idx else {
        return Err("could not locate `run` in argv for background spawn".to_string());
    };
    // Filter original args from `run` onward, dropping parent-only flags.
    let mut rest: Vec<OsString> = Vec::new();
    let mut i = ri + 1;
    while i < argv.len() {
        let a = &argv[i];
        let s = a.to_string_lossy();
        if s == "--background" || s == "--detach" || s == "--background-child" {
            i += 1;
            continue;
        }
        if s == "--run-id" {
            i += 2; // flag + its value
            continue;
        }
        if s.starts_with("--run-id=") || s.starts_with("--background=") || s.starts_with("--detach=") {
            i += 1;
            continue;
        }
        rest.push(a.clone());
        i += 1;
    }
    // Build: [run, --background-child, --run-id, <id>, <rest…>]
    let mut child_args: Vec<OsString> = vec![
        "run".into(),
        "--background-child".into(),
        "--run-id".into(),
        run_id.into(),
    ];
    child_args.extend(rest);

    let state_dir = cfg.effective_state_dir();
    let runs_parent = state_dir.join("runs");
    std::fs::create_dir_all(&runs_parent).map_err(|e| format!("create runs dir: {e}"))?;
    let run_dir = runs_parent.join(run_id);
    // Exclusive reservation: two `--background --run-id X` launches racing
    // before either child has enqueued would otherwise both succeed. The
    // first `create_dir` wins; the loser is a duplicate (exit 2).
    match std::fs::create_dir(&run_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(format!("duplicate --run-id {run_id}: run directory already exists"));
        }
        Err(e) => return Err(format!("create run dir: {e}")),
    }
    let log_path = run_dir.join("broker.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open broker log: {e}"))?;
    let log_err = log_file.try_clone().map_err(|e| format!("clone broker log: {e}"))?;

    let mut cmd = ProcCommand::new(&exe);
    cmd.args(&child_args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(log_file));
    cmd.stderr(Stdio::from(log_err));
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| format!("spawn background child: {e}"))?;
    let _ = std::fs::write(run_dir.join("pid"), child.id().to_string());
    // Also note the child pid in broker.log for debugging.
    let _ = std::fs::write(
        run_dir.join("broker.pid"),
        child.id().to_string(),
    );
    Ok(log_path)
}

fn print_status(broker: &Broker, budget: usize, budget_mem: usize) -> std::process::ExitCode {
    match (broker.list_active(), broker.list_queue()) {
        (Ok(active), Ok(queue)) => {
            let used: usize = active.iter().map(|r| r.cores).sum();
            let used_mem: usize = active.iter().map(|r| r.mem_mb).sum();
            println!(
                "budget: {used}/{budget} cores + {used_mem}/{budget_mem} MiB in use ({} active, {} queued)",
                active.len(),
                queue.len()
            );
            for r in &active {
                let remaining = (r.estimated_finish - chrono::Utc::now())
                    .num_seconds()
                    .max(0);
                println!(
                    "  ACTIVE {} {} {} cores / {}MiB {} remaining (est finish {})",
                    r.id,
                    r.test,
                    r.cores,
                    r.mem_mb,
                    format_eta(remaining as f64),
                    r.estimated_finish.to_rfc3339()
                );
            }
            for (i, q) in queue.iter().enumerate() {
                println!(
                    "  QUEUED #{} {} {} cores / {}MiB enqueued {}",
                    i,
                    q.id,
                    q.cores,
                    q.mem_mb,
                    q.enqueued_at.to_rfc3339()
                );
            }
            0.into()
        }
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("status: {e}");
            1.into()
        }
    }
}
