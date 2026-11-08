//! Reporters: human table, JSON summary, JUnit XML.

use std::path::Path;

use crate::timings::TimingRecord;
use crate::util::xml_escape;

#[derive(Debug, Clone)]
pub struct RunSummary {
    pub run_id: String,
    pub test: String,
    pub secs: f64,
    pub cores: usize,
    pub rc: i32,
    pub status: String,
    pub flaky: bool,
    pub attempts: u32,
    pub estimate_secs: f64,
    pub justification_words: usize,
    pub hypothesis_words: usize,
    pub stdout_path: String,
    pub stderr_path: String,
    pub core_seconds: f64,
}

impl RunSummary {
    #[must_use]
    pub fn from_record(rec: &TimingRecord, estimate_secs: f64, attempts: u32) -> Self {
        let j_words = rec
            .justification
            .as_ref()
            .map_or(0, |s| crate::timings::count_words(s));
        let h_words = rec
            .hypothesis
            .as_ref()
            .map_or(0, |s| crate::timings::count_words(s));
        Self {
            run_id: rec.run_id.clone(),
            test: rec.test.clone(),
            secs: rec.secs,
            cores: rec.cores,
            rc: rec.rc,
            status: rec.status.clone(),
            flaky: rec.status == "flaky",
            attempts,
            estimate_secs,
            justification_words: j_words,
            hypothesis_words: h_words,
            stdout_path: String::new(),
            stderr_path: String::new(),
            core_seconds: rec.secs * rec.cores as f64,
        }
    }
}

/// Write JUnit XML for a single run (one testsuite per binary/run).
pub fn write_junit(path: &Path, summaries: &[RunSummary]) -> std::io::Result<()> {
    let total_tests = summaries.len();
    let total_time: f64 = summaries.iter().map(|s| s.secs).sum();
    let failures = summaries.iter().filter(|s| s.rc != 0).count();
    let testsuite_time = total_time.max(0.0);
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<testsuites tests=\"{total_tests}\" failures=\"{failures}\" time=\"{testsuite_time:.3}\">\n"
    ));
    for s in summaries {
        let failed = s.rc != 0;
        xml.push_str("  <testsuite");
        xml.push_str(&format!(" name=\"{}\"", xml_escape(&s.test)));
        xml.push_str(&format!(
            " tests=\"1\" failures=\"{}\" time=\"{:.3}\"",
            if failed { 1 } else { 0 },
            s.secs
        ));
        xml.push_str(&format!(" timestamp=\"{}\"", iso_now()));
        xml.push_str(">\n");
        xml.push_str(&format!(
            "    <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\"",
            xml_escape(&s.test),
            xml_escape(&s.test),
            s.secs
        ));
        // Properties recording justification/hypothesis proof
        xml.push_str(">\n");
        xml.push_str(&format!(
            "      <property name=\"fuzz-next.run-id\" value=\"{}\"/>\n",
            xml_escape(&s.run_id)
        ));
        xml.push_str(&format!(
            "      <property name=\"fuzz-next.cores\" value=\"{}\"/>\n",
            s.cores
        ));
        xml.push_str(&format!(
            "      <property name=\"fuzz-next.core-seconds\" value=\"{:.0}\"/>\n",
            s.core_seconds
        ));
        xml.push_str(&format!(
            "      <property name=\"fuzz-next.estimate-secs\" value=\"{:.0}\"/>\n",
            s.estimate_secs
        ));
        xml.push_str(&format!(
            "      <property name=\"fuzz-next.justification-words\" value=\"{}\"/>\n",
            s.justification_words
        ));
        xml.push_str("\n");
        if failed {
            xml.push_str(&format!(
                "      <failure message=\"exit {}\" type=\"E2E\">see logs</failure>\n",
                s.rc
            ));
        }
        xml.push_str("    </testcase>\n  </testsuite>\n");
    }
    xml.push_str("</testsuites>\n");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, xml)?;
    Ok(())
}

/// Write result.json for a run (rich, for `runs` / rerun).
pub fn write_result_json(
    path: &Path,
    summary: &RunSummary,
    rec: &TimingRecord,
) -> std::io::Result<()> {
    let value = serde_json::json!({
        "run_id": summary.run_id,
        "test": summary.test,
        "secs": summary.secs,
        "cores": summary.cores,
        "core_seconds": summary.core_seconds,
        "rc": summary.rc,
        "status": summary.status,
        "flaky": summary.flaky,
        "attempts": summary.attempts,
        "estimate_secs": summary.estimate_secs,
        "justification_words": summary.justification_words,
        "hypothesis_words": summary.hypothesis_words,
        "justification": rec.justification,
        "hypothesis": rec.hypothesis,
        "seen_estimate_secs": rec.seen_estimate_secs,
        "stdout": summary.stdout_path,
        "stderr": summary.stderr_path,
        "ts": rec.ts,
        "sha": rec.sha,
        "cases": rec.cases,
        "worktree": rec.worktree,
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(&value).map_err(std::io::Error::other)?,
    )?;
    Ok(())
}

#[must_use]
pub fn format_human(summary: &RunSummary) -> String {
    let delta = summary.secs - summary.estimate_secs;
    let verdict = if summary.estimate_secs > 0.0 {
        format!(" (est {:.0}s, delta {delta:+.0}s)", summary.estimate_secs)
    } else {
        String::new()
    };
    let flaky = if summary.flaky { " FLAKY" } else { "" };
    format!(
        "{} {:<16} {:.1}s x{} cores {:.0} core-s {}{} J {} H {}",
        summary.status.to_uppercase(),
        summary.test,
        summary.secs,
        summary.cores,
        summary.core_seconds,
        verdict,
        flaky,
        summary.justification_words,
        summary.hypothesis_words,
    )
}

fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests_reporter {
    use super::*;
    #[test]
    fn junit_escapes_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("junit.xml");
        let s = RunSummary {
            run_id: "r1".into(),
            test: "a<b>&".into(),
            secs: 1.5,
            cores: 4,
            rc: 1,
            status: "fail".into(),
            flaky: false,
            attempts: 1,
            estimate_secs: 2.0,
            justification_words: 150,
            hypothesis_words: 60,
            stdout_path: String::new(),
            stderr_path: String::new(),
            core_seconds: 6.0,
        };
        write_junit(&p, &[s]).unwrap();
        let xml = std::fs::read_to_string(&p).unwrap();
        assert!(xml.contains("a&lt;b&gt;&amp;"));
        assert!(xml.contains("fuzz-next.justification-words"));
        assert!(xml.contains("failures=\"1\""));
    }
    #[test]
    fn human_format_includes_core_seconds() {
        let s = RunSummary {
            run_id: "r".into(),
            test: "native_fuzz".into(),
            secs: 120.0,
            cores: 8,
            rc: 0,
            status: "pass".into(),
            flaky: false,
            attempts: 1,
            estimate_secs: 110.0,
            justification_words: 200,
            hypothesis_words: 50,
            stdout_path: String::new(),
            stderr_path: String::new(),
            core_seconds: 960.0,
        };
        let h = format_human(&s);
        assert!(h.contains("960 core-s"));
        assert!(h.contains("native_fuzz"));
    }
}
