//! `pty events -f`: tail one session's log, a named set, or (`--all`) every
//! log in the registry as they appear. Existing files start at their
//! current end; files that appear while following replay from offset 0 so
//! their `session_start` line is not skipped; a shrink or changed consumed
//! prefix (retention rewrite or truncation) restarts at 0.
//!
//! Change detection is the `notify` crate on the registry directory plus a
//! poll every 250 ms, so delivery is deterministic even when the watcher
//! misses or coalesces an event, or is still starting.
//!
//! node: src/events.ts:430-546

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, channel, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecursiveMode, Watcher};

use super::Event;
use crate::registry;
use crate::registry::root::{events_path, metadata_path, session_dir};

/// The poll fallback interval.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

const EVENTS_SUFFIX: &str = ".events.jsonl";

/// What to follow.
#[derive(Debug, Clone)]
pub struct FollowerOptions {
    /// Specific sessions, or `None` for every log in the registry (`--all`).
    pub names: Option<Vec<String>>,
    /// How often to re-check files regardless of watcher notifications.
    pub poll_interval: Duration,
}

impl Default for FollowerOptions {
    fn default() -> Self {
        FollowerOptions {
            names: None,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

impl FollowerOptions {
    /// Follow these sessions only.
    pub fn names(names: Vec<String>) -> Self {
        FollowerOptions {
            names: Some(names),
            ..Default::default()
        }
    }

    /// Follow every session (`--all`).
    pub fn all() -> Self {
        FollowerOptions::default()
    }
}

/// A running follower; `stop` (or drop) ends the background thread.
pub struct EventFollower {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EventFollower {
    /// Start following, delivering every new event to `on_event` from a
    /// background thread.
    ///
    /// Returns once the thread has read where each existing log ends, as
    /// Node's `start` does before it returns. An event appended after that
    /// is delivered; one appended before it is history.
    ///
    /// node: src/events.ts:441-449
    pub fn start(options: FollowerOptions, on_event: impl FnMut(Event) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let (ready_tx, ready_rx) = channel();
        let handle = std::thread::Builder::new()
            .name("events-follow".to_string())
            .spawn(move || run(options, on_event, stop_flag, ready_tx))
            .ok();
        // Disconnected when the thread could not start: nothing to wait for.
        let _ = ready_rx.recv();
        EventFollower { stop, handle }
    }

    /// Start following and receive events on a channel.
    pub fn channel(options: FollowerOptions) -> (Self, Receiver<Event>) {
        let (tx, rx) = channel();
        let follower = Self::start(options, move |e| {
            let _ = tx.send(e);
        });
        (follower, rx)
    }

    /// Stop the background thread and wait for it.
    ///
    /// node: src/events.ts:451-458
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for EventFollower {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Tracked {
    path: PathBuf,
    offset: u64,
    snapshot: Vec<u8>,
    identity: Option<(u64, u64)>,
    registry_seen: bool,
}

impl Tracked {
    fn existing(path: PathBuf) -> Self {
        let snapshot = std::fs::read(&path).unwrap_or_default();
        Self {
            path,
            offset: snapshot.len() as u64,
            identity: None,
            snapshot,
            registry_seen: false,
        }
    }
}

fn name_of(file_name: &str) -> Option<&str> {
    file_name.strip_suffix(EVENTS_SUFFIX)
}

// Read/open/close-read notifications must never feed back into another scan.
fn wake_on_change(tx: &SyncSender<()>, result: notify::Result<notify::Event>) {
    if let Ok(event) = result
        && matches!(
            event.kind,
            EventKind::Create(_)
                | EventKind::Modify(_)
                | EventKind::Remove(_)
                | EventKind::Access(AccessKind::Close(AccessMode::Write))
        )
    {
        let _ = tx.try_send(());
    }
}

fn run(
    options: FollowerOptions,
    mut on_event: impl FnMut(Event),
    stop: Arc<AtomicBool>,
    ready: Sender<()>,
) {
    let dir = session_dir();
    let mut tracked: BTreeMap<String, Tracked> = BTreeMap::new();

    // Existing files start at EOF; a named file that does not exist yet
    // starts at 0 when it appears.
    match &options.names {
        Some(names) => {
            for name in names {
                let path = events_path(name);
                let mut entry = Tracked::existing(path);
                entry.registry_seen = metadata_path(name).exists();
                tracked.insert(name.clone(), entry);
            }
        }
        None => {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let file_name = entry.file_name();
                    let Some(name) = file_name.to_str().and_then(name_of) else {
                        continue;
                    };
                    let path = entry.path();
                    tracked.insert(name.to_string(), Tracked::existing(path));
                }
            }
        }
    }
    let _ = ready.send(());

    // Wake-ups from the watcher; the payload is irrelevant, every wake-up
    // re-scans everything tracked. The watcher starts on its own thread:
    // starting an FSEvents stream took 3.5 s on a busy macOS host, and the
    // poll below has to keep delivering meanwhile. That thread holds the
    // watcher until this function returns.
    // A bounded signal coalesces bursts without an unbounded drain that can
    // prevent shutdown while another process keeps changing the directory.
    let (wake_tx, wake_rx) = sync_channel(1);
    let (release_tx, release_rx) = channel::<()>();
    let watch_dir = dir.clone();
    let _ = std::thread::Builder::new()
        .name("events-follow-watch".to_string())
        .spawn(move || {
            let mut watcher =
                notify::recommended_watcher(move |result| wake_on_change(&wake_tx, result))
                .ok();
            if let Some(w) = watcher.as_mut() {
                let _ = w.watch(&watch_dir, RecursiveMode::NonRecursive);
            }
            // Disconnected when the follower stops.
            let _ = release_rx.recv();
            drop(watcher);
        });

    let follow_all = options.names.is_none();
    while !stop.load(Ordering::SeqCst) {
        if follow_all {
            discover_new(&dir, &mut tracked);
        }
        tracked.retain(|name, t| {
            let mut exited = false;
            read_new_lines(t, &mut |event| {
                exited |= event.r#type == super::event_type::SESSION_EXIT;
                on_event(event);
            });
            if follow_all {
                return true;
            }
            let present = metadata_path(name).exists();
            let removed = t.registry_seen && !present;
            t.registry_seen |= present;
            let retained_exit = registry::read_metadata(name)
                .is_some_and(|metadata| metadata.has_exited() || metadata.exit_code.is_some());
            !exited && !removed && !retained_exit
        });
        if !follow_all && tracked.is_empty() {
            break;
        }
        match wake_rx.recv_timeout(options.poll_interval) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                std::thread::sleep(options.poll_interval);
            }
        }
    }
    drop(release_tx);
}

/// Newly created logs replay from offset 0.
///
/// node: src/events.ts:532-544
fn discover_new(dir: &Path, tracked: &mut BTreeMap<String, Tracked>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str().and_then(name_of) else {
            continue;
        };
        if !tracked.contains_key(name) {
            tracked.insert(
                name.to_string(),
                Tracked {
                    path: entry.path(),
                    offset: 0,
                    snapshot: Vec::new(),
                    identity: None,
                    registry_seen: false,
                },
            );
        }
    }
}

/// Deliver every complete line past the tracked offset. A shrink or changed
/// consumed prefix restarts at 0, even if retention regrew between scans.
/// A trailing partial line waits for its newline.
///
/// node: src/events.ts:489-514
fn read_new_lines(t: &mut Tracked, on_event: &mut impl FnMut(Event)) {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;

    let Ok(mut file) = std::fs::File::open(&t.path) else {
        return;
    };
    let Ok(metadata) = file.metadata() else {
        return;
    };
    let identity = (metadata.dev(), metadata.ino());
    if t.identity == Some(identity) && metadata.len() == t.snapshot.len() as u64 {
        return;
    }
    let mut content = Vec::with_capacity(metadata.len() as usize);
    if file.read_to_end(&mut content).is_err() {
        return;
    }
    t.identity = Some(identity);
    let offset = t.offset as usize;
    if content.len() < offset || t.snapshot.get(..offset) != content.get(..offset) {
        t.offset = 0;
    }
    let offset = t.offset as usize;
    let Some(last_newline) = content[offset..].iter().rposition(|b| *b == b'\n') else {
        t.snapshot = content;
        return;
    };
    let end = offset + last_newline + 1;
    let chunk = String::from_utf8_lossy(&content[offset..end]);
    t.offset = end as u64;
    for line in chunk.split('\n').filter(|l| !l.is_empty()) {
        if let Ok(event) = serde_json::from_str::<Event>(line) {
            on_event(event);
        }
    }
    t.snapshot = content;
}

#[cfg(test)]
mod tests {
    #[test]
    fn another_process_reading_logs_does_not_wake_the_follower() {
        let dir = std::env::temp_dir()
            .join(format!("pty-follower-access-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("idle.events.jsonl");
        std::fs::write(&path, "history\n").unwrap();
        let (tx, rx) = sync_channel(1);
        let mut watcher = notify::recommended_watcher(move |result| {
            wake_on_change(&tx, result);
        }).unwrap();
        watcher.watch(&dir, RecursiveMode::NonRecursive).unwrap();
        let status = std::process::Command::new("sh")
            .args(["-c", "i=0; while [ \"$i\" -lt 100 ]; do cat \"$1\" >/dev/null; i=$((i + 1)); done", "read-logs"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let wake = rx.recv_timeout(Duration::from_millis(300));
        drop(watcher);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(wake, Err(RecvTimeoutError::Timeout)), "read caused a scan: {wake:?}");
    }

    #[test]
    fn only_changes_and_close_write_wake_and_signals_are_coalesced() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind};
        let (tx, rx) = sync_channel(1);
        for kind in [
            EventKind::Any,
            EventKind::Other,
            EventKind::Access(AccessKind::Any),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Open(AccessMode::Read)),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
        ] {
            wake_on_change(&tx, Ok(notify::Event::new(kind)));
            assert!(rx.try_recv().is_err(), "{kind:?}");
        }
        for kind in [
            EventKind::Create(CreateKind::Any),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Remove(RemoveKind::Any),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
        ] {
            for _ in 0..100 {
                wake_on_change(&tx, Ok(notify::Event::new(kind)));
            }
            assert_eq!(rx.try_iter().count(), 1, "{kind:?}");
        }
    }

    use super::*;

    #[test]
    fn coalesced_retention_and_regrowth_replays_every_retained_new_record() {
        let path =
            std::env::temp_dir().join(format!("pty-follower-regrowth-{}", std::process::id()));
        let history: String = (0..900)
            .map(|i| {
                format!(
                    "{}\n",
                    Event::bell("regrowth")
                        .with_ts(&format!("old-{i}"))
                        .to_json()
                )
            })
            .collect();
        std::fs::write(&path, &history).unwrap();
        let mut tracked = Tracked::existing(path.clone());
        let padding = "x".repeat(2000);
        let batch: Vec<_> = (0..300)
            .map(|i| {
                Event::new("regrowth", "user.batch")
                    .with("seq", i)
                    .with("padding", padding.as_str())
            })
            .collect();
        crate::events::append_batch(&path, &batch, &mut 0).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > history.len() as u64);
        let mut seen = std::collections::BTreeSet::new();
        read_new_lines(&mut tracked, &mut |event| {
            if event.r#type == "user.batch" {
                seen.insert(event.get("seq").unwrap().as_u64().unwrap());
            }
        });
        assert_eq!(seen, (0..300).collect());
        std::fs::remove_file(path).unwrap();
    }
}
