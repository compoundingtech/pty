//! Per-connection state and the inbound packet handlers, one for one with
//! Node's `handleClient` switch.
//!
//! A client is in one of three roles — command (`pty send`, `pty stats`:
//! never attached), writable (ATTACH) or readonly (PEEK) — and, after an
//! ATTACH or PEEK, waits in `Settling` for its SCREEN cut. A settling client
//! receives no DATA and no EXIT: every byte the child produces meanwhile is
//! parsed into the terminal and lands in the SCREEN. The cut is synchronous
//! on the actor thread, so there is no window between "what the SCREEN
//! shows" and "what the next DATA continues from".
//!
//! node: src/server.ts:75-90, 904-1063, 1213-1267

use std::collections::BTreeMap;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use pty_core::protocol::{
    AttachedClient, MessageType, Packet, decode_attach_identity, decode_cell, decode_peek,
    decode_size, encode_exit, encode_geometry, encode_screen, encode_status_response,
};
use pty_core::registry::{self, MutateOptions, MutateStatus};
use pty_terminal::{Range, SerializeOpts};

use super::lifecycle::Daemon;

/// Node's `REDRAW_SETTLE_MS`: how long after a resize the child gets to
/// redraw before an attacher's SCREEN is cut.
pub const REDRAW_SETTLE: Duration = Duration::from_millis(80);

/// How long a `clientGeneration` write keeps retrying a held metadata lock.
/// Lock holders are short CLI writes; a holder that outlives this loses the
/// write, and the next client change carries the newer counter.
const CLIENT_METADATA_RETRY: Duration = Duration::from_secs(2);

/// The facts `clientGeneration` follows: what `pty stats` reports about the
/// writable clients and the session's negotiated size. Readonly and command
/// connections are left out on purpose, so that observing a session never
/// rewrites its record (docs/decisions/0015).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientFacts {
    rows: u16,
    cols: u16,
    /// `(connection id, rows, cols)` of every size-constraining client, in
    /// connection order.
    writable: Vec<(u64, u16, u16)>,
}

impl ClientFacts {
    /// The facts of a session no client has attached to yet.
    pub fn unattached(rows: u16, cols: u16) -> ClientFacts {
        ClientFacts {
            rows,
            cols,
            writable: Vec::new(),
        }
    }
}

/// Bytes to a client's socket, or an instruction to end/destroy it.
pub enum Out {
    Bytes(Vec<u8>),
    /// `socket.end()`: half-close, the peer closes when it is done.
    End,
    /// `socket.destroy()`: close both ways now.
    Destroy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// `attachSeq === 0 && !readonly`: never sent ATTACH or PEEK.
    Command,
    Writable,
    Readonly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutKind {
    Attach { size_matched: bool },
    Peek { plain: bool, full: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Live,
    Settling {
        deadline: Instant,
        generation: u64,
        kind: CutKind,
    },
}

pub struct Client {
    pub tx: Sender<Out>,
    pub role: Role,
    pub rows: u16,
    pub cols: u16,
    /// `attachSeq`: the negotiation sequence of the last ATTACH/RESIZE.
    pub attach_seq: u64,
    /// `initialScreenGeneration`: bumped by every ATTACH/PEEK so an older
    /// pending cut is superseded.
    pub generation: u64,
    pub phase: Phase,
    pub attached: Option<AttachedClient>,
}

impl Client {
    pub fn new(tx: Sender<Out>, rows: u16, cols: u16) -> Client {
        Client {
            tx,
            role: Role::Command,
            rows,
            cols,
            attach_seq: 0,
            generation: 0,
            phase: Phase::Live,
            attached: None,
        }
    }

    pub fn send(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(Out::Bytes(bytes));
    }

    pub fn is_settling(&self) -> bool {
        matches!(self.phase, Phase::Settling { .. })
    }

    /// Node's `broadcastGeometry` set: attached (ever) or readonly.
    pub fn gets_geometry(&self) -> bool {
        self.attach_seq > 0 || self.role == Role::Readonly
    }

    /// Node's `negotiateSize` set: writable with a negotiation sequence.
    pub fn constrains_size(&self) -> bool {
        self.role == Role::Writable && self.attach_seq > 0
    }
}

fn attached_clients(clients: &BTreeMap<u64, Client>) -> Vec<&AttachedClient> {
    clients.values().filter_map(|client| client.attached.as_ref()).collect()
}

impl Daemon {
    /// A packet from client `id`.
    pub(crate) fn on_packet(&mut self, id: u64, packet: Packet) {
        match packet.type_ {
            MessageType::Attach => self.on_attach(id, &packet.payload),
            MessageType::Peek => self.on_peek(id, &packet.payload),
            MessageType::Data => self.on_data(id, &packet.payload),
            MessageType::Resize => self.on_resize(id, &packet.payload),
            MessageType::Detach => self.on_detach(id),
            MessageType::Status => self.on_status(id, &packet.payload),
            MessageType::AcceptedSocketOwnership => {
                self.on_accepted_socket_ownership(id, &packet.payload);
            }
            MessageType::LifecycleCas => self.on_lifecycle_cas(id, &packet.payload),
            _ => {}
        }
    }

    /// node: src/server.ts:931-996
    fn on_attach(&mut self, id: u64, payload: &[u8]) {
        if payload.len() < 4 {
            return;
        }
        if !self.clients.contains_key(&id) {
            return;
        }
        let (rows, cols) = decode_size(payload);
        self.adopt_cell_size(payload);
        // Read before negotiation: a smaller client shrinks the session to
        // its own size, which would then look like it had matched.
        let size_matched = rows == self.actor.rows() && cols == self.actor.cols();
        self.attach_counter += 1;
        let generation = {
            let c = self.clients.get_mut(&id).expect("checked");
            c.role = Role::Writable;
            c.rows = rows;
            c.cols = cols;
            c.attach_seq = self.attach_counter;
            c.generation += 1;
            let (pid, tty) = decode_attach_identity(payload);
            c.attached = Some(AttachedClient {
                pid,
                tty,
                attached_at: registry::now_iso8601(),
            });
            c.generation
        };
        let resized = self.negotiate_size();
        if !resized {
            let g = encode_geometry(self.actor.rows(), self.actor.cols());
            self.clients[&id].send(g);
        }
        // One write carries both the attach stamp and the client-generation
        // bump. Best-effort under the lock: a concurrent metadata command
        // wins, the write retries briefly, and neither writer can overwrite
        // the other's snapshot.
        self.note_client_change(Some(registry::now_iso8601()));
        let delay = if !self.exited {
            let since_last = self
                .last_resize
                .map(|t| t.elapsed())
                .unwrap_or(Duration::MAX);
            if resized {
                Some(self.settle)
            } else if since_last < self.settle {
                Some(self.settle - since_last)
            } else {
                None
            }
        } else {
            None
        };
        self.schedule_cut(id, generation, CutKind::Attach { size_matched }, delay);
    }

    /// node: src/server.ts:998-1020
    fn on_peek(&mut self, id: u64, payload: &[u8]) {
        if !self.clients.contains_key(&id) {
            return;
        }
        let (generation, was_writable) = {
            let c = self.clients.get_mut(&id).expect("checked");
            let was_writable = c.constrains_size();
            c.role = Role::Readonly;
            c.generation += 1;
            (c.generation, was_writable)
        };
        let resized = self.negotiate_size();
        if !resized {
            let g = encode_geometry(self.actor.rows(), self.actor.cols());
            self.clients[&id].send(g);
        }
        if was_writable {
            self.note_client_change(None);
        }
        let (plain, full) = decode_peek(payload);
        self.schedule_cut(id, generation, CutKind::Peek { plain, full }, None);
    }

    /// node: src/server.ts:1022-1027
    fn on_data(&mut self, id: u64, payload: &[u8]) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        if !self.exited && c.role != Role::Readonly {
            self.write_pty(payload);
        }
    }

    /// node: src/server.ts:1029-1038
    fn on_resize(&mut self, id: u64, payload: &[u8]) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        if c.role != Role::Writable || c.attach_seq == 0 || payload.len() < 4 {
            return;
        }
        let (rows, cols) = decode_size(payload);
        c.rows = rows;
        c.cols = cols;
        self.attach_counter += 1;
        c.attach_seq = self.attach_counter;
        self.adopt_cell_size(payload);
        self.negotiate_size();
        self.note_client_change(None);
    }

    /// Take the cell pixel metrics a client declared on ATTACH or RESIZE.
    ///
    /// Only a client knows how big a cell is — it comes from a font on the
    /// client's host, which this process may never see — and the session's
    /// terminal needs them to answer the cell extent of a kitty placement
    /// that did not name `c=`/`r=` itself. A payload without them (every
    /// older client, the Node one included) changes nothing, and the terminal
    /// keeps its deterministic fallback.
    ///
    /// The most recent declaration wins. Clients that draw cells of different
    /// sizes cannot all be right about an implicit placement, and unlike rows
    /// and cols there is nothing to negotiate: the metrics change no bytes and
    /// no client's screen, only what this session reports as derived
    /// geometry.
    fn adopt_cell_size(&mut self, payload: &[u8]) {
        if let Some((width, height)) = decode_cell(payload) {
            self.actor.set_cell_size(pty_terminal::CellSize {
                width: width as u32,
                height: height as u32,
            });
        }
    }

    /// node: src/server.ts:1040-1043
    /// A client asked to leave: close its socket and take it off the books
    /// at once.
    ///
    /// **Removing it here rather than when its socket close comes back is a
    /// deliberate difference from the Node tool**, which deletes the client
    /// in its `close` handler (`src/server.ts`). Both designs leave a window
    /// between a client observing its own socket close and the daemon
    /// recording it, and in that window `pty stats` counts a client that has
    /// already gone.
    ///
    /// The window is invisible on Linux — 0 stale readings in 60 attempts,
    /// for both implementations, measured 2026-09-02 — and wide enough on
    /// Apple silicon to fail a test that detaches and immediately reattaches.
    /// That fits the close-detection difference in docs/parity.md §12c: the
    /// reader thread there learns of a departure from an ordinary end of
    /// stream rather than a reset.
    ///
    /// There is nothing to wait for. The client has said it is leaving, and
    /// the size negotiation should stop counting it immediately for the same
    /// reason. `on_closed` still runs later and finds nothing to remove.
    ///
    /// The writer thread keeps its own end of the channel, so the `End` it
    /// was just sent still reaches it.
    fn on_detach(&mut self, id: u64) {
        if let Some(c) = self.clients.get(&id) {
            let _ = c.tx.send(Out::End);
        }
        self.on_closed(id);
    }

    /// node: src/server.ts:1045-1049
    fn on_status(&mut self, id: u64, payload: &[u8]) {
        let json = if payload == b"clients" {
            serde_json::to_string(&attached_clients(&self.clients)).unwrap_or_else(|_| "[]".into())
        } else {
            serde_json::to_string(&self.collect_stats()).unwrap_or_else(|_| "{}".into())
        };
        if let Some(c) = self.clients.get(&id) {
            c.send(encode_status_response(&json));
        }
    }

    /// `close` / `error`: forget the socket and renegotiate.
    ///
    /// Only a departing writable client can change the client facts, so a
    /// command connection (`pty stats`, `pty send`) or a peek closing costs
    /// no metadata work at all.
    ///
    /// node: src/server.ts:1054-1062
    pub(crate) fn on_closed(&mut self, id: u64) {
        if let Some(c) = self.clients.remove(&id) {
            self.negotiate_size();
            if c.constrains_size() {
                self.note_client_change(None);
            }
        }
    }

    /// The client facts as they stand now.
    fn client_facts(&self) -> ClientFacts {
        ClientFacts {
            rows: self.actor.rows(),
            cols: self.actor.cols(),
            writable: self
                .clients
                .iter()
                .filter(|(_, c)| c.constrains_size())
                .map(|(id, c)| (*id, c.rows, c.cols))
                .collect(),
        }
    }

    /// Bump `clientGeneration` and publish it when the client facts moved.
    /// An ATTACH (`attached_at` present) always bumps: it stamps
    /// `lastAttachAt` in the same write, and a client that re-attaches on
    /// its connection is a new attach even at an unchanged size.
    fn note_client_change(&mut self, attached_at: Option<String>) {
        let facts = self.client_facts();
        if attached_at.is_none() && facts == self.published_client_facts {
            return;
        }
        self.published_client_facts = facts;
        self.client_generation += 1;
        if attached_at.is_some() {
            self.pending_attach_at = attached_at;
        }
        self.client_meta_retry = None;
        self.write_client_generation();
    }

    /// Write the current `clientGeneration` (and a pending `lastAttachAt`)
    /// into the record, generation-fenced. A held lock or a concurrent
    /// rewrite schedules a retry for up to [`CLIENT_METADATA_RETRY`].
    pub(crate) fn write_client_generation(&mut self) {
        let client_generation = self.client_generation;
        let attached_at = self.pending_attach_at.clone();
        let status = registry::mutate_metadata_under_lock(
            &self.name,
            move |m| {
                let mut changed = false;
                if let Some(at) = attached_at {
                    m.last_attach_at = Some(at);
                    changed = true;
                }
                if m.client_generation != Some(client_generation) {
                    m.client_generation = Some(client_generation);
                    changed = true;
                }
                changed
            },
            &MutateOptions {
                expected_generation: Some(self.generation.clone()),
                expected_metadata: None,
            },
        );
        match status {
            MutateStatus::Busy | MutateStatus::Stale => {
                let now = Instant::now();
                let deadline = self
                    .client_meta_retry
                    .map_or(now + CLIENT_METADATA_RETRY, |(_, deadline)| deadline);
                self.client_meta_retry =
                    (now < deadline).then_some((now + Duration::from_millis(10), deadline));
            }
            MutateStatus::Changed(_)
            | MutateStatus::Unchanged(_)
            | MutateStatus::Missing
            | MutateStatus::GenerationMismatch => {
                self.pending_attach_at = None;
                self.client_meta_retry = None;
            }
        }
    }

    /// Arm (or perform, when `delay` is `None`) the SCREEN cut for `id`.
    fn schedule_cut(&mut self, id: u64, generation: u64, kind: CutKind, delay: Option<Duration>) {
        let deadline = Instant::now() + delay.unwrap_or(Duration::ZERO);
        if let Some(c) = self.clients.get_mut(&id) {
            c.phase = Phase::Settling {
                deadline,
                generation,
                kind,
            };
        }
        if delay.is_none() {
            self.cut(id);
        }
    }

    /// Node's `beginInitialScreenCut` callback: SCREEN from the live
    /// terminal, then live, then EXIT when the child is already gone, then
    /// the redraw nudge for an attacher whose size differed.
    ///
    /// node: src/server.ts:1213-1252
    pub(crate) fn cut(&mut self, id: u64) {
        let Some(c) = self.clients.get(&id) else {
            return;
        };
        let Phase::Settling {
            generation, kind, ..
        } = c.phase
        else {
            return;
        };
        if generation != c.generation {
            return;
        }
        let screen = match kind {
            CutKind::Attach { .. } => self.actor.serialize(SerializeOpts::ATTACH),
            CutKind::Peek { plain: true, full } => {
                self.actor
                    .plain(if full { Range::Full } else { Range::Viewport })
            }
            CutKind::Peek { plain: false, full } => self.actor.serialize(if full {
                SerializeOpts::PEEK_FULL
            } else {
                SerializeOpts::PEEK
            }),
        };
        let c = self.clients.get_mut(&id).expect("checked");
        c.send(encode_screen(screen.as_bytes()));
        c.phase = Phase::Live;
        if self.exited {
            c.send(encode_exit(self.exit_code));
        }
        if let CutKind::Attach { size_matched } = kind
            && !self.exited
            && !size_matched
        {
            self.nudge_redraw();
        }
    }

    /// Every settling client whose deadline has passed gets its cut.
    pub(crate) fn service_cuts(&mut self, now: Instant) {
        let due: Vec<u64> = self
            .clients
            .iter()
            .filter_map(|(id, c)| match c.phase {
                Phase::Settling { deadline, .. } if deadline <= now => Some(*id),
                _ => None,
            })
            .collect();
        for id in due {
            self.cut(id);
        }
    }

    /// The earliest pending cut deadline.
    pub(crate) fn next_cut_deadline(&self) -> Option<Instant> {
        self.clients
            .values()
            .filter_map(|c| match c.phase {
                Phase::Settling { deadline, .. } => Some(deadline),
                Phase::Live => None,
            })
            .min()
    }

    /// Node's `broadcast` for DATA and EXIT: live clients only; settling
    /// clients see the bytes in their SCREEN (and an EXIT after it).
    ///
    /// node: src/server.ts:1255-1267
    pub(crate) fn broadcast(&self, packet: &[u8]) {
        for c in self.clients.values() {
            if c.is_settling() {
                continue;
            }
            c.send(packet.to_vec());
        }
    }
}
