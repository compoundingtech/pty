//! `pty send`: DATA framing and pacing, ported from `client.ts:221-288`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use pty_core::paste::{BRACKETED_PASTE_END, BRACKETED_PASTE_START};
use pty_core::protocol::encode_data;
use pty_core::registry;

use super::{ClientError, GoneSet, connect_session, map_io_error};

/// Default gap between `--seq` items (node's `DEFAULT_SEQ_DELAY_MS`). A
/// trailing `key:return` fired with zero delay routinely lands before the
/// program has parsed the typed text; 300 ms lets each chunk be consumed.
pub const DEFAULT_SEQ_DELAY_MS: u64 = 300;

/// Resolve the `--with-delay <sec>` argument to milliseconds, matching node's
/// `resolveSeqDelayMs`: absent → 300; explicit → `Math.round(sec * 1000)`.
pub fn resolve_seq_delay_ms(delay_secs: Option<f64>) -> u64 {
    match delay_secs {
        None => DEFAULT_SEQ_DELAY_MS,
        Some(n) => (n * 1000.0).round() as u64,
    }
}

/// How `send` paces and wraps its items.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendOptions {
    /// Sleep this long BETWEEN items (not before the first, not after the
    /// last); 0 streams them back to back.
    pub delay_ms: u64,
    /// Wrap the whole payload in bracketed-paste markers, each sent as its own
    /// DATA packet.
    pub paste: bool,
}

/// How long `send` waits for the daemon to close its side after we shut down
/// ours, so the bytes are in the daemon's receive buffer before we exit.
const FINISH_WAIT: Duration = Duration::from_secs(2);

/// Send `items` to a local session as separate DATA packets. Silent on
/// success. No ATTACH, no implicit newline.
///
/// node: tests/send-paste.test.ts:121-219
pub fn send<T: AsRef<[u8]>>(name: &str, items: &[T], opts: SendOptions) -> Result<(), ClientError> {
    let socket = connect_session(name)?;
    send_over(socket, name, false, items, opts)
}

/// [`send`] to a session in the registry at `root` instead of `$PTY_ROOT`.
pub fn send_in<T: AsRef<[u8]>>(
    root: &Path,
    name: &str,
    items: &[T],
    opts: SendOptions,
) -> Result<(), ClientError> {
    registry::with_root(root, || send(name, items, opts))
}

/// [`send`] over an already-connected socket (a `--remote` route). `name` is
/// only used for the error text; `remote` selects the `Remote session …`
/// wording.
pub fn send_over<T: AsRef<[u8]>>(
    mut socket: UnixStream,
    name: &str,
    remote: bool,
    items: &[T],
    opts: SendOptions,
) -> Result<(), ClientError> {
    let path = registry::socket_path(name);
    let map =
        |e: &std::io::Error| map_io_error(name, remote, GoneSet::Broad, "write", Some(&path), e);
    let paste = if opts.paste && !items.is_empty() {
        let status_socket = socket.try_clone().map_err(|e| map(&e))?;
        let status = crate::stats::query_status_json_over(
            status_socket,
            name,
            crate::stats::STATS_TIMEOUT,
        )?;
        serde_json::from_str::<pty_core::stats::StatsResult>(&status)
            .map_err(|_| ClientError::InvalidStats(name.to_string()))?
            .modes
            .bracketed_paste
    } else {
        false
    };
    if paste {
        socket
            .write_all(&encode_data(BRACKETED_PASTE_START.as_bytes()))
            .map_err(|e| map(&e))?;
    }
    for (i, item) in items.iter().enumerate() {
        if i > 0 && opts.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(opts.delay_ms));
        }
        socket
            .write_all(&encode_data(item.as_ref()))
            .map_err(|e| map(&e))?;
    }
    if paste {
        socket
            .write_all(&encode_data(BRACKETED_PASTE_END.as_bytes()))
            .map_err(|e| map(&e))?;
    }
    socket.flush().map_err(|e| map(&e))?;
    // `socket.end()` + exit on 'finish': half-close, then give the daemon a
    // moment to consume and close. Errors here are irrelevant — the bytes are
    // already in the daemon's receive buffer.
    let _ = socket.shutdown(std::net::Shutdown::Write);
    let _ = socket.set_read_timeout(Some(FINISH_WAIT));
    let mut sink = [0u8; 1024];
    while let Ok(n) = socket.read(&mut sink) {
        if n == 0 {
            break;
        }
    }
    Ok(())
}
