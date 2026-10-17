//! Permanent publication fences, independent of session lifecycle files.
//!
//! The root-owning Unix principal is the trust boundary: these records are not
//! signatures or authority separating processes running as the same UID.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ListOptions, LockRefusal, lock_or_refusal, lock_path, observe_session_strict_in, session_dir, validate_name};

const MAX_BYTES: u64 = 16 * 1024;
const STORE: &str = ".publication-revocations";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationBinding {
    pub operation: String,
    pub digest: String,
}

impl PublicationBinding {
    pub fn validate(&self) -> io::Result<()> {
        validate_operation(&self.operation)?;
        validate_digest(&self.digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationRevocation {
    pub schema: u32,
    pub root_id: String,
    pub name: String,
    pub binding: PublicationBinding,
    pub intent_digest: String,
    pub capability_version: u32,
    pub revoked: bool,
    pub attestation_receipt_or_intent_id: String,
    pub writer_uid: u32,
    pub recorded_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevocationInstall {
    pub record: PublicationRevocation,
    pub occupancy: serde_json::Value,
    pub already_installed: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RootIdentity {
    schema: u32,
    root_id: String,
    path: String,
    device: u64,
    inode: u64,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn validate_operation(operation: &str) -> io::Result<()> {
    if operation.is_empty() || operation.len() > 1024 || operation.chars().any(char::is_control) {
        return Err(invalid("invalid publication operation"));
    }
    Ok(())
}

fn validate_digest(digest: &str) -> io::Result<()> {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(invalid("expected 64 lowercase hexadecimal digest"));
    }
    Ok(())
}

fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn component(value: &str) -> io::Result<CString> {
    CString::new(value).map_err(|_| invalid("NUL in filesystem component"))
}

fn validate_file(file: &File, directory: bool) -> io::Result<()> {
    let stat = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if stat.uid() != uid
        || stat.mode() & 0o7777 != if directory { 0o700 } else { 0o600 }
        || if directory { !stat.is_dir() } else { !stat.is_file() || stat.nlink() != 1 }
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe publication revocation owner, type, or permissions"));
    }
    Ok(())
}

// All store traversal uses owned directory descriptors and single components.
fn open_at(parent: &File, name: &str, directory: bool) -> io::Result<File> {
    let name = component(name)?;
    let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent and name remain valid for this call; ownership transfers below.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: openat returned a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    validate_file(&file, directory)?;
    Ok(file)
}

fn directory_at(parent: &File, name: &str, create: bool) -> io::Result<File> {
    if create {
        let c_name = component(name)?;
        // SAFETY: both arguments remain valid for the call.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o700) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists { return Err(error); }
        }
    }
    let directory = open_at(parent, name, true)?;
    if create { parent.sync_all()?; }
    Ok(directory)
}

fn open_root(create: bool) -> io::Result<File> {
    let path = session_dir();
    let path = if path.is_absolute() { path } else { std::env::current_dir()?.join(path) };
    // Resolve ancestor aliases (notably macOS /tmp -> /private/tmp), but never
    // resolve the selected root itself: its final component must not be a symlink.
    // Missing suffixes are still created by the descriptor-relative walk below.
    if path.components().any(|part| matches!(part, Component::ParentDir)) {
        return Err(invalid("registry root must not contain parent components"));
    }
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut ancestor = parent;
    let canonical_parent = loop {
        match ancestor.canonicalize() {
            Ok(canonical) => break canonical.join(parent.strip_prefix(ancestor).map_err(|_| invalid("invalid registry root ancestor"))?),
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                ancestor = ancestor.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
            }
            Err(error) => return Err(error),
        }
    };
    let path = match path.file_name() {
        Some(name) => canonical_parent.join(name),
        None => path.canonicalize()?,
    };
    let mut directory = File::open("/")?;
    for part in path.components() {
        let name = match part {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => CString::new(name.as_bytes()).map_err(|_| invalid("invalid registry root"))?,
            _ => return Err(invalid("registry root must not contain parent components")),
        };
        if create {
            // SAFETY: directory and name remain live; mkdirat never follows the
            // final component, and the subsequent open refuses symlinks.
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists { return Err(error); }
            } else {
                directory.sync_all()?;
            }
        }
        // SAFETY: directory and name are live; a successful descriptor is owned.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW) };
        if fd < 0 { return Err(io::Error::last_os_error()); }
        // SAFETY: openat returned a new owned descriptor.
        directory = unsafe { File::from_raw_fd(fd) };
    }
    validate_file(&directory, true)?;
    Ok(directory)
}

fn read_json<T: for<'de> Deserialize<'de>>(parent: &File, name: &str) -> io::Result<T> {
    let mut file = open_at(parent, name, false)?;
    if file.metadata()?.len() > MAX_BYTES { return Err(invalid("oversized revocation record")); }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES { return Err(invalid("oversized revocation record")); }
    serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))
}

fn atomic_rename_error(error: io::Error) -> io::Error {
    if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)) {
        return io::Error::new(io::ErrorKind::Unsupported, format!(
            "publication revocation requires filesystem and kernel support for atomic no-replace rename (Linux renameat2 RENAME_NOREPLACE or macOS renameatx_np RENAME_EXCL); use a supported filesystem/kernel; no unsafe link/unlink fallback is permitted: {error}"
        ));
    }
    error
}

// A successful publication has exactly one directory entry, even if the
// process dies immediately after this syscall. Never use link/unlink here:
// validate_file deliberately rejects multiply linked canonical records.
fn rename_noreplace(parent: &File, temporary: &CString, name: &CString) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    // SAFETY: the directory descriptor and both component strings remain live.
    let result = unsafe {
        libc::renameat2(parent.as_raw_fd(), temporary.as_ptr(), parent.as_raw_fd(), name.as_ptr(), libc::RENAME_NOREPLACE)
    };
    #[cfg(target_os = "macos")]
    // SAFETY: the directory descriptor and both component strings remain live.
    let result = unsafe {
        libc::renameatx_np(parent.as_raw_fd(), temporary.as_ptr(), parent.as_raw_fd(), name.as_ptr(), libc::RENAME_EXCL)
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(io::Error::new(io::ErrorKind::Unsupported, "atomic no-replace publication requires Linux or macOS"));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if result < 0 { Err(atomic_rename_error(io::Error::last_os_error())) } else { Ok(()) }
}

// Return false for an existing canonical file. Never replace any canonical inode.
fn publish<T: Serialize>(parent: &File, name: &str, value: &T) -> io::Result<bool> {
    let bytes = serde_json::to_vec(value).map_err(|error| invalid(error.to_string()))?;
    if bytes.len() as u64 > MAX_BYTES { return Err(invalid("oversized revocation record")); }
    let temporary = component(&format!(".tmp.{}.{}", std::process::id(), super::random_hex16()))?;
    let name = component(name)?;
    // SAFETY: descriptors and C strings are valid for the call.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), temporary.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW, 0o600) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: openat returned a new owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| {
        validate_file(&file, false)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        if let Err(error) = rename_noreplace(parent, &temporary, &name) {
            if error.kind() == io::ErrorKind::AlreadyExists { return Ok(false); }
            return Err(error);
        }
        Ok(true)
    })();
    if !matches!(&result, Ok(true)) {
        // SAFETY: unlink only the unique temporary component created above.
        if unsafe { libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    parent.sync_all()?;
    result
}

fn store(create: bool) -> io::Result<(File, RootIdentity)> {
    store_at(open_root(create)?, create)
}

fn store_at(root: File, create: bool) -> io::Result<(File, RootIdentity)> {
    let base = directory_at(&root, STORE, create)?;
    let version = directory_at(&base, "v1", create)?;
    let stat = root.metadata()?;
    let path = session_dir().canonicalize()?.into_os_string().into_string().map_err(|_| invalid("registry root is not UTF-8"))?;
    let identity: RootIdentity = match read_json(&version, "root.json") {
        Ok(identity) => identity,
        Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
            let identity = RootIdentity {
                schema: 1,
                root_id: format!("{}{}{}{}", super::random_hex16(), super::random_hex16(), super::random_hex16(), super::random_hex16()),
                path: path.clone(),
                device: stat.dev(),
                inode: stat.ino(),
            };
            publish(&version, "root.json", &identity)?;
            read_json(&version, "root.json")?
        }
        Err(error) => return Err(error),
    };
    validate_digest(&identity.root_id)?;
    if identity.schema != 1 || identity.path != path || identity.device != stat.dev() || identity.inode != stat.ino() {
        return Err(invalid("publication revocation root identity mismatch; do not reset the store: see README publication root re-enrollment"));
    }
    Ok((version, identity))
}

/// Establish or validate enrollment identity. Never changes existing identity.
pub fn root_id() -> io::Result<String> {
    store(true).map(|(_, identity)| identity.root_id)
}

fn read_record(version: &File, identity: &RootIdentity, name: &str, operation: &str) -> io::Result<Option<PublicationRevocation>> {
    let directory = match directory_at(version, &hash(name), false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let record: PublicationRevocation = match read_json(&directory, &format!("{}.json", hash(operation))) {
        Ok(record) => record,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    record.binding.validate()?;
    validate_digest(&record.intent_digest)?;
    // SAFETY: geteuid has no preconditions.
    let writer_uid = unsafe { libc::geteuid() };
    if record.capability_version != 1
        || !record.revoked
        || record.attestation_receipt_or_intent_id != record.intent_digest
        || record.writer_uid != writer_uid
        || super::parse_iso8601_ms(&record.recorded_at).is_none()
    {
        return Err(invalid("publication revocation audit identity mismatch"));
    }
    if record.schema != 1 || record.root_id != identity.root_id || record.name != name || record.binding.operation != operation {
        return Err(invalid("publication revocation record identity mismatch"));
    }
    Ok(Some(record))
}

/// Read one exact fence without acquiring creation ownership or mutating files.
/// Legacy roots may use their existing permissions and symlink ancestors only
/// when descriptor-relative inspection proves the store itself is absent.
pub fn read(name: &str, operation: &str) -> io::Result<Option<PublicationRevocation>> {
    read_with_root_policy(name, operation, false)
}

fn read_with_root_policy(name: &str, operation: &str, managed: bool) -> io::Result<Option<PublicationRevocation>> {
    validate_name(name).map_err(invalid)?;
    validate_operation(operation)?;
    if !managed {
        // Open using the same ancestor-following semantics as legacy root.rs.
        // Only ENOENT for the store entry permits compatibility: a dangling
        // symlink, invalid store, or failed inspection must never bypass checks.
        let root = File::open(session_dir())?;
        let name = component(STORE)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: root and name are live and stat points to writable storage.
        if unsafe { libc::fstatat(root.as_raw_fd(), name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound { return Ok(None); }
            return Err(error);
        }
    }
    // Managed reads always validate the root, even before store enrollment.
    // Any present store also requires strict root and store validation.
    let root = open_root(false)?;
    match open_at(&root, STORE, true) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let (version, identity) = store_at(root, false)?;
    read_record(&version, &identity, name, operation)
}

/// Install durably under the existing name creation lock, without signals or
/// session mutation. A failed observation never produces a safe acknowledgment.
pub fn install(name: &str, binding: &PublicationBinding, intent_digest: &str, expected_root_id: &str) -> io::Result<RevocationInstall> {
    validate_name(name).map_err(invalid)?;
    binding.validate()?;
    validate_digest(intent_digest)?;
    validate_digest(expected_root_id)?;
    // Installation requires prior enrollment, never invents an identity from
    // an untrusted command's target root.
    let (version, identity) = store(false)?;
    if identity.root_id != expected_root_id {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "publication revocation expected root identity mismatch"));
    }
    let claim = lock_or_refusal(&lock_path(name)).map_err(|refusal| match refusal {
        LockRefusal::Busy => io::Error::new(io::ErrorKind::WouldBlock, "publication name is busy"),
        // LockRefusal retains the OS diagnostic but exposes it as a String,
        // so the original error kind is unavailable. Never label it retryable.
        LockRefusal::Unavailable(cause) => io::Error::other(format!("publication name lock unavailable: {cause}")),
    })?;
    // Pin this exact claim nonblockingly before reading or publishing a fence.
    // In-process launches and GC must not borrow installation's ownership.
    let _pin = claim.pin()?;
    crate::operation::barrier::barrier("revocation-install-pinned");
    let existing = read_record(&version, &identity, name, &binding.operation)?;
    let already_installed = existing.is_some();
    let durable = if let Some(record) = existing {
        if record.binding != *binding || record.intent_digest != intent_digest {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "publication revocation conflicts with existing immutable record"));
        }
        record
    } else {
        // SAFETY: geteuid has no preconditions.
        let writer_uid = unsafe { libc::geteuid() };
        let record = PublicationRevocation {
            schema: 1,
            root_id: identity.root_id.clone(),
            name: name.to_string(),
            binding: binding.clone(),
            intent_digest: intent_digest.to_string(),
            capability_version: 1,
            revoked: true,
            attestation_receipt_or_intent_id: intent_digest.to_string(),
            writer_uid,
            recorded_at: super::now_iso8601(),
        };
        let directory = directory_at(&version, &hash(name), true)?;
        publish(&directory, &format!("{}.json", hash(&binding.operation)), &record)?;
        let durable = read_record(&version, &identity, name, &binding.operation)?.ok_or_else(|| invalid("installed revocation disappeared"))?;
        if durable != record {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "publication revocation conflicts with existing immutable record"));
        }
        durable
    };
    // A retry after a failed directory fsync must establish durability again
    // before acknowledging an otherwise exact replay.
    directory_at(&version, &hash(name), false)?.sync_all()?;
    let inventory = observe_session_strict_in(&session_dir(), name, &ListOptions::default());
    if !inventory.complete || !inventory.errors.is_empty() {
        return Err(invalid(format!("publication occupancy unavailable: {:?}", inventory.errors)));
    }
    let entries: Vec<_> = inventory.entries.into_iter().map(|entry| serde_json::json!({
        "name": entry.name, "status": entry.status.to_string(), "pid": entry.pid, "metadata": entry.metadata,
    })).collect();
    Ok(RevocationInstall { record: durable, occupancy: serde_json::json!({"complete": true, "entries": entries}), already_installed })
}

/// Final publication barrier. Caller must retain name creation ownership through
/// every publication effect; this function never acquires that lock itself.
pub fn check(name: &str, binding: Option<&PublicationBinding>, tag_operation: Option<&str>) -> io::Result<()> {
    validate_name(name).map_err(invalid)?;
    if let Some(binding) = binding { binding.validate()?; }
    if let Some(operation) = tag_operation { validate_operation(operation)?; }
    let operation = binding.map(|binding| binding.operation.as_str()).or(tag_operation);
    if let Some(operation) = operation
        && read_with_root_policy(name, operation, binding.is_some())?.is_some()
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "operation-revoked"));
    }
    match (binding, tag_operation) {
        (Some(_), None) => Err(io::Error::new(io::ErrorKind::PermissionDenied, "managed publication requires operation tag")),
        (Some(binding), Some(operation)) if binding.operation != operation =>
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "publication binding and operation tag disagree")),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn unsupported_atomic_rename_errno_explains_required_filesystem_and_kernel_support() {
        for errno in [libc::ENOSYS, libc::EINVAL, libc::EOPNOTSUPP] {
            let error = atomic_rename_error(io::Error::from_raw_os_error(errno));
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            let message = error.to_string();
            assert!(message.contains("filesystem and kernel support for atomic no-replace rename"), "{message}");
            assert!(message.contains("use a supported filesystem/kernel"), "{message}");
            assert!(message.contains("no unsafe link/unlink fallback"), "{message}");
        }
    }

    #[test]
    fn other_atomic_rename_errno_preserves_os_error() {
        for errno in [libc::EEXIST, libc::EACCES, libc::ENOSPC, libc::EROFS] {
            let error = atomic_rename_error(io::Error::from_raw_os_error(errno));
            assert_eq!(error.raw_os_error(), Some(errno));
            assert_eq!(error.kind(), io::Error::from_raw_os_error(errno).kind());
        }
    }


    #[test]
    fn atomic_publication_crash_boundary_has_no_temporary_alias_or_replacement() {
        let path = std::env::temp_dir().join(format!("pty-revocation-rename-{}-{}", std::process::id(), super::super::random_hex16()));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = File::open(&path).unwrap();
        let temporary = component(".tmp.crashed").unwrap();
        let canonical = component("record.json").unwrap();
        let source = path.join(".tmp.crashed");
        std::fs::write(&source, b"durable fence").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        File::open(&source).unwrap().sync_all().unwrap();
        rename_noreplace(&directory, &temporary, &canonical).unwrap();
        // This is precisely the state a killed publisher leaves after the
        // publication syscall, without executing any temporary-file cleanup.
        assert!(!source.exists());
        let record = open_at(&directory, "record.json", false).unwrap();
        assert_eq!(record.metadata().unwrap().nlink(), 1);
        let inode = record.metadata().unwrap().ino();
        std::fs::write(&source, b"must not replace fence").unwrap();
        assert_eq!(rename_noreplace(&directory, &temporary, &canonical).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(path.join("record.json")).unwrap(), b"durable fence");
        assert_eq!(std::fs::metadata(path.join("record.json")).unwrap().ino(), inode);
        std::fs::remove_dir_all(path).unwrap();
    }
}
