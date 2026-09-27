//! `.activity/<name>.json` writes and removals stay fenced on the writing
//! daemon's generation, so a daemon whose id was reused can never clobber
//! or delete its replacement's sidecar (docs/decisions/0015).

mod registry_support;

use std::cell::RefCell;
use std::rc::Rc;

use pty_core::registry::{self, MutateStatus, OutputActivity, SessionMetadata};
use registry_support::{root, unique_name};

const OLD: &str = "0123456789abcdef0123456789abcdef";
const NEW: &str = "fedcba9876543210fedcba9876543210";

fn record(generation: &str) -> SessionMetadata {
    SessionMetadata {
        generation: Some(generation.into()),
        daemon_pid: Some(std::process::id() as i32),
        command: "/bin/cat".into(),
        display_command: "cat".into(),
        cwd: "/tmp".into(),
        rows: Some(24),
        cols: Some(80),
        created_at: "2026-09-27T10:00:00.000Z".into(),
        ..Default::default()
    }
}

fn stamp(generation: &str, at: i64) -> OutputActivity {
    OutputActivity {
        generation: generation.into(),
        last_output_at_ms: at,
    }
}

/// The replacement's start-up: publish its record under the creation lock,
/// then its first activity stamp.
fn publish_replacement(name: &str) {
    registry::write_metadata_publication(name, &record(NEW)).unwrap();
}

#[test]
fn exit_retirement_never_removes_a_replacements_sidecar() {
    let _ = root();
    let name = unique_name("act-retire");
    registry::write_metadata_publication(&name, &record(OLD)).unwrap();
    assert!(matches!(
        registry::publish_output_activity(&name, &stamp(OLD, 1)),
        MutateStatus::Unchanged(_)
    ));

    // Interleave the replacement at the last moment before the old daemon's
    // unlink. It publishes only when it can take the creation lock, as a
    // real replacement must; otherwise it goes once the old daemon is done.
    let deferred = Rc::new(RefCell::new(false));
    let hook_name = name.clone();
    let hook_deferred = Rc::clone(&deferred);
    registry::before_output_activity_retire_on_this_thread(Box::new(move || {
        match registry::acquire_lock(&hook_name) {
            Some(lock) => {
                publish_replacement(&hook_name);
                drop(lock);
                assert!(matches!(
                    registry::publish_output_activity(&hook_name, &stamp(NEW, 2)),
                    MutateStatus::Unchanged(_)
                ));
            }
            None => *hook_deferred.borrow_mut() = true,
        }
    }));

    let status = registry::record_exit_retiring_output_activity(&name, OLD, |m| {
        m.exit_code = Some(0);
        true
    });
    assert!(matches!(status, MutateStatus::Changed(_)), "{status:?}");
    if *deferred.borrow() {
        let lock = registry::acquire_lock(&name).expect("old daemon released the lock");
        publish_replacement(&name);
        drop(lock);
        assert!(matches!(
            registry::publish_output_activity(&name, &stamp(NEW, 2)),
            MutateStatus::Unchanged(_)
        ));
    }

    assert_eq!(
        registry::read_output_activity(&name),
        Some(stamp(NEW, 2)),
        "the old daemon removed its replacement's sidecar"
    );
}

#[test]
fn a_superseded_daemon_cannot_overwrite_the_replacements_sidecar() {
    let _ = root();
    let name = unique_name("act-publish");
    registry::write_metadata_publication(&name, &record(NEW)).unwrap();
    assert!(matches!(
        registry::publish_output_activity(&name, &stamp(NEW, 5)),
        MutateStatus::Unchanged(_)
    ));

    let late = registry::publish_output_activity(&name, &stamp(OLD, 9));
    assert_eq!(late, MutateStatus::GenerationMismatch);
    assert_eq!(registry::read_output_activity(&name), Some(stamp(NEW, 5)));
}

#[test]
fn a_held_lock_defers_the_write_instead_of_racing_it() {
    let _ = root();
    let name = unique_name("act-busy");
    registry::write_metadata_publication(&name, &record(OLD)).unwrap();
    let lock = registry::acquire_lock(&name).unwrap();
    assert_eq!(
        registry::publish_output_activity(&name, &stamp(OLD, 3)),
        MutateStatus::Busy
    );
    assert_eq!(registry::read_output_activity(&name), None);
    drop(lock);
    assert!(matches!(
        registry::publish_output_activity(&name, &stamp(OLD, 3)),
        MutateStatus::Unchanged(_)
    ));
    assert_eq!(registry::read_output_activity(&name), Some(stamp(OLD, 3)));
}
