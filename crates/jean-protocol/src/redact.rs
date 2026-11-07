//! Secret redaction for anything J.E.A.N. logs or persists.
//!
//! ## The boundary of this guarantee
//!
//! **This is a pattern-matching filter, not a secret detector.** It redacts
//! values that *look like* credentials in the shapes covered by
//! `process.redact_patterns`. It cannot and does not:
//!
//! * recognise a secret that does not match a configured pattern;
//! * recognise a secret passed as a positional argument with no key
//!   (`mytool aGVsbG8gd29ybGQ=` looks like an ordinary token);
//! * recognise a secret read from a file, the credential store, or the
//!   environment at runtime rather than passed on the command line;
//! * protect against a secret that is *produced* by a child process and written
//!   to its stdout — output capture is separately size-bounded but not redacted,
//!   because redacting a 10 MB stream with N regexes is a cost Phase 0 does not
//!   pay. See `PLANNING/06-RISKS.md`.
//!
//! Callers must therefore treat redaction as defence in depth. The primary
//! control is that secrets should never be in an argument vector at all: they
//! belong in the environment (allowlisted) or a credential store.
//!
//! An invalid pattern in user configuration is **not** silently dropped. It is
//! reported once at construction and the remaining patterns still apply, so a
//! typo degrades coverage visibly rather than invisibly.

use regex::Regex;

/// A compiled set of redaction patterns.
#[derive(Debug, Clone)]
pub struct Redactor {
    patterns: Vec<Regex>,
    /// Patterns that failed to compile, kept so the caller can surface them.
    pub invalid_patterns: Vec<String>,
}

/// What replaces a redacted value. Deliberately obvious in a log line: a reader
/// must be able to see that something was removed, not wonder whether the value
/// was ever there.
pub const REDACTION_MARKER: &str = "[REDACTED]";

impl Redactor {
    /// Compile patterns. Invalid ones are collected in `invalid_patterns`.
    pub fn new(patterns: &[String]) -> Self {
        let mut compiled = Vec::with_capacity(patterns.len());
        let mut invalid = Vec::new();

        for p in patterns {
            match Regex::new(p) {
                Ok(re) => compiled.push(re),
                Err(e) => {
                    tracing::warn!(
                        pattern_len = p.len(),
                        error = %e,
                        "process.redact_patterns entry failed to compile and will NOT be applied; \
                         redaction coverage is reduced"
                    );
                    invalid.push(p.clone());
                }
            }
        }

        Self {
            patterns: compiled,
            invalid_patterns: invalid,
        }
    }

    /// A redactor that matches nothing. Used when redaction is disabled by
    /// configuration, and named so that the call site states the intent.
    pub fn disabled() -> Self {
        Self {
            patterns: Vec::new(),
            invalid_patterns: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Redact a single string.
    ///
    /// Patterns that contain capture groups have those groups replaced, which
    /// preserves the key name (`token=[REDACTED]`) so a log line stays readable.
    /// Patterns with no capture group have the whole match replaced.
    pub fn redact(&self, input: &str) -> String {
        if self.patterns.is_empty() {
            return input.to_owned();
        }

        let mut out = input.to_owned();
        for re in &self.patterns {
            out = redact_with(re, &out);
        }
        out
    }

    /// Redact every element of an argument vector, preserving length and order.
    ///
    /// The result is what goes into logs and into `tool_runs.args_json` — never
    /// what goes to `CreateProcessW`.
    pub fn redact_args(&self, args: &[String]) -> Vec<String> {
        args.iter().map(|a| self.redact(a)).collect()
    }

    /// Redact the values of an environment map. Keys are not redacted: a key
    /// name is not a secret, and hiding it would make the log useless.
    pub fn redact_env(
        &self,
        env: &std::collections::BTreeMap<String, String>,
    ) -> std::collections::BTreeMap<String, String> {
        env.iter()
            .map(|(k, v)| (k.clone(), self.redact(v)))
            .collect()
    }
}

/// Replace a regex match with the same text but its secret portion masked.
///
/// If the pattern has capture groups, only the **last** one is replaced — that
/// is the convention `default_redact_patterns` follows, and it keeps the key
/// name visible. If it has none, the whole match is replaced.
fn redact_with(re: &Regex, input: &str) -> String {
    let last_group = re.captures_len().saturating_sub(1);

    if last_group == 0 {
        return re.replace_all(input, REDACTION_MARKER).into_owned();
    }

    re.replace_all(input, |caps: &regex::Captures<'_>| {
        let whole = match caps.get(0) {
            Some(m) => m,
            None => return String::new(),
        };
        let secret = match caps.get(last_group) {
            Some(m) if !m.as_str().is_empty() => m,
            // The optional group did not participate; leave the match alone
            // rather than masking text we did not identify as secret.
            _ => return whole.as_str().to_owned(),
        };

        // Offsets are relative to `input`; convert to offsets within the match
        // so the slice is exact. Using offsets rather than `str::find` avoids
        // masking the wrong occurrence of a repeated substring.
        let base = whole.start();
        let start = secret.start() - base;
        let end = secret.end() - base;
        let text = whole.as_str();

        let mut out = String::with_capacity(text.len() + REDACTION_MARKER.len());
        out.push_str(&text[..start]);
        out.push_str(REDACTION_MARKER);
        out.push_str(&text[end..]);
        out
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_redact_patterns;

    fn redactor() -> Redactor {
        Redactor::new(&default_redact_patterns())
    }

    #[test]
    fn default_patterns_all_compile() {
        let r = redactor();
        assert!(
            r.invalid_patterns.is_empty(),
            "shipped patterns must be valid regexes: {:?}",
            r.invalid_patterns
        );
        assert!(!r.is_empty());
    }

    #[test]
    fn key_value_shapes_are_redacted_and_the_key_survives() {
        let r = redactor();
        for input in [
            "token=abc123",
            "TOKEN: abc123",
            "password=hunter2",
            "api_key=zzz",
            "secret: s3cr3t",
            "Authorization=Bearer-ish",
        ] {
            let out = r.redact(input);
            assert!(
                out.contains(REDACTION_MARKER),
                "{input:?} was not redacted -> {out:?}"
            );
            assert!(
                !out.contains("abc123") && !out.contains("hunter2") && !out.contains("s3cr3t"),
                "secret leaked through: {input:?} -> {out:?}"
            );
        }

        // The key name must survive so the log line is still diagnosable.
        let out = r.redact("token=abc123");
        assert!(out.starts_with("token="), "lost the key name: {out:?}");
    }

    #[test]
    fn bearer_prefix_is_redacted() {
        let r = redactor();
        let out = r.redact("Bearer abc.def-ghi_123");
        assert!(out.contains(REDACTION_MARKER), "{out:?}");
        assert!(!out.contains("abc.def-ghi_123"), "leaked: {out:?}");
        assert!(
            out.contains("Bearer"),
            "the scheme word should survive: {out:?}"
        );
    }

    /// The pattern-ordering regression this exists to catch: if the generic
    /// `key=value` pattern runs first it redacts the word `Bearer` as the value
    /// of `Authorization`, and the real token survives into the log.
    #[test]
    fn authorization_header_does_not_leak_the_token() {
        let r = redactor();
        let out = r.redact("Authorization: Bearer abc.def-ghi_123");
        assert!(
            !out.contains("abc.def-ghi_123"),
            "token leaked through sequential redaction: {out:?}"
        );
        assert!(out.contains(REDACTION_MARKER), "{out:?}");
    }

    #[test]
    fn known_provider_prefixes_are_redacted() {
        let r = redactor();
        for input in [
            "sk-abcdefghij1234567890",
            "ghp_abcdefghij1234567890",
            "github_pat_abcdefghij1234567890",
            "xoxb-abcdefghij1234567890",
        ] {
            let out = r.redact(input);
            assert!(out.contains(REDACTION_MARKER), "{input:?} -> {out:?}");
        }
    }

    #[test]
    fn a_secret_passed_as_a_command_line_flag_is_redacted() {
        // The shape the generic pattern cannot see: `-Password hunter2`, with a
        // space instead of an `=`. This is how credentials are typed on Windows
        // command lines, and the first version of the pattern list wrote one into
        // the audit log verbatim.
        let r = redactor();
        // Each case names the secret it expects to disappear, rather than guessing
        // which token in the line was meant to be the credential.
        for (input, secret) in [
            (
                "Set-Content -Password hunter2-SECRET-9999",
                "hunter2-SECRET-9999",
            ),
            (
                "aws --api-key sk-SECRETVALUE0123456 configure",
                "sk-SECRETVALUE0123456",
            ),
            ("/secret hunter2", "hunter2"),
            ("--token=abc123def456", "abc123def456"),
        ] {
            let out = r.redact(input);
            assert!(out.contains(REDACTION_MARKER), "{input:?} -> {out:?}");
            assert!(
                !out.contains(secret),
                "the flag value survived redaction: {input:?} -> {out:?}"
            );
        }

        // The flag name stays, because a log that says only `[REDACTED]` cannot
        // tell you which argument was the problem.
        let out = r.redact("Set-Content -Password hunter2");
        assert!(out.contains("-Password"), "lost the flag name: {out:?}");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        let r = redactor();
        for input in [
            "git status",
            "npm test -- --watch",
            "C:\\Users\\me\\project",
            "--output-format json",
            "",
        ] {
            assert_eq!(r.redact(input), input, "{input:?} should be unchanged");
        }
    }

    #[test]
    fn argument_vectors_keep_length_and_order() {
        let r = redactor();
        let args = vec![
            "-c".to_owned(),
            "token=abc123".to_owned(),
            "plain".to_owned(),
        ];
        let out = r.redact_args(&args);
        assert_eq!(out.len(), args.len());
        assert_eq!(out[0], "-c");
        assert!(out[1].contains(REDACTION_MARKER));
        assert_eq!(out[2], "plain");
    }

    #[test]
    fn env_values_are_redacted_but_keys_are_not() {
        let r = redactor();
        let mut env = std::collections::BTreeMap::new();
        env.insert("PATH".to_owned(), "C:\\bin".to_owned());
        env.insert("TOKEN".to_owned(), "password=hunter2".to_owned());
        let out = r.redact_env(&env);
        assert_eq!(out.get("PATH").unwrap(), "C:\\bin");
        assert!(out.get("TOKEN").unwrap().contains(REDACTION_MARKER));
        assert!(out.contains_key("TOKEN"), "key names must stay visible");
    }

    #[test]
    fn an_invalid_pattern_is_reported_not_silently_dropped() {
        let r = Redactor::new(&["(".to_owned(), "token=(\\S+)".to_owned()]);
        assert_eq!(r.invalid_patterns, vec!["(".to_owned()]);
        // The valid pattern still applies.
        assert!(r.redact("token=abc").contains(REDACTION_MARKER));
    }

    #[test]
    fn a_disabled_redactor_is_a_no_op() {
        let r = Redactor::disabled();
        assert!(r.is_empty());
        assert_eq!(r.redact("token=abc123"), "token=abc123");
    }

    #[test]
    fn redaction_is_idempotent() {
        let r = redactor();
        let once = r.redact("token=abc123");
        let twice = r.redact(&once);
        assert_eq!(once, twice, "redacting twice must not compound");
    }

    #[test]
    fn documented_limitation_positional_secrets_are_not_caught() {
        // This test exists to make the boundary explicit rather than implicit.
        // A bare secret with no key and no known prefix is NOT detected, and
        // must not be claimed as detected anywhere in the documentation.
        let r = redactor();
        let bare = "aGVsbG8gd29ybGQgdGhpcyBpcyBhIHNlY3JldA";
        assert_eq!(
            r.redact(bare),
            bare,
            "if this starts passing, the pattern set changed and the \
             documented redaction boundary in this module must be updated"
        );
    }
}
