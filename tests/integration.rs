//! Integration tests: gates + budget + timings end-to-end with fakes (no cargo build).

use cargo_fuzz_next::timings::{
    count_words, validate_justification, validate_seen_estimate, validate_seen_estimate_mem,
    TimingStore,
};

fn long_text(words: usize) -> String {
    std::iter::repeat("reason")
        .take(words)
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn justification_gate_requires_50_words() {
    assert!(validate_justification(Some(&long_text(30)), Some(&long_text(25))).is_ok());
    assert!(validate_justification(Some(&long_text(20)), Some(&long_text(15))).is_err());
    assert!(validate_justification(None, Some(&long_text(60))).is_err());
    assert!(validate_justification(Some(&long_text(60)), None).is_err());
}

#[test]
fn seen_estimate_gate_proves_agent_looked() {
    assert!(validate_seen_estimate(Some(350.0), 368.0).is_ok());
    assert!(validate_seen_estimate(Some(700.0), 368.0).is_ok()); // generous high side (2x)
    assert!(validate_seen_estimate(Some(50.0), 368.0).is_err()); // way too low
    assert!(validate_seen_estimate(None, 368.0).is_err());
}

#[test]
fn seen_estimate_mem_gate_proves_memory_looked() {
    assert!(validate_seen_estimate_mem(Some(1700.0), 1800.0).is_ok());
    assert!(validate_seen_estimate_mem(Some(3000.0), 1800.0).is_ok());
    assert!(validate_seen_estimate_mem(Some(300.0), 1800.0).is_err());
    assert!(validate_seen_estimate_mem(None, 1800.0).is_err());
}

#[test]
fn timings_store_full_cycle_with_justification_fields() {
    use cargo_fuzz_next::timings::TimingRecord;
    use chrono::Utc;
    let dir = tempfile::tempdir().unwrap();
    let store = TimingStore::new(dir.path().join("timings.jsonl"));
    let rec = TimingRecord {
        ts: Utc::now(),
        test: "native_fuzz".into(),
        binary: Some("native_fuzz".into()),
        secs: 307.0,
        cores: 8,
        rc: 0,
        status: "pass".into(),
        sha: Some("abc123".into()),
        cases: Some(1000),
        worktree: Some("agent-a45".into()),
        run_id: "r1".into(),
        justification: Some(long_text(150)),
        hypothesis: Some(long_text(60)),
        seen_estimate_secs: Some(368.0),
        mem_mb_requested: Some(1800),
        peak_rss_mb: Some(1500.0),
        seen_estimate_mem_mb: Some(1800.0),
    };
    store.append(&rec).unwrap();
    let all = store.load_all();
    assert_eq!(all.len(), 1);
    assert_eq!(count_words(all[0].justification.as_deref().unwrap()), 150);
    let est = store.estimate("native_fuzz", 8, Some(1000));
    assert!(!est.from_defaults);
    assert!((est.p95.unwrap() - 307.0).abs() < 1.0);
    // seen-estimate now validates against history.
    assert!(validate_seen_estimate(Some(310.0), est.effective_secs).is_ok());
}

#[test]
fn budget_broker_cross_worktree_semantics() {
    use cargo_fuzz_next::budget::{Admission, Broker};
    let dir = tempfile::tempdir().unwrap();
    let b = Broker::new(dir.path().to_path_buf(), 12);
    // Throwaway `sleep` pids so the entries survive the broker's reap
    // (unix pid liveness) without SIGTERMing this process on --replace.
    let pid_for = |child: &std::process::Child| child.id() as i32;
    let mut child_a = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let mut child_b = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let pid_a = pid_for(&child_a);
    let pid_b = pid_for(&child_b);
    // Worktree A takes 8.
    assert!(matches!(
        b.try_admit(
            "a1",
            pid_a,
            "native_fuzz",
            8,
            500,
            60.0,
            Some("wt-a".into()),
            200,
            true
        )
        .unwrap(),
        Admission::Admitted { .. }
    ));
    // Worktree B wants 8 -> queued (8+8 > 12).
    assert!(matches!(
        b.try_admit(
            "b1",
            pid_b,
            "post_match",
            8,
            500,
            60.0,
            Some("wt-b".into()),
            200,
            true
        )
        .unwrap(),
        Admission::Queued { .. }
    ));
    // --replace victim picks oldest (a1).
    let v = b.replace_victim(8, 500).unwrap();
    assert_eq!(v.as_deref(), Some("a1"));
    // After replace, B can be admitted on retry.
    assert!(matches!(
        b.try_admit(
            "b1",
            pid_b,
            "post_match",
            8,
            500,
            60.0,
            Some("wt-b".into()),
            200,
            true
        )
        .unwrap(),
        Admission::Admitted { .. }
    ));
    let _ = child_a.kill();
    let _ = child_b.kill();
    let _ = child_a.wait();
    let _ = child_b.wait();
}

#[test]
fn cli_wait_and_poll_alias_parse() {
    use clap::Parser;
    use cargo_fuzz_next::cli::{Cli, Command};
    // `cargo fuzz-next wait <id>` and `poll <id>` are the same subcommand.
    for name in ["wait", "poll"] {
        let cli = Cli::try_parse_from(["cargo-fuzz-next", name, "run-123", "--timeout", "15m", "--json"]).unwrap();
        match cli.command {
            Command::Wait(args) => {
                assert_eq!(args.run_id, vec!["run-123".to_string()]);
                assert_eq!(args.timeout.as_deref(), Some("15m"));
                assert!(args.json);
            }
            other => panic!("expected Wait for {name}, got {other:?}"),
        }
    }
    // Wait by test name.
    let cli = Cli::try_parse_from(["cargo-fuzz-next", "wait", "--test", "native_fuzz", "--interval", "500ms"]).unwrap();
    match cli.command {
        Command::Wait(args) => {
            assert!(args.run_id.is_empty());
            assert_eq!(args.test.as_deref(), Some("native_fuzz"));
            assert_eq!(args.interval, "500ms");
        }
        other => panic!("expected Wait for --test, got {other:?}"),
    }
}

#[test]
fn cli_run_background_flags_parse() {
    use clap::Parser;
    use cargo_fuzz_next::cli::{Cli, Command};

    for flag in ["--background", "--detach"] {
        let cli = Cli::try_parse_from([
            "cargo-fuzz-next", "run", "--test", "rooms", flag, "--run-id", "agent-1",
        ])
        .unwrap();
        match cli.command {
            Command::Run(args) => {
                assert!(args.background, "expected background for {flag}");
                assert_eq!(args.run_id.as_deref(), Some("agent-1"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    // status --poll parses.
    let cli = Cli::try_parse_from(["cargo-fuzz-next", "status", "--poll", "--timeout", "10m"]).unwrap();
    match cli.command {
        Command::Status(args) => {
            assert!(args.poll);
            assert_eq!(args.timeout.as_deref(), Some("10m"));
        }
        other => panic!("expected Status, got {other:?}"),
    }
}

#[test]
fn poll_wait_aggregates_pass_and_fail() {
    use cargo_fuzz_next::budget::Broker;
    use cargo_fuzz_next::poll::{self, WaitOutcome};
    use cargo_fuzz_next::timings::{TimingRecord, TimingStore};
    use chrono::Utc;
    let dir = tempfile::tempdir().unwrap();
    let b = Broker::new(dir.path().to_path_buf(), 12);
    let store = TimingStore::new(dir.path().join("timings.jsonl"));
    let mk = |run_id: &str, rc: i32| TimingRecord {
        ts: Utc::now(),
        test: "rooms".into(),
        binary: None,
        secs: 1.0,
        cores: 2,
        rc,
        status: if rc == 0 { "pass" } else { "fail" }.into(),
        sha: None,
        cases: None,
        worktree: None,
        run_id: run_id.to_string(),
        justification: None,
        hypothesis: None,
        seen_estimate_secs: None,
        mem_mb_requested: Some(317),
        peak_rss_mb: None,
        seen_estimate_mem_mb: None,
    };
    store.append(&mk("ok1", 0)).unwrap();
    store.append(&mk("bad1", 1)).unwrap();
    // Single pass -> Passed (0).
    let r = poll::wait_for_runs(
        &b, &store, dir.path(), &["ok1".to_string()], None,
        std::time::Duration::from_millis(10),
        Some(std::time::Duration::from_secs(2)),
        |_, _| {},
    );
    assert_eq!(r.outcome, WaitOutcome::Passed);
    // Mixed pass+fail -> Failed.
    let r = poll::wait_for_runs(
        &b, &store, dir.path(), &["ok1".to_string(), "bad1".to_string()], None,
        std::time::Duration::from_millis(10),
        Some(std::time::Duration::from_secs(2)),
        |_, _| {},
    );
    assert_eq!(r.outcome, WaitOutcome::Failed);
}
