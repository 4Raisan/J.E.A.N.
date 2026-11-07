//! Verification: distinguishing "the command ran" from "the goal was met".
//!
//! `IMPLEMENTATION_GUIDE.md` §6 is explicit that an exit code of 0 does not prove
//! the user-requested outcome, and `PLANNING/05-ROADMAP.md` makes a declared
//! verification step a Phase-0 deliverable. This module is that step.
//!
//! ## Three answers, not two
//!
//! A boolean would force the weakest case to be called a failure. Instead:
//!
//! * [`Verdict::Proven`] — the declared evidence was found.
//! * [`Verdict::NotProven`] — the declared evidence was looked for and is absent.
//! * [`Verdict::Unverifiable`] — no evidence was declared, or the evidence was
//!   destroyed before it could be checked.
//!
//! The third is what keeps the CLI honest. A tool whose registry entry says
//! `verify = { method = "none" }` completed its command, and J.E.A.N. says exactly
//! that instead of printing "success".
//!
//! ## Truncation matters
//!
//! If stdout was truncated and the needle is not in the retained prefix, the
//! absence proves nothing: the match could be in the discarded tail. That case is
//! reported as `Unverifiable` with an explanation, not as `NotProven`.

use std::path::Path;

use jean_process::ChildOutcome;
use jean_protocol::tool::VerifySpec;
use jean_protocol::Termination;

/// The result of checking a run against its declared verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The declared evidence was found.
    Proven(String),
    /// The declared evidence was looked for and is absent.
    NotProven(String),
    /// No usable evidence was declared, or it could not be checked.
    Unverifiable(String),
}

impl Verdict {
    pub fn is_proven(&self) -> bool {
        matches!(self, Verdict::Proven(_))
    }

    /// One word for the CLI and for the `tasks.outcome` column.
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Proven(_) => "proven",
            Verdict::NotProven(_) => "not_proven",
            Verdict::Unverifiable(_) => "unverifiable",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Verdict::Proven(d) | Verdict::NotProven(d) | Verdict::Unverifiable(d) => d,
        }
    }
}

/// How many bytes of the captured artifact are searched for an output predicate.
///
/// Bounded so verification cannot become the slowest part of a run, and so a
/// 10 MB capture is not slurped whole to look for a four-character needle.
pub const VERIFY_READ_BYTES: u64 = 1024 * 1024;

/// Check a finished run against the verification its tool declared.
pub fn verify(spec: &VerifySpec, outcome: &ChildOutcome) -> Verdict {
    match spec {
        VerifySpec::None => Verdict::Unverifiable(
            "the tool declares no verification; the command completed but nothing \
             proves the broader goal was met"
                .to_owned(),
        ),
        VerifySpec::ExitCode { expected } => check_exit_code(*expected, outcome),
        VerifySpec::OutputContains {
            expected,
            needle,
            case_insensitive,
        } => check_output(*expected, needle, *case_insensitive, outcome),
    }
}

fn check_exit_code(expected: i32, outcome: &ChildOutcome) -> Verdict {
    if let Some(reason) = forced_termination(outcome) {
        return Verdict::NotProven(reason);
    }
    match outcome.exit_code {
        Some(code) if code == expected => Verdict::Proven(format!(
            "the child exited with {code}, which is the expected code"
        )),
        Some(code) => Verdict::NotProven(format!(
            "the child exited with {code}; the tool declared {expected}"
        )),
        None => Verdict::Unverifiable(
            "no exit code could be read, so the declared exit-code check could not run".to_owned(),
        ),
    }
}

fn check_output(
    expected: i32,
    needle: &str,
    case_insensitive: bool,
    outcome: &ChildOutcome,
) -> Verdict {
    let exit = check_exit_code(expected, outcome);
    if !exit.is_proven() {
        // The exit-code half already failed or could not be checked; report that
        // rather than the predicate, because it is the more specific fact.
        return exit;
    }

    let Some(path) = outcome.stdout.path.as_deref() else {
        return Verdict::Unverifiable(
            "output capture was disabled, so the declared output predicate could not be checked"
                .to_owned(),
        );
    };

    let captured = match read_prefix(Path::new(path), VERIFY_READ_BYTES) {
        Ok(text) => text,
        Err(e) => {
            return Verdict::Unverifiable(format!(
                "the captured output at {path} could not be read, so the predicate could not be \
                 checked: {e}"
            ))
        }
    };

    let found = if case_insensitive {
        captured.to_lowercase().contains(&needle.to_lowercase())
    } else {
        captured.contains(needle)
    };

    if found {
        return Verdict::Proven(format!(
            "the child exited with {expected} and its output contains {needle:?}"
        ));
    }

    if outcome.stdout.truncated {
        // The needle may be in the discarded tail. Claiming "not found" here
        // would be a statement about output J.E.A.N. threw away.
        return Verdict::Unverifiable(format!(
            "{needle:?} was not found in the first {} bytes of stdout, but stdout was truncated \
             after {} bytes, so its absence is not conclusive",
            captured.len(),
            outcome.stdout.bytes
        ));
    }

    Verdict::NotProven(format!(
        "the child exited with {expected} but its output does not contain {needle:?}"
    ))
}

/// Why a forced termination makes any declared verification fail.
///
/// A killed child has the exit code J.E.A.N. chose for the kill, so comparing it
/// against a declared expectation is meaningless — and if that code happened to
/// match, the task would be reported as proven when it was actually stopped.
fn forced_termination(outcome: &ChildOutcome) -> Option<String> {
    if !outcome.termination.was_forced() {
        return None;
    }
    Some(match outcome.termination {
        Termination::Timeout => "the child was killed because its timeout elapsed, so it never \
                                 reached a conclusion"
            .to_owned(),
        Termination::Cancelled => {
            "the child was cancelled, so it never reached a conclusion".to_owned()
        }
        Termination::LaunchFailed => "the child never started".to_owned(),
        Termination::JobAssignmentFailed => {
            "the child could not be placed under supervision and was terminated before it ran"
                .to_owned()
        }
        Termination::Orphaned => {
            "the runner died before recording an outcome; what the child did is unknown".to_owned()
        }
        Termination::Exited => unreachable!("was_forced() excludes Exited"),
    })
}

/// Read at most `max_bytes` from a file as UTF-8, lossily.
///
/// Lossy on purpose: a child writing a non-UTF-8 code page is normal on Windows,
/// and failing verification because of an encoding mismatch would be a false
/// negative. The replacement characters cannot create a match for a needle that
/// was really there, so the error is in the safe direction for `Proven` and is
/// reported as `NotProven`/`Unverifiable` otherwise.
fn read_prefix(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; max_bytes.min(64 * 1024 * 1024) as usize];
    let mut filled = 0usize;
    loop {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        if filled == buffer.len() {
            break;
        }
    }
    buffer.truncate(filled);
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jean_protocol::StreamSummary;

    fn outcome(exit: Option<i32>, termination: Termination) -> ChildOutcome {
        ChildOutcome {
            pid: 1,
            exit_code: exit,
            termination,
            stdout: StreamSummary::default(),
            stderr: StreamSummary::default(),
            duration: std::time::Duration::from_millis(1),
            error: None,
        }
    }

    fn with_stdout(mut o: ChildOutcome, path: &str, bytes: u64, truncated: bool) -> ChildOutcome {
        o.stdout = StreamSummary {
            path: Some(path.to_owned()),
            bytes,
            truncated,
        };
        o
    }

    fn temp_file(name: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "jean-verify-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn a_matching_exit_code_is_proven() {
        let v = verify(
            &VerifySpec::ExitCode { expected: 0 },
            &outcome(Some(0), Termination::Exited),
        );
        assert!(v.is_proven(), "{v:?}");
        assert_eq!(v.label(), "proven");
        assert!(v.detail().contains("expected code"), "{v:?}");
    }

    #[test]
    fn a_declared_nonzero_exit_code_can_be_proven() {
        // Some tools legitimately signal a result through a nonzero code, so the
        // expected value comes from the registry rather than being hard-coded to 0.
        let v = verify(
            &VerifySpec::ExitCode { expected: 3 },
            &outcome(Some(3), Termination::Exited),
        );
        assert!(v.is_proven(), "{v:?}");
    }

    #[test]
    fn a_wrong_exit_code_is_not_proven() {
        let v = verify(
            &VerifySpec::ExitCode { expected: 0 },
            &outcome(Some(1), Termination::Exited),
        );
        assert_eq!(v.label(), "not_proven");
        assert!(v.detail().contains("exited with 1"), "{v:?}");
        assert!(v.detail().contains("declared 0"), "{v:?}");
    }

    #[test]
    fn a_timeout_is_never_proven_even_if_the_exit_code_matches() {
        // TIMEOUT_EXIT_CODE is 124. A tool that declared `expected = 124` would
        // otherwise be reported as proven for having been killed.
        let v = verify(
            &VerifySpec::ExitCode { expected: 124 },
            &outcome(Some(124), Termination::Timeout),
        );
        assert_eq!(v.label(), "not_proven");
        assert!(v.detail().contains("timeout elapsed"), "{v:?}");
    }

    #[test]
    fn a_cancelled_child_is_not_proven() {
        let v = verify(
            &VerifySpec::ExitCode { expected: 0 },
            &outcome(Some(0), Termination::Cancelled),
        );
        assert_eq!(v.label(), "not_proven");
        assert!(v.detail().contains("cancelled"), "{v:?}");
    }

    #[test]
    fn a_child_that_never_started_is_not_proven() {
        let v = verify(
            &VerifySpec::ExitCode { expected: 0 },
            &outcome(None, Termination::LaunchFailed),
        );
        assert_eq!(v.label(), "not_proven");
        assert!(v.detail().contains("never started"), "{v:?}");
    }

    #[test]
    fn a_missing_exit_code_from_a_natural_exit_is_unverifiable() {
        let v = verify(
            &VerifySpec::ExitCode { expected: 0 },
            &outcome(None, Termination::Exited),
        );
        assert_eq!(v.label(), "unverifiable");
        assert!(v.detail().contains("no exit code"), "{v:?}");
    }

    #[test]
    fn no_declared_verification_is_unverifiable_not_successful() {
        let v = verify(&VerifySpec::None, &outcome(Some(0), Termination::Exited));
        assert_eq!(v.label(), "unverifiable");
        assert!(
            v.detail().contains("nothing proves"),
            "the message must say the goal is unproven, not that the command worked: {v:?}"
        );
    }

    #[test]
    fn a_found_needle_is_proven() {
        let path = temp_file("found", "Build succeeded. 12 tests passed.\n");
        let o = with_stdout(
            outcome(Some(0), Termination::Exited),
            &path.display().to_string(),
            34,
            false,
        );
        let v = verify(
            &VerifySpec::OutputContains {
                expected: 0,
                needle: "12 tests passed".to_owned(),
                case_insensitive: false,
            },
            &o,
        );
        assert!(v.is_proven(), "{v:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_needle_is_not_proven() {
        let path = temp_file("missing", "Build succeeded.\n");
        let o = with_stdout(
            outcome(Some(0), Termination::Exited),
            &path.display().to_string(),
            17,
            false,
        );
        let v = verify(
            &VerifySpec::OutputContains {
                expected: 0,
                needle: "12 tests passed".to_owned(),
                case_insensitive: false,
            },
            &o,
        );
        assert_eq!(v.label(), "not_proven");
        assert!(v.detail().contains("does not contain"), "{v:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn case_insensitivity_is_opt_in() {
        let path = temp_file("case", "BUILD SUCCEEDED\n");
        let text = path.display().to_string();
        let o = with_stdout(outcome(Some(0), Termination::Exited), &text, 16, false);

        let sensitive = verify(
            &VerifySpec::OutputContains {
                expected: 0,
                needle: "build succeeded".to_owned(),
                case_insensitive: false,
            },
            &o,
        );
        assert_eq!(sensitive.label(), "not_proven");

        let insensitive = verify(
            &VerifySpec::OutputContains {
                expected: 0,
                needle: "build succeeded".to_owned(),
                case_insensitive: true,
            },
            &o,
        );
        assert!(insensitive.is_proven(), "{insensitive:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_wrong_exit_code_short_circuits_the_output_check() {
        let path = temp_file("shortcircuit", "the needle is here\n");
        let o = with_stdout(
            outcome(Some(1), Termination::Exited),
            &path.display().to_string(),
            19,
            false,
        );
        let v = verify(
            &VerifySpec::OutputContains {
                expected: 0,
                needle: "needle".to_owned(),
                case_insensitive: false,
            },
            &o,
        );
        assert_eq!(v.label(), "not_proven");
        assert!(
            v.detail().contains("exited with 1"),
            "the more specific fact must be reported: {v:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_truncated_stream_makes_absence_inconclusive() {
        // The needle could be in the discarded tail. Reporting "not found" would
        // be a claim about output J.E.A.N. threw away.
        let path = temp_file("truncated", "the head of a very long stream\n");
        let o = with_stdout(
            outcome(Some(0), Termination::Exited),
            &path.display().to_string(),
            9_000_000,
            true,
        );
        let v = verify(
            &VerifySpec::OutputContains {
                expected: 0,
                needle: "the tail marker".to_owned(),
                case_insensitive: false,
            },
            &o,
        );
        assert_eq!(v.label(), "unverifiable", "{v:?}");
        assert!(v.detail().contains("not conclusive"), "{v:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_truncated_stream_that_contains_the_needle_is_still_proven() {
        let path = temp_file("truncated-found", "the needle is in the head\n");
        let o = with_stdout(
            outcome(Some(0), Termination::Exited),
            &path.display().to_string(),
            9_000_000,
            true,
        );
        let v = verify(
            &VerifySpec::OutputContains {
                expected: 0,
                needle: "needle".to_owned(),
                case_insensitive: false,
            },
            &o,
        );
        assert!(v.is_proven(), "{v:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn disabled_capture_makes_an_output_predicate_uncheckable() {
        let v = verify(
            &VerifySpec::OutputContains {
                expected: 0,
                needle: "anything".to_owned(),
                case_insensitive: false,
            },
            &outcome(Some(0), Termination::Exited),
        );
        assert_eq!(v.label(), "unverifiable");
        assert!(v.detail().contains("capture was disabled"), "{v:?}");
    }

    #[test]
    fn an_unreadable_artifact_is_unverifiable_not_not_proven() {
        let o = with_stdout(
            outcome(Some(0), Termination::Exited),
            r"Z:\no\such\artifact.log",
            10,
            false,
        );
        let v = verify(
            &VerifySpec::OutputContains {
                expected: 0,
                needle: "anything".to_owned(),
                case_insensitive: false,
            },
            &o,
        );
        assert_eq!(v.label(), "unverifiable");
        assert!(v.detail().contains("could not be read"), "{v:?}");
    }

    #[test]
    fn read_prefix_stops_at_the_requested_bound() {
        let path = temp_file("bound", &"x".repeat(5_000));
        let text = read_prefix(&path, 100).unwrap();
        assert_eq!(text.len(), 100);
        let all = read_prefix(&path, 10_000).unwrap();
        assert_eq!(all.len(), 5_000);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_prefix_tolerates_non_utf8_output() {
        let path =
            std::env::temp_dir().join(format!("jean-verify-bytes-{}.bin", std::process::id()));
        std::fs::write(&path, [0x4f, 0x6b, 0xFF, 0xFE, 0x0A]).unwrap();
        let text = read_prefix(&path, 100).unwrap();
        assert!(text.starts_with("Ok"), "{text:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn every_forced_termination_has_its_own_explanation() {
        let cases = [
            (Termination::Timeout, "timeout"),
            (Termination::Cancelled, "cancelled"),
            (Termination::LaunchFailed, "never started"),
            (Termination::JobAssignmentFailed, "supervision"),
            (Termination::Orphaned, "runner died"),
        ];
        for (termination, expected_fragment) in cases {
            let reason = forced_termination(&outcome(Some(0), termination))
                .unwrap_or_else(|| panic!("{termination} should be forced"));
            assert!(
                reason.contains(expected_fragment),
                "{termination:?} -> {reason:?} should mention {expected_fragment:?}"
            );
        }
        assert!(forced_termination(&outcome(Some(0), Termination::Exited)).is_none());
    }

    #[test]
    fn the_verify_read_bound_is_bounded() {
        // Read through a binding: the point is to fail loudly if someone changes
        // the constant out of the sane range, and `assert!` written directly on a
        // constant is what the lint objects to.
        let bound = VERIFY_READ_BYTES;
        assert!(
            bound >= 64 * 1024,
            "a read bound under 64 KiB cannot prove anything about real output"
        );
        assert!(
            bound <= 16 * 1024 * 1024,
            "a read bound over 16 MiB would make verification read the whole artifact"
        );
    }
}
