//! The `--attach-stream-fd-v1` machine stream (`client.ts:415-429`, `:467-471`,
//! `:596-641`, `:493-523`): a caller-owned inherited descriptor receives the
//! daemon's GEOMETRY/SCREEN/DATA/EXIT packets re-framed exactly, in order,
//! and an empty DETACH on a local detach. The descriptor is validated but
//! never closed.

use std::io;
use std::os::unix::io::RawFd;

use pty_core::protocol::{MessageType, Packet};

use super::node_fs_error_message;
use super::tty::write_all_fd;

/// Validate a dedicated inherited descriptor without taking ownership of it
/// (`validateAttachStreamFdV1`). The error is the unprefixed message; the CLI
/// adds `pty attach: `.
pub fn validate_attach_stream_fd(fd: i64) -> Result<RawFd, String> {
    if fd < 3 || fd > i32::MAX as i64 {
        return Err(format!(
            "--attach-stream-fd-v1 requires a dedicated inherited file descriptor >= 3 (got {fd})"
        ));
    }
    let fd = fd as RawFd;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        let err = io::Error::last_os_error();
        return Err(format!(
            "--attach-stream-fd-v1 descriptor {fd} is not writable: {}",
            node_fs_error_message("fstat", &err)
        ));
    }
    if unsafe { libc::write(fd, [0u8; 0].as_ptr() as *const libc::c_void, 0) } < 0 {
        let err = io::Error::last_os_error();
        return Err(format!(
            "--attach-stream-fd-v1 descriptor {fd} is not writable: {}",
            node_fs_error_message("write", &err)
        ));
    }
    Ok(fd)
}

/// Parse the `--attach-stream-fd-v1 <fd>` token the way `Number(token)` does
/// (trimmed, empty → 0, `0x…` hex, otherwise decimal/float, else NaN) and
/// validate it. Non-integers render as JavaScript would (`NaN`, `3.5`).
pub fn parse_attach_stream_fd_token(token: &str) -> Result<RawFd, String> {
    let t = token.trim();
    let value: f64 = if t.is_empty() {
        0.0
    } else if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16)
            .map(|v| v as f64)
            .unwrap_or(f64::NAN)
    } else if t == "Infinity" || t == "+Infinity" {
        f64::INFINITY
    } else if t == "-Infinity" {
        f64::NEG_INFINITY
    } else {
        t.parse::<f64>().unwrap_or(f64::NAN)
    };
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9007199254740992.0 {
        validate_attach_stream_fd(value as i64)
    } else {
        let shown = if value.is_nan() {
            "NaN".to_string()
        } else if value.is_infinite() {
            if value > 0.0 {
                "Infinity".to_string()
            } else {
                "-Infinity".to_string()
            }
        } else {
            format!("{value}")
        };
        Err(format!(
            "--attach-stream-fd-v1 requires a dedicated inherited file descriptor >= 3 (got {shown})"
        ))
    }
}

/// Per-socket expectation: GEOMETRY first, then SCREEN, then anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Geometry,
    Screen,
    Ready,
}

/// Why the stream had to stop; the `Display` is the full stderr line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamFailure {
    /// The daemon broke the v1 ordering contract.
    Unsupported(String),
    /// Writing to the descriptor failed.
    WriteFailed { fd: RawFd, message: String },
}

impl std::fmt::Display for StreamFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamFailure::Unsupported(what) => {
                write!(
                    f,
                    "pty attach: daemon does not support attach stream v1 ({what})"
                )
            }
            StreamFailure::WriteFailed { fd, message } => {
                write!(
                    f,
                    "pty attach: machine stream descriptor {fd} failed: {message}"
                )
            }
        }
    }
}

/// The text for a close before EXIT (`client.ts:667-670`, `:683-685`).
pub fn truncated_line(detail: &str) -> String {
    format!("pty attach: machine stream truncated before EXIT: {detail}\n")
}

/// What [`MachineStream::accept`] did with a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    /// Not a stream event type; dropped.
    Skipped,
    /// Re-framed to the descriptor.
    Forwarded,
}

/// The re-framing writer over the inherited descriptor.
pub struct MachineStream {
    fd: RawFd,
    phase: Phase,
}

impl MachineStream {
    /// Wrap an already-validated descriptor.
    pub fn new(fd: RawFd) -> MachineStream {
        MachineStream {
            fd,
            phase: Phase::Geometry,
        }
    }

    /// The descriptor.
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Reset the ordering expectation (a fresh socket after a reconnect).
    pub fn reset(&mut self) {
        self.phase = Phase::Geometry;
    }

    /// Check the ordering contract and re-frame `packet` to the descriptor.
    pub fn accept(&mut self, packet: &Packet) -> Result<Accepted, StreamFailure> {
        let is_stream_event = matches!(
            packet.type_,
            MessageType::Geometry | MessageType::Screen | MessageType::Data | MessageType::Exit
        );
        if !is_stream_event {
            return Ok(Accepted::Skipped);
        }
        if self.phase == Phase::Geometry && packet.type_ != MessageType::Geometry {
            return Err(StreamFailure::Unsupported(
                "expected GEOMETRY before terminal events".to_string(),
            ));
        }
        if self.phase == Phase::Screen
            && packet.type_ != MessageType::Geometry
            && packet.type_ != MessageType::Screen
        {
            let what = if packet.type_ == MessageType::Data {
                "DATA"
            } else {
                "EXIT"
            };
            return Err(StreamFailure::Unsupported(format!(
                "expected SCREEN before {what}"
            )));
        }
        if self.phase == Phase::Geometry && packet.type_ == MessageType::Geometry {
            self.phase = Phase::Screen;
        } else if self.phase == Phase::Screen && packet.type_ == MessageType::Screen {
            self.phase = Phase::Ready;
        }
        self.write(&packet.encode())?;
        Ok(Accepted::Forwarded)
    }

    /// Write raw framed bytes (the DETACH marker on a local detach). Blocks
    /// while the reader drains — that is the backpressure: the socket is not
    /// read while we wait.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), StreamFailure> {
        write_all_fd(self.fd, bytes).map_err(|e| StreamFailure::WriteFailed {
            fd: self.fd,
            message: super::node_error_message("write", None, &e),
        })
    }
}
