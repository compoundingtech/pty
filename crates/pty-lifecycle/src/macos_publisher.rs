//! Darwin cannot exec an fdesc vnode. Probe and launch a private, reusable
//! snapshot instead; never return to the replaceable selected pathname.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn hash(image: &mut File) -> io::Result<[u8; 32]> {
    image.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = image.read(&mut buffer)?;
        if count == 0 { break; }
        digest.update(&buffer[..count]);
    }
    Ok(digest.finalize().into())
}

fn quarantine(image: &File) -> io::Result<Option<Vec<u8>>> {
    let name = c"com.apple.quarantine";
    // SAFETY: image owns the descriptor; name and buffers remain valid.
    let count = unsafe { libc::fgetxattr(image.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) };
    if count < 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOATTR) { Ok(None) } else { Err(error) };
    }
    let mut value = vec![0; count as usize];
    let read = unsafe { libc::fgetxattr(image.as_raw_fd(), name.as_ptr(), value.as_mut_ptr().cast(), value.len(), 0, 0) };
    if read < 0 { return Err(io::Error::last_os_error()); }
    value.truncate(read as usize);
    Ok(Some(value))
}

// Flags include security decisions (for example user approval), not merely
// mutable bookkeeping. Validate and retain the entire raw source value.
fn quarantine_identity(value: Option<&[u8]>) -> io::Result<Option<&[u8]>> {
    let Some(value) = value else { return Ok(None) };
    let separator = value.iter().position(|byte| *byte == b';')
        .ok_or_else(|| io::Error::other("malformed publisher quarantine"))?;
    if separator == 0 || !value[..separator].iter().all(u8::is_ascii_hexdigit)
        || value[separator + 1..].split(|byte| *byte == b';').count() != 3
    {
        return Err(io::Error::other("malformed publisher quarantine"));
    }
    Ok(Some(value))
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); }
}

fn open_verified(path: &Path, expected: &[u8; 32], quarantine_value: Option<&[u8]>) -> io::Result<File> {
    let mut image = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
    let metadata = image.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o500 || &hash(&mut image)? != expected
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "managed publisher cache image does not match"));
    }
    if quarantine(&image)?.as_deref() != quarantine_value {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "publisher cache quarantine does not match"));
    }
    Ok(image)
}

pub(super) fn pin(source: File) -> io::Result<(PathBuf, File, Option<Vec<u8>>)> {
    use std::os::unix::ffi::OsStringExt;
    // Use Darwin's per-user cache, never an arbitrary registry root. Resolve
    // the system /var alias, then lstat every actual component before use.
    let length = unsafe { libc::confstr(libc::_CS_DARWIN_USER_CACHE_DIR, std::ptr::null_mut(), 0) };
    if length == 0 {
        return Err(io::Error::other("Darwin user cache directory is unavailable"));
    }
    let mut bytes = vec![0; length];
    let written = unsafe { libc::confstr(
        libc::_CS_DARWIN_USER_CACHE_DIR, bytes.as_mut_ptr().cast(), bytes.len(),
    ) };
    if written == 0 || written > bytes.len() || bytes[written - 1] != 0 {
        return Err(io::Error::other("Darwin user cache directory changed during discovery"));
    }
    bytes.truncate(written - 1);
    let parent = PathBuf::from(std::ffi::OsString::from_vec(bytes)).canonicalize()?;
    let expected = quarantine(&source)?;
    let (path, image) = snapshot(source, &parent.join("compoundingtech-pty-publishers"), expected.as_deref(), true)?;
    Ok((path, image, expected))
}

#[cfg(test)]
fn pin_in(source: File, directory: &Path) -> io::Result<(PathBuf, File)> {
    let expected = quarantine(&source)?;
    snapshot(source, directory, expected.as_deref(), true)
}

// Assessment may change a cached xattr during the capability exec. Never
// execute that changed policy state or chmod an admitted image writable.
// Refresh only from the admitted image bytes, with the original raw policy.
pub(super) fn refresh(
    path: &Path, image: &File, expected: Option<&[u8]>,
) -> io::Result<Option<(PathBuf, File)>> {
    if quarantine(image)?.as_deref() == expected {
        return Ok(None);
    }
    if expected.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot discard quarantine added after publisher admission",
        ));
    }
    let parent = path.parent().ok_or_else(|| io::Error::other("publisher image has no cache"))?;
    snapshot(image.try_clone()?, parent, expected, false).map(Some)
}

fn snapshot(
    mut source: File, directory: &Path, expected: Option<&[u8]>, verify_source: bool,
) -> io::Result<(PathBuf, File)> {
    let parent = directory.parent()
        .ok_or_else(|| io::Error::other("publisher cache has no parent"))?;
    crate::image_ancestry::validate(parent)?;
    let digest = hash(&mut source)?;
    let expected = quarantine_identity(expected)?;
    // Full raw quarantine, including flags, separates security policy states.
    let mut key = Sha256::new();
    key.update(digest);
    key.update([u8::from(expected.is_some())]);
    if let Some(value) = expected { key.update(value); }
    // Validate before creation and again including the leaf. A private leaf
    // alone is insufficient if another UID can rename an ancestor.
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    crate::image_ancestry::validate(directory)?;
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "managed publisher cache must be private"));
    }
    let key = format!("{:x}", key.finalize());
    let path = directory.join(&key);
    match open_verified(&path, &digest, expected) {
        Ok(image) => return Ok((path, image)),
        Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidData) => {}
        Err(error) => return Err(error),
    }
    let temporary = Temporary(directory.join(format!(".tmp-{}", pty_core::registry::atomic::random_hex16())));
    let output = OpenOptions::new().read(true).write(true).create_new(true).mode(0o700).open(&temporary.0)?;
    source.seek(SeekFrom::Start(0))?;
    // Byte copying retains the embedded arm64 code signature. COPYFILE_XATTR
    // copies source xattrs, but Darwin may synthesize new quarantine provenance.
    // Restore the captured source value explicitly; never strip or re-sign.
    // No source ACL/stat copy may broaden the private snapshot's permissions.
    if unsafe { libc::fcopyfile(source.as_raw_fd(), output.as_raw_fd(), std::ptr::null_mut(), libc::COPYFILE_DATA | libc::COPYFILE_XATTR) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if let Some(value) = expected {
        // SAFETY: output owns the fd and both attribute buffers are valid.
        if unsafe { libc::fsetxattr(
            output.as_raw_fd(), c"com.apple.quarantine".as_ptr(),
            value.as_ptr().cast(), value.len(), 0, 0,
        ) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if (verify_source && quarantine(&source)?.as_deref() != expected)
        || quarantine(&output)?.as_deref() != expected
    {
        return Err(io::Error::other("publisher quarantine changed during snapshot"));
    }
    output.set_permissions(std::fs::Permissions::from_mode(0o500))?;
    drop(output); // No writable descriptor survives into capability exec.
    let image = open_verified(&temporary.0, &digest, expected)?;
    match std::fs::hard_link(&temporary.0, &path) {
        Ok(()) => Ok((path, image)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            match open_verified(&path, &digest, expected) {
                Ok(image) => Ok((path, image)),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    // Keep assessed entries immutable for callers already
                    // using them. A changed policy is a miss, never a match.
                    let path = directory.join(format!("{key}-{}", pty_core::registry::atomic::random_hex16()));
                    std::fs::hard_link(&temporary.0, &path)?;
                    Ok((path, image))
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarantine_identity_includes_every_flag_and_provenance_byte() {
        let original = quarantine_identity(Some(b"0081;00000000;agent;UUID")).unwrap();
        for changed in [
            &b"00c1;00000000;agent;UUID"[..],
            &b"0081;00000001;agent;UUID"[..],
            &b"0081;00000000;other;UUID"[..],
            &b"0081;00000000;agent;OTHER"[..],
        ] {
            assert_ne!(original, quarantine_identity(Some(changed)).unwrap());
        }
        assert_ne!(original, quarantine_identity(None).unwrap());
        assert!(quarantine_identity(Some(b"invalid")).is_err());
        assert!(quarantine_identity(Some(b"zzzz;00000000;agent;UUID")).is_err());
    }

    #[test]
    fn cached_image_is_reused_verified_and_preserves_quarantine() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "pty-publisher-cache-{}", pty_core::registry::atomic::random_hex16()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        {
            let directory = root.join("images");
            let selected = root.join("selected");
            // Use an Apple-signed native image whose quarantined execution is
            // allowed, without granting approval to an ad-hoc test binary.
            std::fs::copy("/usr/bin/true", &selected).unwrap();
            let (first, image) = pin_in(File::open(&selected).unwrap(), &directory).unwrap();
            let (second, reused) = pin_in(File::open(&selected).unwrap(), &directory).unwrap();
            assert_eq!(first, second);
            assert_eq!(image.metadata().unwrap().ino(), reused.metadata().unwrap().ino());
            let source = File::open(&selected).unwrap();
            let value = b"0081;00000000;pty-cache-test;";
            // SAFETY: source owns the fd, and both buffers are valid.
            assert_eq!(unsafe { libc::fsetxattr(
                source.as_raw_fd(), c"com.apple.quarantine".as_ptr(),
                value.as_ptr().cast(), value.len(), 0, 0,
            ) }, 0);
            let (quarantined_path, quarantined) = pin_in(source, &directory).unwrap();
            assert_ne!(first, quarantined_path, "quarantine provenance is part of the cache key");
            assert_eq!(quarantine(&quarantined).unwrap().as_deref(), Some(&value[..]));
            assert!(std::process::Command::new(&quarantined_path).status().unwrap().success());
            let after_exec = quarantine(&quarantined).unwrap();
            eprintln!("cached Apple-signed native exec: before={value:?}, after={after_exec:?}");
            let (after_exec_path, after_exec_image) = pin_in(File::open(&selected).unwrap(), &directory).unwrap();
            assert_eq!(quarantine(&after_exec_image).unwrap().as_deref(), Some(&value[..]));
            if after_exec.as_deref() == Some(&value[..]) {
                assert_eq!(after_exec_path, quarantined_path);
            } else {
                assert_ne!(after_exec_path, quarantined_path, "changed flags must be a cache miss");
            }
            // A different source approval state must never select the old copy.
            let source = File::open(&selected).unwrap();
            let assessed = b"00c1;00000000;pty-cache-test;";
            assert_eq!(unsafe { libc::fsetxattr(
                source.as_raw_fd(), c"com.apple.quarantine".as_ptr(),
                assessed.as_ptr().cast(), assessed.len(), 0, 0,
            ) }, 0);
            let (assessed_path, assessed_image) = pin_in(source, &directory).unwrap();
            assert_ne!(assessed_path, quarantined_path);
            assert_ne!(assessed_image.metadata().unwrap().ino(), quarantined.metadata().unwrap().ino());
            assert_eq!(quarantine(&assessed_image).unwrap().as_deref(), Some(&assessed[..]));
            // Simulate assessment changing the cached policy by atomically
            // replacing the fixture entry. Never chmod an admitted image.
            let changed = root.join("assessed-entry");
            std::fs::copy(&assessed_path, &changed).unwrap();
            std::fs::rename(&changed, &quarantined_path).unwrap();
            let changed = File::open(&quarantined_path).unwrap();
            let (fresh_path, fresh) = refresh(&quarantined_path, &changed, Some(value)).unwrap().unwrap();
            assert_ne!(fresh_path, quarantined_path);
            assert_eq!(quarantine(&fresh).unwrap().as_deref(), Some(&value[..]));
            assert_eq!(changed.metadata().unwrap().permissions().mode() & 0o777, 0o500);
            assert_eq!(fresh.metadata().unwrap().permissions().mode() & 0o777, 0o500);
            std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(&first, b"corrupted cache image").unwrap();
            std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o500)).unwrap();
            std::fs::remove_file(&selected).unwrap();
            // An already admitted image remains available after selection unlink.
            assert!(quarantined.metadata().unwrap().len() > 0);
            assert!(open_verified(&first, &hash(&mut File::open(&quarantined_path).unwrap()).unwrap(), None).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
