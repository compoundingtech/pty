// Implementation of the `conformance-map` bin: reads the `/// node:` doc
// comments in `tests/*.rs`, joins them with the fixed classification of
// every Node suite below, and writes `docs/conformance.md`.
//
// ANYTHING THIS PRINTS AS FACT MUST BE READ FROM THE THING, NOT TYPED BESIDE
// IT.
//
// That rule was earned three times over, by this file, in this file:
//
//   1. The Node checkout defaulted to an absolute path on one developer's
//      machine, so running it elsewhere silently described a different tree.
//   2. With no checkout, `node_suites` fell back to the compiled-in `SUITES`
//      list, so it would write a complete-looking document from no source.
//   3. The header named the checkout as "0.12.0+500eab2" from a string
//      literal. It was true the day it was typed and wrong every day after,
//      and regenerating could not fix it, because a constant cannot follow the
//      thing it describes.
//
// The third is the worst of the three and the hardest to notice. The first two
// produce a wrong document; the third produces a document that is right about
// everything except what it was made from, which is the one claim a reader
// uses to decide whether to trust the rest.
//
// A GENERATED DOCUMENT IS WHERE AN UNMEASURED CLAIM SURVIVES LONGEST, BECAUSE
// NOBODY RE-READS WHAT A MACHINE WROTE. A human writing the same sentence by
// hand would eventually notice it had gone stale. Nothing here will.
//
// So: derive it, or do not print it. If a value cannot be read from the input,
// the generator should refuse rather than assert — which is what it now does
// when `PTY_NODE_CHECKOUT` is unset, and what it does when the tests reference
// a suite the checkout does not have.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How a Node suite is covered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Black-box CLI tests (`rig.pty`, `rig.pty_tty`, `rig.daemon`).
    Cli,
    /// Socket-level tests (`rig.daemon` + `rig.connect`).
    Protocol,
    /// Pure-logic tests already ported as crate unit tests.
    Unit,
    /// Cannot run against a binary; listed with the reason.
    NotPortable,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Cli => "cli",
            Kind::Protocol => "protocol",
            Kind::Unit => "unit",
            Kind::NotPortable => "not-portable",
        }
    }
}

const TUI: &str = "TUI widget / framework test against the Node in-process renderer";

/// Every Node suite (tests/*.test.ts in the Node checkout), its kind, and a
/// note: the reason for not-portable, or what the port covers.
const SUITES: &[(&str, Kind, &str)] = &[
    ("accordion", Kind::NotPortable, TUI),
    ("action-list-item", Kind::NotPortable, TUI),
    ("atomic-writes", Kind::Cli, "racing pty tag / emit / metadata patch writers and the lock files; the atomicWriteFileSync and appendEvent queueing loops are library-only"),
    ("attach-no-restart", Kind::Cli, ""),
    ("attach-stream", Kind::Cli, "fd 3 via a pipe; fake daemons behind a UnixListener; the reconnect cases (:495, :563) need attach --remote and are left with the remote suites"),
    ("attach-window", Kind::Cli, "attached child observes the published owner sidecar and immediately patches metadata"),
    ("badge", Kind::NotPortable, TUI),
    ("breadcrumbs", Kind::NotPortable, TUI),
    ("buffer-palette", Kind::NotPortable, TUI),
    ("buffer-wide-char-diff", Kind::NotPortable, TUI),
    ("code-block", Kind::NotPortable, TUI),
    ("codex-integration", Kind::NotPortable, "drives the Node codex integration in-process"),
    ("completions", Kind::Cli, "shell-syntax cases run where the shell exists; the evidence completions (:103, :129, :150) and the in-process spec cases (:54, :64) are left out (evidence deferred)"),
    ("connection", Kind::Protocol, "SessionConnection semantics as raw-socket sequences; paste brackets via pty send --paste"),
    ("disk-layout-docs", Kind::NotPortable, "lint of the Node docs against the Node source"),
    ("display-name", Kind::Cli, ""),
    ("duration", Kind::Unit, "pty-core duration parser unit tests"),
    ("effective-geometry", Kind::Protocol, "GEOMETRY precedes SCREEN/DATA; the SessionConnection, testing-lib and attachPty variants collapse into raw-socket sequences"),
    ("env-isolation", Kind::Unit, "build_spawn_env unit tests in pty-testkit; the CLI half is in spawn_options.rs"),
    ("events-emit", Kind::Cli, "CLI half; emitUserEvent / retention / EventFollower are library-only"),
    ("events", Kind::Cli, "CLI half; EventWriter retention, readRecentEvents, clearEvents and EventFollower cases are library-only"),
    ("exec", Kind::Cli, ""),
    ("exit-event-race", Kind::Protocol, ""),
    ("exit-reap", Kind::Cli, "reap-policy half; the exact-generation evidence half (pty evidence) is deferred"),
    ("exit-signal", Kind::Cli, "sessions tagged keep=true so the exit record outlives the default reap"),
    ("filter", Kind::NotPortable, TUI),
    ("focus", Kind::NotPortable, TUI),
    ("gc-abandoned", Kind::NotPortable, "dropped in docs/parity.md §12 (gc abandoned reap)"),
    ("gc-flap-clear-badge-root-len", Kind::Cli, "root-length half in pty_root.rs; badge and restart bookkeeping-strip halves in gc_badge.rs; the flapping machinery itself is dropped in docs/parity.md §12"),
    ("gc-flapping", Kind::NotPortable, "dropped in docs/parity.md §12 (gc flapping classifier)"),
    ("gc-generation-guard", Kind::NotPortable, "dropped in docs/parity.md §12 (gc permanent respawn)"),
    ("gc-keep-expiry", Kind::Cli, "the keep-tag retention window in the sweep (Node PR #173)"),
    ("gc-parent-child", Kind::Cli, "the library reapSkipped case (:92) is left out"),
    ("gc-permanent", Kind::NotPortable, "dropped in docs/parity.md §12 (gc permanent respawn)"),
    ("gc", Kind::Cli, "debris, orphan tag prune, dry-run, launchd plist (with the plist half of pty-root.test.ts)"),
    ("help", Kind::Cli, "top-level usage only; per-command help belongs to the help work package"),
    ("hit-test", Kind::NotPortable, TUI),
    ("input-parse", Kind::Unit, "pty-core input parser unit tests"),
    ("integration", Kind::Protocol, "sync ordering, roles, malformed packets, geometry via stty, kitty replay, send, peek, stats; :618 injects an EXIT ahead of DATA through server internals and has no socket-level counterpart"),
    ("keys", Kind::Unit, "pty-core key resolver unit tests"),
    ("kill-releases-socket-command", Kind::Cli, "bin/pty-kill-releases-socket-test rewritten as a Rust test; the second binary is dropped in docs/parity.md §12"),
    ("kill-report", Kind::Unit, "the report is pure logic over a process table; ported as pty-client's stop.rs unit tests, which is where the survivor and escalation cases live"),
    ("kill-wait", Kind::Cli, ""),
    ("list-filters", Kind::Cli, ""),
    ("list-live-session-race", Kind::Cli, ""),
    ("list-liveness-budget", Kind::Cli, "100 unreachable sockets listed in under 3 s; EPERM counts as alive"),
    ("list-purity", Kind::Cli, "CLI half; the raw-candidate revalidation case is library-only"),
    ("message", Kind::NotPortable, TUI),
    ("metadata-events", Kind::Cli, "CLI half; patchMetadataById / EventFollower cases are library-only"),
    ("mouse-parse", Kind::Unit, "pty-core mouse parser unit tests"),
    ("nesting-prevention", Kind::Cli, ""),
    ("nesting", Kind::Cli, ""),
    ("panel-footer-title", Kind::NotPortable, TUI),
    ("output-activity", Kind::Cli, ""),
    ("parity-fixtures", Kind::Cli, "shared screens.json loader"),
    ("parity-node-reference", Kind::Cli, "resolveSeqDelayMs rounding (:238, :243) is a pty-core unit test"),
    ("parity-shapes", Kind::Cli, "shared shapes.json loader"),
    ("peek-wait", Kind::Cli, ""),
    ("proc-table", Kind::Unit, "reading the process table is pure logic; ported as pty-core/src/proctable.rs unit tests, including the truncation guard and the layout self-check"),
    ("process-groups", Kind::Unit, "group membership over an injected table; ported as pty-core's process_tree.rs unit tests, which also cover the zombie the sweep must not count"),
    ("process-title", Kind::Cli, "Linux /proc comm"),
    ("process-tree", Kind::Cli, "only the TERM-then-KILL contract (:55) is observable; :10 and :33 are unit tests with injected process lists"),
    ("progress-bars", Kind::NotPortable, TUI),
    ("protocol", Kind::Unit, "pty-core framing unit tests; the socket-level limits are in fixtures_protocol.rs"),
    ("pty-handle", Kind::NotPortable, TUI),
    ("pty-pane", Kind::NotPortable, TUI),
    ("pty-root", Kind::Cli, "root precedence, notices, --root; the gc plist half is in gc.rs"),
    ("ptyfile", Kind::Unit, "pty-core manifest parser unit tests; the CLI half is up_down.rs"),
    ("ratatui-compat", Kind::Protocol, "every case re-expressed as SCREEN payload checks across a reconnect"),
    ("recovery", Kind::NotPortable, "deferred: pty recover is absent (docs/parity.md §12)"),
    ("remote-exec-bridge", Kind::Cli, "stub fabric on PATH bridging to remote-serve --stdio"),
    ("remote-fabric", Kind::Cli, "stub fabric on PATH; the remote-serve --socket cases (:126, :157) are dropped in docs/parity.md §12"),
    ("remote-reconnect", Kind::Cli, "stub fabric whose tunnels the test can sever and block"),
    ("resize-tui", Kind::NotPortable, "drives the Node TUI through the Node testing Session"),
    ("restart-env-scrub", Kind::Cli, ""),
    ("restart-guardrail", Kind::Cli, ""),
    ("restart-launch-parity", Kind::Cli, ""),
    ("rm-immediate-reuse", Kind::Cli, "CLI half; the cleanupOwned* case is library-only"),
    ("rm-kill-ephemeral", Kind::Cli, ""),
    ("sanitize", Kind::Protocol, "the reset sequence and trailers attach and peek emit in a tty; the xterm-behaviour half of each case belongs to the emulator"),
    ("screen-replay-altscreen", Kind::Protocol, ""),
    ("screenshot", Kind::NotPortable, "in-process xterm screenshot of the Node testing library"),
    ("scrollback-fidelity", Kind::Protocol, "buffer text observed through SCREEN/DATA, peek --plain and the STATUS cursor"),
    ("session-trailers", Kind::Unit, "trailer rendering unit tests; the Rust counterpart is the pty-client summary unit tests, and sanitize.rs covers the trailers end to end"),
    ("security-fixes", Kind::Cli, "name validation and lock steal through pty run -d --id; releaseLock idempotence (:73) has no CLI counterpart"),
    ("select", Kind::NotPortable, TUI),
    ("send-paste", Kind::Cli, ""),
    ("seq-delay", Kind::Cli, "end-to-end half; resolveSeqDelayMs is a pty-core unit test"),
    ("shells", Kind::NotPortable, "shell integration through the Node testing Session (pty-testkit covers it)"),
    ("shutdown-backstop", Kind::Cli, ""),
    ("spawn-bundle-fallback", Kind::Cli, "only the run -d argv shape; spawn-strategy resolution (:59, :116, :139) is inside the Node library"),
    ("spawn-options", Kind::Cli, "CLI half; spawnDaemon(...) library cases stay in Node"),
    ("spawner-pid-watchdog", Kind::Cli, ""),
    ("spawn-already-published", Kind::Unit, "is_published_by over an injected liveness predicate; ported as daemon/launch.rs unit tests, including the zombie case against a real corpse rather than a mock"),
    ("stats-cli", Kind::Cli, ""),
    ("tag-bulk", Kind::Cli, ""),
    ("tag-multi", Kind::Cli, ""),
    ("tag-mutate", Kind::Cli, ""),
    ("tags-helpers", Kind::Cli, "CLI half; the pure functions are pty-core unit tests"),
    ("tags", Kind::Cli, ""),
    ("terminal-queries", Kind::Protocol, "responses through an echoing child plus end-to-end stripping; the stripTerminalQueries pure cases are a pty-core unit test"),
    ("tokens", Kind::NotPortable, TUI),
    ("tui-framework", Kind::NotPortable, TUI),
    ("tui", Kind::NotPortable, "drives the Node session manager TUI"),
    ("up-down", Kind::Cli, ""),
    ("up-name-decouple", Kind::Cli, ""),
    ("version", Kind::Cli, "CLI half; formatVersion is a unit test"),
    ("widgets-bar-chart", Kind::NotPortable, TUI),
    ("widgets-command-palette", Kind::NotPortable, TUI),
    ("widgets-command-registry", Kind::NotPortable, TUI),
    ("widgets-confirm", Kind::NotPortable, TUI),
    ("widgets-date-picker", Kind::NotPortable, TUI),
    ("widgets-form", Kind::NotPortable, TUI),
    ("widgets-help-overlay", Kind::NotPortable, TUI),
    ("widgets-markdown", Kind::NotPortable, TUI),
    ("widgets-prompt-bar", Kind::NotPortable, TUI),
    ("widgets-sparkline", Kind::NotPortable, TUI),
    ("widgets-stream-view", Kind::NotPortable, TUI),
    ("widgets-table", Kind::NotPortable, TUI),
    ("widgets-tabs-mouse", Kind::NotPortable, TUI),
    ("widgets-tabs", Kind::NotPortable, TUI),
    ("widgets-text-area", Kind::NotPortable, TUI),
    ("widgets-toast", Kind::NotPortable, TUI),
    ("widgets-toolbar", Kind::NotPortable, TUI),
    ("widgets-tree", Kind::NotPortable, TUI),
    ("widgets-virtual-list-mouse", Kind::NotPortable, TUI),
    ("widgets-virtual-list", Kind::NotPortable, TUI),
    ("wrapper-signal-forwarding", Kind::Cli, "re-expressed: a long-lived CLI has no wrapper child and dies on SIGTERM; remote-serve --stdio answers one request; the --socket form is dropped in docs/parity.md §12"),
];

/// One `#[test]` with its `/// node:` reference.
struct Mapped {
    rust_file: String,
    test_name: String,
    /// `None` for a test with no Node counterpart (the Rust-owned fixtures).
    node_suite: Option<String>,
    node_line: Option<u32>,
    gated: bool,
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Parse `tests/*.rs` for `/// node: tests/<suite>.test.ts[:<line>]`
/// comments attached to `#[test]` functions. A test is gated when its name
/// ends in `_node` or `_rust`.
fn scan_tests(tests_dir: &Path) -> Result<Vec<Mapped>, String> {
    let mut out = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(tests_dir)
        .map_err(|e| format!("{}: {e}", tests_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "rs").unwrap_or(false))
        .collect();
    entries.sort();
    for path in entries {
        let rust_file = path.file_name().unwrap().to_string_lossy().into_owned();
        let src = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut pending: Option<(String, Option<u32>)> = None;
        let mut saw_test_attr = false;
        for line in src.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("/// node:") {
                let rest = rest.trim();
                if let Some(spec) = rest.strip_prefix("tests/") {
                    let (file, line_no) = match spec.split_once(".test.ts") {
                        Some((f, tail)) => (
                            f.to_string(),
                            tail.strip_prefix(':').and_then(|n| n.trim().parse().ok()),
                        ),
                        None => continue,
                    };
                    pending = Some((file, line_no));
                }
                continue;
            }
            if t == "#[test]" {
                saw_test_attr = true;
                continue;
            }
            if saw_test_attr && let Some(name) = t.strip_prefix("fn ") {
                let name = name.split('(').next().unwrap_or("").trim().to_string();
                let gated = name.ends_with("_node") || name.ends_with("_rust");
                let (node_suite, node_line) = match pending.take() {
                    Some((suite, line_no)) => (Some(suite), line_no),
                    None => (None, None),
                };
                out.push(Mapped {
                    rust_file: rust_file.clone(),
                    test_name: name,
                    node_suite,
                    node_line,
                    gated,
                });
                saw_test_attr = false;
                continue;
            }
            if !t.starts_with("///") && !t.starts_with("#[") && !t.is_empty() {
                // Anything else between the comment and the fn resets.
                if !saw_test_attr {
                    pending = None;
                }
            }
        }
    }
    Ok(out)
}

/// Node suites present in the checkout (or the fixed list when unavailable).
/// How to name the Node checkout this map was generated from: its package
/// version and the commit it is on, both read from the checkout itself.
fn node_reference(dir: &Path) -> String {
    let version = std::fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|s| {
            s.split(r#""version""#).nth(1)?.split('"').nth(1).map(str::to_string)
        })
        .unwrap_or_else(|| "unknown-version".to_string());
    let sha = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-commit".to_string());
    format!("{version}+{sha}")
}

fn node_suites(checkout: Option<&Path>) -> Vec<String> {
    if let Some(dir) = checkout
        && let Ok(rd) = std::fs::read_dir(dir.join("tests"))
    {
        let mut v: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter_map(|f| f.strip_suffix(".test.ts").map(|s| s.to_string()))
            .collect();
        v.sort();
        if !v.is_empty() {
            return v;
        }
    }
    let mut v: Vec<String> = SUITES.iter().map(|(n, _, _)| n.to_string()).collect();
    v.sort();
    v
}

pub fn run(args: Vec<String>) -> Result<(), String> {
    let mut checkout: Option<PathBuf> = std::env::var("PTY_NODE_CHECKOUT").ok().map(PathBuf::from);
    let mut out_path = manifest_dir().join("../../docs/conformance.md");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--node" => {
                i += 1;
                checkout = Some(PathBuf::from(args.get(i).ok_or("--node needs a path")?));
            }
            "--out" => {
                i += 1;
                out_path = PathBuf::from(args.get(i).ok_or("--out needs a path")?);
            }
            other => return Err(format!("unknown argument {other}")),
        }
        i += 1;
    }
    // No default, and no silent degrade.
    //
    // This used to fall back to an absolute path on one developer's machine,
    // and `node_suites` falls back again to the compiled-in `SUITES` list when
    // it has no checkout — so running this with the variable unset produced a
    // checked-in document describing a Node tree nobody named, which looked
    // exactly as authoritative as a correct one. That is how docs/conformance.md
    // came to say `0.12.0+500eab2` while the pinned reference in `node-ref` was
    // `86dcc5e`, and a stale map is what a stale comparison is built on.
    //
    // A test may skip when its reference is missing. A generator may not guess:
    // it writes a file that outlives the run and carries no memory of what it
    // was made from.
    let Some(dir) = checkout.clone() else {
        return Err("PTY_NODE_CHECKOUT is not set, and there is no default.\n\
             This writes docs/conformance.md from a Node checkout, and a map \
             built from an unnamed source is worse than no map.\n\
             Set PTY_NODE_CHECKOUT to the commit in crates/pty-conformance/node-ref, \
             or pass --checkout <path>."
            .to_string());
    };
    if !dir.join("tests").is_dir() {
        return Err(format!(
            "PTY_NODE_CHECKOUT is {}, which has no tests/ directory.",
            dir.display()
        ));
    }

    let mapped = scan_tests(&manifest_dir().join("tests"))?;
    let suites = node_suites(checkout.as_deref());
    let classified: BTreeMap<&str, (Kind, &str)> =
        SUITES.iter().map(|(n, k, note)| (*n, (*k, *note))).collect();

    let mut by_suite: BTreeMap<String, Vec<&Mapped>> = BTreeMap::new();
    for m in &mapped {
        if let Some(suite) = &m.node_suite {
            by_suite.entry(suite.clone()).or_default().push(m);
        }
    }
    let unmapped = mapped.iter().filter(|m| m.node_suite.is_none()).count();
    for suite in by_suite.keys() {
        if !suites.contains(suite) {
            return Err(format!("tests reference unknown Node suite {suite}.test.ts"));
        }
    }
    for suite in &suites {
        if !classified.contains_key(suite.as_str()) {
            return Err(format!("Node suite {suite}.test.ts has no classification in conformance_map_impl.rs"));
        }
    }

    let total_tests = mapped.len();
    let gated: Vec<&Mapped> = mapped.iter().filter(|m| m.gated).collect();
    let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new(); // kind -> (suites, ported)
    for suite in &suites {
        let (kind, _) = classified[suite.as_str()];
        let e = counts.entry(kind.label()).or_default();
        e.0 += 1;
        if by_suite.contains_key(suite) {
            e.1 += 1;
        }
    }

    let mut md = String::new();
    md.push_str("# Conformance map\n\n");
    md.push_str("Generated by `cargo run -p pty-conformance --bin conformance-map`; do not edit by hand.\n\n");
    // DERIVED, never typed. This line used to carry the literal
    // "0.12.0+500eab2". It was true when it was written and silently wrong
    // ever afterwards, because regenerating the document could not update a
    // string that was not read from anything. A generated file asserting a
    // fact it did not measure is the same defect as the hardcoded checkout
    // path that used to sit above: it looks authoritative and ages badly.
    md.push_str(&format!(
        "One row per Node test file (`tests/*.test.ts` in the Node checkout, {}). ",
        node_reference(&dir)
    ));
    md.push_str("`kind` is how the suite is covered: `cli` (black-box through the binary), `protocol` (over the session socket), ");
    md.push_str("`unit` (pure logic, ported as crate unit tests), or `not-portable` (with the reason). ");
    md.push_str("A `cli`/`protocol` suite with no Rust file yet is still to do. ");
    md.push_str("Rust tests carry a `/// node: tests/<file>.test.ts:<line>` comment; the `tests` column counts them and the last column lists the Node lines they pin.\n\n");
    md.push_str("Run: `PTY_TEST_BIN=$(which pty) cargo test -p pty-conformance` (Node) and `cargo test -p pty-conformance` after `cargo build -p pty` (Rust). ");
    md.push_str("`scripts/conformance-both.sh` runs every file against both binaries and prints a side-by-side table plus `target/conformance/red.txt`, the tests that pass on Node and fail on Rust.\n\n");

    md.push_str("## Summary\n\n");
    md.push_str(&format!("- Node suites: {}\n", suites.len()));
    md.push_str(&format!(
        "- Rust conformance tests: {total_tests} ({} port a Node test, {unmapped} cover the Rust-owned fixtures)\n",
        total_tests - unmapped
    ));
    md.push_str(&format!(
        "- Gated (`_node`/`_rust` pairs pointing at a decision record): {} — the parity debt\n",
        gated.len()
    ));
    for (kind, (n, ported)) in &counts {
        if *kind == "not-portable" || *kind == "unit" {
            md.push_str(&format!("- {kind}: {n}\n"));
        } else {
            md.push_str(&format!("- {kind}: {n} suites, {ported} with Rust tests, {} to do\n", n - ported));
        }
    }
    md.push('\n');

    md.push_str("## Suites\n\n");
    md.push_str("| Node suite | kind | Rust test file(s) | tests | Node lines / reason |\n");
    md.push_str("|---|---|---|---|---|\n");
    for suite in &suites {
        let (kind, note) = classified[suite.as_str()];
        let tests = by_suite.get(suite);
        let mut files: Vec<String> = tests
            .map(|v| v.iter().map(|m| m.rust_file.clone()).collect())
            .unwrap_or_default();
        files.sort();
        files.dedup();
        let count = tests.map(|v| v.len()).unwrap_or(0);
        let mut lines: Vec<u32> = tests
            .map(|v| v.iter().filter_map(|m| m.node_line).collect())
            .unwrap_or_default();
        lines.sort_unstable();
        lines.dedup();
        let status = match kind {
            Kind::Cli | Kind::Protocol if count == 0 => format!("{} (to do)", kind.label()),
            _ => kind.label().to_string(),
        };
        let detail = if count > 0 {
            let l = lines.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
            if note.is_empty() { l } else { format!("{l} — {note}") }
        } else {
            note.to_string()
        };
        md.push_str(&format!(
            "| {suite}.test.ts | {status} | {} | {count} | {detail} |\n",
            if files.is_empty() { "—".to_string() } else { files.join(", ") }
        ));
    }
    md.push('\n');

    md.push_str("## Gated tests\n\n");
    if gated.is_empty() {
        md.push_str("None.\n\n");
    } else {
        md.push_str("| Rust test | Node suite | decision |\n|---|---|---|\n");
        for m in &gated {
            md.push_str(&format!(
                "| {}::{} | {} | see the test's doc comment |\n",
                m.rust_file,
                m.test_name,
                m.node_suite.as_ref().map(|s| format!("{s}.test.ts")).unwrap_or_else(|| "—".into())
            ));
        }
        md.push('\n');
    }

    md.push_str("## Fixtures\n\n");
    md.push_str("- `tests/fixtures/parity/{screens,shapes}.json` (Node-owned, vendored byte-identical; checked against `PTY_NODE_CHECKOUT` when set) — `parity_fixtures.rs`, `parity_shapes.rs`.\n");
    md.push_str("- `crates/pty-conformance/fixtures/*.json` (Rust-owned, v1) — `fixtures_protocol.rs`: bytes-split, escape-split, raw-bytes (decision 0001), attach-identity, late-events, frame-limits, slow-reader.\n\n");

    md.push_str("## Still to do\n\n");
    let mut todo: Vec<&String> = suites
        .iter()
        .filter(|s| {
            let (k, _) = classified[s.as_str()];
            matches!(k, Kind::Cli | Kind::Protocol) && !by_suite.contains_key(*s)
        })
        .collect();
    todo.sort();
    for s in todo {
        let (k, note) = classified[s.as_str()];
        md.push_str(&format!("- {s}.test.ts ({}): {note}\n", k.label()));
    }
    md.push_str("- Halves left out of otherwise-ported suites: the `pty evidence` half of exit-reap.test.ts and the evidence completions (deferred in docs/parity.md §12); the remote-serve --socket cases of remote-fabric.test.ts and wrapper-signal-forwarding.test.ts (dropped in docs/parity.md §12); the retention cap, readRecentEvents and EventFollower halves of events.test.ts, events-emit.test.ts and metadata-events.test.ts, the atomicWriteFileSync loops of atomic-writes.test.ts, the reapSkipped case of gc-parent-child.test.ts and the spawn-strategy cases of spawn-bundle-fallback.test.ts (library-only); the pure-function halves of terminal-queries.test.ts, process-tree.test.ts and parity-node-reference.test.ts (unit); the emulator-behaviour half of sanitize.test.ts; the attach-stream reconnect cases (:495, :563) that need attach --remote.\n");

    std::fs::write(&out_path, md).map_err(|e| format!("{}: {e}", out_path.display()))?;
    println!(
        "wrote {} ({} suites, {} tests, {} gated)",
        out_path.display(),
        suites.len(),
        total_tests,
        gated.len()
    );
    Ok(())
}
