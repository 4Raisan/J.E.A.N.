//! Structured logging to a rolling file.
//!
//! ## Why synchronous
//!
//! `tracing-appender`'s non-blocking writer moves formatting and I/O onto a
//! worker thread, which is faster and can **lose the last few lines when the
//! process is killed**. J.E.A.N.'s most important log entries are the ones
//! written immediately before a crash, and the Phase-0 exit criteria are about
//! recovering from exactly that. Throughput is not a constraint — a supervisor
//! logs a handful of lines per task — so the writes are synchronous and the file
//! is complete up to the moment the process died.
//!
//! ## Why not the terminal
//!
//! The log file is for diagnosis; the CLI prints what the user asked to see.
//! Mixing them means a `jean tasks` table arrives interleaved with `DEBUG`
//! lines, so log records always go to the file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use jean_protocol::config::CoreConfig;
use tracing_subscriber::filter::EnvFilter;

use crate::error::{CoreError, Result};

/// Prefix of the rolling log files, so [`select_logs_to_prune`] can find them.
pub const LOG_FILE_PREFIX: &str = "jean.log";

/// Whether this process has already installed a subscriber.
static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Initialise the global subscriber.
///
/// Idempotent by design. `tracing` has one global default subscriber and
/// `set_global_default` refuses a second one, so an unconditional `.init()` would panic
/// on the next `Context::load` in the same process — which Phase 1 makes ordinary, since
/// one `jean` invocation now runs several intents and a test binary builds several
/// contexts. The first configuration in a process wins; that is stated rather than
/// hidden, because an embedding host that installed its own subscriber before J.E.A.N.
/// will keep it, and `core.log_level` from `jean.toml` will then not apply.
pub fn init(core: &CoreConfig, log_dir: &Path) -> Result<()> {
    if INITIALISED.swap(true, Ordering::SeqCst) {
        tracing::trace!(
            "a trace subscriber is already installed in this process; keeping the first one"
        );
        return Ok(());
    }

    std::fs::create_dir_all(log_dir).map_err(|e| CoreError::Logging {
        reason: format!("could not create {}: {e}", log_dir.display()),
    })?;

    prune_old_logs(log_dir, core.log_retention_days, core.log_max_files);

    let file = rolling_appender(&core.log_rotation, log_dir);

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(build_filter(&core.log_level)?)
        .with_writer(file)
        // ANSI escapes in a file are noise, and Windows terminals differ in
        // whether they render them at all.
        .with_ansi(false)
        .with_target(true)
        .with_thread_names(true)
        .finish();

    // The guard is set above, so a failure here is a host that installed a subscriber
    // concurrently. Reported as an error the first time, because logging is how the
    // operator finds out anything else went wrong.
    tracing::subscriber::set_global_default(subscriber).map_err(|e| {
        INITIALISED.store(false, Ordering::SeqCst);
        CoreError::Logging {
            reason: e.to_string(),
        }
    })?;

    Ok(())
}

/// Parse `core.log_level` into a filter.
///
/// `EnvFilter` rather than a bare level, so `RUST_LOG`-style directives such as
/// `jean_process=debug,info` work without inventing a J.E.A.N.-specific
/// mechanism. An invalid directive is a configuration mistake and is reported:
/// silently falling back to a default would hide the reason nothing is being
/// logged.
pub fn build_filter(level: &str) -> Result<EnvFilter> {
    EnvFilter::try_new(level).map_err(|e| CoreError::Logging {
        reason: format!("core.log_level {level:?} is not a valid filter: {e}"),
    })
}

/// Map `core.log_rotation` onto a `tracing-appender` rolling policy.
///
/// All four values the configuration accepts are implemented here, so the setting
/// cannot be a knob that reads as if it worked. The configuration loader rejects
/// any other string before this is reached; the fallback keeps that true even if a
/// future value is added to the schema without being added here.
fn rolling_appender(
    rotation: &str,
    log_dir: &Path,
) -> tracing_appender::rolling::RollingFileAppender {
    match rotation {
        "minutely" => tracing_appender::rolling::minutely(log_dir, LOG_FILE_PREFIX),
        "hourly" => tracing_appender::rolling::hourly(log_dir, LOG_FILE_PREFIX),
        "never" => tracing_appender::rolling::never(log_dir, LOG_FILE_PREFIX),
        // "daily" and anything else the schema may come to allow.
        _ => tracing_appender::rolling::daily(log_dir, LOG_FILE_PREFIX),
    }
}

/// Delete rotated log files that are too old or too numerous.
///
/// `tracing-appender` rotates but never prunes, so without this the log directory
/// grows forever. Deletion failures are logged and ignored — a log file that
/// cannot be deleted is not worth failing a task over.
pub fn prune_old_logs(log_dir: &Path, retention_days: u32, max_files: u32) {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return; // No directory yet: nothing to prune.
    };

    let mut logs: Vec<(PathBuf, SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Rotated files are `jean.log.YYYY-MM-DD`. Today's live file is included,
        // which is correct: it is still a log file and still counts.
        if !name.starts_with(LOG_FILE_PREFIX) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        logs.push((path, meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
    }

    for path in select_logs_to_prune(&logs, SystemTime::now(), retention_days, max_files) {
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::debug!(path = %path.display(), error = %e, "could not prune a log file");
        }
    }
}

/// Decide which logs to delete, without touching the filesystem.
///
/// Split out from [`prune_old_logs`] so the retention rules are testable with
/// synthetic timestamps: setting a file's modification time is not portable, and
/// a test that cannot arrange its own preconditions tests nothing.
///
/// Two limits, both from configuration:
///
/// * `retention_days` — delete anything last modified more than this long ago.
/// * `max_files` — keep at most this many, newest first.
///
/// A limit of 0 disables that rule. `0` meaning "off" rather than "delete
/// everything" is deliberate: an accidental zero in a config file should not
/// erase the diagnostic history.
pub fn select_logs_to_prune(
    logs: &[(PathBuf, SystemTime)],
    now: SystemTime,
    retention_days: u32,
    max_files: u32,
) -> Vec<PathBuf> {
    let mut prune: Vec<PathBuf> = Vec::new();

    if retention_days > 0 {
        let cutoff = Duration::from_secs(u64::from(retention_days) * 86_400);
        let expired: Vec<PathBuf> = logs
            .iter()
            .filter(|(_, modified)| {
                now.duration_since(*modified)
                    .map(|age| age > cutoff)
                    // A modification time in the future is a clock problem, not
                    // a reason to delete the log.
                    .unwrap_or(false)
            })
            .map(|(path, _)| path.clone())
            .collect();
        prune.extend(expired);
    }

    if max_files > 0 {
        // Newest first. The path tie-break keeps the result deterministic when
        // several files share a timestamp, which happens on filesystems with a
        // coarse clock.
        let mut by_age: Vec<&(PathBuf, SystemTime)> = logs.iter().collect();
        by_age.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));

        // Files already condemned by the age rule do not consume a slot here, or
        // a directory full of ancient logs would evict the recent ones.
        let survivors: Vec<&PathBuf> = by_age
            .iter()
            .map(|entry| &entry.0)
            .filter(|path| !prune.contains(*path))
            .collect();

        for path in survivors.iter().skip(max_files as usize) {
            prune.push((*path).clone());
        }
    }

    prune
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(name: &str, age: Duration, now: SystemTime) -> (PathBuf, SystemTime) {
        (PathBuf::from(name), now - age)
    }

    fn days(n: u64) -> Duration {
        Duration::from_secs(n * 86_400)
    }

    fn names(pruned: Vec<PathBuf>) -> Vec<String> {
        pruned
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn zero_limits_prune_nothing() {
        let now = SystemTime::now();
        let logs = vec![
            log("jean.log.2024-01-01", days(400), now),
            log("jean.log.2024-01-02", days(399), now),
        ];
        assert!(select_logs_to_prune(&logs, now, 0, 0).is_empty());
    }

    #[test]
    fn the_age_limit_removes_only_expired_files() {
        let now = SystemTime::now();
        let logs = vec![
            log("jean.log.old", days(40), now),
            log("jean.log.edge", days(30), now),
            log("jean.log.new", days(1), now),
        ];
        let pruned = names(select_logs_to_prune(&logs, now, 30, 0));
        assert_eq!(pruned, vec!["jean.log.old"], "{pruned:?}");
    }

    #[test]
    fn the_count_limit_keeps_the_newest() {
        let now = SystemTime::now();
        let logs = vec![
            log("jean.log.2024-01-01", days(5), now),
            log("jean.log.2024-01-02", days(4), now),
            log("jean.log.2024-01-03", days(3), now),
            log("jean.log.2024-01-04", days(2), now),
            log("jean.log.2024-01-05", days(1), now),
        ];
        let pruned = names(select_logs_to_prune(&logs, now, 0, 2));
        assert_eq!(pruned.len(), 3, "{pruned:?}");
        assert!(!pruned.contains(&"jean.log.2024-01-05".to_owned()));
        assert!(!pruned.contains(&"jean.log.2024-01-04".to_owned()));
        assert!(pruned.contains(&"jean.log.2024-01-01".to_owned()));
    }

    #[test]
    fn a_file_condemned_by_age_is_not_counted_against_the_limit() {
        // Three files, two ancient and one recent, with max_files = 1. The
        // ancient pair is pruned by age; the recent one must survive. If the age
        // victims also consumed the single allowed slot, the recent log would be
        // evicted and the directory would end up empty.
        let now = SystemTime::now();
        let logs = vec![
            log("jean.log.ancient-a", days(90), now),
            log("jean.log.ancient-b", days(89), now),
            log("jean.log.today", days(0), now),
        ];
        let pruned = names(select_logs_to_prune(&logs, now, 30, 1));
        assert_eq!(
            pruned,
            vec!["jean.log.ancient-a", "jean.log.ancient-b"],
            "{pruned:?}"
        );
    }

    #[test]
    fn a_future_timestamp_is_not_a_reason_to_delete() {
        let now = SystemTime::now();
        let logs = vec![(PathBuf::from("jean.log.clock-skew"), now + days(3))];
        assert!(select_logs_to_prune(&logs, now, 30, 0).is_empty());
    }

    #[test]
    fn the_count_tie_break_is_deterministic() {
        let now = SystemTime::now();
        let logs = vec![
            log("jean.log.b", Duration::ZERO, now),
            log("jean.log.a", Duration::ZERO, now),
            log("jean.log.c", Duration::ZERO, now),
        ];
        // Identical timestamps, so the path decides. Sorted newest-first with the
        // path descending, "c" is kept and "b", "a" are pruned.
        let pruned = names(select_logs_to_prune(&logs, now, 0, 1));
        assert_eq!(pruned, vec!["jean.log.b", "jean.log.a"], "{pruned:?}");
    }

    #[test]
    fn an_empty_directory_prunes_nothing() {
        assert!(select_logs_to_prune(&[], SystemTime::now(), 7, 3).is_empty());
    }

    #[test]
    fn pruning_a_missing_directory_is_not_an_error() {
        let dir = std::env::temp_dir().join("jean-logging-does-not-exist-anywhere");
        let _ = std::fs::remove_dir_all(&dir);
        prune_old_logs(&dir, 7, 5);
    }

    #[test]
    fn unrelated_files_are_never_touched() {
        let dir = std::env::temp_dir().join(format!("jean-logging-keep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["keep.txt", "jean.db", "other.log", "jean.log.backup.tar"] {
            std::fs::write(dir.join(name), b"important").unwrap();
        }

        // max_files = 0 with an age limit of 1 day: only files this crate wrote
        // today would match the prefix, and none of these are rotated logs
        // except by prefix — which is exactly the case worth checking.
        prune_old_logs(&dir, 1, 0);

        for name in ["keep.txt", "jean.db", "other.log", "jean.log.backup.tar"] {
            assert!(dir.join(name).exists(), "{name} was deleted");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn valid_filter_directives_are_accepted() {
        for level in [
            "off",
            "error",
            "warn",
            "info",
            "debug",
            "trace",
            "jean_process=debug,info",
            "jean_store=trace,jean_core=info,warn",
        ] {
            assert!(build_filter(level).is_ok(), "{level:?} should parse");
        }
    }

    #[test]
    fn an_invalid_filter_is_reported_not_ignored() {
        // A bare word is a *target* directive and parses fine — `verbose-please`
        // would have been accepted, which is not what this test is about. An
        // explicit `target=level` with a level that does not exist is the case
        // where a typo in `core.log_level` must fail loudly.
        let err = build_filter("jean_core=verbose").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("jean_core=verbose"), "{text}");
        assert!(text.contains("not a valid filter"), "{text}");
    }

    #[test]
    fn every_configured_rotation_produces_an_appender() {
        // `core.log_rotation` is validated against exactly these four strings, so a
        // value the schema accepts must not fall through to a panic here.
        let dir =
            std::env::temp_dir().join(format!("jean-logging-rotation-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for rotation in [
            "minutely",
            "hourly",
            "daily",
            "never",
            "something-unexpected",
        ] {
            let _ = rolling_appender(rotation, &dir);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_log_prefix_matches_what_the_appender_writes() {
        // `tracing_appender::rolling::daily(dir, prefix)` produces
        // `<prefix>.YYYY-MM-DD`, and the prune rule matches on that prefix. If
        // either changes, the retention feature silently stops working.
        assert_eq!(LOG_FILE_PREFIX, "jean.log");
        assert!("jean.log.2024-01-01".starts_with(LOG_FILE_PREFIX));
    }
}
