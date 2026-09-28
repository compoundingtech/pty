//! `pty events -f`: tail one session's log, a named set, or (`--all`) every
//! log in the registry as they appear. Existing files start at their
//! current end; files that appear while following replay from the beginning so
//! their `session_start` line is not skipped. Retention rewrites preserve the
//! overlap with the previous snapshot so old events are not replayed.
//!
//! Change detection is the `notify` crate on the registry directory plus a
//! poll every 250 ms, so delivery is deterministic even when the watcher
//! misses or coalesces an event.
//!
//! node: src/events.ts:430-546

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use super::Event;
use crate::registry::root::{events_path, session_dir};

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
    /// node: src/events.ts:441-449
    pub fn start(options: FollowerOptions, on_event: impl FnMut(Event) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = std::thread::Builder::new()
            .name("events-follow".to_string())
            .spawn(move || run(options, on_event, stop_flag))
            .ok();
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
    snapshot: Vec<u8>,
}

impl Tracked {
    fn new(path: PathBuf, replay_existing: bool) -> Self {
        let snapshot = if replay_existing {
            Vec::new()
        } else {
            std::fs::read(&path).unwrap_or_default()
        };
        Tracked { path, snapshot }
    }
}

fn name_of(file_name: &str) -> Option<&str> {
    file_name.strip_suffix(EVENTS_SUFFIX)
}

fn run(options: FollowerOptions, mut on_event: impl FnMut(Event), stop: Arc<AtomicBool>) {
    let dir = session_dir();
    let mut tracked: BTreeMap<String, Tracked> = BTreeMap::new();

    // Existing files start at EOF; a named file that does not exist yet
    // replays from the beginning when it appears.
    match &options.names {
        Some(names) => {
            for name in names {
                let path = events_path(name);
                tracked.insert(name.clone(), Tracked::new(path, false));
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
                    tracked.insert(name.to_string(), Tracked::new(path, false));
                }
            }
        }
    }

    // Wake-ups from the watcher; the payload is irrelevant, every wake-up
    // re-scans everything tracked.
    let (wake_tx, wake_rx) = channel::<()>();
    let mut watcher = notify::recommended_watcher(move |_res: notify::Result<notify::Event>| {
        let _ = wake_tx.send(());
    })
    .ok();
    if let Some(w) = watcher.as_mut() {
        let _ = w.watch(&dir, RecursiveMode::NonRecursive);
    }

    let follow_all = options.names.is_none();
    while !stop.load(Ordering::SeqCst) {
        if follow_all {
            discover_new(&dir, &mut tracked);
        }
        for t in tracked.values_mut() {
            read_new_lines(t, &mut on_event);
        }
        match wake_rx.recv_timeout(options.poll_interval) {
            Ok(()) => {
                // Drain coalesced wake-ups so a burst costs one scan.
                while wake_rx.try_recv().is_ok() {}
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                std::thread::sleep(options.poll_interval);
            }
        }
    }
    drop(watcher);
}

/// Newly created logs replay from the beginning.
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
            tracked.insert(name.to_string(), Tracked::new(entry.path(), true));
        }
    }
}

/// Deliver complete lines not present in the previous atomic snapshot. The
/// writer may remove the oldest lines at retention, so a byte offset does not
/// identify new events even when the new file is as large as the old one.
/// A trailing partial line waits for its newline.
///
/// node: src/events.ts:489-514
fn read_new_lines(t: &mut Tracked, on_event: &mut impl FnMut(Event)) {
    let Ok(buf) = std::fs::read(&t.path) else {
        return;
    };
    let Some(last_newline) = buf.iter().rposition(|b| *b == b'\n') else {
        return;
    };
    let complete = &buf[..=last_newline];
    if complete == t.snapshot {
        return;
    }
    let overlap = t
        .snapshot
        .iter()
        .enumerate()
        .filter(|(i, _)| *i == 0 || t.snapshot[i - 1] == b'\n')
        .map(|(i, _)| &t.snapshot[i..])
        .find(|suffix| complete.starts_with(suffix))
        .map_or(0, |suffix| suffix.len());
    let chunk = String::from_utf8_lossy(&complete[overlap..]);
    for line in chunk.split('\n').filter(|l| !l.is_empty()) {
        if let Ok(event) = serde_json::from_str::<Event>(line) {
            on_event(event);
        }
    }
    t.snapshot = complete.to_vec();
}
