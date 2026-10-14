//! Integration contract for the importable `pty-lifecycle` surface.

mod daemon_support;

use std::ffi::CString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use daemon_support::{pty_bin, serial, unique_name};
use pty_core::proctable::Answer;
use pty_core::registry::{self, SessionMetadata, TagMap};
use pty_lifecycle::{
    GcOptions, GcResult, LockBusy, PrunedTags, SessionGenerationOwner, SpawnError, SpawnParams,
    SpawnedDaemon, cleanup_all, cleanup_owned_all, cleanup_owned_socket, cleanup_socket, gc,
    prune_orphan_layout_tags, spawn_daemon, wait_for_process_exit,
};

const DEAD_PID: i32 = 2_147_483_646;

static ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    let root = std::env::temp_dir().join(format!("pty-lifecycle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create lifecycle test registry");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
        .expect("make lifecycle test registry private");
    // SAFETY: this test binary initializes its registry root once and never
    // changes it while registry code is running.
    unsafe {
        std::env::set_var("PTY_ROOT", &root);
        std::env::set_var("PTY_ROOT_LEGACY_SILENT", "1");
        std::env::remove_var("PTY_SESSION_DIR");
        std::env::remove_var("PTY_SESSION");
    }
    root
});

fn artifact_snapshot(name: &str) -> Vec<Option<Vec<u8>>> {
    [
        registry::socket_path(name),
        registry::pid_path(name),
        registry::metadata_path(name),
        registry::events_path(name),
        registry::recovery_revision_path(name),
    ]
    .iter()
    .map(|path| std::fs::read(path).ok())
    .collect()
}

fn observe_pruned_tags(result: &PrunedTags) {
    let _: (&str, &[String]) = (&result.name, &result.removed_keys);
}

#[test]
fn public_library_surface_has_explicit_launcher_typed_reports_and_safe_cleanup() {
    let _: fn(&Path, SpawnParams) -> Result<SpawnedDaemon, SpawnError> = spawn_daemon;
    let _: fn(GcOptions) -> GcResult = gc;
    let _: fn(bool) -> Vec<PrunedTags> = prune_orphan_layout_tags;
    let _: fn(&str) = cleanup_socket;
    let _: fn(&str) -> Result<(), LockBusy> = cleanup_all;
    let _: fn(&str, &SessionGenerationOwner) -> bool = cleanup_owned_socket;
    let _: fn(&str, &SessionGenerationOwner) -> bool = cleanup_owned_all;
    let _: fn(i32, Duration) -> bool = wait_for_process_exit;
    let _: fn(&PrunedTags) = observe_pruned_tags;
}

#[test]
fn gc_dry_run_reports_without_mutating_then_applies_the_same_cleanup() {
    let _serial = serial();
    let name = "lifecycle-gc-dry-run";
    let metadata = SessionMetadata {
        generation: Some("gone-generation".into()),
        daemon_pid: Some(DEAD_PID),
        daemon_start_token: Some("fixture:dead".into()),
        daemon_boot_id: registry::read_boot_id(),
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "exit 0".into()],
        display_command: "exit 0".into(),
        cwd: ROOT.to_string_lossy().into_owned(),
        rows: Some(24),
        cols: Some(80),
        ephemeral: Some(false),
        created_at: registry::now_iso8601(),
        tags: Some(TagMap::new()),
        ..Default::default()
    };
    registry::write_metadata_publication(name, &metadata).unwrap();
    registry::write_pid(name, DEAD_PID as u32).unwrap();
    std::fs::write(registry::events_path(name), b"existing evidence\n").unwrap();
    let before = artifact_snapshot(name);

    let preview: GcResult = gc(GcOptions { dry_run: true, ..Default::default() });
    assert_eq!(preview.removed, [name]);
    assert!(preview.killed_orphan_children.is_empty());
    assert!(preview.abandoned.is_empty());
    assert!(preview.respawned.is_empty());
    assert!(preview.respawn_failed.is_empty());
    assert!(preview.flapped.is_empty());
    assert!(preview.flapping_skipped.is_empty());
    assert!(preview.kept.is_empty());
    assert!(preview.keep_expired.is_empty());
    assert!(preview.reap_skipped.is_empty());
    assert_eq!(artifact_snapshot(name), before, "dry-run GC changed the registry");

    let applied: GcResult = gc(GcOptions::default());
    assert_eq!(applied.removed, [name]);
    assert!(artifact_snapshot(name).iter().all(Option::is_none));
}

fn stop_daemon(name: &str, started: &SpawnedDaemon) {
    // SAFETY: signal the exact daemon pid returned by `spawn_daemon`.
    let _ = unsafe { libc::kill(started.pid as i32, libc::SIGTERM) };
    let _ = wait_for_process_exit(started.pid as i32, Duration::from_secs(8));
    let _ = cleanup_owned_all(
        name,
        &SessionGenerationOwner { generation: started.generation.clone(), pid: started.pid as i32 },
    );
}

#[test]
fn spawn_params_new_supplies_a_usable_cwd_to_a_real_daemon() {
    skip_without_a_real_machine!();
    let _serial = serial();
    let _root = ROOT.as_path();
    let name = unique_name("library-defaults");
    let params = SpawnParams::new(&name, "/bin/sh", &["-c".into(), "exec sleep 30".into()]);

    assert!(!params.cwd.is_empty(), "SpawnParams::new left cwd empty");
    assert!(
        Path::new(&params.cwd).is_dir(),
        "SpawnParams::new produced an unusable cwd: {:?}",
        params.cwd
    );
    let started = spawn_daemon(Path::new(pty_bin()), params)
        .expect("SpawnParams::new should be spawnable without overwriting cwd");
    let published = registry::read_metadata(&name).expect("daemon published metadata");
    assert_eq!(published.daemon_pid, Some(started.pid as i32));
    assert_eq!(published.daemon_boot_id, registry::read_boot_id());
    assert_eq!(
        registry::check_daemon_identity(started.pid as i32, &published),
        registry::DaemonIdentityCheck::Match,
    );
    assert_eq!(published.generation.as_deref(), Some(started.generation.as_str()));

    stop_daemon(&name, &started);
}

#[test]
fn library_reaps_its_daemon_child_after_the_daemon_exits() {
    skip_without_a_real_machine!();
    let _serial = serial();
    let _root = ROOT.as_path();
    let name = unique_name("library-reaper");
    let mut params = SpawnParams::new(&name, "/bin/sh", &["-c".into(), "sleep 1".into()]);
    // Keep this test independent of the constructor-default finding above.
    params.cwd = std::env::current_dir().expect("current directory").to_string_lossy().into_owned();
    params.tags.insert("keep".into(), "true".into());
    let started =
        spawn_daemon(Path::new(pty_bin()), params).expect("start short-lived real daemon");

    let deadline = Instant::now() + Duration::from_secs(8);
    let final_observation = loop {
        let observation = pty_core::proctable::process(started.pid as i32);
        if matches!(observation, Answer::NotPresent) || Instant::now() >= deadline {
            break observation;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        matches!(final_observation, Answer::NotPresent),
        "library left daemon {} in the process table: {final_observation:?}",
        started.pid
    );
    // This test deliberately never calls waitpid: only the library may reap
    // the child, and process-table absence proves that it did.
    let _ = cleanup_owned_all(
        &name,
        &SessionGenerationOwner { generation: started.generation, pid: started.pid as i32 },
    );
}

#[derive(Clone, Copy)]
enum PublicationMutation {
    Remove,
    Replace,
}

fn fake_publishing_launcher(name: &str) -> PathBuf {
    let launcher = ROOT.join(format!("{name}-launcher"));
    let body = format!(
        "#!/bin/sh\n\
         pid=$$\n\
         cat > '{}' <<EOF\n\
         {{\"generation\":\"publication-generation\",\"daemonPid\":$pid,\"command\":\"sleep\",\"args\":[],\"displayCommand\":\"sleep\",\"cwd\":\"/tmp\",\"createdAt\":\"2026-09-13T00:00:00.000Z\"}}\n\
         EOF\n\
         printf '%s' \"$pid\" > '{}'\n\
         : > '{}'\n\
         eval \"printf '\\001' >&$PTY_DAEMON_READY_FD\"\n\
         exec sleep 30\n",
        registry::metadata_path(name).display(),
        registry::pid_path(name).display(),
        registry::socket_path(name).display(),
    );
    std::fs::write(&launcher, body).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    launcher
}

fn spawn_across_publication_mutation(mutation: PublicationMutation) {
    let name = unique_name("library-publication");
    let launcher = fake_publishing_launcher(&name);
    let events_path = registry::events_path(&name);
    let fifo = CString::new(events_path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `fifo` is a valid NUL-terminated path to a missing test file.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let metadata_path = registry::metadata_path(&name);
    let thread_name = name.clone();
    let publisher = std::thread::spawn(move || {
        // Opening the FIFO completes only after `has_published_session_start`
        // has read the matching metadata snapshot and started reading events.
        let mut events = std::fs::OpenOptions::new()
            .write(true)
            .open(events_path)
            .expect("publication observer opened the events FIFO");
        match mutation {
            PublicationMutation::Remove => {
                std::fs::remove_file(metadata_path).unwrap();
            }
            PublicationMutation::Replace => {
                std::fs::write(
                    metadata_path,
                    format!(
                        "{{\"generation\":\"foreign-generation\",\"daemonPid\":{},\"createdAt\":\"2026-09-13T00:00:00.000Z\"}}",
                        std::process::id()
                    ),
                )
                .unwrap();
            }
        }
        writeln!(
            events,
            "{{\"session\":\"{thread_name}\",\"type\":\"session_start\",\"ts\":\"2026-09-13T00:00:00.001Z\"}}"
        )
        .unwrap();
    });

    let mut params = SpawnParams::new(&name, "/bin/true", &[]);
    params.cwd = ROOT.to_string_lossy().into_owned();
    params.start_timeout = Some(Duration::from_secs(8));
    let started = spawn_daemon(&launcher, params).expect("matching publication was observed");
    publisher.join().unwrap();

    assert_eq!(
        started.generation, "publication-generation",
        "returned generation did not come from the metadata snapshot matched to pid {}",
        started.pid
    );
    assert!(!started.generation.is_empty());

    // SAFETY: the fake launcher execs sleep without changing its pid.
    let _ = unsafe { libc::kill(started.pid as i32, libc::SIGTERM) };
    registry::cleanup(&name);
    let _ = std::fs::remove_file(launcher);
}

#[test]
fn returned_generation_belongs_to_the_publication_matched_with_the_returned_pid() {
    let _serial = serial();
    let _root = ROOT.as_path();
    spawn_across_publication_mutation(PublicationMutation::Remove);
    spawn_across_publication_mutation(PublicationMutation::Replace);
}

#[test]
fn real_daemon_startup_retires_stale_exited_records_without_touching_live_or_unknown() {
    skip_without_a_real_machine!();
    let _serial = serial();
    let _root = ROOT.as_path();
    let stale_name = unique_name("startup-stale");
    let live_name = unique_name("startup-live");
    let unknown_name = unique_name("startup-unknown");
    let name = unique_name("startup-new");
    let pid = std::process::id() as i32;
    let token = registry::read_process_start_token(pid).expect("test process start token");
    let stale = SessionMetadata {
        generation: Some("stale-exited-generation".into()),
        daemon_pid: Some(pid),
        daemon_start_token: Some("previous:process-token".into()),
        daemon_boot_id: registry::read_boot_id(),
        exit_code: Some(0),
        exited_at: Some("2026-10-01T00:00:00.000Z".into()),
        ..Default::default()
    };
    registry::write_metadata_publication(&stale_name, &stale).unwrap();
    registry::write_pid(&stale_name, pid as u32).unwrap();
    let companions = [
        registry::socket_path(&stale_name),
        registry::events_path(&stale_name),
        registry::recovery_revision_path(&stale_name),
        registry::output_activity_path(&stale_name),
    ];
    for path in &companions {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"retained history").unwrap();
    }
    let live = SessionMetadata {
        generation: Some("matching-generation".into()),
        daemon_pid: Some(pid),
        daemon_start_token: Some(token.clone()),
        daemon_boot_id: registry::read_boot_id(),
        ephemeral: Some(false),
        ..Default::default()
    };
    registry::write_metadata_publication(&live_name, &live).unwrap();
    let unknown = SessionMetadata { daemon_start_token: None, ..live.clone() };
    registry::write_metadata_publication(&unknown_name, &unknown).unwrap();
    let live_bytes = std::fs::read(registry::metadata_path(&live_name)).unwrap();
    let unknown_bytes = std::fs::read(registry::metadata_path(&unknown_name)).unwrap();

    let params = SpawnParams::new(&name, "/bin/sh", &["-c".into(), "exec sleep 30".into()]);
    let started =
        spawn_daemon(Path::new(pty_bin()), params).expect("start daemon and sweep registry");
    assert!(!registry::metadata_path(&stale_name).exists());
    assert!(!registry::pid_path(&stale_name).exists());
    assert!(companions.iter().all(|path| !path.exists()));
    assert_eq!(registry::read_metadata(&live_name), Some(live));
    assert_eq!(registry::read_metadata(&unknown_name), Some(unknown));
    assert_eq!(std::fs::read(registry::metadata_path(&live_name)).unwrap(), live_bytes);
    assert_eq!(std::fs::read(registry::metadata_path(&unknown_name)).unwrap(), unknown_bytes);
    assert_eq!(registry::read_process_start_token(pid).as_deref(), Some(token.as_str()));
    assert_eq!(
        registry::check_daemon_identity(
            started.pid as i32,
            &registry::read_metadata(&name).unwrap()
        ),
        registry::DaemonIdentityCheck::Match,
    );
    stop_daemon(&name, &started);
    registry::cleanup_all(&live_name).unwrap();
    registry::cleanup_all(&unknown_name).unwrap();
}
