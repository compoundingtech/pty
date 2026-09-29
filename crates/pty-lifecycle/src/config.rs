//! The daemon's start-up configuration: the `PTY_SERVER_CONFIG` object of
//! Node's spawner, delivered on inherited fd 3 by [`super::launch`] (or in
//! the `PTY_SERVER_CONFIG` variable, which Node's tests and ours use).
//!
//! node: src/spawn.ts:169-184; src/server.ts:1468-1478, 1571-1591

use std::io::Read;

use crate::StartupLeaseOptions;
use pty_core::registry::{EnvMap, TagMap};
use serde::{Deserialize, Serialize};

/// The descriptor the spawner hands the config on.
pub const CONFIG_FD: i32 = 3;

/// The text Node prints when the config is missing or lacks `name`/`command`.
pub const CONFIG_REQUIRED: &str = "PTY_SERVER_CONFIG env var required";


/// `PTY_SERVER_CONFIG`, key for key. `generation` is absent from a spawner's
/// config (the daemon makes one) and present only when a restart wants to
/// keep the old token.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub display_command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<TagMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolate_env: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_env: Option<EnvMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_env: Option<EnvMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unset_env: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<EnvMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_lease: Option<StartupLeaseOptions>,
    /// Internal launch marker: append `session_respawn` in the daemon's
    /// serialized publication batch. Node spawners omit it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub respawn: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl DaemonConfig {
    /// Node's defaults: `args []`, `cwd process.cwd()`, `rows 24`, `cols 80`.
    ///
    /// node: src/server.ts:1571-1591
    pub fn cwd(&self) -> String {
        self.cwd.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".to_string())
        })
    }

    pub fn rows(&self) -> u16 {
        self.rows.unwrap_or(24)
    }

    pub fn cols(&self) -> u16 {
        self.cols.unwrap_or(80)
    }

    /// `tags` when non-empty.
    pub fn tags(&self) -> Option<&TagMap> {
        self.tags.as_ref().filter(|t| !t.is_empty())
    }

    /// `displayName` when truthy.
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref().filter(|d| !d.is_empty())
    }

    pub fn isolate_env(&self) -> bool {
        self.isolate_env == Some(true)
    }

    /// `extraEnv` when non-empty.
    pub fn extra_env(&self) -> Option<&EnvMap> {
        self.extra_env.as_ref().filter(|e| !e.is_empty())
    }

    /// `unsetEnv` when non-empty.
    pub fn unset_env(&self) -> &[String] {
        self.unset_env.as_deref().unwrap_or(&[])
    }

    /// Parse the JSON object; missing `name` or `command` is Node's
    /// `PTY_SERVER_CONFIG env var required`.
    pub fn parse(json: &str) -> Result<DaemonConfig, String> {
        let cfg: DaemonConfig =
            serde_json::from_str(json).map_err(|_| CONFIG_REQUIRED.to_string())?;
        if cfg.name.is_empty() || cfg.command.is_empty() {
            return Err(CONFIG_REQUIRED.to_string());
        }
        Ok(cfg)
    }

    /// The config for this daemon process: `PTY_SERVER_CONFIG` when set,
    /// else everything readable on fd 3.
    pub fn from_process() -> Result<DaemonConfig, String> {
        if let Ok(json) = std::env::var("PTY_SERVER_CONFIG") {
            return DaemonConfig::parse(&json);
        }
        let json = read_fd(CONFIG_FD).ok_or_else(|| CONFIG_REQUIRED.to_string())?;
        DaemonConfig::parse(&json)
    }
}

/// Read a descriptor to EOF, `None` when it is not open or not readable.
fn read_fd(fd: i32) -> Option<String> {
    use std::os::unix::io::FromRawFd;
    // SAFETY: `fstat` only inspects the descriptor.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return None;
    }
    // SAFETY: the descriptor is open (fstat succeeded) and this is the one
    // owner that closes it.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_spawner_shape_with_defaults() {
        let cfg = DaemonConfig::parse(
            r#"{"name":"a","command":"/bin/sh","args":["-c","true"],"displayCommand":"sh -c true","cwd":"/tmp","rows":24,"cols":80,"ephemeral":false}"#,
        )
        .unwrap();
        assert_eq!(cfg.name, "a");
        assert_eq!(cfg.args, vec!["-c", "true"]);
        assert_eq!(cfg.cwd(), "/tmp");
        assert!(cfg.tags().is_none());
        assert!(cfg.generation.is_none());
        let minimal = DaemonConfig::parse(r#"{"name":"b","command":"sleep"}"#).unwrap();
        assert_eq!(minimal.rows(), 24);
        assert_eq!(minimal.cols(), 80);
        assert!(!minimal.ephemeral);
        assert_eq!(minimal.unset_env(), &[] as &[String]);
    }

    #[test]
    fn rejects_missing_name_or_command() {
        assert_eq!(DaemonConfig::parse("{}").unwrap_err(), CONFIG_REQUIRED);
        assert_eq!(
            DaemonConfig::parse(r#"{"name":"x"}"#).unwrap_err(),
            CONFIG_REQUIRED
        );
        assert_eq!(
            DaemonConfig::parse("not json").unwrap_err(),
            CONFIG_REQUIRED
        );
    }
}
