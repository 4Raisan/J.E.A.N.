//! Building a Windows command line from an argument vector.
//!
//! ## Why this exists
//!
//! `CreateProcessW` takes a single command-line *string*, not an argv array.
//! Every child process then re-splits that string with the MSVCRT rules
//! (`CommandLineToArgvW`). So there is no way to hand a child an argument vector
//! except by encoding it correctly into a string.
//!
//! That is not the same as building a shell command, and the difference is the
//! whole point. J.E.A.N. passes `lpApplicationName` explicitly and never routes
//! through `cmd.exe`, so nothing here is ever interpreted by a shell: `&`, `|`,
//! `>`, `;` and `$` are ordinary characters that arrive at the child verbatim.
//! The only metacharacter that matters is `"`, because that is the one the
//! MSVCRT splitter treats specially.
//!
//! ## The rules implemented
//!
//! From the `CommandLineToArgvW` documentation:
//!
//! * An argument with no whitespace and no `"` needs no quoting.
//! * An empty argument must be written as `""`, or it disappears entirely.
//! * Inside quotes, `2n` backslashes before a `"` become `n` backslashes and end
//!   the quoted run; `2n+1` become `n` backslashes plus a literal `"`.
//! * Therefore, to emit a literal `"`, write `\"`, and to emit `n` backslashes
//!   immediately before a literal `"`, write `2n+1` of them.
//! * Backslashes immediately before the *closing* quote must be doubled, or they
//!   would escape the quote and swallow the rest of the line.
//!
//! Getting this wrong is a security bug, not a cosmetic one: an under-escaped
//! quote lets a tool argument terminate its own quoted run and inject extra
//! arguments into the child.

use std::ffi::OsStr;

use crate::error::{ProcessError, Result};

/// Characters that force an argument to be quoted.
///
/// `\t`, `\n` and vertical tab are included because the MSVCRT splitter treats
/// them as separators. `\r` is not in the documented list but is
/// whitespace-like enough that quoting it costs nothing and removes a doubt.
///
/// The vertical tab is written as `\u{b}` because Rust has no `\v` escape.
fn needs_quoting(arg: &str) -> bool {
    arg.is_empty() || arg.contains([' ', '\t', '\n', '\r', '\u{b}', '"'])
}

/// Encode one argument, appending to `out`.
fn push_arg(out: &mut String, arg: &str) -> Result<()> {
    // A NUL cannot be represented at all: the command line is a NUL-terminated
    // wide string, so anything after it would be silently dropped. Refusing is
    // better than launching a child with half the intended arguments.
    if arg.contains('\0') {
        return Err(ProcessError::Invalid {
            field: "args",
            reason: "an argument contains a NUL byte, which cannot be passed to CreateProcessW"
                .to_owned(),
        });
    }

    if !needs_quoting(arg) {
        out.push_str(arg);
        return Ok(());
    }

    out.push('"');
    let mut pending_backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => pending_backslashes += 1,
            '"' => {
                // 2n+1 backslashes then the escaped quote.
                for _ in 0..(pending_backslashes * 2 + 1) {
                    out.push('\\');
                }
                out.push('"');
                pending_backslashes = 0;
            }
            other => {
                for _ in 0..pending_backslashes {
                    out.push('\\');
                }
                out.push(other);
                pending_backslashes = 0;
            }
        }
    }
    // Trailing backslashes sit right before the closing quote, so they must be
    // doubled or they would escape it.
    for _ in 0..(pending_backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    Ok(())
}

/// Build the `lpCommandLine` string for `CreateProcessW`.
///
/// `executable` is emitted as argv[0] using the same quoting rules. It is also
/// passed separately as `lpApplicationName`, so the child's argv[0] is
/// conventional but Windows does not use it to find the program — which is what
/// removes any ambiguity about which binary runs.
///
/// Arguments are already validated by [`jean_protocol::ToolDef::validate`]; this
/// function only refuses NUL bytes, which no earlier layer can see because the
/// restriction belongs to the Win32 encoding.
pub fn build_command_line(executable: &str, args: &[String]) -> Result<String> {
    let mut out = String::new();
    push_arg(&mut out, executable)?;
    for arg in args {
        out.push(' ');
        push_arg(&mut out, arg)?;
    }
    Ok(out)
}

/// Build the `lpCommandLine` buffer as UTF-16 code units, without a terminator.
///
/// argv[0] arrives as an [`OsStr`] because a Windows path is UTF-16 and need not
/// be representable in UTF-8. A path containing unpaired surrogates cannot be
/// encoded into a command-line string at all, so it is refused with an
/// explanation rather than silently mangled by a lossy conversion — a mangled
/// argv[0] is confusing, and J.E.A.N. would rather say what is wrong.
///
/// The arguments are encoded by exactly the same [`build_command_line`] that the
/// tests exercise, so there is one quoting implementation, not two.
pub fn build_command_line_units(executable: &OsStr, args: &[String]) -> Result<Vec<u16>> {
    let executable_str = executable.to_str().ok_or_else(|| ProcessError::Invalid {
        field: "executable",
        reason: "the path contains unpaired UTF-16 surrogates and cannot be encoded into a command line; move or rename the executable to a representable path"
            .to_owned(),
    })?;
    // `OsStr::to_str` returning `Some` is exactly the guarantee that the
    // conversion round-trips, so no separate check is needed.
    Ok(build_command_line(executable_str, args)?
        .encode_utf16()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Re-split an encoded command line the way `CommandLineToArgvW` does.
    ///
    /// Written from the documented rules rather than calling the Win32 function,
    /// so the quoting logic is testable without a Windows build. It handles the
    /// cases J.E.A.N. can produce: backslash runs, escaped quotes, and quoted
    /// runs. It deliberately does not implement the `/S` switch or the
    /// "first argument is the executable" special cases.
    fn split(line: &str) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        let mut args = Vec::new();
        let mut i = 0usize;

        while i < chars.len() {
            // Skip separators between arguments.
            while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                i += 1;
            }
            if i >= chars.len() {
                break;
            }

            let mut arg = String::new();
            let mut in_quotes = false;
            let mut has_arg = false;

            while i < chars.len() {
                let mut backslashes = 0usize;
                while i < chars.len() && chars[i] == '\\' {
                    backslashes += 1;
                    i += 1;
                }

                if i >= chars.len() {
                    for _ in 0..backslashes {
                        arg.push('\\');
                    }
                    break;
                }

                if chars[i] == '"' {
                    // Odd run: the last backslash escapes the quote.
                    for _ in 0..(backslashes / 2) {
                        arg.push('\\');
                    }
                    if backslashes % 2 == 1 {
                        arg.push('"');
                        has_arg = true;
                    } else {
                        in_quotes = !in_quotes;
                        has_arg = true;
                    }
                    i += 1;
                    continue;
                }

                for _ in 0..backslashes {
                    arg.push('\\');
                }

                let c = chars[i];
                if !in_quotes && (c == ' ' || c == '\t') {
                    break;
                }
                arg.push(c);
                has_arg = true;
                i += 1;
            }

            if has_arg {
                args.push(arg);
            }
        }
        args
    }

    fn roundtrip(executable: &str, args: &[&str]) {
        let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let line = build_command_line(executable, &owned).expect("encodes");
        let back = split(&line);
        let mut expected = vec![executable.to_string()];
        expected.extend(owned);
        assert_eq!(
            back, expected,
            "round trip failed for {args:?}\n  line: {line}\n  split: {back:?}"
        );
    }

    #[test]
    fn plain_arguments_are_not_quoted() {
        let line = build_command_line(
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            &["-NoProfile".to_owned(), "-Command".to_owned()],
        )
        .unwrap();
        assert_eq!(
            line,
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -Command"
        );
    }

    #[test]
    fn arguments_with_spaces_are_quoted() {
        let line = build_command_line("prog", &["two words".to_owned()]).unwrap();
        assert_eq!(line, r#"prog "two words""#);
    }

    #[test]
    fn an_empty_argument_survives_as_an_empty_argument() {
        // Without the explicit `""` this argument would vanish and every
        // following argument would shift one position left.
        roundtrip("prog", &["", "after"]);
        let line = build_command_line("prog", &["".to_owned()]).unwrap();
        assert_eq!(line, r#"prog """#);
    }

    #[test]
    fn embedded_quotes_are_escaped() {
        roundtrip("prog", &[r#"say "hi" now"#]);
        roundtrip("prog", &[r#"""#]);
        roundtrip("prog", &[r#"a"b"#]);
    }

    #[test]
    fn backslashes_before_a_quote_are_doubled_plus_one() {
        // One literal backslash then a literal quote: `\\"`.
        roundtrip("prog", &[r#"a\"b"#]);
        // Two literal backslashes then a literal quote: `\\\\"`.
        roundtrip("prog", &[r#"a\\"b"#]);
        roundtrip("prog", &[r#"a\\\"b"#]);
    }

    #[test]
    fn trailing_backslashes_do_not_escape_the_closing_quote() {
        // The classic failure: `prog "C:\path\"` would swallow the next
        // argument, because `\"` is a literal quote rather than a close.
        roundtrip("prog", &[r"C:\path\", "next"]);
        roundtrip("prog", &[r"C:\path\\", "next"]);
        roundtrip("prog", &[r"C:\Program Files\x\\"]);
    }

    #[test]
    fn backslashes_not_before_a_quote_are_left_alone() {
        roundtrip("prog", &[r"C:\Windows\System32", r"\\server\share\file"]);
        // Backslashes only need doubling immediately before a quote; the MSVCRT
        // splitter takes them literally anywhere else, so a UNC path needs no
        // quoting at all. This pins the minimal-quoting policy rather than
        // trusting the round trip alone: an argument that does not contain
        // whitespace or a quote is emitted verbatim, which is what makes a
        // recorded `args_json` readable as the argv the child was given.
        let line = build_command_line("prog", &[r"\\server\share".to_owned()]).unwrap();
        assert_eq!(line, r"prog \\server\share");
    }

    #[test]
    fn shell_metacharacters_are_ordinary_text() {
        // These must arrive at the child exactly as written. Nothing in this
        // crate hands them to a shell, and the quoting rules must not mangle
        // them either.
        roundtrip(
            "prog",
            &[
                "a && b", "x | y", "p > q", "r < s", "t; u", "$(cmd)", "`whoami`", "%PATH%",
            ],
        );
    }

    #[test]
    fn whitespace_variants_force_quoting() {
        roundtrip(
            "prog",
            &["tab\there", "nl\nhere", "cr\rhere", "vt\u{000b}here"],
        );
    }

    #[test]
    fn non_ascii_arguments_survive() {
        roundtrip(
            "prog",
            &["héllo wörld", "日本語 の テキスト", "emoji 🎉 inside"],
        );
    }

    #[test]
    fn an_executable_path_with_spaces_is_quoted() {
        roundtrip(r"C:\Program Files\Git\bin\git.exe", &["status"]);
    }

    #[test]
    fn a_nul_byte_in_an_argument_is_refused() {
        let err = build_command_line("prog", &["before\0after".to_owned()]).unwrap_err();
        match err {
            ProcessError::Invalid { field, reason } => {
                assert_eq!(field, "args");
                assert!(reason.contains("NUL"), "{reason}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_nul_byte_in_the_executable_is_refused() {
        assert!(build_command_line("pro\0g", &[]).is_err());
    }

    #[test]
    fn the_wide_builder_matches_the_string_builder() {
        let args = vec![
            "-Command".to_owned(),
            "two words".to_owned(),
            r#"a\"b"#.to_owned(),
        ];
        let wide = build_command_line_units(OsStr::new("prog.exe"), &args).unwrap();
        let expected: Vec<u16> = build_command_line("prog.exe", &args)
            .unwrap()
            .encode_utf16()
            .collect();
        assert_eq!(wide, expected);
        assert!(
            wide.iter().all(|u| *u != 0),
            "the terminator is added by the caller, not here"
        );
    }

    #[test]
    fn the_wide_builder_handles_a_path_with_spaces() {
        let wide = build_command_line_units(
            OsStr::new(r"C:\Program Files\Git\cmd\git.exe"),
            &["status".to_owned()],
        )
        .unwrap();
        let text = String::from_utf16(&wide).unwrap();
        assert_eq!(text, r#""C:\Program Files\Git\cmd\git.exe" status"#);
    }

    #[test]
    fn the_wide_builder_keeps_non_ascii_paths() {
        let path = r"C:\Programme\Grüße\werkzeug.exe";
        let wide = build_command_line_units(OsStr::new(path), &[]).unwrap();
        assert_eq!(String::from_utf16(&wide).unwrap(), path);
    }

    #[test]
    fn no_arguments_produces_just_the_executable() {
        assert_eq!(build_command_line("prog", &[]).unwrap(), "prog");
    }

    #[test]
    fn a_long_argument_vector_stays_faithful() {
        let args: Vec<&str> = (0..200)
            .map(|i| {
                if i % 3 == 0 {
                    "plain"
                } else if i % 3 == 1 {
                    "with space"
                } else {
                    r#"with "quote" and \"#
                }
            })
            .collect();
        roundtrip("prog", &args);
    }
}
