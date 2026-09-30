//! Fail-closed proof that the accepted side of one held TCP connection belongs
//! to the current PTY child or an exact descendant.
//!
//! node: src/socket-ownership.ts

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::path::Path;

use pty_core::proctable::{LiveIdentity, ProcTable};
use pty_core::protocol::{AcceptedSocketOwnershipResult, TcpConnectionTuple};

// Discovery needs the complete PID topology, including unrelated rows that
// cannot prove a live identity. Only a verified root-to-owner chain can grant
// positive ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntry {
    pid: i32,
    ppid: i32,
    identity: Option<LiveIdentity>,
    zombie: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeIdentity {
    pid: i32,
    identity: LiveIdentity,
}

pub fn inspect_accepted_socket_ownership(
    root_pid: i32,
    root_identity: &LiveIdentity,
    tuple: &TcpConnectionTuple,
) -> AcceptedSocketOwnershipResult {
    inspect_with(
        root_pid,
        root_identity,
        tuple,
        ProcTable::read,
        Path::new("/proc"),
    )
}

fn inspect_with(
    root_pid: i32,
    root_identity: &LiveIdentity,
    tuple: &TcpConnectionTuple,
    read_table: impl Fn() -> ProcTable,
    proc_root: &Path,
) -> AcceptedSocketOwnershipResult {
    if let Some(reason) = validate_tuple(tuple) {
        return unavailable(reason);
    }
    let before = match tree_snapshot(root_pid, root_identity, &read_table()) {
        Ok(tree) => tree,
        Err(reason) => return unavailable(reason),
    };
    let pids: Vec<i32> = before
        .iter()
        // A zombie has already closed every descriptor. Keep it in the
        // topology snapshot, but never ask procfs for sockets it cannot own.
        .filter(|entry| !entry.zombie)
        .map(|entry| entry.pid)
        .collect();
    let observed = inspect_backend(&pids, tuple, proc_root);
    if matches!(observed, AcceptedSocketOwnershipResult::Unavailable { .. }) {
        return observed;
    }
    let middle = match tree_snapshot(root_pid, root_identity, &read_table()) {
        Ok(tree) => tree,
        Err(reason) => return unavailable(reason),
    };
    let owned_pid = match observed {
        AcceptedSocketOwnershipResult::NotOwned if before != middle => {
            return unavailable("process-tree-changed");
        }
        AcceptedSocketOwnershipResult::NotOwned => {
            if let Some(pid) = before
                .iter()
                .find(|entry| !entry.zombie && entry.identity.is_none())
                .map(|entry| entry.pid)
            {
                return unavailable(format!("process-identity-unavailable:{pid}"));
            }
            return observed;
        }
        AcceptedSocketOwnershipResult::Owned { pid } => pid,
        AcceptedSocketOwnershipResult::Unavailable { .. } => unreachable!(),
    };
    let owner_chain = match verified_chain(root_pid, owned_pid, &before) {
        Ok(chain) => chain,
        Err(reason) => return unavailable(reason),
    };
    match verified_chain(root_pid, owned_pid, &middle) {
        Ok(chain) if chain == owner_chain => {}
        Ok(_) => return unavailable("process-tree-changed"),
        Err(reason) => return unavailable(reason),
    }
    let confirmed = inspect_backend(&[owned_pid], tuple, proc_root);
    if confirmed != (AcceptedSocketOwnershipResult::Owned { pid: owned_pid }) {
        return unavailable("socket-ownership-changed");
    }
    let after = match tree_snapshot(root_pid, root_identity, &read_table()) {
        Ok(tree) => tree,
        Err(reason) => return unavailable(reason),
    };
    match verified_chain(root_pid, owned_pid, &after) {
        Ok(chain) if chain == owner_chain => confirmed,
        Ok(_) => unavailable("process-tree-changed"),
        Err(reason) => unavailable(reason),
    }
}

fn unavailable(reason: impl Into<String>) -> AcceptedSocketOwnershipResult {
    AcceptedSocketOwnershipResult::Unavailable {
        reason: reason.into(),
    }
}

fn validate_tuple(tuple: &TcpConnectionTuple) -> Option<&'static str> {
    if tuple.local_address.contains('%') || tuple.remote_address.contains('%') {
        return Some("scoped-ipv6-unavailable");
    }
    let local: IpAddr = match tuple.local_address.parse() {
        Ok(address) => address,
        Err(_) => return Some("invalid-local-address"),
    };
    let remote: IpAddr = match tuple.remote_address.parse() {
        Ok(address) => address,
        Err(_) => return Some("invalid-remote-address"),
    };
    if std::mem::discriminant(&local) != std::mem::discriminant(&remote) {
        return Some("address-family-mismatch");
    }
    if tuple.local_port == 0 {
        return Some("invalid-local-port");
    }
    if tuple.remote_port == 0 {
        return Some("invalid-remote-port");
    }
    None
}

fn tree_snapshot(
    root_pid: i32,
    root_identity: &LiveIdentity,
    table: &ProcTable,
) -> Result<Vec<TreeEntry>, String> {
    if !table.is_readable() {
        return Err("process-table-unknown".to_string());
    }
    let mut by_pid = HashMap::new();
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for row in table.rows() {
        by_pid.insert(row.pid, row);
        children.entry(row.ppid).or_default().push(row.pid);
    }
    let Some(root) = by_pid.get(&root_pid) else {
        return Err("child-identity-unavailable".to_string());
    };
    if root.is_zombie() || root.identity.as_ref() != Some(root_identity) {
        return Err("child-identity-unavailable".to_string());
    }
    let mut queue = VecDeque::from([root_pid]);
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        let Some(row) = by_pid.get(&pid) else {
            return Err("process-tree-changed".to_string());
        };
        entries.push(TreeEntry {
            pid,
            ppid: row.ppid,
            identity: row.identity.clone(),
            zombie: row.is_zombie(),
        });
        queue.extend(children.get(&pid).into_iter().flatten().copied());
    }
    entries.sort_by_key(|entry| entry.pid);
    Ok(entries)
}

// Reconstruct the security-relevant ancestry and require every member to be a
// live, start-identified process. Comparing this chain around both socket
// lookups fences owner reuse, reparenting, and root-generation changes without
// making unrelated descendants part of the authority proof.
fn verified_chain(
    root_pid: i32,
    owner_pid: i32,
    tree: &[TreeEntry],
) -> Result<Vec<TreeIdentity>, String> {
    let by_pid: HashMap<i32, &TreeEntry> = tree.iter().map(|entry| (entry.pid, entry)).collect();
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut pid = owner_pid;
    loop {
        if !seen.insert(pid) {
            return Err("process-tree-changed".to_string());
        }
        let Some(entry) = by_pid.get(&pid) else {
            return Err(if pid == owner_pid {
                "backend-returned-non-descendant".to_string()
            } else {
                "process-tree-changed".to_string()
            });
        };
        let Some(identity) = entry.identity.clone().filter(|_| !entry.zombie) else {
            return Err(format!("process-identity-unavailable:{pid}"));
        };
        chain.push(TreeIdentity { pid, identity });
        if pid == root_pid {
            chain.reverse();
            return Ok(chain);
        }
        pid = entry.ppid;
    }
}

fn inspect_backend(
    pids: &[i32],
    tuple: &TcpConnectionTuple,
    proc_root: &Path,
) -> AcceptedSocketOwnershipResult {
    #[cfg(target_os = "linux")]
    {
        inspect_linux(pids, tuple, proc_root)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = proc_root;
        inspect_darwin(pids, tuple)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (pids, tuple, proc_root);
        unavailable(concat!("unsupported-platform:", std::env::consts::OS))
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct LinuxTcpRow {
    local_address: String,
    local_port: String,
    remote_address: String,
    remote_port: String,
    state: String,
    inode: String,
}

#[cfg(target_os = "linux")]
fn inspect_linux(
    pids: &[i32],
    tuple: &TcpConnectionTuple,
    proc_root: &Path,
) -> AcceptedSocketOwnershipResult {
    let Ok(local_ip) = tuple.remote_address.parse::<IpAddr>() else {
        return unavailable("address-encoding-failed");
    };
    let Ok(remote_ip) = tuple.local_address.parse::<IpAddr>() else {
        return unavailable("address-encoding-failed");
    };
    let expected_local = encode_linux_proc_address(local_ip);
    let expected_remote = encode_linux_proc_address(remote_ip);
    let mapped = match (local_ip, remote_ip) {
        (IpAddr::V4(local), IpAddr::V4(remote)) => Some((
            encode_linux_proc_address(local.to_ipv6_mapped().into()),
            encode_linux_proc_address(remote.to_ipv6_mapped().into()),
        )),
        _ => None,
    };
    let table_name = if local_ip.is_ipv4() { "tcp" } else { "tcp6" };
    let local_port = format!("{:04X}", tuple.remote_port);
    let remote_port = format!("{:04X}", tuple.local_port);
    for pid in pids {
        let base = proc_root.join(pid.to_string());
        let table = match std::fs::read_to_string(base.join("net").join(table_name)) {
            Ok(table) => table,
            Err(_) => return unavailable(format!("procfs-unreadable:{pid}")),
        };
        let fds = match std::fs::read_dir(base.join("fd")) {
            Ok(fds) => fds,
            Err(_) => return unavailable(format!("procfs-unreadable:{pid}")),
        };
        let Some(rows) = parse_linux_tcp_table(&table) else {
            return unavailable(format!("socket-table-invalid:{pid}"));
        };
        let mut inodes = HashSet::new();
        let mut collect_matches = |rows: Vec<LinuxTcpRow>, local: &str, remote: &str| {
            inodes.extend(
                rows.into_iter()
                    .filter(|row| {
                        row.state == "01"
                            && row.local_address == local
                            && row.local_port == local_port
                            && row.remote_address == remote
                            && row.remote_port == remote_port
                    })
                    .map(|row| row.inode),
            );
        };
        collect_matches(rows, &expected_local, &expected_remote);
        if let Some((mapped_local, mapped_remote)) = &mapped {
            // IPv4 peers of a dual-stack listener appear only in tcp6.
            // An absent tcp6 table is normal on IPv6-disabled kernels.
            if let Ok(table) = std::fs::read_to_string(base.join("net/tcp6")) {
                let Some(rows) = parse_linux_tcp_table(&table) else {
                    return unavailable(format!("socket-table-invalid:{pid}"));
                };
                collect_matches(rows, mapped_local, mapped_remote);
            }
        }
        if inodes.is_empty() {
            continue;
        }
        for fd in fds {
            let Ok(fd) = fd else {
                return unavailable(format!("fd-table-unreadable:{pid}"));
            };
            let target = match std::fs::read_link(fd.path()) {
                Ok(target) => target,
                // Descriptor tables are live: an unrelated descriptor may
                // close after readdir returned its name. The vanished entry
                // cannot own the still-established tuple, so skip it while
                // retaining fail-closed handling for every other read error.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return unavailable(format!("fd-table-unreadable:{pid}")),
            };
            let target = target.to_string_lossy();
            if let Some(inode) = target
                .strip_prefix("socket:[")
                .and_then(|value| value.strip_suffix(']'))
                && inodes.contains(inode)
            {
                return AcceptedSocketOwnershipResult::Owned { pid: *pid };
            }
        }
    }
    AcceptedSocketOwnershipResult::NotOwned
}

#[cfg(target_os = "linux")]
fn parse_linux_tcp_table(input: &str) -> Option<Vec<LinuxTcpRow>> {
    let mut lines = input.lines();
    if !lines.next()?.contains("local_address") {
        return None;
    }
    let mut rows = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 {
            return None;
        }
        let (local_address, local_port) = parse_proc_endpoint(fields[1])?;
        let (remote_address, remote_port) = parse_proc_endpoint(fields[2])?;
        if fields[3].len() != 2
            || !fields[3].bytes().all(|byte| byte.is_ascii_hexdigit())
            || !fields[9].bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        rows.push(LinuxTcpRow {
            local_address,
            local_port,
            remote_address,
            remote_port,
            state: fields[3].to_ascii_uppercase(),
            inode: fields[9].to_string(),
        });
    }
    Some(rows)
}

#[cfg(target_os = "linux")]
fn parse_proc_endpoint(value: &str) -> Option<(String, String)> {
    let (address, port) = value.split_once(':')?;
    if address.is_empty()
        || !address.bytes().all(|byte| byte.is_ascii_hexdigit())
        || port.len() != 4
        || !port.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some((address.to_ascii_uppercase(), port.to_ascii_uppercase()))
}

#[cfg(target_os = "linux")]
fn encode_linux_proc_address(address: IpAddr) -> String {
    let bytes: Vec<u8> = match address {
        IpAddr::V4(address) => address.octets().to_vec(),
        IpAddr::V6(address) => address.octets().to_vec(),
    };
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for word in bytes.chunks_exact(4) {
        #[cfg(target_endian = "little")]
        let word = [word[3], word[2], word[1], word[0]];
        #[cfg(target_endian = "big")]
        let word = [word[0], word[1], word[2], word[3]];
        for byte in word {
            use std::fmt::Write as _;
            let _ = write!(encoded, "{byte:02X}");
        }
    }
    encoded
}

#[cfg(target_os = "macos")]
fn inspect_darwin(pids: &[i32], tuple: &TcpConnectionTuple) -> AcceptedSocketOwnershipResult {
    use std::ffi::CString;

    unsafe extern "C" {
        fn pty_inspect_socket_owner_darwin(
            local_address: *const libc::c_char,
            local_port: u16,
            foreign_address: *const libc::c_char,
            foreign_port: u16,
            pids: *const i32,
            count: usize,
        ) -> i32;
    }

    let Ok(local) = CString::new(tuple.remote_address.as_str()) else {
        return unavailable("darwin-table-unavailable");
    };
    let Ok(foreign) = CString::new(tuple.local_address.as_str()) else {
        return unavailable("darwin-table-unavailable");
    };
    // SAFETY: the C boundary accepts read-only NUL-terminated addresses and a
    // read-only PID slice for exactly this call. It owns no Rust memory.
    let result = unsafe {
        pty_inspect_socket_owner_darwin(
            local.as_ptr(),
            tuple.remote_port,
            foreign.as_ptr(),
            tuple.local_port,
            pids.as_ptr(),
            pids.len(),
        )
    };
    if result > 0 && pids.contains(&result) {
        AcceptedSocketOwnershipResult::Owned { pid: result }
    } else if result == 0 {
        AcceptedSocketOwnershipResult::NotOwned
    } else {
        unavailable("darwin-table-unavailable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_addresses_match_linux_tables() {
        assert_eq!(
            encode_linux_proc_address("127.0.0.1".parse().unwrap()),
            if cfg!(target_endian = "little") {
                "0100007F"
            } else {
                "7F000001"
            }
        );
        assert!(parse_linux_tcp_table("sl local_address\n0: broken").is_none());
    }

    #[test]
    fn scoped_ipv6_fails_closed() {
        let result = inspect_accepted_socket_ownership(
            std::process::id() as i32,
            &LiveIdentity::new("owner"),
            &TcpConnectionTuple {
                local_address: "fe80::1%eth0".to_string(),
                local_port: 51_000,
                remote_address: "fe80::2%eth0".to_string(),
                remote_port: 3_000,
            },
        );
        assert_eq!(
            result,
            AcceptedSocketOwnershipResult::Unavailable {
                reason: "scoped-ipv6-unavailable".to_string()
            }
        );
    }
    #[test]
    fn real_accepted_loopback_socket_is_owned_by_the_exact_process() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let server_address = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(server_address).unwrap();
        let client_address = client.local_addr().unwrap();
        let (_accepted, _) = listener.accept().unwrap();
        let tuple = TcpConnectionTuple {
            local_address: client_address.ip().to_string(),
            local_port: client_address.port(),
            remote_address: server_address.ip().to_string(),
            remote_port: server_address.port(),
        };
        let pid = std::process::id() as i32;
        // Keep the ancestry oracle deterministic: other tests in this binary
        // may concurrently spawn descendants whose identities legitimately
        // fail closed. The socket and descriptor tables remain the real
        // kernel observations under test.
        assert_eq!(
            inspect_with(
                pid,
                &LiveIdentity::new("owner"),
                &tuple,
                || pty_core::proctable::table_from_shape(&format!("{pid} 1 {pid} S owner")),
                Path::new("/proc"),
            ),
            AcceptedSocketOwnershipResult::Owned { pid }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dual_stack_accepted_ipv4_socket_is_owned() {
        let Ok(listener) = std::net::TcpListener::bind("[::]:0") else {
            return; // IPv6 unavailable.
        };
        let port = listener.local_addr().unwrap().port();
        let Ok(client) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
            return; // This kernel defaults to IPV6_V6ONLY.
        };
        let client_address = client.local_addr().unwrap();
        let (_accepted, _) = listener.accept().unwrap();
        let tuple = TcpConnectionTuple {
            local_address: client_address.ip().to_string(),
            local_port: client_address.port(),
            remote_address: "127.0.0.1".to_string(),
            remote_port: port,
        };
        let pid = std::process::id() as i32;
        assert_eq!(
            inspect_with(
                pid,
                &LiveIdentity::new("owner"),
                &tuple,
                || pty_core::proctable::table_from_shape(&format!("{pid} 1 {pid} S owner")),
                Path::new("/proc"),
            ),
            AcceptedSocketOwnershipResult::Owned { pid }
        );
    }

    #[test]
    fn unrelated_descendant_churn_keeps_a_stable_socket_owner_valid() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let server_address = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(server_address).unwrap();
        let client_address = client.local_addr().unwrap();
        let (_accepted, _) = listener.accept().unwrap();
        let tuple = TcpConnectionTuple {
            local_address: client_address.ip().to_string(),
            local_port: client_address.port(),
            remote_address: server_address.ip().to_string(),
            remote_port: server_address.port(),
        };
        let reads = AtomicUsize::new(0);
        let pid = std::process::id() as i32;
        let helper_pid = pid + 1_000_000;
        let result = inspect_with(
            pid,
            &LiveIdentity::new("owner"),
            &tuple,
            || {
                if reads.fetch_add(1, Ordering::SeqCst) == 1 {
                    pty_core::proctable::table_from_shape(&format!(
                        "{pid} 1 {pid} S owner\n{helper_pid} {pid} {pid} S helper"
                    ))
                } else {
                    pty_core::proctable::table_from_shape(&format!("{pid} 1 {pid} S owner"))
                }
            },
            Path::new("/proc"),
        );
        assert_eq!(result, AcceptedSocketOwnershipResult::Owned { pid });
    }

    #[test]
    fn unrelated_unverifiable_descendants_keep_a_stable_socket_owner_valid() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let server_address = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(server_address).unwrap();
        let client_address = client.local_addr().unwrap();
        let (_accepted, _) = listener.accept().unwrap();
        let tuple = TcpConnectionTuple {
            local_address: client_address.ip().to_string(),
            local_port: client_address.port(),
            remote_address: server_address.ip().to_string(),
            remote_port: server_address.port(),
        };
        let pid = std::process::id() as i32;
        let zombie_pid = pid + 1_000_000;
        let identityless_pid = pid + 2_000_000;
        let result = inspect_with(
            pid,
            &LiveIdentity::new("owner"),
            &tuple,
            || {
                pty_core::proctable::table_from_shape(&format!(
                    "{pid} 1 {pid} S owner\n\
                     {zombie_pid} {pid} {pid} Z zombie\n\
                     {identityless_pid} {pid} {pid} S -"
                ))
            },
            Path::new("/proc"),
        );
        assert_eq!(result, AcceptedSocketOwnershipResult::Owned { pid });
    }

    #[test]
    fn unverifiable_socket_owner_ancestor_fails_closed() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let server_address = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(server_address).unwrap();
        let client_address = client.local_addr().unwrap();
        let (_accepted, _) = listener.accept().unwrap();
        let tuple = TcpConnectionTuple {
            local_address: client_address.ip().to_string(),
            local_port: client_address.port(),
            remote_address: server_address.ip().to_string(),
            remote_port: server_address.port(),
        };
        let owner_pid = std::process::id() as i32;
        let ancestor_pid = owner_pid + 1_000_000;
        let root_pid = owner_pid + 2_000_000;
        for ancestor_state in ["Z ancestor", "S -"] {
            let result = inspect_with(
                root_pid,
                &LiveIdentity::new("root"),
                &tuple,
                || {
                    pty_core::proctable::table_from_shape(&format!(
                        "{root_pid} 1 {root_pid} S root\n\
                         {ancestor_pid} {root_pid} {root_pid} {ancestor_state}\n\
                         {owner_pid} {ancestor_pid} {root_pid} S owner"
                    ))
                },
                Path::new("/proc"),
            );
            assert_eq!(
                result,
                AcceptedSocketOwnershipResult::Unavailable {
                    reason: format!("process-identity-unavailable:{ancestor_pid}")
                }
            );
        }
    }

    #[test]
    fn new_descendant_during_negative_lookup_is_not_definitive() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let server_address = listener.local_addr().unwrap();
        let tuple = TcpConnectionTuple {
            local_address: server_address.ip().to_string(),
            local_port: 1,
            remote_address: server_address.ip().to_string(),
            remote_port: server_address.port(),
        };
        let reads = AtomicUsize::new(0);
        let pid = std::process::id() as i32;
        let helper_pid = pid + 1_000_000;
        let result = inspect_with(
            pid,
            &LiveIdentity::new("owner"),
            &tuple,
            || {
                if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                    pty_core::proctable::table_from_shape(&format!("{pid} 1 {pid} S owner"))
                } else {
                    pty_core::proctable::table_from_shape(&format!(
                        "{pid} 1 {pid} S owner\n{helper_pid} {pid} {pid} S helper"
                    ))
                }
            },
            Path::new("/proc"),
        );
        assert_eq!(
            result,
            AcceptedSocketOwnershipResult::Unavailable {
                reason: "process-tree-changed".to_string()
            }
        );
    }

    #[test]
    fn identityless_live_descendant_keeps_a_negative_lookup_fail_closed() {
        use std::process::{Command, Stdio};

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let server_address = listener.local_addr().unwrap();
        let tuple = TcpConnectionTuple {
            local_address: server_address.ip().to_string(),
            local_port: 1,
            remote_address: server_address.ip().to_string(),
            remote_port: server_address.port(),
        };
        let pid = std::process::id() as i32;
        let mut helper = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let helper_pid = helper.id() as i32;
        let result = inspect_with(
            pid,
            &LiveIdentity::new("owner"),
            &tuple,
            || {
                pty_core::proctable::table_from_shape(&format!(
                    "{pid} 1 {pid} S owner\n{helper_pid} {pid} {pid} S -"
                ))
            },
            Path::new("/proc"),
        );
        let _ = helper.kill();
        let _ = helper.wait();
        assert_eq!(
            result,
            AcceptedSocketOwnershipResult::Unavailable {
                reason: format!("process-identity-unavailable:{helper_pid}")
            }
        );
    }

    #[test]
    fn process_identity_change_during_socket_proof_fails_closed() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let server_address = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(server_address).unwrap();
        let client_address = client.local_addr().unwrap();
        let (_accepted, _) = listener.accept().unwrap();
        let tuple = TcpConnectionTuple {
            local_address: client_address.ip().to_string(),
            local_port: client_address.port(),
            remote_address: server_address.ip().to_string(),
            remote_port: server_address.port(),
        };
        let reads = AtomicUsize::new(0);
        let pid = std::process::id() as i32;
        let result = inspect_with(
            pid,
            &LiveIdentity::new("before"),
            &tuple,
            || {
                let identity = if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                    "before"
                } else {
                    "after"
                };
                pty_core::proctable::table_from_shape(&format!("{pid} 1 {pid} S {identity}"))
            },
            Path::new("/proc"),
        );
        assert_eq!(
            result,
            AcceptedSocketOwnershipResult::Unavailable {
                reason: "child-identity-unavailable".to_string()
            }
        );
    }
}
