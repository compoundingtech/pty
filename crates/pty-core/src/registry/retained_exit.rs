//! Private local exit receipts, independent of reapable session artifacts.
//!
//! One atomic, owner-only file retains the newest 256 exits (at most 8 KiB of
//! terminal text each). Consumers must apply their own publication/redaction
//! policy before forwarding terminal content outside the private PTY root.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{atomic_write, ensure_session_dir, session_dir, try_acquire_file_lock};

pub const RETAINED_EXIT_LIMIT: usize = 256;
pub const RETAINED_EXIT_TAIL_BYTES: usize = 8192;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const FILE_NAME: &str = ".retained-exits.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetainedSessionExit {
    pub name: String,
    pub generation: String,
    pub daemon_pid: u32,
    pub created_at: String,
    /// Actual wait status, not the shell convention 128 + signal.
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub output_tail: String,
}

fn read_all(root: &Path) -> io::Result<Vec<RetainedSessionExit>> {
    let mut file = match OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(root.join(FILE_NAME)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let stat = file.metadata()?;
    if !stat.is_file() || stat.len() > MAX_FILE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid retained exit file"));
    }
    let mut bytes = Vec::with_capacity(stat.len() as usize);
    file.by_ref().take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized retained exit file"));
    }
    let records: Vec<RetainedSessionExit> = serde_json::from_slice(&bytes)?;
    if records.len() > RETAINED_EXIT_LIMIT || records.iter().any(|row|
        row.output_tail.len() > RETAINED_EXIT_TAIL_BYTES || row.name.len() > 256
            || row.generation.len() > 256 || row.created_at.len() > 128) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid retained exit bounds"));
    }
    Ok(records)
}

/// Exact incarnation lookup; never falls back to a replacement's evidence.
pub fn retained_session_exit_in(root: &Path, name: &str, daemon_pid: u32, created_at: &str)
    -> io::Result<Option<RetainedSessionExit>> {
    Ok(read_all(root)?.into_iter().rev().find(|row|
        row.name == name && row.daemon_pid == daemon_pid && row.created_at == created_at))
}

/// Persist before session cleanup. Serialize writers independently of registry
/// generation ownership, because an old child can finish after its replacement.
pub fn retain_session_exit(mut record: RetainedSessionExit) -> io::Result<()> {
    if record.name.len() > 256 || record.generation.len() > 256 || record.created_at.len() > 128 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid retained exit identity"));
    }
    if record.output_tail.len() > RETAINED_EXIT_TAIL_BYTES {
        let mut start = record.output_tail.len() - RETAINED_EXIT_TAIL_BYTES;
        while !record.output_tail.is_char_boundary(start) { start += 1; }
        // Drop the partial first line rather than publish a sliced secret token.
        let tail = &record.output_tail[start..];
        record.output_tail = tail.find('\n').map_or_else(String::new, |offset| tail[offset + 1..].to_owned());
    }
    ensure_session_dir()?;
    let root = session_dir();
    let deadline = Instant::now() + Duration::from_secs(2);
    let _lock = loop {
        if let Some(lock) = try_acquire_file_lock(&root.join(".retained-exits.lock"))? { break lock; }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "retained exit file busy"));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut records = read_all(&root)?;
    records.retain(|row| !(row.name == record.name && row.daemon_pid == record.daemon_pid
        && row.created_at == record.created_at));
    if records.len() == RETAINED_EXIT_LIMIT { records.remove(0); }
    records.push(record);
    atomic_write(&root.join(FILE_NAME), &serde_json::to_vec(&records)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_survive_cleanup_and_are_incarnation_fenced_and_bounded() {
        let root = std::env::temp_dir().join(format!("pty-retained-exit-{}", super::super::random_hex16()));
        std::fs::create_dir_all(&root).unwrap();
        super::super::with_root(&root, || {
            for index in 0..=RETAINED_EXIT_LIMIT {
                retain_session_exit(RetainedSessionExit {
                    name: "work".into(), generation: format!("g-{index}"), daemon_pid: 42,
                    created_at: index.to_string(), exit_code: Some(23), signal: None,
                    output_tail: format!("{}\nfatal: fixture failure\n", "x".repeat(RETAINED_EXIT_TAIL_BYTES)),
                }).unwrap();
            }
            super::super::cleanup_all("work").unwrap();
            assert!(retained_session_exit_in(&root, "work", 42, "0").unwrap().is_none());
            assert!(retained_session_exit_in(&root, "work", 43, "256").unwrap().is_none());
            let receipt = retained_session_exit_in(&root, "work", 42, "256").unwrap().unwrap();
            assert_eq!(receipt.exit_code, Some(23));
            assert_eq!(receipt.output_tail, "fatal: fixture failure\n");
            assert_eq!(read_all(&root).unwrap().len(), RETAINED_EXIT_LIMIT);
        });
        std::fs::remove_dir_all(root).unwrap();
    }
}
