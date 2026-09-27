# pty-rust

`pty` keeps terminal sessions alive after you walk away. `pty run -- <command>`
starts the command inside a real pseudo-terminal that a small per-session daemon
owns; you can detach, come back with `pty attach`, read the screen from a script
with `pty peek --plain`, type into it with `pty send`, and list, tag, restart,
or kill sessions from any shell. Programs and agents drive sessions through the
same commands and JSON output. This repository is a Rust port of the Node
[`pty`](https://github.com/compoundingtech/pty-original-experiment), with
[libghostty](https://libghostty.tip.ghostty.org/) (the terminal core extracted
from [Ghostty](https://ghostty.org)) in place of `@xterm/headless`. The port is
meant as a drop-in: same commands, flags, texts, JSON shapes, exit codes, files
under `$PTY_ROOT`, and socket protocol, so the two implementations can share a
registry while a fleet migrates.

## Where it stands

**It is the daily driver on two machines** — an arm64 Mac and an x86_64 Linux
box — and has been since 2026-09-02. That is the honest measure of "does it
work": it is what those machines run, not a demo.

**The documented command surface matches the Node tool exactly.** Both
`pty help` and `pty --help` document the same command surface. Two are
deferred rather than missing — `pty recover` and `pty test` keep their help
text and print `pty <cmd>: not available in this build. See docs/parity.md.`

**1357 tests pass**, including a conformance suite that runs against *either*
binary. That is the part worth knowing: the two implementations are held to the
same assertions rather than compared by hand, so a behavioural difference fails
a test instead of surfacing later on somebody's machine.

**What is not finished** is written down rather than implied: the surface-by-
surface state is in [docs/parity.md](docs/parity.md), the decisions where the
two tools deliberately differ are in [docs/decisions/](docs/decisions/), and the
limits worth knowing before you rely on it are
[below](#one-known-defect-documented-rather-than-fixed).

**There are no prebuilt binaries yet.** You build it — see
[Install](#install) — and [Which systems it runs
on](#which-systems-it-runs-on) says what a built binary needs.

## Direction: compatibility and embedding

The long-term target is a behavior-compatible Rust implementation of the Node
`pty`, plus a first-class Rust API for embedding a live terminal in clients such
as Fractal. The Node implementation is the behavioral reference while the port
converges. **This README does not claim full parity**, and
[docs/parity.md](docs/parity.md) is where the remaining gaps are named surface by
surface.

Compatibility means that the same user-visible operations and wire messages have
the same result. It does not require identical source code or internal design.
Rust, `portable-pty`, and `libghostty` can require a different implementation.
When that difference changes behavior, record a decision that states the Node
behavior, the Rust behavior, the reason, the client effect, and the conformance
test.

The embedding API should serve the Rust CLI and other Rust clients through one
implementation. Its terminal handle should eventually provide the capabilities
that Node's `@myobie/pty/tui` `PtyHandle` provides: attach lifecycle, input,
resize, typed cell-grid and wrapped-line reads, cursor and terminal-mode state,
scrollback access, and activity or exit events. `libghostty::Terminal` is not
`Send`, so one clear actor must own it and publish typed events or snapshots to
consumers.

Keep the current protocol as the baseline. Add a protocol feature only after a
real failing use case shows that the current byte-framed messages cannot express
the required behavior. Track the compatibility matrix, crate boundaries, and
acceptance tests in [issue #1](https://github.com/compoundingtech/pty-rust/issues/1).

Where the port stands against the Node `pty`, surface by surface, is in
[docs/parity.md](docs/parity.md); the work packages that close the gap are in
[docs/parity-plan.md](docs/parity-plan.md).

What must stay true of terminal images — the Kitty graphics protocol state a
session holds and replays — is in
[docs/vrs/01-images/requirements.md](docs/vrs/01-images/requirements.md); how
the session meets it is in
[docs/vrs/01-images/spec.md](docs/vrs/01-images/spec.md).

## Install

With Nix (flakes), from a checkout or straight from GitHub:

```sh
nix build                                   # ./result/bin/pty, completions under ./result/share
nix run . -- help
nix profile install github:compoundingtech/pty-rust
```

The flake builds hermetically: Ghostty's source and its Zig packages are
fixed-output fetches, so `nix build` needs no network beyond the Nix cache.
`nix flake check` also runs the test suite and verifies that the installed
completion files are what the binary prints. `nix develop` opens a shell with
the Rust toolchain and Zig, pointed at the same pre-fetched Ghostty.

With Cargo (see the build requirements below):

```sh
cargo install --path crates/pty
```

**On macOS, use Nix instead.** The Cargo route fails on macOS 26.6 with the
26.5 SDK. Ghostty pins Zig 0.15.2, and that Zig cannot link the 26.5 SDK: it
reports undefined macOS system symbols. A newer Zig does not help, because the
Ghostty build requires 0.15.2 and refuses 0.16.0. Fresh checkouts and fresh Zig
caches give the same result. The Nix route builds and runs on the same machine.

**If you have no Nix, use the Node implementation that this port tracks.** It
runs on macOS and builds from a checkout in seconds:

```sh
git clone https://github.com/compoundingtech/pty-original-experiment node-pty && cd node-pty
npm install && npm run build
./bin/pty --version                         # 0.12.0+<short-sha>
```

Build it from the checkout. The package is not on npm, so `npm install -g`
does not work. The two tools read and write the same on-disk session registry,
so you can move between them later.

The Cargo route is verified on Linux: measured on x86_64 at commit `5d0d674`
with Zig 0.15.2.

Shell completions ship in [`completions/`](completions/) and are also printed
by `pty completions <fish|bash|zsh>`.

## Which systems it runs on

**There are no prebuilt binaries yet.** Today you build it. Nix works on Linux
and macOS. Cargo works on Linux; on macOS it fails, and the Install section
above says why.
This section says what a built binary needs, because that is the question a
release has to answer.

### Linux: glibc 2.34 or newer, and the floor depends on where you build

A binary links against the glibc it was built with, and refuses to start on
anything older:

```
$ ./pty --version
./pty: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found (required by ./pty)
```

**The floor is not set by this codebase.** It is set by two symbols the Rust
standard library uses for process spawning, `pidfd_getpid` and `pidfd_spawnp`,
which appear in any Rust program that calls `Command::status()`. Nothing here
references them.

That means the build host decides the floor, and the difference is large:

| built on | floor | runs on |
| --- | --- | --- |
| glibc 2.43 (Ubuntu 25.10) | `GLIBC_2.39` | Ubuntu 24.04+, Debian 13+, Fedora 40+ |
| glibc 2.36 (Debian 12) | `GLIBC_2.34` | the above, plus Ubuntu 22.04, Debian 12, RHEL 9, Rocky 9, Amazon Linux 2023 |

Measured on 2026-09-05 by running the same binary in each distribution's
container. **Build releases on the oldest glibc you intend to support**; no
source change is involved.

### macOS: it runs, and here is exactly what was tested

**Gatekeeper does not refuse the binary.** Tested on **one** machine: arm64,
macOS 26.6, build 25G72, Darwin 25.6.0, with Gatekeeper assessments enabled.

The `com.apple.quarantine` attribute was written onto the real binary, confirmed
present, and the binary ran and exited 0. The attribute was then removed and the
run repeated with an identical result — **without that control the first run
would prove nothing about the attribute.**

Three conditions travel with that result:

- **It is one macOS version.** A bare "Gatekeeper does not apply" would outlive
  the release it was true of.
- **The binary carried an ad-hoc linker signature**, which macOS applies
  automatically on Apple silicon. That may be why it passed. A release built
  somewhere that strips or omits it is a different case, and an untested one, so
  **the release process must preserve it.**
- **This result does not transfer to a published asset.** The same test, with
  its removal control, has to run against the first one we ship.

### CI is Linux only, on purpose

Both workflows run on `ubuntu-latest`. Nothing in CI builds or tests this on a
Mac.

That matters more here than it usually would.
[`crates/pty-core/src/proctable.rs`](crates/pty-core/src/proctable.rs) carries a
macOS process-table reader that no Linux job ever compiles: libproc, plus a
sysctl fallback with hand-declared `kinfo_proc` struct offsets. Offsets are
exactly the kind of thing a new macOS moves. **So a change to that reader, or a
macOS SDK change, breaks the Mac build and CI does not notice.**

This is a decision, not an oversight. The tool runs on two Macs every day, so a
broken Mac build surfaces immediately in use, and human use is the detection
mechanism. That trade holds while the daily users are the affected users. If
this ever ships to people who are not in the room, the trade changes and the
macOS job comes back — via Nix, because Cargo cannot build it there.

## Usage

```sh
pty run -d --name "API" -- node server.js    # start a session in the background
pty list                                     # sessions (--json for programs)
pty peek --plain API                         # the screen as plain text
pty send API --seq "npm test" --seq key:return
pty attach API                               # interactive; Ctrl+\ detaches
pty kill API && pty rm API
pty help                                     # every command; pty <cmd> --help for one
```

Sessions live under `$PTY_ROOT` (default `~/.local/state/pty`): one unix socket,
pid file, and metadata file per session. Set `PTY_ROOT` to isolate a registry,
for example in tests.

`pty list --json --clients` adds `clients` to each running session: an array
of `{ "pid": 1234, "tty": "/dev/pts/3", "attachedAt": "2026-09-25T12:00:00.000Z" }`.
It is opt-in because it asks every running daemon (up to 16 at a time, with
one 500 ms budget for the whole listing); plain `pty list --json` contacts no
daemon and has no `clients` key. `--clients` is ignored without `--json`, like
the other view flags. The array is empty when no client is attached, `null`
when the daemon did not answer in time or predates the query, and the key is
absent on exited or vanished sessions. A client without a terminal reports
`tty: null`; older clients also report `pid: null` because they do not send
their identity.

`pty version` prints `0.13.<n>-rust+<short-sha>`: one minor above the Node line,
a `rust` pre-release tag, and the commit it was built from.

### Library API

`pty-core` exposes the same listing to Rust consumers through
`pty_core::client::list`. `pty list --json --clients` uses this
implementation too. The module is unstable: it will move to
`SessionRef`/`PtyRoot` (#1, #3), and `SessionInfo` currently exposes the
on-disk session metadata as-is.

```rust
use pty_core::client::list::{ClientQuery, ClientSet, ListOptions, list};
use pty_core::registry::session_dir;

let sessions = list(
    &session_dir(),
    &ListOptions {
        clients: Some(ClientQuery::default()), // 500 ms total, 16 at a time
        ..Default::default()
    },
);
for s in &sessions {
    match &s.clients {
        Some(ClientSet::Known(clients)) => {
            for c in clients {
                println!("{} <- pid {:?} on {:?}", s.info.name, c.pid, c.tty);
            }
        }
        Some(ClientSet::Unknown) => println!("{}: clients unknown", s.info.name),
        None => {} // not running, or clients not requested
    }
}
```

`attached_clients(&sessions, &ClientQuery)` queries an already-filtered
`&[SessionInfo]` and returns one `ClientSet` per session, in the same order.

### If you are already inside a session

**`run` and `attach` refuse to nest, and `--force` is how you say you meant
it.** A session inside a session is usually a mistake — a detach key press then
reaches the wrong one — so both commands stop and explain instead.

```sh
pty attach --force API      # attach from inside another session
pty run --force -- <cmd>    # create one from inside another session
```

**The refusal goes to standard error and exits 1**, in this tool and in the
Node one. A script that captures only standard output sees an empty result
and can mistake that for success, which is what happens if you forget
`--force`; the exit code tells you the truth.

`run` and `restart` are different and deliberately so: from inside a session
`run` runs the command directly and `restart` restarts without attaching.
Both did what was asked, so both exit 0.

### Commands not in this build

Two Node commands are deferred (see [docs/parity.md §12](docs/parity.md#12-candidates-to-leave-off)
for the reasoning): `pty recover` and `pty test`. Their help texts are kept
verbatim so `--help` still describes them, but running them prints
`pty <cmd>: not available in this build. See docs/parity.md.` and exits 1.

### Lock compatibility boundary

Rust publishes each `0600` lock owner record under a unique sibling name and
hard-links it into the canonical path with no replacement. The canonical path
is therefore never visible before its complete decimal pid. A stale-lock
stealer also locks the inode it inspected and verifies that the path still
names that inode before unlinking it, so a delayed Rust stealer cannot remove
a newer owner's lock.

The Node tool still creates the canonical file before writing its pid and
steals with an unbound read-then-unlink sequence. Rust safely respects a live
Node lock after its complete pid is visible, and Rust-only stale recovery is
exclusive. If any concurrent stale-recovery path involves Node, a delayed
Node contender can still unlink a newer Rust or Node claim. Such recovery
must be externally serialized in a mixed registry.
`crates/pty-core/src/registry/lock.rs` and `docs/hardening.md` describe the
boundary in full.

## The crates

A Cargo workspace of six crates under `crates/`:

- **`pty-core`** — the wire protocol, session registry and locks, events,
  metadata, names and tags, key/paste/duration/input parsing, `pty.toml`
  manifests, and the client operations (attach loop, peek, send, status). No
  terminal emulator, no Zig.
- **`pty-terminal`** — the libghostty actor: owns the terminal, produces typed
  snapshots and the VT/plain serializations, answers terminal queries.
- **`pty-testkit`** — Playwright-style test sessions: spawn a process in a real
  PTY, feed it to libghostty, take screenshots, wait for text, send named keys.
- **`pty-tui`** — the TUI library (ratatui + crossterm): pane, theme, focus,
  widgets, and the app runner behind the interactive session manager.
- **`pty-conformance`** — the black-box suite that runs against any `pty`
  binary, Node or Rust, chosen with `PTY_TEST_BIN`.
- **`pty`** — the `pty` binary: the command-line interface, the per-session
  daemon, and the remote bridge.

## Building from source

- Rust 1.88 or newer (edition 2024; `rust-version` is pinned in `Cargo.toml`).
- Zig 0.15.2 on `PATH`, and `git`: the `libghostty-vt-sys` crate builds
  Ghostty's terminal core from source with Zig. The version is exact. Ghostty
  refuses 0.16.0, and 0.15.2 cannot link the macOS 26.5 SDK, so a source build
  on current macOS needs Nix.
- The first build clones Ghostty at the commit `libghostty-vt-sys` pins and
  lets Zig fetch Ghostty's own packages; both are cached under `target/` after
  that. To build without network, point `GHOSTTY_SOURCE_DIR` at a Ghostty
  checkout and `GHOSTTY_ZIG_SYSTEM_DIR` at a populated Zig package directory
  (`flake.nix` shows how both are produced).

```sh
cargo build --release                        # target/release/pty
```

## Running the tests

```sh
cargo test --workspace                       # every crate's suite
PTY_TEST_BIN=target/release/pty cargo test -p pty-conformance   # black-box, any binary
./scripts/conformance-both.sh                # both binaries, side by side
python3 scripts/check-divergences.py         # fail on an unrecorded difference
```

`conformance-both.sh` runs every conformance file against both binaries and
writes `target/conformance/red.txt`: the tests whose result differs.
`check-divergences.py` compares that against
[`crates/pty-conformance/divergences.toml`](crates/pty-conformance/divergences.toml)
and fails both when a difference is unrecorded and when a record no longer
happens, so the ledger cannot drift into a list of stale claims. CI runs both.
The Node commit it compares against is pinned in
[`crates/pty-conformance/node-ref`](crates/pty-conformance/node-ref); a stale
reference invents differences that are not there.

The workspace tests drive real programs through real PTYs and real daemons,
with each test on its own `PTY_ROOT` under the temp dir. The conformance suite
runs the same way against whichever binary `PTY_TEST_BIN` names, so it can be
pointed at the Node `pty` to check the reference itself. Help texts and
completion scripts are vendored byte for byte from the Node repository
(`crates/pty/tests/fixtures/help/`, `completions/`) and the tests hold the
binary to them.

### Reading a single failure from a full run

**One test failing in a whole-workspace run is not yet a defect. Re-run it
alone before treating it as one.**

```sh
cargo test -p <crate> --test <binary> -- --exact --test-threads=1 <name>
```

The suite runs 139 binaries in parallel, and each one drives real processes
through real terminals. On a machine slow enough, one or two of them lose a
race per run — **and which ones varies across the whole suite**, so a name you
have never seen before is the normal case rather than a new regression.

**Measured on 2026-09-02, and the two machines differ sharply.** Seventeen
whole-workspace runs on one Linux host: fifteen completely clean, and the two
that were not each named a real defect that was then fixed. No run there lost
a race. Four runs on an Apple silicon laptop: one or two lost races every
time, never the same ones, all green when run alone.

**So do not chase these by name.** A failure worth fixing has a cause you can
state — the two in this repository's history that looked like this both did: a
sleep standing in for a handshake, in a test that then failed reliably once
the timing was turned up. **A name that passes alone and has no such cause is
a scheduling accident**, and hunting them one at a time is unbounded work.

Whether a slower machine is the whole explanation is not established; there is
one laptop and nothing to compare it against.

### Checking the macOS build without a Mac

`pty-core` deliberately has no Zig dependency, so it can be type-checked for
Apple silicon from any machine:

```sh
rustup target add aarch64-apple-darwin
cargo check -p pty-core --target aarch64-apple-darwin
cargo check -p pty      --target aarch64-apple-darwin
```

**This is worth running before you touch anything platform-specific.** It
caught a call to `pipe2`, which Linux has and macOS does not, and it produced
the same error a Mac did.

Both crates that hold platform-specific code are covered, and the check really
does compile the macOS branches — a deliberate error inside one is reported,
and the host build is unaffected by it. Running the whole workspace's TESTS
still needs a Mac.
