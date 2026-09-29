//! Session id and display-name rules, random ids, the automatic display
//! name, and the two small presentation helpers.

mod registry_support;

use std::path::Path;

use pty_core::registry;
use registry_support::root;

/// node: tests/security-fixes.test.ts:23-44; src/sessions.ts:35-64
#[test]
fn validate_name_messages_in_order() {
    let root = root();
    assert_eq!(registry::validate_name("myserver"), Ok(()));
    assert_eq!(registry::validate_name("normal.dotted-task_id"), Ok(()));
    assert_eq!(
        registry::validate_name(""),
        Err("Session name cannot be empty.".into())
    );
    assert_eq!(
        registry::validate_name("."),
        Err("Invalid session name \".\". Names cannot be \".\" or \"..\".".into())
    );
    assert_eq!(
        registry::validate_name(".."),
        Err("Invalid session name \"..\". Names cannot be \".\" or \"..\".".into())
    );
    assert_eq!(
        registry::validate_name(&"a".repeat(256)),
        Err("Session name too long (max 255 characters).".into())
    );
    assert_eq!(
        registry::validate_name("has/slash"),
        Err("Invalid session name \"has/slash\". Names may only contain letters, numbers, dots, hyphens, and underscores.".into())
    );
    assert_eq!(
        registry::validate_name("has space"),
        Err("Invalid session name \"has space\". Names may only contain letters, numbers, dots, hyphens, and underscores.".into())
    );

    // Socket path limit, computed against the live root.
    let long = "a".repeat(100);
    let err = registry::validate_name(&long).unwrap_err();
    let bytes = root.join(format!("{long}.sock")).as_os_str().len();
    assert_eq!(
        err,
        format!(
            "Session name \"{long}\" produces a socket path of {bytes} bytes, which exceeds the {}-byte kernel limit by {}. Shorten the name or set PTY_SESSION_DIR to a shorter path.",
            registry::SUN_PATH_MAX,
            bytes - registry::SUN_PATH_MAX
        )
    );
    // Exactly +1 over the limit is rejected; exactly at the limit passes.
    let overhead = root.join(".sock").as_os_str().len();
    let overshoot = "a".repeat((registry::SUN_PATH_MAX - overhead + 1).max(1));
    assert!(
        registry::validate_name(&overshoot)
            .unwrap_err()
            .contains("socket path")
    );
    let fits = "a".repeat(registry::SUN_PATH_MAX - overhead);
    assert_eq!(registry::validate_name(&fits), Ok(()));
}

/// node: tests/display-name.test.ts:184-211; tests/metadata-events.test.ts:339-357
#[test]
fn validate_display_name_messages() {
    assert_eq!(registry::validate_display_name("Worker"), Ok(()));
    assert_eq!(
        registry::validate_display_name(""),
        Err("Display name cannot be empty.".into())
    );
    assert_eq!(
        registry::validate_display_name(" Worker"),
        Err("Display name must be trimmed.".into())
    );
    assert_eq!(
        registry::validate_display_name("Worker "),
        Err("Display name must be trimmed.".into())
    );
    assert_eq!(
        registry::validate_display_name(&"😀".repeat(161)),
        Err("Display name too long (max 160 Unicode scalars).".into())
    );
    for bad in [
        "Worker\u{0007}",
        "Worker\u{2028}Next",
        "Worker\u{2029}Next",
        "Two\nLines",
        "Tab\there",
    ] {
        assert_eq!(
            registry::validate_display_name(bad),
            Err("Display name must be single-line and contain no control characters.".into()),
            "{bad:?}"
        );
    }
    // 160 scalars including `/` and `\` are pure metadata.
    let boundary = format!("{}/a\\b", "😀".repeat(156));
    assert_eq!(registry::validate_display_name(&boundary), Ok(()));
    // JavaScript's trim also strips U+FEFF and NBSP.
    assert_eq!(
        registry::validate_display_name("\u{FEFF}Worker"),
        Err("Display name must be trimmed.".into())
    );
    assert_eq!(
        registry::validate_display_name("Worker\u{00A0}"),
        Err("Display name must be trimmed.".into())
    );
}

/// node: src/cli.ts:642-648; tests/display-name.test.ts:66-80
#[test]
fn random_session_name_shape() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..200 {
        let id = registry::random_session_name();
        assert_eq!(id.len(), 8);
        assert!(
            id.bytes()
                .all(|b| registry::SESSION_ID_ALPHABET.contains(&b)),
            "{id}"
        );
        assert!(!id.contains(['0', '1', 'o', 'i', 'l']));
        seen.insert(id);
    }
    assert!(seen.len() > 190, "ids must be random");
    assert_eq!(registry::generate_id().len(), 8);
    assert_eq!(
        registry::unique_id_failure_message(),
        "Could not generate a unique session id after 8 attempts."
    );
}

/// node: src/cli.ts:651-668, 971-973
#[test]
fn auto_display_name_examples() {
    let cwd = Path::new("/home/example/myapp");
    let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        registry::auto_display_name(cwd, "node", &s(&["server.js"])),
        "myapp-node-server"
    );
    assert_eq!(
        registry::auto_display_name(cwd, "/usr/bin/python3", &s(&["-u", "main.py"])),
        "myapp-python3-main"
    );
    assert_eq!(registry::auto_display_name(cwd, "cat", &[]), "myapp-cat");
    // A first arg that is not a clean identifier keeps the bare command.
    assert_eq!(
        registry::auto_display_name(cwd, "sh", &s(&["-c", "echo hi; exit 3"])),
        "myapp-sh"
    );
    // Args of 30+ characters are skipped.
    assert_eq!(
        registry::auto_display_name(cwd, "run", &s(&["a".repeat(30).as_str(), "b"])),
        "myapp-run-b"
    );
    // Extension stripping keeps a dotted stem, and `.bashrc` becomes empty.
    assert_eq!(
        registry::auto_display_name(cwd, "tail", &s(&["app.log.txt"])),
        "myapp-tail-app.log"
    );
    assert_eq!(
        registry::auto_display_name(cwd, "cat", &s(&[".bashrc"])),
        "myapp-cat"
    );
    // Sanitizing: bad characters -> `-`, runs collapsed, edges stripped.
    assert_eq!(
        registry::auto_display_name(Path::new("/tmp/my app!"), "a b", &[]),
        "my-app-a-b"
    );
    assert_eq!(registry::sanitize_display_name("--a__b..c--"), "a__b..c");
    assert_eq!(
        registry::sanitize_display_name("héllo wörld"),
        "h-llo-w-rld"
    );
    assert_eq!(
        registry::auto_display_name(Path::new("/"), "cat", &[]),
        "cat"
    );
}

/// node: src/cli.ts:4114-4130
#[test]
fn short_path_and_time_ago() {
    let home = std::env::var("HOME").unwrap();
    assert_eq!(registry::short_path(&home), "~");
    assert_eq!(registry::short_path(&format!("{home}/src/x")), "~/src/x");
    assert_eq!(
        registry::short_path(&format!("{home}2/src")),
        format!("{home}2/src")
    );
    assert_eq!(registry::short_path("/tmp"), "/tmp");
    assert_eq!(registry::time_ago_from_seconds(0), "0s ago");
    assert_eq!(registry::time_ago_from_seconds(59), "59s ago");
    assert_eq!(registry::time_ago_from_seconds(60), "1m ago");
    assert_eq!(registry::time_ago_from_seconds(3599), "59m ago");
    assert_eq!(registry::time_ago_from_seconds(3600), "1h ago");
    assert_eq!(registry::time_ago_from_seconds(86_399), "23h ago");
    assert_eq!(registry::time_ago_from_seconds(86_400), "1d ago");
    assert_eq!(registry::time_ago_from_seconds(10 * 86_400 + 5), "10d ago");
    let recent = registry::now_iso8601();
    assert!(registry::time_ago(&recent).ends_with("s ago"));
}
