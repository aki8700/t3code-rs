//! Structured evidence feedback and loop-exit policy for post-turn `verify_command` runs.
//!
//! Loop exits follow two rules:
//! 1. Never exceed the configured auto-fix attempt cap.
//! 2. Escalate immediately when the same failure outcome repeats ("same result twice"), even
//!    when intervening file edits changed the Git tree hash.

use t3_core::normalize_failure_output;
use t3_store::VerifyDeadEnd;

const MAX_DEAD_ENDS: usize = 10;
const SUMMARY_MAX_CHARS: usize = 200;

/// Computes a deterministic signature of the failure outcome itself (excluding `post_tree`)
/// so editing files and hitting the exact same error still triggers a same-result-twice exit.
///
/// Parallel test runners report in completion order, so the signature ignores line order, and
/// it skips passing-test lines because the tail cut keeps a different subset of them each run.
pub(crate) fn outcome_signature(
    exit_code: Option<i32>,
    timed_out: bool,
    redacted_tail: &str,
) -> String {
    let normalized = normalize_failure_output(redacted_tail);
    let mut lines: Vec<&str> = normalized
        .lines()
        .filter(|line| !line.ends_with(" ... ok"))
        .collect();
    lines.sort_unstable();
    let input = format!("{exit_code:?}|{timed_out}|{}", lines.join("\n"));
    blake3::hash(input.as_bytes()).to_hex().to_string()
}

/// Returns `true` when `signature` has already been observed in the current verify loop.
pub(crate) fn is_repeated_outcome(
    signature: &str,
    last_signature: Option<&str>,
    dead_ends: &[VerifyDeadEnd],
) -> bool {
    last_signature == Some(signature) || dead_ends.iter().any(|entry| entry.signature == signature)
}

/// Extracts a concise single-line `SAW` summary from the failure output.
pub(crate) fn summarize_failure(
    command: &str,
    exit_code: Option<i32>,
    timed_out: bool,
    redacted_tail: &str,
) -> String {
    let prefix = if timed_out {
        "timed out".to_owned()
    } else {
        match exit_code {
            Some(code) => format!("exit {code}"),
            None => "terminated by signal".to_owned(),
        }
    };
    let headline = pick_headline(redacted_tail)
        .unwrap_or_else(|| format!("command `{command}` failed without output"));
    let full = format!("{prefix}: {headline}");
    truncate_chars(&full, SUMMARY_MAX_CHARS)
}

fn pick_headline(text: &str) -> Option<String> {
    let non_empty: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if non_empty.is_empty() {
        return None;
    }
    let priority = non_empty.iter().find(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("error[")
            || lower.starts_with("error:")
            || lower.contains("panicked at")
            || lower.contains("assertion")
            || lower.contains("failed")
            || lower.starts_with("fail ")
            || lower.contains("exception:")
    });
    let chosen = priority
        .copied()
        .unwrap_or_else(|| *non_empty.last().unwrap());
    Some(chosen.to_owned())
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Appends a dead end to the history, keeping at most `MAX_DEAD_ENDS` recent entries.
pub(crate) fn record_dead_end(
    mut dead_ends: Vec<VerifyDeadEnd>,
    attempt: u8,
    signature: String,
    exit_code: Option<i32>,
    timed_out: bool,
    summary: String,
) -> Vec<VerifyDeadEnd> {
    dead_ends.push(VerifyDeadEnd {
        attempt,
        signature,
        exit_code,
        timed_out,
        summary,
    });
    if dead_ends.len() > MAX_DEAD_ENDS {
        let drop_count = dead_ends.len() - MAX_DEAD_ENDS;
        dead_ends.drain(0..drop_count);
    }
    dead_ends
}

/// Builds a structured `EXPECTED vs SAW` + `DEAD ENDS` repair prompt for the next loop attempt.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_fix_prompt(
    command: &str,
    exit_code: Option<i32>,
    timed_out: bool,
    summary: &str,
    redacted_tail: &str,
    next_attempt: u8,
    max_attempts: u8,
    dead_ends: &[VerifyDeadEnd],
) -> String {
    let mut prompt = format!(
        "The project verify command (`{command}`) failed. Fix the root cause so the verify command exits 0 cleanly.\n\n\
ATTEMPT   {next_attempt}/{max_attempts}\n\
FAILED    {command}\n\
EXPECTED  exit_code=0, timed_out=false\n\
SAW       exit_code={exit_code:?}, timed_out={timed_out} — {summary}\n"
    );
    if !dead_ends.is_empty() {
        prompt
            .push_str("\nDEAD ENDS (previous failed outcomes — try a genuinely different fix):\n");
        for entry in dead_ends {
            prompt.push_str(&format!("- attempt {}: {}\n", entry.attempt, entry.summary));
        }
    }
    prompt.push_str(&format!(
        "\nDiagnostic output (redacted tail):\n```\n{redacted_tail}\n```"
    ));
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_signature_ignores_volatile_durations_timestamps_and_hex() {
        let a = "2026-11-26T14:01:02Z test foo ... FAILED in 0.42s at 0x7ffee1234abc\n";
        let b = "2026-11-26T14:05:59Z test foo ... FAILED in 1.87s at 0x7ffee9999def\n";
        assert_eq!(
            outcome_signature(Some(101), false, a),
            outcome_signature(Some(101), false, b)
        );
        assert_ne!(
            outcome_signature(Some(101), false, a),
            outcome_signature(Some(1), false, a)
        );
    }

    /// libtest output as printed by rustc 1.100: the panicking thread's id changes on every
    /// run, and parallel tests finish in any order.
    #[test]
    fn outcome_signature_ignores_rust_thread_ids_and_test_completion_order() {
        let a = "running 3 tests\n\
                 test one ... ok\n\
                 test boom ... FAILED\n\
                 test two ... ok\n\
                 test bang ... FAILED\n\
                 thread 'boom' (28490) panicked at src/lib.rs:1:18:\nboom\n";
        let b = "running 3 tests\n\
                 test bang ... FAILED\n\
                 test two ... ok\n\
                 test boom ... FAILED\n\
                 thread 'boom' (28497) panicked at src/lib.rs:1:18:\nboom\n";
        assert_eq!(
            outcome_signature(Some(101), false, a),
            outcome_signature(Some(101), false, b)
        );
        let moved = b.replace("src/lib.rs:1:18", "src/lib.rs:9:18");
        assert_ne!(
            outcome_signature(Some(101), false, a),
            outcome_signature(Some(101), false, &moved)
        );
        let other_failure = b.replace("test bang ... FAILED", "test other ... FAILED");
        assert_ne!(
            outcome_signature(Some(101), false, a),
            outcome_signature(Some(101), false, &other_failure)
        );
    }

    #[test]
    fn is_repeated_outcome_detects_any_prior_dead_end() {
        let sig1 = outcome_signature(Some(1), false, "error: missing field foo");
        let sig2 = outcome_signature(Some(1), false, "error: missing field bar");
        let dead_ends = record_dead_end(
            Vec::new(),
            1,
            sig1.clone(),
            Some(1),
            false,
            "exit 1: error: missing field foo".into(),
        );
        assert!(is_repeated_outcome(&sig1, Some(&sig2), &dead_ends));
        assert!(!is_repeated_outcome(
            &outcome_signature(Some(1), false, "error: baz"),
            Some(&sig2),
            &dead_ends
        ));
    }

    #[test]
    fn build_fix_prompt_formats_expected_saw_and_dead_ends() {
        let dead_ends = vec![VerifyDeadEnd {
            attempt: 1,
            signature: "abc".into(),
            exit_code: Some(1),
            timed_out: false,
            summary: "exit 1: assertion failed".into(),
        }];
        let prompt = build_fix_prompt(
            "cargo test",
            Some(101),
            false,
            "exit 101: error[E0308]: mismatched types",
            "error[E0308]: mismatched types",
            2,
            3,
            &dead_ends,
        );
        assert!(prompt.contains("verify command"));
        assert!(prompt.contains("ATTEMPT   2/3"));
        assert!(prompt.contains("EXPECTED  exit_code=0, timed_out=false"));
        assert!(prompt.contains("SAW       exit_code=Some(101), timed_out=false"));
        assert!(prompt.contains("DEAD ENDS"));
        assert!(prompt.contains("- attempt 1: exit 1: assertion failed"));
    }

    #[test]
    fn summarize_failure_prefers_error_headline() {
        let output = "compiling app...\nerror[E0425]: cannot find value `x`\nfor more info run rustc --explain";
        let summary = summarize_failure("cargo check", Some(101), false, output);
        assert_eq!(summary, "exit 101: error[E0425]: cannot find value `x`");
    }
}
