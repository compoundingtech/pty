//! Launch operations: cancel-before-start launches for an owner that must be
//! able to prove a session never ran its command, or retire it, before it
//! counts the launch as decided.
//!
//! An owner reserves the operation (`pty operation reserve`), then launches
//! it (`pty run -d --operation-fd N --launch-operation <h>`). The daemon
//! publishes the session and prepares its child, but holds the child before
//! `exec` until the owner transfers the session. Until then every process
//! involved is one of pty's own, each carrying the birth marker
//! `--launch-operation <h>` in its argv, so a cancel or a crash at any point
//! can be settled to a positive answer on Linux and macOS alike.
//!
//! - [`envelope`]: the owner's exact declaration and its hash.
//! - [`record`]: `<root>/operations/<h>.json`, its lock, and process identity.
//! - [`frames`]: the control channel's frames.
//! - [`settle`]: occupancy, the birth-marker match, `settle` and `retire`.
//! - [`barrier`]: debug-build test barriers.
//!
//! The contract this implements is the pty producer contract for Smalltalk
//! #2489 slice 2, revision 4.

pub mod barrier;
pub mod envelope;
pub mod exact;
pub mod frames;
pub mod record;
pub mod settle;

pub use envelope::{
    ENVELOPE_MAX_BYTES, Envelope, EnvelopeError, is_digest, operation_digest, parse_envelope,
    sha256_hex,
};
pub use record::{
    AFTER_EXIT_CAP, Cause, Decision, Incarnation, Liveness, OperationLock, OperationRecord,
    OperationStore, Phase, ProcessId, RECORD_SCHEMA, Retirement, RetirementPhase, State, Transfer,
    Writer, WriterRole,
};

/// The capability `pty capabilities --json` advertises.
pub const CAPABILITY: &str = "launch-operation";
/// The protocol versions this build speaks.
pub const VERSIONS: [u64; 1] = [1];
