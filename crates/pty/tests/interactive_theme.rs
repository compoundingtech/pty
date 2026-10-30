//! The session picker must let the terminal's own theme through by default.
//!
//! **Asserting that a config value equals "terminal" would prove nothing.**
//! What matters is what lands on the terminal, so these tests run the real
//! binary in a real pty, parse its output with a real VT, and assert on the
//! serialized terminal state.
//!
//! node: `src/tui/interactive.ts` — `loadSavedThemeIndex` returns `terminalIdx`
//! when nothing is saved, and `themes.terminal` has all thirteen slots `null`.

use pty_testkit::{Session, SpawnOptions};

/// Every SGR sequence in the serialized state that SETS a colour.
///
/// `39`, `49` and `59` are excluded on purpose: they reset foreground,
/// background and underline colour to the terminal's default, which is the
/// opposite of painting over it.
fn colour_sequences(ansi: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes: Vec<char> = ansi.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '\u{1b}' && i + 1 < bytes.len() && bytes[i + 1] == '[' {
            let start = i;
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == ';') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == 'm' {
                let body: String = bytes[start + 2..j].iter().collect();
                if sets_a_colour(&body) {
                    found.push(format!("\\x1b[{body}m"));
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    found
}

fn sets_a_colour(body: &str) -> bool {
    let mut parts = body.split(';').filter(|p| !p.is_empty()).peekable();
    while let Some(p) = parts.next() {
        // 38 / 48 introduce an indexed or truecolour value.
        if p == "38" || p == "48" {
            return true;
        }
        if let Ok(n) = p.parse::<u16>()
            && ((30..=37).contains(&n)
                || (40..=47).contains(&n)
                || (90..=97).contains(&n)
                || (100..=107).contains(&n))
        {
            return true;
        }
    }
    false
}

fn picker(root: &std::path::Path) -> Session {
    // `build_spawn_env` scrubs PTY_SESSION for us, so the nesting guard does
    // not fire, and honours an explicit PTY_ROOT. crossterm paints no colour
    // while NO_COLOR is non-empty, so an inherited one would blind both checks
    // here; an empty value is unset by the NO_COLOR convention.
    Session::spawn(
        env!("CARGO_BIN_EXE_pty"),
        &[],
        SpawnOptions {
            rows: Some(24),
            cols: Some(80),
            env: vec![
                ("PTY_ROOT".to_string(), root.to_string_lossy().into_owned()),
                ("NO_COLOR".to_string(), String::new()),
            ],
            ..Default::default()
        },
    )
    .expect("spawn the picker")
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pty-theme-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp root");
    dir
}

/// With nothing saved, the picker must paint no colour at all, so whatever the
/// user's terminal is configured with shows through.
#[test]
fn the_default_picker_paints_no_colour() {
    let root = temp_root("default");
    let mut s = picker(&root);
    // Wait for something to be drawn rather than sampling an empty frame.
    let drawn = s.wait_for(|ss| !ss.text.trim().is_empty(), 8000, "the picker to draw");
    let ansi = match drawn {
        Ok(ss) => ss.ansi,
        Err(_) => s.screenshot().ansi,
    };
    let painted = colour_sequences(&ansi);
    assert!(
        painted.is_empty(),
        "the default picker painted {} colour sequence(s), so it overrides the \
         terminal's theme: {:?}",
        painted.len(),
        &painted[..painted.len().min(6)]
    );
    let _ = s.press("q");
    s.close();
    let _ = std::fs::remove_dir_all(&root);
}

/// **The control for the test above.** A saved theme that does paint must be
/// detected, or the first test would pass by being unable to see anything.
#[test]
fn a_saved_painted_theme_is_visible_to_the_same_check() {
    let root = temp_root("painted");
    std::fs::write(root.join("theme"), "coolBlue\n").expect("save a theme");
    let mut s = picker(&root);
    let drawn = s.wait_for(|ss| !ss.text.trim().is_empty(), 8000, "the picker to draw");
    let ansi = match drawn {
        Ok(ss) => ss.ansi,
        Err(_) => s.screenshot().ansi,
    };
    let painted = colour_sequences(&ansi);
    assert!(
        !painted.is_empty(),
        "coolBlue painted nothing, so the check in the sibling test cannot \
         distinguish a painted theme from an unpainted one"
    );
    let _ = s.press("q");
    s.close();
    let _ = std::fs::remove_dir_all(&root);
}

/// The classifier itself, so the two tests above rest on something checked.
#[test]
fn only_colour_setting_sequences_count() {
    // Resets to the terminal's default are not painting.
    assert!(colour_sequences("\x1b[0m\x1b[39m\x1b[49m\x1b[59m").is_empty());
    assert!(colour_sequences("\x1b[1m\x1b[22m\x1b[4m").is_empty());
    // Every way of setting one is.
    assert_eq!(colour_sequences("\x1b[31m").len(), 1, "basic foreground");
    assert_eq!(colour_sequences("\x1b[41m").len(), 1, "basic background");
    assert_eq!(colour_sequences("\x1b[91m").len(), 1, "bright foreground");
    assert_eq!(colour_sequences("\x1b[38;5;33m").len(), 1, "indexed");
    assert_eq!(colour_sequences("\x1b[38;2;10;20;30m").len(), 1, "truecolour");
    assert_eq!(colour_sequences("\x1b[48;2;10;20;30m").len(), 1, "truecolour bg");
    // Mixed with attributes.
    assert_eq!(colour_sequences("\x1b[1;38;2;1;2;3m").len(), 1);
}
