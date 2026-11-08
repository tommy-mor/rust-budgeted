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
fn justification_gate_requires_200_words() {
    assert!(validate_justification(Some(&long_text(120)), Some(&long_text(90))).is_ok());
    assert!(validate_justification(Some(&long_text(80)), Some(&long_text(60))).is_err());
    assert!(validate_justification(None, Some(&long_text(250))).is_err());
    assert!(validate_justification(Some(&long_text(250)), None).is_err());
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
    // Worktree A takes 8.
    assert!(matches!(
        b.try_admit(
            "a1",
            111,
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
            222,
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
            222,
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
}
