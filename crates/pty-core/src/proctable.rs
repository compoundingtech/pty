//! One reader for the process table.
//!
//! Four wrong answers this week came from the same shape: a caller ran its own
//! `ps`, and treated whatever came back as fact. `ps` is a subprocess. It can
//! be slow, truncated, or silent, and each of those looked exactly like "the
//! process is gone".
//!
//! - `pty stats` reported nothing at all off Linux.
//! - A descendant was dropped from a teardown because its start token could not
//!   be read.
//! - `registry_list` failed under load because `ps` went quiet.
//! - The Node tool read an empty `stat` field as "exited".
//!
//! So this module exists to make that mistake hard to write rather than to fix
//! its four instances. Two rules carry the weight:
//!
//! **One read, not one per caller.** A [`ProcTable`] is a snapshot of every
//! process, taken with a single `ps` call, or read from `/proc` with no
//! subprocess at all. A caller that needs five facts about four processes asks
//! one table, not twenty subprocesses.
//!
//! **Silence is its own answer.** Every query returns [`Answer`], which
//! separates "the table says this process is not there" from "I could not find
//! out". There is deliberately no `Default`, no `unwrap_or`, and no conversion
//! to `Option` that would let the second quietly become the first.

use std::collections::HashMap;
use std::time::Duration;

/// How long `ps` gets before the table is declared unreadable. Under contention
/// `ps` is exactly the thing that goes quiet, so this bound is the point rather
/// than a formality.
const PS_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a fact is not available. None of these mean the process is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unknown {
    /// The table could not be read at all: `ps` failed, timed out, returned
    /// nothing, or returned something that did not contain this very process.
    TableUnreadable,
    /// The table has the process, but this column was empty.
    FieldEmpty,
    /// The table has the process and the column, and it did not parse.
    FieldUnparsable,
}

/// What the process table said about one thing.
///
/// **`Unknown` is not `NotPresent`.** Folding them together is the defect this
/// type exists to prevent, so there is no `Default`, no `unwrap_or`, and no
/// `From<Answer<T>> for Option<T>`. A caller that genuinely wants to treat
/// silence as death must call [`Answer::or_absent_when_unknown`], which is
/// named so it shows up in a review and in a grep.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer<T> {
    Known(T),
    /// The table was read successfully and this process was not in it. This is
    /// a fact: the process is gone.
    NotPresent,
    Unknown(Unknown),
}

impl<T> Answer<T> {
    /// The value if the table knew it. Silence and absence both yield `None`,
    /// so this is for callers that have already decided the difference does not
    /// matter to them.
    pub fn known(self) -> Option<T> {
        match self {
            Answer::Known(v) => Some(v),
            _ => None,
        }
    }

    /// Is the process definitely gone? Only `NotPresent` says so. An unreadable
    /// table never does.
    pub fn is_definitely_absent(&self) -> bool {
        matches!(self, Answer::NotPresent)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Answer::Unknown(_))
    }

    /// Deliberately treat silence as absence.
    ///
    /// Sometimes that is the right call — a best-effort display, say. It is
    /// never the right default, which is why it has a long name instead of
    /// being what happens when you write nothing.
    pub fn or_absent_when_unknown(self) -> Answer<T> {
        match self {
            Answer::Unknown(_) => Answer::NotPresent,
            other => other,
        }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Answer<U> {
        match self {
            Answer::Known(v) => Answer::Known(f(v)),
            Answer::NotPresent => Answer::NotPresent,
            Answer::Unknown(u) => Answer::Unknown(u),
        }
    }
}

/// A process identity that is only ever compared with another one taken from
/// the same run.
///
/// **This is deliberately not the same type as the registry's
/// `recovery.processStartToken`, and it must never be compared with it.** That
/// token is written into session metadata, read by the Node tool from the same
/// registry, and its exact text is a contract between the two. This one is
/// private to a single command's lifetime, so it is free to be whatever is
/// cheapest to read. Making them different types is what stops the two from
/// meeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveIdentity(String);

impl LiveIdentity {
    pub fn new(value: impl Into<String>) -> Self {
        LiveIdentity(value.into())
    }
}

impl From<&str> for LiveIdentity {
    fn from(value: &str) -> Self {
        LiveIdentity(value.to_string())
    }
}

impl From<String> for LiveIdentity {
    fn from(value: String) -> Self {
        LiveIdentity(value)
    }
}

/// A table built from `pid ppid pgid state` lines, for tests that care about
/// tree shape rather than about reading a real machine.
pub fn table_from_shape(spec: &str) -> ProcTable {
    ProcTable::from_rows(
        spec.lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.split_whitespace().collect();
                if f.len() < 3 {
                    return None;
                }
                Some(Row {
                    pid: f[0].parse().ok()?,
                    ppid: f[1].parse().ok()?,
                    pgid: f[2].parse().ok()?,
                    state: f.get(3).unwrap_or(&"S").to_string(),
                    rss_kb: None,
                    cpu_percent: None,
                    // A literal `-` in the identity column means the table
                    // had the process but could not name it.
                    identity: match f.get(4) {
                        Some(&"-") => None,
                        Some(t) => Some(LiveIdentity::new(*t)),
                        None => Some(LiveIdentity::new(format!("tok:{}", f[0]))),
                    },
                })
            })
            .collect(),
    )
}

/// One process, as the table saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// `ps` state letters, or the `/proc` state character. Empty when the
    /// source did not give one.
    pub state: String,
    pub rss_kb: Option<u64>,
    pub cpu_percent: Option<f64>,
    /// Proof of identity for the length of one command. See [`LiveIdentity`]:
    /// this is NOT the registry's `recovery.processStartToken`.
    pub identity: Option<LiveIdentity>,
}

impl Row {
    /// A zombie is a dead process that still has a row, still has a process
    /// group, and still answers `kill(pid, 0)`.
    pub fn is_zombie(&self) -> bool {
        self.state.starts_with('Z')
    }

    /// Stopped by a signal (`T`) or by a tracer (`t`). A stopped process runs
    /// no code, so it cannot fork until it is continued.
    pub fn is_stopped(&self) -> bool {
        self.state.starts_with('T') || self.state.starts_with('t')
    }
}

/// A snapshot of the process table.
#[derive(Debug, Clone)]
pub struct ProcTable {
    rows: HashMap<i32, Row>,
    readable: bool,
    /// `false` when the reader skipped a process it could see but not read.
    /// Every row present is still true; what is missing is proof that the
    /// rows are all there is. See [`ProcTable::is_complete`].
    complete: bool,
}

impl ProcTable {
    /// Read the table once.
    pub fn read() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::read_proc()
        }
        #[cfg(target_os = "macos")]
        {
            Self::read_libproc()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Self::from_ps_listing(&run_ps(PS_TIMEOUT).unwrap_or_default())
        }
    }

    /// macOS: `proc_listpids` + `proc_pidinfo`, both syscalls. No subprocess,
    /// and `proc_bsdinfo` carries ppid, pgid, status and the start time, which
    /// is every fact the callers ask for.
    #[cfg(target_os = "macos")]
    fn read_libproc() -> Self {
        // <sys/proc_info.h>: PROC_ALL_PIDS. Not exported by the libc crate.
        const PROC_ALL_PIDS: u32 = 1;
        // <sys/proc.h>: SZOMB. A zombie is a corpse that still has a row.
        const SZOMB: u32 = 5;

        // SAFETY: a zero buffer asks for the size in bytes.
        let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
        if bytes <= 0 {
            return Self::unreadable();
        }
        let mut pids = vec![0i32; (bytes as usize / size_of::<i32>()) + 64];
        let cap = (pids.len() * size_of::<i32>()) as i32;
        // SAFETY: the buffer and its length agree.
        let written = unsafe {
            libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr() as *mut libc::c_void, cap)
        };
        if written <= 0 {
            return Self::unreadable();
        }
        pids.truncate(written as usize / size_of::<i32>());

        let _ = SZOMB;
        let mut rows = Vec::with_capacity(pids.len());
        let mut complete = true;
        for pid in pids.into_iter().filter(|&p| p > 0) {
            if let Some(row) = read_bsdinfo(pid) {
                rows.push(row);
                continue;
            }
            // `proc_listpids` listed it and `proc_pidinfo` refuses it, which on
            // this platform is what an unreaped child looks like. Dropping it
            // here would make the table disagree with Linux, where the corpse
            // keeps its row. A process that genuinely exited between the two
            // calls comes back `NotPresent` and is skipped.
            match sysctl_proc(pid) {
                Answer::Known(row) => rows.push(row),
                Answer::NotPresent => {}
                Answer::Unknown(_) => complete = false,
            }
        }
        if !rows.iter().any(|r| r.pid == std::process::id() as i32) {
            return Self::unreadable();
        }
        let table = Self::from_rows(rows);
        if complete {
            table
        } else {
            table.marked_incomplete()
        }
    }

    /// The Linux reader over its inputs: the `/proc` listing and a way to
    /// read one `stat`. Every entry the listing could not produce, and every
    /// row that was refused or would not parse, makes the table incomplete:
    /// that process may be the one linking a descendant into a tree.
    ///
    /// A process that exits between the listing and the read is simply gone;
    /// skipping it is correct and is not the silence this module guards
    /// against.
    #[cfg(any(target_os = "linux", test))]
    fn from_proc_listing(
        entries: impl Iterator<Item = std::io::Result<std::ffi::OsString>>,
        read_stat: impl Fn(i32) -> std::io::Result<String>,
        own_pid: i32,
    ) -> Self {
        let mut rows = Vec::new();
        let mut complete = true;
        for entry in entries {
            let Ok(name) = entry else {
                complete = false;
                continue;
            };
            let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            match read_stat(pid) {
                Ok(stat) => match parse_proc_stat(pid, &stat) {
                    Some(row) => rows.push(row),
                    None => complete = false,
                },
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        || e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(_) => complete = false,
            }
        }
        if !rows.iter().any(|r| r.pid == own_pid) {
            return Self::unreadable();
        }
        let table = Self::from_rows(rows);
        if complete {
            table
        } else {
            table.marked_incomplete()
        }
    }

    /// A table that could not be read. Every query returns
    /// `Unknown(TableUnreadable)`.
    pub fn unreadable() -> Self {
        ProcTable {
            rows: HashMap::new(),
            readable: false,
            complete: false,
        }
    }

    /// Build a table from rows that are already known good. Used by the
    /// `/proc` reader and by tests.
    pub fn from_rows(rows: Vec<Row>) -> Self {
        ProcTable {
            rows: rows.into_iter().map(|r| (r.pid, r)).collect(),
            readable: true,
            complete: true,
        }
    }

    /// The same rows, no longer claiming to be every process. Used by the
    /// readers when they skip a process they could not read, and by tests.
    pub fn marked_incomplete(mut self) -> Self {
        self.complete = false;
        self
    }

    /// Parse `ps -axo pid=,ppid=,pgid=,state=,rss=,pcpu=,lstart=`.
    ///
    /// **An empty or self-omitting listing is an unreadable table, not an empty
    /// machine.** `ps` always lists at least the process that ran it, so a
    /// listing without our own pid was truncated or never produced. That check
    /// is what turns a silent `ps` into `Unknown` instead of "everything is
    /// dead".
    pub fn from_ps_listing(listing: &str) -> Self {
        Self::from_ps_listing_checked(listing, std::process::id() as i32)
    }

    pub fn from_ps_listing_checked(listing: &str, must_contain: i32) -> Self {
        let mut rows = Vec::new();
        for line in listing.lines() {
            if let Some(row) = parse_ps_row(line) {
                rows.push(row);
            }
        }
        if !rows.iter().any(|r| r.pid == must_contain) {
            return Self::unreadable();
        }
        Self::from_rows(rows)
    }

    pub fn is_readable(&self) -> bool {
        self.readable
    }

    /// Readable, and no process was skipped for being unreadable.
    ///
    /// **A readable table is not necessarily a whole one.** A process that
    /// exits between the listing and the read is really gone and costs
    /// nothing; one whose row was refused or would not parse is still there,
    /// unseen. Per-row lookups do not care, but a caller that must prove it
    /// saw *every* descendant does: without this, a skipped row reads as a
    /// tree that has no such process.
    pub fn is_complete(&self) -> bool {
        self.readable && self.complete
    }

    pub fn rows(&self) -> impl Iterator<Item = &Row> {
        self.rows.values()
    }

    fn lookup(&self, pid: i32) -> Answer<&Row> {
        if !self.readable {
            return Answer::Unknown(Unknown::TableUnreadable);
        }
        match self.rows.get(&pid) {
            Some(r) => Answer::Known(r),
            None => Answer::NotPresent,
        }
    }

    pub fn row(&self, pid: i32) -> Answer<&Row> {
        self.lookup(pid)
    }

    /// Is this process alive and not a corpse awaiting reaping?
    pub fn is_running(&self, pid: i32) -> Answer<bool> {
        self.lookup(pid).map(|r| !r.is_zombie())
    }

    pub fn identity(&self, pid: i32) -> Answer<LiveIdentity> {
        match self.lookup(pid) {
            Answer::Known(r) => match &r.identity {
                Some(t) => Answer::Known(t.clone()),
                None => Answer::Unknown(Unknown::FieldEmpty),
            },
            Answer::NotPresent => Answer::NotPresent,
            Answer::Unknown(u) => Answer::Unknown(u),
        }
    }

    pub fn resources(&self, pid: i32) -> Answer<(u64, f64)> {
        match self.lookup(pid) {
            Answer::Known(r) => match (r.rss_kb, r.cpu_percent) {
                (Some(rss), Some(cpu)) => Answer::Known((rss, cpu)),
                _ => Answer::Unknown(Unknown::FieldEmpty),
            },
            Answer::NotPresent => Answer::NotPresent,
            Answer::Unknown(u) => Answer::Unknown(u),
        }
    }

    #[cfg(target_os = "linux")]
    fn read_proc() -> Self {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return Self::unreadable();
        };
        Self::from_proc_listing(
            dir.map(|entry| entry.map(|entry| entry.file_name())),
            |pid| std::fs::read_to_string(format!("/proc/{pid}/stat")),
            std::process::id() as i32,
        )
    }
}

/// One process, without reading the whole table.
///
/// A poll loop asking about a single pid should not pay for every process on
/// the machine, and it must not pay for a subprocess either. On Linux this is
/// one small file read; on macOS it is one `proc_pidinfo` call.
pub fn process(pid: i32) -> Answer<Row> {
    if pid <= 0 {
        return Answer::NotPresent;
    }
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => match parse_proc_stat(pid, &stat) {
                Some(row) => Answer::Known(row),
                None => Answer::Unknown(Unknown::FieldUnparsable),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Answer::NotPresent,
            // Permission denied, or /proc not mounted. We did not find out.
            Err(_) => Answer::Unknown(Unknown::TableUnreadable),
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(row) = read_bsdinfo(pid) {
            return Answer::Known(row);
        }
        // **`proc_pidinfo` refuses an unreaped child, and that is not the same
        // as the child being gone.** Measured by `Silber.pty` on a real Mac on
        // 2026-09-03, for zombie pid 92893:
        //
        //     proc_listpids           contains=1
        //     PROC_PIDTBSDINFO        got=0 want=136 errno=3 (ESRCH)
        //     PROC_PIDT_SHORTBSDINFO  got=0 want=64  errno=3 (ESRCH)
        //     sysctl KERN_PROC_PID    rc=0 size=648 status=5 ppid=92892 pgid=92540
        //     /bin/ps                 92893 92892 92540 Z
        //
        // The pid listing is right and only the per-process detail call
        // refuses. **This is this module's own defect class at the scale of a
        // single lookup**: a reader returning nothing, read as "the process
        // does not exist". `KERN_PROC_PID` is the only API that answers, so
        // the layout below is a cost rather than a preference.
        sysctl_proc(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        ProcTable::read().row(pid).map(|r| r.clone())
    }
}

/// Is `pid` gone, as far as reaping is concerned? A zombie counts as gone.
///
/// The three answers are kept apart on purpose. A caller that cannot proceed
/// without knowing should say so rather than guess.
pub fn has_exited(pid: i32) -> Answer<bool> {
    process(pid).map(|r| r.is_zombie())
}

/// `<pid> <ppid> <pgid> <state> <rss> <pcpu> <lstart...>`
///
/// `lstart` is last because it is the only column with spaces in it.
fn parse_ps_row(line: &str) -> Option<Row> {
    // The tail is taken as raw text rather than re-joined from split tokens.
    // `ps -o lstart=` pads a single-digit day with two spaces, and re-joining
    // would quietly rewrite `Wed Sep  3` as `Wed Sep 3`. That text is also the
    // registry's on-disk token, so normalising it here would silently stop
    // matching what the Node tool wrote.
    let (fields, tail) = split_leading_fields(line, 6);
    let mut it = fields.into_iter();
    let pid: i32 = it.next()?.parse().ok()?;
    let ppid: i32 = it.next()?.parse().ok()?;
    let pgid: i32 = it.next()?.parse().ok()?;
    let state = it.next().unwrap_or("").to_string();
    let rss_kb = it.next().and_then(|s| s.parse::<u64>().ok());
    let cpu_percent = it.next().and_then(|s| s.parse::<f64>().ok());
    let identity =
        (!tail.trim().is_empty()).then(|| LiveIdentity::new(format!("darwin:{}", tail.trim())));
    Some(Row {
        pid,
        ppid,
        pgid,
        state,
        rss_kb,
        cpu_percent,
        identity,
    })
}

/// Split off the first `n` whitespace-delimited fields and return the rest of
/// the line untouched, spacing included.
fn split_leading_fields(line: &str, n: usize) -> (Vec<&str>, &str) {
    let mut fields = Vec::with_capacity(n);
    let mut rest = line;
    for _ in 0..n {
        let start = rest.len() - rest.trim_start().len();
        rest = &rest[start..];
        match rest.find(char::is_whitespace) {
            Some(end) => {
                fields.push(&rest[..end]);
                rest = &rest[end..];
            }
            None => {
                if !rest.is_empty() {
                    fields.push(rest);
                }
                return (fields, "");
            }
        }
    }
    (fields, rest.trim_start_matches(' '))
}

/// `/proc/<pid>/stat`. Field 2 is the comm in parentheses and may contain
/// spaces and brackets, so everything is read relative to the LAST `)`.
pub fn parse_proc_stat(pid: i32, stat: &str) -> Option<Row> {
    let tail = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = tail.split_whitespace().collect();
    // tail[0] is field 3 (state), so field N is tail[N - 3].
    let state = (*f.first()?).to_string();
    let ppid: i32 = f.get(1)?.parse().ok()?;
    let pgid: i32 = f.get(2)?.parse().ok()?;
    let start_time = f.get(19)?;
    let rss_pages: u64 = f.get(21).and_then(|s| s.parse().ok()).unwrap_or(0);
    Some(Row {
        pid,
        ppid,
        pgid,
        state,
        rss_kb: Some(rss_pages * (page_size_kb())),
        // Not read here: an average needs utime, stime and uptime together, and
        // `stats.rs` already computes it. Left as "the table did not say".
        cpu_percent: None,
        identity: Some(LiveIdentity::new(format!("linux:{start_time}"))),
    })
}

fn page_size_kb() -> u64 {
    // SAFETY: sysconf(3) with a constant name.
    let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if sz > 0 { (sz as u64) / 1024 } else { 4 }
}

/// `sysctl KERN_PROC_PID`, the only macOS API that answers for a zombie.
///
/// Field offsets into `kinfo_proc`, measured on a Mac by `Silber.pty` on
/// 2026-09-03 with an `offsetof` probe compiled warnings-as-errors, and
/// checked identical against both the 26.5 and 15.4 SDKs:
///
/// ```text
/// sizeof(kinfo_proc) = 648   sizeof(extern_proc) = 296
/// kp_proc = 0                kp_eproc = 296
/// p_starttime = 0            tv_sec +0 (8 bytes), tv_usec +8 (4 bytes)
/// p_stat = 36 (1 byte)       p_pid = 40 (4 bytes, signed)
/// e_ppid = 560 (4 bytes)     e_pgid = 564 (4 bytes)
/// ```
#[cfg(target_os = "macos")]
mod kinfo {
    pub const SIZE: usize = 648;
    pub const START_SEC: usize = 0;
    pub const START_USEC: usize = 8;
    pub const P_STAT: usize = 36;
    pub const P_PID: usize = super::P_PID_OFFSET;
    pub const E_PPID: usize = 560;
    pub const E_PGID: usize = 564;
    /// `SZOMB` from `<sys/proc.h>`.
    pub const SZOMB: u8 = 5;
    /// `SSTOP` from `<sys/proc.h>`.
    pub const SSTOP: u8 = 4;
}

#[cfg(target_os = "macos")]
fn sysctl_proc(pid: i32) -> Answer<Row> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut buf = [0u8; kinfo::SIZE];
    let mut len = buf.len();
    // SAFETY: a four-element MIB and a buffer whose length is passed alongside.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Answer::NotPresent,
            _ => Answer::Unknown(Unknown::TableUnreadable),
        };
    }
    // A zero-length answer is how this call reports a pid it no longer has.
    if len == 0 {
        return Answer::NotPresent;
    }
    if len < kinfo::SIZE {
        return Answer::Unknown(Unknown::TableUnreadable);
    }

    // **A layout that cannot find itself is not a layout.** The offsets above
    // were measured on someone else's machine against two SDKs, and this code
    // cannot be run where it was written. So before any of them is believed,
    // the struct has to contain the pid that was asked for. A wrong offset then
    // returns "I could not find out" rather than feeding a garbage ppid into a
    // kill path.
    if !layout_finds_itself(&buf, pid) {
        return Answer::Unknown(Unknown::TableUnreadable);
    }

    let sec = u64::from_ne_bytes(buf[kinfo::START_SEC..kinfo::START_SEC + 8].try_into().unwrap());
    let usec = u32::from_ne_bytes(buf[kinfo::START_USEC..kinfo::START_USEC + 4].try_into().unwrap());
    Some(Row {
        pid,
        ppid: read_i32(&buf, kinfo::E_PPID),
        pgid: read_i32(&buf, kinfo::E_PGID),
        state: match buf[kinfo::P_STAT] {
            kinfo::SZOMB => "Z".into(),
            kinfo::SSTOP => "T".into(),
            _ => "S".into(),
        },
        rss_kb: None,
        cpu_percent: None,
        identity: Some(LiveIdentity::new(format!("darwin:{sec}.{usec:06}"))),
    })
    .map(Answer::Known)
    .unwrap_or(Answer::Unknown(Unknown::TableUnreadable))
}

fn read_i32(buf: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes(buf[at..at + 4].try_into().unwrap())
}

/// Does the buffer contain the pid it was fetched for?
///
/// Split out so it can be tested where `sysctl` cannot be called. The offsets
/// it checks were measured on another machine, and this is what stops them
/// being believed when they are wrong.
fn layout_finds_itself(buf: &[u8], pid: i32) -> bool {
    buf.len() >= P_PID_OFFSET + 4 && read_i32(buf, P_PID_OFFSET) == pid
}

/// `kinfo_proc.kp_proc.p_pid`, measured on a Mac. Kept outside the `macos`
/// module so the guard above is compiled and tested everywhere.
const P_PID_OFFSET: usize = 40;

/// One `proc_pidinfo` call. `None` means the process is not there./// One `proc_pidinfo` call. `None` means the process is not there.
#[cfg(target_os = "macos")]
fn read_bsdinfo(pid: i32) -> Option<Row> {
    // <sys/proc.h>: SZOMB. A zombie is a corpse that still has a row.
    const SZOMB: u32 = 5;
    // <sys/proc.h>: SSTOP. Stopped by a signal; it cannot fork until continued.
    const SSTOP: u32 = 4;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let want = size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: `info` is the struct `PROC_PIDTBSDINFO` fills.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            want,
        )
    };
    if got != want {
        return None;
    }
    Some(Row {
        pid,
        ppid: info.pbi_ppid as i32,
        pgid: info.pbi_pgid as i32,
        state: match info.pbi_status {
            SZOMB => "Z".into(),
            SSTOP => "T".into(),
            _ => "S".into(),
        },
        rss_kb: None,
        cpu_percent: None,
        // Microsecond start time, a STRONGER identity than the
        // second-resolution text `ps -o lstart=` prints. It is only usable
        // because this is a `LiveIdentity` and never reaches disk; the
        // registry's token keeps its own text format, because the Node tool
        // reads that one from the same registry.
        identity: Some(LiveIdentity::new(format!(
            "darwin:{}.{:06}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))),
    })
}

/// Resident set and lifetime-average CPU for one process, from
/// `proc_pidinfo`. macOS only; Linux computes these from `/proc` in `stats`.
#[cfg(target_os = "macos")]
pub fn resources_of(pid: i32) -> Answer<(u64, f64)> {
    let Some(bsd) = read_bsdinfo(pid) else {
        return Answer::NotPresent;
    };
    let _ = bsd;
    let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let want = size_of::<libc::proc_taskinfo>() as i32;
    // SAFETY: `task` is the struct `PROC_PIDTASKINFO` fills.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            &mut task as *mut _ as *mut libc::c_void,
            want,
        )
    };
    if got != want {
        return Answer::Unknown(Unknown::FieldEmpty);
    }
    let rss_kb = task.pti_resident_size / 1024;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let bwant = size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: as above.
    let bgot = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            bwant,
        )
    };
    if bgot != bwant {
        return Answer::Unknown(Unknown::FieldEmpty);
    }
    let started = info.pbi_start_tvsec as f64 + (info.pbi_start_tvusec as f64 / 1e6);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(started);
    let elapsed = (now - started).max(0.001);
    let cpu_ns = task.pti_total_user as f64 + task.pti_total_system as f64;
    Answer::Known((rss_kb, (cpu_ns / 1e9) / elapsed * 100.0))
}

/// Run `ps` with a deadline.
///
/// Returns `None` when `ps` fails, is killed, or does not answer in time. The
/// reading happens on another thread so a full pipe cannot deadlock the wait;
/// if `ps` never returns, that thread ends when it finally does, and this
/// function has already given up.
fn run_ps(timeout: Duration) -> Option<String> {
    run_ps_program("ps", timeout)
}

/// The program is a parameter so a test can point this at a `ps` that is slow,
/// silent or truncated. That is the failure this module exists for, and it
/// cannot be tested against the real one.
pub(crate) fn run_ps_program(program: &str, timeout: Duration) -> Option<String> {
    let program = program.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new(program)
            .args(["-axo", "pid=,ppid=,pgid=,state=,rss=,pcpu=,lstart="])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> i32 {
        std::process::id() as i32
    }

    // ---- the truncation guard -------------------------------------------

    /// `ps` always lists at least the process that ran it. A listing without
    /// our own pid was truncated or never produced, and reading it as "the
    /// machine has no processes" is the defect this whole module exists for.
    #[test]
    fn a_listing_without_our_own_pid_is_unreadable_not_empty() {
        let t = ProcTable::from_ps_listing("4242 1 4242 S 100 0.0 Wed Sep  3 11:00:00 2026\n");
        assert!(!t.is_readable(), "a listing that omits the reader is truncated");
        assert!(
            t.is_running(4242).is_unknown(),
            "and it must not answer questions about the row it does contain"
        );
    }

    #[test]
    fn an_empty_listing_is_unreadable_not_empty() {
        assert!(!ProcTable::from_ps_listing("").is_readable());
        assert!(!ProcTable::from_ps_listing("   \n\n").is_readable());
    }

    #[test]
    fn a_listing_that_contains_us_is_readable() {
        let listing = format!("{} 1 {} S 100 0.5 Wed Sep  3 11:00:00 2026\n", me(), me());
        let t = ProcTable::from_ps_listing(&listing);
        assert!(t.is_readable());
        assert_eq!(t.is_running(me()), Answer::Known(true));
        assert_eq!(t.resources(me()), Answer::Known((100, 0.5)));
        assert_eq!(
            t.identity(me()),
            Answer::Known(LiveIdentity::new("darwin:Wed Sep  3 11:00:00 2026"))
        );
    }

    // ---- a readable table is not necessarily a whole one ----------------

    /// PR #34: a caller that must prove it saw every descendant needs to
    /// know when the reader skipped a row it could not read.
    #[test]
    fn a_table_that_skipped_rows_is_readable_but_not_complete() {
        let listing = format!("{} 1 {} S 100 0.5 Wed Sep  3 11:00:00 2026\n", me(), me());
        let whole = ProcTable::from_ps_listing(&listing);
        assert!(whole.is_complete());
        let partial = whole.clone().marked_incomplete();
        assert!(partial.is_readable());
        assert!(!partial.is_complete());
        // Every row it does have still answers exactly as before.
        assert_eq!(partial.is_running(me()), whole.is_running(me()));
        assert_eq!(partial.identity(me()), whole.identity(me()));
        assert_eq!(partial.rows().count(), whole.rows().count());
    }

    /// A `/proc/<pid>/stat` line for `pid`, child of `ppid`, in `state`.
    fn stat(pid: i32, ppid: i32, state: &str) -> String {
        format!(
            "{pid} (cat) {state} {ppid} {pid} {ppid} 0 -1 4194304 503 0 1 0 0 0 0 0 20 0 1 0 666751667 16637952 1837"
        )
    }

    fn listing(names: &[std::io::Result<&str>]) -> Vec<std::io::Result<std::ffi::OsString>> {
        names
            .iter()
            .map(|name| match name {
                Ok(name) => Ok(std::ffi::OsString::from(name)),
                Err(error) => Err(std::io::Error::from(error.kind())),
            })
            .collect()
    }

    /// PR #39 review: a `/proc` entry that could not be listed may be the
    /// process that links a descendant into the tree. The table keeps every
    /// row it read but no longer claims to be whole.
    #[test]
    fn a_listing_error_makes_the_linux_table_incomplete() {
        let read = |pid: i32| Ok(stat(pid, 1, "S"));
        let whole = ProcTable::from_proc_listing(listing(&[Ok("7"), Ok("8")]).into_iter(), read, 7);
        assert!(whole.is_complete());

        let entries = listing(&[Ok("7"), Err(std::io::Error::other("readdir")), Ok("8")]);
        let partial = ProcTable::from_proc_listing(entries.into_iter(), read, 7);
        assert!(
            partial.is_readable(),
            "a listing error is not an unreadable table"
        );
        assert!(!partial.is_complete(), "a listing error was hidden");
        assert_eq!(partial.rows().count(), 2);
    }

    #[test]
    fn a_refused_or_garbled_row_makes_the_linux_table_incomplete_but_an_exit_does_not() {
        let entries = || listing(&[Ok("7"), Ok("8"), Ok("self")]).into_iter();
        let with = |eight: fn() -> std::io::Result<String>| {
            ProcTable::from_proc_listing(
                entries(),
                move |pid| {
                    if pid == 8 {
                        eight()
                    } else {
                        Ok(stat(pid, 1, "S"))
                    }
                },
                7,
            )
        };
        assert!(
            !with(|| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))).is_complete()
        );
        assert!(!with(|| Ok("8 (cat)".to_string())).is_complete());
        // Gone between the listing and the read: really absent, not a gap.
        assert!(with(|| Err(std::io::Error::from(std::io::ErrorKind::NotFound))).is_complete());
        assert!(with(|| Err(std::io::Error::from_raw_os_error(libc::ESRCH))).is_complete());
    }

    #[test]
    fn a_stopped_row_reads_as_stopped() {
        for (state, stopped) in [
            ("T", true),
            ("t", true),
            ("S", false),
            ("R", false),
            ("Z", false),
            ("D", false),
        ] {
            let row = parse_proc_stat(9, &stat(9, 1, state)).unwrap();
            assert_eq!(row.is_stopped(), stopped, "state {state}");
        }
    }

    #[test]
    fn an_unreadable_table_is_not_complete() {
        assert!(!ProcTable::unreadable().is_complete());
    }

    /// The real reader, on a machine where every row is readable, says so.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_real_read_of_this_machine_is_complete() {
        let table = ProcTable::read();
        assert!(table.is_readable());
        assert!(
            table.is_complete(),
            "a normal read skipped a row it could not read"
        );
    }

    // ---- three answers, never two ---------------------------------------

    #[test]
    fn absent_and_unknown_are_different_answers() {
        let listing = format!("{} 1 {} S 100 0.5 Wed Sep  3 11:00:00 2026\n", me(), me());
        let readable = ProcTable::from_ps_listing(&listing);
        let unreadable = ProcTable::unreadable();

        assert_eq!(readable.is_running(999_999), Answer::NotPresent);
        assert!(readable.is_running(999_999).is_definitely_absent());

        assert_eq!(
            unreadable.is_running(999_999),
            Answer::Unknown(Unknown::TableUnreadable)
        );
        assert!(
            !unreadable.is_running(999_999).is_definitely_absent(),
            "an unreadable table never proves a process is gone"
        );
    }

    /// An empty column is its own answer too. The process is there; `ps` just
    /// did not say. This is the Node defect in miniature.
    #[test]
    fn an_empty_column_is_not_an_absent_process() {
        let listing = format!("{} 1 {} S\n", me(), me());
        let t = ProcTable::from_ps_listing(&listing);
        assert!(t.is_readable());
        assert_eq!(t.resources(me()), Answer::Unknown(Unknown::FieldEmpty));
        assert_eq!(t.identity(me()), Answer::Unknown(Unknown::FieldEmpty));
        assert!(!t.resources(me()).is_definitely_absent());
    }

    #[test]
    fn treating_silence_as_death_has_to_be_asked_for_by_name() {
        let t = ProcTable::unreadable();
        assert!(t.is_running(1).is_unknown());
        assert!(t.is_running(1).or_absent_when_unknown().is_definitely_absent());
    }

    // ---- the struct layout, which was measured elsewhere ----------------

    /// **This tests the guard, not the offset.** It writes the pid at
    /// `P_PID_OFFSET` and reads it back from the same constant, so it stays
    /// green whatever that constant is — I changed 40 to 44 and it still
    /// passed. Only a Mac can prove the number, and `Silber.pty` did, against
    /// two SDKs.
    ///
    /// What this pins is the thing that makes a wrong number safe: **a layout
    /// that cannot find itself is not a layout**, so a bad offset returns "I
    /// could not find out" instead of a garbage ppid in a kill path.
    #[test]
    fn a_buffer_that_does_not_contain_its_own_pid_is_refused() {
        let mut buf = [0u8; 648];
        buf[P_PID_OFFSET..P_PID_OFFSET + 4].copy_from_slice(&4242i32.to_ne_bytes());
        assert!(layout_finds_itself(&buf, 4242));
        assert!(!layout_finds_itself(&buf, 4243), "a different pid is a wrong layout");

        // The same bytes one field out, which is what a bad offset looks like.
        let mut shifted = [0u8; 648];
        shifted[P_PID_OFFSET + 4..P_PID_OFFSET + 8].copy_from_slice(&4242i32.to_ne_bytes());
        assert!(
            !layout_finds_itself(&shifted, 4242),
            "an off-by-one-field layout must not be believed"
        );
    }

    #[test]
    fn a_short_buffer_is_refused_rather_than_indexed() {
        assert!(!layout_finds_itself(&[0u8; 8], 1));
        assert!(!layout_finds_itself(&[], 1));
    }

    // ---- a ps that is slow, silent or truncated -------------------------

    /// A script for `run_ps_program` to run in place of `ps`.
    ///
    /// **The script is written by a child `sh`, never by this process.** The
    /// tests run on parallel threads and several of them spawn processes. If
    /// this process held the script open for writing when another thread
    /// forked, the forked child kept that descriptor until its own exec, and
    /// Linux refuses to exec a file that any process has open for writing
    /// (`ETXTBSY`, "Text file busy"). `run_ps_program` turns that refusal
    /// into `None`, so the fake looked as if it never ran. It only happened
    /// under load and only now and then, which is how it reached a nix build
    /// on Small Talk's CI. Once the child has exited, no process can hold
    /// the script open for writing, however the other threads interleave.
    fn fake_ps(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", r#"printf '%s\n' "$2" > "$1""#, "sh"])
            .arg(&path)
            .arg(format!("#!/bin/sh\n{body}"))
            .status()
            .expect("run sh to write the fake ps");
        assert!(status.success(), "sh could not write {}", path.display());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Under contention `ps` is the thing that goes quiet. The read must give
    /// up on a deadline rather than wait for it.
    #[test]
    fn a_slow_ps_is_abandoned_and_reads_as_unreadable() {
        let dir = std::env::temp_dir().join(format!("proctable-slow-{}", me()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("it-ran");
        let ps = fake_ps(
            &dir,
            "slow-ps",
            &format!("touch {}\nsleep 30", marker.display()),
        );

        let start = std::time::Instant::now();
        let out = run_ps_program(ps.to_str().unwrap(), Duration::from_millis(200));
        let elapsed = start.elapsed();

        assert!(out.is_none(), "a ps that never answers must not produce a listing");
        assert!(
            elapsed < Duration::from_secs(2),
            "the deadline was not honoured: waited {elapsed:?}"
        );
        assert!(!ProcTable::from_ps_listing("").is_readable());

        // Prove the fake really ran, rather than the deadline being honoured
        // because the exec failed. Polled rather than asserted outright: this
        // is a test about a slow subprocess, so its own subprocess may be slow.
        assert!(
            wait_for(&marker, Duration::from_secs(30)),
            "the slow ps never started, so the deadline proved nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn wait_for(path: &std::path::Path, budget: Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        path.exists()
    }

    /// A `ps` that runs and says nothing.
    ///
    /// `true` is used rather than a written script, because it needs no
    /// marker to prove it ran: it exists already, ignores its arguments,
    /// prints nothing and exits 0. Two earlier versions of this test were
    /// flaky, and both times the flake was in the scaffolding rather than in
    /// the thing under test.
    #[test]
    fn a_silent_ps_reads_as_unreadable() {
        let Some(program) = ["/bin/true", "/usr/bin/true"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
        else {
            return; // no `true` on this machine; nothing to prove with
        };
        let out = run_ps_program(program, Duration::from_secs(30));
        assert_eq!(out.as_deref(), Some(""), "`true` should run and print nothing");
        assert!(
            !ProcTable::from_ps_listing("").is_readable(),
            "a ps that runs and says nothing must not read as an empty machine"
        );
    }

    #[test]
    fn a_truncated_ps_reads_as_unreadable() {
        let dir = std::env::temp_dir().join(format!("proctable-trunc-{}", me()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Real rows, but the listing stops before it reaches us.
        let ps = fake_ps(&dir, "trunc-ps", "echo '1 0 1 S 100 0.0 Wed Sep  3 11:00:00 2026'");
        let out = run_ps_program(ps.to_str().unwrap(), Duration::from_secs(30))
            .expect("the fake ps did not run, so this proved nothing");
        assert!(!out.is_empty(), "precondition: this ps did print something");
        assert!(
            !ProcTable::from_ps_listing(&out).is_readable(),
            "a listing that stops before our own row is truncated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_ps_reads_as_unreadable() {
        assert!(run_ps_program("/nonexistent/ps", Duration::from_secs(1)).is_none());
    }

    // ---- against the real machine ---------------------------------------

    #[test]
    fn the_real_table_knows_this_very_process() {
        let t = ProcTable::read();
        assert!(t.is_readable(), "could not read the process table at all");
        assert_eq!(t.is_running(me()), Answer::Known(true));
        match t.identity(me()) {
            Answer::Known(id) => assert_ne!(id, LiveIdentity::new("")),
            other => panic!("no identity for our own pid: {other:?}"),
        }
        let row = t.row(me()).known().cloned().expect("our own row");
        assert_eq!(row.pid, me());
        assert!(row.ppid > 0);
        assert!(row.pgid > 0);
    }

    #[test]
    fn a_pid_that_cannot_exist_is_definitely_absent() {
        let t = ProcTable::read();
        assert!(t.is_readable());
        assert!(t.is_running(0x7FFF_FFFF).is_definitely_absent());
    }

    /// An unreaped child must never read as running. **The two platforms
    /// reach that answer differently, and the test asserts the answer.**
    ///
    /// On Linux the corpse keeps a row with state `Z`, so the table says
    /// `Known(false)`. On macOS libproc stops listing it the moment it exits,
    /// even before `wait`, so the table says `NotPresent`. Measured on a real
    /// Mac by `Silber.pty` on 2026-09-03: the child was seen once as `S`, and
    /// every read after that was `NotPresent`.
    ///
    /// An earlier version of this test asserted the Linux mechanism and failed
    /// on macOS after 200 reads. The product was right and the test was wrong.
    #[test]
    fn an_unreaped_child_never_reads_as_running() {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id() as i32;

        let mut settled = None;
        for _ in 0..500 {
            let t = ProcTable::read();
            match t.is_running(pid) {
                // Linux: still listed, but a corpse.
                Answer::Known(false) => {
                    settled = Some("listed as not running");
                    break;
                }
                // macOS: gone from the listing entirely.
                Answer::NotPresent => {
                    settled = Some("no longer listed");
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let how = settled.expect("an exited child still read as running");

        // Whichever route, the conclusion callers depend on is the same.
        assert!(
            registry_has_exited(pid),
            "an unreaped child must count as exited ({how})"
        );
        let _ = child.wait();
    }

    fn registry_has_exited(pid: i32) -> bool {
        match has_exited(pid) {
            Answer::Known(z) => z,
            Answer::NotPresent => true,
            Answer::Unknown(_) => false,
        }
    }
}
