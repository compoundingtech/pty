//! The session child's environment and working-directory checks.
//!
//! node: src/server.ts:131-209 (`buildChildEnv`), 236-260 (`describeInvalidCwd`)

use std::collections::BTreeMap;

use pty_lifecycle::{DaemonConfig, READY_FD_ENV};

/// What an isolated child keeps of the daemon's environment (plus `LC_*`).
///
/// node: src/server.ts:134-140
pub const ISOLATED_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "COLORTERM",
    "LANG",
    "TZ",
    "PWD",
    "TMPDIR",
    "PTY_ROOT",
    "PTY_SESSION_DIR",
];

/// The `TERM` a child gets when none was inherited.
pub const DEFAULT_CHILD_TERM: &str = "xterm-256color";

/// Values that identify the terminal which launched the daemon. A session
/// may later be attached from another terminal, so inherited copies become
/// stale. Explicit `env` and `extraEnv` remain under the caller's control.
const HOST_TERMINAL_IDENTITY: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "LC_TERMINAL",
    "ITERM_SESSION_ID",
    "TERM_SESSION_ID",
    "WT_SESSION",
    "Q_TERM",
    "IRIS_FD",
];

/// The text Node throws when `env` is combined with the inherited-policy
/// options.
///
/// node: src/server.ts:163-168
pub fn env_exclusive_error() -> String {
    "ServerOptions.env is mutually exclusive with isolateEnv/extraEnv/unsetEnv. \
     Use env for verbatim control, or inherited environment policy options — not both."
        .to_string()
}

fn ensure_child_term(env: &mut BTreeMap<String, String>) {
    if env.get("TERM").map(|t| t.is_empty()).unwrap_or(true) {
        env.insert("TERM".to_string(), DEFAULT_CHILD_TERM.to_string());
    }
}

/// [`build_child_env`] over an explicit parent environment.
pub fn build_child_env_from(
    cfg: &DaemonConfig,
    generation: &str,
    source: &[(String, String)],
) -> Result<BTreeMap<String, String>, String> {
    if cfg.env.is_some()
        && (cfg.isolate_env() || cfg.extra_env.is_some() || !cfg.unset_env().is_empty())
    {
        return Err(env_exclusive_error());
    }
    let mut env: BTreeMap<String, String> = if let Some(replacement) = &cfg.env {
        replacement
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    } else if !cfg.isolate_env() {
        let mut env: BTreeMap<String, String> = source.iter().cloned().collect();
        env.remove("PTY_SERVER_CONFIG");
        env.remove(READY_FD_ENV);
        env
    } else {
        source
            .iter()
            .filter(|(k, _)| ISOLATED_ENV_ALLOWLIST.contains(&k.as_str()) || k.starts_with("LC_"))
            .cloned()
            .collect()
    };
    if cfg.env.is_none() {
        for key in HOST_TERMINAL_IDENTITY {
            env.remove(*key);
        }
        for key in cfg.unset_env() {
            env.remove(key);
        }
        if let Some(extra) = &cfg.extra_env {
            for (k, v) in extra {
                env.insert(k.clone(), v.clone());
            }
        }
    }
    // `PWD` is the working directory as the caller WROTE it, not as the
    // kernel resolves it. node-pty does exactly this
    // (`src/unixTerminal.ts`, `env.PWD = cwd`) and the difference is
    // visible wherever the path runs through a symlink.
    //
    // Without it the child's shell derives `PWD` from `getcwd()` whenever
    // the inherited one does not name the same directory — so the value
    // depended on WHERE THE LAUNCHER STOOD. `pty run --cwd X` from inside X
    // gave the path as written, and the same command from anywhere else
    // gave the resolved one. Same arguments, two answers.
    //
    // On Linux this hides, because the usual temp directories are real
    // directories. On a Mac `/tmp` and `/var` are both symlinks, so every
    // session under either showed it. Reproduced on Linux with a symlinked
    // directory on 2026-09-02; it is not a platform difference, it is a
    // symlink difference.
    env.insert("PWD".to_string(), cfg.cwd());
    env.insert("PTY_SESSION".to_string(), cfg.name.clone());
    if !generation.is_empty() {
        env.insert("PTY_SESSION_GENERATION".to_string(), generation.to_string());
    }
    ensure_child_term(&mut env);
    Ok(env)
}

/// Node's `buildChildEnv`: a verbatim replacement, the daemon's own
/// environment, or the isolated allow-list; then `unsetEnv`, then
/// `extraEnv`, then the forced `PTY_SESSION` / `PTY_SESSION_GENERATION`
/// and the `TERM` default.
///
/// node: src/server.ts:131-209
pub fn build_child_env(
    cfg: &DaemonConfig,
    generation: &str,
) -> Result<BTreeMap<String, String>, String> {
    let source: Vec<(String, String)> = std::env::vars().collect();
    build_child_env_from(cfg, generation, &source)
}

fn errno_detail(err: &std::io::Error, syscall: &str, path: &str) -> String {
    match err.raw_os_error().and_then(pty_client::errno_name) {
        Some(code) => {
            let desc = err.to_string();
            let desc = desc
                .split(" (os error")
                .next()
                .unwrap_or(&desc)
                .to_lowercase();
            format!("{code}: {desc}, {syscall} '{path}'")
        }
        None => err.to_string(),
    }
}

/// Node's `describeInvalidCwd`: `None` when usable, else one of the five
/// texts.
///
/// node: src/server.ts:236-260
pub fn describe_invalid_cwd(cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return Some("Working directory is empty.".to_string());
    }
    let meta = match std::fs::metadata(cwd) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(format!("Working directory does not exist: {cwd}"));
        }
        Err(e) => {
            return Some(format!(
                "Working directory is not accessible: {cwd} ({})",
                errno_detail(&e, "stat", cwd)
            ));
        }
    };
    if !meta.is_dir() {
        return Some(format!("Working directory is not a directory: {cwd}"));
    }
    let searchable = std::ffi::CString::new(cwd)
        .map(|c| unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 })
        .unwrap_or(false);
    if !searchable {
        return Some(format!("Working directory is not searchable: {cwd}"));
    }
    None
}

/// The whole error a bad `cwd` aborts the daemon with.
///
/// node: src/server.ts:524-528
pub fn invalid_cwd_error(reason: &str, name: &str, command: &str) -> String {
    format!("{reason}\nCannot start session \"{name}\" for command \"{command}\".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pty_core::registry::EnvMap;

    fn cfg() -> DaemonConfig {
        DaemonConfig {
            name: "sess".into(),
            command: "/bin/sh".into(),
            ..Default::default()
        }
    }

    fn src(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn inherited_terminal_identity_does_not_follow_a_detached_session() {
        let source = src(&[
            ("HOME", "/h"),
            ("TERM_PROGRAM", "old-terminal"),
            ("TERM_PROGRAM_VERSION", "1.0"),
            ("LC_TERMINAL", "old-terminal"),
            ("ITERM_SESSION_ID", "old-session"),
            ("TERM_SESSION_ID", "old-session"),
            ("WT_SESSION", "old-session"),
            ("Q_TERM", "old-terminal"),
            ("IRIS_FD", "13"),
        ]);
        for isolated in [false, true] {
            let mut config = cfg();
            config.isolate_env = Some(isolated);
            let env = build_child_env_from(&config, "g", &source).unwrap();
            assert_eq!(env.get("HOME").map(String::as_str), Some("/h"));
            for key in [
                "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "LC_TERMINAL",
                "ITERM_SESSION_ID", "TERM_SESSION_ID", "WT_SESSION", "Q_TERM", "IRIS_FD",
            ] {
                assert!(!env.contains_key(key), "{key} leaked with isolate_env={isolated}");
            }
        }
    }

    /// node: tests/restart-launch-parity.test.ts:106-189
    #[test]
    fn inherited_unset_then_extra_then_forced_keys() {
        let mut c = cfg();
        c.unset_env = Some(vec!["NO_COLOR".into(), "ASSIGNMENT_WINS".into()]);
        let mut extra = EnvMap::new();
        extra.insert("ASSIGNMENT_WINS".into(), "explicit".into());
        extra.insert("PTY_SESSION".into(), "spoofed".into());
        c.extra_env = Some(extra);
        let env = build_child_env_from(
            &c,
            "gen1",
            &src(&[
                ("NO_COLOR", "1"),
                ("PTY_SERVER_CONFIG", "{}"),
                ("HOME", "/h"),
                ("SECRET", "s"),
            ]),
        )
        .unwrap();
        assert_eq!(env.get("ASSIGNMENT_WINS").unwrap(), "explicit");
        assert!(!env.contains_key("NO_COLOR"));
        assert!(!env.contains_key("PTY_SERVER_CONFIG"));
        assert_eq!(env.get("PTY_SESSION").unwrap(), "sess");
        assert_eq!(env.get("PTY_SESSION_GENERATION").unwrap(), "gen1");
        assert_eq!(env.get("TERM").unwrap(), DEFAULT_CHILD_TERM);
        assert_eq!(env.get("SECRET").unwrap(), "s");
    }

    #[test]
    fn isolated_keeps_only_the_allow_list_and_lc_star() {
        let mut c = cfg();
        c.isolate_env = Some(true);
        let env = build_child_env_from(
            &c,
            "g",
            &src(&[
                ("PATH", "/bin"),
                ("LC_ALL", "C"),
                ("SECRET", "s"),
                ("TERM", "xterm-kitty"),
            ]),
        )
        .unwrap();
        assert_eq!(env.get("PATH").unwrap(), "/bin");
        assert_eq!(env.get("LC_ALL").unwrap(), "C");
        assert_eq!(env.get("TERM").unwrap(), "xterm-kitty");
        assert!(!env.contains_key("SECRET"));
    }

    #[test]
    fn replacement_env_is_verbatim_plus_forced_keys() {
        let mut c = cfg();
        let mut env = EnvMap::new();
        env.insert("ONLY".into(), "this".into());
        env.insert("TERM".into(), "".into());
        c.env = Some(env);
        let out = build_child_env_from(&c, "g", &src(&[("HOME", "/h")])).unwrap();
        assert_eq!(out.get("ONLY").unwrap(), "this");
        assert!(!out.contains_key("HOME"));
        assert_eq!(out.get("TERM").unwrap(), DEFAULT_CHILD_TERM);
        assert_eq!(out.get("PTY_SESSION").unwrap(), "sess");
    }

    #[test]
    fn replacement_env_excludes_policy_options() {
        let mut c = cfg();
        c.env = Some(EnvMap::new());
        c.unset_env = Some(vec!["X".into()]);
        assert_eq!(
            build_child_env_from(&c, "g", &[]).unwrap_err(),
            env_exclusive_error()
        );
    }

    #[test]
    fn invalid_cwd_texts() {
        assert_eq!(
            describe_invalid_cwd("").unwrap(),
            "Working directory is empty."
        );
        assert_eq!(
            describe_invalid_cwd("/no/such/dir").unwrap(),
            "Working directory does not exist: /no/such/dir"
        );
        assert_eq!(
            describe_invalid_cwd("/bin/sh").unwrap(),
            "Working directory is not a directory: /bin/sh"
        );
        assert_eq!(describe_invalid_cwd("/tmp"), None);
        assert_eq!(
            invalid_cwd_error("Working directory is empty.", "s", "/bin/sh"),
            "Working directory is empty.\nCannot start session \"s\" for command \"/bin/sh\"."
        );
    }
}
