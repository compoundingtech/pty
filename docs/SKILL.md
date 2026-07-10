---
name: pty
description: >-
  Run and manage long-lived or background processes — dev servers, test suites,
  builds, interactive CLIs, agents — in persistent, detachable terminal
  sessions. Reach for pty INSTEAD of `&` / nohup / raw background shell whenever
  you need to start work, go do something else, then come back to read its
  output, send it input, or restart it; and for any interactive tool that needs
  a real TTY (auth/keychain prompts, TUIs, REPLs).
when_to_use: >-
  An agent needs to start a process and check on it later; run a dev server /
  test suite / build and wait for a readiness or result line; drive an
  interactive CLI that needs a real terminal; or keep a process alive across
  disconnects. NOT for one-shot commands whose output you read immediately —
  run those directly.
---

# pty — persistent terminal sessions

## What it is
`pty` runs a command in a managed terminal session you can detach from and
reconnect to later, from anywhere (including over SSH). It's the terminal /
session layer: `run`, `list`, `peek`, `send`, `restart`, `kill`, `up`.

## When to reach for it
- A long-lived / background process: dev server, test suite, build, watcher, an agent.
- An interactive CLI that needs a real TTY: keychain/auth prompts, a TUI, a REPL.
- Any "start it, go do something else, come back to read / send / restart" task.

Prefer `pty` over `&` / `nohup` / pipes for these — you get lifecycle control,
readable replayed output, and the ability to wait for specific text. For a
one-shot command whose output you read right now, just run it directly.

## The idiom (happy path)
```sh
pty run -d --name <name> --tag owner=<you> -- <command>   # start detached, tagged
pty peek --wait "<ready text>" --plain <name> -t 30       # block until ready
pty peek --full --plain <name>                            # read full output
pty send <name> --seq "<text>" --seq key:return           # send input + Enter
pty kill <name>                                           # clean up when done
```
Tag the sessions you create; only touch sessions you created.

## Footguns (the ones that actually bite)
- **A broken global `pty` on `$PATH` silently breaks the whole message bus.**
  `st` / smalltalk delivery shells out to `pty send` found on `$PATH`. If a
  global-install symlink points at a stale or broken `pty`, *every* agent's
  message delivery fails network-wide — silently. Run `pty` from the intended
  install; if you do global-install, confirm `pty --version` works before
  trusting delivery.
- **Isolation is `PTY_ROOT`, not `PTY_SESSION_DIR`.** To keep scratch/test
  sessions out of the production registry, set `PTY_ROOT=<dir>`.
  `PTY_SESSION_DIR` is a deprecated alias and is *ignored* when `PTY_ROOT` is
  already set (as it is inside a supervised session tree) — so setting only
  `PTY_SESSION_DIR` there leaks your sessions into the ambient registry. pty
  now warns when both are set.
- **Sending text + Enter: mind the timing (top cause of "I sent it but nothing
  happened").** `pty send <ref> "text"` sends NO newline — to submit, use
  `pty send <ref> --seq "text" --seq key:return`. The *why* it can silently fail:
  a terminal program processes a burst of bytes differently from spaced-out
  input. With zero spacing, the trailing `key:return` routinely arrives before
  the program's readline / PTY event loop has parsed and rendered the typed
  text (and before bracketed-paste framing closes), so the Enter submits an
  empty or partial line. `pty send` now inserts a **0.3s gap between `--seq`
  items by default** so each chunk is consumed before the next — you usually
  don't need to think about it. Overrides: `--with-delay <sec>` to tune (some
  slow TUIs want 0.5s+), and **`--with-delay 0` for a raw back-to-back stream**
  (fast/bulk sends where you know the receiver can take it).
- **Don't nest.** Inside a session, a bare `pty run` runs the command directly
  (nesting guard); use `pty run -d` to explicitly background a new session from
  inside one.

## The exact surface
Run `pty --help` for the full subcommand list, and `pty <subcommand> --help` for
that command's flags and examples. `pty --version` prints `<semver>+<short-sha>`.
