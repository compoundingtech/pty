//! Bounded cross-binary lifecycle and resource-safety soak. Every wait is on
//! an observable publication, detach, process-exit, or file-removal handshake;
//! elapsed time is never used as proof of correctness.

use pty_conformance::*;
use pty_core::protocol::MessageType;
use std::ffi::CString;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

const RAPID_EXIT_CYCLES: usize = 12;
const ATTACHED_DETACH_CYCLES: usize = 6;
const FD_CYCLES: usize = 32;

fn operation_budget() -> Duration {
    if unoptimized_binary_under_test() {
        deadline() * 2
    } else {
        deadline()
    }
}

fn session_entries(rig: &Rig, id: &str) -> Vec<String> {
    let mut entries: Vec<String> = std::fs::read_dir(rig.root())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(id))
        .collect();
    entries.sort();
    entries
}

fn wait_for_session_entries_gone(rig: &Rig, id: &str) {
    wait_until_for(
        &format!("all registry entries for {id} to be removed"),
        operation_budget(),
        &mut || session_entries(rig, id).is_empty(),
    );
}

fn read_observed_pid(path: &Path, what: &str) -> i32 {
    wait_until_for(what, operation_budget(), &mut || {
        std::fs::metadata(path)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    });
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("invalid daemon pid in {}: {e}", path.display()))
}

fn make_fifo(path: &Path) {
    let path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: `path` is a valid NUL-terminated pathname and the mode is an
    // ordinary owner-readable/writable FIFO permission set.
    let rc = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());
}
fn release_fifo(path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;

    let mut writer = None;
    wait_until_for(
        &format!("child reader to open {}", path.display()),
        operation_budget(),
        &mut || match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => {
                writer = Some(file);
                true
            }
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => false,
            Err(e) => panic!("open detach gate {}: {e}", path.display()),
        },
    );
    writer
        .expect("FIFO writer after successful open")
        .write_all(b"exit\n")
        .unwrap_or_else(|e| panic!("release detach gate {}: {e}", path.display()));
}

fn assert_no_registry_debris(rig: &Rig) {
    let mut debris: Vec<String> = std::fs::read_dir(rig.root())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.ends_with(".json")
                || name.ends_with(".jsonl")
                || name.ends_with(".sock")
                || name.ends_with(".pid")
                || name.ends_with(".lock")
                || name.contains(".tmp.")
        })
        .collect();
    debris.sort();
    assert!(debris.is_empty(), "registry debris after soak: {debris:?}");
    assert!(
        rig.list_json().is_empty(),
        "registry still lists sessions after soak"
    );
}

#[test]
fn rapid_exit_and_attached_detach_soak_leaves_no_resources() {
    let rig = Rig::new();
    let observations = rig.make_dir("soak-observations");
    let mut daemon_pids = Vec::with_capacity(RAPID_EXIT_CYCLES + ATTACHED_DETACH_CYCLES);

    for cycle in 0..RAPID_EXIT_CYCLES {
        let id = unique_id("sx");
        let observed = observations.join(format!("rapid-{cycle}.pid"));
        let opts = DaemonOpts::no_display_name()
            .ephemeral()
            .with_env("SOAK_OBSERVED_PID", observed.to_str().unwrap());
        let daemon = rig.daemon_try(
            &id,
            &[
                "sh",
                "-c",
                "cat \"$PTY_ROOT/$PTY_SESSION.pid\" > \"$SOAK_OBSERVED_PID\"",
            ],
            opts,
        );
        expect_status(&daemon.launch, 0);
        let pid = read_observed_pid(
            &observed,
            &format!("rapid cycle {cycle} to publish its daemon pid"),
        );
        daemon_pids.push(pid);
        wait_for_process_gone(pid);
        wait_for_session_entries_gone(&rig, &id);
        std::fs::remove_file(&observed).unwrap();
    }

    for cycle in 0..ATTACHED_DETACH_CYCLES {
        let id = unique_id("sd");
        let gate = observations.join(format!("detach-{cycle}.fifo"));
        make_fifo(&gate);
        let marker = format!("SOAK_READY_{cycle}");
        let gate_arg = format!("SOAK_GATE={}", gate.display());
        let marker_arg = format!("SOAK_MARKER={marker}");
        let args = [
            "run",
            "-e",
            "--id",
            id.as_str(),
            "--no-display-name",
            "--env",
            gate_arg.as_str(),
            "--env",
            marker_arg.as_str(),
            "--",
            "sh",
            "-c",
            "printf '%s\\n' \"$SOAK_MARKER\"; IFS= read -r _ < \"$SOAK_GATE\"",
        ];
        let mut attached = rig.pty_tty_raw(&[], &[], &args, 24, 80);
        assert!(
            attached.wait_for_text(&marker, operation_budget()),
            "cycle {cycle} child never reached the detach gate: {:?}",
            attached.output_str()
        );
        let pid = read_pid_file(&rig.pid_path(&id))
            .unwrap_or_else(|| panic!("cycle {cycle} missing daemon pid"));
        daemon_pids.push(pid);

        attached.write(&[0x1c]);
        let detach_status = attached.wait_exit(operation_budget()).unwrap_or_else(|| {
            panic!(
                "cycle {cycle} attached client did not detach: {:?}",
                attached.output_str()
            )
        });
        assert_eq!(
            detach_status,
            0,
            "cycle {cycle}: {:?}",
            attached.output_str()
        );

        release_fifo(&gate);
        wait_for_process_gone(pid);
        wait_for_session_entries_gone(&rig, &id);
        std::fs::remove_file(&gate).unwrap();
    }

    for pid in daemon_pids {
        assert!(
            pty_core::registry::has_process_exited_for_reap(pid),
            "daemon {pid} from the soak is still running"
        );
    }
    assert_no_registry_debris(&rig);
    std::fs::remove_dir(&observations).unwrap();
}

#[cfg(target_os = "linux")]
fn fd_count(pid: i32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap_or_else(|e| panic!("read /proc/{pid}/fd: {e}"))
        .count()
}

/// Each open descriptor and what it points at, to name one that leaked.
#[cfg(target_os = "linux")]
fn fd_table(pid: i32) -> std::collections::BTreeMap<u32, String> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap_or_else(|e| panic!("read /proc/{pid}/fd: {e}"))
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let fd = entry.file_name().to_str()?.parse().ok()?;
            // A descriptor can close between the listing and the read.
            let target = std::fs::read_link(entry.path())
                .map(|path| path.display().to_string())
                .unwrap_or_else(|e| format!("<{e}>"));
            Some((fd, target))
        })
        .collect()
}

/// How many more descriptors of each kind `after` holds than `before`.
/// Descriptor numbers and inodes are reused across attach cycles, so naming the
/// kind (`socket`, `pipe`, `/dev/ptmx`) is what says which one leaked.
#[cfg(target_os = "linux")]
fn fd_kinds_over(
    before: &std::collections::BTreeMap<u32, String>,
    after: &std::collections::BTreeMap<u32, String>,
) -> std::collections::BTreeMap<String, isize> {
    fn kinds(table: &std::collections::BTreeMap<u32, String>) -> std::collections::BTreeMap<&str, isize> {
        let mut kinds = std::collections::BTreeMap::new();
        for target in table.values() {
            let kind = target.split(":[").next().unwrap_or(target);
            *kinds.entry(kind).or_insert(0) += 1;
        }
        kinds
    }
    let before = kinds(before);
    kinds(after)
        .into_iter()
        .filter_map(|(kind, n)| {
            let more = n - before.get(kind).copied().unwrap_or(0);
            (more > 0).then(|| (kind.to_string(), more))
        })
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn fd_kinds_over_names_what_was_added_and_ignores_reused_numbers() {
    let table = |entries: &[(u32, &str)]| {
        entries.iter().map(|(fd, target)| (*fd, target.to_string())).collect()
    };
    let before = table(&[(0, "/dev/null"), (3, "/dev/ptmx"), (5, "socket:[10]"), (6, "socket:[11]")]);
    // fd 6 now holds a different socket, which is a reused slot and not a leak.
    let reused = table(&[(0, "/dev/null"), (3, "/dev/ptmx"), (5, "socket:[10]"), (6, "socket:[99]")]);
    assert!(fd_kinds_over(&before, &reused).is_empty());
    // An extra socket and an extra pipe are named by kind; a closed fd is not negative.
    let leaked = table(&[(0, "/dev/null"), (5, "socket:[10]"), (6, "socket:[99]"), (7, "socket:[12]"), (8, "pipe:[13]")]);
    let over = fd_kinds_over(&before, &leaked);
    assert_eq!(over.get("socket"), Some(&1));
    assert_eq!(over.get("pipe"), Some(&1));
    assert_eq!(over.len(), 2, "{over:?}");
}

fn attach_then_detach(rig: &Rig, id: &str) {
    let mut conn = rig.connect(id);
    conn.attach(24, 80);
    conn.wait_for(MessageType::Screen, operation_budget())
        .unwrap_or_else(|| panic!("{id}: daemon did not answer ATTACH with SCREEN"));
    conn.detach();
    wait_until_for(
        &format!("{id}: daemon to acknowledge DETACH by closing the socket"),
        operation_budget(),
        &mut || {
            let _ = conn.next_packet(Duration::from_millis(20));
            conn.is_eof()
        },
    );
}

#[test]
fn repeated_attach_detach_keeps_daemon_fd_count_stable() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP: daemon fd stability requires Linux /proc/<pid>/fd");
        return;
    }

    #[cfg(target_os = "linux")]
    {
        let rig = Rig::new();
        let id = unique_id("fd");
        let daemon = rig.daemon(&id, &["cat"], DaemonOpts::no_display_name().ephemeral());
        let pid = daemon.pid();

        attach_then_detach(&rig, &id);
        let baseline_fds = fd_table(pid);
        let baseline = baseline_fds.len();
        for _ in 0..FD_CYCLES {
            attach_then_detach(&rig, &id);
        }

        wait_until_for(
            "daemon fd count to return to its warmed baseline",
            operation_budget(),
            &mut || fd_count(pid) <= baseline,
        );
        let final_fds = fd_table(pid);
        let final_count = final_fds.len();
        assert!(
            final_count <= baseline,
            "daemon {pid} leaked fds across {FD_CYCLES} attach/detach cycles: baseline={baseline}, final={final_count}; \
             more of: {:?}; baseline table: {baseline_fds:?}; final table: {final_fds:?}",
            fd_kinds_over(&baseline_fds, &final_fds)
        );

        let stopped = rig.pty(&["kill", &id]);
        expect_status(&stopped, 0);
        wait_for_process_gone(pid);
        wait_for_session_entries_gone(&rig, &id);
    }
}
