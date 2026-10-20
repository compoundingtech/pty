//! The peer half of `--remote`: one request on stdin, then either a JSON
//! answer or a spliced session socket.
//!
//! `fabric expose pty-remote --exec -- pty remote-serve --stdio` runs one of
//! these per connection, with stdin and stdout wired to the dialing side.
//! The protocol is one `\n`-terminated JSON line in, one `\n`-terminated
//! JSON line out, and for a route the raw session bytes after that.
//!
//! node: src/remote.ts (the server half), src/cli.ts:1331-1339 (usage)

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use pty_core::registry::{self, SessionInfo};
use serde::Serialize;
use serde_json::Value;

/// One row of the `list` answer. A field is present only when it is set, so
/// the shape matches what the dialing side expects to parse.
///
/// node: src/remote.ts (`listSessions` response)
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionRow {
    name: String,
    status: String,
    identity: registry::DaemonIdentityCheck,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<registry::TagMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
}

impl SessionRow {
    fn of(s: &SessionInfo) -> Self {
        let meta = s.metadata.as_ref();
        SessionRow {
            name: s.name.clone(),
            status: s.status.as_str().to_string(),
            identity: s.identity,
            // `command` is the command as the user typed it.
            command: meta
                .map(|m| m.display_command.clone())
                .filter(|c| !c.is_empty()),
            cwd: meta.map(|m| m.cwd.clone()).filter(|c| !c.is_empty()),
            tags: meta
                .and_then(|m| m.tags.clone())
                .filter(|t| !t.is_empty()),
            display_name: meta.and_then(|m| m.display_name.clone()),
        }
    }
}

fn write_line(out: &mut impl Write, value: &impl Serialize) {
    let line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn write_error(out: &mut impl Write, message: &str) {
    write_line(out, &serde_json::json!({ "error": message }));
}

/// `pty remote-serve --stdio`. Reads the ambient `PTY_ROOT`.
pub fn serve_stdio() -> i32 {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();

    // The request line, byte at a time: anything after the newline belongs to
    // the routed session, so it must not be swallowed by a buffered read.
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stdin.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => line.push(byte[0]),
            Err(_) => break,
        }
    }

    let Ok(request) = serde_json::from_slice::<Value>(&line) else {
        write_error(&mut stdout, "malformed request");
        return 0;
    };
    match request.get("op").and_then(Value::as_str) {
        Some("list") => {
            let rows: Vec<SessionRow> = registry::list_sessions().iter().map(SessionRow::of).collect();
            write_line(&mut stdout, &serde_json::json!({ "sessions": rows }));
            0
        }
        Some("route") => {
            let name = request.get("name").and_then(Value::as_str).unwrap_or_default();
            route(name, &mut stdout)
        }
        Some(op) => {
            write_error(&mut stdout, &format!("unknown op: {op}"));
            0
        }
        None => {
            write_error(&mut stdout, "malformed request");
            0
        }
    }
}

/// Connect to `<name>.sock` and splice it to this process's stdin and
/// stdout until either side closes.
fn route(reference: &str, stdout: &mut impl Write) -> i32 {
    let session = match registry::get_session(reference) {
        Ok(Some(s)) => s,
        // An ambiguous display name reports the registry's own text; a
        // missing one reports the not-found shape the dialing side matches.
        Err(ambiguous) => {
            write_error(stdout, &ambiguous);
            return 0;
        }
        Ok(None) => {
            write_error(stdout, &format!("session \"{reference}\" not found"));
            return 0;
        }
    };
    // A socket that will not connect is reported as the same not-found, so a
    // caller cannot tell a dead session from an absent one — nor needs to.
    let Ok(socket) = UnixStream::connect(registry::socket_path(&session.name)) else {
        write_error(stdout, &format!("session \"{reference}\" not found"));
        return 0;
    };

    write_line(stdout, &serde_json::json!({ "ok": true }));
    splice(socket);
    0
}

/// Splice the session socket to this process's stdin and stdout until
/// either side ends.
///
/// It works on the descriptors rather than the locked handles: the request
/// line was read through a lock, and a second thread taking that same lock
/// would block for as long as the tunnel lives.
fn splice(socket: UnixStream) {
    use std::fs::File;
    use std::mem::ManuallyDrop;
    use std::os::fd::FromRawFd;

    let Ok(mut to_session) = socket.try_clone() else {
        return;
    };
    let mut from_session = socket;

    std::thread::scope(|scope| {
        // Anything the dialing side sent after its request line is still
        // unread, because the request was read one byte at a time.
        scope.spawn(move || {
            // SAFETY: fd 0 is this process's stdin for its whole life; the
            // wrapper is never dropped, so the descriptor is not closed.
            let mut stdin = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
            let mut buf = [0u8; 16384];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if to_session.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            // The caller has stopped talking; let the session see it.
            let _ = to_session.shutdown(std::net::Shutdown::Write);
        });

        // SAFETY: as above, for fd 1.
        let mut stdout = ManuallyDrop::new(unsafe { File::from_raw_fd(1) });
        let mut buf = [0u8; 16384];
        loop {
            match from_session.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = stdout.flush();
        let _ = from_session.shutdown(std::net::Shutdown::Both);
        // The session is finished, so the tunnel is. Leaving normally would
        // wait for the reader thread, which is blocked on a stdin that the
        // dialing side may hold open indefinitely.
        std::process::exit(0);
    });
}
