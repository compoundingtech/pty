//! What a spawner settles before a daemon starts: the command path.
//!
//! node: src/spawn.ts:372-393

use std::path::{Path, PathBuf};
use std::io::Read;

/// Node's `resolveCommand`: an absolute path must exist; a path with a `/`
/// is resolved against the current directory and must exist; a bare name is
/// looked up on `PATH` the way `which` does (first regular file with an
/// execute bit). Errors are `Command not found: <cmd>`.
///
/// node: src/spawn.ts:372-393
pub fn resolve_command(cmd: &str) -> Result<String, String> {
    let not_found = || format!("Command not found: {cmd}");
    let path = Path::new(cmd);
    if path.is_absolute() {
        return if path.exists() {
            check_executable(path, cmd)?;
            Ok(cmd.to_string())
        } else {
            Err(not_found())
        };
    }
    if cmd.contains('/') {
        let resolved = std::env::current_dir()
            .map(|cwd| normalize(&cwd.join(path)))
            .map_err(|_| not_found())?;
        return if resolved.exists() {
            check_executable(&resolved, cmd)?;
            Ok(resolved.to_string_lossy().into_owned())
        } else {
            Err(not_found())
        };
    }
    let found = which(cmd).ok_or_else(not_found)?;
    check_executable(Path::new(&found), cmd)?;
    Ok(found)
}

fn check_executable(path: &Path, requested: &str) -> Result<(), String> {
    if !is_executable_file(path) {
        return Err(format!("Command is not executable: {requested}"));
    }
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        // An executable binary need not be readable. The spawn path will
        // report any real execution failure after this optional shebang check.
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err(format!("Cannot read command {requested}: {error}")),
    };
    let mut head = [0u8; 512];
    let size = file
        .read(&mut head)
        .map_err(|error| format!("Cannot read command {requested}: {error}"))?;
    if head[..size].starts_with(b"#!") {
        let line = head[2..size].split(|byte| *byte == b'\n').next().unwrap_or_default();
        let interpreter = line
            .split(|byte| byte.is_ascii_whitespace())
            .find(|part| !part.is_empty())
            .unwrap_or_default();
        let interpreter = std::str::from_utf8(interpreter).unwrap_or_default();
        if !is_executable_file(Path::new(interpreter)) {
            return Err(format!(
                "Command interpreter not found or not executable: {requested} ({interpreter})"
            ));
        }
    }
    Ok(())
}

/// `path.resolve`: collapse `.` and `..` lexically (no symlink resolution).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The first executable regular file named `cmd` on `PATH`.
fn which(cmd: &str) -> Option<String> {
    if cmd.is_empty() {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = if dir.as_os_str().is_empty() {
            PathBuf::from(cmd)
        } else {
            dir.join(cmd)
        };
        if is_executable_file(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn is_executable_file(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok();
    match c {
        // SAFETY: `access` only reads the path.
        Some(c) => unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 },
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_only_binary_can_be_resolved() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::current_dir()
            .unwrap()
            .join(format!("target-exec-only-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("true");
        std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o111)).unwrap();
        let resolved = resolve_command(binary.to_str().unwrap());
        std::fs::remove_file(&binary).unwrap();
        std::fs::remove_dir(&dir).unwrap();
        assert_eq!(resolved.unwrap(), binary.to_string_lossy());
    }

    #[test]
    fn absolute_paths_must_exist() {
        assert_eq!(resolve_command("/bin/sh").unwrap(), "/bin/sh");
        assert_eq!(
            resolve_command("/definitely/not/here").unwrap_err(),
            "Command not found: /definitely/not/here"
        );
    }

    #[test]
    fn bare_names_come_from_path() {
        let sh = resolve_command("sh").unwrap();
        assert!(sh.ends_with("/sh"), "{sh}");
        assert_eq!(
            resolve_command("no-such-command-xyz").unwrap_err(),
            "Command not found: no-such-command-xyz"
        );
    }

    #[test]
    fn relative_paths_resolve_against_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let dir = cwd.join("target-rc-test-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tool");
        std::fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rel = "./target-rc-test-dir/../target-rc-test-dir/tool";
        assert_eq!(
            resolve_command(rel).unwrap(),
            file.to_string_lossy().into_owned()
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(resolve_command(rel).is_err());
    }
}
