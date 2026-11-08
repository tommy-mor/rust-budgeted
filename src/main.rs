use cargo_fuzz_next::budget::{format_eta, Broker};
use cargo_fuzz_next::cli::{config_for, parse, run_request_from, Command};
use cargo_fuzz_next::timings::TimingStore;

fn main() -> std::process::ExitCode {
    let cli = parse();
    match cli.command {
        Command::Run(args) => {
            let state_dir = args.state_dir.clone();
            let budget = args.budget_cores;
            let budget_mem = args.budget_mem_mb;
            let verbose = args.verbose;
            let cfg = config_for(state_dir, budget, budget_mem);
            let req = run_request_from(args);
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
                    if res.summary.rc == 0 {
                        0.into()
                    } else {
                        1.into()
                    }
                }
                Err(e) => {
                    eprintln!("[fuzz-next] {e}");
                    // 2 = gate (justification/estimate/approval), 3 = budget reject.
                    if e.starts_with("NEEDS-APPROVAL")
                        || e.contains("200")
                        || e.contains("seen-estimate")
                        || e.contains("justification")
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
            let cfg = config_for(args.state_dir, args.budget_cores, args.budget_mem_mb);
            let broker = Broker::with_mem(
                cfg.effective_state_dir(),
                cfg.budget_cores,
                cfg.budget_mem_mb,
            );
            print_status(&broker, cfg.budget_cores, cfg.budget_mem_mb);
            0.into()
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
