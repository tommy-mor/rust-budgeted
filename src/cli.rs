//! CLI: cargo-fuzz-next (invoked as `cargo fuzz-next ...`).
//!
//! Subcommands:
//! - `run`      budgeted, gated, timed test run (the point of the tool)
//! - `estimate` print p50/p95 without running (always allowed, no justification needed)
//! - `status`   show budget usage / active / queue
//! - `runs`     list recent runs from timings.jsonl
//! - `cancel`   cancel an active run by id (for --replace semantics)
//! - `clean`    prune stale active/queue entries
//! - `doctor`   check config, budget, histories

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueHint};

use crate::config::Config;
use crate::runner::{opt_duration, RunRequest};

#[derive(Parser, Debug)]
#[command(name = "cargo-fuzz-next", version, about = "Nextest-slice budgeted fuzz runner", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run a test binary under the global core budget + timing gate.
    Run(RunArgs),
    /// Print the timing estimate for a test without running it.
    Estimate(EstimateArgs),
    /// Show budget usage now.
    Status(StatusArgs),
    /// List recent runs from timings.jsonl.
    Runs(RunsArgs),
    /// Cancel an active run.
    Cancel(CancelArgs),
    /// Prune stale budget entries.
    Clean,
    /// Sanity-check config + store + budget.
    Doctor,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Test binary / test name (e.g. native_fuzz, post_match).
    #[arg(long, value_name = "TEST")]
    pub test: String,
    /// Cores to request from the global budget (also caps child thread pools via env).
    #[arg(long, default_value_t = 8)]
    pub cores: usize,
    /// Use --release for cargo test.
    #[arg(long)]
    pub release: bool,
    /// Case count knob (JOINT_FUZZ_CASES etc.) — also used to scale the estimate.
    #[arg(long, value_name = "N")]
    pub cases: Option<u64>,
    /// 200-word justification: why this run deserves the core-seconds NOW.
    /// Combined with --hypothesis must be >=200 words. Stored in history.
    #[arg(long, value_name = "TEXT")]
    pub justification: Option<String>,
    /// Hypothesis the run tests (falsifiable expectation + decision it unlocks).
    #[arg(long, value_name = "TEXT")]
    pub hypothesis: Option<String>,
    /// Estimate wall seconds you saw from `cargo fuzz-next estimate` (proves you looked).
    #[arg(long, value_name = "SECS")]
    pub seen_estimate_secs: Option<f64>,
    /// Queue when over budget (default). --no-queue rejects instead.
    #[arg(long)]
    pub no_queue: bool,
    /// Cancel a victim active run to make room when over budget.
    #[arg(long)]
    pub replace: bool,
    /// Imply long-run approval (estimate > threshold) — require 200 words first.
    #[arg(long)]
    pub yes: bool,
    /// CI / manual emergency: bypass justification + seen-estimate + approval gates.
    #[arg(long)]
    pub allow_no_justification: bool,
    /// Slow timeout, e.g. "60s", "5m".
    #[arg(long, value_name = "DUR")]
    pub slow_timeout: Option<String>,
    /// Terminate after N slow-timeout periods (e.g. --terminate-after 4).
    #[arg(long)]
    pub terminate_after: Option<u32>,
    /// Global timeout for the whole run, e.g. "10m".
    #[arg(long, value_name = "DUR")]
    pub global_timeout: Option<String>,
    /// Retries on failure (flaky marking), e.g. 2.
    #[arg(long, default_value_t = 0)]
    pub retries: u32,
    /// Write JUnit XML here (default: runs/<id>/junit.xml).
    #[arg(long, value_name = "PATH", value_hint = ValueHint::FilePath)]
    pub junit: Option<PathBuf>,
    /// State dir override (timings.jsonl, active.json). Default: ~/.local/share/fuzz-next.
    #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub state_dir: Option<PathBuf>,
    /// Global budget cores (default: ncpus - 4 reserved).
    #[arg(long, value_name = "CORES")]
    pub budget_cores: Option<usize>,
    /// Extra args to pass to `cargo test` (before --).
    #[arg(long, value_name = "ARG", num_args = 0..)]
    pub cargo_args: Vec<String>,
    /// Extra args after `--` to the test binary.
    #[arg(last = true, value_name = "ARG")]
    pub test_args: Vec<String>,
    /// Verbose: print estimate + budget lines, SLOW markers, tails.
    #[arg(short, long)]
    pub verbose: bool,
}

#[derive(Args, Debug)]
pub struct EstimateArgs {
    #[arg(long, value_name = "TEST")]
    pub test: String,
    #[arg(long, default_value_t = 8)]
    pub cores: usize,
    #[arg(long, value_name = "N")]
    pub cases: Option<u64>,
    #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub state_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub state_dir: Option<PathBuf>,
    #[arg(long, value_name = "CORES")]
    pub budget_cores: Option<usize>,
}

#[derive(Args, Debug)]
pub struct RunsArgs {
    /// Show last N runs.
    #[arg(long, default_value_t = 20)]
    pub last: usize,
    #[arg(long)]
    pub test: Option<String>,
    #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub state_dir: Option<PathBuf>,
    /// Machine-readable JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct CancelArgs {
    #[arg(value_name = "RUN_ID")]
    pub run_id: String,
    #[arg(long, value_name = "PATH", value_hint = ValueHint::DirPath)]
    pub state_dir: Option<PathBuf>,
    #[arg(long, value_name = "CORES")]
    pub budget_cores: Option<usize>,
}

/// Parse CLI, honouring `cargo fuzz-next <args>` (cargo passes "fuzz-next" as argv[1]).
#[must_use]
pub fn parse() -> Cli {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.len() > 1 && args[1] == "fuzz-next" {
        args.remove(1);
    }
    Cli::parse_from(args)
}

/// Build a `Config` from CLI state-dir / budget overrides + file.
#[must_use]
pub fn config_for(state_dir: Option<PathBuf>, budget_cores: Option<usize>) -> Config {
    let mut cfg = Config::load(None);
    if let Some(sd) = state_dir {
        cfg.state_dir = Some(sd);
    }
    if let Some(bc) = budget_cores {
        cfg.budget_cores = bc.max(1);
    }
    cfg
}

/// Convert parsed `Run` args to a `RunRequest`.
#[must_use]
pub fn run_request_from(args: RunArgs) -> RunRequest {
    RunRequest {
        test: args.test,
        cores: args.cores,
        release: args.release,
        cases: args.cases,
        justification: args.justification,
        hypothesis: args.hypothesis,
        seen_estimate_secs: args.seen_estimate_secs,
        queue_mode: !args.no_queue,
        replace: args.replace,
        yes: args.yes,
        slow_timeout: opt_duration(args.slow_timeout),
        terminate_after: args.terminate_after,
        global_timeout: opt_duration(args.global_timeout),
        retries: args.retries,
        extra_cargo_args: args.cargo_args,
        extra_test_args: args.test_args,
        junit: args.junit,
        allow_no_justification: args.allow_no_justification,
        verbose: args.verbose,
        worktree: None,
        sha: None,
    }
}

/// Parse a duration string arg via util.
pub fn dur(v: Option<String>) -> Option<Duration> {
    opt_duration(v)
}
