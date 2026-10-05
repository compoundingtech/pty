//! Help text for the `pty` binary.
//!
//! Every text here is vendored verbatim from the Node `pty` at `e6c1cd4`. The
//! fixtures under `crates/pty/tests/fixtures/help/` are the bytes that binary
//! printed (`pty help`, `pty <cmd> --help`, ...), captured by running it under a
//! scratch `PTY_ROOT`, trailing newline included. Nothing is generated or
//! reformatted here; a text changes only when its fixture is re-captured.
//! One edit since the capture: the `--remote` examples in `attach`, `list`,
//! `peek` and `send` name an invented peer, `example-host`.
//!
//! The two deferred commands (`recover`, `test`) keep the Node help so the
//! binary stays a drop-in; the commands themselves report that they are not
//! available in this build (see README.md and docs/parity.md §12).
//!
//! node: src/cli.ts:109-451 (`COMMAND_HELP`), src/cli.ts:470-478
//! (`printCommandHelp`), src/cli.ts:480-603 (`usage`),
//! src/cli.ts:3489-3511 (`printTagMultiHelp`).

use std::io::Write;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!("../../tests/fixtures/help/", $name, ".txt"))
    };
}

/// The top-level usage text, as printed by `pty help`, `pty --help`, `pty -h`
/// (and after `Unknown command: <cmd>`).
pub fn usage() -> &'static str {
    fixture!("usage")
}

/// The focused help for one command, as printed by `pty <cmd> --help`.
/// Resolves the aliases `a` → `attach`, `ls` → `list`, `remove` → `rm`.
/// `None` for anything without a `COMMAND_HELP` entry (`help`, `version`,
/// `completions`, `interactive`, unknown names); those callers print their own
/// text.
pub fn command_help(cmd: &str) -> Option<&'static str> {
    let canonical = match cmd {
        "a" => "attach",
        "ls" => "list",
        "remove" => "rm",
        other => other,
    };
    Some(match canonical {
        "run" => fixture!("run"),
        "attach" => fixture!("attach"),
        "exec" => fixture!("exec"),
        "peek" => fixture!("peek"),
        "send" => fixture!("send"),
        "events" => fixture!("events"),
        "list" => fixture!("list"),
        "stats" => fixture!("stats"),
        "restart" => fixture!("restart"),
        "kill" => fixture!("kill"),
        "recover" => fixture!("recover"),
        "rm" => fixture!("rm"),
        "gc" => fixture!("gc"),
        "tag" => fixture!("tag"),
        "tag-multi" => fixture!("tag-multi"),
        "emit" => fixture!("emit"),
        "rename" => fixture!("rename"),
        "metadata" => fixture!("metadata"),
        "readiness" => fixture!("readiness"),
        "evidence" => fixture!("evidence"),
        "up" => fixture!("up"),
        "down" => fixture!("down"),
        "test" => fixture!("test"),
        "remote-serve" => fixture!("remote-serve"),
        "root" => fixture!("root"),
        _ => return None,
    })
}

/// The help `tag-multi`'s own argument parser prints when `-h`/`--help` shows
/// up after other arguments (`pty tag-multi --all --help`). `pty tag-multi
/// --help` itself is intercepted before the parser and prints
/// [`command_help`]`("tag-multi")`, a different text.
pub fn tag_multi_parser_help() -> &'static str {
    fixture!("tag-multi-parser")
}

/// The leaf help `pty evidence snapshot --help` / `pty evidence remove --help`
/// print (`COMMAND_HELP.snapshot` / `.remove`, reachable only through
/// `evidence`'s own parser). `None` for any other leaf.
pub fn evidence_leaf_help(leaf: &str) -> Option<&'static str> {
    match leaf {
        "snapshot" => Some(fixture!("evidence-snapshot")),
        "remove" => Some(fixture!("evidence-remove")),
        _ => None,
    }
}
/// Leaf help for the two readiness operations.
pub fn readiness_leaf_help(leaf: &str) -> Option<&'static str> {
    match leaf {
        "ownership" => Some(fixture!("readiness-ownership")),
        "cas" => Some(fixture!("readiness-cas")),
        _ => None,
    }
}

/// Write [`usage`] to stdout. A closed pipe is not an error worth reporting
/// for help output, so write failures are ignored.
pub fn print_usage() {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(usage().as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// node: tests/help.test.ts:13-18 (`COMMANDS`).
    const COMMANDS: [&str; 24] = [
        "run",
        "attach",
        "exec",
        "peek",
        "send",
        "events",
        "list",
        "stats",
        "restart",
        "kill",
        "recover",
        "rm",
        "gc",
        "tag",
        "tag-multi",
        "emit",
        "rename",
        "metadata",
        "readiness",
        "up",
        "down",
        "test",
        "remote-serve",
        "evidence",
    ];

    /// node: tests/help.test.ts:38-50 — every command has focused help that
    /// opens with a usage synopsis and shows at least one `  pty ...` example.
    #[test]
    fn every_command_has_focused_help() {
        for cmd in COMMANDS {
            let help = command_help(cmd).unwrap_or_else(|| panic!("no help for {cmd}"));
            assert!(help.starts_with("Usage: pty "), "{cmd}: {help:?}");
            assert!(help.ends_with('\n'), "{cmd}: missing trailing newline");
            assert!(
                help.lines().any(|l| l.starts_with("  pty ")),
                "{cmd}: no example line"
            );
        }
    }

    /// node: tests/help.test.ts:20 (`ALIASES`), src/cli.ts:473.
    #[test]
    fn aliases_resolve_to_the_same_help() {
        assert_eq!(command_help("a"), command_help("attach"));
        assert_eq!(command_help("ls"), command_help("list"));
        assert_eq!(command_help("remove"), command_help("rm"));
    }

    /// node: tests/help.test.ts:24-27 (`NON_COMMAND_CASES`): the verbs without
    /// a `COMMAND_HELP` entry fall through to their own handling.
    #[test]
    fn non_commands_have_no_entry() {
        for cmd in [
            "interactive",
            "i",
            "help",
            "--help",
            "-h",
            "version",
            "--version",
            "-v",
            "-V",
            "completions",
            "snapshot",
            "",
            "nope",
        ] {
            assert!(command_help(cmd).is_none(), "{cmd:?} should have no help");
        }
    }

    /// node: tests/help.test.ts:100-112 — the top-level usage names every
    /// command.
    #[test]
    fn usage_lists_every_command() {
        let usage = usage();
        assert!(usage.starts_with("Usage:\n"));
        assert!(usage.ends_with('\n'));
        for cmd in COMMANDS {
            assert!(usage.contains(&format!("pty {cmd} ")), "usage lacks {cmd}");
        }
    }

    /// node: tests/help.test.ts:53-70 — the evidence leaves have their own help.
    #[test]
    fn evidence_leaves() {
        let snapshot = evidence_leaf_help("snapshot").unwrap();
        assert!(snapshot.starts_with("Usage: pty evidence snapshot "));
        assert!(snapshot.contains("--id <stable-id>"));
        assert!(!snapshot.contains("--expected-generation"));
        let remove = evidence_leaf_help("remove").unwrap();
        assert!(remove.starts_with("Usage: pty evidence remove "));
        assert!(remove.contains("--expected-generation <opaque>"));
        assert!(evidence_leaf_help("evidence").is_none());
    }
    #[test]
    fn readiness_leaves() {
        let ownership = readiness_leaf_help("ownership").unwrap();
        assert!(ownership.starts_with("Usage: pty readiness ownership "));
        let cas = readiness_leaf_help("cas").unwrap();
        assert!(cas.starts_with("Usage: pty readiness cas "));
        assert!(readiness_leaf_help("readiness").is_none());
    }

    /// node: src/cli.ts:3489-3511 — the parser's own text differs from the
    /// intercepted `COMMAND_HELP` entry.
    #[test]
    fn tag_multi_has_two_help_texts() {
        let parser = tag_multi_parser_help();
        assert!(parser.starts_with("Usage:\n  pty tag-multi <selector>"));
        assert_ne!(parser, command_help("tag-multi").unwrap());
    }

    /// node: tests/help.test.ts:73-88 — texts that other tests rely on.
    #[test]
    fn no_drift() {
        let send = command_help("send").unwrap();
        for needle in ["key:ctrl+c", "key:ctrl-c", "key:C-c", "_ separators"] {
            assert!(send.contains(needle), "send help lacks {needle:?}");
        }
        let run = command_help("run").unwrap();
        for needle in [
            "--env KEY=VALUE",
            "environment variable (repeatable)",
            "--unset-env KEY",
            "inherited environment variable (repeatable)",
        ] {
            assert!(run.contains(needle), "run help lacks {needle:?}");
        }
    }
}
