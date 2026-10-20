use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

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
    seen: bool,
}

/// Stateful parsed-metadata cache for repeated registry observations.
///
/// Keep this handle between calls. Only regular-file JSON metadata is cached;
/// directory discovery, PID reads, process identity and socket probes run anew.
/// Each hit opens the file to observe current readability and descriptor identity.
/// Errors are never retained. A handle can be reused across different roots.
#[derive(Debug, Default)]
pub struct RegistryListCache {
    entries: HashMap<PathBuf, Entry>,
}

impl RegistryListCache {
    /// Observe the registry with the same semantics and probe budget as
    /// [`super::list_sessions_in`], reusing unchanged parsed metadata.
    pub fn list(&mut self, root: &Path, options: &ListOptions) -> Vec<SessionInfo> {
        for entry in self.entries.values_mut() {
            entry.seen = false;
        }
        let inventory = super::scan_sessions(root, options, false, None, Some(self));
        self.entries.retain(|_, entry| entry.seen);
        inventory.entries
    }

    pub(super) fn metadata(
        &mut self,
        path: &Path,
    ) -> Result<SessionMetadata, (InventoryErrorKind, String)> {
        let unreadable = |error: io::Error| (InventoryErrorKind::MetadataUnreadable, error.to_string());
        let read = (|| {
            let mut file = File::options()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)?;
            let before = Identity::of(&file)?;
            if let Some(identity) = &before
                && let Some(entry) = self.entries.get_mut(path)
                && &entry.identity == identity
                && Identity::of(&file)?.as_ref() == Some(identity)
            {
                entry.seen = true;
                return Ok(Ok(entry.metadata.clone()));
            }
            self.entries.remove(path);
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
                    self.entries.insert(path.to_path_buf(), Entry {
                        identity,
                        metadata: metadata.clone(),
                        seen: true,
                    });
                }
                Ok(metadata)
            }
            Err(error) => {
                self.entries.remove(path);
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
    fn unchanged_changed_new_removed_and_same_size_replacement() {
        let root = Root::new();
        root.write("first", "before");
        let options = ListOptions::default();
        let mut cache = RegistryListCache::default();
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
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn failures_are_not_cached_and_missing_root_evicts() {
        let root = Root::new();
        let path = root.0.join("first.json");
        std::fs::write(&path, b"invalid").unwrap();
        let options = ListOptions::default();
        let mut cache = RegistryListCache::default();
        let before = parses();
        assert!(cache.list(&root.0, &options).is_empty());
        assert!(cache.list(&root.0, &options).is_empty());
        assert_eq!(parses() - before, 2);
        assert!(cache.entries.is_empty());
        root.write("first", "valid");
        assert_eq!(cache.list(&root.0, &options).len(), 1);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(cache.list(&root.0, &options).is_empty());
        assert!(cache.entries.is_empty());
        std::fs::remove_dir(&path).unwrap();
        root.write("first", "restored");
        cache.list(&root.0, &options);
        assert!(cache.list(&root.0.join("absent"), &options).is_empty());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn readability_changes_match_stateless_listing() {
        use std::os::unix::fs::PermissionsExt;
        let root = Root::new();
        root.write("first", "label");
        let path = root.0.join("first.json");
        let options = ListOptions::default();
        let mut cache = RegistryListCache::default();
        assert_eq!(cache.list(&root.0, &options).len(), 1);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0)).unwrap();
        let cached = cache.list(&root.0, &options);
        let ordinary = super::super::list_sessions_in(&root.0, &options);
        assert_eq!(cached, ordinary);
        // A privileged test process may still read a mode-000 file.
        if ordinary.is_empty() {
            assert!(cache.entries.is_empty());
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
        let mut cache = RegistryListCache::default();
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
    fn socket_reachability_is_not_cached() {
        let root = Root::new();
        root.write("first", "label");
        std::fs::write(root.0.join("first.pid"), "2147483647").unwrap();
        let socket = super::super::socket_path_in(&root.0, "first");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let options = ListOptions::default();
        let mut cache = RegistryListCache::default();
        let before = parses();
        assert_eq!(cache.list(&root.0, &options)[0].status, SessionStatus::Running);
        drop(listener);
        assert_eq!(cache.list(&root.0, &options)[0].status, SessionStatus::Vanished);
        assert_eq!(parses() - before, 1);
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
        let mut cache = RegistryListCache::default();
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
    fn pid_sidecar_and_exit_metadata_changes_are_observed_next_pass() {
        let root = Root::new();
        root.write("first", "label");
        let mut cache = RegistryListCache::default();
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
        let mut cache = RegistryListCache::default();
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
