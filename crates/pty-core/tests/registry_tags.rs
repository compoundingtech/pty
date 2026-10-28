//! Tag rules: reserved keys, filter matching, `--filter-tag` extraction, the
//! `keep` tag and its retention window, exit-time reap precedence, and gc
//! bookkeeping stripping.

use pty_core::registry::{self, SessionMetadata, TagMap};

fn tags(pairs: &[(&str, &str)]) -> TagMap {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// node: tests/tags-helpers.test.ts:62-94; src/tags.ts:56-79
#[test]
fn reserved_tag_keys() {
    for k in [
        "ptyfile",
        "ptyfile.session",
        "ptyfile.tags",
        "strategy",
        ":l123-abc",
        ":",
    ] {
        assert!(registry::is_reserved_tag_key(k), "{k}");
    }
    for k in [
        "parent",
        "keep",
        "strategy.status",
        "strategy.idle-days",
        "role",
        "ptyfile.other",
        "",
    ] {
        assert!(!registry::is_reserved_tag_key(k), "{k}");
    }
    assert_eq!(
        registry::EXACT_RESERVED_TAG_KEYS,
        &["ptyfile", "ptyfile.session", "ptyfile.tags", "strategy"]
    );
}

/// node: tests/tags-helpers.test.ts:4-60; src/tags.ts:17-46
#[test]
fn matches_all_tags_and_extract_filter_tags() {
    let session = tags(&[("role", "web"), ("env", "prod")]);
    assert!(registry::matches_all_tags(Some(&session), &tags(&[])));
    assert!(registry::matches_all_tags(
        Some(&session),
        &tags(&[("role", "web")])
    ));
    assert!(registry::matches_all_tags(
        Some(&session),
        &tags(&[("role", "web"), ("env", "prod")])
    ));
    assert!(!registry::matches_all_tags(
        Some(&session),
        &tags(&[("role", "web"), ("env", "dev")])
    ));
    assert!(!registry::matches_all_tags(
        Some(&session),
        &tags(&[("owner", "x")])
    ));
    assert!(registry::matches_all_tags(None, &tags(&[])));
    assert!(!registry::matches_all_tags(None, &tags(&[("role", "web")])));

    let mut args: Vec<String> = [
        "--json",
        "--filter-tag",
        "role=web",
        "x",
        "--filter-tag",
        "k=v=w",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let extracted = registry::extract_filter_tags(&mut args).unwrap();
    assert_eq!(extracted, tags(&[("role", "web"), ("k", "v=w")]));
    assert_eq!(args, vec!["--json".to_string(), "x".to_string()]);

    let mut bad: Vec<String> = vec!["--filter-tag".into()];
    assert_eq!(
        registry::extract_filter_tags(&mut bad),
        Err("--filter-tag expects \"key=value\"".into())
    );
    let mut bad: Vec<String> = vec!["--filter-tag".into(), "no-equals".into()];
    assert_eq!(
        registry::extract_filter_tags(&mut bad),
        Err("--filter-tag expects \"key=value\"".into())
    );
    let mut empty_key: Vec<String> = vec!["--filter-tag".into(), "=v".into()];
    assert_eq!(
        registry::extract_filter_tags(&mut empty_key).unwrap(),
        tags(&[("", "v")])
    );
}

/// node: src/sessions.ts:1020-1044; tests/exit-reap.test.ts:772-842
#[test]
fn keep_tag_semantics() {
    assert_eq!(registry::KEEP_TAG, "keep");
    assert!(!registry::is_keep_requested(None));
    assert!(!registry::is_keep_requested(Some(&tags(&[(
        "role", "web"
    )]))));
    for yes in ["true", "1", "yes", "TRUE", "anything", "", " on "] {
        assert!(
            registry::is_keep_requested(Some(&tags(&[("keep", yes)]))),
            "{yes:?}"
        );
    }
    for no in ["false", "0", "no", "off", "FALSE", " Off ", "No"] {
        assert!(
            !registry::is_keep_requested(Some(&tags(&[("keep", no)]))),
            "{no:?}"
        );
    }
}

/// The retention window that bounds `keep` in `pty gc`'s sweep: the anchor
/// precedence, the record that has no age, and the zero that sweeps
/// everything.
///
/// node: src/sessions.ts:1084-1109
#[test]
fn keep_expiry_window() {
    let day = 24 * 60 * 60 * 1000;
    assert_eq!(registry::DEFAULT_KEEP_MAX_AGE_MS, 7 * day);
    let now = registry::parse_iso8601_ms("2026-09-04T12:00:00.000Z").unwrap();
    let at = |offset_days: i64| registry::iso8601_from_epoch_ms(now - offset_days * day);

    // `exitedAt` wins over `createdAt`: created long ago, died just now.
    let fresh_exit = SessionMetadata {
        created_at: at(30),
        exited_at: Some(at(2)),
        ..Default::default()
    };
    assert!(!registry::is_keep_expired(Some(&fresh_exit), now, 7 * day));
    assert!(registry::is_keep_expired(Some(&fresh_exit), now, day));

    // No exit record (a vanished session): `createdAt` is the anchor.
    let vanished = SessionMetadata {
        created_at: at(30),
        ..Default::default()
    };
    assert!(registry::is_keep_expired(Some(&vanished), now, 7 * day));

    // Neither timestamp, or an unparseable one: no age, so never expired —
    // retaining an unaged record is the recoverable failure.
    let unaged = SessionMetadata::default();
    assert!(!registry::is_keep_expired(Some(&unaged), now, 7 * day));
    let bogus = SessionMetadata {
        created_at: "not a timestamp".into(),
        ..Default::default()
    };
    assert!(!registry::is_keep_expired(Some(&bogus), now, 7 * day));
    assert!(!registry::is_keep_expired(None, now, 7 * day));

    // Zero is the explicit "sweep the backlog now": every record expires,
    // including the unaged ones and a session that died this instant.
    for meta in [&fresh_exit, &vanished, &unaged, &bogus] {
        assert!(registry::is_keep_expired(Some(meta), now, 0));
    }
    assert!(registry::is_keep_expired(None, now, 0));

    // The boundary is inclusive: exactly the window old is expired.
    let exactly = SessionMetadata {
        created_at: at(0),
        exited_at: Some(at(7)),
        ..Default::default()
    };
    assert!(registry::is_keep_expired(Some(&exactly), now, 7 * day));
}

/// node: src/sessions.ts:1069-1089
#[test]
fn should_reap_at_exit_precedence() {
    let none: Option<&TagMap> = None;
    // Default reap.
    assert!(registry::should_reap_at_exit(none, false, true));
    assert!(!registry::should_reap_at_exit(none, false, false));
    // Ephemeral forces reap even under preserve.
    assert!(registry::should_reap_at_exit(none, true, false));
    // Permanent preserves…
    let permanent = tags(&[("strategy", "permanent")]);
    assert!(!registry::should_reap_at_exit(
        Some(&permanent),
        false,
        true
    ));
    // …unless ephemeral.
    assert!(registry::should_reap_at_exit(Some(&permanent), true, true));
    // keep beats everything, including ephemeral.
    let keep = tags(&[("keep", "true")]);
    assert!(!registry::should_reap_at_exit(Some(&keep), true, true));
    let keep_false = tags(&[("keep", "false")]);
    assert!(registry::should_reap_at_exit(
        Some(&keep_false),
        false,
        true
    ));
    let keep_and_ephemeral = tags(&[("keep", "yes"), ("strategy", "permanent")]);
    assert!(!registry::should_reap_at_exit(
        Some(&keep_and_ephemeral),
        true,
        true
    ));
}

/// node: src/cli.ts:3667-3672, 4088-4100
#[test]
fn strip_gc_bookkeeping_keys() {
    assert_eq!(
        registry::GC_BOOKKEEPING_KEYS,
        &[
            "strategy.status",
            "strategy.consecutive-fast-fails",
            "strategy.last-respawn-at",
            "strategy.command-hash",
            "retention.lost-at",
        ]
    );
    assert_eq!(registry::strip_gc_bookkeeping(None), None);
    assert_eq!(
        registry::strip_gc_bookkeeping(Some(&tags(&[]))),
        Some(tags(&[]))
    );
    let full = tags(&[
        ("strategy", "permanent"),
        ("strategy.status", "flapping"),
        ("role", "web"),
        ("strategy.consecutive-fast-fails", "3"),
        ("strategy.last-respawn-at", "2026-01-01T00:00:00.000Z"),
        ("strategy.command-hash", "abcd"),
        ("strategy.fast-fail-window", "60"),
    ]);
    let stripped = registry::strip_gc_bookkeeping(Some(&full)).unwrap();
    assert_eq!(
        stripped,
        tags(&[
            ("strategy", "permanent"),
            ("role", "web"),
            ("strategy.fast-fail-window", "60")
        ])
    );
    let only_bookkeeping = tags(&[("strategy.status", "flapping")]);
    assert_eq!(
        registry::strip_gc_bookkeeping(Some(&only_bookkeeping)),
        None
    );
}

/// Retention is off unless `PTY_RETAIN_FINISHED` is a positive duration; a
/// value that cannot be read is off, never "remove now".
#[test]
fn retain_finished_is_off_unless_a_positive_duration() {
    for raw in [
        None,
        Some(""),
        Some("off"),
        Some(" No "),
        Some("false"),
        Some("0"),
        Some("0d"),
        Some("-1d"),
        Some("7"),
        Some("1w"),
        Some("1d12h"),
        Some("99999999999999999d"),
    ] {
        assert_eq!(registry::parse_retain_finished(raw), None, "{raw:?}");
    }
    assert_eq!(registry::parse_retain_finished(Some("90s")), Some(90_000));
    assert_eq!(
        registry::parse_retain_finished(Some(" 7d ")),
        Some(7 * 24 * 60 * 60 * 1000)
    );
}

/// A restart drops the retention stamp with gc's other bookkeeping.
#[test]
fn strip_gc_bookkeeping_drops_the_retention_stamp() {
    let stamped = tags(&[
        (registry::RETENTION_LOST_AT_TAG, "2026-01-01T00:00:00.000Z"),
        ("user", "kept"),
    ]);
    assert_eq!(
        registry::strip_gc_bookkeeping(Some(&stamped)),
        Some(tags(&[("user", "kept")]))
    );
}
