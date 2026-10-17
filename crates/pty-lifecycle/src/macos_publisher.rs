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

// Darwin may rewrite the flags field during copy or assessment. The remaining
// timestamp;agent;UUID provenance is stable and must match exactly. Missing,
// malformed, or changed provenance is not permission to reuse another image.
fn quarantine_provenance(value: Option<&[u8]>) -> io::Result<Option<&[u8]>> {
    let Some(value) = value else { return Ok(None) };
    let separator = value.iter().position(|byte| *byte == b';')
        .ok_or_else(|| io::Error::other("malformed publisher quarantine"))?;
    if separator == 0 || !value[..separator].iter().all(u8::is_ascii_hexdigit)
        || value[separator + 1..].split(|byte| *byte == b';').count() != 3
    {
        return Err(io::Error::other("malformed publisher quarantine"));
    }
    Ok(Some(&value[separator + 1..]))
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); }
}

fn open_verified(path: &Path, expected: &[u8; 32], provenance: Option<&[u8]>) -> io::Result<File> {
    let mut image = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
    let metadata = image.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o500 || &hash(&mut image)? != expected
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "managed publisher cache image does not match"));
    }
    if quarantine_provenance(quarantine(&image)?.as_deref())? != provenance {
        return Err(io::Error::other("publisher cache quarantine provenance does not match"));
    }
    Ok(image)
}

pub(super) fn pin(source: File) -> io::Result<(PathBuf, File)> {
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
    pin_in(source, &parent.join("compoundingtech-pty-publishers"))
}

fn pin_in(mut source: File, directory: &Path) -> io::Result<(PathBuf, File)> {
    let parent = directory.parent()
        .ok_or_else(|| io::Error::other("publisher cache has no parent"))?;
    crate::image_ancestry::validate(parent)?;
    let digest = hash(&mut source)?;
    let source_quarantine = quarantine(&source)?;
    let source_provenance = quarantine_provenance(source_quarantine.as_deref())?;
    // A newly quarantined selection must not reuse an already assessed copy
    // of the same bytes from a different quarantine provenance.
    let mut key = Sha256::new();
    key.update(digest);
    key.update([u8::from(source_quarantine.is_some())]);
    if let Some(value) = source_provenance { key.update(value); }
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
    let path = directory.join(format!("{:x}", key.finalize()));
    match open_verified(&path, &digest, source_provenance) {
        Ok(image) => return Ok((path, image)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
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
    if let Some(value) = &source_quarantine {
        // SAFETY: output owns the fd and both attribute buffers are valid.
        if unsafe { libc::fsetxattr(
            output.as_raw_fd(), c"com.apple.quarantine".as_ptr(),
            value.as_ptr().cast(), value.len(), 0, 0,
        ) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if quarantine_provenance(quarantine(&source)?.as_deref())? != source_provenance
        || quarantine_provenance(quarantine(&output)?.as_deref())? != source_provenance
    {
        return Err(io::Error::other("publisher quarantine changed during snapshot"));
    }
    output.set_permissions(std::fs::Permissions::from_mode(0o500))?;
    drop(output); // No writable descriptor survives into capability exec.
    let image = open_verified(&temporary.0, &digest, source_provenance)?;
    match std::fs::hard_link(&temporary.0, &path) {
        Ok(()) => Ok((path, image)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let image = open_verified(&path, &digest, source_provenance)?;
            Ok((path, image))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarantine_provenance_ignores_only_system_flags() {
        let original = quarantine_provenance(Some(b"0081;00000000;agent;UUID")).unwrap();
        assert_eq!(original, quarantine_provenance(Some(b"00c1;00000000;agent;UUID")).unwrap());
        for changed in [
            &b"0081;00000001;agent;UUID"[..],
            &b"0081;00000000;other;UUID"[..],
            &b"0081;00000000;agent;OTHER"[..],
        ] {
            assert_ne!(original, quarantine_provenance(Some(changed)).unwrap());
        }
        assert_ne!(original, quarantine_provenance(None).unwrap());
        assert!(quarantine_provenance(Some(b"invalid")).is_err());
        assert!(quarantine_provenance(Some(b"zzzz;00000000;agent;UUID")).is_err());
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
            std::fs::copy(std::env::current_exe().unwrap(), &selected).unwrap();
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
            assert_eq!(
                quarantine_provenance(quarantine(&quarantined).unwrap().as_deref()).unwrap(),
                quarantine_provenance(Some(value)).unwrap(),
            );
            // A system flags rewrite must reuse the already assessed inode,
            // not create another first-exec path or reject the source.
            let source = File::open(&selected).unwrap();
            let assessed = b"00c1;00000000;pty-cache-test;";
            assert_eq!(unsafe { libc::fsetxattr(
                source.as_raw_fd(), c"com.apple.quarantine".as_ptr(),
                assessed.as_ptr().cast(), assessed.len(), 0, 0,
            ) }, 0);
            let (assessed_path, assessed_image) = pin_in(source, &directory).unwrap();
            assert_eq!(assessed_path, quarantined_path);
            assert_eq!(assessed_image.metadata().unwrap().ino(), quarantined.metadata().unwrap().ino());
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
