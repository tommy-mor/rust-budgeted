//! Timing store: append-only JSONL + estimator (p50/p95) with joint-fuzz seed defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::util::percentile;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimingRecord {
    /// ISO-8601 creation time.
    pub ts: DateTime<Utc>,
    /// Harness test name (e.g. "native_fuzz" or binary name).
    pub test: String,
    /// Compiled test binary name, if distinct.
    pub binary: Option<String>,
    /// Wall seconds taken.
    pub secs: f64,
    /// Cores the run requested.
    pub cores: usize,
    /// Exit code (0 = pass). Retried-then-passed runs record rc=0 + flaky=true.
    pub rc: i32,
    /// pass | fail | flaky | timeout
    pub status: String,
    /// git short sha if known.
    pub sha: Option<String>,
    /// Case count knob if known (JOINT_FUZZ_CASES etc.).
    pub cases: Option<u64>,
    /// worktree / cwd basename for debugging.
    pub worktree: Option<String>,
    /// run-id dir name under runs/.
    pub run_id: String,
    /// 50-word justification: why this run deserves the cores/time.
    #[serde(default)]
    pub justification: Option<String>,
    /// Hypothesis the run tests (what we expect to learn).
    #[serde(default)]
    pub hypothesis: Option<String>,
    /// Estimate the agent saw (wall secs) before running — required to prove
    /// the agent actually looked up `estimate` instead of launching blind.
    #[serde(default)]
    pub seen_estimate_secs: Option<f64>,
    /// Memory requested from the broker (MiB). Defaults to estimate when None.
    #[serde(default)]
    pub mem_mb_requested: Option<usize>,
    /// Peak RSS measured for the process tree (MiB), if sampled.
    #[serde(default)]
    pub peak_rss_mb: Option<f64>,
    /// Memory estimate the agent saw (MiB) — required like seen_estimate_secs.
    #[serde(default)]
    pub seen_estimate_mem_mb: Option<f64>,
}

/// Minimum combined words for justification + hypothesis.
pub const MIN_JUSTIFICATION_WORDS: usize = 50;
/// How close `seen_estimate_secs` must be to our effective estimate (ratio).
pub const SEEN_ESTIMATE_TOLERANCE: f64 = 0.50;

/// Count words (whitespace-separated tokens).
#[must_use]
pub fn count_words(text: &str) -> usize {
    text.split_whitespace().filter(|w| !w.is_empty()).count()
}

/// Validate justification + hypothesis meet the run policy.
/// Returns Ok(word_count) or Err(human-readable problem).
pub fn validate_justification(
    justification: Option<&str>,
    hypothesis: Option<&str>,
) -> Result<usize, String> {
    let j = justification.unwrap_or("").trim();
    let h = hypothesis.unwrap_or("").trim();
    if j.is_empty() {
        return Err(
            "missing --justification: every fuzz run needs a 50-word justification (why it deserves the CPU time). See README for the template.".to_string(),
        );
    }
    if h.is_empty() {
        return Err(
            "missing --hypothesis: every fuzz run needs a hypothesis (what we expect to learn / falsify). See README for the template.".to_string(),
        );
    }
    let words = count_words(j) + count_words(h);
    if words < MIN_JUSTIFICATION_WORDS {
        return Err(format!(
            "justification+hypothesis is {words} words, need >={MIN_JUSTIFICATION_WORDS}. Write the full 50-word section (question, why now, why this test, why these cores, what we give up by waiting, what we learn).",
        ));
    }
    Ok(words)
}

/// Check the agent's seen estimate matches ours within tolerance.
/// `effective_secs` is our p95 (or default). Allows [-50%, +50%]; generous on the high side.
pub fn validate_seen_estimate(seen: Option<f64>, effective_secs: f64) -> Result<(), String> {
    let Some(seen) = seen else {
        return Err(format!(
            "missing --seen-estimate-secs: run `cargo fuzz-next estimate --test <name> --cores <N>` first and pass --seen-estimate-secs {effective_secs:.0} in `run` to prove you saw the estimate. Effective p95 is {effective_secs:.0}s.",
        ));
    };
    validate_seen_value(seen, effective_secs, "--seen-estimate-secs", "s")
}

/// Memory twin of the seen-estimate gate (MiB). Same tolerance logic.
pub fn validate_seen_estimate_mem(seen: Option<f64>, effective_mb: f64) -> Result<(), String> {
    let Some(seen) = seen else {
        return Err(format!(
            "missing --seen-estimate-mem-mb: run `cargo fuzz-next estimate --test <name> --cores <N>` first and pass --seen-estimate-mem-mb {effective_mb:.0} in `run` to prove you saw the memory estimate. Effective mem p95 is {effective_mb:.0} MiB.",
        ));
    };
    validate_seen_value(seen, effective_mb, "--seen-estimate-mem-mb", "MiB")
}

fn validate_seen_value(seen: f64, effective: f64, flag: &str, unit: &str) -> Result<(), String> {
    if seen <= 0.0 || !seen.is_finite() {
        return Err(format!("{flag} must be positive, got {seen}"));
    }
    let lo = effective * (1.0 - SEEN_ESTIMATE_TOLERANCE);
    let hi = effective * (1.0 + SEEN_ESTIMATE_TOLERANCE * 2.0);
    if seen < lo || seen > hi.max(effective * 2.0) {
        return Err(format!(
            "your {flag} {seen:.0}{unit} doesn't match the store's effective estimate {effective:.0}{unit} (acceptable {lo:.0}..{:.0}{unit}). Re-run `estimate` at your --cores/--cases/--mem-mb and use that number.",
            hi.max(effective * 2.0)
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Estimate {
    pub test: String,
    pub cores: usize,
    /// p50/p95/p99 seconds (None = no samples).
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
    pub n: usize,
    pub last_secs: Option<f64>,
    /// True if estimate comes from built-in defaults (no history yet).
    pub from_defaults: bool,
    /// Effective seconds to use for approval / ETA (p95 if present else default).
    pub effective_secs: f64,
    /// Human-readable notes.
    pub notes: String,
    /// Memory: p50/p95 MiB peak RSS (None = no samples).
    pub mem_p50: Option<f64>,
    pub mem_p95: Option<f64>,
    /// Effective MiB to use for budget admission (p95 scaled, or default).
    pub effective_mem_mb: f64,
    /// mem-seconds cost = effective_secs * effective_mem_mb.
    pub effective_mem_seconds: f64,
}

/// Built-in seed defaults from joint-fuzz `sweep_summary.txt` (release, 8 threads).
/// These make the runner useful before it has ever seen a run.
#[must_use]
pub fn default_secs_for_test(test: &str) -> Option<f64> {
    Some(match test {
        "rooms" | "rooms_native" | "rooms_replay" => 30.0,
        "end_screen" => 37.0,
        "enter_match" => 57.0,
        "match_routing" => 103.0,
        "ticker" => 121.0,
        "bot_hud" => 128.0,
        "native_fuzz" => 307.0,
        "spectate" | "native_spectate" => 313.0,
        "post_match" => 574.0,
        "replay_loop" | "result_side" => 180.0,
        "sweep" => 60.0,
        "parallel" | "multi" | "multi_native" | "multi_sweep" => 240.0,
        "real_server" | "fuzz" => 120.0,
        "conformance" | "faults" | "partial_log" | "native_engine" => 90.0,
        _ => return None,
    })
}

/// Peak RSS defaults (MiB) at 8 threads, from observed `ps` + tree sums.
/// These gate memory admission before history exists.
#[must_use]
pub fn default_mem_mb_for_test(test: &str) -> Option<f64> {
    Some(match test {
        "rooms" | "rooms_native" | "rooms_replay" => 400.0,
        "end_screen" => 500.0,
        "enter_match" => 600.0,
        "match_routing" => 800.0,
        "ticker" => 700.0,
        "bot_hud" => 700.0,
        "native_fuzz" => 1800.0,
        "spectate" | "native_spectate" => 900.0,
        "post_match" => 2500.0,
        "replay_loop" | "result_side" => 1000.0,
        "sweep" => 800.0,
        "parallel" | "multi" | "multi_native" | "multi_sweep" => 2000.0,
        "real_server" | "fuzz" => 1200.0,
        "conformance" | "faults" | "partial_log" | "native_engine" => 1000.0,
        _ => return None,
    })
}

/// Memory scaling is sub-linear too but weaker than time: more threads = more
/// stacks/arenas, roughly `^0.30`. 8→16 cores ≈ +23% RSS.
#[must_use]
pub fn mem_scale_factor(ref_cores: usize, cores: usize) -> f64 {
    if ref_cores == cores || cores == 0 {
        1.0
    } else {
        (cores as f64 / ref_cores as f64).powf(0.30)
    }
}

#[must_use]
pub fn scale_mem_mb(mb: f64, ref_cores: usize, cores: usize) -> f64 {
    mb * mem_scale_factor(ref_cores, cores)
}

pub struct TimingStore {
    path: PathBuf,
}

impl TimingStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub fn default_store() -> Self {
        Self {
            path: crate::state_dir::timings_path(),
        }
    }

    fn ensure_parent(path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    /// Append one record (atomic append; JSONL is safe under OS append for < PIPE_BUF).
    pub fn append(&self, rec: &TimingRecord) -> std::io::Result<()> {
        Self::ensure_parent(&self.path)?;
        let line = serde_json::to_string(rec).map_err(std::io::Error::other)?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    /// Load all records (best-effort: skip corrupt lines).
    #[allow(clippy::doc_markdown)]
    pub fn load_all(&self) -> Vec<TimingRecord> {
        Self::load_from(&self.path)
    }

    #[must_use]
    pub fn load_from(path: &Path) -> Vec<TimingRecord> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<TimingRecord>(l).ok())
            .collect()
    }

    /// Estimate wall seconds for `test` at `cores`.
    ///
    /// If we have samples, uses a cores-scaled p95 (sub-linear speedup
    /// exponent 0.85, matching joint-fuzz parallel throughput ~7.4x on 16 cores).
    /// Otherwise falls back to built-in defaults, or 60s.
    #[must_use]
    pub fn estimate(&self, test: &str, cores: usize, cases: Option<u64>) -> Estimate {
        let all = self.load_all();
        let mut eps: Vec<(f64, usize, Option<u64>)> = all
            .iter()
            .filter(|r| r.test == test || r.binary.as_deref() == Some(test))
            .map(|r| (r.secs, r.cores.max(1), r.cases))
            .collect();
        if eps.is_empty() {
            let def = default_secs_for_test(test).unwrap_or(60.0);
            // Scale default (measured at 8 threads) to requested cores.
            let scaled = scale_secs(def, 8, cores);
            // Cases scaling: if caller passes cases and we know def assumes default
            // case count, also scale linearly by cases/default ref only when explicit.
            let scaled = match cases {
                Some(c) if c > 0 => scaled * (c as f64 / default_cases_for_test(test) as f64),
                _ => scaled,
            };
            let mem_def = default_mem_mb_for_test(test).unwrap_or(1024.0);
            let mem_scaled = scale_mem_mb(mem_def, 8, cores);
            let mem_scaled = match cases {
                Some(c) if c > 0 => mem_scaled * (c as f64 / default_cases_for_test(test) as f64),
                _ => mem_scaled,
            };
            let eff_secs = (scaled * 1.20).max(scaled);
            let eff_mem = (mem_scaled * 1.20).max(mem_scaled);
            return Estimate {
                test: test.to_string(),
                cores,
                p50: Some(scaled),
                p95: Some((scaled * 1.20).max(scaled)),
                p99: Some((scaled * 1.50).max(scaled)),
                n: 0,
                last_secs: None,
                from_defaults: true,
                effective_secs: eff_secs,
                notes: format!("no history; built-in default scaled to {cores} cores"),
                mem_p50: Some(mem_scaled),
                mem_p95: Some((mem_scaled * 1.20).max(mem_scaled)),
                effective_mem_mb: eff_mem,
                effective_mem_seconds: eff_secs * eff_mem,
            };
        }
        // Prefer same-case-count samples when cases filters; otherwise all samples.
        let same_cases: Vec<(f64, usize, Option<u64>)> = if let Some(c) = cases {
            eps.iter()
                .filter(|(_, _, rcases)| *rcases == Some(c))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        if !same_cases.is_empty() {
            eps = same_cases;
        }
        let raw: Vec<f64> = eps.iter().map(|(s, _, _)| *s).collect();
        let p50 = percentile(&raw, 0.50);
        let p95 = percentile(&raw, 0.95);
        let p99 = percentile(&raw, 0.99);
        let last = eps.iter().map(|(s, _, _)| *s).next_back();
        // Scale by cores relative to median sample cores.
        let mut mids: Vec<usize> = eps.iter().map(|(_, c, _)| *c).collect();
        mids.sort_unstable();
        let ref_cores = mids[mids.len() / 2].max(1);
        let factor = if ref_cores != cores {
            scale_factor(ref_cores, cores)
        } else {
            1.0
        };
        let scale = |v: Option<f64>| v.map(|x| x * factor);
        let p50s = scale(p50);
        let p95s = scale(p95);
        let p99s = scale(p99);
        let effective = p95s.or(p50s).unwrap_or(60.0);
        // Memory branch: peak RSS samples scaled by mem factor.
        let mem_raw: Vec<f64> = all
            .iter()
            .filter(|r| r.test == test || r.binary.as_deref() == Some(test))
            .filter_map(|r| r.peak_rss_mb)
            .filter(|v| *v > 0.0)
            .map(|v| {
                // Scale each sample from its own cores to requested cores.
                let rc = 8usize.max(1); // fallback; true per-sample cores below
                let _ = rc;
                v
            })
            .collect();
        // Proper per-sample scaling needs each record's cores — collect with cores.
        let mem_samples: Vec<(f64, usize)> = all
            .iter()
            .filter(|r| r.test == test || r.binary.as_deref() == Some(test))
            .filter_map(|r| r.peak_rss_mb.map(|mb| (mb, r.cores.max(1))))
            .filter(|(mb, _)| *mb > 0.0)
            .collect();
        let (mem_p50, mem_p95, eff_mem) = if mem_samples.is_empty() {
            let def = default_mem_mb_for_test(test).unwrap_or(1024.0);
            let scaled = scale_mem_mb(def, 8, cores);
            (
                Some(scaled),
                Some((scaled * 1.20).max(scaled)),
                (scaled * 1.20).max(scaled),
            )
        } else {
            let mids: Vec<usize> = mem_samples.iter().map(|(_, c)| *c).collect();
            let mut mids_sorted = mids.clone();
            mids_sorted.sort_unstable();
            let ref_mem_cores = mids_sorted[mids_sorted.len() / 2].max(1);
            let mf = mem_scale_factor(ref_mem_cores, cores);
            let scaled: Vec<f64> = mem_samples.iter().map(|(mb, _)| mb * mf).collect();
            let p50 = percentile(&scaled, 0.50);
            let p95 = percentile(&scaled, 0.95);
            let eff = p95.or(p50).unwrap_or(1024.0);
            (p50, p95, eff)
        };
        let _ = mem_raw;
        Estimate {
            test: test.to_string(),
            cores,
            p50: p50s,
            p95: p95s,
            p99: p99s,
            n: raw.len(),
            last_secs: last.map(|x| x * factor),
            from_defaults: false,
            effective_secs: effective,
            notes: format!("n={} ref_cores={} factor={factor:.2}", raw.len(), ref_cores),
            mem_p50,
            mem_p95,
            effective_mem_mb: eff_mem,
            effective_mem_seconds: effective * eff_mem,
        }
    }

    /// All tests we know (history ∪ defaults), grouped for `runs list` / stats.
    pub fn known_tests(&self) -> Vec<String> {
        let mut set: BTreeMap<String, u32> = BTreeMap::new();
        for r in self.load_all() {
            *set.entry(r.test.clone()).or_default() += 1;
        }
        set.into_keys().collect()
    }

    /// Per-test stats rows: (test, n, p50, p95, last).
    #[must_use]
    pub fn stats(&self) -> Vec<(String, usize, Option<f64>, Option<f64>, Option<f64>)> {
        let all = self.load_all();
        let mut by_test: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for r in all {
            by_test.entry(r.test).or_default().push(r.secs);
        }
        by_test
            .into_iter()
            .map(|(t, v)| {
                let last = v.last().copied();
                let p50 = percentile(&v, 0.50);
                let p95 = percentile(&v, 0.95);
                (t, v.len(), p50, p95, last)
            })
            .collect()
    }
}

#[must_use]
pub fn default_cases_for_test(test: &str) -> u64 {
    // Mirrors joint-fuzz docs defaults; only used for explicit --cases scaling.
    match test {
        "multi" | "multi_native" => 240,
        "fuzz" | "parallel" => 1000,
        _ => 1000,
    }
}

/// Sub-linear scaling: T(cores) ~ T(ref) * (ref/cores)^0.85.
#[must_use]
pub fn scale_factor(ref_cores: usize, cores: usize) -> f64 {
    if ref_cores == cores || cores == 0 {
        1.0
    } else {
        (ref_cores as f64 / cores as f64).powf(0.85)
    }
}

#[must_use]
pub fn scale_secs(secs: f64, ref_cores: usize, cores: usize) -> f64 {
    secs * scale_factor(ref_cores, cores)
}

#[cfg(test)]
mod tests_timings {
    use super::*;
    use chrono::Utc;

    fn rec(test: &str, secs: f64, cores: usize) -> TimingRecord {
        TimingRecord {
            ts: Utc::now(),
            test: test.to_string(),
            binary: None,
            secs,
            cores,
            rc: 0,
            status: "pass".to_string(),
            sha: None,
            cases: None,
            worktree: None,
            run_id: "test-run".to_string(),
            justification: None,
            hypothesis: None,
            seen_estimate_secs: None,
            mem_mb_requested: None,
            peak_rss_mb: None,
            seen_estimate_mem_mb: None,
        }
    }

    #[test]
    fn estimate_uses_history_and_cores_scaling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timings.jsonl");
        let store = TimingStore::new(path.clone());
        for s in [90.0, 100.0, 110.0, 120.0, 130.0] {
            store.append(&rec("native_fuzz", s, 8)).unwrap();
        }
        let est = store.estimate("native_fuzz", 4, None);
        assert!(!est.from_defaults);
        assert_eq!(est.n, 5);
        // p50 ~110 at 8 cores; at 4 cores it must be larger (sub-linear).
        assert!(est.p50.unwrap() > 110.0);
        let est16 = store.estimate("native_fuzz", 16, None);
        assert!(est16.p50.unwrap() < 110.0);
    }

    #[test]
    fn estimate_defaults_for_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let store = TimingStore::new(dir.path().join("t.jsonl"));
        let est = store.estimate("post_match", 12, None);
        assert!(est.from_defaults);
        assert!(est.effective_secs > 300.0); // post_match is long — approval gate fires
    }

    #[test]
    fn stats_group_by_test() {
        let dir = tempfile::tempdir().unwrap();
        let store = TimingStore::new(dir.path().join("t.jsonl"));
        store.append(&rec("a", 10.0, 4)).unwrap();
        store.append(&rec("a", 20.0, 4)).unwrap();
        store.append(&rec("b", 5.0, 4)).unwrap();
        let st = store.stats();
        assert_eq!(st.len(), 2);
        let a = st.iter().find(|(t, _, _, _, _)| t == "a").unwrap();
        assert_eq!(a.1, 2);
    }

    #[test]
    fn jsonl_append_load_roundtrip_skips_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(&path, "not json\n").unwrap();
        let store = TimingStore::new(path.clone());
        store.append(&rec("x", 1.5, 2)).unwrap();
        let all = TimingStore::load_from(&path);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].test, "x");
        assert!((all[0].secs - 1.5).abs() < f64::EPSILON);
    }
}
