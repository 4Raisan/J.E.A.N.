//! The environment block handed to `CreateProcessW`.
//!
//! `CREATE_UNICODE_ENVIRONMENT` requires a single buffer of `KEY=VALUE` wide
//! strings, each NUL-terminated, with one extra NUL at the end. This module
//! builds that buffer and validates the keys, and nothing else.
//!
//! ## The allowlist is enforced upstream, not here
//!
//! `jean-core` decides *which* variables a child may see, by intersecting the
//! tool's `env` with `process.environment_allowlist` and the parent environment.
//! By the time a map reaches [`encode`] it is already the final set. Re-checking
//! the allowlist here would need a second copy of the policy and would let the
//! two disagree.
//!
//! What this module does check is what only it can see: that a key is
//! encodable at all. A key containing `=` would silently split into a different
//! variable in the child, and a NUL anywhere would truncate the block — in both
//! cases the child would run with an environment nobody approved.

use std::collections::BTreeMap;

use crate::error::{ProcessError, Result};

/// Encode a variable set as a UTF-16LE environment block.
///
/// The result is the exact byte buffer to pass as `lpEnvironment`. It is empty
/// only when `vars` is empty, which is legal: the child then starts with no
/// environment at all. That is a valid configuration for a hermetic tool, so it
/// is not treated as an error.
///
/// Variables are emitted in case-insensitive key order. Windows treats variable
/// names as case-insensitive, so a plain byte sort could place `Path` and
/// `PATH` far apart; the documented advice is to keep the block sorted the way
/// the system would, which avoids surprising a child that scans it linearly.
pub fn encode(vars: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    validate(vars)?;

    let mut entries: Vec<(&String, &String)> = vars.iter().collect();
    entries.sort_by(|a, b| {
        let by_key = a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase());
        by_key.then_with(|| a.0.cmp(b.0))
    });

    let mut block: Vec<u16> = Vec::new();
    for (key, value) in entries {
        block.extend(key.encode_utf16());
        block.push(b'=' as u16);
        block.extend(value.encode_utf16());
        block.push(0);
    }
    // The double NUL terminator. Without it the child's CRT reads past the end.
    block.push(0);

    let mut bytes = Vec::with_capacity(block.len() * 2);
    for unit in block {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(bytes)
}

/// Reject keys and values that cannot be represented.
fn validate(vars: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in vars {
        if key.is_empty() {
            return Err(invalid("an environment key is empty"));
        }
        if key.contains('\0') || value.contains('\0') {
            return Err(invalid(&format!(
                "environment entry {key:?} contains a NUL byte, which would \
                 truncate the block and leave the child with an unapproved \
                 environment"
            )));
        }
        // Windows reserves names of the form `=X:` for per-drive current
        // directories. Letting a tool set one would let it move the child's
        // notion of a drive's cwd behind the supervisor's back.
        //
        // This must be tested before the general `=` rule below: every reserved
        // name starts with `=`, so the general rule would otherwise always win and
        // this branch would be unreachable — losing the message that explains the
        // actual danger.
        if key.starts_with('=') {
            return Err(invalid(&format!(
                "environment key {key:?} starts with '='; those names are \
                 reserved by Windows for per-drive working directories"
            )));
        }
        if key.contains('=') {
            return Err(invalid(&format!(
                "environment key {key:?} contains '=', which would split into a \
                 different variable in the child"
            )));
        }
    }
    Ok(())
}

fn invalid(reason: &str) -> ProcessError {
    ProcessError::Invalid {
        field: "env",
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Decode a block back into pairs, the way a child's CRT would.
    fn decode(bytes: &[u8]) -> Vec<(String, String)> {
        assert_eq!(bytes.len() % 2, 0, "a UTF-16LE block has an even length");
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();

        let mut out = Vec::new();
        let mut current: Vec<u16> = Vec::new();
        for unit in units {
            if unit == 0 {
                if current.is_empty() {
                    // The terminating empty string.
                    break;
                }
                let s = String::from_utf16(&current).expect("valid UTF-16");
                let (k, v) = s.split_once('=').expect("every entry has a separator");
                out.push((k.to_owned(), v.to_owned()));
                current.clear();
            } else {
                current.push(unit);
            }
        }
        out
    }

    #[test]
    fn a_single_variable_round_trips() {
        let bytes = encode(&map(&[("PATH", r"C:\Windows\System32")])).unwrap();
        assert_eq!(
            decode(&bytes),
            vec![("PATH".to_owned(), r"C:\Windows\System32".to_owned())]
        );
    }

    #[test]
    fn the_block_ends_with_exactly_two_nul_units() {
        let bytes = encode(&map(&[("A", "1")])).unwrap();
        let tail: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .rev()
            .take(3)
            .map(|pair| u16::from_le_bytes(*pair))
            .rev()
            .collect();
        assert_eq!(tail[1], 0, "entry terminator");
        assert_eq!(tail[2], 0, "block terminator");
        assert_ne!(tail[0], 0, "the value '1' precedes them");
    }

    #[test]
    fn an_empty_set_still_terminates() {
        let bytes = encode(&BTreeMap::new()).unwrap();
        assert_eq!(bytes, vec![0, 0], "just the double NUL");
    }

    #[test]
    fn multiple_variables_round_trip() {
        let input = map(&[
            ("PATH", r"C:\Windows\System32;C:\Windows"),
            ("SystemRoot", r"C:\Windows"),
            ("TEMP", r"C:\Users\test\AppData\Local\Temp"),
            (
                "PSModulePath",
                r"C:\Program Files\WindowsPowerShell\Modules",
            ),
        ]);
        let decoded = decode(&encode(&input).unwrap());
        assert_eq!(decoded.len(), 4);
        for (k, v) in &input {
            assert!(
                decoded.contains(&(k.clone(), v.clone())),
                "{k} did not survive encoding"
            );
        }
    }

    #[test]
    fn entries_are_emitted_in_case_insensitive_key_order() {
        let bytes = encode(&map(&[("zeta", "1"), ("Alpha", "2"), ("beta", "3")])).unwrap();
        let keys: Vec<String> = decode(&bytes).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["Alpha", "beta", "zeta"]);
    }

    #[test]
    fn non_ascii_values_are_encoded_as_utf16() {
        let input = map(&[("JEAN_NOTE", "héllo 日本語 🎉")]);
        let decoded = decode(&encode(&input).unwrap());
        assert_eq!(decoded[0].1, "héllo 日本語 🎉");
        // The emoji is a surrogate pair, so the byte count is not the char count.
        assert!(encode(&input).unwrap().len() > "JEAN_NOTE=héllo 日本語 🎉".len());
    }

    #[test]
    fn an_empty_value_is_preserved() {
        let decoded = decode(&encode(&map(&[("EMPTY", "")])).unwrap());
        assert_eq!(decoded, vec![("EMPTY".to_owned(), String::new())]);
    }

    #[test]
    fn a_key_containing_an_equals_sign_is_refused() {
        let err = encode(&map(&[("A=B", "c")])).unwrap_err();
        match err {
            ProcessError::Invalid { field, reason } => {
                assert_eq!(field, "env");
                assert!(reason.contains("A=B"), "{reason}");
                assert!(reason.contains("different variable"), "{reason}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_key_is_refused() {
        assert!(encode(&map(&[("", "v")])).is_err());
    }

    #[test]
    fn a_nul_in_a_key_or_value_is_refused() {
        let k = encode(&map(&[("A\0B", "v")])).unwrap_err();
        assert!(k.to_string().contains("NUL"), "{k}");
        let v = encode(&map(&[("A", "v\0w")])).unwrap_err();
        assert!(v.to_string().contains("NUL"), "{v}");
    }

    #[test]
    fn a_reserved_drive_letter_key_is_refused() {
        // `=C:` is how cmd.exe remembers each drive's current directory. A tool
        // that could set it could redirect a relative path the supervisor had
        // already approved.
        let err = encode(&map(&[("=C:", r"C:\elsewhere")])).unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    #[test]
    fn the_allowlist_is_not_rechecked_here() {
        // Documented behaviour, asserted so a future "helpful" filter added in
        // this module fails a test instead of silently narrowing a tool's
        // environment twice.
        let decoded = decode(&encode(&map(&[("NOT_ON_ANY_ALLOWLIST", "x")])).unwrap());
        assert_eq!(decoded[0].0, "NOT_ON_ANY_ALLOWLIST");
    }
}
