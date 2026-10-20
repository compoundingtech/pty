use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::{InventoryErrorKind, ListOptions, SessionInfo, SessionMetadata};

#[derive(Debug, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Identity {
    fn of(file: &File) -> io::Result<Option<Self>> {
        let stat = file.metadata()?;
        Ok(stat.is_file().then(|| Self {
            dev: stat.dev(),
            ino: stat.ino(),
            size: stat.size(),
            mtime: (stat.mtime(), stat.mtime_nsec()),
            ctime: (stat.ctime(), stat.ctime_nsec()),
        }))
    }
}

#[derive(Debug)]
struct Entry {
    identity: Identity,
    metadata: SessionMetadata,
}

/// Stateful parsed-metadata cache for repeated registry observations.
///
/// Keep this handle between calls. Only regular-file JSON metadata is cached;
/// directory discovery, PID reads, process identity and socket probes run anew.
/// Each hit opens the file to observe current readability and descriptor identity.
/// Errors are never retained. Concurrent calls share only short cache operations;
/// no cache lock is held during file reads, parsing or live observations.
#[derive(Debug, Default)]
pub struct RegistryListCache {
    entries: Mutex<HashMap<PathBuf, Entry>>,
}

impl RegistryListCache {
    /// Observe the registry with the same semantics and probe budget as
    /// [`super::list_sessions_in`], reusing unchanged parsed metadata.
    pub fn list(&self, root: &Path, options: &ListOptions) -> Vec<SessionInfo> {
        super::scan_sessions(root, options, false, None, Some(self)).entries
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<PathBuf, Entry>> {
        self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn retain(&self, root: &Path, names: &std::collections::BTreeSet<&str>) {
        self.entries().retain(|path, _| {
            path.parent() != Some(root)
                || path.file_stem().and_then(|name| name.to_str())
                    .is_some_and(|name| names.contains(name))
        });
    }

    pub(super) fn metadata(
        &self,
        path: &Path,
    ) -> Result<SessionMetadata, (InventoryErrorKind, String)> {
        let unreadable = |error: io::Error| (InventoryErrorKind::MetadataUnreadable, error.to_string());
        let read = (|| {
            let mut file = File::options()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)?;
            let before = Identity::of(&file)?;
            let cached = self.entries().get(path)
                .filter(|entry| Some(&entry.identity) == before.as_ref())
                .map(|entry| entry.metadata.clone());
            if let Some(metadata) = cached
                && Identity::of(&file)? == before
            {
                return Ok(Ok(metadata));
            }
            self.entries().remove(path);
            let mut bytes = Vec::new();
            if before.is_some() {
                file.read_to_end(&mut bytes)?;
            } else {
                // Match ordinary listing's treatment of nonregular companions,
                // but never retain their contents in the cache.
                bytes = std::fs::read(path)?;
            }
            let after = Identity::of(&file)?;
            Ok(Err((bytes, before.filter(|identity| Some(identity) == after.as_ref()))))
        })();
        match read {
            Ok(Ok(metadata)) => Ok(metadata),
            Ok(Err((bytes, identity))) => {
                let metadata = parse_metadata(&bytes).map_err(|error| {
                    (InventoryErrorKind::MetadataMalformed, error.to_string())
                })?;
                if let Some(identity) = identity {
                    self.entries().insert(path.to_path_buf(), Entry {
                        identity,
                        metadata: metadata.clone(),
                    });
                }
                Ok(metadata)
            }
            Err(error) => {
                self.entries().remove(path);
                Err(unreadable(error))
            }
        }
    }
}

pub(super) fn parse_metadata(bytes: &[u8]) -> Result<SessionMetadata, serde_json::Error> {
    #[cfg(test)]
    PARSES.with(|count| count.set(count.get() + 1));
    let value = serde_json::from_slice::<serde_json::Value>(bytes)?;
    if !value.is_object() {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "metadata must be a JSON object",
        ));
    }
    serde_json::from_value(value)
}

#[cfg(test)]
thread_local! {
    static PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::SessionStatus;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    struct Root(PathBuf);

    impl Root {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "pty-metadata-cache-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, label: &str) {
            std::fs::write(self.0.join(format!("{name}.json")), record(label)).unwrap();
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn record(label: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "command": "cat", "args": [], "displayCommand": "cat", "cwd": "/tmp",
            "createdAt": "2026-10-20T00:00:00Z", "displayName": label
        })).unwrap()
    }

    fn parses() -> usize {
        PARSES.with(std::cell::Cell::get)
    }

    #[test]
    // The probe is deliberately held until the second observer has completed.
    // The timeout only bounds a broken test; channels establish the ordering.
    fn concurrent_probe_does_not_hold_cache_lock() {
        use std::sync::mpsc;
        use std::time::Duration;
        let root = Root::new();
        root.write("first", "label");
        std::fs::write(super::super::socket_path_in(&root.0, "first"), []).unwrap();
        let cache = RegistryListCache::default();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let root = &root.0;
            let cache = &cache;
            let first = scope.spawn(move || {
                super::super::scan_sessions_with_probe(
                    root, &ListOptions::default(), true, None, Some(cache),
                    |paths, _| {
                        assert_eq!(paths.len(), 1);
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        HashMap::new()
                    },
                )
            });
            entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            scope.spawn(move || {
                let rows = cache.list(root, &ListOptions::default());
                completed_tx.send(rows.len()).unwrap();
            });
            let completed = completed_rx.recv_timeout(Duration::from_secs(10));
            release_tx.send(()).unwrap();
            first.join().unwrap();
            assert_eq!(completed.unwrap(), 1, "second listing completes before the first probe");
        });
    }

    #[test]
    fn unchanged_changed_new_removed_and_same_size_replacement() {
        let root = Root::new();
        root.write("first", "before");
        let options = ListOptions::default();
        let cache = RegistryListCache::default();
        let before = parses();
        assert_eq!(cache.list(&root.0, &options).len(), 1);
        assert_eq!(parses() - before, 1);
        cache.list(&root.0, &options);
        assert_eq!(parses() - before, 1, "unchanged JSON is not reparsed");

        // Equal byte length and restored mtime must not conceal atomic replacement.
        let path = root.0.join("first.json");
        let old_time = std::fs::metadata(&path).unwrap().modified().unwrap();
        let replacement = root.0.join("replacement");
        std::fs::write(&replacement, record("after!")).unwrap();
        File::options().write(true).open(&replacement).unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time)).unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert_eq!(cache.list(&root.0, &options)[0].display_name(), Some("after!"));
        assert_eq!(parses() - before, 2);

        root.write("first", "changed in place");
        root.write("second", "new");
        let rows = cache.list(&root.0, &options);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].display_name(), Some("changed in place"));
        assert_eq!(parses() - before, 4);
        std::fs::remove_file(path).unwrap();
        assert_eq!(cache.list(&root.0, &options)[0].name, "second");
        assert_eq!(cache.entries().len(), 1);
    }

    #[test]
    fn failures_are_not_cached_and_missing_root_evicts() {
        let root = Root::new();
        let path = root.0.join("first.json");
        std::fs::write(&path, b"invalid").unwrap();
        let options = ListOptions::default();
        let cache = RegistryListCache::default();
        let before = parses();
        assert!(cache.list(&root.0, &options).is_empty());
        assert!(cache.list(&root.0, &options).is_empty());
        assert_eq!(parses() - before, 2);
        assert!(cache.entries().is_empty());
        root.write("first", "valid");
        assert_eq!(cache.list(&root.0, &options).len(), 1);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(cache.list(&root.0, &options).is_empty());
        assert!(cache.entries().is_empty());
        std::fs::remove_dir(&path).unwrap();
        root.write("first", "restored");
        cache.list(&root.0, &options);
        let moved = root.0.with_extension("moved");
        std::fs::rename(&root.0, &moved).unwrap();
        let missing = cache.list(&root.0, &options);
        std::fs::rename(&moved, &root.0).unwrap();
        assert!(missing.is_empty());
        assert!(cache.entries().is_empty());
    }

    #[test]
    fn readability_changes_match_stateless_listing() {
        use std::os::unix::fs::PermissionsExt;
        let root = Root::new();
        root.write("first", "label");
        let path = root.0.join("first.json");
        let options = ListOptions::default();
        let cache = RegistryListCache::default();
        assert_eq!(cache.list(&root.0, &options).len(), 1);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0)).unwrap();
        let cached = cache.list(&root.0, &options);
        let ordinary = super::super::list_sessions_in(&root.0, &options);
        assert_eq!(cached, ordinary);
        // A privileged test process may still read a mode-000 file.
        if ordinary.is_empty() {
            assert!(cache.entries().is_empty());
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(cache.list(&root.0, &options).len(), 1);
    }

    #[test]
    fn process_death_with_unchanged_metadata_is_observed_next_pass() {
        let root = Root::new();
        root.write("first", "live");
        let mut child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        std::fs::write(root.0.join("first.pid"), child.id().to_string()).unwrap();
        let cache = RegistryListCache::default();
        let options = ListOptions::default();
        let before = parses();
        let running = cache.list(&root.0, &options);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(running[0].status, SessionStatus::Running);
        let gone = cache.list(&root.0, &options);
        assert_eq!(gone[0].status, SessionStatus::Vanished);
        assert_eq!(gone[0].pid, None);
        assert_eq!(parses() - before, 1);
    }

    #[test]
    fn reachable_socket_cannot_rescue_gone_identity_with_cached_metadata() {
        let root = Root::new();
        root.write("first", "label");
        std::fs::write(root.0.join("first.pid"), "2147483647").unwrap();
        let socket = super::super::socket_path_in(&root.0, "first");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let options = ListOptions::default();
        let cache = RegistryListCache::default();
        let before = parses();
        let reachable = cache.list(&root.0, &options);
        assert_eq!(reachable[0].status, SessionStatus::Vanished);
        assert_eq!(reachable[0].identity, super::super::DaemonIdentityCheck::Gone);
        drop(listener);
        // Unlink the endpoint before replacing it with an unreachable companion:
        // another test's concurrent fork can briefly inherit the listener FD.
        std::fs::remove_file(&socket).unwrap();
        std::fs::write(&socket, []).unwrap();
        assert_eq!(cache.list(&root.0, &options)[0].status, SessionStatus::Vanished);
        assert_eq!(parses() - before, 1);
    }

    #[test]
    fn cached_metadata_preserves_unknown_owner_and_fresh_identity_classification() {
        use super::super::{DaemonIdentityCheck, read_boot_id, read_process_start_token};
        let root = Root::new();
        let pid = std::process::id() as i32;
        let mut metadata: serde_json::Value = serde_json::from_slice(&record("label")).unwrap();
        metadata["daemonPid"] = pid.into();
        metadata["daemonStartToken"] = read_process_start_token(pid).unwrap().into();
        metadata["tags"] = serde_json::json!({"owner": "person/robin"});
        metadata["futureField"] = serde_json::json!({"retained": true});
        let path = root.0.join("first.json");
        std::fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let cache = RegistryListCache::default();
        let options = ListOptions::default();
        let before = parses();
        for _ in 0..2 {
            let unknown = cache.list(&root.0, &options);
            assert_eq!(unknown[0].identity, DaemonIdentityCheck::Unknown);
            assert_eq!(unknown[0].status, SessionStatus::Running);
            assert_eq!(unknown[0].pid, Some(pid));
            assert_eq!(unknown[0].metadata.as_ref().unwrap().to_map()["tags"], metadata["tags"]);
            assert_eq!(unknown[0].metadata.as_ref().unwrap().to_map()["futureField"], metadata["futureField"]);
        }
        assert_eq!(parses() - before, 1);

        metadata["daemonBootId"] = read_boot_id().unwrap().into();
        std::fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert_eq!(cache.list(&root.0, &options)[0].identity, DaemonIdentityCheck::Match);
        // Companion disagreement changes identity even with unchanged cached JSON.
        let sidecar = root.0.join("first.pid");
        std::fs::write(&sidecar, "2147483647").unwrap();
        let conflicting = cache.list(&root.0, &options);
        assert_eq!(conflicting[0].identity, DaemonIdentityCheck::Unknown);
        assert_eq!(conflicting[0].status, SessionStatus::Running);
        assert_eq!(conflicting[0].pid, Some(2147483647));
        assert!(conflicting[0].metadata.is_some());
        std::fs::remove_file(sidecar).unwrap();
        assert_eq!(cache.list(&root.0, &options)[0].identity, DaemonIdentityCheck::Match);
        assert_eq!(parses() - before, 2);

        metadata.as_object_mut().unwrap().remove("daemonBootId");
        metadata["exitCode"] = 0.into();
        std::fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let exited = cache.list(&root.0, &options);
        assert_eq!(exited[0].identity, DaemonIdentityCheck::Unknown);
        assert_eq!(exited[0].status, SessionStatus::Exited);
        assert_eq!(exited[0].pid, Some(pid));
        assert!(exited[0].metadata.is_some());
    }

    #[test]
    fn fallback_process_start_token_is_not_cached() {
        let root = Root::new();
        let mut child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let token = super::super::read_process_start_token(child.id() as i32).unwrap();
        let mut metadata: serde_json::Value = serde_json::from_slice(&record("label")).unwrap();
        metadata["daemonPid"] = child.id().into();
        metadata["recovery"] = serde_json::json!({"processStartToken": token});
        std::fs::write(root.0.join("first.json"), serde_json::to_vec(&metadata).unwrap()).unwrap();
        let options = ListOptions::default();
        let cache = RegistryListCache::default();
        let before = parses();
        let running = cache.list(&root.0, &options);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(running[0].status, SessionStatus::Running);
        assert_eq!(running[0].identity, super::super::DaemonIdentityCheck::Unknown);
        let gone = cache.list(&root.0, &options);
        assert_eq!(gone[0].status, SessionStatus::Vanished);
        assert_eq!(gone[0].pid, None);
        assert_eq!(parses() - before, 1);
    }

    #[test]
    fn pid_sidecar_and_exit_metadata_changes_are_observed_next_pass() {
        let root = Root::new();
        root.write("first", "label");
        let cache = RegistryListCache::default();
        let options = ListOptions::default();
        assert_eq!(cache.list(&root.0, &options)[0].status, SessionStatus::Vanished);
        let before = parses();
        let pid = root.0.join("first.pid");
        std::fs::write(&pid, std::process::id().to_string()).unwrap();
        assert_eq!(cache.list(&root.0, &options)[0].status, SessionStatus::Running);
        std::fs::write(&pid, "2147483647").unwrap();
        assert_eq!(cache.list(&root.0, &options)[0].status, SessionStatus::Vanished);
        assert_eq!(parses(), before);
        let mut metadata: serde_json::Value = serde_json::from_slice(&record("label")).unwrap();
        metadata["exitCode"] = 0.into();
        metadata["exitedAt"] = "2026-10-20T00:01:00Z".into();
        std::fs::write(root.0.join("first.json"), serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert_eq!(cache.list(&root.0, &options)[0].status, SessionStatus::Exited);
        assert_eq!(parses() - before, 1);
    }

    #[test]
    fn metadata_cache_200_session_measurement() {
        let root = Root::new();
        for index in 0..200 {
            root.write(&format!("session-{index:03}"), "label");
        }
        let options = ListOptions::default();
        let before = parses();
        let started = Instant::now();
        for _ in 0..20 {
            assert_eq!(super::super::list_sessions_in(&root.0, &options).len(), 200);
        }
        let uncached = started.elapsed();
        let uncached_parses = parses() - before;
        let cache = RegistryListCache::default();
        cache.list(&root.0, &options);
        let before = parses();
        let started = Instant::now();
        for _ in 0..20 {
            assert_eq!(cache.list(&root.0, &options).len(), 200);
        }
        let cached = started.elapsed();
        let cached_parses = parses() - before;
        assert_eq!(uncached_parses, 4000);
        assert_eq!(cached_parses, 0);
        eprintln!("200 sessions, 20 snapshots: uncached={uncached:?} ({uncached_parses} parses), warm cached={cached:?} ({cached_parses} parses)");
    }
}
