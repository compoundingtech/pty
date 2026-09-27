//! The `--remote` client path (`remote.ts:196-301`): a stub `fabric` on disk
//! prints a control-socket path; a fake control server answers the route and
//! list requests.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread::JoinHandle;
use std::time::Duration;

use common::*;
use pty_client::{RemoteDialer, RemoteError, RemoteSessionRow};
use pty_core::protocol::{MessageType, encode_screen};

const T: Duration = Duration::from_secs(5);

static FABRIC: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// One stub `fabric`, written before any test spawns a child (a script written
/// while another thread forks would be executed with a writer still open →
/// ETXTBSY). `dial <peer> pty-remote` prints `<root>/<peer>.dial`; a missing
/// file is a failing dial.
fn fabric_bin() -> String {
    FABRIC
        .get_or_init(|| {
            let path = test_root().join("fabric");
            std::fs::write(
                &path,
                "#!/bin/sh\ntest \"$1\" = dial || exit 2\ntest \"$3\" = pty-remote || exit 2\nf=\"$(dirname \"$0\")/$2.dial\"\nif [ -f \"$f\" ]; then cat \"$f\"; else echo boom >&2; exit 3; fi\n",
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        })
        .clone()
}

/// Make `fabric dial <peer>` print `socket`.
fn peer(name: &str, socket: &str) -> String {
    let bin = fabric_bin();
    std::fs::write(
        test_root().join(format!("{name}.dial")),
        format!("{socket}\n"),
    )
    .unwrap();
    bin
}

/// A control server: reads the request line and hands it to `f` with the
/// socket (residual bytes untouched).
fn control_server(
    name: &str,
    f: impl Fn(String, UnixStream) + Send + 'static,
) -> (String, JoinHandle<()>) {
    let path = test_root().join(format!("{name}.ctl"));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let h = std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(s) = conn else { break };
            let mut line = String::new();
            let mut reader = BufReader::new(s.try_clone().unwrap());
            reader.read_line(&mut line).unwrap();
            f(line, s);
        }
    });
    (path.to_string_lossy().into_owned(), h)
}

fn dialer(bin: &str) -> RemoteDialer {
    RemoteDialer {
        fabric_bin: bin.to_string(),
        timeout: T,
    }
}

/// node: tests/remote-fabric.test.ts:200-270 — a routed socket speaks the
/// ordinary protocol after the ack line.
#[test]
fn dial_and_route_hands_back_a_routed_socket() {
    let (ctl, _h) = control_server("route-ok", |line, mut s| {
        assert_eq!(line, "{\"op\":\"route\",\"name\":\"demo\"}\n");
        s.write_all(b"{\"ok\":true}\n").unwrap();
        // The first per-session frame follows the ack immediately; then the
        // "daemon" side ends.
        s.write_all(&encode_screen(b"routed")).unwrap();
    });
    let bin = peer("ok", &ctl);
    let mut sock = dialer(&bin).dial_and_route("ok", "demo").unwrap();
    let got = read_packets_until_eof(&mut sock, T);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].type_, MessageType::Screen);
    assert_eq!(got[0].payload, b"routed");
}

/// node: remote.ts:242-248, :254-257
#[test]
fn route_refusal_and_bad_response() {
    let (ctl, _h) = control_server("route-refuse", |_, mut s| {
        s.write_all(b"{\"error\":\"session \\\"demo\\\" not found\"}\n")
            .unwrap();
    });
    let bin = peer("refuse", &ctl);
    let err = dialer(&bin).dial_and_route("refuse", "demo").unwrap_err();
    assert!(err.is_refused(), "{err:?}");
    assert_eq!(err.to_string(), "session \"demo\" not found");

    let (ctl, _h) = control_server("route-bad", |_, mut s| {
        s.write_all(b"garbage line\n").unwrap();
    });
    let bin = peer("bad", &ctl);
    let err = dialer(&bin).dial_and_route("bad", "demo").unwrap_err();
    assert_eq!(err.to_string(), "bad route response: garbage line");

    let (ctl, _h) = control_server("route-notok", |_, mut s| {
        s.write_all(b"{\"ok\":false}\n").unwrap();
    });
    let bin = peer("notok", &ctl);
    let err = dialer(&bin).dial_and_route("notok", "demo").unwrap_err();
    assert!(matches!(err, RemoteError::Refused(_)));
    assert_eq!(err.to_string(), "route refused");
}

/// node: remote.ts:221-224, :259-262
#[test]
fn handshake_timeout_and_close_before_ack() {
    // The two numbers are far apart on purpose. One budget covers BOTH the
    // dial, which spawns a process, and the handshake that follows it. When
    // the budget was 200 ms against a server that slept 500 ms, a slow spawn
    // spent the whole budget before the handshake began and the error came
    // back as a dial timeout rather than a handshake one — the right failure
    // reported by the wrong name. `Silber.mandat-macos` hit it on Apple
    // silicon on 2026-09-02, one run in three at default parallelism. It
    // never failed here in twenty runs, including under load, which is what
    // a race that is tight everywhere and lucky on one machine looks like.
    //
    // A second is more than any spawn needs and five is far more than the
    // handshake is allowed, so neither end is close to the other now.
    let (ctl, _h) = control_server("route-hang", |_, s| {
        std::thread::sleep(Duration::from_secs(5));
        drop(s);
    });
    let bin = peer("hang", &ctl);
    let mut d = dialer(&bin);
    d.timeout = Duration::from_secs(1);
    let err = d.dial_and_route("hang", "demo").unwrap_err();
    assert_eq!(err.to_string(), "route handshake timed out");

    let (ctl, _h) = control_server("route-close", |_, s| drop(s));
    let bin = peer("close", &ctl);
    let err = dialer(&bin).dial_and_route("close", "demo").unwrap_err();
    assert_eq!(err.to_string(), "remote session \"demo\" not reachable");
}

/// node: remote.ts:213-216; tests/remote-fabric.test.ts:129-146 (dial failure text)
#[test]
fn dial_failures() {
    let bin = peer("empty", "");
    let err = dialer(&bin).dial_and_route("empty", "demo").unwrap_err();
    assert_eq!(err.to_string(), "fabric dial empty returned no socket");

    let bin = fabric_bin();
    let err = dialer(&bin).dial_and_route("fail", "demo").unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("Command failed: {bin} dial fail pty-remote\nboom\n")
    );

    let err = dialer("/nonexistent/fabric")
        .dial_and_route("peer", "demo")
        .unwrap_err();
    assert_eq!(err.to_string(), "spawnSync /nonexistent/fabric ENOENT");
}

/// node: tests/remote-fabric.test.ts:86-128 (`{"sessions":[...]}` row shape)
#[test]
fn fetch_remote_list_parses_rows_at_eof() {
    let (ctl, _h) = control_server("list", |line, mut s| {
        assert_eq!(line, "{\"op\":\"list\"}\n");
        s.write_all(br#"{"sessions":[{"name":"abc","status":"running","command":"sleep 300","displayName":"Demo Session"}]}"#).unwrap();
    });
    let rows = dialer("unused").fetch_remote_list(&ctl).unwrap();
    assert_eq!(
        rows,
        vec![RemoteSessionRow {
            name: "abc".into(),
            status: "running".into(),
            command: Some("sleep 300".into()),
            cwd: None,
            tags: None,
            display_name: Some("Demo Session".into()),
        }]
    );
    assert_eq!(
        serde_json::to_string(&rows[0]).unwrap(),
        r#"{"name":"abc","status":"running","command":"sleep 300","displayName":"Demo Session"}"#
    );

    let (ctl, _h) = control_server("list-err", |_, mut s| {
        s.write_all(br#"{"error":"nope"}"#).unwrap();
    });
    assert_eq!(
        dialer("unused")
            .fetch_remote_list(&ctl)
            .unwrap_err()
            .to_string(),
        "nope"
    );

    let (ctl, _h) = control_server("list-bad", |_, mut s| {
        s.write_all(b"<html>").unwrap();
    });
    let err = dialer("unused").fetch_remote_list(&ctl).unwrap_err();
    assert!(
        err.to_string().starts_with("bad remote response: "),
        "{err}"
    );
}
