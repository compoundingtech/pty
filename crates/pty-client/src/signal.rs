//! Signal the program a session runs: its terminal child and that child's
//! process group, never the daemon that hosts it.
//!
//! `pty list` reports the daemon's pid. Signalling that pid makes the
//! registry entry disappear and leaves the program running, so a caller that
//! wants the program to get a signal has to resolve the child through the
//! daemon first. This does that, and fences each step against the session
//! having been replaced in between.

use std::fmt;
use std::io;
use std::path::Path;

use pty_core::registry;

use crate::{ClientError, query_stats};

/// Why the signal was not delivered.
#[derive(Debug)]
pub enum SignalError {
    NotFound {
        name: String,
    },
    /// The caller's generation is not the session's any more.
    GenerationChanged {
        name: String,
        expected: String,
        actual: Option<String>,
    },
    /// No live daemon is bound to the session's record: it is not running,
    /// or its recorded pid no longer proves to be the daemon that published
    /// it.
    NotRunning {
        name: String,
    },
    /// The daemon did not answer the STATUS query.
    Status(ClientError),
    /// The socket answered for a different daemon than the one the record
    /// binds: the session was replaced between the two reads.
    DaemonChanged {
        name: String,
        bound: i32,
        answered: i32,
    },
    /// The daemon says its child has exited.
    ChildNotRunning {
        name: String,
    },
    /// `kill(2)` refused the signal (an invalid signal number, a permission
    /// failure).
    Os {
        name: String,
        error: io::Error,
    },
}

impl fmt::Display for SignalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignalError::NotFound { name } => write!(f, "Session \"{name}\" not found."),
            SignalError::GenerationChanged {
                name,
                expected,
                actual,
            } => match actual {
                Some(actual) => write!(
                    f,
                    "Session \"{name}\" is generation {actual}, not {expected}: it was replaced."
                ),
                None => write!(
                    f,
                    "Session \"{name}\" has no generation, not {expected}: it was replaced."
                ),
            },
            SignalError::NotRunning { name } => write!(f, "Session \"{name}\" is not running."),
            SignalError::Status(error) => error.fmt(f),
            SignalError::DaemonChanged {
                name,
                bound,
                answered,
            } => write!(
                f,
                "Session \"{name}\" is answered by daemon {answered}, not {bound}: it was replaced."
            ),
            SignalError::ChildNotRunning { name } => {
                write!(f, "Session \"{name}\" has no running process to signal.")
            }
            SignalError::Os { name, error } => {
                write!(f, "Could not signal session \"{name}\": {error}")
            }
        }
    }
}

impl std::error::Error for SignalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SignalError::Status(error) => Some(error),
            SignalError::Os { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Where the signal went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// To the child's process group.
    Group,
    /// The group could not be signalled, so to the child alone.
    Process,
    /// The child exited between the daemon's answer and the signal.
    Gone,
}

/// A signal sent to a session's program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signalled {
    /// The terminal child's pid, as its daemon reported it.
    pub pid: i32,
    pub delivery: Delivery,
}

/// Send `signal` to the program the session `name` runs, under `$PTY_ROOT`.
///
/// With `expected_generation`, the session's record must still carry that
/// generation. Either way, the daemon must be the one the record binds by
/// start identity, and the daemon that answers the STATUS query must be that
/// same process. Only then is the child it reports signalled: its process
/// group first, the child alone when the group cannot be.
///
/// One window remains. The child can exit and be reaped after the daemon
/// answers and before the signal lands, and its pid can then in principle
/// be reused. That window is one socket round trip.
pub fn signal(
    name: &str,
    signal: i32,
    expected_generation: Option<&str>,
) -> Result<Signalled, SignalError> {
    let Some(metadata) = registry::read_metadata(name) else {
        return Err(SignalError::NotFound {
            name: name.to_string(),
        });
    };
    if let Some(expected) = expected_generation
        && metadata.generation.as_deref() != Some(expected)
    {
        return Err(SignalError::GenerationChanged {
            name: name.to_string(),
            expected: expected.to_string(),
            actual: metadata.generation.clone(),
        });
    }
    let Some(bound) = registry::read_signal_target_with(name, Some(&metadata)) else {
        return Err(SignalError::NotRunning {
            name: name.to_string(),
        });
    };
    let stats = query_stats(name).map_err(SignalError::Status)?;
    if stats.daemon.pid != bound {
        return Err(SignalError::DaemonChanged {
            name: name.to_string(),
            bound,
            answered: stats.daemon.pid,
        });
    }
    let (true, Some(pid)) = (stats.process.alive, stats.process.pid) else {
        return Err(SignalError::ChildNotRunning {
            name: name.to_string(),
        });
    };
    // SAFETY: kill(2) on a pid the session's own daemon reported, after the
    // fences above.
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(Signalled {
            pid,
            delivery: Delivery::Group,
        });
    }
    // SAFETY: as above.
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(Signalled {
            pid,
            delivery: Delivery::Process,
        });
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(Signalled {
            pid,
            delivery: Delivery::Gone,
        });
    }
    Err(SignalError::Os {
        name: name.to_string(),
        error,
    })
}

/// [`signal`] in the registry at `root` instead of `$PTY_ROOT`.
pub fn signal_in(
    root: &Path,
    name: &str,
    signal: i32,
    expected_generation: Option<&str>,
) -> Result<Signalled, SignalError> {
    registry::with_root(root, || self::signal(name, signal, expected_generation))
}
