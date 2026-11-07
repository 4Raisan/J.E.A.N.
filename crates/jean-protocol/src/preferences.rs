use crate::config::AppConfig;

pub const SUPPORTED_KEYS: &[&str] = &[
    "voice.enabled",
    "voice.mode",
    "desktop.enabled",
    "desktop.retention",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreferenceKey {
    VoiceEnabled,
    VoiceMode,
    DesktopEnabled,
    DesktopRetention,
}

impl PreferenceKey {
    pub const ALL: [Self; 4] = [
        Self::VoiceEnabled,
        Self::VoiceMode,
        Self::DesktopEnabled,
        Self::DesktopRetention,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VoiceEnabled => "voice.enabled",
            Self::VoiceMode => "voice.mode",
            Self::DesktopEnabled => "desktop.enabled",
            Self::DesktopRetention => "desktop.retention",
        }
    }

    pub fn parse(key: &str) -> Option<Self> {
        let trimmed = key.trim();
        Self::ALL.into_iter().find(|k| k.as_str() == trimmed)
    }

    pub const fn accepted_values(self) -> &'static [&'static str] {
        match self {
            Self::VoiceEnabled | Self::DesktopEnabled => &["true", "false"],
            Self::VoiceMode => &["push_to_talk"],
            Self::DesktopRetention => &["discard_after_task", "keep_24h", "keep_until_manual"],
        }
    }

    pub const fn reader(self) -> &'static str {
        match self {
            Self::VoiceEnabled => "Controller::from_context starts no voice worker while false",
            Self::VoiceMode => "VoiceConfig::validate accepts only push_to_talk",
            Self::DesktopEnabled => "jean_core::desktop refuses an action while false",
            Self::DesktopRetention => "jean_core::captures expires only keep_24h frames",
        }
    }

    pub const fn meaning(self) -> &'static str {
        match self {
            Self::VoiceEnabled => "whether the voice worker is started by `jean interact`",
            Self::VoiceMode => "how a spoken goal is captured and submitted",
            Self::DesktopEnabled => "whether J.E.A.N. may act on this desktop at all",
            Self::DesktopRetention => "how long a captured screen frame is kept on disk",
        }
    }

    pub fn validate(self, value: &str) -> Result<(), String> {
        let accepted = self.accepted_values();
        let matched = accepted
            .iter()
            .find(|v| self.eq_value(v, value))
            .ok_or_else(|| {
                format!(
                    "{} accepts {}, got {value:?}",
                    self.as_str(),
                    accepted.join(" or ")
                )
            })?;
        let _ = matched;
        Ok(())
    }

    fn eq_value(self, accepted: &str, value: &str) -> bool {
        let value = value.trim();
        if self.is_boolean() {
            value.eq_ignore_ascii_case(accepted)
        } else {
            value == accepted
        }
    }

    const fn is_boolean(self) -> bool {
        matches!(self, Self::VoiceEnabled | Self::DesktopEnabled)
    }

    pub fn canonical_value(self, value: &str) -> String {
        let value = value.trim();
        if self.is_boolean() {
            return if value.eq_ignore_ascii_case("true") {
                "true".to_owned()
            } else {
                "false".to_owned()
            };
        }
        value.to_owned()
    }

    pub fn apply(self, value: &str, config: &mut AppConfig) -> Result<(), String> {
        self.validate(value)?;
        match self {
            Self::VoiceEnabled => config.voice.enabled = value.trim().eq_ignore_ascii_case("true"),
            Self::VoiceMode => config.voice.mode = self.canonical_value(value),
            Self::DesktopEnabled => {
                config.desktop.enabled = value.trim().eq_ignore_ascii_case("true")
            }
            Self::DesktopRetention => config.desktop.retention = self.canonical_value(value),
        }
        Ok(())
    }
}

pub fn overlay(rows: &[(String, String)], config: &mut AppConfig) -> Result<Vec<String>, String> {
    let mut seen = std::collections::HashSet::new();
    for (key, _) in rows {
        if !SUPPORTED_KEYS.contains(&key.as_str()) || !seen.insert(key) {
            return Err("unknown or duplicate stored preference key".into());
        }
    }
    let mut candidate = config.clone();
    let mut applied = Vec::new();
    for key in SUPPORTED_KEYS {
        let Some(raw) = rows.iter().find(|(k, _)| k == key).map(|(_, v)| v) else {
            continue;
        };
        let parsed = PreferenceKey::parse(key)
            .ok_or_else(|| format!("{key:?} is not a supported preference key"))?;
        parsed
            .apply(raw, &mut candidate)
            .map_err(|reason| format!("preference {key}: {reason}"))?;
        applied.push((*key).to_owned());
    }
    candidate.validate().map_err(|e| {
        format!("applying these preferences would make the configuration invalid: {e}")
    })?;
    *config = candidate;
    Ok(applied)
}

/// Recover the only preference field masked by environment overrides.
/// Other environment fields do not overlap the closed preference vocabulary.
pub fn file_preference_base(loaded: &crate::config::LoadedConfig) -> Result<AppConfig, String> {
    let mut base = loaded.config.clone();
    base.voice.enabled = AppConfig::default().voice.enabled;
    for source in &loaded.sources {
        if let crate::config::ConfigSource::File(path) = source {
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let value: toml::Value = toml::from_str(&text).map_err(|e| e.to_string())?;
            if let Some(enabled) = value.get("voice").and_then(|v| v.get("enabled")) {
                base.voice.enabled = enabled.as_bool().ok_or("voice.enabled must be boolean")?;
            }
        }
    }
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_key_parses_back_from_its_own_name() {
        for name in SUPPORTED_KEYS {
            let key = PreferenceKey::parse(name)
                .unwrap_or_else(|| panic!("{name} is listed as supported but does not parse"));
            assert_eq!(key.as_str(), *name);
            assert!(!key.reader().is_empty(), "{name} must name a reader");
        }
        assert_eq!(SUPPORTED_KEYS.len(), PreferenceKey::ALL.len());
    }

    #[test]
    fn a_key_with_no_reader_in_this_build_is_not_parsed() {
        for name in [
            "ui.theme",
            "ui.show_task_board",
            "vision.enabled",
            "model.api_key",
            "policy.default_tier",
            "workspace.root",
            "",
            "voice",
        ] {
            assert!(
                PreferenceKey::parse(name).is_none(),
                "{name:?} has no reader in this build and must not be storable"
            );
        }
    }

    #[test]
    fn a_key_name_is_matched_after_trimming_only() {
        assert_eq!(
            PreferenceKey::parse("  voice.mode  ").map(|k| k.as_str()),
            Some("voice.mode")
        );
        assert!(PreferenceKey::parse("voice.enabled_extra").is_none());
    }

    #[test]
    fn a_boolean_accepts_both_spellings_and_stores_one() {
        let key = PreferenceKey::VoiceEnabled;
        assert!(key.validate("TRUE").is_ok());
        assert!(key.validate("  false ").is_ok());
        assert_eq!(key.canonical_value("TRUE"), "true");
        assert_eq!(key.canonical_value("  false "), "false");
        for bad in ["yes", "1", "on", ""] {
            let reason = key
                .validate(bad)
                .expect_err("a boolean word must be refused");
            assert!(
                reason.contains("true or false"),
                "the refusal must say what is accepted: {reason}"
            );
        }
    }

    #[test]
    fn a_retention_word_is_refused_rather_than_defaulted() {
        let key = PreferenceKey::DesktopRetention;
        for good in ["discard_after_task", "keep_24h", "keep_until_manual"] {
            assert!(
                key.validate(good).is_ok(),
                "{good} is the shipped vocabulary"
            );
        }
        let reason = key
            .validate("keep_forever")
            .expect_err("an unknown retention word must be refused");
        assert!(reason.contains("discard_after_task"), "{reason}");
        assert!(
            key.validate("KEEP_24H").is_err(),
            "retention is case-sensitive"
        );
    }

    #[test]
    fn applying_a_voice_mode_only_ever_produces_push_to_talk() {
        let mut config = AppConfig::default();
        PreferenceKey::VoiceMode
            .apply("push_to_talk", &mut config)
            .expect("the only implemented mode");
        assert_eq!(config.voice.mode, "push_to_talk");
        assert!(PreferenceKey::VoiceMode
            .apply("wake_word", &mut config)
            .is_err());
        assert!(config.voice.validate().is_ok());
    }

    #[test]
    fn the_overlay_applies_supported_rows_and_leaves_the_rest_alone() {
        let mut config = AppConfig::default();
        assert!(!config.voice.enabled);
        assert!(!config.desktop.enabled);
        let applied = overlay(
            &[
                ("voice.enabled".to_owned(), "true".to_owned()),
                ("desktop.retention".to_owned(), "keep_24h".to_owned()),
            ],
            &mut config,
        )
        .expect("a valid pair of rows applies");
        assert_eq!(applied, ["voice.enabled", "desktop.retention"]);
        assert!(config.voice.enabled);
        assert_eq!(config.desktop.retention, "keep_24h");
        assert!(!config.desktop.enabled);
    }

    #[test]
    fn an_invalid_row_refuses_the_whole_overlay_instead_of_applying_half() {
        let mut config = AppConfig::default();
        let before = (config.voice.enabled, config.desktop.retention.clone());
        let error = overlay(
            &[
                ("voice.enabled".to_owned(), "true".to_owned()),
                ("desktop.retention".to_owned(), "nonsense".to_owned()),
            ],
            &mut config,
        )
        .expect_err("a row this build cannot apply must not be applied silently");
        assert!(error.contains("desktop.retention"), "{error}");
        assert_eq!(
            (config.voice.enabled, config.desktop.retention.clone()),
            before,
            "the file's values must still be what a later read sees"
        );
    }

    #[test]
    fn enabling_desktop_control_while_keeping_captures_with_nowhere_to_put_them_is_refused() {
        let mut config = AppConfig::default();
        config.desktop.capture_dir = String::new();
        config.desktop.retention = "discard_after_task".to_owned();
        let error = overlay(
            &[
                ("desktop.enabled".to_owned(), "true".to_owned()),
                ("desktop.retention".to_owned(), "keep_24h".to_owned()),
            ],
            &mut config,
        )
        .expect_err("kept captures with no capture_dir is the case the config validator refuses");
        assert!(error.contains("invalid"), "{error}");
        assert!(
            !config.desktop.enabled,
            "and it left the file's value standing"
        );
    }

    #[test]
    fn an_empty_overlay_changes_nothing_and_names_no_keys() {
        let mut config = AppConfig::default();
        let before = config.clone();
        assert!(overlay(&[], &mut config)
            .expect("nothing to apply")
            .is_empty());
        assert_eq!(config.voice.enabled, before.voice.enabled);
    }
}
