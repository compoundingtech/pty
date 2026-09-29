//! `<name>.json`: publication key order, unknown-field preservation,
//! `mutate_metadata_under_lock` statuses, the presentation patches and the
//! events they emit, and a byte-level round trip against a file the Node
//! daemon wrote.

mod registry_support;

use std::time::{Duration, Instant};

use indexmap::IndexMap;
use pty_core::events::{Event, EventWriter};
use pty_core::registry::{
    self, MetadataPatch, MutateOptions, MutateStatus, SessionMetadata, TagMap,
};
use registry_support::{node_pty, read_events, root, run_node_pty, unique_name, wait_for};
use serde_json::{Value, json};

fn tags(pairs: &[(&str, &str)]) -> TagMap {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn base_metadata() -> SessionMetadata {
    SessionMetadata {
        generation: Some("0123456789abcdef0123456789abcdef".into()),
        daemon_pid: Some(std::process::id() as i32),
        command: "/bin/cat".into(),
        args: vec![],
        display_command: "cat".into(),
        cwd: "/tmp".into(),
        rows: Some(24),
        cols: Some(80),
        ephemeral: Some(false),
        created_at: "2026-04-05T10:15:03.000Z".into(),
        ..Default::default()
    }
}

fn plant(name: &str) -> SessionMetadata {
    let meta = base_metadata();
    registry::write_metadata_publication(name, &meta).unwrap();
    meta
}

fn keys_of(name: &str) -> Vec<String> {
    registry::read_metadata_map(name)
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

/// node: src/server.ts:655-673; node-daemon-protocol-disk.md §2.4
#[test]
fn publication_writes_node_key_order_and_presence_rules() {
    let _ = root();
    let name = unique_name("pub");
    let mut meta = base_metadata();
    meta.tags = Some(tags(&[("role", "web")]));
    meta.display_name = Some("Worker".into());
    meta.isolate_env = Some(true);
    meta.extra_env = Some(tags(&[("A", "1")]));
    meta.unset_env = Some(vec!["NO_COLOR".into()]);
    meta.env = Some(tags(&[("PATH", "/bin")]));
    meta.recovery = Some(json!({"protocol": 1, "processStartToken": "linux:1"}));
    registry::write_metadata_publication(&name, &meta).unwrap();
    assert_eq!(
        keys_of(&name),
        [
            "generation",
            "daemonPid",
            "recovery",
            "command",
            "args",
            "displayCommand",
            "cwd",
            "rows",
            "cols",
            "ephemeral",
            "createdAt",
            "tags",
            "displayName",
            "isolateEnv",
            "extraEnv",
            "unsetEnv",
            "env"
        ]
    );

    // Minimal publication: ephemeral always written, optional maps omitted.
    let name2 = unique_name("pub");
    let mut minimal = base_metadata();
    minimal.ephemeral = None;
    minimal.tags = Some(TagMap::new());
    minimal.isolate_env = Some(false);
    minimal.extra_env = Some(TagMap::new());
    minimal.unset_env = Some(vec![]);
    registry::write_metadata_publication(&name2, &minimal).unwrap();
    assert_eq!(
        keys_of(&name2),
        [
            "generation",
            "daemonPid",
            "command",
            "args",
            "displayCommand",
            "cwd",
            "rows",
            "cols",
            "ephemeral",
            "createdAt"
        ]
    );
    let raw = registry::read_metadata_map(&name2).unwrap();
    assert_eq!(raw["ephemeral"], Value::Bool(false));

    // Pretty JSON, two-space indent, no trailing newline, `[]` for empty arrays.
    let text = std::fs::read_to_string(registry::metadata_path(&name2)).unwrap();
    assert!(text.starts_with("{\n  \"generation\": \""), "{text}");
    assert!(text.contains("\n  \"args\": [],\n"), "{text}");
    assert!(text.ends_with("\n}"), "{text}");
}

#[test]
fn daemon_start_identity_accepts_additive_rust_and_node_recovery_shapes() {
    let rust = SessionMetadata {
        daemon_start_token: Some("linux:rust".into()),
        ..Default::default()
    };
    assert_eq!(rust.daemon_start_token(), Some("linux:rust"));

    let node = SessionMetadata {
        recovery: Some(json!({
            "protocol": 1,
            "processStartToken": "linux:node"
        })),
        ..Default::default()
    };
    assert_eq!(node.daemon_start_token(), Some("linux:node"));
}

/// node: tests/metadata-events.test.ts:169-202
#[test]
fn unknown_fields_and_launch_settings_survive_a_patch() {
    let _ = root();
    let name = unique_name("future");
    let mut raw = plant(&name).to_map();
    raw.insert("rows".into(), json!(41));
    raw.insert("cols".into(), json!(121));
    raw.insert("ephemeral".into(), json!(true));
    raw.insert("isolateEnv".into(), json!(true));
    raw.insert("extraEnv".into(), json!({"ASSIGNED": "yes"}));
    raw.insert("unsetEnv".into(), json!(["NO_COLOR"]));
    raw.insert("futureRecoveryCapability".into(), json!({"version": 2}));
    registry::write_metadata_map(&name, &raw).unwrap();

    let patch = MetadataPatch::from_json(
        &json!({"displayName": "Recovery-safe", "tags": {"owner": "agent"}}),
    )
    .unwrap();
    let result = registry::patch_metadata_by_id(&name, &patch).unwrap();
    assert!(result.changed);
    let m = result.metadata;
    assert_eq!(m.rows, Some(41));
    assert_eq!(m.cols, Some(121));
    assert_eq!(m.ephemeral, Some(true));
    assert_eq!(m.isolate_env, Some(true));
    assert_eq!(m.extra_env, Some(tags(&[("ASSIGNED", "yes")])));
    assert_eq!(m.unset_env, Some(vec!["NO_COLOR".to_string()]));
    assert_eq!(
        m.extra.get("futureRecoveryCapability"),
        Some(&json!({"version": 2}))
    );
    assert_eq!(m.display_name.as_deref(), Some("Recovery-safe"));
    assert_eq!(m.tags, Some(tags(&[("owner", "agent")])));

    // On disk the unknown field is still there, in its original position,
    // and the new keys were appended in the order they were set.
    let keys = keys_of(&name);
    let pos = |k: &str| {
        keys.iter()
            .position(|x| x == k)
            .unwrap_or_else(|| panic!("{k} missing in {keys:?}"))
    };
    assert!(pos("futureRecoveryCapability") < pos("displayName"));
    assert!(pos("displayName") < pos("tags"));
    assert_eq!(
        registry::read_metadata_map(&name).unwrap()["futureRecoveryCapability"],
        json!({"version": 2})
    );
}

/// node: src/sessions.ts:347-398
#[test]
fn mutate_statuses() {
    let root = root();
    let name = unique_name("mut");
    let opts = MutateOptions::default();

    // Missing.
    assert_eq!(
        registry::mutate_metadata_under_lock(&name, |_| true, &opts),
        MutateStatus::Missing
    );
    assert!(
        !root.join(format!("{name}.lock")).exists(),
        "the lock is released on every path"
    );

    let meta = plant(&name);

    // Busy: a live holder of <name>.lock.
    {
        let _held = registry::acquire_lock(&name).unwrap();
        assert_eq!(
            registry::mutate_metadata_under_lock(&name, |_| true, &opts),
            MutateStatus::Busy
        );
    }

    // Generation mismatch, both guards.
    let wrong_gen = MutateOptions {
        expected_generation: Some("ffff".into()),
        expected_metadata: None,
    };
    assert_eq!(
        registry::mutate_metadata_under_lock(&name, |_| true, &wrong_gen),
        MutateStatus::GenerationMismatch
    );
    let mut observed = meta.clone();
    observed.generation = Some("other".into());
    let wrong_obs = MutateOptions {
        expected_generation: None,
        expected_metadata: Some(observed),
    };
    assert_eq!(
        registry::mutate_metadata_under_lock(&name, |_| true, &wrong_obs),
        MutateStatus::GenerationMismatch
    );
    let right = MutateOptions {
        expected_generation: meta.generation.clone(),
        expected_metadata: Some(meta.clone()),
    };

    // Unchanged: the closure declines.
    match registry::mutate_metadata_under_lock(&name, |_| false, &right) {
        MutateStatus::Unchanged(m) => assert_eq!(m.command, "/bin/cat"),
        other => panic!("{other:?}"),
    }

    // Stale: the file changed underneath the mutation.
    let name_for_closure = name.clone();
    let status = registry::mutate_metadata_under_lock(
        &name,
        move |m| {
            let mut raw = registry::read_metadata_map(&name_for_closure).unwrap();
            raw.insert("lastAttachAt".into(), json!("2026-04-05T10:16:00.000Z"));
            registry::write_metadata_map(&name_for_closure, &raw).unwrap();
            m.display_name = Some("lost".into());
            true
        },
        &right,
    );
    assert_eq!(status, MutateStatus::Stale);
    assert_eq!(registry::read_metadata(&name).unwrap().display_name, None);

    // Changed: published and returned, key appended last.
    match registry::mutate_metadata_under_lock(
        &name,
        |m| {
            m.exit_code = Some(7);
            m.exited_at = Some("2026-04-05T10:17:00.000Z".into());
            m.last_lines = Some(vec!["bye".into()]);
            true
        },
        &right,
    ) {
        MutateStatus::Changed(m) => {
            assert_eq!(m.exit_code, Some(7));
            assert_eq!(
                m.last_attach_at.as_deref(),
                Some("2026-04-05T10:16:00.000Z")
            );
        }
        other => panic!("{other:?}"),
    }
    let keys = keys_of(&name);
    assert_eq!(
        &keys[keys.len() - 4..],
        ["lastAttachAt", "exitCode", "exitedAt", "lastLines"]
    );
    assert!(!root.join(format!("{name}.lock")).exists());
}

/// Legacy records without a generation compare structurally.
///
/// node: src/sessions.ts:740-752
#[test]
fn legacy_observation_compares_structurally() {
    let mut a = base_metadata();
    a.generation = None;
    let b = a.clone();
    assert!(registry::metadata_matches_observation(&a, &b));
    let mut c = b.clone();
    c.display_name = Some("x".into());
    assert!(!registry::metadata_matches_observation(&a, &c));
    let mut with_gen = a.clone();
    with_gen.generation = Some("g".into());
    assert!(
        !registry::metadata_matches_observation(&a, &with_gen),
        "observed without generation never matches one with"
    );
    assert!(registry::metadata_matches_observation(&with_gen, &with_gen));
}

/// node: tests/metadata-events.test.ts:263-308
#[test]
fn patch_changes_display_name_and_tags_preserving_unrelated_tags() {
    let _ = root();
    let name = unique_name("patch");
    plant(&name);
    registry::update_tags(
        &name,
        &tags(&[("keep", "yes"), ("replace", "old"), ("remove", "old")]),
        &[],
    )
    .unwrap();

    let patch = MetadataPatch::from_json(&json!({
        "displayName": "Worker",
        "tags": {"replace": "new", "remove": null, "added": "yes"}
    }))
    .unwrap();
    let result = registry::patch_metadata_by_id(&name, &patch).unwrap();
    assert!(result.changed);
    assert_eq!(result.metadata.display_name.as_deref(), Some("Worker"));
    assert_eq!(
        result.metadata.tags,
        Some(tags(&[
            ("keep", "yes"),
            ("replace", "new"),
            ("added", "yes")
        ]))
    );

    let changes: Vec<Value> = read_events(&name)
        .into_iter()
        .filter(|e| e["type"] == "metadata_change")
        .collect();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["session"], name);
    assert!(changes[0]["ts"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        changes[0]["previous"],
        json!({"displayName": null, "tags": {"added": null, "remove": "old", "replace": "old"}})
    );
    assert_eq!(
        changes[0]["value"],
        json!({"displayName": "Worker", "tags": {"added": "yes", "remove": null, "replace": "new"}})
    );
    // Envelope key order: session, type, ts, previous, value.
    let line = std::fs::read_to_string(registry::events_path(&name)).unwrap();
    let last = line.lines().last().unwrap();
    let obj: serde_json::Map<String, Value> = serde_json::from_str(last).unwrap();
    assert_eq!(
        obj.keys().cloned().collect::<Vec<_>>(),
        ["session", "type", "ts", "previous", "value"]
    );
}

/// node: tests/metadata-events.test.ts:310-325
#[test]
fn patch_supports_clear_operations_and_emits_one_coherent_event() {
    let _ = root();
    let name = unique_name("clear");
    plant(&name);
    let first = MetadataPatch::from_json(
        &json!({"displayName": "Before", "tags": {"remove": "yes", "keep": "yes"}}),
    )
    .unwrap();
    registry::patch_metadata_by_id(&name, &first).unwrap();

    let second =
        MetadataPatch::from_json(&json!({"displayName": null, "tags": {"remove": null}})).unwrap();
    let result = registry::patch_metadata_by_id(&name, &second).unwrap();
    assert_eq!(result.metadata.display_name, None);
    assert_eq!(result.metadata.tags, Some(tags(&[("keep", "yes")])));
    let changes: Vec<Value> = read_events(&name)
        .into_iter()
        .filter(|e| e["type"] == "metadata_change")
        .collect();
    assert_eq!(changes.len(), 2);
    assert_eq!(
        changes[1]["previous"],
        json!({"displayName": "Before", "tags": {"remove": "yes"}})
    );
    assert_eq!(
        changes[1]["value"],
        json!({"displayName": null, "tags": {"remove": null}})
    );
    assert!(
        !registry::read_metadata_map(&name)
            .unwrap()
            .contains_key("displayName")
    );
}

/// node: tests/metadata-events.test.ts:327-337
#[test]
fn patch_no_op_writes_nothing_and_emits_nothing() {
    let _ = root();
    let name = unique_name("noop");
    plant(&name);
    let patch =
        MetadataPatch::from_json(&json!({"displayName": "Stable", "tags": {"role": "worker"}}))
            .unwrap();
    registry::patch_metadata_by_id(&name, &patch).unwrap();
    let before_bytes = std::fs::read(registry::metadata_path(&name)).unwrap();
    let before_events = read_events(&name).len();

    let again = MetadataPatch::from_json(
        &json!({"displayName": "Stable", "tags": {"role": "worker", "absent": null}}),
    )
    .unwrap();
    let result = registry::patch_metadata_by_id(&name, &again).unwrap();
    assert!(!result.changed);
    assert_eq!(
        std::fs::read(registry::metadata_path(&name)).unwrap(),
        before_bytes
    );
    assert_eq!(read_events(&name).len(), before_events);
}

/// node: tests/metadata-events.test.ts:339-357; node-cli-surface.md 2.19
#[test]
fn invalid_patches_are_rejected_before_any_write() {
    let _ = root();
    let name = unique_name("invalid");
    plant(&name);
    let before = std::fs::read(registry::metadata_path(&name)).unwrap();
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({"displayName": " Worker"}),
            "Invalid displayName: Display name must be trimmed.",
        ),
        (
            json!({"displayName": "Worker\u{2028}Next"}),
            "Invalid displayName: Display name must be single-line and contain no control characters.",
        ),
        (
            json!({"displayName": "Worker\u{2029}Next"}),
            "Invalid displayName: Display name must be single-line and contain no control characters.",
        ),
        (
            json!({"displayName": "😀".repeat(161)}),
            "Invalid displayName: Display name too long (max 160 Unicode scalars).",
        ),
        (
            json!({"displayName": ""}),
            "Invalid displayName: Display name cannot be empty.",
        ),
        (
            json!({"displayName": 5}),
            "Metadata patch displayName must be a string or null.",
        ),
        (
            json!({"tags": {"": "value"}}),
            "Metadata patch tag keys must be non-empty.",
        ),
        (
            json!({"tags": {"role": 1}}),
            "Metadata patch tag values must be strings or null (invalid key: \"role\").",
        ),
        (
            json!({"tags": []}),
            "Metadata patch tags must be a JSON object.",
        ),
        (
            json!({"tags": null}),
            "Metadata patch tags must be a JSON object.",
        ),
        (
            json!({"unknown": true}),
            "Metadata patch has unknown field \"unknown\". Allowed fields: displayName, tags.",
        ),
        (json!([]), "Metadata patch must be a JSON object."),
        (json!(null), "Metadata patch must be a JSON object."),
        (json!("x"), "Metadata patch must be a JSON object."),
    ];
    for (patch, message) in cases {
        let err = match MetadataPatch::from_json(&patch) {
            Err(e) => e,
            Ok(p) => registry::patch_metadata_by_id(&name, &p).unwrap_err(),
        };
        assert_eq!(err, message, "{patch}");
    }
    assert_eq!(
        std::fs::read(registry::metadata_path(&name)).unwrap(),
        before
    );
    assert!(
        read_events(&name)
            .iter()
            .all(|e| e["type"] != "metadata_change")
    );
}

/// node: tests/metadata-events.test.ts:329-337 (never falls back to a displayName)
#[test]
fn patch_by_id_never_falls_back_to_a_matching_display_name() {
    let _ = root();
    let name = unique_name("nofallback");
    plant(&name);
    registry::set_display_name(&name, Some("missing-id-label")).unwrap();
    let patch = MetadataPatch::from_json(&json!({"tags": {"wrong": "target"}})).unwrap();
    let err = registry::patch_metadata_by_id("missing-id-label", &patch).unwrap_err();
    assert_eq!(err, "Session id \"missing-id-label\" not found.");
    assert_eq!(registry::read_metadata(&name).unwrap().tags, None);
}

/// A missing record is not treated as unknown while a live creation lock
/// proves that publication is still in flight.
#[test]
fn patch_waits_for_a_late_record_during_creation() {
    let _ = root();
    let name = unique_name("late-record");
    let held = registry::acquire_lock(&name).unwrap();
    let publisher_name = name.clone();
    let publisher = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        plant(&publisher_name);
        drop(held);
    });
    let patch = MetadataPatch::from_json(&json!({"tags": {"published": "1"}})).unwrap();
    let result = registry::patch_metadata_by_id(&name, &patch).unwrap();
    assert_eq!(
        result
            .metadata
            .tags
            .as_ref()
            .and_then(|tags| tags.get("published"))
            .map(String::as_str),
        Some("1")
    );
    publisher.join().unwrap();
}

/// A metadata patch waiting for the creation lock must not monopolize the
/// event lock: the daemon's bounded writer still has to publish startup.
#[test]
fn patch_creation_wait_does_not_block_daemon_events() {
    let _ = root();
    let name = unique_name("patch-event-window");
    plant(&name);
    let creation_lock = registry::acquire_lock(&name).unwrap();
    let event_lock = registry::acquire_event_lock(&name).unwrap();
    let patch_name = name.clone();
    let patcher = std::thread::spawn(move || {
        let patch =
            MetadataPatch::from_json(&json!({"tags": {"created-by-child": "1"}})).unwrap();
        registry::patch_metadata_by_id(&patch_name, &patch)
    });

    // Give the patch time to observe both locks held. An event-first patch
    // claims the event lock when it opens; a creation-first patch does not.
    std::thread::sleep(Duration::from_millis(100));
    drop(event_lock);
    std::thread::sleep(Duration::from_millis(100));

    let writer = EventWriter::new(&name);
    writer.append(Event::new(&name, "session_start"));
    let event_published_during_creation = wait_for(1_000, || {
        read_events(&name)
            .iter()
            .any(|event| event["type"] == "session_start")
    });

    drop(creation_lock);
    let result = patcher.join().unwrap().unwrap();
    writer.close();
    assert!(result.changed);
    assert!(
        event_published_during_creation,
        "metadata patch held the event lock while waiting for creation"
    );
}

/// node: tests/metadata-events.test.ts:146-167
#[test]
fn patch_fails_before_any_write_when_the_event_lock_is_held() {
    let _ = root();
    let name = unique_name("evlocked");
    plant(&name);
    registry::update_tags(&name, &tags(&[("seed", "1")]), &[]).unwrap();
    let metadata_before = std::fs::read(registry::metadata_path(&name)).unwrap();
    let events_before = std::fs::read(registry::events_path(&name)).unwrap();
    let _held = registry::acquire_event_lock(&name).unwrap();
    let patch = MetadataPatch::from_json(
        &json!({"displayName": "Blocked", "tags": {"description": "x".repeat(1000)}}),
    )
    .unwrap();
    let started = Instant::now();
    let err = registry::patch_metadata_by_id(&name, &patch).unwrap_err();
    assert_eq!(
        err,
        format!("Session id \"{name}\" event log is busy. Retry the operation.")
    );
    assert!(
        started.elapsed() >= Duration::from_secs(7) && started.elapsed() < Duration::from_secs(20),
        "metadata patch did not honor its bounded event-lock budget: {:?}",
        started.elapsed()
    );
    assert_eq!(
        std::fs::read(registry::metadata_path(&name)).unwrap(),
        metadata_before
    );
    assert_eq!(
        std::fs::read(registry::events_path(&name)).unwrap(),
        events_before
    );
}

/// A creation lock that outlives the eight-second metadata-patch budget
/// surfaces the metadata-busy text and fails closed.
///
/// node: src/sessions.ts `METADATA_PATCH_WAIT_MS`
#[test]
fn patch_reports_metadata_busy_after_the_bounded_wait() {
    let _ = root();
    let name = unique_name("metabusy");
    plant(&name);
    let _held = registry::acquire_lock(&name).unwrap();
    let patch = MetadataPatch::from_json(&json!({"tags": {"blocked": "1"}})).unwrap();
    let started = Instant::now();
    let err = registry::patch_metadata_by_id(&name, &patch).unwrap_err();
    assert_eq!(
        err,
        format!("Session id \"{name}\" metadata is busy. Retry the operation.")
    );
    assert!(
        started.elapsed() >= Duration::from_secs(7) && started.elapsed() < Duration::from_secs(20),
        "metadata patch did not honor its bounded creation-lock budget: {:?}",
        started.elapsed()
    );
}

/// `rename` and `tag` deliberately keep their historical fail-fast
/// contention behavior; only `metadata patch --id` waits out creation.
#[test]
fn rename_and_tag_remain_fail_fast_when_creation_is_locked() {
    let _ = root();
    let name = unique_name("presentation-fast");
    plant(&name);
    let _held = registry::acquire_lock(&name).unwrap();
    let started = Instant::now();
    let rename = registry::set_display_name(&name, Some("blocked")).unwrap_err();
    let tag = registry::update_tags(&name, &tags(&[("blocked", "1")]), &[]).unwrap_err();
    assert_eq!(
        rename,
        format!("Session id \"{name}\" metadata is busy. Retry the operation.")
    );
    assert_eq!(tag, rename);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "fail-fast presentation writes stalled for {:?}",
        started.elapsed()
    );
}

/// A transient creation/attach holder is waited out instead of failing busy.
///
/// compoundingtech/pty#180: `pty run` holds `<name>.lock` across daemon
/// spawn and publication, so a just-spawned attached child's first metadata
/// patch met a held lock. `metadata patch --id` waits up to
/// `METADATA_PATCH_LOCK_WAIT`; a holder that clears quickly is invisible.
#[test]
fn patch_waits_for_a_transient_creation_lock() {
    let _ = root();
    let name = unique_name("transient");
    plant(&name);
    let holder = std::thread::spawn({
        let name = name.clone();
        move || {
            let _held = registry::acquire_lock(&name).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    });
    // Let the holder take the lock first, so the patch really meets it held.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let patch = MetadataPatch::from_json(&json!({"tags": {"waited": "1"}})).unwrap();
    let result = registry::patch_metadata_by_id(&name, &patch).unwrap();
    assert!(result.changed);
    assert_eq!(
        result
            .metadata
            .tags
            .as_ref()
            .and_then(|t| t.get("waited"))
            .map(String::as_str),
        Some("1")
    );
    holder.join().unwrap();
}

/// node: tests/metadata-events.test.ts:420-476
#[test]
fn set_display_name_events() {
    let _ = root();
    let name = unique_name("dn");
    plant(&name);
    registry::set_display_name(&name, Some("my-label")).unwrap();
    let ev = read_events(&name)
        .into_iter()
        .find(|e| e["type"] == "display_name_change")
        .unwrap();
    assert_eq!(ev["previous"], Value::Null);
    assert_eq!(ev["value"], "my-label");

    registry::set_display_name(&name, Some("my-label")).unwrap(); // no-op
    registry::set_display_name(&name, None).unwrap(); // clear
    registry::set_display_name(&name, None).unwrap(); // no-op clear
    registry::set_display_name(&name, Some("")).unwrap(); // "" == clear, still a no-op
    let changes: Vec<Value> = read_events(&name)
        .into_iter()
        .filter(|e| e["type"] == "display_name_change")
        .collect();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[1]["previous"], "my-label");
    assert_eq!(changes[1]["value"], Value::Null);
    assert_eq!(registry::read_metadata(&name).unwrap().display_name, None);
}

/// node: tests/metadata-events.test.ts:525-598
#[test]
fn update_tags_events_carry_full_maps_and_skip_no_ops() {
    let _ = root();
    let name = unique_name("tags");
    plant(&name);
    registry::update_tags(&name, &tags(&[("role", "web")]), &[]).unwrap();
    registry::update_tags(&name, &tags(&[("owner", "forge")]), &[]).unwrap();
    registry::update_tags(&name, &tags(&[("role", "web")]), &[]).unwrap(); // no-op
    registry::update_tags(&name, &TagMap::new(), &["never-was-set".into()]).unwrap(); // no-op
    registry::update_tags(&name, &TagMap::new(), &["role".into()]).unwrap();
    let changes: Vec<Value> = read_events(&name)
        .into_iter()
        .filter(|e| e["type"] == "tags_change")
        .collect();
    assert_eq!(changes.len(), 3);
    assert_eq!(changes[0]["previous"], json!({}));
    assert_eq!(changes[0]["value"], json!({"role": "web"}));
    assert_eq!(changes[1]["previous"], json!({"role": "web"}));
    assert_eq!(
        changes[1]["value"],
        json!({"role": "web", "owner": "forge"})
    );
    assert_eq!(
        changes[2]["previous"],
        json!({"role": "web", "owner": "forge"})
    );
    assert_eq!(changes[2]["value"], json!({"owner": "forge"}));
    // Insertion order is preserved on disk (`role` before `owner`, then removed).
    let raw = registry::read_metadata_map(&name).unwrap();
    assert_eq!(
        raw["tags"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["owner"]
    );

    // Removing the last tag deletes the field; a key set and removed in one
    // call ends removed.
    registry::update_tags(&name, &tags(&[("k", "v")]), &["owner".into(), "k".into()]).unwrap();
    assert!(
        !registry::read_metadata_map(&name)
            .unwrap()
            .contains_key("tags")
    );
    let last = read_events(&name)
        .into_iter()
        .rfind(|e| e["type"] == "tags_change")
        .unwrap();
    assert_eq!(last["value"], json!({}));
}

/// Take a metadata file written by the Node daemon, rewrite it through
/// `update_tags`, and assert the only differences are the appended `tags`
/// map and the appended event — including byte-level formatting parity.
///
/// node: tests/metadata-events.test.ts:169-202
#[test]
fn node_written_metadata_round_trips_byte_for_byte() {
    let Some(bin) = node_pty() else {
        eprintln!("skipping: Node pty 0.12 not on PATH");
        return;
    };
    let root = root();
    let name = unique_name("nodert");
    let run = run_node_pty(
        &bin,
        &root,
        &["run", "-d", "--id", &name, "--no-display-name", "--", "cat"],
    );
    assert!(
        run.status.success(),
        "node pty run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let metadata_path = registry::metadata_path(&name);
    assert!(wait_for(5000, || metadata_path.exists()));

    let original = std::fs::read(&metadata_path).unwrap();
    let original_text = String::from_utf8(original.clone()).unwrap();
    let original_map: serde_json::Map<String, Value> =
        serde_json::from_str(&original_text).unwrap();
    // serde's pretty printer reproduces Node's `JSON.stringify(x, null, 2)`.
    assert_eq!(
        registry::pretty_json(&original_map),
        original_text,
        "pretty-print parity with Node"
    );
    assert!(original_map.contains_key("generation") && original_map.contains_key("daemonPid"));
    let events_before = std::fs::read(registry::events_path(&name)).unwrap();

    let result = registry::update_tags(&name, &tags(&[("role", "web"), ("owner", "forge")]), &[]);

    let kill = run_node_pty(&bin, &root, &["kill", &name]);
    let _ = run_node_pty(&bin, &root, &["rm", &name]);
    let result = result.expect("update_tags on a Node session");
    assert!(result.changed);
    assert!(
        kill.status.success(),
        "node pty kill failed: {}",
        String::from_utf8_lossy(&kill.stderr)
    );

    // Reconstruct the expectation: the original object plus `tags` appended.
    let mut expected = original_map.clone();
    expected.insert("tags".into(), json!({"role": "web", "owner": "forge"}));
    let rewritten = std::fs::read_to_string(&metadata_path).unwrap_or_else(|_| {
        // `pty rm` above removed the file; the copy in `result.metadata` and
        // the pre-kill snapshot below carry the same bytes.
        registry::pretty_json(&result.metadata.to_map())
    });
    // The rewrite as it was on disk right after `update_tags`.
    let rewritten_map: serde_json::Map<String, Value> = serde_json::from_str(&rewritten).unwrap();
    assert_eq!(rewritten_map, expected);
    assert!(
        original_map.contains_key("recovery"),
        "a private root makes the Node daemon advertise recovery"
    );
    assert_eq!(
        result.metadata.to_map()["recovery"],
        original_map["recovery"],
        "recovery preserved verbatim"
    );
    assert_eq!(
        result.metadata.generation.as_deref(),
        original_map["generation"].as_str()
    );

    // Only one event was appended: the `tags_change`.
    let _ = events_before;
    let events = read_events(&name);
    if !events.is_empty() {
        let tag_events: Vec<&Value> = events
            .iter()
            .filter(|e| e["type"] == "tags_change")
            .collect();
        assert_eq!(tag_events.len(), 1);
        assert_eq!(tag_events[0]["previous"], json!({}));
        assert_eq!(
            tag_events[0]["value"],
            json!({"role": "web", "owner": "forge"})
        );
    }
}

/// The file on disk right after `update_tags` is byte-identical to the
/// Node original with `tags` appended — checked while the Node daemon is
/// still running so nothing else has touched the record.
#[test]
fn node_written_metadata_rewrite_is_original_plus_tags() {
    let Some(bin) = node_pty() else {
        eprintln!("skipping: Node pty 0.12 not on PATH");
        return;
    };
    let root = root();
    let name = unique_name("nodebytes");
    let run = run_node_pty(
        &bin,
        &root,
        &[
            "run",
            "-d",
            "--id",
            &name,
            "--name",
            "Näme with späce",
            "--tag",
            "seed=1",
            "--",
            "cat",
        ],
    );
    assert!(
        run.status.success(),
        "node pty run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let metadata_path = registry::metadata_path(&name);
    assert!(wait_for(5000, || metadata_path.exists()));
    let original_text = std::fs::read_to_string(&metadata_path).unwrap();
    let events_before = std::fs::read_to_string(registry::events_path(&name)).unwrap_or_default();

    let result = registry::update_tags(&name, &tags(&[("role", "web")]), &["seed".into()]);
    let rewritten = std::fs::read_to_string(&metadata_path).unwrap();
    let events_after = std::fs::read_to_string(registry::events_path(&name)).unwrap_or_default();
    let _ = run_node_pty(&bin, &root, &["kill", &name]);
    let _ = run_node_pty(&bin, &root, &["rm", &name]);
    result.expect("update_tags on a Node session");

    // Node wrote `"tags": {\n    "seed": "1"\n  }`; the rewrite replaces that
    // one map in place (position kept) and nothing else moves.
    let expected = original_text.replace("\"seed\": \"1\"", "\"role\": \"web\"");
    assert_ne!(
        expected, original_text,
        "the fixture must carry the seed tag: {original_text}"
    );
    assert_eq!(rewritten, expected);

    // Exactly one line was appended to the events log.
    assert!(events_after.starts_with(&events_before));
    let appended: Vec<&str> = events_after[events_before.len()..].lines().collect();
    assert_eq!(appended.len(), 1, "{appended:?}");
    let ev: Value = serde_json::from_str(appended[0]).unwrap();
    assert_eq!(ev["type"], "tags_change");
    assert_eq!(ev["previous"], json!({"seed": "1"}));
    assert_eq!(ev["value"], json!({"role": "web"}));
}

#[test]
fn metadata_patch_typed_validation() {
    let mut p = MetadataPatch::default();
    assert_eq!(p.validate(), Ok(()));
    p.display_name = Some(Some(" x".into()));
    assert_eq!(
        p.validate(),
        Err("Invalid displayName: Display name must be trimmed.".into())
    );
    let mut t = IndexMap::new();
    t.insert(String::new(), Some("v".to_string()));
    let p = MetadataPatch {
        display_name: None,
        tags: Some(t),
    };
    assert_eq!(
        p.validate(),
        Err("Metadata patch tag keys must be non-empty.".into())
    );
}
