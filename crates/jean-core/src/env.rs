//! Building the environment a child process is allowed to see.
//!
//! ## The rule
//!
//! A child gets **only** what the allowlist names. It never inherits the
//! supervisor's environment wholesale. `process.inherit_environment` exists in
//! the schema and is rejected by [`jean_protocol::AppConfig::validate`], because
//! blanket inheritance would hand every tool every credential that happens to be
//! sitting in the ambient environment — and on a developer machine that list
//! routinely includes cloud tokens, npm auth and database passwords.
//!
//! ## Case
//!
//! Windows environment variable names are case-insensitive, so the allowlist is
//! matched case-insensitively. The child receives the spelling from the
//! allowlist, not from the parent environment, so two variables differing only in
//! case cannot both slip through.

use std::collections::{BTreeMap, BTreeSet};

/// Variables a tool asked for that were not on the allowlist.
///
/// Reported rather than silently dropped: a tool that cannot see `JAVA_HOME`
/// fails in a confusing way, and "we removed it" is the explanation the operator
/// needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvReport {
    /// Allowlisted variables copied from the supervisor's environment.
    pub inherited: Vec<String>,
    /// Allowlisted variables the tool set explicitly.
    pub from_tool: Vec<String>,
    /// Variables the tool asked for that are not allowlisted, and were dropped.
    pub rejected: Vec<String>,
    /// Allowlisted names that were not present in the supervisor's environment
    /// and were not set by the tool, so the child will not have them.
    pub absent: Vec<String>,
}

impl EnvReport {
    pub fn is_empty(&self) -> bool {
        self.rejected.is_empty() && self.absent.is_empty()
    }

    /// A one-line summary for the log, naming no values.
    pub fn summary(&self) -> String {
        format!(
            "{} inherited, {} from the tool, {} rejected, {} absent",
            self.inherited.len(),
            self.from_tool.len(),
            self.rejected.len(),
            self.absent.len()
        )
    }
}

/// Build the child's environment.
///
/// `parent` is the supervisor's environment. Passing it in, rather than calling
/// `std::env::vars()` inside, is what makes this function testable and keeps the
/// source of truth in one place.
pub fn build_child_env(
    allowlist: &[String],
    tool_env: &BTreeMap<String, String>,
    parent: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, EnvReport) {
    // Index the parent environment case-insensitively. If the parent somehow
    // holds two spellings of the same name, the first in sorted order wins,
    // which is deterministic and therefore reproducible.
    let mut parent_by_upper: BTreeMap<String, &String> = BTreeMap::new();
    for (key, value) in parent {
        parent_by_upper
            .entry(key.to_ascii_uppercase())
            .or_insert(value);
    }

    let mut child: BTreeMap<String, String> = BTreeMap::new();
    let mut report = EnvReport::default();
    // Names already handled, so an allowlist containing both `Path` and `PATH`
    // cannot put two spellings of one variable into the child.
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for name in allowlist {
        let upper = name.to_ascii_uppercase();
        if !seen.insert(upper.clone()) {
            continue;
        }
        match parent_by_upper.get(&upper) {
            Some(value) => {
                child.insert(name.clone(), (*value).clone());
                report.inherited.push(name.clone());
            }
            None => report.absent.push(name.clone()),
        }
    }

    for (key, value) in tool_env {
        let upper = key.to_ascii_uppercase();
        // The canonical name is the allowlist's spelling, not the tool's, so the
        // child sees exactly one casing per variable.
        let Some(canonical) = allowlist.iter().find(|n| n.to_ascii_uppercase() == upper) else {
            report.rejected.push(key.clone());
            continue;
        };
        child.retain(|existing, _| existing.to_ascii_uppercase() != upper);
        child.insert(canonical.clone(), value.clone());

        // A variable the tool set was not inherited, and is not absent.
        report.inherited.retain(|n| n != canonical);
        report.absent.retain(|n| n != canonical);
        if !report.from_tool.contains(canonical) {
            report.from_tool.push(canonical.clone());
        }
    }

    report.inherited.sort();
    report.from_tool.sort();
    report.rejected.sort();
    report.absent.sort();
    (child, report)
}

/// The names on the allowlist, for logging. Values are never logged: an
/// allowlisted variable can legitimately hold a token, and the redactor is a
/// pattern filter with a documented boundary, not a guarantee.
pub fn allowlist_names(allowlist: &[String]) -> String {
    allowlist.join(", ")
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

    fn allow(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn only_allowlisted_variables_reach_the_child() {
        let parent = map(&[
            ("PATH", r"C:\Windows\System32"),
            ("SystemRoot", r"C:\Windows"),
            ("AWS_SECRET_ACCESS_KEY", "super-secret"),
            ("GITHUB_TOKEN", "ghp_abcdefgh"),
        ]);
        let (child, report) =
            build_child_env(&allow(&["PATH", "SystemRoot"]), &BTreeMap::new(), &parent);

        assert_eq!(child.len(), 2);
        assert_eq!(
            child.get("PATH").map(String::as_str),
            Some(r"C:\Windows\System32")
        );
        assert!(!child.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!child.contains_key("GITHUB_TOKEN"));
        assert_eq!(report.inherited, vec!["PATH", "SystemRoot"]);
        assert!(report.rejected.is_empty());
    }

    #[test]
    fn an_empty_allowlist_produces_an_empty_environment() {
        let parent = map(&[("PATH", "x"), ("TEMP", "y")]);
        let (child, report) = build_child_env(&[], &BTreeMap::new(), &parent);
        assert!(child.is_empty());
        assert!(report.inherited.is_empty());
        assert_eq!(report.absent.len(), 0);
    }

    #[test]
    fn allowlisting_is_case_insensitive_but_the_spelling_is_canonical() {
        let parent = map(&[("path", r"C:\bin"), ("SystemRoot", r"C:\Windows")]);
        let (child, _) =
            build_child_env(&allow(&["PATH", "systemroot"]), &BTreeMap::new(), &parent);
        assert_eq!(child.len(), 2);
        assert!(child.contains_key("PATH"), "the allowlist spelling wins");
        assert!(child.contains_key("systemroot"));
        assert!(!child.contains_key("path"));
    }

    #[test]
    fn two_spellings_in_the_parent_cannot_both_reach_the_child() {
        let parent = map(&[("Path", "a"), ("PATH", "b")]);
        let (child, _) = build_child_env(&allow(&["PATH"]), &BTreeMap::new(), &parent);
        assert_eq!(child.len(), 1);
        assert!(child.contains_key("PATH"));
    }

    #[test]
    fn a_tool_can_set_an_allowlisted_variable() {
        let parent = map(&[("PATH", r"C:\Windows\System32")]);
        let tool = map(&[("PATH", r"C:\tools\bin;C:\Windows\System32")]);
        let (child, report) = build_child_env(&allow(&["PATH"]), &tool, &parent);
        assert_eq!(
            child.get("PATH").map(String::as_str),
            Some(r"C:\tools\bin;C:\Windows\System32"),
            "the tool's value must win"
        );
        assert_eq!(report.from_tool, vec!["PATH"]);
        assert!(
            !report.inherited.contains(&"PATH".to_owned()),
            "a variable the tool set is not also reported as inherited"
        );
    }

    #[test]
    fn a_tool_cannot_set_a_variable_that_is_not_allowlisted() {
        // This is the boundary that matters: a registry entry is configuration,
        // and configuration must not be able to widen the environment.
        let parent = map(&[("PATH", "x")]);
        let tool = map(&[("AWS_SECRET_ACCESS_KEY", "injected"), ("PATH", "y")]);
        let (child, report) = build_child_env(&allow(&["PATH"]), &tool, &parent);

        assert!(!child.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert_eq!(child.get("PATH").map(String::as_str), Some("y"));
        assert_eq!(report.rejected, vec!["AWS_SECRET_ACCESS_KEY"]);
        assert!(!report.is_empty());
    }

    #[test]
    fn a_tool_can_supply_a_variable_the_parent_does_not_have() {
        let parent = map(&[("PATH", "x")]);
        let tool = map(&[("PSModulePath", r"C:\Modules")]);
        let (child, report) = build_child_env(&allow(&["PATH", "PSModulePath"]), &tool, &parent);
        assert_eq!(
            child.get("PSModulePath").map(String::as_str),
            Some(r"C:\Modules")
        );
        assert_eq!(report.from_tool, vec!["PSModulePath"]);
        assert_eq!(report.absent, Vec::<String>::new());
    }

    #[test]
    fn an_allowlisted_variable_nobody_provides_is_reported_absent() {
        let parent = map(&[("PATH", "x")]);
        let (child, report) =
            build_child_env(&allow(&["PATH", "JAVA_HOME"]), &BTreeMap::new(), &parent);
        assert!(!child.contains_key("JAVA_HOME"));
        assert_eq!(report.absent, vec!["JAVA_HOME"]);
        assert!(!report.is_empty());
    }

    #[test]
    fn a_tool_variable_spelled_differently_still_replaces_the_inherited_one() {
        // `path` in the registry and `PATH` on the allowlist are the same
        // variable on Windows. Leaving both in the child would give it two values
        // and make the outcome depend on which one the child happens to read.
        let parent = map(&[("PATH", r"C:\Windows\System32")]);
        let tool = map(&[("path", r"C:\tools\bin")]);
        let (child, report) = build_child_env(&allow(&["PATH"]), &tool, &parent);

        assert_eq!(child.len(), 1, "{child:?}");
        assert_eq!(child.get("PATH").map(String::as_str), Some(r"C:\tools\bin"));
        assert!(!child.contains_key("path"));
        assert_eq!(report.from_tool, vec!["PATH"]);
        assert!(report.inherited.is_empty());
    }

    #[test]
    fn a_duplicate_allowlist_entry_cannot_produce_two_spellings() {
        let parent = map(&[("PATH", "p")]);
        let (child, report) =
            build_child_env(&allow(&["PATH", "path", "Path"]), &BTreeMap::new(), &parent);
        assert_eq!(child.len(), 1);
        assert_eq!(report.inherited, vec!["PATH"]);
        assert_eq!(report.absent, Vec::<String>::new());
    }

    #[test]
    fn the_report_names_variables_but_never_values() {
        let parent = map(&[("SECRET_TOKEN", "hunter2")]);
        let (_, report) = build_child_env(&allow(&["PATH"]), &BTreeMap::new(), &parent);
        let summary = report.summary();
        assert!(!summary.contains("hunter2"), "{summary}");
        assert!(!summary.contains("SECRET_TOKEN"), "{summary}");
        assert!(summary.contains("inherited"), "{summary}");
    }

    #[test]
    fn the_report_is_sorted_so_it_can_be_compared() {
        let parent = map(&[("ZZZ", "1"), ("AAA", "2"), ("MMM", "3")]);
        let (_, report) =
            build_child_env(&allow(&["ZZZ", "AAA", "MMM"]), &BTreeMap::new(), &parent);
        assert_eq!(report.inherited, vec!["AAA", "MMM", "ZZZ"]);
    }

    #[test]
    fn allowlist_names_are_joinable_for_a_log_line() {
        assert_eq!(allowlist_names(&allow(&["PATH", "TEMP"])), "PATH, TEMP");
        assert_eq!(allowlist_names(&[]), "");
    }

    #[test]
    fn the_child_environment_is_sorted_for_a_stable_block() {
        let parent = map(&[("TEMP", "t"), ("PATH", "p"), ("SystemRoot", "w")]);
        let (child, _) = build_child_env(
            &allow(&["SystemRoot", "TEMP", "PATH"]),
            &BTreeMap::new(),
            &parent,
        );
        let keys: Vec<&String> = child.keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "BTreeMap ordering must survive construction");
    }
}
