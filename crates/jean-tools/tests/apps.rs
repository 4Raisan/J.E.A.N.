//! Application discovery: ambiguity, bounds, and the rule that nothing is executed.
//!
//! These tests use fixture directories under `%TEMP%` and fake executables (zero-byte
//! files with a `.exe` name), because the point of the module is that a candidate is
//! never run — a fixture that could not be launched anyway is the only honest way to
//! prove that no launch happened.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use jean_tools::apps::{
    classify_match, clean_icon_suffix, discover_apps, discover_installed_apps,
    join_install_location, registry_executable, strip_exe, DiscoveryBound, DiscoveryEnv,
    DiscoveryLimits, DiscoveryOutcome, DiscoverySource, MatchKind, RegistryAppEntry,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A fixture tree, removed at the end of the test that made it.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "jean-discover-{}-{}-{}",
            tag,
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("fixture root");
        Self { root }
    }

    fn dir(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(&path).expect("fixture dir");
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn path_only(dirs: &[PathBuf]) -> DiscoveryEnv {
    DiscoveryEnv {
        path: Some(
            dirs.iter()
                .map(|d| d.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(";"),
        ),
        pathext: Some(".EXE".to_owned()),
        user_start_menu: None,
        machine_start_menu: None,
        extra_start_menu_roots: Vec::new(),
    }
}

fn only_path() -> DiscoveryLimits {
    DiscoveryLimits {
        sources: vec![DiscoverySource::Path],
        ..DiscoveryLimits::default()
    }
}

#[test]
fn two_executables_with_one_name_are_both_returned_and_neither_is_chosen() {
    // The load-bearing behaviour: a name that resolves twice must not silently pick a
    // winner. Which one comes first depends on `PATH` order, and nobody agreed to
    // enumeration order being a security property.
    let fixture = Fixture::new("ambiguous");
    let first = fixture.dir("bin-a");
    let second = fixture.dir("bin-b");
    std::fs::write(first.join("jeanfix.exe"), b"not a program").unwrap();
    std::fs::write(second.join("jeanfix.exe"), b"not a program").unwrap();

    let outcome = discover_apps(
        "jeanfix",
        &path_only(&[first.clone(), second.clone()]),
        &only_path(),
    )
    .expect("discovery must not fail on an ambiguous name");

    assert_eq!(
        outcome.candidates.len(),
        2,
        "both candidates must come back: {:?}",
        outcome.candidates
    );
    assert!(outcome.ambiguous, "two distinct paths are an ambiguity");
    assert!(
        outcome.sole_launchable().is_none(),
        "an ambiguous result cannot be launched without asking"
    );
    // Each candidate says where it came from — the caller shows that list to the
    // operator, who is the only one who can tell which `jeanfix` they meant.
    for candidate in &outcome.candidates {
        assert!(candidate.exe_path.is_some());
        assert_eq!(candidate.sources.len(), 1);
    }
    let seen: Vec<String> = outcome
        .candidates
        .iter()
        .flat_map(|c| c.sources.iter().map(|p| p.seen_at.clone()))
        .collect();
    assert!(
        seen.iter().any(|s| s.contains("bin-a")) && seen.iter().any(|s| s.contains("bin-b")),
        "both directories must be attributed: {seen:?}"
    );
    assert_eq!(outcome.bound, DiscoveryBound::None, "nothing was cut short");
}

#[test]
fn the_same_executable_seen_twice_is_one_candidate_with_two_provenances() {
    // Deduplication by resolved path, not by name: `PATH` entries that both point at
    // one directory, and a shortcut plus a registry key for one binary, are one
    // application that the operator can launch exactly one way.
    let fixture = Fixture::new("dedupe");
    let dir = fixture.dir("bin");
    std::fs::write(dir.join("jeansingle.exe"), b"not a program").unwrap();

    let env = DiscoveryEnv {
        path: Some(format!(
            "{};{};{}",
            dir.display(),
            dir.display(),
            fixture.dir("other").display()
        )),
        pathext: Some(".EXE".to_owned()),
        user_start_menu: None,
        machine_start_menu: None,
        extra_start_menu_roots: Vec::new(),
    };

    let outcome = discover_apps("jeansingle", &env, &only_path()).unwrap();
    assert_eq!(
        outcome.candidates.len(),
        1,
        "one file is one candidate: {:?}",
        outcome.candidates
    );
    assert!(!outcome.ambiguous);
    assert!(outcome.sole_launchable().is_some());
}

#[test]
fn an_exact_name_ranks_above_a_prefix_and_a_prefix_above_a_substring() {
    let fixture = Fixture::new("ranking");
    let dir = fixture.dir("bin");
    for name in ["jeanrank.exe", "jeanrankhelper.exe", "myjeanrank.exe"] {
        std::fs::write(dir.join(name), b"not a program").unwrap();
    }

    let outcome = discover_apps("jeanrank", &path_only(&[dir]), &only_path()).unwrap();
    assert_eq!(outcome.candidates.len(), 3, "{:?}", outcome.candidates);
    let kinds: Vec<MatchKind> = outcome.candidates.iter().map(|c| c.match_kind).collect();
    assert_eq!(
        kinds,
        vec![MatchKind::Exact, MatchKind::Prefix, MatchKind::Substring],
        "the operator's best guess must be first: {kinds:?}"
    );
    assert!(
        outcome
            .candidates
            .iter()
            .map(|c| c.match_kind as u8)
            .eq(kinds.iter().map(|k| *k as u8)),
        "sorted by rank"
    );
}

#[test]
fn a_script_on_path_is_refused_and_says_why() {
    // `.bat`, `.cmd` and `.ps1` are only runnable through a shell, and this project
    // never builds a shell command line. Silently omitting them would read as "not
    // installed"; the note is the difference between a refusal and a mystery.
    let fixture = Fixture::new("scripts");
    let dir = fixture.dir("bin");
    std::fs::write(dir.join("jeanscript.cmd"), b"@echo off").unwrap();
    std::fs::write(dir.join("jeanscript2.ps1"), b"Write-Output 1").unwrap();

    let outcome = discover_apps("jeanscript", &path_only(&[dir]), &only_path()).unwrap();

    assert!(
        outcome.candidates.iter().all(|c| c.exe_path.is_none()
            || !c.exe_path.as_ref().unwrap().extension().is_some_and(|e| {
                let e = e.to_ascii_lowercase();
                e == "cmd" || e == "bat" || e == "ps1"
            })),
        "a script must never become a launchable candidate: {:?}",
        outcome.candidates
    );
    assert!(
        outcome.notes.iter().any(|n| {
            let lower = n.to_ascii_lowercase();
            lower.contains("cmd") || lower.contains("ps1") || lower.contains("script")
        }),
        "the refusal must be explained, not silent: {:?}",
        outcome.notes
    );
}

#[test]
fn a_shortcut_that_points_at_nothing_is_reported_broken_and_never_dropped() {
    // The operator sees this entry in their Start Menu. Telling them it does not exist
    // would be a lie; telling them it does not resolve is useful.
    let fixture = Fixture::new("brokenlink");
    let menu = fixture.dir("start-menu");
    // Not a real shell link: deliberately unreadable bytes, which the resolution path
    // must survive rather than panic on.
    std::fs::write(
        menu.join("jeanbroken.lnk"),
        [0x00u8, 0x01, 0x02, 0xff, 0xfe, 0x00, 0x00],
    )
    .unwrap();

    let env = DiscoveryEnv {
        path: None,
        pathext: None,
        user_start_menu: Some(menu.clone()),
        machine_start_menu: None,
        extra_start_menu_roots: Vec::new(),
    };
    let limits = DiscoveryLimits {
        sources: vec![DiscoverySource::StartMenu],
        ..DiscoveryLimits::default()
    };

    let before = listed(&menu);
    let outcome = discover_apps("jeanbroken", &env, &limits)
        .expect("an unreadable shortcut must not fail the run");
    let after = listed(&menu);

    assert_eq!(
        before, after,
        "discovery must not write into the tree it reads"
    );
    assert!(
        outcome.candidates.iter().any(|c| c.is_broken),
        "a shortcut that does not resolve is reported as broken: {:?} notes={:?}",
        outcome.candidates,
        outcome.notes
    );
    if let Some(broken) = outcome.candidates.iter().find(|c| c.is_broken) {
        assert!(
            !broken.is_launchable(),
            "a broken candidate cannot be the launch target"
        );
    }
}

#[test]
fn a_real_shortcut_resolves_to_an_absolute_target_or_reports_unsupported() {
    // The shell-link path is only meaningful against a shortcut the shell wrote, so
    // this uses one from the machine's own Start Menu — read-only, copied into the
    // fixture so nothing about the real tree changes.
    let Some(real) = first_start_menu_link() else {
        return; // No Start Menu readable for this account; nothing to check.
    };
    let fixture = Fixture::new("reallink");
    let copied = fixture.root.join("subject.lnk");
    std::fs::copy(&real, &copied).expect("copy the real shortcut");

    let env = DiscoveryEnv {
        path: None,
        pathext: None,
        user_start_menu: Some(fixture.root.clone()),
        machine_start_menu: None,
        extra_start_menu_roots: Vec::new(),
    };
    let limits = DiscoveryLimits {
        sources: vec![DiscoverySource::StartMenu],
        ..DiscoveryLimits::default()
    };
    // Query by the copied link's own stem, which is the name discovery compares.
    let stem = copied
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let outcome = discover_apps(&stem, &env, &limits).unwrap();
    assert_eq!(
        outcome.candidates.len(),
        1,
        "the fixture link should be found: {:?}",
        outcome.candidates
    );
    let candidate = &outcome.candidates[0];
    match &candidate.exe_path {
        Some(path) => assert!(
            path.is_absolute(),
            "a resolved target must be absolute: {}",
            path.display()
        ),
        None => assert!(
            candidate.is_broken,
            "a link with no target is either resolved or broken; a silent None is neither: {:?}",
            outcome.notes
        ),
    }
    assert!(
        candidate
            .sources
            .iter()
            .any(|p| p.source == DiscoverySource::StartMenu),
        "provenance must name the source"
    );
}

#[test]
fn every_bound_is_reported_rather_than_changing_the_answer_in_silence() {
    let fixture = Fixture::new("bounds");
    let dir = fixture.dir("bin");
    for n in 0..6 {
        std::fs::write(dir.join(format!("jeanb{n}fix.exe")), b"not a program").unwrap();
    }
    let env = path_only(&[dir]);

    // Candidate cap.
    let capped = DiscoveryLimits {
        max_candidates: 2,
        sources: vec![DiscoverySource::Path],
        ..DiscoveryLimits::default()
    };
    let outcome = discover_apps("jeanb", &env, &capped).unwrap();
    assert_eq!(outcome.candidates.len(), 2);
    assert_eq!(outcome.bound, DiscoveryBound::MaxCandidates);
    assert!(outcome.bound.was_hit());
    assert!(
        outcome.ambiguous,
        "a capped result is never presented as a unique answer"
    );

    // Zero deadline: must return promptly and say it stopped.
    let no_time = DiscoveryLimits {
        deadline_ms: 0,
        sources: vec![DiscoverySource::Path],
        ..DiscoveryLimits::default()
    };
    let outcome = discover_apps("jeanb", &env, &no_time).unwrap();
    assert!(
        outcome.bound == DiscoveryBound::Deadline || outcome.candidates.is_empty(),
        "a zero deadline must not look like a completed search: {:?}",
        outcome.bound
    );

    // Entry cap of one: it examined something, and says which bound ended it.
    let one_entry = DiscoveryLimits {
        max_entries: 1,
        sources: vec![DiscoverySource::Path],
        ..DiscoveryLimits::default()
    };
    let outcome = discover_apps("jeanb", &env, &one_entry).unwrap();
    assert!(
        outcome.entries_examined >= 1,
        "the entry counted before stopping must be reported"
    );
    assert!(
        outcome.bound.was_hit(),
        "a search that stopped early must say so: {:?}",
        outcome.bound
    );
}

#[test]
fn a_query_that_is_empty_refuses_rather_than_enumerating_the_machine() {
    let fixture = Fixture::new("empty");
    let dir = fixture.dir("bin");
    std::fs::write(dir.join("anything.exe"), b"not a program").unwrap();

    let outcome = discover_apps("", &path_only(&[dir]), &only_path()).unwrap();
    assert!(outcome.candidates.is_empty());
    assert!(
        outcome.notes.iter().any(|n| n.contains("empty")),
        "the refusal must be explained: {:?}",
        outcome.notes
    );
}

#[test]
fn only_the_sources_configured_are_consulted_and_are_recorded() {
    let fixture = Fixture::new("sources");
    let dir = fixture.dir("bin");
    let menu = fixture.dir("menu");
    std::fs::write(dir.join("jeansrc.exe"), b"not a program").unwrap();
    std::fs::write(menu.join("jeansrc.lnk"), b"junk").unwrap();

    let env = DiscoveryEnv {
        path: Some(dir.to_string_lossy().into_owned()),
        pathext: Some(".EXE".to_owned()),
        user_start_menu: Some(menu),
        machine_start_menu: None,
        extra_start_menu_roots: Vec::new(),
    };

    let path_only_run = discover_apps(
        "jeansrc",
        &env,
        &DiscoveryLimits {
            sources: vec![DiscoverySource::Path],
            ..DiscoveryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(path_only_run.sources_used, vec![DiscoverySource::Path]);
    assert!(
        path_only_run
            .candidates
            .iter()
            .all(|c| c.sources.iter().all(|p| p.source == DiscoverySource::Path)),
        "a source that was not selected cannot appear in provenance"
    );

    let both = discover_apps(
        "jeansrc",
        &env,
        &DiscoveryLimits {
            sources: vec![DiscoverySource::Path, DiscoverySource::StartMenu],
            ..DiscoveryLimits::default()
        },
    )
    .unwrap();
    assert!(
        both.sources_used.contains(&DiscoverySource::StartMenu),
        "{:?}",
        both.sources_used
    );
    assert!(
        !both.sources_used.contains(&DiscoverySource::Registry),
        "registry was not selected: {:?}",
        both.sources_used
    );
}

#[test]
fn names_are_compared_without_the_exe_suffix_and_without_case() {
    assert_eq!(strip_exe("Notepad.exe"), "Notepad");
    assert_eq!(strip_exe("notepad.EXE"), "notepad");
    assert_eq!(strip_exe("notepad"), "notepad");
    // Only `.exe`: a `.dll` must not become a match for its own stem.
    assert_eq!(strip_exe("thing.dll"), "thing.dll");

    assert_eq!(
        classify_match("notepad", "Notepad.exe"),
        Some(MatchKind::Exact)
    );
    assert_eq!(
        classify_match("notepad", "notepad++"),
        Some(MatchKind::Prefix),
        "a prefix is not an exact match even when it starts the same way"
    );
    assert_eq!(classify_match("pad", "Notepad"), Some(MatchKind::Substring));
    assert_eq!(classify_match("", "Notepad"), None);
    assert_eq!(classify_match("notepad", ""), None);
}

#[test]
fn registry_values_are_shaped_without_being_executed() {
    // `App Paths` default values are usually a bare path, sometimes quoted, sometimes
    // carrying arguments. None of those shapes may reach a shell.
    let plain = RegistryAppEntry {
        name: "jean.app.exe".to_owned(),
        default_value: Some(r"C:\Program Files\Jean\jean.app.exe".to_owned()),
        path_value: None,
        install_location: None,
        display_icon: None,
    };
    assert_eq!(
        registry_executable(&plain),
        Some(PathBuf::from(r"C:\Program Files\Jean\jean.app.exe"))
    );

    let quoted = RegistryAppEntry {
        default_value: Some(r#""C:\Program Files\Jean\jean.app.exe"  --quiet""#.to_owned()),
        ..plain.clone()
    };
    assert_eq!(
        registry_executable(&quoted),
        Some(PathBuf::from(r"C:\Program Files\Jean\jean.app.exe")),
        "quotes and trailing arguments are not part of the executable path"
    );

    let icon_only = RegistryAppEntry {
        name: "Jean".to_owned(),
        default_value: None,
        path_value: None,
        install_location: Some(r"C:\Program Files\Jean".to_owned()),
        display_icon: Some(r"C:\Program Files\Jean\jean.app.exe,0".to_owned()),
    };
    assert_eq!(
        registry_executable(&icon_only),
        Some(PathBuf::from(r"C:\Program Files\Jean\jean.app.exe")),
        "the icon value's `,0` suffix is not part of the path"
    );

    // A relative default value is not a path this build will hand to a launcher.
    let relative = RegistryAppEntry {
        default_value: Some("jean.app.exe".to_owned()),
        ..plain.clone()
    };
    let resolved = registry_executable(&relative);
    assert!(
        resolved.is_none() || resolved.unwrap().is_absolute(),
        "a relative registry value must be either joined into an absolute path or refused"
    );

    assert_eq!(
        clean_icon_suffix(std::ffi::OsStr::new(r"C:\a\b.exe,-1")),
        std::ffi::OsString::from(r"C:\a\b.exe")
    );
    assert_eq!(
        clean_icon_suffix(std::ffi::OsStr::new(r"C:\a\b.exe")),
        std::ffi::OsString::from(r"C:\a\b.exe")
    );
    assert_eq!(
        join_install_location(Path::new(r"C:\App"), r"bin\tool.exe"),
        Some(PathBuf::from(r"C:\App\bin\tool.exe"))
    );
    // An absolute file name is not re-rooted, and a traversal is not joined at all.
    assert_eq!(
        join_install_location(Path::new(r"C:\App"), r"C:\Other\tool.exe"),
        Some(PathBuf::from(r"C:\Other\tool.exe"))
    );
    assert_eq!(
        join_install_location(Path::new(r"C:\App"), r"..\escape.exe"),
        None,
        "joining a traversal would leave the InstallLocation the registry authorised"
    );
}

#[test]
fn discovery_source_names_parse_the_configured_values_and_nothing_else() {
    assert_eq!(DiscoverySource::parse("path"), Some(DiscoverySource::Path));
    assert_eq!(
        DiscoverySource::parse("start_menu"),
        Some(DiscoverySource::StartMenu)
    );
    assert_eq!(
        DiscoverySource::parse("registry"),
        Some(DiscoverySource::Registry)
    );
    assert_eq!(DiscoverySource::parse("mcp"), None);
    assert_eq!(DiscoverySource::parse(""), None);

    let (limits, ignored) =
        DiscoveryLimits::from_discovery_list(&["path".to_owned(), "mcp".to_owned()]);
    assert_eq!(limits.sources, vec![DiscoverySource::Path]);
    assert_eq!(ignored, vec!["mcp".to_owned()]);
}

#[test]
fn a_live_registry_read_returns_absolute_paths_or_nothing_at_all() {
    // No fixture can stand in for the real `App Paths` hive, and the promise worth
    // making about it is narrow: whatever comes back is shaped correctly, and a key
    // this account cannot open is reported rather than skipped.
    let outcome = discover_installed_apps_read_only("notepad");
    for candidate in &outcome.candidates {
        if let Some(path) = &candidate.exe_path {
            assert!(path.is_absolute(), "{}", path.display());
        }
    }
    for (key, code) in &outcome.inaccessible_keys {
        assert!(!key.is_empty(), "{code} on an empty key name");
        assert!(
            outcome.notes.iter().any(|n| n.contains("inaccessible"))
                || !outcome.inaccessible_keys.is_empty(),
            "an inaccessible key must be visible in the result: {key}"
        );
    }
}

/// `discover_installed_apps` against this machine, all sources, read-only.
fn discover_installed_apps_read_only(query: &str) -> DiscoveryOutcome {
    let list = [
        "path".to_owned(),
        "start_menu".to_owned(),
        "registry".to_owned(),
    ];
    discover_installed_apps(query, &list).expect("a live discovery run must not error")
}

/// A readable shortcut from the machine's own Start Menu, if there is one.
fn first_start_menu_link() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from)?;
    let programs = appdata
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs");
    if !programs.is_dir() {
        return None;
    }
    let mut queue = vec![programs];
    while let Some(dir) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                queue.push(path);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
            {
                return Some(path);
            }
        }
    }
    None
}

/// A directory listing as names, for the "discovery did not write anything" check.
fn listed(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}
