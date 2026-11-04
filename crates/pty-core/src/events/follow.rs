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
    /// End named lifecycle waits when a daemon vanishes without exit metadata.
    pub stop_on_vanished: bool,
}

impl Default for FollowerOptions {
    fn default() -> Self {
        FollowerOptions {
            names: None,
            poll_interval: DEFAULT_POLL_INTERVAL,
            stop_on_vanished: false,
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

// A removed/renamed registry is no longer watched, even if it is recreated.
// Retire a broken backend instead of leaving its worker alive indefinitely;
// polling continues to discover logs in the replacement directory.
fn watch_event(
    dir: &Path,
    wake: &SyncSender<()>,
    retire: &SyncSender<()>,
    result: notify::Result<notify::Event>,
) {
    let invalidated = match &result {
        Err(_) => true,
        Ok(event) => {
            matches!(
                event.kind,
                EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
            ) && event.paths.iter().any(|path| path == dir)
        }
    };
    if invalidated {
        let _ = retire.try_send(());
    } else {
        wake_on_change(wake, result);
    }
}

fn wait_for_change(wake: &Receiver<()>, interval: Duration, sleep: impl FnOnce(Duration)) {
    if matches!(
        wake.recv_timeout(interval),
        Err(RecvTimeoutError::Disconnected)
    ) {
        // A failed watcher disconnects immediately, not after the timeout.
        sleep(interval);
    }
}

fn registry_entry_removed(name: &str) -> bool {
    // Permanent respawn holds a creation claim across cleanup and replacement
    // publication. Observe its live owner without creating, stealing, or
    // removing a writer's claim: followers must never make a creator Busy.
    !registry::is_creation_lock_held(name) && !metadata_path(name).exists()
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
    // watcher until shutdown, a backend error, or registry removal.
    // A bounded signal coalesces bursts without an unbounded drain that can
    // prevent shutdown while another process keeps changing the directory.
    let (wake_tx, wake_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let watch_dir = dir.clone();
    let retire_tx = release_tx.clone();
    let _ = std::thread::Builder::new()
        .name("events-follow-watch".to_string())
        .spawn(move || {
            let callback_dir = watch_dir.clone();
            let Ok(mut watcher) = notify::recommended_watcher(move |result| {
                watch_event(&callback_dir, &wake_tx, &retire_tx, result);
            }) else {
                return;
            };
            if watcher.watch(&watch_dir, RecursiveMode::NonRecursive).is_err() {
                return;
            }
            // Both the follower and callback can retire the watcher. Never
            // block its callback: dropping the watcher joins its worker.
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
            let removed = t.registry_seen && !present && registry_entry_removed(name);
            t.registry_seen |= present;
            let retained_exit = registry::read_metadata(name)
                .is_some_and(|metadata| metadata.has_exited() || metadata.exit_code.is_some());
            if retained_exit {
                // finalize_exit flushes session_exit before saving terminal
                // metadata. It may have done both after our first log read.
                read_new_lines(t, &mut on_event);
            }
            let vanished = options.stop_on_vanished
                && !registry::is_creation_lock_held(name)
                && registry::get_session_by_name(name)
                    .is_some_and(|session| session.status == registry::SessionStatus::Vanished);
            !exited && !removed && !retained_exit && !vanished
        });
        if !follow_all && tracked.is_empty() {
            break;
        }
        wait_for_change(&wake_rx, options.poll_interval, std::thread::sleep);
    }
    let _ = release_tx.try_send(());
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
        // Establish that the watch is delivering changes before measuring
        // silence. In particular, FSEvents may attach asynchronously.
        std::fs::write(&path, "canary\n").unwrap();
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        while rx.recv_timeout(Duration::from_millis(300)).is_ok() {}
        let status = std::process::Command::new("sh")
            .args(["-c", "i=0; while [ \"$i\" -lt 100 ]; do cat \"$1\" >/dev/null; i=$((i + 1)); done", "read-logs"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let wake = rx.recv_timeout(Duration::from_millis(300));
        assert!(matches!(wake, Err(RecvTimeoutError::Timeout)), "read caused a scan: {wake:?}");
        // A silent or broken watcher must not make the idle assertion pass.
        std::fs::write(&path, "changed\n").unwrap();
        let changed = rx.recv_timeout(Duration::from_secs(2));
        drop(watcher);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(changed.is_ok(), "a real write must wake the follower: {changed:?}");
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
    fn read_notifications_never_feed_back_even_after_each_scan() {
        let dir = Path::new("/registry");
        let (wake_tx, wake_rx) = sync_channel(1);
        let (retire_tx, retire_rx) = sync_channel(1);
        for _ in 0..100 {
            for kind in [
                EventKind::Access(AccessKind::Open(AccessMode::Read)),
                EventKind::Access(AccessKind::Read),
                EventKind::Access(AccessKind::Close(AccessMode::Read)),
            ] {
                watch_event(dir, &wake_tx, &retire_tx, Ok(notify::Event::new(kind)));
            }
            // An empty queue after every synthetic scan, rather than only
            // after a burst, catches a self-sustaining read/scan cycle.
            assert!(matches!(wake_rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
            assert!(matches!(retire_rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        }
    }

    #[test]
    fn watcher_errors_and_registry_removal_retire_without_scan_storms() {
        use notify::event::{ModifyKind, RemoveKind, RenameMode};
        let dir = Path::new("/registry");
        for event in [
            None,
            Some(notify::Event::new(EventKind::Remove(RemoveKind::Folder))
                .add_path(dir.to_path_buf())),
            Some(notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                .add_path(dir.to_path_buf())),
            Some(notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(dir.to_path_buf())
                .add_path(PathBuf::from("/old-registry"))),
        ] {
            let (wake_tx, wake_rx) = sync_channel(1);
            let (retire_tx, retire_rx) = sync_channel(1);
            for _ in 0..100 {
                let result = event.clone().ok_or_else(|| notify::Error::generic("backend failed"));
                watch_event(dir, &wake_tx, &retire_tx, result);
            }
            assert_eq!(retire_rx.try_iter().count(), 1);
            assert!(matches!(wake_rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
            // Already-retired holders must not block their callback either.
            drop(retire_rx);
            let result = event.ok_or_else(|| notify::Error::generic("backend failed"));
            watch_event(dir, &wake_tx, &retire_tx, result);
        }
    }

    #[test]
    fn removing_a_log_does_not_retire_the_registry_watcher() {
        use notify::event::RemoveKind;
        let (wake_tx, wake_rx) = sync_channel(1);
        let (retire_tx, retire_rx) = sync_channel(1);
        watch_event(
            Path::new("/registry"),
            &wake_tx,
            &retire_tx,
            Ok(notify::Event::new(EventKind::Remove(RemoveKind::File))
                .add_path(PathBuf::from("/registry/session.events.jsonl"))),
        );
        assert_eq!(wake_rx.try_recv(), Ok(()));
        assert!(matches!(retire_rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn disconnected_watcher_sleeps_once_per_poll_but_notifications_do_not() {
        let interval = Duration::from_millis(250);
        let (tx, rx) = sync_channel(1);
        tx.try_send(()).unwrap();
        wait_for_change(&rx, interval, |_| panic!("a notification must not sleep"));
        wait_for_change(&rx, Duration::ZERO, |_| panic!("a timeout already waited"));
        drop(tx);
        for _ in 0..100 {
            let mut slept = None;
            wait_for_change(&rx, interval, |duration| slept = Some(duration));
            assert_eq!(slept, Some(interval));
        }
    }

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
