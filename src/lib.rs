//! cargo-fuzz-next: nextest slice for joint-fuzz style workspaces.
//!
//! Design notes — see README.md and docs/design.md for the full spec.

pub mod budget;
pub mod cli;
pub mod config;
pub mod executor;
pub mod poll;
pub mod reporter;
pub mod runner;
pub mod state_dir;
pub mod timings;
pub mod util;
