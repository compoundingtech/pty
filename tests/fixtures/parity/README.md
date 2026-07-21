# Parity fixtures (shared, language-neutral)

Round-2 of the parity drive. These fixtures are the **single source of truth**
both the node suite and the Rust port (pty-rust) assert against, so the two
implementations cannot silently drift.

**Ownership:** the node repo **owns** these files. pty-rust vendors a
**byte-identical mirror** and writes the equivalent Rust assertions. When a
fixture changes here, the mirror must be re-synced.

**Assertion rules (agreed with pty-rust-claude):**
- **Plain-screen bytes** are asserted **EXACTLY** (`peek --plain` output, with
  the CLI's single trailing `\n` stripped).
- **ANSI** is asserted as a **mode-set** — *which* modes are present (e.g.
  `?25l`, `?1000h`, `?1006h`), never the raw byte stream.

## `screens.json`

```
{
  version: 2,
  fixtures: [
    {
      id: string,                 // stable identifier
      kind: "plain-screen"        // assert the live plain screen
          | "plain-screen-after-exit" // let it exit, then assert the preserved final screen + exit status
          | "reaped-after-exit",  // let it exit under REAP mode, then assert it is gone
      description: string,
      spawn: { command: string, args: string[], rows: number, cols: number },
      env?: { [k: string]: string }, // per-fixture daemon env overlay; pins the
                                  // exit-time mode (PTY_REAP_ON_EXIT=false = preserve)
      settleMs: number,           // wait after spawn (or after exit) before capturing
      expect: {
        plainScreen?: string,     // EXACT bytes of `peek --plain` (trailing \n stripped)
        plainScreenLength?: number,   // optional guard against pad/trim regressions
        status?: "exited",        // for *-after-exit fixtures: registry status
        exitCode?: number,        // for *-after-exit fixtures: recorded exit code
        reaped?: boolean          // for reaped-after-exit: peek fails + ls omits it
      }
    }
  ]
}
```

Exit-time behavior is **configurable** (`PTY_REAP_ON_EXIT`; shipped default
`reap`). The per-fixture `env` overlay pins the mode a fixture needs:
`post-exit-final-screen` runs with `PTY_REAP_ON_EXIT=false` (preserve → final
screen survives); `post-exit-reaped` runs with the default (reap → the session
removes itself, `peek` fails, `ls` omits it).

The node harness (`tests/parity-fixtures.test.ts`) spawns the same daemon module
the CLI daemonizes (with the fixture's `env`), waits `settleMs`, then for
plain-screen kinds runs `peek --plain` and asserts `stdout.replace(/\n$/,"") ===
expect.plainScreen`; for `reaped-after-exit` it asserts `peek` exits non-zero and
`ls --json` has no entry. pty-rust does the equivalent against its own binary.

**Seeds:** `idle-prompt-plain`, `post-exit-final-screen` (preserve),
`post-exit-reaped` (reap) are landed in `screens.json`. Coming next (JSON-shape
fixtures — captured example + per-field policy `exact | type:<t> |
omit-when-unset`, asserted as shape + stable values, not raw bytes):
`ls-json-shape` and `client-count-during-peek` (attached=0), in a sibling
`shapes.json`.
