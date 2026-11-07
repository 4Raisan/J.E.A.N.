//! The one place a credential is allowed to exist.
//!
//! `model.api_key_env` names an environment variable. Its value is read when a request is built,
//! held for the duration of that request, and never written anywhere: not to the database, not to
//! the audit trail, not to a log line, not to an error message. The scrubbing helpers here are what
//! make "never logged" true even when a provider echoes the authorization header back inside an
//! error body.

use jean_protocol::redact::REDACTION_MARKER;

/// A bearer token read from the environment named by configuration.
///
/// The value is overwritten on drop rather than merely freed, which is a weak guarantee — memory
/// can still be paged or core-dumped — but it is strictly better than leaving a token readable in
/// a freed buffer, and it costs nothing here. What is genuinely enforced is that no code path in
/// this workspace serialises a `Secret`.
pub struct Secret {
    /// The environment variable's name. Always safe to print, and it is what diagnostics show.
    name: String,
    value: Option<Vec<u8>>,
}

impl Secret {
    /// Read the named variable now. Does not cache: a rotated key takes effect on the next
    /// request, and a cached copy would be a second place the secret lives.
    pub fn from_env(name: &str) -> Self {
        let value = std::env::var(name)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .map(|v| v.into_bytes());
        Self {
            name: name.to_owned(),
            value,
        }
    }

    /// A secret that is deliberately absent, e.g. a loopback endpoint that needs no credential.
    pub fn absent(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: None,
        }
    }

    pub fn is_present(&self) -> bool {
        self.value.is_some()
    }

    /// The variable's name, for diagnostics. Never the value.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The value as text, for building one request header. Callers must not store the result.
    pub fn reveal(&self) -> Option<String> {
        self.value
            .as_ref()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }

    /// Replace every occurrence of the secret in `text` with the redaction marker.
    ///
    /// Applied to anything that could carry the provider's own echo of a header back to a log or
    /// an error: the whole point is that a provider's 401 page is quoted to the operator verbatim.
    pub fn scrub(&self, text: &str) -> String {
        let Some(value) = self.reveal() else {
            return text.to_owned();
        };
        if value.len() < 4 {
            // A very short "secret" would redact ordinary text. It is not scrubbed, and the
            // configuration that let it through is a separate problem.
            return text.to_owned();
        }
        let mut out = text.replace(&value, REDACTION_MARKER);
        // Providers sometimes HTML- or JSON-escape what they echo. A key containing a slash comes
        // back as an escaped slash, in one of two spellings, and both have to be caught before the
        // text reaches a log, a record, or an error message shown to the operator.
        let escaped = json_escaped(&value);
        for variant in [
            escaped.clone(),
            escaped.replace("\\/", "\\u002f"),
            value.replace('/', "\\u002f"),
        ] {
            if variant != value {
                out = out.replace(&variant, REDACTION_MARKER);
            }
        }
        out
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        if let Some(bytes) = &mut self.value {
            for byte in bytes.iter_mut() {
                *byte = 0;
            }
        }
    }
}

impl std::fmt::Debug for Secret {
    /// `name` and presence only. A `Debug` implementation that could print the value would end up
    /// in someone's `{:?}` of a client struct.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("name", &self.name)
            .field("present", &self.is_present())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "env var {}", self.name)
    }
}

/// The value as it would appear inside a JSON string literal.
fn json_escaped(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('/', "\\/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_variable_is_absent_not_empty() {
        let secret = Secret::from_env("JEAN_TEST_DEFINITELY_NOT_SET_9F3A");
        assert!(!secret.is_present());
        assert_eq!(secret.reveal(), None);
        assert_eq!(secret.scrub("nothing to hide"), "nothing to hide");
    }

    #[test]
    fn the_debug_format_never_contains_the_value() {
        std::env::set_var("JEAN_TEST_SECRET_VALUE", "hunter2-abcdefgh");
        let secret = Secret::from_env("JEAN_TEST_SECRET_VALUE");
        assert!(secret.is_present());
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("JEAN_TEST_SECRET_VALUE"), "{rendered}");
        std::env::remove_var("JEAN_TEST_SECRET_VALUE");
    }

    fn secret_with(value: &str) -> Secret {
        Secret {
            name: "TEST".to_owned(),
            value: Some(value.to_owned().into_bytes()),
        }
    }

    #[test]
    fn a_provider_echo_of_the_header_is_scrubbed() {
        let secret = secret_with("sk-live/abc123");
        let echoed = "Authorization: Bearer sk-live/abc123 failed";
        assert_eq!(
            secret.scrub(echoed),
            format!("Authorization: Bearer {REDACTION_MARKER} failed")
        );

        // A key that contains a slash comes back escaped, and servers use two spellings for it.
        // Built at runtime so this file never carries the escape sequences itself.
        let slash = '\\';
        let forms = [
            format!(r#"{{"auth":"sk-live{slash}/abc123"}}"#),
            format!(r#"{{"auth":"sk-live{slash}u002fabc123"}}"#),
            r#"{"auth":"sk-live/abc123"}"#.to_owned(),
        ];
        for echoed_body in forms {
            let shown = secret.scrub(&echoed_body);
            assert!(!shown.contains("abc123"), "the value survived: {shown}");
            assert!(
                shown.contains(REDACTION_MARKER),
                "nothing was redacted: {shown}"
            );
        }
    }

    #[test]
    fn a_stupidly_short_secret_is_not_scrubbed_and_says_why_by_being_unusable() {
        let secret = Secret {
            name: "TEST".to_owned(),
            value: Some("ab".to_owned().into_bytes()),
        };
        // Scrubbing "ab" would eat ordinary text; the check above returns the input untouched.
        assert_eq!(secret.scrub("about a cab"), "about a cab");
        assert!(secret.is_present());
    }
}
