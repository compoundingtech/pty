//! Port of tests/effective-geometry.test.ts: the effective terminal size is
//! the per-axis minimum over writable attached clients, and every GEOMETRY
//! frame is delivered to a socket before any SCREEN/DATA produced for that
//! size. The Node suite drives a `PtyServer` in-process and three client
//! flavours (raw sockets, `SessionConnection`, the testing `Session`,
//! `attachPty`); here every flavour collapses onto raw sockets through
//! `Rig::connect`, the daemon is a real `pty run -d`, and grid checks are
//! made through `pty peek --plain`.
//!
//! The child is the same `node -e` script as Node's: on SIGWINCH it clears
//! the screen and prints `X` × (cols * 2), so a resize always produces two
//! full rows of output.

use pty_conformance::*;
use pty_core::protocol::{MessageType, Packet, decode_geometry};
use std::time::{Duration, Instant};

const WIDTH_SCRIPT: &str = "process.on('SIGWINCH', () => setImmediate(() => {\n  const width = process.stdout.columns || 0;\n  process.stdout.write('\\x1b[2J\\x1b[H' + 'X'.repeat(width * 2));\n}));\nsetInterval(() => {}, 1000);";

fn start(rig: &Rig, id: &str) {
    rig.daemon(id, &["node", "-e", WIDTH_SCRIPT], DaemonOpts::no_display_name());
}

/// A connection that keeps every packet it has received.
struct Rec {
    conn: Conn,
    packets: Vec<Packet>,
}

impl Rec {
    fn open(rig: &Rig, id: &str) -> Rec {
        Rec {
            conn: rig.connect(id),
            packets: Vec::new(),
        }
    }

    fn pump(&mut self) {
        while let Some(p) = self.conn.next_packet(Duration::from_millis(20)) {
            self.packets.push(p);
        }
    }

    /// Drop everything received so far (including what is still queued on
    /// the socket, which Node's event loop would already have recorded).
    fn clear(&mut self) {
        self.pump();
        self.packets.clear();
    }

    fn wait_for(&mut self, what: &str, pred: impl Fn(&[Packet]) -> bool) {
        let start = Instant::now();
        loop {
            self.pump();
            if pred(&self.packets) {
                return;
            }
            assert!(start.elapsed() < deadline(), "timed out waiting for {what}");
        }
    }
}

/// Poll `pred` over several recorders' packets, pumping them all.
fn wait_for_all(recs: &mut [&mut Rec], what: &str, pred: impl Fn(&[&[Packet]]) -> bool) {
    let start = Instant::now();
    loop {
        for r in recs.iter_mut() {
            r.pump();
        }
        let views: Vec<&[Packet]> = recs.iter().map(|r| r.packets.as_slice()).collect();
        if pred(&views) {
            return;
        }
        assert!(start.elapsed() < deadline(), "timed out waiting for {what}");
    }
}

fn geometry_index(packets: &[Packet], rows: u16, cols: u16) -> Option<usize> {
    packets
        .iter()
        .position(|p| p.type_ == MessageType::Geometry && decode_geometry(&p.payload) == (rows, cols))
}

fn output_indices(packets: &[Packet]) -> Vec<usize> {
    packets
        .iter()
        .enumerate()
        .filter(|(_, p)| p.type_ == MessageType::Screen || p.type_ == MessageType::Data)
        .map(|(i, _)| i)
        .collect()
}

fn has(packets: &[Packet], t: MessageType) -> bool {
    packets.iter().any(|p| p.type_ == t)
}

#[track_caller]
fn expect_geometry_before_all_output(packets: &[Packet], rows: u16, cols: u16) {
    let names = sequence_names(&packets.iter().map(|p| p.type_).collect::<Vec<_>>());
    let geometry = geometry_index(packets, rows, cols)
        .unwrap_or_else(|| panic!("no GEOMETRY({rows},{cols}) in {names:?}"));
    let output = output_indices(packets);
    assert!(!output.is_empty(), "no SCREEN/DATA in {names:?}");
    assert!(
        output.iter().all(|&i| geometry < i),
        "GEOMETRY({rows},{cols}) at {geometry} is not before all output in {names:?}"
    );
}

fn settle() {
    std::thread::sleep(Duration::from_millis(250));
}

fn terminal_cols(rig: &Rig, id: &str) -> u64 {
    let mut c = rig.connect(id);
    let s = c.status_json(Duration::from_secs(2));
    s["terminal"]["cols"].as_u64().expect("terminal.cols")
}

/// The two-row resize write appears as one logical plain line in Rust and
/// two physical rows in Node. This test checks the resized width in either.
fn has_resize_output(rig: &Rig, id: &str, width: usize) -> bool {
    let out = rig.pty(&["peek", "--plain", id]);
    if out.status != 0 {
        return false;
    }
    let row = "X".repeat(width);
    let logical = "X".repeat(width * 2);
    let text = out.stdout();
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    (lines.len() == 1 && lines[0] == logical)
        || (lines.len() >= 2 && lines[0] == row && lines[1] == row)
}

/// node: tests/effective-geometry.test.ts:152
#[test]
fn orders_geometry_before_output_for_existing_and_smaller_attaching_clients() {
    let rig = Rig::new();
    start(&rig, "geo1");
    let mut large = Rec::open(&rig, "geo1");
    large.conn.attach(24, 20);
    large.wait_for("large initial attach", |p| {
        geometry_index(p, 24, 20).is_some() && has(p, MessageType::Screen)
    });
    expect_geometry_before_all_output(&large.packets, 24, 20);
    settle();
    large.clear();

    let mut small = Rec::open(&rig, "geo1");
    small.conn.attach(24, 10);
    wait_for_all(&mut [&mut large, &mut small], "smaller peer attach output", |v| {
        geometry_index(v[0], 24, 10).is_some()
            && has(v[0], MessageType::Data)
            && geometry_index(v[1], 24, 10).is_some()
            && !output_indices(v[1]).is_empty()
    });
    expect_geometry_before_all_output(&large.packets, 24, 10);
    expect_geometry_before_all_output(&small.packets, 24, 10);
}

/// node: tests/effective-geometry.test.ts:180
#[test]
fn orders_geometry_before_resize_and_disconnect_output() {
    let rig = Rig::new();
    start(&rig, "geo2");
    let mut large = Rec::open(&rig, "geo2");
    large.conn.attach(24, 20);
    large.wait_for("large geometry", |p| geometry_index(p, 24, 20).is_some());
    let mut small = Rec::open(&rig, "geo2");
    small.conn.attach(24, 10);
    large.wait_for("small geometry", |p| geometry_index(p, 24, 10).is_some());
    settle();

    large.clear();
    small.clear();
    small.conn.resize(24, 8);
    wait_for_all(&mut [&mut large, &mut small], "peer resize output", |v| {
        geometry_index(v[0], 24, 8).is_some()
            && has(v[0], MessageType::Data)
            && geometry_index(v[1], 24, 8).is_some()
    });
    expect_geometry_before_all_output(&large.packets, 24, 8);
    expect_geometry_before_all_output(&small.packets, 24, 8);
    settle();

    large.clear();
    drop(small);
    large.wait_for("peer disconnect output", |p| {
        geometry_index(p, 24, 20).is_some() && has(p, MessageType::Data)
    });
    expect_geometry_before_all_output(&large.packets, 24, 20);
}

/// node: tests/effective-geometry.test.ts:217
#[test]
fn streams_geometry_to_read_only_viewers_without_letting_them_select_size() {
    let rig = Rig::new();
    start(&rig, "geo3");
    let mut writable = Rec::open(&rig, "geo3");
    writable.conn.attach(24, 20);
    writable.wait_for("writable geometry", |p| geometry_index(p, 24, 20).is_some());

    let mut read_only = Rec::open(&rig, "geo3");
    read_only.conn.peek(false, false);
    read_only.wait_for("read-only initial geometry", |p| {
        geometry_index(p, 24, 20).is_some() && has(p, MessageType::Screen)
    });
    expect_geometry_before_all_output(&read_only.packets, 24, 20);
    assert_eq!(terminal_cols(&rig, "geo3"), 20);
    settle();

    read_only.clear();
    writable.conn.resize(24, 10);
    read_only.wait_for("read-only resized geometry", |p| {
        geometry_index(p, 24, 10).is_some() && has(p, MessageType::Data)
    });
    expect_geometry_before_all_output(&read_only.packets, 24, 10);

    drop(writable);
    drop(read_only);
    wait_until("zero-viewer last size", || terminal_cols(&rig, "geo3") == 10);
}

/// The geometry columns a client has been told, in order.
fn geometry_cols(packets: &[Packet]) -> Vec<u16> {
    packets
        .iter()
        .filter(|p| p.type_ == MessageType::Geometry)
        .map(|p| decode_geometry(&p.payload).1)
        .collect()
}

/// node: tests/effective-geometry.test.ts:252
#[test]
fn updates_effective_geometry_before_subsequent_data() {
    let rig = Rig::new();
    start(&rig, "geo4");
    let mut large = Rec::open(&rig, "geo4");
    large.conn.attach(24, 20);
    large.wait_for("large connect", |p| has(p, MessageType::Screen));
    assert_eq!(geometry_cols(&large.packets), vec![20]);

    let mut small = Rec::open(&rig, "geo4");
    small.conn.attach(24, 10);
    small.wait_for("small connect", |p| has(p, MessageType::Screen));
    large.wait_for("attach geometry", |p| geometry_cols(p).last() == Some(&10));
    // A larger request from the constrained client changes nothing.
    large.conn.resize(24, 30);
    std::thread::sleep(Duration::from_millis(50));
    large.pump();
    assert_eq!(geometry_cols(&large.packets).last(), Some(&10));
    small.conn.resize(24, 8);
    large.wait_for("resize geometry", |p| geometry_cols(p).last() == Some(&8));
    small.conn.detach();
    drop(small);
    large.wait_for("disconnect geometry", |p| geometry_cols(p).last() == Some(&30));

    assert_eq!(geometry_cols(&large.packets), vec![20, 10, 8, 30]);
}

/// node: tests/effective-geometry.test.ts:277
#[test]
fn resizes_the_grid_on_peer_attach_resize_and_disconnect() {
    let rig = Rig::new();
    start(&rig, "geo5");
    let mut large = Rec::open(&rig, "geo5");
    large.conn.attach(24, 20);
    large.wait_for("large attach", |p| has(p, MessageType::Screen));
    let mut small = Rec::open(&rig, "geo5");
    small.conn.attach(24, 10);
    small.wait_for("small attach", |p| has(p, MessageType::Screen));
    large.wait_for("testing attach geometry", |p| geometry_cols(p).last() == Some(&10));
    wait_until("testing attach grid", || has_resize_output(&rig, "geo5", 10));
    large.conn.resize(24, 30);
    std::thread::sleep(Duration::from_millis(50));
    large.pump();
    assert_eq!(geometry_cols(&large.packets).last(), Some(&10));

    small.conn.resize(24, 8);
    large.wait_for("testing resize geometry", |p| geometry_cols(p).last() == Some(&8));
    wait_until("testing resize grid", || has_resize_output(&rig, "geo5", 8));

    small.conn.detach();
    drop(small);
    large.wait_for("testing disconnect geometry", |p| geometry_cols(p).last() == Some(&30));
    wait_until("testing disconnect grid", || has_resize_output(&rig, "geo5", 30));
}

/// node: tests/effective-geometry.test.ts:304
#[test]
fn resizes_the_terminal_on_peer_attach_resize_and_disconnect() {
    // Node's attachPty variant runs `sleep 30`; the grid width is what the
    // daemon reports as its terminal size.
    let rig = Rig::new();
    rig.daemon("geo6", &["sleep", "30"], DaemonOpts::no_display_name());
    let mut large = Rec::open(&rig, "geo6");
    large.conn.attach(24, 20);
    large.wait_for("large attach", |p| has(p, MessageType::Screen));
    let mut small = Rec::open(&rig, "geo6");
    small.conn.attach(24, 10);
    small.wait_for("small attach", |p| has(p, MessageType::Screen));
    large.wait_for("attachPty attach geometry", |p| geometry_cols(p).last() == Some(&10));
    assert_eq!(terminal_cols(&rig, "geo6"), 10);
    large.conn.resize(24, 30);
    std::thread::sleep(Duration::from_millis(50));
    large.pump();
    assert_eq!(geometry_cols(&large.packets).last(), Some(&10));
    assert_eq!(terminal_cols(&rig, "geo6"), 10);

    small.conn.resize(24, 8);
    large.wait_for("attachPty resize geometry", |p| geometry_cols(p).last() == Some(&8));
    assert_eq!(terminal_cols(&rig, "geo6"), 8);

    drop(small);
    large.wait_for("attachPty disconnect geometry", |p| geometry_cols(p).last() == Some(&30));
    assert_eq!(terminal_cols(&rig, "geo6"), 30);
}
