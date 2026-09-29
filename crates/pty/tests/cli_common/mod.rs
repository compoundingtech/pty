//! Shared rig for the `cli_*` integration tests: an isolated `PTY_ROOT`,
//! a scrubbed environment, fabricated registry records, and a few daemons
//! started through `pty run -d`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

pub fn pty_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pty")
}

/// One command's captured result.
#[derive(Debug, Clone)]
pub struct Out {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    pub fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {:?}", self.stdout))
    }
}

/// An isolated registry root; daemons started through it are stopped on
/// drop.
pub struct Rig {
    pub root: PathBuf,
    pub scratch: PathBuf,
}

impl Rig {
    pub fn new() -> Rig {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("pty-cli-{}-{n}", std::process::id()));
        let root = base.join("root");
        let scratch = base.join("scratch");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        // The same trap as the other rigs: a long TMPDIR makes every socket
        // bind fail with a message about `sun_path` that reads like a defect
        // in the port. macOS has 104 bytes and spends about 49 of them on its
        // own temp directory before anything nests inside it.
        let longest = root.join("a-session-name-of-a-plausible-length.sock");
        assert!(
            longest.as_os_str().len() <= pty_core::registry::SUN_PATH_MAX,
            "the test root is too long for a unix socket path: {} bytes of {} at\n  {}\n  \
             This is the test harness, not the software. Point TMPDIR at a SHORTER path.\n  \
             Shorter is the whole requirement, and a short path on a disk-backed \
             filesystem is the safe answer: on some machines /tmp is a RAM disk \
             shared with everything else on the box.",
            longest.as_os_str().len(),
            pty_core::registry::SUN_PATH_MAX,
            root.display()
        );
        Rig { root, scratch }
    }

    /// A `Command` for the binary with the rig's environment.
    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(pty_bin());
        c.args(args)
            .env_remove("PTY_SESSION")
            .env_remove("PTY_SESSION_DIR")
            .env_remove("PTY_REAP_ON_EXIT")
            .env("PTY_ROOT", &self.root)
            .env("PTY_ROOT_LEGACY_SILENT", "1")
            .current_dir(&self.scratch)
            .stdin(Stdio::null());
        c
    }

    pub fn run(&self, args: &[&str]) -> Out {
        self.run_env(args, &[])
    }

    pub fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Out {
        let mut c = self.cmd(args);
        for (k, v) in env {
            c.env(k, v);
        }
        finish(c.output().expect("spawn pty"))
    }

    /// Run with `stdin` fed from `input`.
    pub fn run_stdin(&self, args: &[&str], input: &str) -> Out {
        use std::io::Write;
        let mut c = self.cmd(args);
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn().expect("spawn pty");
        // A command that rejects its arguments exits without reading stdin,
        // and a write that loses that race gets EPIPE. The test checks the
        // command's status and output, so a closed stdin is not a failure;
        // any other write error still is.
        match child.stdin.take().unwrap().write_all(input.as_bytes()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => panic!("writing the command's stdin: {e}"),
        }
        finish(child.wait_with_output().unwrap())
    }

    pub fn ok(&self, args: &[&str]) -> Out {
        let out = self.run(args);
        assert_eq!(out.code, 0, "`pty {}` failed: {}", args.join(" "), out.stderr);
        out
    }

    pub fn path(&self, file: &str) -> PathBuf {
        self.root.join(file)
    }

    pub fn exists(&self, file: &str) -> bool {
        self.path(file).exists()
    }

    /// Fabricate `<name>.json` from a JSON object, with Node's usual fields
    /// defaulted (`command`, `args`, `displayCommand`, `cwd`, `rows`,
    /// `cols`, `createdAt`).
    pub fn write_meta(&self, name: &str, fields: Value) -> Value {
        let mut m: Map<String, Value> = Map::new();
        m.insert("command".into(), json!("sh"));
        m.insert("args".into(), json!([]));
        m.insert("displayCommand".into(), json!("sh"));
        m.insert("cwd".into(), json!(self.scratch.to_string_lossy()));
        m.insert("rows".into(), json!(24));
        m.insert("cols".into(), json!(80));
        m.insert("createdAt".into(), json!(iso_now(0)));
        if let Value::Object(extra) = fields {
            for (k, v) in extra {
                m.insert(k, v);
            }
        }
        let v = Value::Object(m);
        std::fs::write(
            self.path(&format!("{name}.json")),
            serde_json::to_string_pretty(&v).unwrap(),
        )
        .unwrap();
        v
    }

    pub fn read_meta(&self, name: &str) -> Option<Value> {
        let bytes = std::fs::read(self.path(&format!("{name}.json"))).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Every event line of `<name>.events.jsonl`.
    pub fn events(&self, name: &str) -> Vec<Value> {
        std::fs::read_to_string(self.path(&format!("{name}.events.jsonl")))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Start `cat` as a session through `pty run -d`, returning once its
    /// socket answers.
    pub fn spawn_cat(&self, name: &str, extra: &[&str]) {
        let mut args = vec!["run", "-d", "--id", name];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--", "cat"]);
        let out = self.run(&args);
        assert_eq!(out.code, 0, "run -d {name}: {}", out.stderr);
        wait_until("socket up", || {
            std::os::unix::net::UnixStream::connect(self.path(&format!("{name}.sock"))).is_ok()
        });
    }

    pub fn kill_all(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(_name) = n.strip_suffix(".pid")
                && let Ok(pid) = std::fs::read_to_string(e.path())
                && let Ok(pid) = pid.trim().parse::<i32>()
                && pid != std::process::id() as i32
            {
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
    }

    /// The manifest dir for `up`/`down` tests.
    pub fn write_toml(&self, name: &str, body: &str) -> PathBuf {
        let dir = self.scratch.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pty.toml"), body).unwrap();
        dir
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.kill_all();
        std::thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_dir_all(self.root.parent().unwrap());
    }
}

fn finish(out: std::process::Output) -> Out {
    Out {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

/// Poll until `cond` holds (10 s budget).
pub fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("timed out waiting for {what}");
}

/// `new Date(Date.now() + offset_ms).toISOString()`.
pub fn iso_now(offset_ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    pty_core::registry::iso8601_from_epoch_ms(now + offset_ms)
}

/// A pid that is certainly dead.
pub const DEAD_PID: i32 = 2147483647;

pub fn file_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}
