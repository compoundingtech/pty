//! Exact-generation retained evidence and replacement-safe removal.

mod registry_support;

use pty_core::registry::{
    self, EvidenceUnavailableReason, RemoveSessionGenerationResult, SessionExitEvidenceResult,
    SessionExitEvidenceTail, SessionExitStatus, SessionMetadata, TagCompareAndSetResult, TagMap,
};
use registry_support::{root, unique_name};

fn terminal_metadata(generation: &str) -> SessionMetadata {
    SessionMetadata {
        generation: Some(generation.to_string()),
        daemon_pid: Some(registry_support::DEAD_PID),
        command: "/bin/sh".to_string(),
        display_command: "sh".to_string(),
        cwd: "/tmp".to_string(),
        created_at: "2026-09-20T12:00:00.000Z".to_string(),
        exit_code: Some(7),
        exited_at: Some("2026-09-20T12:00:01.000Z".to_string()),
        last_lines: Some(vec!["ready".to_string(), "done".to_string()]),
        ..Default::default()
    }
}

#[test]
fn snapshot_preserves_bounded_combined_exit_evidence() {
    root();
    let name = unique_name("evidence-snapshot");
    registry::write_metadata_publication(&name, &terminal_metadata("generation-a")).unwrap();

    let SessionExitEvidenceResult::Snapshot { snapshot } =
        registry::get_session_exit_evidence(&name)
    else {
        panic!("expected snapshot");
    };
    assert_eq!(snapshot.name, name);
    assert_eq!(snapshot.generation, "generation-a");
    assert_eq!(snapshot.status, SessionExitStatus::Exited);
    assert_eq!(snapshot.exit_code, Some(7));
    assert_eq!(
        snapshot.tail,
        SessionExitEvidenceTail::Present {
            last_lines: vec!["ready".to_string(), "done".to_string()]
        }
    );
}

#[test]
fn snapshot_ignores_a_live_pid_from_another_daemon_generation() {
    root();
    let name = unique_name("evidence-pid-reuse");
    let mut metadata = terminal_metadata("generation-a");
    metadata.daemon_pid = Some(std::process::id() as i32);
    metadata.daemon_start_token = Some("stale-generation-token".to_string());
    metadata.daemon_boot_id = registry::read_boot_id();
    registry::write_metadata_publication(&name, &metadata).unwrap();

    assert!(matches!(
        registry::get_session_exit_evidence(&name),
        SessionExitEvidenceResult::Snapshot { .. }
    ));
}

#[test]
fn removal_refuses_a_replacement_generation_then_removes_the_exact_one() {
    root();
    let name = unique_name("evidence-generation");
    registry::write_metadata_publication(&name, &terminal_metadata("replacement")).unwrap();

    assert_eq!(
        registry::remove_session_generation(&name, "observed-before-replacement").unwrap(),
        RemoveSessionGenerationResult::GenerationMismatch
    );
    assert!(registry::metadata_path(&name).exists());
    assert_eq!(
        registry::remove_session_generation(&name, "replacement").unwrap(),
        RemoveSessionGenerationResult::Removed
    );
    assert!(!registry::metadata_path(&name).exists());
}

#[test]
fn strict_snapshot_rejects_partial_terminal_metadata() {
    root();
    let name = unique_name("evidence-invalid");
    std::fs::write(
        registry::metadata_path(&name),
        r#"{"generation":"generation-a","exitCode":7}"#,
    )
    .unwrap();
    assert_eq!(
        registry::get_session_exit_evidence(&name),
        SessionExitEvidenceResult::Unavailable {
            reason: EvidenceUnavailableReason::InvalidMetadata
        }
    );
}

#[test]
fn evidence_library_boundary_rejects_path_traversal() {
    let registry_root = root();
    let victim = unique_name("evidence-outside");
    let outside = registry_root
        .parent()
        .unwrap()
        .join(format!("{victim}.json"));
    std::fs::write(&outside, b"must remain").unwrap();
    let traversal = format!("../{victim}");

    assert_eq!(
        registry::get_session_exit_evidence(&traversal),
        SessionExitEvidenceResult::Unavailable {
            reason: EvidenceUnavailableReason::InvalidMetadata
        }
    );
    assert_eq!(
        registry::remove_session_generation(&traversal, "generation").unwrap(),
        RemoveSessionGenerationResult::InvalidMetadata
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"must remain");
    std::fs::remove_file(outside).unwrap();
}

#[test]
fn lifecycle_compare_and_set_is_fenced_by_generation_and_exact_value() {
    root();
    let name = unique_name("lifecycle-cas");
    let mut metadata = terminal_metadata("generation-a");
    metadata.tags = Some(TagMap::from([(
        "run.lifecycle".to_string(),
        "starting-a".to_string(),
    )]));
    registry::write_metadata_publication(&name, &metadata).unwrap();

    assert_eq!(
        registry::compare_and_set_tag_value(
            &name,
            "replacement",
            "run.lifecycle",
            "starting-a",
            "ready-a",
        ),
        TagCompareAndSetResult::GenerationMismatch
    );
    assert_eq!(
        registry::compare_and_set_tag_value(
            &name,
            "generation-a",
            "run.lifecycle",
            "stale-observation",
            "ready-a",
        ),
        TagCompareAndSetResult::ValueMismatch {
            value: Some("starting-a".to_string())
        }
    );
    assert_eq!(
        registry::compare_and_set_tag_value(
            &name,
            "generation-a",
            "run.lifecycle",
            "starting-a",
            "ready-a",
        ),
        TagCompareAndSetResult::Changed {
            value: "ready-a".to_string()
        }
    );
}

#[test]
fn unknown_identity_refuses_snapshot_and_generation_removal() {
    root();
    let name = unique_name("evidence-unknown");
    let mut metadata = terminal_metadata("unknown-generation");
    metadata.daemon_pid = Some(std::process::id() as i32);
    metadata.daemon_start_token = None;
    registry::write_metadata_publication(&name, &metadata).unwrap();
    assert_eq!(
        registry::get_session_exit_evidence(&name),
        SessionExitEvidenceResult::Unavailable {
            reason: EvidenceUnavailableReason::InvalidMetadata
        },
    );
    assert_eq!(
        registry::remove_session_generation(&name, "unknown-generation").unwrap(),
        RemoveSessionGenerationResult::NotTerminal,
    );
    assert!(registry::metadata_path(&name).exists());
}

#[test]
fn pidless_generation_evidence_agrees_with_listing_and_removal() {
    root();
    let name = unique_name("pidless-generation");
    let mut metadata = terminal_metadata("legacy-generation");
    metadata.daemon_pid = None;
    registry::write_metadata_publication(&name, &metadata).unwrap();
    let listed = registry::get_session_by_name(&name).unwrap();
    assert_eq!(listed.identity, registry::DaemonIdentityCheck::Gone);
    let SessionExitEvidenceResult::Snapshot { snapshot } =
        registry::get_session_exit_evidence(&name)
    else {
        panic!("pidless generation must have terminal evidence");
    };
    assert_eq!(snapshot.generation, "legacy-generation");
    assert_eq!(snapshot.status, SessionExitStatus::Exited);
    assert_eq!(snapshot.exit_code, Some(7));
    assert_eq!(
        registry::remove_session_generation(&name, "legacy-generation").unwrap(),
        RemoveSessionGenerationResult::Removed,
    );
    assert!(!registry::metadata_path(&name).exists());
}
