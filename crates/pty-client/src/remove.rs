//! Remove a session that is not running from the registry — socket, pid,
//! metadata, events, recovery revision — under the creation lock with a
//! generation check: the operation behind `pty rm`.
//!
//! node: src/cli.ts:3036-3087 (`cmdRm`)

use std::fmt;
use std::path::Path;
use std::time::Duration;

use pty_core::registry::{
    self, SessionGenerationOwner, SessionStatus, cleanup_owned_all, wait_for_process_exit,
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
    let daemon_pid = session
        .pid
        .or_else(|| session.metadata.as_ref().and_then(|m| m.daemon_pid))
        .or_else(|| registry::read_session_pid(name));
    if let Some(pid) = daemon_pid
        && !wait_for_process_exit(pid, DAEMON_EXIT_WAIT)
    {
        return Err(RemoveError::DaemonDidNotExit {
            name: name.to_string(),
        });
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

/// [`remove`] in the registry at `root` instead of `$PTY_ROOT`.
pub fn remove_in(root: &Path, name: &str) -> Result<(), RemoveError> {
    registry::with_root(root, || remove(name))
}
