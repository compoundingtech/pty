//! Machine-only readiness controls over the live daemon socket.
//!
//! node: src/client.ts `requestControlJson`, `queryAcceptedSocketOwnership`,
//! `compareAndSetLifecycle`

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use pty_core::protocol::{
    AcceptedSocketOwnershipRequest, AcceptedSocketOwnershipResult, LifecycleCompareAndSetRequest,
    LifecycleCompareAndSetResult, MessageType, PacketReader,
    encode_accepted_socket_ownership_request, encode_lifecycle_compare_and_set_request,
};
use pty_core::registry;
use pty_core::unix_peer;

use super::{ClientError, GoneSet, map_io_error};

pub const READINESS_TIMEOUT: Duration = Duration::from_secs(2);

pub fn query_accepted_socket_ownership(
    name: &str,
    request: &AcceptedSocketOwnershipRequest,
) -> Result<AcceptedSocketOwnershipResult, ClientError> {
    request_control_json(
        name,
        &request.expected_generation,
        MessageType::AcceptedSocketOwnership,
        &encode_accepted_socket_ownership_request(request),
        READINESS_TIMEOUT,
    )
}


pub fn compare_and_set_lifecycle(
    name: &str,
    request: &LifecycleCompareAndSetRequest,
) -> Result<LifecycleCompareAndSetResult, ClientError> {
    request_control_json(
        name,
        &request.expected_generation,
        MessageType::LifecycleCas,
        &encode_lifecycle_compare_and_set_request(request),
        READINESS_TIMEOUT,
    )
}


#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonPeerBinding {
    pid: i32,
    start_token: String,
}

fn verify_daemon_peer(
    name: &str,
    expected_generation: &str,
    socket: &UnixStream,
) -> Result<DaemonPeerBinding, ClientError> {
    let invalid = || ClientError::InvalidReadiness(name.to_string());
    let metadata = registry::read_metadata(name).ok_or_else(invalid)?;
    if metadata.generation.as_deref() != Some(expected_generation) {
        return Err(invalid());
    }
    let pid = metadata.daemon_pid.ok_or_else(invalid)?;
    let start_token = metadata.daemon_start_token().ok_or_else(invalid)?.to_string();
    let peer = unix_peer::credentials(socket).ok_or_else(invalid)?;
    if peer.uid != unix_peer::effective_uid()
        || peer.pid != pid
        || registry::read_process_start_token(pid).as_deref() != Some(start_token.as_str())
    {
        return Err(invalid());
    }
    Ok(DaemonPeerBinding { pid, start_token })
}

fn verify_binding_still_current(
    name: &str,
    expected_generation: &str,
    binding: &DaemonPeerBinding,
) -> Result<(), ClientError> {
    let metadata = registry::read_metadata(name)
        .ok_or_else(|| ClientError::InvalidReadiness(name.to_string()))?;
    if metadata.generation.as_deref() != Some(expected_generation)
        || metadata.daemon_pid != Some(binding.pid)
        || metadata.daemon_start_token() != Some(binding.start_token.as_str())
        || registry::read_process_start_token(binding.pid).as_deref()
            != Some(binding.start_token.as_str())
    {
        return Err(ClientError::InvalidReadiness(name.to_string()));
    }
    Ok(())
}

fn remaining_poll_ms(deadline: Instant) -> io::Result<i32> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline elapsed"));
    }
    Ok(remaining.as_millis().clamp(1, i32::MAX as u128) as i32)
}

fn wait_fd(fd: i32, events: i16, deadline: Instant) -> io::Result<()> {
    loop {
        let mut descriptor = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: `descriptor` is one initialized pollfd and the timeout is
        // bounded to i32 milliseconds.
        let result = unsafe { libc::poll(&mut descriptor, 1, remaining_poll_ms(deadline)?) };
        if result > 0 {
            if descriptor.revents & events != 0 {
                return Ok(());
            }
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "socket poll failed"));
            }
        } else if result == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline elapsed"));
        } else if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
}

fn connect_until(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let path_bytes = path.as_os_str().as_bytes();
    // SAFETY: all-zero is a valid initial sockaddr_un; family/path are filled
    // before it is passed to connect(2).
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path_bytes.is_empty()
        || path_bytes.contains(&0)
        || path_bytes.len() >= address.sun_path.len()
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid Unix socket path"));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
    }
    for (destination, source) in address.sun_path.iter_mut().zip(path_bytes) {
        *destination = *source as libc::c_char;
    }

    // SAFETY: socket(2) returns a new owned fd on success.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` was just returned by socket and ownership moves into the
    // guard exactly once.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: F_GETFL/F_SETFL operate on the live owned fd.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `address` is initialized and the full sockaddr_un is accepted
    // for filesystem Unix socket addresses on Linux and Darwin.
    let connected = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            std::ptr::addr_of!(address).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if connected != 0 {
        let error = io::Error::last_os_error();
        let in_progress = error.raw_os_error().is_some_and(|code| {
            code == libc::EINPROGRESS || code == libc::EAGAIN || code == libc::EWOULDBLOCK
        });
        if !in_progress {
            return Err(error);
        }
        wait_fd(fd.as_raw_fd(), libc::POLLOUT, deadline)?;
        let mut socket_error: libc::c_int = 0;
        let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: output storage matches SO_ERROR's c_int.
        if unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                std::ptr::addr_of_mut!(socket_error).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if socket_error != 0 {
            return Err(io::Error::from_raw_os_error(socket_error));
        }
    }
    Ok(UnixStream::from(fd))
}

fn write_all_until(socket: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        match socket.write(bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "socket closed")),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_fd(socket.as_raw_fd(), libc::POLLOUT, deadline)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}


fn request_control_json<TResult: DeserializeOwned>(
    name: &str,
    expected_generation: &str,
    expected_type: MessageType,
    packet: &[u8],
    timeout: Duration,
) -> Result<TResult, ClientError> {
    let deadline = Instant::now() + timeout;
    let path = registry::socket_path(name);
    let mut socket = connect_until(&path, deadline).map_err(|error| {
        if error.kind() == io::ErrorKind::TimedOut {
            ClientError::ReadinessTimeout(name.to_string())
        } else {
            map_io_error(name, false, GoneSet::Strict, "connect", Some(&path), &error)
        }
    })?;
    let binding = verify_daemon_peer(name, expected_generation, &socket)?;
    write_all_until(&mut socket, packet, deadline).map_err(|error| {
        if error.kind() == io::ErrorKind::TimedOut {
            ClientError::ReadinessTimeout(name.to_string())
        } else {
            map_io_error(name, false, GoneSet::Strict, "write", Some(&path), &error)
        }
    })?;
    let mut reader = PacketReader::new();
    let mut buffer = [0u8; 8192];
    loop {
        wait_fd(socket.as_raw_fd(), libc::POLLIN, deadline)
            .map_err(|_| ClientError::ReadinessTimeout(name.to_string()))?;
        match socket.read(&mut buffer) {
            Ok(0) => return Err(ClientError::ReadinessTimeout(name.to_string())),
            Ok(length) => {
                let packets = reader
                    .feed(&buffer[..length])
                    .map_err(|_| ClientError::InvalidReadiness(name.to_string()))?;
                for response in packets {
                    if response.type_ != expected_type {
                        continue;
                    }
                    let result = serde_json::from_slice(&response.payload)
                        .map_err(|_| ClientError::InvalidReadiness(name.to_string()))?;
                    verify_binding_still_current(name, expected_generation, &binding)?;
                    return Ok(result);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => {
                return Err(map_io_error(
                    name,
                    false,
                    GoneSet::Strict,
                    "read",
                    None,
                    &error,
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_control_write_obeys_the_end_to_end_deadline() {
        let (mut writer, _reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let payload = vec![0u8; 16 * 1024 * 1024];
        let started = Instant::now();
        let error = write_all_until(
            &mut writer,
            &payload,
            started + Duration::from_millis(20),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
