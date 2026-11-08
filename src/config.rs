//! Configuration: file (fuzz-next.toml) + env + defaults.
//!
//! Format is a tiny subset of nextest.toml so joint-fuzz users feel at home:
//!
//! ```toml
//! [profile.default]
//! budget-cores = 12
//! reserved-cores = 4
//! approval-threshold = "5m"
//! slow-timeout = "60s"
//!
//! [[profile.default.overrides]]
//! filter = 'test(native_fuzz)'
//! threads-required = 8
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util::{num_cpus, parse_duration};

#[derive(Debug, Clone)]
pub struct Config {
    /// Total cores the broker may hand out at once (across all worktrees/agents).
    pub budget_cores: usize,
    /// Cores to keep for the desktop (used to derive default budget = ncpus - reserved).
    pub reserved_cores: usize,
    /// p95 estimate above this (secs) requires explicit --yes / approval.
    pub approval_threshold: Duration,
    /// When a test is marked SLOW (log only).
    pub slow_timeout: Duration,
    /// Where timings.jsonl lives (override for tests / per-repo DBs).
    pub state_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let ncpus = num_cpus();
        let reserved = 4usize.min(ncpus.saturating_sub(1).max(1).min(4));
        let budget = ncpus.saturating_sub(reserved).max(1);
        Self {
            budget_cores: budget,
            reserved_cores: reserved,
            approval_threshold: Duration::from_secs(300),
            slow_timeout: Duration::from_secs(60),
            state_dir: None,
        }
    }
}

impl Config {
    /// Load from `fuzz-next.toml` in cwd (or `path` if given), falling back to defaults.
    /// Missing / malformed files yield defaults — never fail to start because of config.
    #[must_use]
    pub fn load(path: Option<&Path>) -> Self {
        let mut cfg = Self::default();
        let file = path.map(|p| p.to_path_buf()).or_else(|| {
            let cwd = std::env::current_dir().ok();
            cwd.and_then(|c| {
                let p = c.join("fuzz-next.toml");
                p.exists().then_some(p)
            })
        });
        let Some(file) = file else {
            return cfg;
        };
        let Ok(text) = std::fs::read_to_string(&file) else {
            return cfg;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() || line.starts_with('[') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let k = k.trim();
            let v = v.trim().trim_matches('"').trim_matches('\'');
            match k {
                "budget-cores" | "budget_cores" => {
                    if let Ok(n) = v.parse::<usize>() {
                        cfg.budget_cores = n.max(1);
                    }
                }
                "reserved-cores" | "reserved_cores" => {
                    if let Ok(n) = v.parse::<usize>() {
                        cfg.reserved_cores = n;
                        cfg.budget_cores = num_cpus().saturating_sub(n).max(1);
                    }
                }
                "approval-threshold" | "approval_threshold" => {
                    if let Some(d) = parse_duration(v) {
                        cfg.approval_threshold = d;
                    }
                }
                "slow-timeout" | "slow_timeout" => {
                    if let Some(d) = parse_duration(v) {
                        cfg.slow_timeout = d;
                    }
                }
                _ => {}
            }
        }
        // Env wins over file.
        if let Ok(s) = std::env::var("FUZZ_NEXT_BUDGET_CORES") {
            if let Ok(n) = s.parse::<usize>() {
                cfg.budget_cores = n.max(1);
            }
        }
        if let Ok(s) = std::env::var("FUZZ_NEXT_APPROVAL_THRESHOLD") {
            if let Some(d) = parse_duration(&s) {
                cfg.approval_threshold = d;
            }
        }
        cfg
    }

    /// Effective state dir for this config (file-scoped DBs per repo via --state-dir).
    #[must_use]
    pub fn effective_state_dir(&self) -> PathBuf {
        if let Some(d) = &self.state_dir {
            return d.clone();
        }
        crate::state_dir::state_dir()
    }
}

/// Per-test overrides from `[profile.default.overrides]` (subset).
#[derive(Debug, Clone, Default)]
pub struct TestOverride {
    pub filter_substr: String,
    pub threads_required: Option<usize>,
    pub retries: Option<u32>,
}

#[cfg(test)]
mod tests_config {
    use super::*;
    #[test]
    fn defaults_leave_reserved_cores() {
        let cfg = Config::default();
        let n = num_cpus();
        assert_eq!(cfg.budget_cores + cfg.reserved_cores, n);
        assert!(cfg.budget_cores >= 1);
    }

    #[test]
    fn load_parses_budget_and_thresholds() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("fuzz-next.toml");
        std::fs::write(
            &f,
            "budget-cores = 6\napproval-threshold = \"10m\"\nslow-timeout = \"30s\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(f.as_path()));
        assert_eq!(cfg.budget_cores, 6);
        assert_eq!(cfg.approval_threshold, Duration::from_secs(600));
        assert_eq!(cfg.slow_timeout, Duration::from_secs(30));
    }

    #[test]
    fn load_missing_file_is_defaults() {
        let cfg = Config::load(Some(Path::new("/nonexistent/fuzz-next.toml")));
        assert!(cfg.budget_cores >= 1);
    }
}
