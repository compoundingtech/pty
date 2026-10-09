//! Atomic file publication: write `<path>.tmp.<pid>.<16 hex>` in the same
//! directory, then rename over the target. Readers see the old file or the
//! new one, never a torn intermediate. Concurrent writers do not coordinate
//! (last rename wins) but cannot corrupt each other because every writer
//! uses its own temporary name.
//!
//! node: src/sessions.ts:245-284

use std::io::{self, Write};
use std::path::Path;

/// 16 hex characters of randomness (Node's `randomHex(8)`), from
/// `/dev/urandom` when available, else a time/pid/counter mix. This only
/// needs low collision probability between concurrent writers.
pub fn random_hex16() -> String {
    let bytes = random_bytes(8);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `n` random bytes, `/dev/urandom` first, a cheap mixer as the fallback.
pub fn random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom")
        && f.read_exact(&mut buf).is_ok()
    {
        return buf;
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ COUNTER.fetch_add(0x632B_E59B_D9B4_E019, Ordering::Relaxed);
    for b in buf.iter_mut() {
        // splitmix64
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        *b = z as u8;
    }
    buf
}

/// The temporary name an atomic write of `target` uses.
pub fn tmp_path_for(target: &Path) -> std::path::PathBuf {
    let mut os = target.as_os_str().to_owned();
    os.push(format!(".tmp.{}.{}", std::process::id(), random_hex16()));
    std::path::PathBuf::from(os)
}

/// Does a directory entry name belong to an in-flight atomic write? Readers
/// skip these everywhere (`docs/disk-layout.md`: `*.tmp.<pid>.<rand>`).
pub fn is_tmp_name(file_name: &str) -> bool {
    file_name.contains(".tmp.")
}

/// Write `bytes` to `target` atomically. The temporary file is created
/// owner-only (0600) at open, before any byte is written: registry records
/// carry session environment values (`extraEnv`/`sessionEnv`), and a
/// create-then-chmod window would leave them readable in a traversable
/// directory. On failure the temporary file is unlinked and the error
/// returned; the previous target is intact either way.
///
/// node: src/sessions.ts:251-264
pub fn atomic_write(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_path_for(target);
    let result = create_owner_only(&tmp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, target));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Open `path` for writing, created fresh with owner-only permissions.
///
/// `create_new` refuses a name that already exists, which cannot happen for
/// a `tmp_path_for` name, and `mode(0o600)` is applied by the creating open
/// itself (subject only to umask, which strips no owner bits), so the file
/// is never observable with wider permissions.
#[cfg(unix)]
fn create_owner_only(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_owner_only(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registry records persist session environment values, so both the
    /// in-flight temporary and the published file must be owner-only even
    /// where the surrounding directory is looser.
    #[test]
    fn a_written_file_is_owner_only() {
        let target = std::env::temp_dir().join(format!(
            "pty-atomic-mode-{}-{}",
            std::process::id(),
            random_hex16()
        ));
        atomic_write(&target, b"{}").unwrap();
        let mode = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777
            }
            #[cfg(not(unix))]
            {
                0o600
            }
        };
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(&target);
    }

    /// The owner-only mode is a property of the creating open, not a later
    /// chmod: the descriptor's file is 0600 the moment it exists, with no
    /// create-then-tighten window, whatever the ambient umask leaves alone.
    #[cfg(unix)]
    #[test]
    fn the_temporary_is_owner_only_from_the_creating_open() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = std::env::temp_dir().join(format!(
            "pty-atomic-create-mode-{}-{}",
            std::process::id(),
            random_hex16()
        ));
        let file = create_owner_only(&tmp).unwrap();
        let mode = file
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        drop(file);
        let _ = std::fs::remove_file(&tmp);
    }
}
