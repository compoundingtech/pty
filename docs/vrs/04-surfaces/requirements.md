# Surface requirements

> **Role.** Compose the runtime, stream, and registry through supported user and
> embedding boundaries. Every requirement refines a root `PTY-R*` requirement.

## Assumptions

- **PTY.SURF-A01 Node package:** The distributed CLI and libraries run on a
  supported Node.js runtime with the package's native `node-pty` dependency.
- **PTY.SURF-A02 Trusted fabric peer:** Remote routing delegates peer transport
  and authorization to `fabric`; `pty` receives an ordered local stream.

## Acceptable tradeoffs

- **PTY.SURF-T01 Alpha TUI toolkit:** `@compoundingtech/pty/tui` is a shipped but
  alpha surface and may evolve under the pre-1.0 compatibility policy.
- **PTY.SURF-T02 CLI fallback for bundled embedders:** If an embedded client
  cannot locate the sibling server module, daemon creation may delegate to the
  installed `pty` CLI rather than materialize bundled source.

## Requirements

- **PTY.SURF-R01 — One behavioral core.** CLI and library operations call the
  same runtime, protocol, registry, event, and lifecycle primitives rather than
  defining parallel semantics. _refines: PTY-R08._
- **PTY.SURF-R02 — Real terminal testing.** The testing library drives real PTY
  processes and the same server protocol, exposes reconstructable text/ANSI
  screenshots, and applies effective geometry before affected terminal bytes.
  _refines: PTY-R03, PTY-R04, PTY-R08._
- **PTY.SURF-R03 — Explicit capability failure.** A surface validates required
  descriptors, refs, roots, and protocol order before mutation or output; an
  older daemon that lacks a required machine-stream contract fails clearly
  rather than silently degrading. _refines: PTY-R09._
- **PTY.SURF-R04 — Descriptor-preserving package entrypoint.** The shipped
  `bin/pty` runs the compiled CLI in the invoking process, preserving inherited
  descriptors above stderr and delivering signals to the actual CLI process.
  _refines: PTY-R08._
- **PTY.SURF-R05 — Schema-consistent CLI.** Command help, parsing, and generated
  bash/fish/zsh completions agree about flag arity and choices; stable ids and
  ambiguous display references follow registry resolution. _refines: PTY-R07,
  PTY-R08, PTY-R09._
- **PTY.SURF-R06 — Protocol-preserving remote route.** Remote list returns a
  structured control response; routed attach/peek/send hands the ordinary
  per-session protocol through unchanged, including reconnect baselines and
  machine-stream order. _refines: PTY-R03, PTY-R08._
- **PTY.SURF-R07 — Buildable published surface.** Package exports resolve to
  compiled `dist` modules with matching declarations, while TypeScript sources
  remain directly runnable for development. Missing compiled CLI output fails
  with an actionable error. _refines: PTY-R08, PTY-R09._

See [CLI and package](./01-cli-package/requirements.md) and
[libraries](./02-libraries/requirements.md) for concrete refinements.
