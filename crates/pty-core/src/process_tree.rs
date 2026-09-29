//! Descendant termination without process-group signals: one snapshot of
//! the child's tree bound to process start identities, then exact TERM and
//! KILL signals to the identities that still match.
//!
//! node: src/process-tree.ts

use std::time::{Duration, Instant};
use crate::proctable::Answer;

pub use crate::proctable::{LiveIdentity, ProcTable, Row as ProcessRow};

/// One descendant, pinned to its start identity so a reused pid is skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub identity: LiveIdentity,
    pub depth: u32,
}

/// Walk a tree from `root_pid`, breadth first, recording depth.
fn walk(root_pid: i32, table: &ProcTable) -> Vec<(i32, u32)> {
    let mut children: std::collections::HashMap<i32, Vec<i32>> = Default::default();
    for r in table.rows() {
        children.entry(r.ppid).or_default().push(r.pid);
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::from([root_pid]);
    let mut queue: std::collections::VecDeque<(i32, u32)> = children
        .get(&root_pid)
        .map(|c| c.iter().map(|&p| (p, 1)).collect())
        .unwrap_or_default();
    while let Some((pid, depth)) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        out.push((pid, depth));
        for &c in children.get(&pid).map(|v| v.as_slice()).unwrap_or(&[]) {
            queue.push_back((c, depth + 1));
        }
    }
    out
}

/// Every distinct process group inside `root_pid`'s tree, plus the groups of
/// the root's own children.
///
/// **Deliberately not filtered by start token.** `snapshot_from_listing` drops
/// a descendant whose token cannot be read, and that process is then never
/// signalled. A group needs no identity, so collecting groups from the raw
/// listing reaches a process the token reader could not name.
///
/// The root's own group is excluded. A pty child calls `setsid`, so the daemon
/// sits alone in its group and signalling it would reach the daemon and
/// nothing else. Measured on Linux, both tools, 2026-09-03.
pub fn groups_in_tree(root_pid: i32, table: &ProcTable) -> Vec<i32> {
    let pgid_of: std::collections::HashMap<i32, i32> =
        table.rows().map(|r| (r.pid, r.pgid)).collect();
    let root_group = pgid_of.get(&root_pid).copied();
    let mut groups: Vec<i32> = Vec::new();
    for (pid, _) in walk(root_pid, table) {
        if let Some(&g) = pgid_of.get(&pid)
            && Some(g) != root_group
            && g > 1
            && !groups.contains(&g)
        {
            groups.push(g);
        }
    }
    groups.sort_unstable();
    groups
}

/// The live pids that still belong to any of `groups`. A zombie is excluded:
/// `ps` still lists it with its group, and counting it would make the sweep
/// report a group it has already emptied.
pub fn members_of_groups(groups: &[i32], table: &ProcTable) -> Vec<i32> {
    let mut out: Vec<i32> = table
        .rows()
        .filter(|r| !r.is_zombie() && groups.contains(&r.pgid))
        .map(|r| r.pid)
        .collect();
    out.sort_unstable();
    out
}

/// TERM every group, wait, KILL what is left, wait, then say what is STILL
/// there. The caller reports the return value; it never reports the sending.
///
/// `own_group` is skipped so the command survives to print its own result.
pub fn sweep_groups(
    groups: &[i32],
    own_group: i32,
    term_wait: Duration,
    kill_wait: Duration,
    mut live: impl FnMut(&[i32]) -> Vec<i32>,
    mut signal: impl FnMut(i32, i32),
) -> Vec<i32> {
    let targets: Vec<i32> = groups
        .iter()
        .copied()
        .filter(|&g| g > 1 && g != own_group)
        .collect();
    if targets.is_empty() {
        return live(groups);
    }
    for &g in &targets {
        signal(g, libc::SIGTERM);
    }
    let deadline = Instant::now() + term_wait;
    let mut remaining = live(&targets);
    while !remaining.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
        remaining = live(&targets);
    }
    if remaining.is_empty() {
        return Vec::new();
    }
    for &g in &targets {
        signal(g, libc::SIGKILL);
    }
    let deadline = Instant::now() + kill_wait;
    remaining = live(&targets);
    while !remaining.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
        remaining = live(&targets);
    }
    remaining
}

/// `kill(2)` on a whole process group.
pub fn signal_group(pgid: i32, signal: i32) {
    if pgid <= 1 {
        return;
    }
    // SAFETY: a negative pid is the documented way to signal a process group.
    unsafe { libc::kill(-pgid, signal) };
}

/// The caller's own process group.
pub fn own_process_group() -> i32 {
    // SAFETY: getpgrp(2) takes no arguments and cannot fail.
    unsafe { libc::getpgrp() }
}

/// BFS from `root_pid` over a `pid ppid` listing; deepest first, then the
/// higher pid first.
///
/// The descendants of `root_pid` from one table, deepest first, then the
/// higher pid first.
///
/// node: src/process-tree.ts:28-61
pub fn snapshot_from_table(root_pid: i32, table: &ProcTable) -> Vec<ProcessIdentity> {
    let mut out: Vec<ProcessIdentity> = walk(root_pid, table)
        .into_iter()
        .filter_map(|(pid, depth)| {
            table
                .identity(pid)
                .known()
                .map(|identity| ProcessIdentity { pid, identity, depth })
        })
        .collect();
    out.sort_by(|a, b| b.depth.cmp(&a.depth).then(b.pid.cmp(&a.pid)));
    out
}

/// A descendant snapshot that knows whether it saw the whole tree.
///
/// [`snapshot_from_table`] quietly leaves out a descendant it cannot name, and
/// an unreadable table gives it an empty tree. Either one looks exactly like a
/// tree with nothing left in it, so a teardown built on it claims a
/// containment it never observed. This type makes the difference a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeSnapshot {
    /// Every live descendant, each pinned to its identity.
    Complete(Vec<ProcessIdentity>),
    /// Completeness could not be proved. `identities` is every exact
    /// descendant that was still observed: safe to signal, but not all.
    Unavailable {
        reason: String,
        identities: Vec<ProcessIdentity>,
    },
}

impl TreeSnapshot {
    pub fn is_complete(&self) -> bool {
        matches!(self, TreeSnapshot::Complete(_))
    }

    pub fn identities(&self) -> &[ProcessIdentity] {
        match self {
            TreeSnapshot::Complete(identities) | TreeSnapshot::Unavailable { identities, .. } => {
                identities
            }
        }
    }

    fn unavailable(reason: impl Into<String>, identities: Vec<ProcessIdentity>) -> Self {
        TreeSnapshot::Unavailable {
            reason: reason.into(),
            identities,
        }
    }
}

/// Every exact, live descendant visible in `table`, deepest first. Zombies are
/// skipped (they are dead) but their children are still walked.
fn observed_identities(root_pid: i32, table: &ProcTable) -> Vec<ProcessIdentity> {
    let mut out: Vec<ProcessIdentity> = walk(root_pid, table)
        .into_iter()
        .filter_map(|(pid, depth)| {
            let row = table.row(pid).known()?;
            if row.is_zombie() {
                return None;
            }
            let identity = row.identity.clone()?;
            Some(ProcessIdentity {
                pid,
                identity,
                depth,
            })
        })
        .collect();
    out.sort_by(|a, b| b.depth.cmp(&a.depth).then(b.pid.cmp(&a.pid)));
    out
}

/// Snapshot `root_pid`'s descendants from `table`, or say why the snapshot
/// cannot be called complete.
///
/// Complete means: the table is whole, the root is live and named, and every
/// live descendant is named. Anything less is `Unavailable`, carrying the
/// identities that were observed.
///
/// node: src/process-tree.ts `snapshotDescendantProcessesComplete` (PR #182)
pub fn complete_snapshot_from_table(root_pid: i32, table: &ProcTable) -> TreeSnapshot {
    if !table.is_readable() {
        return TreeSnapshot::unavailable("process-table-unreadable", Vec::new());
    }
    let observed = observed_identities(root_pid, table);
    if !table.is_complete() {
        return TreeSnapshot::unavailable("process-table-incomplete", observed);
    }
    match table.row(root_pid).known() {
        Some(root) if !root.is_zombie() && root.identity.is_some() => {}
        _ => return TreeSnapshot::unavailable("root-identity-unavailable", observed),
    }
    for (pid, _) in walk(root_pid, table) {
        if let Some(row) = table.row(pid).known()
            && !row.is_zombie()
            && row.identity.is_none()
        {
            return TreeSnapshot::unavailable(
                format!("process-identity-unavailable:{pid}"),
                observed,
            );
        }
    }
    TreeSnapshot::Complete(observed)
}

/// Add every identity in `more` that `into` does not already hold.
fn merge_identities(into: &mut Vec<ProcessIdentity>, more: &[ProcessIdentity]) {
    for candidate in more {
        if !into
            .iter()
            .any(|known| known.pid == candidate.pid && known.identity == candidate.identity)
        {
            into.push(candidate.clone());
        }
    }
    into.sort_by(|a, b| b.depth.cmp(&a.depth).then(b.pid.cmp(&a.pid)));
}

fn same_identities(a: &[ProcessIdentity], b: &[ProcessIdentity]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| a.pid == b.pid && a.identity == b.identity)
}

/// Identities observed anywhere beneath the root that this table still shows
/// as the same live processes. Reparenting does not release them from the
/// containment proof; only confirmed exit, zombie state, or PID reuse does.
fn still_live_identities(
    observed: &[ProcessIdentity],
    table: &ProcTable,
) -> Result<Vec<ProcessIdentity>, i32> {
    let mut live = Vec::new();
    for identity in observed {
        match table.row(identity.pid) {
            Answer::Known(row) if row.is_zombie() => {}
            Answer::Known(row) => match row.identity.as_ref() {
                Some(current) if current == &identity.identity => live.push(identity.clone()),
                Some(_) => {}
                None => return Err(identity.pid),
            },
            Answer::NotPresent => {}
            Answer::Unknown(_) => return Err(identity.pid),
        }
    }
    live.sort_by(|a, b| b.depth.cmp(&a.depth).then(b.pid.cmp(&a.pid)));
    Ok(live)
}

/// How many times the tree is re-read, after the stop is sent, before it is
/// declared unable to hold still.
const FREEZE_ATTEMPTS: usize = 16;

/// Is the root, and every identity in `identities`, stopped in `table`?
fn all_stopped(root_pid: i32, identities: &[ProcessIdentity], table: &ProcTable) -> bool {
    std::iter::once(root_pid)
        .chain(identities.iter().map(|identity| identity.pid))
        .all(|pid| table.row(pid).known().is_some_and(|row| row.is_stopped()))
}

/// Stop the whole tree, then prove the snapshot complete.
///
/// A snapshot of a running tree is out of date as soon as it is taken: any
/// member can fork before it is signalled, and the new process was never
/// seen. So this stops the root and its group (`stop_root`, which is `false`
/// if the root could not be stopped) and every descendant it observes, then
/// re-reads the table.
///
/// **A sent SIGSTOP is not a stopped process.** `kill` returns once the stop
/// is queued; a target running on another CPU keeps running, and can fork or
/// reparent until it takes it. So a read counts only when it shows the root
/// and every still-live identity observed on any read in a stopped state, and
/// containment is `Complete` only when two such reads in a row agree. The
/// second read starts after every known member was seen stopped, so any child
/// forked before the stop took effect is already in it. Until then the members
/// are stopped again (a new one may have joined) and the table re-read, a
/// little later each time.
///
/// An incomplete first read is never upgraded: the tree is still stopped, but
/// the answer stays `Unavailable`, with every identity observed on any read.
///
/// node: src/process-tree.ts `freezeDescendantProcesses` (PR #182), which
/// trusts the sent signal; this does not.
pub fn freeze_descendants(
    root_pid: i32,
    stop_root: impl FnOnce() -> bool,
    mut read: impl FnMut() -> ProcTable,
    mut stop: impl FnMut(&[ProcessIdentity]) -> Vec<i32>,
) -> TreeSnapshot {
    let first = complete_snapshot_from_table(root_pid, &read());
    let mut observed = first.identities().to_vec();
    let first_unavailable = match &first {
        TreeSnapshot::Complete(_) => None,
        TreeSnapshot::Unavailable { reason, .. } => Some(reason.clone()),
    };
    if !stop_root() {
        return TreeSnapshot::unavailable("root-freeze-failed", observed);
    }
    let attempts = if first_unavailable.is_some() {
        1
    } else {
        FREEZE_ATTEMPTS
    };
    let mut previous: Option<Vec<ProcessIdentity>> = None;
    let mut seen_running = false;
    for attempt in 0..attempts {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(attempt.min(10) as u64));
        }
        let targeted = observed.clone();
        let signalled = stop(&targeted);
        let table = read();
        let next = complete_snapshot_from_table(root_pid, &table);
        merge_identities(&mut observed, next.identities());
        match next {
            TreeSnapshot::Complete(_) => {
                let identities = match still_live_identities(&observed, &table) {
                    Ok(identities) => identities,
                    Err(pid) => {
                        return TreeSnapshot::unavailable(
                            format!("observed-identity-unavailable:{pid}"),
                            observed,
                        );
                    }
                };
                let failed_to_stop = targeted.iter().any(|target| {
                    identities.iter().any(|live| {
                        live.pid == target.pid
                            && live.identity == target.identity
                            && !signalled.contains(&live.pid)
                            && table
                                .row(live.pid)
                                .known()
                                .is_none_or(|row| !row.is_stopped())
                    })
                });
                if failed_to_stop {
                    return TreeSnapshot::unavailable("descendant-freeze-unverified", observed);
                }
                if all_stopped(root_pid, &identities, &table) {
                    if previous
                        .as_deref()
                        .is_some_and(|previous| same_identities(previous, &identities))
                    {
                        return match first_unavailable {
                            Some(reason) => TreeSnapshot::unavailable(reason, observed),
                            None => TreeSnapshot::Complete(identities),
                        };
                    }
                    previous = Some(identities);
                } else {
                    seen_running = true;
                    previous = None;
                }
            }
            TreeSnapshot::Unavailable { reason, .. } => {
                return TreeSnapshot::unavailable(reason, observed);
            }
        }
    }
    let reason = match first_unavailable {
        Some(reason) => reason,
        None if seen_running => "process-tree-not-stopped".to_string(),
        None => "process-tree-did-not-quiesce".to_string(),
    };
    TreeSnapshot::unavailable(reason, observed)
}

/// Does any process still belong to group `pgid`? A permission error still
/// means yes.
pub fn group_exists(pgid: i32) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: signal 0 to a process group only asks whether it has members.
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The fallback when a complete descendant snapshot is unavailable: TERM the
/// group the PTY child leads, wait for it to empty, KILL what is left.
/// `true` when the group is gone.
///
/// Each signal is sent only while the group still has a member. A group id
/// cannot be handed out again while any process belongs to the group, so this
/// reaches the tree and nothing that later reused the number.
///
/// node: src/process-tree.ts `terminateProcessGroup` (PR #182)
pub fn terminate_process_group(pgid: i32, term_wait: Duration, kill_wait: Duration) -> bool {
    terminate_process_group_with(pgid, term_wait, kill_wait, group_exists, signal_group)
}

fn terminate_process_group_with(
    pgid: i32,
    term_wait: Duration,
    kill_wait: Duration,
    mut exists: impl FnMut(i32) -> bool,
    mut signal: impl FnMut(i32, i32),
) -> bool {
    for (sig, wait) in [(libc::SIGTERM, term_wait), (libc::SIGKILL, kill_wait)] {
        if !exists(pgid) {
            return true;
        }
        signal(pgid, sig);
        let deadline = Instant::now() + wait;
        while exists(pgid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    !exists(pgid)
}

/// Is this still the same process, and still running?
///
/// **A corpse is not a survivor.** On Linux an unreaped descendant keeps its
/// `/proc` row and its identity, so matching on identity alone counted it as
/// alive: the teardown would wait out its whole TERM budget for a process that
/// had already died, then report it as a descendant that survived a SIGKILL.
/// That is the kill over-claiming again, in the other direction.
///
/// macOS never had this: libproc stops listing a process the moment it exits.
/// `Silber.pty` measured that on a real Mac on 2026-09-03, and chasing why its
/// zombie test failed is what found this.
///
/// **One process, not the whole table.** A full table read costs about as much
/// as 473 single reads (measured on Linux, 2026-09-03: 4.21 ms against
/// 0.0089 ms), so a poll loop over a handful of descendants asks about each one
/// rather than re-reading the machine.
///
/// **An unreadable answer means `false`, and that is deliberate**: it says
/// "do not signal", never "it is gone". Every caller here wants the safe
/// direction for a signal; a caller that needs the difference asks
/// `proctable` and reads the three cases.
fn is_same_process(id: &ProcessIdentity) -> bool {
    match crate::proctable::process(id.pid) {
        crate::proctable::Answer::Known(row) if !row.is_zombie() => {
            row.identity.as_ref() == Some(&id.identity)
        }
        _ => false,
    }
}

/// Signal the identities whose start token still matches. Returns the pids
/// signalled.
///
/// node: src/process-tree.ts:69-86
pub fn signal_process_identities(ids: &[ProcessIdentity], signal: i32) -> Vec<i32> {
    let mut signalled = Vec::new();
    for id in ids {
        if !is_same_process(id) {
            continue;
        }
        // SAFETY: plain kill(2) on a pid we just re-verified.
        if unsafe { libc::kill(id.pid, signal) } == 0 {
            signalled.push(id.pid);
        }
    }
    signalled
}

/// **One question per surviving identity, and each question is now cheap.**
///
/// The shape of this loop is unchanged and it is now affordable.
///
/// It asks about each surviving descendant separately, which is right: a full
/// table read costs about as much as 473 single reads. What changed is the
/// cost of one question. On macOS it used to be a `ps` subprocess, so at 25 ms
/// polling inside a 1500 ms budget four descendants cost 240 spawns — 2.6
/// seconds of spawning inside a 1.5 second deadline, at the 10.9 ms a spawn was
/// measured to take. **The loop could not meet its own deadline on an idle
/// machine.** Each question is now one `proc_pidinfo` call there and one small
/// `/proc` read on Linux.
fn wait_for_identities_to_exit(ids: &[ProcessIdentity], timeout: Duration) -> Vec<ProcessIdentity> {
    let deadline = Instant::now() + timeout;
    let mut survivors: Vec<ProcessIdentity> =
        ids.iter().filter(|i| is_same_process(i)).cloned().collect();
    while !survivors.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
        survivors.retain(is_same_process);
    }
    survivors
}

/// Default TERM grace (`termWaitMs`).
pub const TERM_WAIT: Duration = Duration::from_millis(1_500);
/// Default KILL grace (`killWaitMs`).
pub const KILL_WAIT: Duration = Duration::from_millis(500);

/// After a terminal hangup, give descendants time to handle SIGHUP before
/// escalating to TERM and KILL. The wait ends as soon as they all exit.
pub fn terminate_process_identities_after_hangup(
    ids: &[ProcessIdentity],
    hup_wait: Duration,
    term_wait: Duration,
    kill_wait: Duration,
) -> Vec<ProcessIdentity> {
    let survivors = wait_for_identities_to_exit(ids, hup_wait);
    terminate_process_identities(&survivors, term_wait, kill_wait)
}

/// TERM, wait ≤ `term_wait`; KILL the survivors, wait ≤ `kill_wait`; return
/// whatever is still alive.
///
/// node: src/process-tree.ts:109-124
pub fn terminate_process_identities(
    ids: &[ProcessIdentity],
    term_wait: Duration,
    kill_wait: Duration,
) -> Vec<ProcessIdentity> {
    if ids.is_empty() {
        return Vec::new();
    }
    signal_process_identities(ids, libc::SIGTERM);
    let after_term = wait_for_identities_to_exit(ids, term_wait);
    if after_term.is_empty() {
        return Vec::new();
    }
    signal_process_identities(&after_term, libc::SIGKILL);
    wait_for_identities_to_exit(&after_term, kill_wait)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// node: tests/process-tree.test.ts:10-79
    #[test]
    fn snapshot_is_deepest_first_with_tokens() {
        // 201 has no readable identity, so it is not in the snapshot.
        let table = crate::proctable::table_from_shape(
            "100 1 100 Ss linux:100\n200 100 200 S linux:200\n300 200 200 S linux:300\n201 100 200 S -\n999 1 999 S linux:999\n",
        );
        let ids = snapshot_from_table(100, &table);
        assert_eq!(
            ids,
            vec![
                ProcessIdentity {
                    pid: 300,
                    identity: LiveIdentity::new("linux:300"),
                    depth: 2
                },
                ProcessIdentity {
                    pid: 200,
                    identity: LiveIdentity::new("linux:200"),
                    depth: 1
                },
            ]
        );
    }

    /// An unreaped descendant kept its `/proc` row and its identity on Linux,
    /// so the teardown counted a corpse as a survivor: it waited out the whole
    /// TERM budget for a process that had already died, and would then report
    /// it as having survived a SIGKILL.
    ///
    /// macOS never had this, because libproc drops the process at once. The
    /// disagreement between the two platforms is what exposed it.
    #[test]
    fn a_corpse_is_not_a_survivor() {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id() as i32;
        let identity = crate::proctable::process(pid)
            .known()
            .and_then(|r| r.identity);
        // On macOS it may already be gone, and then there is nothing to pin.
        if let Some(identity) = identity {
            let id = ProcessIdentity { pid, identity, depth: 1 };
            let deadline = Instant::now() + Duration::from_secs(5);
            while is_same_process(&id) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                !is_same_process(&id),
                "an exited but unreaped child still reads as a live descendant"
            );
            assert!(
                signal_process_identities(&[id], 0).is_empty(),
                "and nothing should be signalled at it"
            );
        }
        let _ = child.wait();
    }

    #[test]
    fn dead_identities_are_skipped_by_signal() {
        let ids = vec![ProcessIdentity {
            pid: std::process::id() as i32,
            identity: LiveIdentity::new("not-the-real-identity"),
            depth: 1,
        }];
        assert!(signal_process_identities(&ids, 0).is_empty());
        assert!(terminate_process_identities(&ids, Duration::ZERO, Duration::ZERO).is_empty());
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use std::cell::RefCell;

    // daemon 100 (its own group), pty child 200 (setsid: its own group and
    // session), 300 under the child, and 400 in a background group of its own.
    // 900 is an unrelated process. This is the shape measured on Linux for
    // both tools on 2026-09-03.
    fn rows() -> ProcTable {
        crate::proctable::table_from_shape(
            "100 1 100 Ss\n200 100 200 Ss\n300 200 200 S\n400 300 400 S\n900 1 900 S\n",
        )
    }

    #[test]
    fn the_daemons_own_group_is_not_a_target() {
        let groups = groups_in_tree(100, &rows());
        assert!(
            !groups.contains(&100),
            "the daemon sits alone in its group; signalling it reaches the daemon and nothing else"
        );
        assert_eq!(groups, vec![200, 400]);
    }

    #[test]
    fn an_unrelated_group_is_never_a_target() {
        assert!(!groups_in_tree(100, &rows()).contains(&900));
    }

    /// The reason to sweep groups at all: `snapshot_from_listing` drops a
    /// descendant whose start token cannot be read, so that process is never
    /// signalled. Groups are read from the raw listing, so its group is still
    /// a target.
    #[test]
    fn a_descendant_with_no_readable_token_is_still_inside_a_target_group() {
        let rows = rows();
        // 400 is the one whose identity cannot be read.
        let unnamed = crate::proctable::table_from_shape(
            "100 1 100 Ss\n200 100 200 Ss\n300 200 200 S\n400 300 400 S -\n900 1 900 S\n",
        );
        let snapshot = snapshot_from_table(100, &unnamed);
        assert!(
            !snapshot.iter().any(|i| i.pid == 400),
            "precondition: the token reader drops 400 from the snapshot"
        );
        assert!(
            groups_in_tree(100, &unnamed).contains(&400),
            "its group must still be a target"
        );
    }

    #[test]
    fn members_are_read_back_by_group() {
        let rows = rows();
        assert_eq!(members_of_groups(&[200], &rows), vec![200, 300]);
        assert_eq!(members_of_groups(&[400], &rows), vec![400]);
        assert_eq!(members_of_groups(&[], &rows), Vec::<i32>::new());
    }

    /// `ps` lists a zombie with its process group. Counting it would make the
    /// sweep report a group it has already emptied, and then signal it again.
    /// Measured on Linux 2026-09-03: the row reads `<pid> <ppid> <pgid> Z`.
    #[test]
    fn a_zombie_is_not_a_group_member() {
        let rows = crate::proctable::table_from_shape(
            "100 1 100 Ss\n200 100 200 Sl\n300 200 200 Z\n",
        );
        assert_eq!(
            members_of_groups(&[200], &rows),
            vec![200],
            "300 is a corpse and must not count as a member"
        );
    }

    fn sweep(groups: &[i32], own: i32, alive: Vec<Vec<i32>>) -> (Vec<i32>, Vec<(i32, i32)>) {
        let sent = RefCell::new(Vec::new());
        let step = RefCell::new(0usize);
        let left = sweep_groups(
            groups,
            own,
            Duration::ZERO,
            Duration::ZERO,
            |_| {
                let mut i = step.borrow_mut();
                let v = alive.get(*i).cloned().unwrap_or_default();
                *i += 1;
                v
            },
            |g, sig| sent.borrow_mut().push((g, sig)),
        );
        (left, sent.into_inner())
    }

    #[test]
    fn a_group_that_answers_term_is_never_killed() {
        let (left, sent) = sweep(&[200], 5, vec![vec![]]);
        assert!(left.is_empty());
        assert_eq!(sent, vec![(200, libc::SIGTERM)], "SIGKILL must not be sent");
    }

    /// A coding agent was measured ignoring SIGTERM for ten seconds. A design
    /// that sends one TERM and hopes is already known not to work here.
    #[test]
    fn a_group_that_ignores_term_is_killed() {
        let (left, sent) = sweep(&[200], 5, vec![vec![300], vec![]]);
        assert!(left.is_empty());
        assert_eq!(sent, vec![(200, libc::SIGTERM), (200, libc::SIGKILL)]);
    }

    /// The command reports the machine, not the signals. A group that outlives
    /// SIGKILL comes back as still there.
    #[test]
    fn what_outlives_sigkill_is_returned_not_swallowed() {
        let (left, _) = sweep(&[200], 5, vec![vec![300], vec![300]]);
        assert_eq!(left, vec![300]);
    }

    #[test]
    fn my_own_group_is_never_signalled() {
        let (_, sent) = sweep(&[200], 200, vec![vec![]]);
        assert!(sent.is_empty(), "the command must survive to print its result");
    }

    #[test]
    fn group_one_and_below_are_never_signalled() {
        let (_, sent) = sweep(&[0, 1, -1], 999, vec![vec![]]);
        assert!(sent.is_empty());
    }
}

#[cfg(test)]
mod real_group_tests {
    use super::*;

    fn live(targets: &[i32]) -> Vec<i32> {
        members_of_groups(targets, &ProcTable::read())
    }

    /// The mocked sweep tests prove the ordering. This one proves the ordering
    /// is about real processes: it builds a real process group whose members
    /// ignore SIGTERM, runs the real sweep against it, and checks the machine
    /// afterwards rather than the signals sent at it.
    ///
    /// A coding agent was measured ignoring SIGTERM for ten seconds on
    /// 2026-09-02. This is that shape, in a test.
    #[test]
    fn a_real_group_that_ignores_term_is_killed_and_verified_gone() {
        // A new process group of our own, so nothing else on the machine is in
        // it.
        //
        // **macOS has the `setsid` system call but no `setsid` executable.**
        // An earlier version of this test spawned the binary, so on the one
        // platform where process groups are the whole escalation story, the
        // test could not run at all. `process_group(0)` is the same thing
        // without the command. Reported from a real Mac by `Silber.pty` on
        // 2026-09-03.
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 60 & sleep 60"])
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sh in its own group");
        let leader = child.id() as i32;

        // Wait for the group to actually exist before testing anything.
        let deadline = Instant::now() + Duration::from_secs(5);
        while live(&[leader]).len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        let members = live(&[leader]);
        assert!(
            members.len() >= 2,
            "precondition: expected a group with the shell and its child, got {members:?}"
        );

        // SIGTERM alone must not be enough, or the test proves nothing about
        // escalation.
        signal_group(leader, libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !live(&[leader]).is_empty(),
            "precondition: this group is supposed to ignore SIGTERM"
        );

        let still_there = sweep_groups(
            &[leader],
            own_process_group(),
            Duration::from_millis(500),
            Duration::from_secs(2),
            |t| live(t),
            signal_group,
        );

        // Clean up the whole group before asserting. The leader alone is not
        // the cleanup: on a failure its group would leak into the next run.
        let verdict = (still_there.clone(), live(&[leader]));
        signal_group(leader, libc::SIGKILL);
        let _ = child.wait();

        assert!(
            verdict.0.is_empty(),
            "the sweep reported success while these were alive: {:?}",
            verdict.0
        );
        assert!(
            verdict.1.is_empty(),
            "the process table disagrees with the sweep"
        );
    }

    /// The command must not signal the group it is running in, or it dies
    /// before it can report. Proven against this test process's own group.
    #[test]
    fn the_running_process_group_is_never_signalled() {
        let own = own_process_group();
        let me = std::process::id() as i32;
        let still_there = sweep_groups(
            &[own],
            own,
            Duration::ZERO,
            Duration::ZERO,
            |t| live(t),
            signal_group,
        );
        assert!(
            still_there.contains(&me) || !still_there.is_empty(),
            "the sweep should report our own group as still there, not signal it"
        );
        assert!(registry_alive(me), "we must still be running");
    }

    fn registry_alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }
}

#[cfg(test)]
mod unreadable_token_tests {
    use super::*;

    /// **A descendant whose start token cannot be read is left out of the
    /// snapshot entirely, and therefore never signalled.**
    ///
    /// This pins the current behaviour rather than endorsing it. It matters
    /// less than it did: the group sweep reaches such a process anyway,
    /// because a group needs no identity. It still means the exact per-pid
    /// teardown will not signal it.
    ///
    /// The Node tool omits it in the same way (`src/process-tree.ts`), so
    /// this is shared rather than a difference. Recorded because it is a
    /// candidate mechanism for a harness that survived a `pty kill` on a Mac
    /// on 2026-09-02.
    #[test]
    fn a_descendant_with_no_readable_token_is_not_in_the_snapshot() {
        use crate::proctable::table_from_shape;
        // daemon 100 -> middle 200 -> harness 300. The middle answers; the
        // harness does not.
        let unnamed = table_from_shape("100 1 100\n200 100 200\n300 200 200 S -");
        let ids = super::snapshot_from_table(100, &unnamed);
        let pids: Vec<i32> = ids.iter().map(|i| i.pid).collect();
        assert_eq!(pids, vec![200], "300 was dropped from the exact teardown");

        // And with a readable identity it is there, deepest first.
        let named = table_from_shape("100 1 100\n200 100 200\n300 200 200");
        let ids = super::snapshot_from_table(100, &named);
        let pids: Vec<i32> = ids.iter().map(|i| i.pid).collect();
        assert_eq!(pids, vec![300, 200]);

        // But its GROUP is still a target, which is how it now gets signalled.
        assert!(super::groups_in_tree(100, &unnamed).contains(&200));
    }

    /// Its children are still walked, so only the unreadable process escapes
    /// and not its subtree.
    #[test]
    fn the_subtree_below_an_unreadable_process_is_still_reached() {
        let table = crate::proctable::table_from_shape(
            "100 1 100\n200 100 200\n300 200 200 S -\n400 300 200",
        );
        let ids = super::snapshot_from_table(100, &table);
        let pids: Vec<i32> = ids.iter().map(|i| i.pid).collect();
        assert_eq!(pids, vec![400, 200]);
    }
}

/// PR #34: the deadline teardown recorded `deadline` from a snapshot that
/// could not tell a whole tree from an unreadable or partial one. These pin
/// the observation that now decides it.
#[cfg(test)]
mod containment_tests {
    use super::*;
    use crate::proctable::table_from_shape;
    use std::cell::RefCell;

    /// daemon child 100 -> 200 -> 300, and 400 in a group of its own.
    const TREE: &str = "100 1 100\n200 100 100\n300 200 100\n400 100 400\n999 1 999\n";

    fn pids(snapshot: &TreeSnapshot) -> Vec<i32> {
        snapshot
            .identities()
            .iter()
            .map(|identity| identity.pid)
            .collect()
    }

    fn reason(snapshot: &TreeSnapshot) -> &str {
        match snapshot {
            TreeSnapshot::Unavailable { reason, .. } => reason,
            TreeSnapshot::Complete(_) => {
                panic!("expected an unavailable snapshot, got {snapshot:?}")
            }
        }
    }

    #[test]
    fn a_whole_readable_tree_is_complete() {
        let snapshot = complete_snapshot_from_table(100, &table_from_shape(TREE));
        assert!(snapshot.is_complete());
        assert_eq!(pids(&snapshot), vec![300, 400, 200]);
    }

    #[test]
    fn an_unreadable_table_is_never_an_empty_complete_tree() {
        let snapshot = complete_snapshot_from_table(100, &ProcTable::unreadable());
        assert_eq!(reason(&snapshot), "process-table-unreadable");
        assert!(snapshot.identities().is_empty());
    }

    #[test]
    fn a_table_that_skipped_rows_is_incomplete_but_keeps_what_it_saw() {
        let snapshot =
            complete_snapshot_from_table(100, &table_from_shape(TREE).marked_incomplete());
        assert_eq!(reason(&snapshot), "process-table-incomplete");
        assert_eq!(
            pids(&snapshot),
            vec![300, 400, 200],
            "observed identities were dropped"
        );
    }

    #[test]
    fn a_descendant_without_an_identity_makes_the_tree_incomplete() {
        let table = table_from_shape("100 1 100\n200 100 100 S -\n300 200 100\n400 100 400\n");
        let snapshot = complete_snapshot_from_table(100, &table);
        assert_eq!(reason(&snapshot), "process-identity-unavailable:200");
        // Its named child, and the out-of-group sibling, are still signalled.
        assert_eq!(pids(&snapshot), vec![300, 400]);
    }

    #[test]
    fn a_root_that_is_gone_or_dead_or_unnamed_proves_nothing() {
        for (shape, what) in [
            ("200 100 100\n", "gone: its children were reparented"),
            ("100 1 100 Z\n200 100 100\n", "a zombie"),
            ("100 1 100 S -\n200 100 100\n", "unnamed"),
        ] {
            let snapshot = complete_snapshot_from_table(100, &table_from_shape(shape));
            assert_eq!(
                reason(&snapshot),
                "root-identity-unavailable",
                "root {what}"
            );
        }
    }

    #[test]
    fn a_zombie_descendant_is_skipped_but_its_children_are_walked() {
        let snapshot = complete_snapshot_from_table(
            100,
            &table_from_shape("100 1 100\n200 100 100 Z\n300 200 100\n"),
        );
        assert!(snapshot.is_complete());
        assert_eq!(pids(&snapshot), vec![300]);
    }

    /// A freeze over `reads`, recording what was stopped.
    fn freeze(
        stop_root: bool,
        reads: Vec<ProcTable>,
        stop_all: bool,
    ) -> (TreeSnapshot, Vec<Vec<i32>>, usize) {
        let reads = RefCell::new(reads.into_iter());
        let stopped = RefCell::new(Vec::new());
        let read_count = RefCell::new(0);
        let snapshot = freeze_descendants(
            100,
            || stop_root,
            || {
                *read_count.borrow_mut() += 1;
                reads
                    .borrow_mut()
                    .next()
                    .expect("read more often than scripted")
            },
            |identities| {
                let pids: Vec<i32> = identities.iter().map(|identity| identity.pid).collect();
                stopped.borrow_mut().push(pids.clone());
                if stop_all {
                    pids
                } else {
                    pids.into_iter().skip(1).collect()
                }
            },
        );
        (snapshot, stopped.into_inner(), read_count.into_inner())
    }

    /// `TREE` with the child and every descendant stopped; 999, outside the
    /// tree, still runs.
    const STOPPED: &str = "100 1 100 T\n200 100 100 T\n300 200 100 T\n400 100 400 T\n999 1 999\n";

    fn stopped() -> ProcTable {
        table_from_shape(STOPPED)
    }

    #[test]
    fn a_tree_seen_stopped_twice_is_frozen_complete() {
        let (snapshot, stopped_sets, reads) = freeze(
            true,
            vec![table_from_shape(TREE), stopped(), stopped()],
            true,
        );
        assert!(snapshot.is_complete());
        assert_eq!(pids(&snapshot), vec![300, 400, 200]);
        assert_eq!(stopped_sets, vec![vec![300, 400, 200]; 2]);
        assert_eq!(reads, 3);
    }

    /// PR #39 review: `kill(SIGSTOP)` returns once the stop is sent, not once
    /// the target has stopped, and a target still running can fork. A tree
    /// whose members all accepted the signal but still read as runnable is
    /// never called complete.
    #[test]
    fn a_sent_stop_that_has_not_taken_effect_is_not_containment() {
        let reads = (0..=FREEZE_ATTEMPTS)
            .map(|_| table_from_shape(TREE))
            .collect();
        let (snapshot, stopped_sets, _) = freeze(true, reads, true);
        assert_eq!(reason(&snapshot), "process-tree-not-stopped");
        assert_eq!(
            pids(&snapshot),
            vec![300, 400, 200],
            "observed identities were dropped"
        );
        assert!(
            stopped_sets.iter().all(|set| set == &vec![300, 400, 200]),
            "every attempt re-sends the stop"
        );

        // One runnable member is enough, and so is a runnable root.
        for runnable in ["300 200 100 R", "100 1 100 R"] {
            let pid = &runnable[..3];
            let shape = STOPPED
                .lines()
                .map(|line| {
                    if line.starts_with(pid) {
                        runnable
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let reads = (0..=FREEZE_ATTEMPTS)
                .map(|_| table_from_shape(&shape))
                .collect();
            let (snapshot, _, _) = freeze(true, reads, true);
            assert_eq!(
                reason(&snapshot),
                "process-tree-not-stopped",
                "{runnable} still runs"
            );
        }
    }

    /// A member that takes the stop late is waited for, not given up on.
    #[test]
    fn a_stop_that_takes_effect_late_is_waited_for() {
        let late = "100 1 100 T\n200 100 100 T\n300 200 100 R\n400 100 400 T\n999 1 999\n";
        let (snapshot, _, reads) = freeze(
            true,
            vec![
                table_from_shape(TREE),
                table_from_shape(late),
                stopped(),
                stopped(),
            ],
            true,
        );
        assert!(snapshot.is_complete(), "{snapshot:?}");
        assert_eq!(reads, 4);
    }

    /// PR #39 review: a descendant can reparent after accepting SIGSTOP but
    /// before the confirming reads. It remains part of the containment proof
    /// and teardown even though it is no longer beneath the root.
    #[test]
    fn a_reparented_observed_descendant_remains_in_complete_containment() {
        let running = table_from_shape("100 1 100 R\n200 100 100 R\n");
        let reparented = table_from_shape("100 1 100 T\n200 1 200 T\n");
        let (snapshot, stopped_sets, reads) =
            freeze(true, vec![running, reparented.clone(), reparented], true);

        assert!(
            snapshot.is_complete(),
            "a stopped reparented descendant is still contained: {snapshot:?}"
        );
        assert_eq!(
            pids(&snapshot),
            vec![200],
            "an observed live descendant was dropped from complete containment"
        );
        assert_eq!(stopped_sets, vec![vec![200], vec![200]]);
        assert_eq!(reads, 3);
    }

    #[test]
    fn a_fork_before_the_stop_took_effect_is_caught_and_stopped_too() {
        let grown_running = format!("{STOPPED}500 300 100 R\n");
        let grown_stopped = format!("{STOPPED}500 300 100 T\n");
        let (snapshot, stopped_sets, _) = freeze(
            true,
            vec![
                table_from_shape(TREE),
                table_from_shape(&grown_running),
                table_from_shape(&grown_stopped),
                table_from_shape(&grown_stopped),
            ],
            true,
        );
        assert!(snapshot.is_complete());
        assert_eq!(pids(&snapshot), vec![500, 300, 400, 200]);
        assert_eq!(stopped_sets.last().unwrap(), &vec![500, 300, 400, 200]);
    }

    #[test]
    fn a_tree_that_never_holds_still_is_not_complete() {
        let reads = (0..=FREEZE_ATTEMPTS)
            .map(|n| table_from_shape(&format!("{STOPPED}{} 100 100 T\n", 1000 + n)))
            .collect();
        let (snapshot, _, _) = freeze(true, reads, true);
        assert_eq!(reason(&snapshot), "process-tree-did-not-quiesce");
    }

    #[test]
    fn an_incomplete_first_read_is_never_upgraded_to_complete() {
        let (snapshot, stopped_sets, _) = freeze(
            true,
            vec![
                table_from_shape(TREE).marked_incomplete(),
                stopped(),
                stopped(),
            ],
            true,
        );
        assert_eq!(reason(&snapshot), "process-table-incomplete");
        assert_eq!(pids(&snapshot), vec![300, 400, 200]);
        assert_eq!(
            stopped_sets,
            vec![vec![300, 400, 200]],
            "what was observed must still be stopped"
        );
    }

    #[test]
    fn an_unreadable_rescan_is_unavailable_with_every_identity_seen() {
        let (snapshot, _, _) = freeze(
            true,
            vec![table_from_shape(TREE), ProcTable::unreadable()],
            true,
        );
        assert_eq!(reason(&snapshot), "process-table-unreadable");
        assert_eq!(pids(&snapshot), vec![300, 400, 200]);
    }

    #[test]
    fn a_root_that_cannot_be_stopped_is_unavailable() {
        let (snapshot, stopped, _) = freeze(false, vec![table_from_shape(TREE)], true);
        assert_eq!(reason(&snapshot), "root-freeze-failed");
        assert!(stopped.is_empty());
    }

    #[test]
    fn a_descendant_that_could_not_be_stopped_is_unavailable() {
        let (snapshot, _, _) = freeze(
            true,
            vec![table_from_shape(TREE), table_from_shape(TREE)],
            false,
        );
        assert_eq!(reason(&snapshot), "descendant-freeze-unverified");
    }

    #[test]
    fn the_group_fallback_terms_then_kills_only_while_the_group_exists() {
        let sent = RefCell::new(Vec::new());
        // Ignores TERM, dies on KILL.
        let gone = terminate_process_group_with(
            4242,
            Duration::from_millis(50),
            Duration::from_millis(50),
            |_| !sent.borrow().contains(&libc::SIGKILL),
            |pgid, signal| {
                assert_eq!(pgid, 4242);
                sent.borrow_mut().push(signal);
            },
        );
        assert!(gone);
        assert_eq!(sent.into_inner(), vec![libc::SIGTERM, libc::SIGKILL]);

        // An empty group is never signalled at all.
        let sent = RefCell::new(Vec::new());
        assert!(terminate_process_group_with(
            4242,
            Duration::ZERO,
            Duration::ZERO,
            |_| false,
            |_, signal| { sent.borrow_mut().push(signal) }
        ));
        assert!(sent.into_inner().is_empty());

        // A group that outlives KILL is reported, not swallowed.
        assert!(!terminate_process_group_with(
            4242,
            Duration::ZERO,
            Duration::ZERO,
            |_| true,
            |_, _| {}
        ));
    }

    /// PR #39 review, on real processes that are busy on a CPU when the stop
    /// is sent: when the freeze says `Complete`, every member really is
    /// stopped.
    #[test]
    fn a_real_busy_tree_is_complete_only_once_it_is_really_stopped() {
        use std::os::unix::process::CommandExt;
        let mut leader = std::process::Command::new("sh")
            .args([
                "-c",
                "(while :; do :; done) & (while :; do :; done) & while :; do :; done",
            ])
            .process_group(0)
            .spawn()
            .expect("spawn");
        let root = leader.id() as i32;
        let deadline = Instant::now() + Duration::from_secs(5);
        while observed_identities(root, &ProcTable::read()).len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let snapshot = freeze_descendants(
            root,
            || {
                signal_group(root, libc::SIGSTOP);
                unsafe { libc::kill(root, libc::SIGSTOP) == 0 }
            },
            ProcTable::read,
            |identities| signal_process_identities(identities, libc::SIGSTOP),
        );
        let after = ProcTable::read();
        let members: Vec<i32> = std::iter::once(root)
            .chain(snapshot.identities().iter().map(|id| id.pid))
            .collect();
        let states: Vec<String> = members
            .iter()
            .map(|pid| {
                after
                    .row(*pid)
                    .known()
                    .map(|row| row.state.clone())
                    .unwrap_or_default()
            })
            .collect();
        signal_group(root, libc::SIGKILL);
        let _ = leader.wait();
        assert!(snapshot.is_complete(), "{snapshot:?}");
        assert_eq!(snapshot.identities().len(), 2);
        assert!(
            states.iter().all(|state| state.starts_with('T')),
            "not every member was stopped: {states:?}"
        );
    }

    /// A real group: a member that ignores TERM is KILLed and verified gone.
    #[test]
    fn a_real_group_is_emptied_by_the_fallback() {
        use std::os::unix::process::CommandExt;
        let mut leader = std::process::Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 30 & wait"])
            .process_group(0)
            .spawn()
            .expect("spawn");
        let pgid = leader.id() as i32;
        // An unreaped member still counts as one; reap the leader as the
        // daemon's substrate would.
        let reaper = std::thread::spawn(move || leader.wait());
        std::thread::sleep(Duration::from_millis(100));
        assert!(group_exists(pgid));
        assert!(terminate_process_group(
            pgid,
            Duration::from_millis(200),
            Duration::from_secs(2)
        ));
        assert!(!group_exists(pgid));
        let _ = reaper.join();
    }
}
