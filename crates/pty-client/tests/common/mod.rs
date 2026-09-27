//! Shared helpers for the client tests: a scripted fake daemon on a unix
//! socket, pipes, and byte collectors. Every client operation is exercised
//! against packet sequences written here, so these tests do not need a real
//! daemon (the conformance suite covers that later).
#![allow(dead_code)]

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pty_core::protocol::{MessageType, Packet, PacketReader};

static ROOT: OnceLock<PathBuf> = OnceLock::new();
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A per-process temp dir, exported as `PTY_ROOT` once so `registry::socket_path`
/// resolves into it. Short path (unix socket names are capped at 108 bytes).
pub fn test_root() -> PathBuf {
    ROOT.get_or_init(|| {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "t".into());
        let short: String = exe.chars().take(12).collect();
        let dir = std::env::temp_dir().join(format!("ptyc-{}-{short}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test root");
        // Safety: set before any test thread reads it (OnceLock init).
        unsafe { std::env::set_var("PTY_ROOT", &dir) };
        // Say this once, here, rather than letting the tests that bind a
        // socket fail with "path must be shorter than SUN_LEN" and look like
        // a defect in the port.
        //
        // The budget is the rig's, not the product's. `pty` generates short
        // ids and its own check allows for those; a test picks its own name,
        // and the longest in the suite is 16 characters plus a counter. So a
        // root that satisfies the product can still be too long here, which
        // is exactly what made this confusing the first time.
        //
        // macOS caps the socket path at 104 bytes where Linux allows 108,
        // and macOS's own temp directory already spends about 49 of them.
        // Anything that nests inside it eats the rest: a `nix develop` shell
        // took 16 more and turned twenty tests red on 2026-09-02.
        let longest = dir.join("a-session-name-of-the-length-tests-use.sock");
        assert!(
            longest.as_os_str().len() <= pty_core::registry::SUN_PATH_MAX,
            "the test root is too long for a unix socket path: {} bytes of {} at
  {}
  \
             This is the test harness, not the software. Point TMPDIR at a SHORTER path.\n  \
             Shorter is the whole requirement, and a short path on a disk-backed \
             filesystem is the safe answer: on some machines /tmp is a RAM disk \
             shared with everything else on the box.",
            longest.as_os_str().len(),
            pty_core::registry::SUN_PATH_MAX,
            dir.display()
        );
        dir
    })
    .clone()
}

/// A session name unique within this test binary.
pub fn unique_name(prefix: &str) -> String {
    format!("{prefix}{}", COUNTER.fetch_add(1, Ordering::SeqCst))
}

/// A listening `<root>/<name>.sock`.
pub struct FakeDaemon {
    pub name: String,
    pub path: PathBuf,
    pub listener: UnixListener,
}

impl FakeDaemon {
    pub fn bind(prefix: &str) -> FakeDaemon {
        let name = unique_name(prefix);
        let path = test_root().join(format!("{name}.sock"));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind fake daemon");
        FakeDaemon {
            name,
            path,
            listener,
        }
    }

    pub fn accept(&self) -> UnixStream {
        self.listener.accept().expect("accept").0
    }

    pub fn connect(&self) -> UnixStream {
        UnixStream::connect(&self.path).expect("connect fake daemon")
    }

    /// Run `script` for each accepted connection on a background thread.
    pub fn serve<F>(self, script: F) -> JoinHandle<()>
    where
        F: Fn(usize, UnixStream) + Send + 'static,
    {
        std::thread::spawn(move || {
            let mut n = 0;
            for conn in self.listener.incoming() {
                n += 1;
                match conn {
                    Ok(s) => script(n, s),
                    Err(_) => break,
                }
            }
        })
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One blocking read (the client's first packet, usually ATTACH/PEEK).
pub fn read_chunk(s: &mut UnixStream) -> Vec<u8> {
    let mut buf = [0u8; 65536];
    let n = s.read(&mut buf).expect("read chunk");
    buf[..n].to_vec()
}

/// Read packets until EOF (or `timeout`), parsed.
pub fn read_packets_until_eof(s: &mut UnixStream, timeout: Duration) -> Vec<Packet> {
    let _ = s.set_read_timeout(Some(timeout));
    let mut reader = PacketReader::new();
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend(reader.feed(&buf[..n]).unwrap()),
        }
    }
    out
}

/// Block until a packet of `type_` arrives (collecting everything before it).
pub fn read_until(
    s: &mut UnixStream,
    reader: &mut PacketReader,
    type_: MessageType,
    timeout: Duration,
) -> Vec<Packet> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for {type_:?}; got {out:?}"
        );
        let _ = s.set_read_timeout(Some(remaining));
        match s.read(&mut buf) {
            Ok(0) => panic!("eof waiting for {type_:?}; got {out:?}"),
            Ok(n) => {
                for p in reader.feed(&buf[..n]).unwrap() {
                    let hit = p.type_ == type_;
                    out.push(p);
                    if hit {
                        return out;
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => panic!("read error waiting for {type_:?}: {e}"),
        }
    }
}

/// Wait until the peer has sent bytes we have not read (without reading them).
pub fn wait_unread(s: &UnixStream, timeout: Duration) {
    let mut fds = [libc::pollfd {
        fd: s.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    let n = pty_client::tty::poll(&mut fds, timeout.as_millis() as i32).unwrap();
    assert!(n > 0, "no unread bytes arrived");
}

/// The packet types in a byte stream.
pub fn types(bytes: &[u8]) -> Vec<MessageType> {
    PacketReader::new()
        .feed(bytes)
        .unwrap()
        .into_iter()
        .map(|p| p.type_)
        .collect()
}

/// Parse a byte stream.
pub fn packets(bytes: &[u8]) -> Vec<Packet> {
    PacketReader::new().feed(bytes).unwrap()
}

/// Concatenate framed packets.
pub fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flatten().copied().collect()
}

pub struct Pipe {
    pub r: OwnedFd,
    pub w: OwnedFd,
}

pub fn pipe() -> Pipe {
    // Not `libc::pipe2` directly: Apple has no such call, and this helper
    // is the one place that difference is handled.
    let fds = pty_client::tty::cloexec_pipe(false).expect("pipe");
    unsafe {
        Pipe {
            r: OwnedFd::from_raw_fd(fds[0]),
            w: OwnedFd::from_raw_fd(fds[1]),
        }
    }
}

/// Drains a read end on a thread into a shared buffer.
pub struct Collector {
    data: Arc<Mutex<Vec<u8>>>,
    handle: Option<JoinHandle<()>>,
}

pub fn collect(fd: OwnedFd) -> Collector {
    let data = Arc::new(Mutex::new(Vec::new()));
    let sink = data.clone();
    let handle = std::thread::spawn(move || {
        let mut file = std::fs::File::from(fd);
        let mut buf = [0u8; 65536];
        loop {
            match file.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
            }
        }
    });
    Collector {
        data,
        handle: Some(handle),
    }
}

impl Collector {
    pub fn snapshot(&self) -> Vec<u8> {
        self.data.lock().unwrap().clone()
    }

    /// Wait until `pred(bytes so far)` holds.
    pub fn wait_for(&self, timeout: Duration, pred: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        loop {
            let snap = self.snapshot();
            if pred(&snap) {
                return snap;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting; have {:?}",
                String::from_utf8_lossy(&snap)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Wait for a packet of `type_` in the framed stream so far.
    pub fn wait_for_packet(&self, type_: MessageType, timeout: Duration) -> Vec<Packet> {
        let bytes = self.wait_for(timeout, |b| types(b).contains(&type_));
        packets(&bytes)
    }

    /// Wait for EOF (every write end closed) and return everything.
    pub fn finish(mut self) -> Vec<u8> {
        if let Some(h) = self.handle.take() {
            h.join().unwrap();
        }
        self.snapshot()
    }
}

/// Join a thread, failing instead of hanging.
pub fn join_within<T>(handle: JoinHandle<T>, timeout: Duration, what: &str) -> T {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        assert!(
            Instant::now() < deadline,
            "{what} did not finish within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.join().unwrap()
}
