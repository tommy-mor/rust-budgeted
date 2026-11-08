//! Cross-platform utilities: pid liveness, durations, backoff, cpu count.

use std::time::Duration;

/// Number of logical CPUs available, floored at 1.
#[must_use]
pub fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Total physical RAM in MiB (best-effort, cross-platform).
#[must_use]
pub fn total_physical_mem_mb() -> usize {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with("MemTotal:") {
                    // "MemTotal:  16306736 kB"
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        if let Ok(kb) = parts[1].parse::<usize>() {
                            return (kb / 1024).max(256);
                        }
                    }
                }
            }
        }
        // /proc/stat fallback: total RAM from /proc/meminfo else 8GB.
    }
    #[cfg(windows)]
    {
        // GlobalMemoryStatusEx would need windows crate; fallback to env is fine.
        // The broker only uses this for a sane default budget.
    }
    // Reasonable default for test / unknown hosts.
    8192
}

/// Parse a duration like "5m", "60s", "2h", "1h30m", "300" into seconds (f64).
/// Returns None on parse failure.
#[must_use]
pub fn parse_duration_secs(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Bare number = seconds.
    if let Ok(v) = s.parse::<f64>() {
        if v >= 0.0 {
            return Some(v);
        }
        return None;
    }
    let mut total = 0f64;
    let mut num = String::new();
    let mut chars = s.chars().peekable();
    let mut consumed_any = false;
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else if c.is_whitespace() {
            // separator between groups e.g. "1h 30m"
            continue;
        } else {
            if num.is_empty() {
                return None;
            }
            let val: f64 = num.parse().ok()?;
            num.clear();
            let mut unit = String::new();
            unit.push(c.to_ascii_lowercase());
            while let Some(&p) = chars.peek() {
                if p.is_alphabetic() {
                    // peek consumes one char; advance
                    unit.push(chars.next().unwrap().to_ascii_lowercase());
                } else {
                    break;
                }
            }
            let mult = match unit.as_str() {
                "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
                "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
                "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
                "d" | "day" | "days" => 86_400.0,
                "ms" | "millisecond" | "milliseconds" => 0.001,
                _ => return None,
            };
            total += val * mult;
            consumed_any = true;
        }
    }
    if !num.is_empty() {
        // trailing bare number after groups: treat as seconds (e.g. "1h 30" -> 3630)
        total += num.parse::<f64>().ok()?;
        consumed_any = true;
    }
    if consumed_any {
        Some(total)
    } else {
        None
    }
}

/// Parse to `Duration` (convenience over `parse_duration_secs`).
#[must_use]
pub fn parse_duration(s: &str) -> Option<Duration> {
    parse_duration_secs(s).map(Duration::from_secs_f64)
}

/// Percentile helper (linear interpolation) for estimate math.
#[must_use]
pub fn percentile(data: &[f64], p: f64) -> Option<f64> {
    if data.is_empty() {
        return None;
    }
    let mut sorted = data.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let rank = (sorted.len() as f64 - 1.0) * p.clamp(0.0, 1.0);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        Some(sorted[lo])
    } else {
        let frac = rank - lo as f64;
        Some(sorted[lo] * (1.0 - frac) + sorted[hi] * frac)
    }
}

/// XML-escape for JUnit output.
#[must_use]
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Check whether a pid is alive (cross-platform, no signalling side effects).
#[must_use]
pub fn pid_exists(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // Probe with signal 0 (kill(pid, 0)); ESRCH means dead.
        unsafe {
            match libc::kill(pid, 0) {
                0 => true,
                _ => {
                    let err = std::io::Error::last_os_error();
                    err.raw_os_error() != Some(libc::ESRCH)
                }
            }
        }
    }
    #[cfg(windows)]
    {
        // Pathless fallback: without extra deps we cannot probe without side effects.
        // The budget broker treats Windows liveness via heartbeat files (see budget.rs).
        // Returning true avoids premature reaping; stale entries expire by heartbeat.
        let _ = pid;
        true
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

/// Retry backoff policy (subset of nextest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backoff {
    Fixed,
    Exponential,
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub count: u32,
    pub backoff: Backoff,
    pub delay: Duration,
    pub max_delay: Duration,
    pub jitter: bool,
}

impl RetryPolicy {
    #[must_use]
    pub fn new(count: u32) -> Self {
        Self {
            count,
            backoff: Backoff::Exponential,
            delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            jitter: true,
        }
    }

    /// Delay before the next retry (attempt is 1-based retry index).
    #[must_use]
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base = match self.backoff {
            Backoff::Fixed => self.delay,
            Backoff::Exponential => {
                let exp = attempt.saturating_sub(1).min(10);
                self.delay.checked_mul(1u32 << exp).unwrap_or(self.delay)
            }
        };
        let mut d = base.min(self.max_delay);
        if self.jitter {
            // Cheap pseudo-jitter 0..25% above base derived from attempt + time.
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            attempt.hash(&mut h);
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .hash(&mut h);
            let jitter_pct = (h.finish() % 25) as f64 / 100.0;
            d += Duration::from_secs_f64(d.as_secs_f64() * jitter_pct);
            d = d.min(self.max_delay);
        }
        d
    }
}

#[cfg(test)]
mod tests_util {
    use super::*;
    #[test]
    fn percentiles_match_expectations() {
        assert_eq!(percentile(&[10.0], 0.95), Some(10.0));
        assert!(percentile(&[], 0.5).is_none());
        let v = percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 0.5);
        assert!(matches!(v, Some(3.0)));
        let p95 = percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 0.95).unwrap();
        assert!(p95 > 4.5 && p95 <= 5.0);
    }

    #[test]
    fn retry_backoff_grows_then_clamps() {
        let p = RetryPolicy::new(10);
        let d1 = p.delay_for_attempt(1);
        let d2 = p.delay_for_attempt(2);
        let d3 = p.delay_for_attempt(3);
        assert!(d2.as_secs_f64() >= d1.as_secs_f64());
        assert!(d3.as_secs_f64() >= d2.as_secs_f64());
        let d_big = p.delay_for_attempt(12);
        assert!(d_big <= p.max_delay);
    }

    #[test]
    fn parse_durations() {
        assert_eq!(parse_duration_secs("60s"), Some(60.0));
        assert_eq!(parse_duration_secs("5m"), Some(300.0));
        assert_eq!(parse_duration_secs("2h"), Some(7200.0));
        assert_eq!(parse_duration_secs("300"), Some(300.0));
        assert_eq!(parse_duration_secs("1h30m"), Some(5400.0));
        assert_eq!(parse_duration_secs("90s"), Some(90.0));
        assert_eq!(parse_duration_secs("bogus"), None);
    }

    #[test]
    fn xml_escape_escapes_all() {
        assert_eq!(xml_escape("<a&'\">"), "&lt;a&amp;&apos;&quot;&gt;");
    }
}
