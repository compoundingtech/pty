//! Daemon-owned startup lease values and host-boot monotonic deadlines.
//!
//! node: src/startup-lease.ts

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Public startup lease options carried in `PTY_SERVER_CONFIG`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupLeaseOptions {
    /// `None` means the daemon owns the lifecycle generation without enforcing
    /// a startup deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub lifecycle_tag: String,
}

/// The generation-fenced lifecycle value published before the launcher returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmedStartupLease {
    pub generation: String,
    pub lifecycle_tag: String,
    pub boot_id: String,
    pub deadline_monotonic_ns: Option<u128>,
    pub starting_value: String,
}

/// Why a startup lease became terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupLeaseTerminalCause {
    Exit,
    Deadline,
    TeardownUnavailable,
}

impl StartupLeaseTerminalCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exit => "exit",
            Self::Deadline => "deadline",
            Self::TeardownUnavailable => "teardown-unavailable",
        }
    }
}

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;

/// Publish a daemon-owned startup generation, with an optional budget in the
/// current boot's monotonic domain.
pub fn arm_startup_lease(
    options: &StartupLeaseOptions,
    generation: &str,
) -> Result<ArmedStartupLease, String> {
    if options
        .timeout_ms
        .is_some_and(|timeout_ms| timeout_ms == 0 || timeout_ms > MAX_SAFE_INTEGER)
    {
        return Err("startup lease timeoutMs must be a positive safe integer".to_string());
    }
    if options.lifecycle_tag.is_empty() {
        return Err("startup lease lifecycleTag must not be empty".to_string());
    }
    let boot_id = read_boot_identity()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "startup lease boot identity is unavailable".to_string())?;
    let deadline_monotonic_ns = match options.timeout_ms {
        Some(timeout_ms) => {
            let now = monotonic_now_ns()
                .ok_or_else(|| "startup lease monotonic clock is unavailable".to_string())?;
            Some(now + u128::from(timeout_ms) * 1_000_000)
        }
        None => None,
    };
    let mut starting = serde_json::Map::new();
    starting.insert("_tag".to_string(), serde_json::Value::String("starting".to_string()));
    starting.insert(
        "generation".to_string(),
        serde_json::Value::String(generation.to_string()),
    );
    starting.insert(
        "bootId".to_string(),
        serde_json::Value::String(boot_id.clone()),
    );
    if let Some(deadline) = deadline_monotonic_ns {
        starting.insert(
            "deadlineMonotonicNs".to_string(),
            serde_json::Value::String(deadline.to_string()),
        );
    }
    let starting_value = serde_json::Value::Object(starting).to_string();
    Ok(ArmedStartupLease {
        generation: generation.to_string(),
        lifecycle_tag: options.lifecycle_tag.clone(),
        boot_id,
        deadline_monotonic_ns,
        starting_value,
    })
}

pub fn terminal_startup_lease_value(generation: &str, cause: StartupLeaseTerminalCause) -> String {
    serde_json::json!({
        "_tag": "terminal",
        "generation": generation,
        "cause": cause.as_str(),
    })
    .to_string()
}

pub fn startup_lease_deadline_cause(containment_complete: bool) -> StartupLeaseTerminalCause {
    if containment_complete {
        StartupLeaseTerminalCause::Deadline
    } else {
        StartupLeaseTerminalCause::TeardownUnavailable
    }
}

pub fn remaining_lease_delay(deadline_monotonic_ns: u128) -> Duration {
    let Some(now) = monotonic_now_ns() else {
        return Duration::ZERO;
    };
    let remaining_ns = deadline_monotonic_ns.saturating_sub(now);
    let remaining_ms = remaining_ns.div_ceil(1_000_000);
    Duration::from_millis(
        u64::try_from(remaining_ms)
            .unwrap_or(u64::MAX)
            .min(MAX_TIMER_DELAY_MS),
    )
}

pub fn monotonic_now_ns() -> Option<u128> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `value` points to writable storage for one `timespec`.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) } != 0 {
        return None;
    }
    let seconds = u128::try_from(value.tv_sec).ok()?;
    let nanos = u128::try_from(value.tv_nsec).ok()?;
    Some(seconds * 1_000_000_000 + nanos)
}

/// The host's current boot, shared with the launch-operation record.
pub fn read_boot_identity() -> Option<String> {
    pty_core::boot::read_boot_identity()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_values_match_node_json() {
        assert_eq!(
            terminal_startup_lease_value("generation", StartupLeaseTerminalCause::Deadline),
            r#"{"_tag":"terminal","generation":"generation","cause":"deadline"}"#
        );
        assert_eq!(
            startup_lease_deadline_cause(false),
            StartupLeaseTerminalCause::TeardownUnavailable
        );
    }

    #[test]
    fn untimed_starting_value_has_no_deadline() {
        let lease = arm_startup_lease(
            &StartupLeaseOptions {
                timeout_ms: None,
                lifecycle_tag: "run.lifecycle".to_string(),
            },
            "generation",
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&lease.starting_value).unwrap(),
            serde_json::json!({
                "_tag": "starting",
                "generation": "generation",
                "bootId": lease.boot_id,
            })
        );
        assert_eq!(lease.deadline_monotonic_ns, None);
    }
}
