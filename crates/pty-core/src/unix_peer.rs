//! Kernel-authenticated identity for one end of a Unix-domain stream.
//!
//! Readiness controls use this instead of trusting the socket pathname: a
//! same-uid process can unlink and rebind that pathname while a client is
//! connecting. Peer credentials remain bound to the connected socket.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCredentials {
    pub pid: i32,
    pub uid: u32,
    /// Process-start identity captured immediately after accept.
    pub process_start_token: crate::proctable::LiveIdentity,
}

pub fn effective_uid() -> u32 {
    // SAFETY: geteuid(2) has no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "linux")]
pub fn credentials(stream: &UnixStream) -> Option<PeerCredentials> {
    // SAFETY: all-zero is a valid ucred output buffer; getsockopt fills every
    // field before any is observed.
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `peer` and `length` describe writable storage of the exact type
    // required by SO_PEERCRED. The stream owns a live Unix socket fd.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(peer).cast(),
            &mut length,
        )
    };
    if result != 0 || length as usize != std::mem::size_of::<libc::ucred>() {
        return None;
    }
    let row = crate::proctable::process(peer.pid).known()?;
    if row.is_zombie() || row.is_exiting() {
        return None;
    }
    Some(PeerCredentials {
        pid: peer.pid,
        uid: peer.uid,
        process_start_token: row.identity?,
    })
}

#[cfg(target_os = "macos")]
pub fn credentials(stream: &UnixStream) -> Option<PeerCredentials> {
    // Darwin's <sys/un.h>: LOCAL_PEERPID. libc does not expose the constant.
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERPID: libc::c_int = 2;
    let mut pid: libc::pid_t = 0;
    let mut pid_length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: the output pointer and length match pid_t and the fd is a live
    // Unix-domain stream.
    let pid_result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            SOL_LOCAL,
            LOCAL_PEERPID,
            std::ptr::addr_of_mut!(pid).cast(),
            &mut pid_length,
        )
    };
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: getpeereid writes the uid/gid authenticated by the local socket.
    let uid_result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if pid_result != 0
        || pid_length as usize != std::mem::size_of::<libc::pid_t>()
        || uid_result != 0
    {
        return None;
    }
    let row = crate::proctable::process(pid).known()?;
    if row.is_zombie() || row.is_exiting() {
        return None;
    }
    Some(PeerCredentials {
        pid,
        uid,
        process_start_token: row.identity?,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn credentials(_stream: &UnixStream) -> Option<PeerCredentials> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_pair_reports_the_exact_local_process() {
        let (left, right) = UnixStream::pair().unwrap();
        let row = crate::proctable::process(std::process::id() as i32)
            .known()
            .unwrap();
        let expected = PeerCredentials {
            pid: std::process::id() as i32,
            uid: effective_uid(),
            process_start_token: row.identity.unwrap(),
        };
        assert_eq!(credentials(&left), Some(expected.clone()));
        assert_eq!(credentials(&right), Some(expected));
    }
}
