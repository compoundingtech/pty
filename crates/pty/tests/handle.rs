//! `TerminalHandle`: spawning, attaching to a real daemon, attach identity
//! across a replacement under the same id, and late-event rejection; and the
//! terminal behaviour that needs a real child on the other end — input
//! reaching it, query answers echoed back by it, and images it draws.
//!
//! The attach tests run this crate's own `pty` binary, each on its own
//! `PTY_ROOT` under the temp dir.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pty::{AttachOptions, HandleEvent, SessionRef, SpawnOptions, TerminalHandle};
use pty_terminal::graphics::PLACEHOLDER;
use pty_terminal::input::{Key, KeyEvent, MouseButton, MouseEvent};
use pty_terminal::{GraphicsOptions, PlacementPosition, Range};

fn wait_text(h: &TerminalHandle, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = h.plain(Range::Full);
        if text.contains(needle) {
            return text;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {needle:?}; screen:\n{text}");
        h.wait_rev(h.rev(), Duration::from_millis(200));
    }
}

/// node: tests/pty-handle.test.ts (createPty), issue #1
#[test]
fn spawn_cat_write_and_snapshot() {
    let h = TerminalHandle::spawn("cat", &[], SpawnOptions::default()).expect("spawn");
    assert!(h.wait_ready(Duration::from_secs(1)));
    let events = h.subscribe();
    h.write(b"hello\r");
    let text = wait_text(&h, "hello");
    assert!(text.starts_with("hello"), "{text:?}");
    let g = h.snapshot(0);
    assert_eq!(g.rows[0][..5].iter().map(|c| c.text.as_str()).collect::<String>(), "hello");
    assert!(matches!(events.try_recv(), Ok(HandleEvent::Dirty(_))));
    assert!(!h.exited());
    h.kill();
    assert!(h.exited() || !h.connected());
}

#[test]
fn spawn_reports_exit_code() {
    // The child waits for a line before it exits, so the subscription is in
    // place before the exit it has to hear. A child that exits at once can do
    // so before `subscribe` returns, and then the event goes to nobody.
    let h = TerminalHandle::spawn("sh", &["-c", "read _; printf done; exit 3"], SpawnOptions::default())
        .expect("spawn");
    let events = h.subscribe();
    h.write(b"\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !h.exited() && Instant::now() < deadline {
        h.wait_rev(h.rev(), Duration::from_millis(100));
    }
    assert_eq!(h.exit_code(), Some(3));
    // The PTY echoed the line the child read; its output follows it.
    assert_eq!(h.plain(Range::Viewport), "\ndone");
    let mut saw_exit = false;
    while let Ok(ev) = events.try_recv() {
        if ev == HandleEvent::Exited(3) {
            saw_exit = true;
        }
    }
    assert!(saw_exit);
    h.kill();
}

#[test]
fn spawn_resize_changes_the_grid_and_emits_geometry() {
    let h = TerminalHandle::spawn("cat", &[], SpawnOptions::default()).expect("spawn");
    let events = h.subscribe();
    h.resize(40, 10);
    let deadline = Instant::now() + Duration::from_secs(2);
    while (h.cols(), h.rows()) != (40, 10) && Instant::now() < deadline {
        h.wait_rev(h.rev(), Duration::from_millis(50));
    }
    assert_eq!((h.cols(), h.rows()), (40, 10));
    let g = h.snapshot(0);
    assert_eq!((g.cols, g.rows_n), (40, 10));
    let mut saw = false;
    while let Ok(ev) = events.try_recv() {
        if ev == HandleEvent::Geometry(10, 40) {
            saw = true;
        }
    }
    assert!(saw);
    h.kill();
}

// ── against the Rust daemon ──

struct Rig {
    bin: PathBuf,
    root: PathBuf,
}

impl Rig {
    fn new() -> Rig {
        let bin = PathBuf::from(env!("CARGO_BIN_EXE_pty"));
        // A counter, not a clock: `Instant::now().elapsed()` is however long
        // those two calls took, which is nanoseconds and often the same
        // number twice. Every rig in this process was getting the same
        // directory, so tests running in parallel shared a registry and
        // fought over the session id they all use.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "pty-handle-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create the test's PTY_ROOT");
        Rig { bin, root }
    }

    fn pty(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(&self.bin)
            .args(args)
            .env("PTY_ROOT", &self.root)
            .env_remove("PTY_SESSION")
            .env_remove("PTY_SESSION_DIR")
            .env_remove("PTY_SERVER_CONFIG")
            .env("PTY_REAP_ON_EXIT", "false")
            .output()
            .expect("run pty");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn run(&self, id: &str, script: &str) {
        let (code, out, err) = self.pty(&["run", "-d", "--id", id, "--", "sh", "-c", script]);
        assert_eq!(code, 0, "pty run failed: {out}{err}");
        // `pty run -d` returns as soon as the session looks up; when a
        // preserved session under the same id exists, that can be before the
        // replacement's socket is listening.
        let sock = self.root.join(format!("{id}.sock"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !sock.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(sock.exists(), "socket for {id} never appeared");
    }

    fn session(&self, id: &str) -> SessionRef {
        SessionRef {
            root: self.root.clone(),
            id: id.to_string(),
        }
    }

    fn kill(&self, id: &str) {
        let _ = self.pty(&["kill", id]);
        let sock = self.root.join(format!("{id}.sock"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while sock.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        for entry in std::fs::read_dir(&self.root).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".pid") {
                let _ = self.pty(&["kill", id]);
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// node: src/tui/builders.ts:600-779 (attachPty): ATTACH, SCREEN replay,
/// DATA, DETACH; readiness is the first SCREEN.
#[test]
fn attach_replays_screen_streams_data_and_detaches() {
    let rig = Rig::new();
    rig.run("a", "printf 'first\\n'; exec cat");
    let h = TerminalHandle::attach(rig.session("a"), AttachOptions::default()).expect("attach");
    assert!(h.wait_ready(Duration::from_secs(5)), "first SCREEN");
    assert!(h.is_ready());
    let text = h.plain(Range::Full);
    assert!(text.contains("first"), "{text:?}");
    h.write(b"typed\r");
    wait_text(&h, "typed");
    h.kill();
    assert!(!h.connected());
    // The daemon is still running after DETACH.
    assert!(rig.root.join("a.sock").exists());
    rig.kill("a");
}

/// A read-only attach never sends input.
#[test]
fn readonly_attach_drops_input() {
    let rig = Rig::new();
    rig.run("r", "printf 'ro\\n'; exec cat");
    let h = TerminalHandle::attach(
        rig.session("r"),
        AttachOptions {
            readonly: true,
            ..Default::default()
        },
    )
    .expect("attach");
    assert!(h.wait_ready(Duration::from_secs(5)));
    h.write(b"nope\r");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!h.plain(Range::Full).contains("nope"));
    h.kill();
    rig.kill("r");
}

/// node-daemon-protocol-disk.md §1.12 / conformance fixture "attach identity
/// with a replacement under the same id": `--id a`, exit, `--id a` again — a
/// reconnect reaches the replacement, and nothing from the old daemon (its
/// EXIT, its screen) survives into the new attempt.
#[test]
fn attach_identity_reconnect_reaches_the_replacement() {
    let rig = Rig::new();
    rig.run("a", "printf 'first\\n'; exec sleep 60");
    let h = TerminalHandle::attach(rig.session("a"), AttachOptions::default()).expect("attach");
    assert!(h.wait_ready(Duration::from_secs(5)));
    assert!(h.plain(Range::Full).contains("first"));
    let first_attempt = h.attempt();

    // The first daemon goes away. On an external kill the daemon destroys
    // its client sockets before the child dies (server.ts:1364-1372), so
    // the handle sees the socket close, not an EXIT.
    rig.kill("a");
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.connected() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!h.connected());

    // A replacement under the same id.
    rig.run("a", "printf 'second\\n'; exec sleep 60");
    h.reconnect().expect("reconnect");
    assert!(h.attempt() > first_attempt);
    assert!(!h.exited(), "the old EXIT does not belong to the new attempt");
    assert!(h.wait_ready(Duration::from_secs(5)), "replacement SCREEN");
    let text = h.plain(Range::Full);
    assert!(text.contains("second"), "{text:?}");
    assert!(!text.contains("first"), "old screen must be gone: {text:?}");
    h.kill();
    rig.kill("a");
}

/// The case a Node daemon cannot serve: the child draws a kitty image, and a
/// client attaches only afterwards. The image was in `DATA` that this client
/// never saw, so everything it knows comes from the daemon's `SCREEN` — which
/// carries the image because the session's own terminal holds it
/// (docs/decisions/0012-kitty-graphics-replay.md).
///
/// The bytes are OMP's (`packages/tui/src/terminal-capabilities.ts`
/// `encodeKittyTransmit`, `packages/tui/src/kitty-graphics.ts`
/// `encodeKittyVirtualPlacement` / `encodeKittyPlaceholderGrid`): a `f=100`
/// PNG transmission, a virtual placement, and a placeholder cell carrying the
/// image id in its foreground colour and the placement id in its underline
/// colour.
#[test]
fn a_late_attach_gets_the_image_the_child_drew_before_it_connected() {
    let rig = Rig::new();
    // A 1x1 red PNG (the kitty protocol's own example image), image id 4242,
    // placement id 7, one placeholder cell at image row 0, column 0.
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
    let script = format!(
        "printf 'drawn\\n'; \
         printf '\\033_Ga=t,f=100,q=2,i=4242;{png}\\033\\\\'; \
         printf '\\033_Ga=p,U=1,q=2,i=4242,p=7,c=1,r=1\\033\\\\'; \
         printf '\\033[38;2;0;16;146m\\033[58:2::0:0:7m\u{10eeee}\u{305}\u{305}\\033[39;59m'; \
         exec cat"
    );
    rig.run("g", &script);
    // Let the child finish drawing before anyone attaches: this client must
    // learn the image from the replay, not from live DATA.
    std::thread::sleep(Duration::from_millis(400));

    let h = TerminalHandle::attach(
        rig.session("g"),
        AttachOptions {
            graphics: Some(GraphicsOptions::DEFAULT),
            ..Default::default()
        },
    )
    .expect("attach");
    assert!(h.wait_ready(Duration::from_secs(5)), "first SCREEN");
    assert!(h.plain(Range::Full).contains("drawn"));

    let deadline = Instant::now() + Duration::from_secs(5);
    let state = loop {
        let state = h.graphics(0);
        if !state.placements.is_empty() {
            break state;
        }
        assert!(
            Instant::now() < deadline,
            "the replay carried no placement; screen:\n{}",
            h.plain(Range::Full)
        );
        h.wait_rev(h.rev(), Duration::from_millis(100));
    };

    let image = state.image(4242).expect("the image came with the replay");
    assert_eq!((image.width, image.height), (1, 1));
    assert_eq!(
        h.image_bytes(4242).map(|b| b.data),
        Some(vec![255, 0, 0, 255]),
        "the pixels themselves, decoded from the PNG"
    );
    let p = &state.placements[0];
    assert_eq!((p.image_id, p.placement_id), (4242, 7));
    assert!(p.is_virtual);
    assert!(
        matches!(p.position, PlacementPosition::Placeholder(_)),
        "located by its placeholder cell, at {:?}",
        p.position
    );

    // A reconnect resets the terminal and replays a fresh SCREEN. The image
    // has to come back with it: the reset must not take the storage away, or
    // the replay's own transmission would be rejected.
    let position = p.position;
    h.reconnect().expect("reconnect");
    assert!(h.wait_ready(Duration::from_secs(5)), "SCREEN after reconnect");
    let deadline = Instant::now() + Duration::from_secs(5);
    let after = loop {
        let state = h.graphics(0);
        if !state.placements.is_empty() {
            break state;
        }
        assert!(Instant::now() < deadline, "reconnect lost the image");
        h.wait_rev(h.rev(), Duration::from_millis(100));
    };
    assert_eq!(
        h.image_bytes(4242).map(|b| b.data),
        Some(vec![255, 0, 0, 255])
    );
    assert_eq!(after.placements[0].placement_id, 7);
    assert_eq!(after.placements[0].position, position, "same cell");

    h.kill();
    rig.kill("g");
}

/// A daemon-side detach is a state a consumer has to be able to enter, so it
/// is an event, not something to poll `connected()` for. A reconnect is the
/// symmetric one: `Connected` means the new socket is up, and the attempt's
/// first SCREEN follows.
#[test]
fn a_lost_socket_and_a_reconnect_are_both_announced() {
    let rig = Rig::new();
    rig.run("d", "printf 'first\\n'; exec sleep 60");
    let h = TerminalHandle::attach(rig.session("d"), AttachOptions::default()).expect("attach");
    assert!(h.wait_ready(Duration::from_secs(5)));
    let events = h.subscribe();

    rig.kill("d");
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.connected() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!h.connected());
    let mut saw_disconnected = false;
    while let Ok(ev) = events.try_recv() {
        if ev == HandleEvent::Disconnected {
            saw_disconnected = true;
        }
    }
    assert!(saw_disconnected, "the lost socket was announced");

    rig.run("d", "printf 'second\\n'; exec sleep 60");
    h.reconnect().expect("reconnect");
    assert!(h.wait_ready(Duration::from_secs(5)));
    let mut saw_connected = false;
    while let Ok(ev) = events.try_recv() {
        if ev == HandleEvent::Connected(h.attempt()) {
            saw_connected = true;
        }
    }
    assert!(saw_connected, "the new socket was announced");
    h.kill();
    rig.kill("d");
}

/// Cell metrics live on the client's host — its font — but the session's
/// terminal is what answers geometry for a placement that left `c=`/`r=`
/// implicit, including in the replay every other client gets. So the client
/// declares them on ATTACH and RESIZE, and the daemon adopts them.
#[test]
fn a_client_declares_its_cell_size_and_the_session_geometry_follows() {
    let rig = Rig::new();
    // A 16x16 RGBA image (f=32, 1024 pixel bytes) placed with no c=/r=, so
    // its cell extent is purely derived from the cell size.
    let px = format!("{}==", "A".repeat(1366));
    let script = format!(
        "printf 'drawn\\n'; \
         printf '\\033_Ga=t,q=2,i=77,f=32,s=16,v=16;{px}\\033\\\\'; \
         printf '\\033_Ga=p,q=2,i=77,p=1\\033\\\\'; \
         exec cat"
    );
    rig.run("c", &script);
    std::thread::sleep(Duration::from_millis(400));

    let graphics = pty_terminal::GraphicsOptions {
        cell: pty_terminal::CellSize {
            width: 16,
            height: 16,
        },
        ..GraphicsOptions::DEFAULT
    };
    let h = TerminalHandle::attach(
        rig.session("c"),
        AttachOptions {
            graphics: Some(graphics),
            ..Default::default()
        },
    )
    .expect("attach");
    assert!(h.wait_ready(Duration::from_secs(5)));

    // 16x16 pixels at a declared 16x16 cell is 1x1 cells, on both sides: the
    // client's own terminal and the daemon's, which was told on ATTACH.
    let deadline = Instant::now() + Duration::from_secs(5);
    let state = loop {
        let state = h.graphics(0);
        if !state.placements.is_empty() {
            break state;
        }
        assert!(
            Instant::now() < deadline,
            "no placement; screen:\n{}",
            h.plain(Range::Full)
        );
        h.wait_rev(h.rev(), Duration::from_millis(100));
    };
    assert!(state.cell_declared, "the client declared its cell size");
    assert_eq!(state.placements[0].cell_size, (1, 1));
    assert_eq!(state.placements[0].requested_cells, (0, 0));

    // The daemon's own answer, via a PEEK-based read-only attach that never
    // declares anything: the replay it gets carries the image, and the
    // session's terminal is what resolved the geometry.
    let (code, out, err) = rig.pty(&["peek", "c"]);
    assert_eq!(code, 0, "peek failed: {out}{err}");
    assert!(
        out.contains("i=77"),
        "the session's replay carries the image: {out:?}"
    );

    // A later declaration travels on RESIZE and moves the derived extent.
    h.set_cell_size(8, 16);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = h.graphics(0);
        if state.placements.first().map(|p| p.cell_size) == Some((2, 1)) {
            assert_eq!(state.cell, pty_terminal::CellSize { width: 8, height: 16 });
            break;
        }
        assert!(Instant::now() < deadline, "the new cell size never took");
        h.wait_rev(h.rev(), Duration::from_millis(100));
    }

    h.kill();
    rig.kill("c");
}

#[test]
fn attach_to_missing_session_is_an_error() {
    let root = std::env::temp_dir();
    let r = TerminalHandle::attach(
        SessionRef {
            root,
            id: "definitely-not-a-session".into(),
        },
        AttachOptions::default(),
    );
    assert!(r.is_err());
}

// ── input reaches the child ──

/// The handle path: the events are `Send`, the encoding happens on the actor
/// thread against the live terminal, and `send_*` is ordered with `write`.
#[test]
fn send_key_reaches_the_child_and_encode_key_agrees() {
    let h = TerminalHandle::spawn("cat", &[], SpawnOptions::default()).expect("spawn");
    assert!(h.wait_ready(Duration::from_secs(2)));

    assert_eq!(h.encode_key(&KeyEvent::press(Key::ArrowUp)), b"\x1b[A");
    assert_eq!(
        h.encode_mouse(&MouseEvent::press(MouseButton::Left, 3, 4)),
        None,
        "no tracking, no report"
    );

    // `cat` echoes: what the child received comes back on the screen.
    h.send_key(&KeyEvent::typed(Key::A, "a", Some('a')));
    h.send_key(&KeyEvent::press(Key::Enter));
    let grid = h
        .wait_for(Duration::from_secs(5), |g| g.text().starts_with('a'))
        .expect("the child got the key");
    assert!(grid.text().starts_with('a'));

    h.send_paste("pasted");
    let grid = h
        .wait_for(Duration::from_secs(5), |g| g.text().contains("pasted"))
        .expect("the child got the paste");
    assert!(grid.text().contains("pasted"));
    h.kill();
}

// ── query answers, echoed back by a real child ──

fn spawn_echo(script: &str) -> TerminalHandle {
    TerminalHandle::spawn("sh", &["-c", script], SpawnOptions::default()).expect("spawn")
}

/// node: tests/terminal-queries.test.ts:94-105
#[test]
fn child_sees_da1_answer() {
    let h = spawn_echo("printf '\\033[c'; exec cat");
    wait_text(&h, "62;22");
    h.kill();
}

/// node: tests/terminal-queries.test.ts:107-116
#[test]
fn child_sees_osc11_answer() {
    let h = spawn_echo("printf '\\033]11;?\\033\\\\'; exec cat");
    wait_text(&h, "0000/0000/0000");
    h.kill();
}

/// node: tests/terminal-queries.test.ts:118-126
#[test]
fn child_sees_osc10_answer() {
    let h = spawn_echo("printf '\\033]10;?\\033\\\\'; exec cat");
    wait_text(&h, "c0c0/c0c0/c0c0");
    h.kill();
}

/// node: tests/terminal-queries.test.ts:128-138
#[test]
fn child_sees_dsr_answer() {
    let h = spawn_echo("printf '\\033[6n'; exec cat");
    let text = wait_text(&h, "R");
    let has_cpr = text.lines().any(|l| {
        let Some(r) = l.find('R') else { return false };
        let before = &l[..r];
        let Some(semi) = before.rfind(';') else { return false };
        let col = &before[semi + 1..];
        let row: String = before[..semi]
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        !row.is_empty() && !col.is_empty() && col.chars().all(|c| c.is_ascii_digit())
    });
    assert!(has_cpr, "expected a \\d+;\\d+R report:\n{text}");
    h.kill();
}

/// node: tests/terminal-queries.test.ts:140-149
#[test]
fn child_sees_da2_answer() {
    let h = spawn_echo("printf '\\033[>c'; exec cat");
    wait_text(&h, "382");
    h.kill();
}

// ── images a real child draws ──

/// A 16x8 RGBA PNG, the same bytes `pty-terminal`'s graphics tests decode.
const PNG_16X8_BASE64: &str = concat!(
    "iVBORw0KGgoAAAANSUhEUgAAABAAAAAICAYAAADwdn+XAAAAqUlEQVR4nBXMwYBFIQAAwIewCCGEEEIIIYQQQggh",
    "hBBCCCFk8Hc6zHW+72u/PwKRRKZQaXQGk8XmcPm+ICAQSWQKlUZnMFlsDje8IAkIRBKZQqXRGUwWm8NNLygCApFE",
    "plBpdAaTxeZwywuagEAkkSlUGp3BZLE53PaCISAQSWQKlUZnMFlsDne8YAkIRBKZQqXRGUwWm8NdLzgCApFEplBp",
    "dAaTxeZw+Qf7mTOQwddl2QAAAABJRU5ErkJggg=="
);

const IMAGE_ID: u32 = 4242;
const PLACEMENT_ID: u32 = 7;

/// The row/column diacritics OMP indexes into, first four entries.
const DIACRITICS: [char; 4] = ['\u{305}', '\u{30d}', '\u{30e}', '\u{310}'];

/// OMP's whole render (`encodeKittyTransmit`, `encodeKittyVirtualPlacement`,
/// `encodeKittyPlaceholderGrid`): a `f=100` PNG transmission, the virtual
/// placement in front of the first placeholder row, and placeholder cells
/// carrying the image id in their foreground colour and the placement id in
/// their underline colour, rows separated by CR/LF.
fn omp_image(id: u32, pid: u32, cols: usize, rows: usize) -> String {
    let mut out = format!("\x1b_Ga=t,f=100,q=2,i={id};{PNG_16X8_BASE64}\x1b\\");
    let fg = format!("\x1b[38;2;{};{};{}m", (id >> 16) & 0xff, (id >> 8) & 0xff, id & 0xff);
    let ul = format!("\x1b[58:2::{}:{}:{}m", (pid >> 16) & 0xff, (pid >> 8) & 0xff, pid & 0xff);
    for r in 0..rows {
        if r == 0 {
            out.push_str(&format!("\x1b_Ga=p,U=1,q=2,i={id},p={pid},c={cols},r={rows}\x1b\\"));
        } else {
            out.push_str("\r\n");
        }
        out.push_str(&format!("{fg}{ul}"));
        for c in 0..cols {
            out.push(PLACEHOLDER);
            out.push(DIACRITICS[r]);
            out.push(DIACRITICS[c]);
        }
        out.push_str("\x1b[39;59m");
    }
    out
}

/// The handle path: a real child in a real PTY, the state read from another
/// thread. Kitty graphics need a per-thread PNG decoder and an `!Send`
/// terminal, so this is the case that proves the actor thread set both up.
#[test]
fn a_spawned_child_that_draws_an_image_is_queryable_through_the_handle() {
    let sequence = omp_image(IMAGE_ID, PLACEMENT_ID, 2, 2);
    let h = TerminalHandle::spawn(
        "cat",
        &[],
        SpawnOptions {
            rows: 10,
            cols: 20,
            graphics: Some(GraphicsOptions::DEFAULT),
            ..SpawnOptions::default()
        },
    )
    .expect("spawn");
    assert!(h.wait_ready(Duration::from_secs(2)));
    let events = h.subscribe();

    // `cat` echoes what we write, so the child is the one emitting the
    // sequence into the terminal.
    h.write(sequence.as_bytes());

    let deadline = Instant::now() + Duration::from_secs(5);
    let state = loop {
        let state = h.graphics(0);
        if !state.placements.is_empty() {
            break state;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the image; screen:\n{}",
            h.plain(Range::Full)
        );
        h.wait_rev(h.rev(), Duration::from_millis(100));
    };

    assert!(state.enabled);
    let image = state.image(IMAGE_ID).expect("the image is stored");
    assert_eq!((image.width, image.height), (16, 8));
    assert_eq!(
        h.image_bytes(IMAGE_ID).map(|b| b.data.len()),
        Some(16 * 8 * 4)
    );
    assert_eq!(h.graphics_generation(), state.generation);

    let p = &state.placements[0];
    assert_eq!((p.image_id, p.placement_id), (IMAGE_ID, PLACEMENT_ID));
    assert!(matches!(p.position, PlacementPosition::Placeholder(_)));

    let mut saw_graphics = false;
    while let Ok(ev) = events.try_recv() {
        if matches!(ev, HandleEvent::Graphics(g) if g == state.generation) {
            saw_graphics = true;
        }
    }
    assert!(saw_graphics, "the storage change is announced");

    h.clear_graphics();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !h.graphics(0).placements.is_empty() {
        assert!(Instant::now() < deadline, "clear_graphics did not take");
        h.wait_rev(h.rev(), Duration::from_millis(100));
    }
    assert!(h.image_bytes(IMAGE_ID).is_none());
    h.kill();
}
