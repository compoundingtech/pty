//! Path execution is safe from other-UID replacement only if no directory
//! component can be renamed by another UID. Same-UID mutation is out of scope.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

pub(crate) fn validate(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "publisher cache path must be absolute"));
    }
    let uid = unsafe { libc::geteuid() };
    let mut current = PathBuf::from("/");
    let mut parent = None;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => current.push(name),
            _ => return Err(io::Error::new(io::ErrorKind::PermissionDenied, "publisher cache path must be normalized")),
        }
        let metadata = std::fs::symlink_metadata(&current)?;
        if !metadata.is_dir() || (metadata.uid() != 0 && metadata.uid() != uid) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "publisher cache ancestry must contain only trusted directories, not symlinks"));
        }
        // A sticky writable parent cannot rename this root/euid-owned child.
        // A non-sticky writable ancestor permits an attacker to replace the
        // entire validated subtree, even when the leaf itself is private.
        if parent.is_some_and(|mode: u32| mode & 0o022 != 0 && mode & 0o1000 == 0) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "publisher cache ancestor is writable by another UID"));
        }
        parent = Some(metadata.mode());
    }
    if parent.is_some_and(|mode| mode & 0o022 != 0) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "publisher cache directory must not be writable by another UID"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    #[test]
    fn private_leaf_does_not_hide_replaceable_ancestors() {
        // Resolve the platform's /tmp alias before constructing a test tree;
        // the validator itself must reject, never follow, symlinks.
        let temporary = std::env::temp_dir().canonicalize().unwrap();
        let root = temporary.join(format!("pty-image-ancestry-{}", pty_core::registry::atomic::random_hex16()));
        std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let shared = root.join("shared");
        let private = shared.join("private");
        std::fs::create_dir(&shared).unwrap();
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        validate(&private).unwrap();
        for mode in [0o777, 0o770] {
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(validate(&private).is_err(), "writable ancestor mode {mode:o}");
        }
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        validate(&private).unwrap();
        assert!(validate(&shared).is_err(), "sticky is allowed only above a trusted child");
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&private, &alias).unwrap();
        assert!(validate(&alias).is_err());
        assert!(validate(&alias.join("child")).is_err());
        assert!(validate(Path::new("relative")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
