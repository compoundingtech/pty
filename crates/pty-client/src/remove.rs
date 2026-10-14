//! Remove a session that is not running from the registry — socket, pid,
//! metadata, events, recovery revision — under the creation lock with a
//! generation check: the operation behind `pty rm`.
//!
//! node: src/cli.ts:3036-3087 (`cmdRm`)

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use pty_core::registry::{
    self, DaemonIdentityCheck, SessionGenerationOwner, SessionMetadata, SessionStatus,
    cleanup_owned_all,
};

/// How long an exited session's daemon gets to finish before the removal
/// gives up on it.
const DAEMON_EXIT_WAIT: Duration = Duration::from_secs(7);

/// Why a session was not removed. `Display` is the `pty rm` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveError {
    /// The name matched several sessions by display name; the message lists
    /// their ids.
    Lookup(String),
    NotFound {
        name: String,
    },
    /// Stop it first.
    StillRunning {
        name: String,
    },
    /// The exited session's daemon had not gone after 7 s.
    DaemonDidNotExit {
        name: String,
    },
    /// Recorded owner identity could not be proved or disproved.
    IdentityUnverified {
        name: String,
    },
    /// A new generation published under the same name while this waited,
    /// and it was left alone.
    Replaced {
        name: String,
    },
}

impl fmt::Display for RemoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoveError::Lookup(message) => f.write_str(message),
            RemoveError::NotFound { name } => write!(f, "Session \"{name}\" not found."),
            RemoveError::StillRunning { name } => write!(
                f,
                "Session \"{name}\" is still running. Use \"pty kill {name}\" first."
            ),
            RemoveError::DaemonDidNotExit { name } => write!(
                f,
                "Session \"{name}\" daemon did not exit within 7s; not removed. Try again."
            ),
            RemoveError::IdentityUnverified { name } => write!(
                f,
                "Refusing to remove session \"{name}\": daemon identity is unknown."
            ),
            RemoveError::Replaced { name } => write!(
                f,
                "Session \"{name}\" was replaced while waiting; new generation was not removed."
            ),
        }
    }
}

impl std::error::Error for RemoveError {}

/// Remove the session `name` under `$PTY_ROOT`.
///
/// Like Node's `cmdRm`, the session is looked up by id or unique display
/// name, but the pid lookup and the cleanup use `name` exactly as given.
pub fn remove(name: &str) -> Result<(), RemoveError> {
    let Some(session) = registry::get_session(name).map_err(RemoveError::Lookup)? else {
        return Err(RemoveError::NotFound {
            name: name.to_string(),
        });
    };
    if session.status == SessionStatus::Running {
        return Err(RemoveError::StillRunning {
            name: name.to_string(),
        });
    }

    // `exited` means the child is gone, not necessarily the daemon: it keeps
    // its socket alive briefly so attached clients receive the exit packet,
    // then cleans up. Wait on the old generation's daemon so an immediate
    // same-name `pty run` cannot publish a socket the old daemon unlinks.
    let generation = session.metadata.as_ref().and_then(|m| m.generation.clone());
    let sidecar = registry::read_session_pid(name);
    let daemon_pid = sidecar.or_else(|| session.metadata.as_ref().and_then(|m| m.daemon_pid));
    match registry::check_record_identity(sidecar, session.metadata.as_ref()) {
        DaemonIdentityCheck::Gone | DaemonIdentityCheck::Mismatch => {}
        DaemonIdentityCheck::Unknown => {
            return Err(RemoveError::IdentityUnverified {
                name: name.to_string(),
            });
        }
        DaemonIdentityCheck::Match => {
            let (Some(pid), Some(metadata)) = (daemon_pid, session.metadata.as_ref()) else {
                return Err(RemoveError::IdentityUnverified {
                    name: name.to_string(),
                });
            };
            wait_for_recorded_daemon_exit(name, pid, metadata, DAEMON_EXIT_WAIT)?;
        }
    }

    // Re-check generation ownership in the same critical section as the
    // unlink so a replacement that published meanwhile is left alone.
    let owner = SessionGenerationOwner {
        generation: generation.unwrap_or_default(),
        pid: daemon_pid.unwrap_or(-1),
    };
    if !cleanup_owned_all(name, &owner) {
        return Err(RemoveError::Replaced {
            name: name.to_string(),
        });
    }
    Ok(())
}

fn wait_for_recorded_daemon_exit(
    name: &str,
    pid: i32,
    metadata: &SessionMetadata,
    timeout: Duration,
) -> Result<(), RemoveError> {
    let started = Instant::now();
    loop {
        match registry::check_daemon_identity(pid, metadata) {
            DaemonIdentityCheck::Gone | DaemonIdentityCheck::Mismatch => return Ok(()),
            DaemonIdentityCheck::Unknown => {
                return Err(RemoveError::IdentityUnverified {
                    name: name.to_string(),
                });
            }
            DaemonIdentityCheck::Match if registry::has_process_exited_for_reap(pid) => {
                return Ok(());
            }
            DaemonIdentityCheck::Match => {}
        }
        if started.elapsed() >= timeout {
            return Err(RemoveError::DaemonDidNotExit {
                name: name.to_string(),
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// [`remove`] in the registry at `root` instead of `$PTY_ROOT`.
pub fn remove_in(root: &Path, name: &str) -> Result<(), RemoveError> {
    registry::with_root(root, || remove(name))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn remove_skips_reused_pid_wait_and_refuses_unknown_identity() {
        struct Fixture {
            child: std::process::Child,
            root: std::path::PathBuf,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("owned child");
        let pid = child.id() as i32;
        let root = std::env::temp_dir().join(format!("ptyrm-{}-{pid}", std::process::id()));
        std::fs::create_dir(&root).expect("isolated registry");
        let mut fixture = Fixture { child, root };
        registry::with_root(&fixture.root, || {
            for (name, token) in [("mismatch", Some("linux:1".to_string())), ("unknown", None)] {
                let listener = std::os::unix::net::UnixListener::bind(registry::socket_path(name))
                    .expect("stale socket");
                registry::write_pid(name, fixture.child.id()).expect("retained pid");
                registry::write_metadata(
                    name,
                    &SessionMetadata {
                        generation: Some(format!("retained-{name}")),
                        daemon_pid: Some(pid),
                        daemon_start_token: token,
                        daemon_boot_id: registry::read_boot_id(),
                        ..SessionMetadata::default()
                    },
                )
                .expect("retained record");
                if name == "mismatch" {
                    // No wait on the unrelated sleep: the old owner is lost.
                    assert!(remove(name).is_ok());
                    assert!(!registry::metadata_path(name).exists());
                    assert!(!registry::socket_path(name).exists());
                } else {
                    assert!(matches!(
                        remove(name),
                        Err(RemoveError::IdentityUnverified { .. })
                    ));
                    assert!(registry::metadata_path(name).exists());
                    assert!(registry::socket_path(name).exists());
                }
                assert!(fixture.child.try_wait().unwrap().is_none());
                drop(listener);
            }
        });
    }
}
