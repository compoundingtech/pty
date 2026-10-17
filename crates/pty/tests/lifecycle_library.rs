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

    let preview: GcResult = gc(GcOptions {
        dry_run: true,
        ..Default::default()
    });
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
    assert_eq!(
        artifact_snapshot(name),
        before,
        "dry-run GC changed the registry"
    );

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
        &SessionGenerationOwner {
            generation: started.generation.clone(),
            pid: started.pid as i32,
        },
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
    assert_eq!(
        published.generation.as_deref(),
        Some(started.generation.as_str())
    );

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
    params.cwd = std::env::current_dir()
        .expect("current directory")
        .to_string_lossy()
        .into_owned();
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
        &SessionGenerationOwner {
            generation: started.generation,
            pid: started.pid as i32,
        },
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

fn managed_params(
    name: &str,
    operation: &str,
    digest: &str,
    root: &str,
    marker: &Path,
) -> SpawnParams {
    let mut params = SpawnParams::new(name, "/bin/sh", &[
        "-c".into(),
        "printf ran > \"$1\"; exec cat".into(),
        "sh".into(),
        marker.to_string_lossy().into_owned(),
    ]);
    params.cwd = ROOT.to_string_lossy().into_owned();
    params.publication_binding = Some(registry::revocation::PublicationBinding {
        operation: operation.into(),
        digest: digest.into(),
    });
    params.publication_root_id = Some(root.into());
    params.tags.insert("st3.start-operation".into(), operation.into());
    params.tags.insert("keep".into(), "true".into());
    params
}

struct DaemonCleanup<'a> {
    name: &'a str,
    started: &'a SpawnedDaemon,
}

impl Drop for DaemonCleanup<'_> {
    fn drop(&mut self) {
        if registry::read_metadata(self.name).is_some_and(|metadata| {
            metadata.generation.as_deref() == Some(self.started.generation.as_str())
                && registry::read_signal_target_with(self.name, Some(&metadata))
                    == Some(self.started.pid as i32)
        }) {
            stop_daemon(self.name, self.started);
        }
    }
}

fn wait_for_marker(marker: &Path) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "child did not create {}", marker.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn library_managed_launch_rejects_revoked_binding_without_touching_live_survivor() {
    use std::os::unix::fs::MetadataExt;

    skip_without_a_real_machine!();
    let _serial = serial();
    let _root = ROOT.as_path();
    let name = unique_name("library-revocation");
    let root = registry::revocation::root_id().unwrap();
    let old_marker = ROOT.join(format!("{name}-old-ran"));
    let survivor_marker = ROOT.join(format!("{name}-survivor-ran"));
    let old = managed_params(&name, "revoked-library-start", &"a".repeat(64), &root, &old_marker);
    registry::revocation::install(
        &name, old.publication_binding.as_ref().unwrap(), &"b".repeat(64), &root,
    ).unwrap();
    let refused = spawn_daemon(Path::new(pty_bin()), old.clone());
    assert!(refused.is_err(), "library admitted revoked managed launch");
    assert!(refused.unwrap_err().to_string().contains("operation-revoked"));
    assert!(!old_marker.exists());
    assert!(artifact_snapshot(&name).iter().all(Option::is_none));

    let replacement = managed_params(
        &name, "replacement-library-start", &"c".repeat(64), &root, &survivor_marker,
    );
    let survivor = spawn_daemon(Path::new(pty_bin()), replacement.clone()).unwrap();
    let _cleanup = DaemonCleanup { name: &name, started: &survivor };
    wait_for_marker(&survivor_marker);
    let metadata = registry::read_metadata(&name).unwrap();
    assert_eq!(metadata.publication_binding, replacement.publication_binding);
    assert_eq!(metadata.publication_root_id.as_deref(), Some(root.as_str()));
    assert_eq!(metadata.generation.as_deref(), Some(survivor.generation.as_str()));
    let before = artifact_snapshot(&name);
    let socket = std::fs::metadata(registry::socket_path(&name)).unwrap();
    let refused = spawn_daemon(Path::new(pty_bin()), old).unwrap_err();
    assert!(refused.to_string().contains("operation-revoked"), "{refused}");
    assert!(!old_marker.exists());
    assert_eq!(artifact_snapshot(&name), before);
    let after = std::fs::metadata(registry::socket_path(&name)).unwrap();
    assert_eq!((after.dev(), after.ino()), (socket.dev(), socket.ino()));
    assert!(registry::pid_alive(survivor.pid as i32));
    assert!(std::os::unix::net::UnixStream::connect(registry::socket_path(&name)).is_ok());
}

#[test]
fn library_gc_retains_revoked_desired_state_and_respawns_distinct_managed_binding() {
    skip_without_a_real_machine!();
    let _serial = serial();
    let _root = ROOT.as_path();
    let name = unique_name("library-gc-revocation");
    let root = registry::revocation::root_id().unwrap();
    let old_marker = ROOT.join(format!("{name}-old-ran"));
    let replacement_marker = ROOT.join(format!("{name}-replacement-ran"));
    let old = managed_params(&name, "revoked-gc-start", &"a".repeat(64), &root, &old_marker);
    registry::revocation::install(
        &name, old.publication_binding.as_ref().unwrap(), &"b".repeat(64), &root,
    ).unwrap();
    let mut tags = old.tags.clone();
    tags.insert("strategy".into(), "permanent".into());
    let metadata = SessionMetadata {
        generation: Some("ended-managed-generation".into()),
        daemon_pid: Some(DEAD_PID),
        command: old.command.clone(),
        args: old.args.clone(),
        display_command: old.display_command.clone(),
        cwd: old.cwd.clone(),
        created_at: registry::now_iso8601(),
        tags: Some(tags),
        publication_binding: old.publication_binding.clone(),
        publication_root_id: old.publication_root_id.clone(),
        publication_capability: Some(1),
        ..Default::default()
    };
    registry::write_metadata_publication(&name, &metadata).unwrap();
    registry::write_pid(&name, DEAD_PID as u32).unwrap();
    std::fs::write(registry::events_path(&name), b"retained predecessor evidence\n").unwrap();
    let before = artifact_snapshot(&name);
    for _ in 0..2 {
        let result = gc(GcOptions {
            daemon_executable: Some(PathBuf::from(pty_bin())),
            ..Default::default()
        });
        let failed = result.respawn_failed.iter().find(|entry| entry.name == name)
            .expect("revoked permanent session must report failed respawn");
        assert!(failed.error.contains("operation-revoked"), "{}", failed.error);
        assert!(!result.respawned.iter().any(|entry| entry.name == name));
        assert_eq!(artifact_snapshot(&name), before);
        assert!(!old_marker.exists());
    }
    assert!(registry::revocation::read(&name, "revoked-gc-start").unwrap().is_some());

    // A new desired launch is a new immutable operation, never an unrevocation.
    let replacement = managed_params(
        &name, "replacement-gc-start", &"c".repeat(64), &root, &replacement_marker,
    );
    let mut replacement_meta = metadata;
    replacement_meta.args = replacement.args;
    replacement_meta.publication_binding = replacement.publication_binding.clone();
    replacement_meta.tags.as_mut().unwrap().insert(
        "st3.start-operation".into(), "replacement-gc-start".into(),
    );
    registry::write_metadata_publication(&name, &replacement_meta).unwrap();
    let result = gc(GcOptions {
        daemon_executable: Some(PathBuf::from(pty_bin())),
        ..Default::default()
    });
    let published = registry::read_metadata(&name).unwrap();
    let started = SpawnedDaemon {
        pid: published.daemon_pid.unwrap() as u32,
        generation: published.generation.clone().unwrap(),
    };
    let _cleanup = DaemonCleanup { name: &name, started: &started };
    assert!(result.respawned.iter().any(|entry| entry.name == name));
    wait_for_marker(&replacement_marker);
    assert!(!old_marker.exists());
    assert_eq!(published.publication_binding, replacement.publication_binding);
    assert_eq!(published.publication_root_id.as_deref(), Some(root.as_str()));
    assert_ne!(published.generation, replacement_meta.generation);
    let events = std::fs::read_to_string(registry::events_path(&name)).unwrap();
    assert!(events.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line).unwrap()["type"] == "session_respawn"
    }));
}

#[test]
fn library_gc_requires_new_binding_for_changed_manifest_launch_inputs() {
    skip_without_a_real_machine!();
    let _serial = serial();
    let _root = ROOT.as_path();
    let name = unique_name("library-gc-manifest-binding");
    let dir = ROOT.join(format!("{name}-manifest"));
    std::fs::create_dir(&dir).unwrap();
    let manifest = dir.join("pty.toml");
    let marker = dir.join("original-ran");
    let unrelated_marker = dir.join("unrelated-ran");
    let command = format!("printf ran > \"{}\"; exec cat", marker.display());
    let original = format!("[sessions.worker]\ncommand = '{command}'\n");
    std::fs::write(&manifest, &original).unwrap();
    let file = pty_core::ptyfile::read_pty_file(Some(&dir)).unwrap();
    let session = &file.sessions[0];
    let root = registry::revocation::root_id().unwrap();
    let binding = registry::revocation::PublicationBinding {
        operation: "manifest-bound-start".into(),
        // This is an external request digest, not a command fingerprint.
        digest: "d".repeat(64),
    };
    let mut tags = TagMap::new();
    tags.insert("strategy".into(), "permanent".into());
    tags.insert("st3.start-operation".into(), binding.operation.clone());
    tags.insert("ptyfile".into(), manifest.to_string_lossy().into_owned());
    tags.insert("ptyfile.session".into(), "worker".into());
    let metadata = SessionMetadata {
        generation: Some("ended-manifest-generation".into()),
        daemon_pid: Some(DEAD_PID),
        command: "/bin/sh".into(),
        args: vec!["-c".into(), pty_core::ptyfile::command_with_env_exports(session)],
        display_command: session.command.clone(),
        cwd: file.dir.to_string_lossy().into_owned(),
        created_at: registry::now_iso8601(),
        tags: Some(tags),
        publication_binding: Some(binding.clone()),
        publication_root_id: Some(root.clone()),
        publication_capability: Some(1),
        ..Default::default()
    };
    registry::write_metadata_publication(&name, &metadata).unwrap();
    registry::write_pid(&name, DEAD_PID as u32).unwrap();
    std::fs::write(registry::events_path(&name), b"retained manifest predecessor\n").unwrap();
    let before = artifact_snapshot(&name);
    // An unchanged managed task still cannot be handed to an older/native
    // publisher without revocation support, even though it is executable.
    for _ in 0..2 {
        let result = gc(GcOptions {
            daemon_executable: Some(PathBuf::from("/bin/false")),
            ..Default::default()
        });
        let failed = result.respawn_failed.iter().find(|entry| entry.name == name)
            .expect("unsupported managed publisher must report failed respawn");
        assert!(failed.error.contains("lacks publication-revocation v1"), "{}", failed.error);
        assert!(!result.respawned.iter().any(|entry| entry.name == name));
        assert_eq!(artifact_snapshot(&name), before);
        assert!(!marker.exists());
        assert!(!unrelated_marker.exists());
    }
    // Legacy launchers need not implement the managed capability protocol.
    let legacy = SpawnParams::new("legacy-publisher", "/bin/true", &[]);
    assert!(pty_lifecycle::prepare_daemon(Path::new("/bin/false"), &legacy).is_ok());

    let changed_command = format!(
        "[sessions.worker]\ncommand = 'printf ran > \"{}\"; exec cat'\n",
        unrelated_marker.display(),
    );
    let changed_env = format!("{original}env = {{ BOUND_VALUE = 'changed' }}\n");
    let changed_cwd = format!("{original}cwd = '..'\n");
    for changed in [changed_command, changed_env, changed_cwd] {
        std::fs::write(&manifest, changed).unwrap();
        for dry_run in [true, false, false] {
            let result = gc(GcOptions {
                dry_run,
                daemon_executable: Some(PathBuf::from(pty_bin())),
                ..Default::default()
            });
            let failed = result.respawn_failed.iter().find(|entry| entry.name == name)
                .expect("changed managed launch must report failed respawn");
            assert!(failed.error.contains("new operation and publication binding"), "{}", failed.error);
            assert!(!result.respawned.iter().any(|entry| entry.name == name));
            assert_eq!(artifact_snapshot(&name), before);
            assert!(!marker.exists());
            assert!(!unrelated_marker.exists());
        }
    }

    // The original desired launch can still respawn under its exact binding.
    std::fs::write(&manifest, original).unwrap();
    let result = gc(GcOptions {
        daemon_executable: Some(PathBuf::from(pty_bin())),
        ..Default::default()
    });
    let published = registry::read_metadata(&name).unwrap();
    let started = SpawnedDaemon {
        pid: published.daemon_pid.unwrap() as u32,
        generation: published.generation.clone().unwrap(),
    };
    let cleanup = DaemonCleanup { name: &name, started: &started };
    assert!(result.respawned.iter().any(|entry| entry.name == name));
    wait_for_marker(&marker);
    assert!(!unrelated_marker.exists());
    assert_eq!(published.command, metadata.command);
    assert_eq!(published.args, metadata.args);
    assert_eq!(published.cwd, metadata.cwd);
    assert_eq!(published.publication_binding, Some(binding));
    assert_eq!(published.publication_root_id.as_deref(), Some(root.as_str()));
    drop(cleanup);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[cfg(debug_assertions)]
fn ordinary_daemon_retains_publication_pin_until_child_spawn() {
    use std::os::unix::fs::MetadataExt;
    use std::process::{Command, Stdio};

    struct HeldDaemon {
        child: std::process::Child,
        release: PathBuf,
    }
    impl Drop for HeldDaemon {
        fn drop(&mut self) {
            // Signal before releasing the pre-fork barrier: cleanup must not
            // race the daemon into creating a child, and a serving daemon
            // must get its normal graceful child teardown.
            if matches!(self.child.try_wait(), Ok(None)) {
                // SAFETY: try_wait observed a live, unreaped direct child.
                // It cannot have its pid reused before this Child reaps it.
                unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
                let _ = std::fs::write(&self.release, "");
                if !wait_for_process_exit(self.child.id() as i32, Duration::from_secs(8)) {
                    let _ = self.child.kill();
                }
            } else {
                let _ = std::fs::write(&self.release, "");
            }
            let _ = self.child.wait();
        }
    }

    skip_without_a_real_machine!();
    let _serial = serial();
    let _root = ROOT.as_path();
    let name = unique_name("library-child-pin");
    let root = registry::revocation::root_id().unwrap();
    let marker = ROOT.join(format!("{name}-child-ran"));
    let params = managed_params(&name, "ordinary-child-pin", &"f".repeat(64), &root, &marker);
    let barriers = ROOT.join(format!("{name}-barriers"));
    std::fs::create_dir(&barriers).unwrap();
    std::fs::write(barriers.join("published-before-child-fork.hold"), "").unwrap();
    let mut daemon = HeldDaemon {
        child: Command::new(pty_bin())
            .arg("__daemon")
            .env("PTY_SERVER_CONFIG", serde_json::to_string(&pty_lifecycle::config_for(&params)).unwrap())
            .env("PTY_TEST_OPERATION_BARRIERS", &barriers)
            .env_remove("PTY_CREATION_LOCK_OWNER_PID")
            .env_remove("PTY_SPAWNER_PID")
            .env_remove("PTY_DAEMON_READY_FD")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn().unwrap(),
        release: barriers.join("published-before-child-fork.release"),
    };
    wait_for_marker(&barriers.join("published-before-child-fork.reached"));
    assert!(!marker.exists());
    let before = artifact_snapshot(&name);
    let busy = registry::revocation::install(
        &name, params.publication_binding.as_ref().unwrap(), &"e".repeat(64), &root,
    ).unwrap_err();
    assert_eq!(busy.kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(artifact_snapshot(&name), before);
    assert!(daemon.child.try_wait().unwrap().is_none());
    assert!(!marker.exists(), "child ran while publication barrier was held");
    std::fs::write(&daemon.release, "").unwrap();
    wait_for_marker(&marker);
    // The child's marker can precede the parent's release of the publication
    // pin. A STATUS response proves that the daemon has entered its serving
    // loop, after dropping that pin; merely connecting to its socket does not.
    let socket_path = registry::socket_path(&name);
    let mut conn = daemon_support::Conn::connect(
        socket_path.parent().unwrap(),
        socket_path.file_stem().unwrap().to_str().unwrap(),
    );
    conn.query_status();
    let before = artifact_snapshot(&name);
    let socket = std::fs::metadata(registry::socket_path(&name)).unwrap();
    let installed = registry::revocation::install(
        &name, params.publication_binding.as_ref().unwrap(), &"e".repeat(64), &root,
    ).unwrap();
    assert_eq!(installed.occupancy["entries"][0]["status"], "running");
    assert_eq!(artifact_snapshot(&name), before);
    let after = std::fs::metadata(registry::socket_path(&name)).unwrap();
    assert_eq!((after.dev(), after.ino()), (socket.dev(), socket.ino()));
    assert!(daemon.child.try_wait().unwrap().is_none(), "revocation killed serving daemon");
    assert!(std::os::unix::net::UnixStream::connect(registry::socket_path(&name)).is_ok());
    let metadata = registry::read_metadata(&name).unwrap();
    stop_daemon(&name, &SpawnedDaemon {
        pid: daemon.child.id(),
        generation: metadata.generation.unwrap(),
    });
}

/// Keep process-global barrier configuration out of the parallel test runner.
/// The outer process bounds the entire race, including blocking library calls.
#[cfg(debug_assertions)]
fn isolated_library_claim_race(test: &str, scenario: fn()) {
    const FIXTURE: &str = "PTY_LIBRARY_CLAIM_RACE";
    if std::env::var(FIXTURE).as_deref() == Ok(test) {
        scenario();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(FIXTURE, test)
        .spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "isolated library claim race failed: {status}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("isolated library claim race exceeded 45 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(debug_assertions)]
struct LibraryRaceBarrier(PathBuf);

#[cfg(debug_assertions)]
impl LibraryRaceBarrier {
    fn hold(name: &str) -> Self {
        let directory = ROOT.join(format!("{name}-barriers"));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join(format!("{name}.hold")), "").unwrap();
        // SAFETY: this isolated fixture configures barriers before starting
        // its installer/GC thread or any daemon.
        unsafe { std::env::set_var("PTY_TEST_OPERATION_BARRIERS", &directory) };
        Self(directory.join(format!("{name}.release")))
    }

    fn wait_until_reached(&self) {
        wait_for_marker(&self.0.with_extension("reached"));
    }

    fn release(&self) {
        std::fs::write(&self.0, "").unwrap();
    }
}

#[cfg(debug_assertions)]
impl Drop for LibraryRaceBarrier {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "");
    }
}

#[test]
#[cfg(debug_assertions)]
fn in_process_install_pins_its_claim_before_a_library_publisher_can_check() {
    skip_without_a_real_machine!();
    isolated_library_claim_race(
        "in_process_install_pins_its_claim_before_a_library_publisher_can_check",
        || {
            let _root = ROOT.as_path();
            let name = unique_name("library-install-race");
            let root = registry::revocation::root_id().unwrap();
            let marker = ROOT.join(format!("{name}-ran"));
            let mut params = managed_params(&name, "install-race", &"a".repeat(64), &root, &marker);
            params.start_timeout = Some(Duration::from_secs(3));
            params.bind_to_spawner_lifetime = true;
            let barrier = LibraryRaceBarrier::hold("revocation-install-pinned");
            let install_name = name.clone();
            let binding = params.publication_binding.clone().unwrap();
            let installer = std::thread::spawn(move || {
                registry::revocation::install(&install_name, &binding, &"b".repeat(64), &root)
            });
            barrier.wait_until_reached();
            assert!(registry::revocation::read(&name, "install-race").unwrap().is_none());
            let attempted = spawn_daemon(Path::new(pty_bin()), params.clone());
            if let Ok(started) = &attempted {
                stop_daemon(&name, started);
            }
            let error = attempted.expect_err("publisher borrowed an in-flight install's PID claim");
            assert!(error.to_string().contains("busy"), "{error}");
            assert!(!marker.exists());
            assert!(artifact_snapshot(&name).iter().all(Option::is_none));
            barrier.release();
            let installed = installer.join().unwrap().unwrap();
            assert_eq!(installed.occupancy["entries"], serde_json::json!([]));
            let refused = spawn_daemon(Path::new(pty_bin()), params).unwrap_err();
            assert!(refused.to_string().contains("operation-revoked"), "{refused}");
            assert!(!marker.exists());
            assert!(artifact_snapshot(&name).iter().all(Option::is_none));
        },
    );
}

#[test]
#[cfg(debug_assertions)]
fn in_process_gc_claim_cannot_admit_a_publisher_or_delete_its_live_replacement() {
    skip_without_a_real_machine!();
    isolated_library_claim_race(
        "in_process_gc_claim_cannot_admit_a_publisher_or_delete_its_live_replacement",
        || {
            let _root = ROOT.as_path();
            let name = unique_name("library-gc-claim-race");
            let metadata = SessionMetadata {
                generation: Some("gc-predecessor".into()),
                daemon_pid: Some(DEAD_PID),
                command: "cat".into(),
                display_command: "cat".into(),
                cwd: ROOT.to_string_lossy().into_owned(),
                created_at: registry::now_iso8601(),
                ..Default::default()
            };
            registry::write_metadata_publication(&name, &metadata).unwrap();
            registry::write_pid(&name, DEAD_PID as u32).unwrap();
            let before = artifact_snapshot(&name);
            let barrier = LibraryRaceBarrier::hold("gc-cleanup-before-effects");
            let collector = std::thread::spawn(|| gc(GcOptions::default()));
            barrier.wait_until_reached();
            let mut params = SpawnParams::new(&name, "cat", &[]);
            params.cwd = ROOT.to_string_lossy().into_owned();
            params.start_timeout = Some(Duration::from_secs(3));
            params.bind_to_spawner_lifetime = true;
            let attempted = spawn_daemon(Path::new(pty_bin()), params.clone());
            if let Ok(started) = &attempted {
                stop_daemon(&name, started);
            }
            let error = attempted.expect_err("publisher borrowed an in-process GC's PID claim");
            assert!(error.to_string().contains("busy"), "{error}");
            assert_eq!(artifact_snapshot(&name), before);
            barrier.release();
            let collected = collector.join().unwrap();
            assert!(collected.removed.iter().any(|removed| removed == &name));
            let replacement = spawn_daemon(Path::new(pty_bin()), params).unwrap();
            let _cleanup = DaemonCleanup { name: &name, started: &replacement };
            let live = artifact_snapshot(&name);
            let collected = gc(GcOptions::default());
            assert!(!collected.removed.iter().any(|removed| removed == &name));
            assert_eq!(artifact_snapshot(&name), live);
            assert!(registry::pid_alive(replacement.pid as i32));
        },
    );
}
