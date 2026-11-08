//! State directory resolution (XDG / per-OS), env overridable for tests.

use std::path::PathBuf;

/// Resolve the base state dir for fuzz-next data.
/// Order: $FUZZ_NEXT_STATE_DIR > XDG ProjectDirs > $HOME/.local/share (unix) /
/// %APPDATA% (windows). Always a directory the caller should create.
#[must_use]
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("FUZZ_NEXT_STATE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(proj) = directories::ProjectDirs::from("org", "cargo-fuzz", "fuzz-next") {
        return proj.data_dir().to_path_buf();
    }
    // Fallback: home-based default.
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    #[cfg(windows)]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| home.clone());
        return PathBuf::from(base).join("fuzz-next");
    }
    #[cfg(not(windows))]
    {
        return PathBuf::from(home).join(".local/share/fuzz-next");
    }
}

#[must_use]
pub fn timings_path() -> PathBuf {
    state_dir().join("timings.jsonl")
}

#[must_use]
pub fn active_path() -> PathBuf {
    state_dir().join("active.json")
}

#[must_use]
pub fn lock_path() -> PathBuf {
    state_dir().join("budget.lock")
}

#[must_use]
pub fn runs_dir() -> PathBuf {
    state_dir().join("runs")
}

#[must_use]
pub fn queue_path() -> PathBuf {
    state_dir().join("queue.json")
}

#[cfg(test)]
mod tests_state_dir {
    use super::*;
    #[test]
    fn env_override_wins() {
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("FUZZ_NEXT_STATE_DIR").ok();
        std::env::set_var("FUZZ_NEXT_STATE_DIR", dir.path());
        assert_eq!(state_dir(), dir.path());
        match prev {
            Some(v) => std::env::set_var("FUZZ_NEXT_STATE_DIR", v),
            None => std::env::remove_var("FUZZ_NEXT_STATE_DIR"),
        }
    }
}
