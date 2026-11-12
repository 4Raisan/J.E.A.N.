//! Ownership and cleanup for browser profiles beneath J.E.A.N.'s artifact directory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const LEASE: &str = "browser-profile.owner";
const MAGIC: &[u8] = b"JEAN disposable browser profile v1\n";
const REPARSE_POINT: u32 = 0x400;
const SWEEP_LIMIT: usize = 16;

pub(super) struct Disposable {
    dir: PathBuf,
    lease: Option<File>,
}

impl Disposable {
    pub(super) fn create(dir: &Path) -> io::Result<Self> {
        fs::create_dir(dir)?;
        let mut lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join(LEASE))?;
        // Publish the marker only after the exclusive lock is held, and
        // publish it before any browser can write to the profile.
        lease.try_lock().map_err(io::Error::other)?;
        lease.write_all(MAGIC)?;
        lease.sync_all()?;
        let owned = Self {
            dir: dir.to_path_buf(),
            lease: Some(lease),
        };
        fs::create_dir(dir.join("profile"))?;
        Ok(owned)
    }

    pub(super) fn cleanup(&mut self) -> io::Result<()> {
        self.finish(21)
    }

    fn finish(&mut self, attempts: usize) -> io::Result<()> {
        let Some(lease) = self.lease.take() else {
            return Ok(());
        };
        let result = remove(&self.dir, attempts);
        drop(lease);
        match &result {
            Ok(()) => {
                let _ = fs::remove_file(self.dir.join(LEASE));
            }
            Err(error) => tracing::warn!(profile = %self.dir.join("profile").display(), %error,
                "disposable browser profile cleanup deferred"),
        }
        result
    }
}

impl Drop for Disposable {
    fn drop(&mut self) {
        let _ = self.finish(1);
    }
}

fn plain(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes() & REPARSE_POINT == 0
}

fn remove(dir: &Path, attempts: usize) -> io::Result<()> {
    if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && plain(&m)) {
        return Err(io::Error::other("refusing a replaced artifact directory"));
    }
    let profile = dir.join("profile");
    for attempt in 1..=attempts {
        match fs::symlink_metadata(&profile) {
            Ok(m) if m.is_dir() && plain(&m) => {}
            Ok(_) => {
                return Err(io::Error::other(
                    "refusing a reparse point or non-directory profile",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        match fs::remove_dir_all(&profile) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempt < attempts && matches!(e.raw_os_error(), Some(32 | 33 | 145)) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("profile cleanup always makes at least one attempt")
}

fn abandoned(dir: &Path) -> Option<Disposable> {
    let marker = dir.join(LEASE);
    let metadata = fs::symlink_metadata(&marker).ok()?;
    if !metadata.is_file() || !plain(&metadata) {
        return None;
    }
    let mut lease = OpenOptions::new()
        .read(true)
        .write(true)
        .open(marker)
        .ok()?;
    lease.try_lock().ok()?;
    let mut contents = Vec::new();
    (&mut lease)
        .take(MAGIC.len() as u64 + 1)
        .read_to_end(&mut contents)
        .ok()?;
    (contents == MAGIC).then_some(Disposable {
        dir: dir.to_path_buf(),
        lease: Some(lease),
    })
}

pub(super) fn sweep(root: &Path, store: &jean_store::Store) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut attempted = 0;
    for entry in entries.flatten() {
        if attempted == SWEEP_LIMIT {
            break;
        }
        let name = entry.file_name();
        let Some((task, stamp)) = name
            .to_str()
            .and_then(|name| name.strip_prefix("browser-"))
            .and_then(|name| name.rsplit_once('-'))
        else {
            continue;
        };
        if stamp.parse::<u128>().is_err() || jean_protocol::ipc::validate_task_id(task).is_err() {
            continue;
        }
        let dir = entry.path();
        if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir() && plain(&m)) {
            continue;
        }
        if let Some(mut owned) = abandoned(&dir) {
            attempted += 1;
            let _ = owned.finish(1);
        } else if fs::symlink_metadata(dir.join(LEASE))
            .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
            && legacy_profile(&dir, task, store)
        {
            attempted += 1;
            if let Err(error) = remove(&dir, 1) {
                tracing::warn!(profile = %dir.join("profile").display(), %error,
                    "legacy disposable browser profile cleanup deferred");
            }
        }
    }
}

fn legacy_profile(dir: &Path, id: &str, store: &jean_store::Store) -> bool {
    let profile = dir.join("profile");
    if !fs::symlink_metadata(&profile).is_ok_and(|m| m.is_dir() && plain(&m)) {
        return false;
    }
    // Older builds did not write a lease. Require the exact recorded profile
    // and a settled run with no owner before touching one of those directories.
    store.task(id).ok().flatten().is_some_and(|task| {
        !task.state.is_in_flight()
            && task.runner_pid.is_none()
            && store.tool_runs(id).is_ok_and(|runs| {
                runs.iter().any(|run| {
                    run.tool_id == "browser.local"
                        && run.ended_at_ms.is_some()
                        && run
                            .working_dir
                            .as_deref()
                            .is_some_and(|path| Path::new(path) == profile)
                })
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    #[test]
    fn browser_profile_sweep_retries_locks_and_preserves_active_and_unmarked_profiles() {
        let id = jean_protocol::ipc::new_task_id();
        let root = std::env::temp_dir().join(format!("jean-profile-sweep-{id}"));
        fs::create_dir(&root).unwrap();
        let store = jean_store::Store::open_in_memory().unwrap();
        let active = root.join(format!("browser-{id}-1"));
        let mut active_lease = Disposable::create(&active).unwrap();
        let unknown = root.join(format!("browser-{id}-2/profile"));
        fs::create_dir_all(&unknown).unwrap();
        fs::write(unknown.join("keep"), b"not JEAN-owned").unwrap();

        let stale = root.join(format!("browser-{id}-3"));
        let mut stale_lease = Disposable::create(&stale).unwrap();
        let locked_file = stale.join("profile").join("locked");
        fs::write(&locked_file, b"browser data").unwrap();
        fs::write(stale.join("screenshot.png"), b"retained artifact").unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked_file)
            .unwrap();
        assert_eq!(stale_lease.finish(1).unwrap_err().raw_os_error(), Some(32));
        sweep(&root, &store);
        assert!(locked_file.exists());
        assert!(stale.join(LEASE).exists());
        drop(lock);
        sweep(&root, &store);
        assert!(!stale.join("profile").exists());
        assert!(stale.join("screenshot.png").exists());
        assert!(active.join("profile").exists());
        assert!(unknown.join("keep").exists());
        active_lease.cleanup().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
