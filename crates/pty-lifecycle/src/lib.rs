//! Daemon launch and registry lifecycle operations for `pty` sessions.
//!
//! This crate contains the importable, terminal-free lifecycle surface shared
//! by the `pty` binary and library callers. Callers choose the executable that
//! implements the private `__daemon` entrypoint when spawning.

mod config;
mod gc;
#[cfg(all(unix, any(target_os = "macos", test)))]
mod image_ancestry;
mod launch;
mod startup;

pub use config::DaemonConfig;
pub use gc::{
    Abandoned, Flapped, GcOptions, GcResult, OrphanKill, PrunedTags, ReapSkip, RespawnFailed,
    Respawned, cleanup_dead_registry_temps, gc, prune_orphan_layout_tags,
};
pub use launch::{
    DEFAULT_START_TIMEOUT, GATE_FD_ENV, GatedDaemon, OPERATION_FD_ENV, OperationLaunch,
    READY_FD_ENV, ReadyNotifier, SpawnError, SpawnParams, SpawnedDaemon,
    PreparedDaemon, apply_persisted_launch_options, config_for, prepare_daemon, set_process_title,
    spawn_daemon, spawn_gated_daemon,
};
pub use pty_core::registry::{
    LockBusy, SessionGenerationOwner, cleanup_all, cleanup_owned_all, cleanup_owned_socket,
    cleanup_socket, wait_for_process_exit,
};
pub use startup::{
    ArmedStartupLease, StartupLeaseOptions, StartupLeaseTerminalCause, arm_startup_lease,
    monotonic_now_ns, read_boot_identity, remaining_lease_delay, startup_lease_deadline_cause,
    terminal_startup_lease_value,
};
