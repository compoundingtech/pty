import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as readline from "node:readline/promises";
import { spawnSync, execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { attach, peek, send, queryStats, type StatsResult } from "./client.ts";
import { parseSeqValue } from "./keys.ts";
import {
  listSessions,
  getSession,
  gc,
  pruneOrphanLayoutTags,
  isGone,
  cleanupAll,
  cleanupSocket,
  validateName,
  validateDisplayName,
  acquireLock,
  releaseLock,
  updateTags,
  setDisplayName,
  allRefs,
  readMetadata,
  writeMetadata,
  atomicWriteFileSync,
  getSessionDir,
  DEFAULT_SESSION_DIR,
  getState, getStateKey, setState, deleteState, listStateKeys,
  type SessionInfo,
} from "./sessions.ts";
import { spawnDaemon, resolveCommand } from "./spawn.ts";
import {
  EventFollower, EventWriter, EventType,
  readRecentEvents, formatEvent,
  emitUserEvent,
} from "./events.ts";
import { readPtyFile, commandWithEnvExports, type PtySessionDef } from "./ptyfile.ts";
import { extractFilterTags as extractFilterTagsImpl, matchesAllTags, isReservedTagKey } from "./tags.ts";
import { parseDuration, formatDuration } from "./duration.ts";

// Name this process so it shows up meaningfully in ps/top/htop/btm instead of
// "MainThread" (V8's default main-thread name under Node 24+). `process.title`
// is the only thing that overrides /proc/<pid>/comm, and only when set from
// within the running process after V8 init — launch flags like `node --title`
// or `exec -a` do not work. Linux truncates comm at 15 chars (TASK_COMM_LEN).
// This module is only ever an entrypoint (it calls main() on load), so setting
// the title at module scope is safe.
try { process.title = "pty"; } catch {}

// Lazy-load the interactive TUI so non-interactive commands don't crash when
// the caller's cwd was deleted (the TUI module evaluates process.cwd() at load).
async function runInteractive(options?: { preselectNew?: boolean; filterTags?: Record<string, string>; force?: boolean }): Promise<void> {
  ensureNotNested("interactive", {
    force: options?.force,
    hint:
      "  The interactive picker would render inside your current session and detach would route to the outer client.\n" +
      "  Detach first (Ctrl+\\) and run `pty` from outside, or pass --force to open the picker anyway.",
  });
  const mod = await import("./tui/interactive.ts");
  await mod.runInteractive(options);
}

/** CLI wrapper around `extractFilterTags` that exits on invalid input. */
function extractFilterTags(args: string[]): Record<string, string> {
  try {
    return extractFilterTagsImpl(args);
  } catch (e: any) {
    console.error(e.message);
    process.exit(1);
  }
}

function usage(): void {
  console.log(`Usage:
  pty                                       Interactive session manager
  pty --preselect-new                       Open the TUI with "Create new session..." pre-selected
  pty --filter-tag key=value                Filter TUI to sessions with a tag; auto-applied to new sessions
  pty --root <path> <subcommand> [...]     Pin the state registry for this call (same as PTY_ROOT env)
  pty run -- <command> [args...]            Create a session and attach (random id + auto label)
  pty run --id <id> -- <command>            Pin the on-disk id (sock + json filename)
  pty run --name <dn> -- <command>          Set the display label (arbitrary length / chars)
  pty run --id <id> --name <dn> -- <cmd>    Pin both id and display label
  pty run -d -- <command> [args...]         Create in the background
  pty run -a -- <command> [args...]         Create or attach if already running
  pty run --tag key=value -- <command>      Tag a session with metadata
  pty run --cwd /path -- <command>          Run in a specific directory
  pty run --isolate-env -- <command>        Scrub env down to a safe allow-list (for remote-reachable sessions)
  pty run --no-display-name -- <command>    Skip the friendly cwd+command label (just an id)
  pty rename <new>                        Inside a session, set its displayName
  pty rename <ref> <new>                  Outside, set displayName on <ref>
  pty rename --show <ref>                 Show current displayName for <ref>
  pty rename --clear [ref]                Remove displayName (ref required outside a session)
  pty attach <name>                        Attach to an existing session
  pty attach --force <name>                Attach even when already inside a pty session (nested)
  pty exec -- <command> [args...]          Replace the current session's command
  pty attach -r <name>                     Attach, auto-restart if exited
  pty peek <name>                          Print current screen and exit
  pty peek --plain <name>                  Print current screen as plain text (no ANSI)
  pty peek --full <name>                   Print full scrollback (not just viewport)
  pty peek --wait "text" <name>            Wait until text appears on screen
  pty peek --wait "text" -t 10 <name>      Wait with timeout (seconds)
  pty peek -f <name>                       Follow output read-only (Ctrl+\\ to stop)
  pty send <name> "text"                   Send text to a session
  pty send <name> --seq "text" --seq key:return  Send an ordered sequence
  pty send <name> --with-delay 0.5 --seq ...     Delay between each --seq item
  pty send <name> --paste "<big text>"           Wrap payload in bracketed-paste markers
  pty restart <name>                       Restart a session (prompts if running)
  pty restart -y <name>                    Restart without confirmation
  pty events <name>                        Follow events from a session
  pty events --all                         Follow events from all sessions
  pty events --recent <name>               Show recent events and exit
  pty events --json <name>                 Output raw JSONL
  pty list                                 List active sessions (with tags)
  pty list --tags                          Show all tags including internal bookkeeping (ptyfile*, strategy, etc.)
  pty list --json                          List sessions as JSON
  pty list --filter-tag key=value          List only sessions matching the tag (repeatable)
  pty up                                   Start all sessions from pty.toml
  pty up <dir>                             Start sessions from <dir>/pty.toml
  pty up <name> [<name>...]               Start specific sessions from pty.toml
  pty down                                 Stop all sessions from pty.toml
  pty down <dir>                           Stop sessions from <dir>/pty.toml
  pty down <name> [<name>...]             Stop specific sessions from pty.toml
  pty kill <name>                          Kill or remove a session
  pty gc                                   Remove all exited sessions
  pty tag <name>                           Show tags on a session
  pty tag <name> key=value [key=value...]  Set tags
  pty tag <name> --rm key [--rm key...]    Remove tags
  pty tag-multi <selector> [ops...]        Read/write tags across multiple sessions (--all / --filter-tag k=v / <name>...)
  pty gc --print-launchd-plist [--interval=N]   Print a launchd plist that runs 'pty gc' every N seconds (default 30)
  pty gc [--dry-run] [--idle-days N]       Reconciliation pass; --idle-days N reaps permanent sessions with no attach in N days
  pty wrap <command>                       Auto-wrap a command in pty sessions
  pty unwrap <command>                     Remove a wrap
  pty wrap --list                          List wrapped commands
  pty test                                 Run tests (vitest)
  pty test watch                           Watch mode
  pty test -t "pattern"                    Run matching tests

Detach from a session with Ctrl+\\ (press twice to send Ctrl+\\ to the process)`);
}

/** Resolve a user-supplied session reference (name OR displayName) to the
 *  stable `name`. Errors and exits if no session matches. Use this whenever
 *  a command is about to hit the socket, metadata file, or anything else
 *  keyed by the stable id — it ensures typing the displayName works the
 *  same as typing the underlying name. */
async function resolveRef(ref: string): Promise<string> {
  const session = await getSession(ref);
  if (!session) {
    console.error(`Session "${ref}" not found.`);
    process.exit(1);
  }
  return session.name;
}

/** Refuse a command that would start a nested client inside an existing
 *  pty session. Several commands (attach, restart-then-attach, the
 *  interactive picker, run -a when the target is running) silently created
 *  a client-inside-a-client, routing detach keybindings to the outer
 *  client and tangling the user up. `--force` opts back into the old
 *  behavior for the rare cases where nesting is intentional (debugging,
 *  screen-sharing demos). Prints + exits; does not return on refusal. */
function ensureNotNested(
  cmd: string,
  opts: { force?: boolean; hint?: string } = {},
): void {
  if (opts.force) return;
  const nested = process.env.PTY_SESSION;
  if (!nested) return;
  console.error(`pty ${cmd}: already inside pty session "${nested}".`);
  if (opts.hint) console.error(opts.hint);
  else console.error("  Pass --force to override.");
  process.exit(1);
}

/** Generate a short random session id. Base32 (Crockford-ish, no 0/O/1/I
 *  confusion). 8 chars = 40 bits — plenty of headroom against collisions
 *  even with thousands of sessions per machine. */
function randomSessionName(): string {
  const alphabet = "23456789abcdefghjkmnpqrstuvwxyz";
  const bytes = randomBytes(8);
  let out = "";
  for (const b of bytes) out += alphabet[b % alphabet.length];
  return out;
}

/** Generate a session name from the cwd and command. */
function autoName(cmd: string, cmdArgs: string[]): string {
  // Directory component: last part of cwd
  const dirPart = path.basename(process.cwd());

  // Command component: base name of the command + first meaningful arg
  const cmdBase = path.basename(cmd);
  const firstArg = cmdArgs.find(a => !a.startsWith("-") && a.length < 30);
  let cmdPart = cmdBase;
  if (firstArg) {
    // Strip extension and path, keep only alphanumeric/dash/dot
    const argBase = path.basename(firstArg).replace(/\.[^.]+$/, "");
    if (argBase && /^[a-zA-Z0-9._-]+$/.test(argBase)) {
      cmdPart = `${cmdBase}-${argBase}`;
    }
  }

  return `${dirPart}-${cmdPart}`;
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);

  // Global --root <path>: pin the state registry for this invocation.
  // Consumed here so every subcommand transparently scopes via
  // getSessionDir(). Equivalent to PTY_ROOT=<path> for one call.
  // Scanned across the full argv because no subcommand uses --root.
  const rootIdx = args.indexOf("--root");
  if (rootIdx !== -1) {
    const val = args[rootIdx + 1];
    if (!val || val.startsWith("-")) {
      console.error("pty: --root requires a path (e.g. pty --root /var/lib/pty-eval list)");
      process.exit(1);
    }
    process.env.PTY_ROOT = val;
    args.splice(rootIdx, 2);
  }

  // Interactive-mode flags (--preselect-new, --filter-tag) can appear before
  // the subcommand. Peek at the subcommand without consuming flags; if it's
  // the interactive TUI (none, "i", or "interactive"), consume those flags
  // here. Otherwise leave them in args for the subcommand to parse itself.
  //
  // Detect the subcommand: first positional that isn't a flag or a value for
  // a known flag that takes a value (currently just --filter-tag).
  let subcommand = "";
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "--filter-tag") { i++; continue; }
    if (a.startsWith("-")) continue;
    subcommand = a;
    break;
  }

  let preselectNew = false;
  let interactiveFilterTags: Record<string, string> = {};
  let interactiveForce = false;
  if (!subcommand || subcommand === "i" || subcommand === "interactive") {
    preselectNew = args.includes("--preselect-new");
    interactiveForce = args.includes("--force");
    interactiveFilterTags = extractFilterTags(args);
  }
  const dispatchArgs = args.filter((a) => a !== "--preselect-new" && a !== "--force");

  if (dispatchArgs.length === 0) {
    await runInteractive({ preselectNew, filterTags: interactiveFilterTags, force: interactiveForce });
    return;
  }

  const command = dispatchArgs[0];

  switch (command) {
    case "interactive":
    case "i": {
      await runInteractive({ preselectNew, filterTags: interactiveFilterTags, force: interactiveForce });
      break;
    }

    case "run": {
      // Parse flags before the -- separator. The flag model:
      //   --id <id>     explicit on-disk id (sock/json filename). Validated:
      //                 charset, sock-path length, no existing-ref collision.
      //   --name <dn>   explicit display label (arbitrary length / chars,
      //                 within the permissive validateDisplayName rules).
      //                 Replaces the auto-generated cwd+cmd label.
      //   --no-display-name  skip displayName entirely.
      //   Both omitted → random short id + auto-generated displayName.
      let detach = false;
      let attachExisting = false;
      let ephemeral = false;
      let isolateEnv = false;
      let noDisplayName = false;
      let force = false;
      let explicitId: string | null = null;
      let explicitDisplayName: string | null = null;
      let cwd: string | null = null;
      const tags: Record<string, string> = {};
      let i = 1;
      while (i < args.length && args[i] !== "--") {
        if (args[i] === "-d" || args[i] === "--detach") { detach = true; i++; }
        else if (args[i] === "-a" || args[i] === "--attach") { attachExisting = true; i++; }
        else if (args[i] === "-e" || args[i] === "--ephemeral") { ephemeral = true; i++; }
        else if (args[i] === "--isolate-env") { isolateEnv = true; i++; }
        else if (args[i] === "--no-display-name") { noDisplayName = true; i++; }
        else if (args[i] === "--force") { force = true; i++; }
        else if (args[i] === "--id" && i + 1 < args.length) { explicitId = args[i + 1]; i += 2; }
        else if (args[i] === "--name" && i + 1 < args.length) { explicitDisplayName = args[i + 1]; i += 2; }
        else if (args[i] === "--cwd" && i + 1 < args.length) { cwd = args[i + 1]; i += 2; }
        else if (args[i] === "--tag" && i + 1 < args.length) {
          const eq = args[i + 1].indexOf("=");
          if (eq === -1) {
            console.error(`Invalid tag format: "${args[i + 1]}". Use --tag key=value`);
            process.exit(1);
          }
          tags[args[i + 1].slice(0, eq)] = args[i + 1].slice(eq + 1);
          i += 2;
        }
        else break;
        // Note: unknown flags or positional args before -- break the loop
      }

      // Everything after -- is the command
      const dashDash = args.indexOf("--", i);
      let cmd: string;
      let cmdArgs: string[];

      if (dashDash !== -1) {
        // Anything between flags and -- that isn't a flag is a legacy
        // positional that's now interpreted as the display name. The
        // previous semantics (positional = on-disk id) is gone — use --id.
        const between = args.slice(i, dashDash);
        if (between.length > 0 && !explicitDisplayName) {
          explicitDisplayName = between[0];
          console.error(`Hint: use --name instead: pty run --name ${between[0]} -- ...`);
        }
        cmd = args[dashDash + 1];
        cmdArgs = args.slice(dashDash + 2);
      } else {
        // No -- separator: legacy positional format
        // pty run mydisplayname node server.js
        const rest = args.slice(i);
        if (!explicitDisplayName && rest.length >= 2) {
          explicitDisplayName = rest[0];
          cmd = rest[1];
          cmdArgs = rest.slice(2);
          console.error(`Hint: use --name instead: pty run --name ${rest[0]} -- ${cmd} ${cmdArgs.join(" ")}`.trimEnd());
        } else {
          cmd = rest[0];
          cmdArgs = rest.slice(1);
        }
      }

      if (!cmd) {
        console.error("Usage: pty run [--id <id>] [--name <displayName>] [-d] [-a] -- <command> [args...]");
        process.exit(1);
      }

      const autoNameCmd = cmd;
      const displayCmd = [cmd, ...cmdArgs].join(" ");
      try {
        cmd = resolveCommand(cmd);
      } catch (e: any) {
        console.error(e.message);
        process.exit(1);
      }

      // Nesting prevention: if inside a pty session and not detaching, exec
      // directly. The -a branch is narrower: if the caller asked to attach-
      // if-running and the target IS running, attaching would nest a client —
      // error with a clear message unless --force. If the target isn't
      // running, the original "exec directly" behavior still makes sense
      // (there's no session to attach to anyway).
      if (process.env.PTY_SESSION && !detach) {
        // Nested-attach check: target by either the explicit id or display name
        const lookupRef = explicitId ?? explicitDisplayName;
        if (attachExisting && lookupRef && !force) {
          const existing = await getSession(lookupRef);
          if (existing && existing.status === "running") {
            ensureNotNested("run -a", {
              force: false,
              hint:
                `  Target session "${lookupRef}" is already running; attaching would nest a client inside the current session.\n` +
                "  Pass --force to attach anyway, or detach first (Ctrl+\\) and re-run from outside.",
            });
          }
        }
        console.error(
          `Already inside pty session "${process.env.PTY_SESSION}", running directly.`
        );
        const result = spawnSync(cmd, cmdArgs, {
          stdio: "inherit",
          env: process.env,
        });
        process.exit(result.status ?? 1);
      }

      const existingRefs = await allRefs();

      // Resolve `name` (the on-disk id). If --id was passed, validate and use
      // it verbatim; otherwise generate a short random id. Charset, length,
      // and uniqueness checks are all done up front so automation fails
      // loudly rather than hitting EINVAL/ENAMETOOLONG deep in spawn.
      //
      // Uniqueness exception: under `-a` (attach-or-create), a collision
      // with an existing session is the *expected* path — cmdRun attaches
      // a running session or recreates an exited one. Defer to cmdRun.
      let name: string;
      if (explicitId) {
        try {
          validateName(explicitId);
        } catch (e: any) {
          console.error(e.message);
          process.exit(1);
        }
        if (existingRefs.has(explicitId) && !attachExisting) {
          console.error(`Session id "${explicitId}" is already in use (as a name or displayName).`);
          process.exit(1);
        }
        name = explicitId;
      } else {
        let candidate: string | null = null;
        for (let attempt = 0; attempt < 8; attempt++) {
          const c = randomSessionName();
          if (!existingRefs.has(c)) { candidate = c; break; }
        }
        if (!candidate) {
          console.error("Could not generate a unique session id after 8 attempts.");
          process.exit(1);
        }
        name = candidate;
      }

      // Resolve `displayName`. Precedence:
      //   1. --no-display-name → null
      //   2. --name <x>         → x (validated permissively, deduped only
      //                            against existing refs)
      //   3. otherwise          → auto cwd+cmd label (sanitized + deduped)
      let displayName: string | null = null;
      if (!noDisplayName) {
        if (explicitDisplayName) {
          try {
            validateDisplayName(explicitDisplayName);
          } catch (e: any) {
            console.error(`Invalid displayName: ${e.message}`);
            process.exit(1);
          }
          if (explicitDisplayName === name) {
            console.error(`displayName cannot equal the session's id ("${name}").`);
            process.exit(1);
          }
          if (existingRefs.has(explicitDisplayName)) {
            console.error(`"${explicitDisplayName}" is already in use by another session (as a name or displayName).`);
            process.exit(1);
          }
          displayName = explicitDisplayName;
        } else {
          let candidate = autoName(autoNameCmd, cmdArgs);
          candidate = candidate.replace(/[^a-zA-Z0-9._-]/g, "-").replace(/-+/g, "-").replace(/^-|-$/g, "");
          if (existingRefs.has(candidate) || candidate === name) {
            for (let n = 2; ; n++) {
              const c = `${candidate}-${n}`;
              if (!existingRefs.has(c) && c !== name) { candidate = c; break; }
            }
          }
          displayName = candidate;
        }
      }

      await cmdRun(name, cmd, cmdArgs, detach, attachExisting, displayCmd, ephemeral, tags, cwd, isolateEnv, displayName);
      break;
    }

    case "attach":
    case "a": {
      let autoRestart = false;
      let force = false;
      let attachName: string | null = null;
      for (let ai = 1; ai < args.length; ai++) {
        const a = args[ai];
        if (a === "--auto-restart" || a === "-r") autoRestart = true;
        else if (a === "--force") force = true;
        else if (!attachName) attachName = a;
        else {
          console.error(`pty attach: unexpected argument "${a}"`);
          process.exit(1);
        }
      }
      if (!attachName) {
        console.error("Usage: pty attach [-r|--auto-restart] [--force] <name>");
        process.exit(1);
      }
      // Nesting guard runs BEFORE name validation / ref resolution. A nested
      // caller gets the informative nesting message even if they mistyped
      // the session name — otherwise they'd fix the typo, try again, and
      // only then discover they shouldn't attach at all.
      ensureNotNested("attach", {
        force,
        hint:
          "  Attaching now would nest a client inside the current session — detach keys route to the outer client and get tangled.\n" +
          "  Detach first (Ctrl+\\) or, from inside pty-layout, use ^]n to pick a session.\n" +
          "  Pass --force to attach anyway (nested clients are usually a mistake).",
      });
      const resolvedAttachName = await resolveRef(attachName);
      await cmdAttach(resolvedAttachName, autoRestart, force);
      break;
    }

    case "exec": {
      // pty exec -- <command> [args...]
      const dashDash = args.indexOf("--", 1);
      if (dashDash === -1 || dashDash + 1 >= args.length) {
        console.error("Usage: pty exec -- <command> [args...]");
        process.exit(1);
      }
      const execCmd = args[dashDash + 1];
      const execArgs = args.slice(dashDash + 2);
      await cmdExec(execCmd, execArgs);
      break;
    }

    case "peek": {
      let follow = false;
      let plain = false;
      let full = false;
      const waitPatterns: string[] = [];
      let timeoutSec = 0;
      let pi = 1;
      while (pi < args.length && args[pi].startsWith("-")) {
        if (args[pi] === "-f" || args[pi] === "--follow") { follow = true; pi++; }
        else if (args[pi] === "--plain") { plain = true; pi++; }
        else if (args[pi] === "--full") { full = true; pi++; }
        else if (args[pi] === "--wait" && pi + 1 < args.length) { waitPatterns.push(args[pi + 1]); pi += 2; }
        else if ((args[pi] === "-t" || args[pi] === "--timeout") && pi + 1 < args.length) { timeoutSec = parseFloat(args[pi + 1]); pi += 2; }
        else break;
      }
      const peekName = args[pi];
      if (!peekName) {
        console.error("Usage: pty peek [-f] [--plain] [--full] [--wait <pattern>] [-t <seconds>] <name>");
        process.exit(1);
      }
      const resolvedPeekName = await resolveRef(peekName);
      if (waitPatterns.length > 0) {
        await cmdPeekWait(resolvedPeekName, waitPatterns, timeoutSec, plain);
      } else {
        cmdPeek(resolvedPeekName, follow, plain, full);
      }
      break;
    }

    case "send": {
      const sendName = args[1];
      if (!sendName) {
        console.error('Usage: pty send <name> "text"  or  pty send <name> --seq "text" --seq key:return');
        process.exit(1);
      }

      let sendArgs = args.slice(2);
      // --paste can appear anywhere; pull it out before the rest of the
      // parsing so its position relative to --seq / text doesn't matter.
      let paste = false;
      sendArgs = sendArgs.filter((a) => {
        if (a === "--paste") { paste = true; return false; }
        return true;
      });
      let delaySecs: number | undefined;
      if (sendArgs[0] === "--with-delay") {
        sendArgs = sendArgs.slice(1);
        const val = parseFloat(sendArgs[0]);
        if (isNaN(val) || val < 0) {
          console.error("--with-delay requires a non-negative number (seconds).");
          process.exit(1);
        }
        delaySecs = val;
        sendArgs = sendArgs.slice(1);
      }

      const hasSeq = sendArgs.includes("--seq");
      const hasPositional = sendArgs.length > 0 && !sendArgs[0].startsWith("--");

      if (hasSeq && hasPositional) {
        console.error("Cannot mix positional text with --seq flags.");
        process.exit(1);
      }

      // Common typos for "send a return key" — accepted nowhere; suggest
      // the real syntax so agents / humans don't silently lose a keystroke.
      const ENTER_TYPOS = new Set(["--enter", "--newline", "--return", "--cr"]);
      for (const a of sendArgs) {
        if (ENTER_TYPOS.has(a)) {
          console.error(`Unknown flag "${a}". Use \`--seq "<text>" --seq key:return\` to send text followed by Enter.`);
          process.exit(1);
        }
      }

      let data: string[];
      if (hasSeq) {
        data = [];
        for (let j = 0; j < sendArgs.length; j++) {
          if (sendArgs[j] === "--seq") {
            j++;
            if (j >= sendArgs.length) {
              console.error("--seq requires a value.");
              process.exit(1);
            }
            data.push(parseSeqValue(sendArgs[j]));
          } else {
            console.error(`Unexpected argument: ${sendArgs[j]}`);
            process.exit(1);
          }
        }
      } else if (hasPositional) {
        // Mirror the --seq branch's strictness: reject any trailing args
        // after the positional text so unknown flags (e.g. --enter) no
        // longer get silently dropped on the floor (closes #20).
        if (sendArgs.length > 1) {
          console.error(`Unexpected argument: ${sendArgs[1]}`);
          process.exit(1);
        }
        data = [sendArgs[0]];
      } else {
        console.error("Nothing to send.");
        process.exit(1);
      }

      const resolvedSendName = await resolveRef(sendName);
      send({
        name: resolvedSendName,
        data,
        delayMs: delaySecs != null ? delaySecs * 1000 : undefined,
        ...(paste ? { paste: true } : {}),
      });
      break;
    }

    case "events": {
      let all = false;
      let recent = false;
      let json = false;
      let waitEventType: string | null = null;
      let eventsTimeout = 0;
      let ei = 1;
      while (ei < args.length && args[ei].startsWith("-")) {
        if (args[ei] === "--all") { all = true; ei++; }
        else if (args[ei] === "--recent") { recent = true; ei++; }
        else if (args[ei] === "--json") { json = true; ei++; }
        else if (args[ei] === "--wait" && ei + 1 < args.length) { waitEventType = args[ei + 1]; ei += 2; }
        else if ((args[ei] === "-t" || args[ei] === "--timeout") && ei + 1 < args.length) { eventsTimeout = parseFloat(args[ei + 1]); ei += 2; }
        else break;
      }
      const eventsName = args[ei];

      if (!all && !eventsName) {
        console.error("Usage: pty events [--all] [--recent] [--json] [--wait <type>] [-t <seconds>] [<name>]");
        process.exit(1);
      }

      let resolvedEventsName: string | null = null;
      if (eventsName) {
        resolvedEventsName = await resolveRef(eventsName);
      }

      await cmdEvents(resolvedEventsName, { all, recent, json, waitEventType, timeout: eventsTimeout });
      break;
    }

    case "list":
    case "ls": {
      const listArgs = args.slice();
      const listFilterTags = extractFilterTags(listArgs);

      // Consume optional flag+value pairs (--status / --older-than / --newer-than)
      // in a single pass so they can appear in any order. Presence checks for
      // boolean flags happen after this loop.
      let statusFilter: "running" | "exited" | "vanished" | null = null;
      let olderThanMs: number | null = null;
      let newerThanMs: number | null = null;
      const consumed = new Set<number>();
      for (let i = 1; i < listArgs.length; i++) {
        const arg = listArgs[i];
        const val = listArgs[i + 1];
        if (arg === "--status") {
          if (val !== "running" && val !== "exited" && val !== "vanished") {
            console.error(`--status expects one of: running, exited, vanished`);
            process.exit(1);
          }
          statusFilter = val;
          consumed.add(i); consumed.add(i + 1);
          i++;
        } else if (arg === "--older-than" || arg === "--newer-than") {
          const parsed = val == null ? null : parseDuration(val);
          if (parsed == null) {
            console.error(`${arg} expects a duration like 30s, 5m, 2h, 1d`);
            process.exit(1);
          }
          if (arg === "--older-than") olderThanMs = parsed;
          else newerThanMs = parsed;
          consumed.add(i); consumed.add(i + 1);
          i++;
        }
      }
      const remainingArgs = listArgs.filter((_, i) => !consumed.has(i));

      const jsonFlag = remainingArgs.includes("--json");
      const tagsFlag = remainingArgs.includes("--tags");
      const remoteFlag = remainingArgs.includes("--remote");
      const summaryFlag = remainingArgs.includes("--summary");
      await cmdList({
        json: jsonFlag,
        showTags: tagsFlag,
        remote: remoteFlag,
        filterTags: listFilterTags,
        statusFilter,
        olderThanMs,
        newerThanMs,
        summary: summaryFlag,
      });
      break;
    }

    case "stats": {
      let statsJson = false;
      let statsAll = false;
      let statsName: string | undefined;
      for (let i = 1; i < args.length; i++) {
        if (args[i] === "--json") statsJson = true;
        else if (args[i] === "--all") statsAll = true;
        else if (!statsName) statsName = args[i];
      }
      await cmdStats(statsName, statsJson, statsAll);
      break;
    }

    case "restart": {
      // -y / --yes: skip the "kill and restart?" prompt when already running
      // --force: also skip the nesting guard (restart + attach even when
      //          already inside a pty session — nested client, caveat emptor)
      let yes = false;
      let force = false;
      let restartName: string | null = null;
      for (let ai = 1; ai < args.length; ai++) {
        const a = args[ai];
        if (a === "-y" || a === "--yes") yes = true;
        else if (a === "--force") force = true;
        else if (!restartName) restartName = a;
        else {
          console.error(`pty restart: unexpected argument "${a}"`);
          process.exit(1);
        }
      }
      if (!restartName) {
        console.error("Usage: pty restart [-y] [--force] <name>");
        process.exit(1);
      }
      const resolvedRestartName = await resolveRef(restartName);
      await cmdRestart(resolvedRestartName, yes, force);
      break;
    }

    case "kill": {
      if (args.length < 2) {
        console.error("Usage: pty kill <name>");
        process.exit(1);
      }
      const resolvedKillName = await resolveRef(args[1]);
      await cmdKill(resolvedKillName);
      break;
    }

    case "gc": {
      const gcArgs = args.slice(1);
      const dryRun = gcArgs.some((a) => a === "--dry-run" || a === "-n");
      const printPlist = gcArgs.includes("--print-launchd-plist");
      let interval = 30;
      let idleDays: number | undefined;
      for (let i = 0; i < gcArgs.length; i++) {
        const a = gcArgs[i];
        if (a === "--interval" && i + 1 < gcArgs.length) {
          const v = parseInt(gcArgs[i + 1], 10);
          if (!Number.isFinite(v) || v <= 0) {
            console.error(`pty gc: --interval expects a positive integer (got "${gcArgs[i + 1]}")`);
            process.exit(1);
          }
          interval = v;
          i++;
        } else if (a.startsWith("--interval=")) {
          const v = parseInt(a.slice("--interval=".length), 10);
          if (!Number.isFinite(v) || v <= 0) {
            console.error(`pty gc: --interval expects a positive integer (got "${a.slice("--interval=".length)}")`);
            process.exit(1);
          }
          interval = v;
        } else if (a === "--idle-days" && i + 1 < gcArgs.length) {
          const v = parseInt(gcArgs[i + 1], 10);
          if (!Number.isFinite(v) || v <= 0) {
            console.error(`pty gc: --idle-days expects a positive integer (got "${gcArgs[i + 1]}")`);
            process.exit(1);
          }
          idleDays = v;
          i++;
        } else if (a.startsWith("--idle-days=")) {
          const v = parseInt(a.slice("--idle-days=".length), 10);
          if (!Number.isFinite(v) || v <= 0) {
            console.error(`pty gc: --idle-days expects a positive integer (got "${a.slice("--idle-days=".length)}")`);
            process.exit(1);
          }
          idleDays = v;
        }
      }
      if (printPlist) {
        printLaunchdPlist(interval);
        break;
      }
      await cmdGc(dryRun, idleDays);
      break;
    }

    case "tag": {
      const tagName = args[1];
      if (!tagName) {
        console.error("Usage: pty tag <name> [key=value...] [--rm key...]");
        process.exit(1);
      }
      const resolvedTagName = await resolveRef(tagName);

      // Bulk-friendly parsing. Multiple `key=value` and `--rm <key>` may
      // appear in any order; updates apply before removals (see updateTags
      // in sessions.ts), so `pty tag X k=v --rm k` ends with k removed.
      const updates: Record<string, string> = {};
      const removals: string[] = [];
      for (let i = 2; i < args.length; i++) {
        if (args[i] === "--rm") {
          if (i + 1 >= args.length) {
            console.error("pty tag: --rm requires a key (e.g. --rm role)");
            process.exit(1);
          }
          const rmKey = args[i + 1];
          if (rmKey === "") {
            console.error("pty tag: --rm requires a non-empty key");
            process.exit(1);
          }
          removals.push(rmKey);
          i++;
          continue;
        }
        const eq = args[i].indexOf("=");
        if (eq === -1) {
          console.error(`pty tag: invalid argument "${args[i]}". Use key=value or --rm key.`);
          process.exit(1);
        }
        const key = args[i].slice(0, eq);
        if (key === "") {
          console.error(`pty tag: empty key in "${args[i]}". Tag keys must be non-empty.`);
          process.exit(1);
        }
        updates[key] = args[i].slice(eq + 1);
      }

      // No updates or removals — show current tags
      if (Object.keys(updates).length === 0 && removals.length === 0) {
        const meta = readMetadata(resolvedTagName);
        if (!meta) {
          console.error(`Session "${tagName}" not found.`);
          process.exit(1);
        }
        if (!meta.tags || Object.keys(meta.tags).length === 0) {
          console.log(`No tags on "${resolvedTagName}".`);
        } else {
          for (const [k, v] of Object.entries(meta.tags)) {
            console.log(`  ${k}=${v}`);
          }
        }
        break;
      }

      try {
        // Check if session is managed by a pty.toml before modifying
        const beforeMeta = readMetadata(resolvedTagName);
        const ptyfilePath = beforeMeta?.tags?.ptyfile;

        updateTags(resolvedTagName, updates, removals);
        const meta = readMetadata(resolvedTagName);
        if (!meta?.tags || Object.keys(meta.tags).length === 0) {
          console.log(`Tags cleared on "${resolvedTagName}".`);
        } else {
          console.log(`Tags on "${resolvedTagName}":`);
          for (const [k, v] of Object.entries(meta.tags)) {
            console.log(`  ${k}=${v}`);
          }
        }

        if (ptyfilePath) {
          console.error(`\nWarning: this session is managed by ${ptyfilePath}`);
          console.error("Running 'pty up' will sync tags from the toml and may overwrite this change.");
          console.error("To make it permanent, edit the pty.toml file directly.");
        }
      } catch (e: any) {
        console.error(e.message);
        process.exit(1);
      }
      break;
    }

    case "tag-multi": {
      await cmdTagMulti(args.slice(1));
      break;
    }

    case "emit": {
      await cmdEmit(args.slice(1));
      break;
    }

    case "state": {
      await cmdState(args.slice(1));
      break;
    }

    case "up": {
      if (args[1] === "-h" || args[1] === "--help") {
        console.log("Usage: pty up [dir] [name...]\n\nStart sessions defined in pty.toml.");
        break;
      }
      // pty up [dir] [name...]
      const upArgs = args.slice(1);
      let dir: string | undefined;
      const names: string[] = [];

      for (const arg of upArgs) {
        if (arg.startsWith("-")) break;
        if (!dir && names.length === 0 && hasPtyFile(arg)) {
          dir = arg;
        } else {
          names.push(arg);
        }
      }

      await cmdUp(dir, names);
      break;
    }

    case "down": {
      if (args[1] === "-h" || args[1] === "--help") {
        console.log("Usage: pty down [dir] [name...]\n\nStop sessions defined in pty.toml.");
        break;
      }
      // pty down [dir] [name...]
      const downArgs = args.slice(1);
      let dir: string | undefined;
      const names: string[] = [];

      for (const arg of downArgs) {
        if (arg.startsWith("-")) break;
        if (!dir && names.length === 0 && hasPtyFile(arg)) {
          dir = arg;
        } else {
          names.push(arg);
        }
      }

      await cmdDown(dir, names);
      break;
    }

    case "rename": {
      await cmdRename(args.slice(1));
      break;
    }

    case "rm":
    case "remove": {
      if (args.length < 2) {
        console.error("Usage: pty rm <name>");
        process.exit(1);
      }
      const resolvedRmName = await resolveRef(args[1]);
      await cmdRm(resolvedRmName);
      break;
    }

    case "wrap": {
      if (args[1] === "--list" || args[1] === "-l") {
        cmdWrapList();
      } else if (!args[1]) {
        console.error("Usage: pty wrap <command>");
        process.exit(1);
      } else {
        cmdWrap(args[1]);
      }
      break;
    }

    case "unwrap": {
      if (!args[1]) {
        console.error("Usage: pty unwrap <command>");
        process.exit(1);
      }
      cmdUnwrap(args[1]);
      break;
    }

    case "test": {
      await cmdTest(args.slice(1));
      break;
    }

    case "help":
    case "--help":
    case "-h": {
      usage();
      break;
    }

    default: {
      // Look for pty-<command> in PATH (like git does with git-<command>)
      const ext = `pty-${command}`;
      let extPath: string | null = null;
      try {
        extPath = execFileSync("which", [ext], { encoding: "utf8" }).trim();
      } catch {}

      if (extPath) {
        const result = spawnSync(extPath, args.slice(1), {
          stdio: "inherit",
          env: process.env,
        });
        process.exit(result.status ?? 1);
      }

      console.error(`Unknown command: ${command}`);
      usage();
      process.exit(1);
    }
  }
}

async function cmdRun(
  name: string,
  command: string,
  args: string[],
  detach = false,
  attachExisting = false,
  displayCommand: string,
  ephemeral = false,
  tags: Record<string, string> = {},
  explicitCwd: string | null = null,
  isolateEnv = false,
  displayName: string | null = null,
): Promise<void> {
  const session = await getSession(name);
  if (session?.status === "running") {
    if (attachExisting) {
      console.log(`Session "${name}" already running, attaching.`);
      doAttach(name);
      return;
    }
    console.error(
      `Session "${name}" is already running. Use "pty attach ${name}" to connect.`
    );
    process.exit(1);
  }

  if (!acquireLock(name)) {
    console.error(
      `Session "${name}" is being created by another process. Try again.`
    );
    process.exit(1);
  }

  // Clean up any dead session with the same name, but preserve cwd and tags
  // so that `run -a` re-creates the session in the original directory with original tags.
  const previousCwd = session && isGone(session.status) ? session.metadata?.cwd : undefined;
  const previousTags = session && isGone(session.status) ? session.metadata?.tags : undefined;
  if (session && isGone(session.status)) {
    cleanupAll(name);
  }

  try {
    const tagOpt = Object.keys(tags).length > 0 ? tags : previousTags;
    const cwdOpt = explicitCwd ?? previousCwd;
    // If the session had a previous displayName (e.g., it was renamed before
    // exiting), prefer preserving it over the fresh auto-generated label so
    // `pty run -a` re-creates the session feeling-identical to the last one.
    const prevDisplayName = session && isGone(session.status) ? session.metadata?.displayName : undefined;
    const displayNameOpt = displayName ?? prevDisplayName;
    await spawnDaemon({
      name, command, args, displayCommand, cwd: cwdOpt, ephemeral, tags: tagOpt,
      ...(displayNameOpt ? { displayName: displayNameOpt } : {}),
      ...(isolateEnv ? { isolateEnv: true } : {}),
    });
  } finally {
    releaseLock(name);
  }

  console.log(`Session "${name}" created.`);

  if (detach) {
    return;
  }

  doAttach(name);
}

async function cmdAttach(
  name: string,
  autoRestart = false,
  _force = false,
): Promise<void> {
  // Nesting guard runs in the dispatcher (before name resolution) so the
  // user gets the nesting hint even for typo'd refs. cmdAttach itself is
  // only reached once that check has passed; _force is retained in the
  // signature for clarity and potential future use.
  void _force;
  const session = await getSession(name);

  if (!session) {
    console.error(`Session "${name}" not found.`);
    process.exit(1);
  }

  if (session.status === "running") {
    doAttach(name);
    return;
  }

  // Dead session — show last lines and offer to restart
  await handleDeadSession(session, autoRestart);
}

async function handleDeadSession(
  session: SessionInfo,
  autoRestart = false
): Promise<void> {
  const meta = session.metadata;
  if (!meta) {
    console.error(`Session "${session.name}" exited (no metadata available).`);
    cleanupAll(session.name);
    process.exit(1);
  }

  // Show last lines
  if (meta.lastLines && meta.lastLines.length > 0) {
    console.log("");
    for (const line of meta.lastLines) {
      console.log(`  ${line}`);
    }
    console.log("");
  }

  console.log(
    `Session "${session.name}" exited with code ${meta.exitCode ?? "unknown"}.`
  );

  const cmd = [meta.displayCommand, ...(meta.args ?? [])].join(" ");
  console.log(`Command was: ${cmd}`);
  console.log("");

  if (!autoRestart) {
    const answer = await ask("Restart? [Y/n] ");
    if (answer.toLowerCase() === "n") {
      process.exit(0);
    }
  }

  // Restart
  cleanupAll(session.name);
  await spawnDaemon({ name: session.name, command: meta.command, args: meta.args, displayCommand: meta.displayCommand, cwd: meta.cwd, tags: meta.tags });
  console.log(`Session "${session.name}" restarted.`);
  doAttach(session.name);
}

function doAttach(name: string): void {
  attach({
    name,
    onDetach: () => process.exit(0),
    onExit: (code) => process.exit(code),
  });
}

async function cmdExec(command: string, cmdArgs: string[]): Promise<void> {
  const sessionName = process.env.PTY_SESSION;
  if (!sessionName) {
    console.error("pty exec: not inside a pty session (PTY_SESSION not set).");
    process.exit(1);
  }

  const meta = readMetadata(sessionName);
  if (!meta) {
    console.error(`pty exec: session "${sessionName}" metadata not found.`);
    process.exit(1);
  }

  if (meta.tags?.ptyfile) {
    console.error(`pty exec: session "${sessionName}" is managed by ${meta.tags.ptyfile}`);
    console.error("Edit the pty.toml to change the command instead.");
    process.exit(1);
  }

  // Resolve the command to an absolute path
  let resolved: string;
  try {
    resolved = resolveCommand(command);
  } catch (e: any) {
    console.error(e.message);
    process.exit(1);
  }

  // Update metadata with the new command
  const previousCommand = meta.displayCommand ?? [meta.command, ...(meta.args ?? [])].join(" ");
  const displayCommand = [command, ...cmdArgs].join(" ");
  writeMetadata(sessionName, {
    ...meta,
    command: resolved,
    args: cmdArgs,
    displayCommand,
  });

  // Emit exec event
  const writer = new EventWriter(sessionName);
  writer.append({
    session: sessionName,
    type: EventType.SESSION_EXEC,
    ts: new Date().toISOString(),
    previousCommand,
    command: displayCommand,
  } as any);
  await writer.flush();

  // Replace this process with the new command
  const result = spawnSync(resolved, cmdArgs, {
    stdio: "inherit",
    env: process.env,
  });
  process.exit(result.status ?? 1);
}

async function cmdPeekWait(name: string, patterns: string[], timeoutSec: number, plain: boolean): Promise<void> {
  const { peekScreen } = await import("./connection.ts");
  const start = Date.now();
  const timeoutMs = timeoutSec > 0 ? timeoutSec * 1000 : 0;
  const matchesAny = (text: string) => patterns.some((p) => text.includes(p));
  const patternDesc = patterns.length === 1 ? `"${patterns[0]}"` : patterns.map((p) => `"${p}"`).join(" or ");

  while (true) {
    if (timeoutMs > 0 && Date.now() - start > timeoutMs) {
      console.error(`Timed out after ${timeoutSec}s waiting for ${patternDesc}.`);
      process.exit(1);
    }

    // Try live session first
    try {
      const screen = await peekScreen({ name, plain: true });
      if (matchesAny(screen)) {
        if (plain) {
          process.stdout.write(screen + "\n");
        } else {
          const ansiScreen = await peekScreen({ name, plain: false });
          process.stdout.write(ansiScreen + "\n");
        }
        return;
      }
    } catch {
      // Session might have exited — check metadata for lastLines
      const meta = readMetadata(name);
      if (meta?.exitedAt && meta.lastLines) {
        const lastOutput = meta.lastLines.join("\n");
        if (matchesAny(lastOutput)) {
          process.stdout.write(lastOutput + "\n");
          return;
        }
        // Session exited but pattern not found in last lines
        console.error(`Session "${name}" exited (code ${meta.exitCode ?? "?"}) without matching ${patternDesc}.`);
        if (meta.lastLines.length > 0) {
          console.error("Last output:");
          for (const line of meta.lastLines) {
            console.error(`  ${line}`);
          }
        }
        process.exit(1);
      }
      // No exitedAt — might be a transient connection error, retry
    }

    await new Promise((r) => setTimeout(r, 200));
  }
}

async function cmdPeek(name: string, follow: boolean, plain: boolean, full = false): Promise<void> {
  // Dead daemon (cleanly exited or vanished) — fall back to saved lastLines.
  const session = await getSession(name);
  if (session && isGone(session.status)) {
    const meta = session.metadata;
    if (meta?.lastLines && meta.lastLines.length > 0) {
      process.stdout.write(meta.lastLines.join("\n") + "\n");
    } else {
      const verb = session.status === "vanished" ? "vanished" : "exited";
      console.error(`Session "${name}" has ${verb} with no saved output.`);
    }
    return;
  }

  peek({
    name,
    follow,
    plain,
    full,
    onDetach: () => process.exit(0),
    onExit: (code) => process.exit(code),
  });
}

interface ListOptions {
  json?: boolean;
  showTags?: boolean;
  remote?: boolean;
  filterTags?: Record<string, string>;
  statusFilter?: "running" | "exited" | "vanished" | null;
  olderThanMs?: number | null;
  newerThanMs?: number | null;
  summary?: boolean;
}

async function cmdList(opts: ListOptions = {}): Promise<void> {
  const {
    json = false,
    showTags = false,
    remote = false,
    filterTags = {},
    statusFilter = null,
    olderThanMs = null,
    newerThanMs = null,
    summary = false,
  } = opts;

  let sessions = await listSessions();
  if (Object.keys(filterTags).length > 0) {
    sessions = sessions.filter((s) => matchesAllTags(s.metadata?.tags, filterTags));
  }
  if (statusFilter) {
    sessions = sessions.filter((s) => s.status === statusFilter);
  }
  if (olderThanMs != null || newerThanMs != null) {
    const now = Date.now();
    sessions = sessions.filter((s) => {
      // Anchor age on exitedAt when available (true exit), else createdAt.
      // Running sessions have no exit yet, so use createdAt. Missing
      // metadata entirely means we can't filter on age — include by default.
      const anchor = s.metadata?.exitedAt ?? s.metadata?.createdAt;
      if (!anchor) return olderThanMs == null && newerThanMs == null;
      const ageMs = now - new Date(anchor).getTime();
      if (olderThanMs != null && ageMs < olderThanMs) return false;
      if (newerThanMs != null && ageMs > newerThanMs) return false;
      return true;
    });
  }

  // Stable display order: ASCII sort on the user-visible label (displayName
  // when set, otherwise the stable id). Without this, `fs.readdirSync` on
  // APFS returns sessions in roughly insertion order, which drifts as
  // sessions come and go and reads like accidental chaos to the eye.
  const sortKey = (s: SessionInfo): string => s.metadata?.displayName ?? s.name;
  sessions = [...sessions].sort((a, b) => {
    const ka = sortKey(a), kb = sortKey(b);
    return ka < kb ? -1 : ka > kb ? 1 : 0;
  });

  // Fetch relay hosts if --remote
  let remoteHosts: {
    label: string;
    sessions: {
      name: string;
      status: string;
      command?: string;
      cwd?: string;
      tags?: Record<string, string>;
      displayName?: string;
    }[];
    error: string | null;
  }[] = [];
  if (remote) {
    try {
      const relayBin = execFileSync("which", ["pty-relay"], { encoding: "utf-8" }).trim();
      const result = spawnSync(relayBin, ["ls", "--json"], { encoding: "utf-8", timeout: 5000 });
      if (result.status === 0 && result.stdout.trim()) {
        remoteHosts = JSON.parse(result.stdout);
      }
    } catch {}
  }

  // Build the summary payload — used by both --json --summary and the
  // human-facing --summary rendering below. Oldest/newest are picked from
  // the filtered `sessions` so they match whatever the caller narrowed to.
  const buildSummary = () => {
    const byStatus: Record<string, number> = {
      running: 0, exited: 0, vanished: 0,
    };
    for (const s of sessions) byStatus[s.status] = (byStatus[s.status] ?? 0) + 1;

    let oldest: SessionInfo | null = null;
    let newest: SessionInfo | null = null;
    let oldestTs = Infinity;
    let newestTs = -Infinity;
    for (const s of sessions) {
      const anchor = s.metadata?.createdAt;
      if (!anchor) continue;
      const ts = new Date(anchor).getTime();
      if (ts < oldestTs) { oldestTs = ts; oldest = s; }
      if (ts > newestTs) { newestTs = ts; newest = s; }
    }
    const now = Date.now();
    const pickEndpoint = (s: SessionInfo | null, ts: number) => {
      if (!s) return null;
      return {
        name: s.name,
        status: s.status,
        ageSeconds: Math.max(0, Math.floor((now - ts) / 1000)),
        ...(s.metadata?.displayName ? { displayName: s.metadata.displayName } : {}),
      };
    };
    return {
      total: sessions.length,
      byStatus,
      oldest: pickEndpoint(oldest, oldestTs),
      newest: pickEndpoint(newest, newestTs),
    };
  };

  if (json) {
    if (summary) {
      console.log(JSON.stringify(buildSummary()));
      return;
    }
    const localOutput = sessions.map((s) => ({
      name: s.name,
      status: s.status,
      pid: s.pid,
      command: s.metadata
        ? s.metadata.displayCommand
        : null,
      cwd: s.metadata?.cwd ?? null,
      createdAt: s.metadata?.createdAt ?? null,
      exitCode: s.metadata?.exitCode ?? null,
      exitedAt: s.metadata?.exitedAt ?? null,
      ...(s.metadata?.tags ? { tags: s.metadata.tags } : {}),
      ...(s.metadata?.displayName ? { displayName: s.metadata.displayName } : {}),
    }));
    if (remote && remoteHosts.length > 0) {
      console.log(JSON.stringify({ local: localOutput, remote: remoteHosts }));
    } else {
      console.log(JSON.stringify(localOutput));
    }
    return;
  }

  if (summary) {
    const s = buildSummary();
    if (s.total === 0) {
      console.log("No matching sessions.");
      return;
    }
    const parts: string[] = [];
    if (s.byStatus.running) parts.push(`${s.byStatus.running} running`);
    if (s.byStatus.exited) parts.push(`${s.byStatus.exited} exited`);
    if (s.byStatus.vanished) parts.push(`${s.byStatus.vanished} vanished`);
    console.log(`${s.total} session${s.total === 1 ? "" : "s"} — ${parts.join(", ")}`);
    const label = (e: { name: string; status: string; displayName?: string }) =>
      e.displayName ? `${e.displayName} (${e.name})` : e.name;
    if (s.oldest) {
      console.log(`oldest: ${label(s.oldest)} (${s.oldest.status}, ${formatDuration(s.oldest.ageSeconds * 1000)})`);
    }
    if (s.newest && (!s.oldest || s.newest.name !== s.oldest.name)) {
      console.log(`newest: ${label(s.newest)} (${s.newest.status}, ${formatDuration(s.newest.ageSeconds * 1000)})`);
    }
    return;
  }

  if (sessions.length === 0 && remoteHosts.length === 0) {
    console.log("No active sessions.");
    return;
  }

  const running = sessions.filter((s) => s.status === "running");
  const exited = sessions.filter((s) => s.status === "exited");
  const vanished = sessions.filter((s) => s.status === "vanished");

  // Render tags as hashtags. When `showAll` is false, hide reserved keys
  // (pty-internal bookkeeping like `ptyfile*`/`strategy`, plus any key
  // starting with `:` which is the tool-owned-tag convention — e.g.,
  // pty-layout's `:l<pid>-<rand>` view membership markers). `--tags`
  // (showAll=true) shows everything.
  const renderTags = (tags: Record<string, string> | undefined, showAll: boolean): string => {
    if (!tags) return "";
    const entries = Object.entries(tags).filter(([k]) => showAll || !isReservedTagKey(k));
    return entries.length > 0 ? " " + entries.map(([k, v]) => `#${k}=${v}`).join(" ") : "";
  };

  // Render the session's primary label. If displayName is set, it appears
  // first with the stable id in parens for disambiguation; otherwise just
  // the id. Users can match either in any CLI command.
  const renderLabel = (session: SessionInfo, boldCode: string): string => {
    const dn = session.metadata?.displayName;
    if (dn) {
      return `${boldCode}${dn}\x1b[0m \x1b[2m(${session.name})\x1b[0m`;
    }
    return `${boldCode}${session.name}\x1b[0m`;
  };

  if (running.length > 0) {
    console.log("Active sessions:");
    for (const session of running) {
      const cmd = session.metadata
        ? session.metadata.displayCommand
        : "unknown";
      const cwd = session.metadata?.cwd
        ? shortPath(session.metadata.cwd)
        : "";
      const tagStr = renderTags(session.metadata?.tags, showTags);
      const marker = strategyMarker(session.metadata?.tags);
      const label = renderLabel(session, "\x1b[1;36m");
      console.log(`  ${label}${marker}${tagStr} (pid: ${session.pid}) — ${cwd} — \x1b[2m${cmd}\x1b[0m`);
    }
  }

  if (exited.length > 0) {
    if (running.length > 0) console.log("");
    console.log("Exited sessions:");
    for (const session of exited) {
      const meta = session.metadata;
      const code = meta?.exitCode ?? "?";
      const ago = meta?.exitedAt ? timeAgo(new Date(meta.exitedAt)) : "unknown";
      const cwd = meta?.cwd ? shortPath(meta.cwd) : "";
      const cmd = meta
        ? meta.displayCommand
        : "";
      const tagStr = renderTags(meta?.tags, showTags);
      const marker = strategyMarker(meta?.tags);
      const label = renderLabel(session, "\x1b[1m");
      console.log(`  ${label}${marker}${tagStr} (exited with code ${code}, ${ago}) — ${cwd} — \x1b[2m${cmd}\x1b[0m`);
    }
  }

  if (vanished.length > 0) {
    if (running.length > 0 || exited.length > 0) console.log("");
    // Dim-yellow header because vanished is a warning state — daemon was
    // killed (SIGKILL / OOM / crash) without writing an exit record, so we
    // can't say *why* it's gone, only that the socket stopped responding.
    // `pty gc` cleans these up alongside normal exits.
    console.log("\x1b[33mVanished sessions (no exit record — killed or crashed):\x1b[0m");
    for (const session of vanished) {
      const meta = session.metadata;
      const ago = meta?.createdAt ? timeAgo(new Date(meta.createdAt)) : "unknown";
      const cwd = meta?.cwd ? shortPath(meta.cwd) : "";
      const cmd = meta ? meta.displayCommand : "";
      const tagStr = renderTags(meta?.tags, showTags);
      const marker = strategyMarker(meta?.tags);
      const label = renderLabel(session, "\x1b[1;33m");
      console.log(`  \u26a0 ${label}${marker}${tagStr} (vanished, started ${ago}) — ${cwd} — \x1b[2m${cmd}\x1b[0m`);
    }
  }

  // Remote hosts — render with the same treatment as local: displayName
  // in parens after the id, strategy marker, user-facing tags inline.
  for (const host of remoteHosts) {
    console.log("");
    if (host.error) {
      console.log(`\x1b[1m${host.label}\x1b[0m \x1b[31m(error: ${host.error})\x1b[0m`);
      continue;
    }
    console.log(`\x1b[1m${host.label}\x1b[0m (${host.sessions.length} sessions):`);
    const sortedRemote = [...host.sessions].sort((a, b) => {
      const ka = a.displayName ?? a.name;
      const kb = b.displayName ?? b.name;
      return ka < kb ? -1 : ka > kb ? 1 : 0;
    });
    for (const s of sortedRemote) {
      const icon = s.status === "running" ? "\u25cf" : "\u25cb";
      const cwd = s.cwd ? shortPath(s.cwd) : "";
      const cmd = s.command ?? "";
      const labelBase = s.displayName
        ? `\x1b[1;36m${s.displayName}\x1b[0m \x1b[2m(${s.name})\x1b[0m`
        : `\x1b[1;36m${s.name}\x1b[0m`;
      const tagStr = renderTags(s.tags, showTags);
      const marker = strategyMarker(s.tags);
      console.log(`  ${icon} ${labelBase}${marker}${tagStr} — ${cwd} — \x1b[2m${cmd}\x1b[0m`);
    }
  }
}

async function cmdStats(
  name?: string,
  json = false,
  all = false,
): Promise<void> {
  if (name) {
    const session = await getSession(name);
    if (!session) {
      console.error(`Session "${name}" not found.`);
      process.exit(1);
    }
    if (isGone(session.status)) {
      if (json) {
        console.log(JSON.stringify({
          name: session.name,
          status: session.status,
          exitCode: session.metadata?.exitCode ?? null,
          exitedAt: session.metadata?.exitedAt ?? null,
          ...(session.metadata?.tags ? { tags: session.metadata.tags } : {}),
        }));
      } else if (session.status === "vanished") {
        console.log(`Session "${name}" has vanished (no exit record — killed or crashed).`);
      } else {
        const code = session.metadata?.exitCode ?? "?";
        console.log(`Session "${name}" has exited (code ${code}).`);
      }
      return;
    }

    try {
      // Use the resolved stable `name` so queryStats connects to the right
      // socket when the caller passed a `displayName`.
      const stats = await queryStats(session.name);
      if (json) {
        console.log(JSON.stringify(stats));
      } else {
        printStats(stats, session.metadata);
      }
    } catch (e: any) {
      console.error(e.message);
      process.exit(1);
    }
    return;
  }

  // All sessions
  const sessions = await listSessions();
  const running = sessions.filter((s) => s.status === "running");
  const gone = sessions.filter((s) => isGone(s.status));

  if (running.length === 0 && (!all || gone.length === 0)) {
    console.log("No running sessions.");
    return;
  }

  // Query all running sessions in parallel
  const results = await Promise.all(
    running.map(async (s) => {
      try {
        const stats = await queryStats(s.name);
        return { session: s, stats, error: null as string | null };
      } catch (e: any) {
        return { session: s, stats: null as StatsResult | null, error: e.message as string };
      }
    }),
  );

  if (json) {
    const output = [
      ...results.map((r) => r.stats ?? { name: r.session.name, error: r.error }),
      ...(all
        ? gone.map((s) => ({
            name: s.name,
            status: s.status,
            exitCode: s.metadata?.exitCode ?? null,
            exitedAt: s.metadata?.exitedAt ?? null,
            ...(s.metadata?.tags ? { tags: s.metadata.tags } : {}),
          }))
        : []),
    ];
    console.log(JSON.stringify(output));
    return;
  }

  for (let i = 0; i < results.length; i++) {
    const r = results[i];
    if (r.stats) {
      printStats(r.stats, r.session.metadata);
    } else {
      console.log(`Session: ${r.session.name}`);
      console.log(`  Error: ${r.error}`);
    }
    if (i < results.length - 1) console.log("");
  }

  if (all && gone.length > 0) {
    if (results.length > 0) console.log("");
    const exited = gone.filter((s) => s.status === "exited");
    const vanished = gone.filter((s) => s.status === "vanished");
    if (exited.length > 0) {
      console.log("Exited sessions:");
      for (const s of exited) {
        const code = s.metadata?.exitCode ?? "?";
        const ago = s.metadata?.exitedAt ? timeAgo(new Date(s.metadata.exitedAt)) : "unknown";
        console.log(`  ${s.name} (exited with code ${code}, ${ago})`);
      }
    }
    if (vanished.length > 0) {
      if (exited.length > 0) console.log("");
      console.log("Vanished sessions (no exit record):");
      for (const s of vanished) {
        const ago = s.metadata?.createdAt ? timeAgo(new Date(s.metadata.createdAt)) : "unknown";
        console.log(`  \u26a0 ${s.name} (started ${ago})`);
      }
    }
  }
}

function printStats(stats: StatsResult, meta: SessionInfo["metadata"]): void {
  const cmd = meta
    ? meta.displayCommand
    : "unknown";
  const cwd = meta?.cwd ? shortPath(meta.cwd) : "unknown";

  console.log(`Session: ${stats.name}`);
  console.log(`  Command:    ${cmd}`);
  console.log(`  CWD:        ${cwd}`);
  console.log(`  Uptime:     ${formatUptime(stats.uptimeSeconds)}`);
  const pidSuffix = stats.process?.pid ? ` (pid ${stats.process.pid})` : "";
  console.log(`  Process:    ${stats.process.alive ? "running" : `exited (code ${stats.process.exitCode})`}${pidSuffix}`);
  if (stats.process?.resources) {
    console.log(`  CPU:        ${stats.process.resources.cpuPercent.toFixed(1)}%`);
    console.log(`  Memory:     ${formatMemory(stats.process.resources.rssKb)}`);
  }
  if (stats.daemon) {
    console.log(`  Daemon:     pid ${stats.daemon.pid}${stats.daemon.resources ? `, ${formatMemory(stats.daemon.resources.rssKb)}` : ""}`);
  }
  console.log(`  Terminal:   ${stats.terminal.cols}x${stats.terminal.rows}`);
  console.log(`  Cursor:     row ${stats.terminal.cursorY}, col ${stats.terminal.cursorX}`);
  console.log(`  Scrollback: ${stats.terminal.scrollbackUsed} / ${stats.terminal.scrollbackCapacity} lines`);
  console.log(`  Clients:    ${stats.clients.total} (${stats.clients.attached} attached, ${stats.clients.readOnly} readonly)`);

  const modes: string[] = [];
  if (stats.modes.sgrMouse) modes.push("SGR mouse");
  if (stats.modes.cursorHidden) modes.push("cursor hidden");
  if (stats.modes.kittyKeyboard) modes.push(`kitty keyboard (flags: ${stats.modes.kittyKeyboardFlags.join(",")})`);
  console.log(`  Modes:      ${modes.length > 0 ? modes.join(", ") : "none"}`);
}

function formatMemory(rssKb: number): string {
  if (rssKb < 1024) return `${rssKb} KB`;
  const mb = rssKb / 1024;
  if (mb < 1024) return `${mb.toFixed(1)} MB`;
  const gb = mb / 1024;
  return `${gb.toFixed(2)} GB`;
}

function formatUptime(seconds: number | null): string {
  if (seconds == null) return "unknown";
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  if (m < 60) return `${m}m ${s}s`;
  const h = Math.floor(m / 60);
  const rm = m % 60;
  if (h < 24) return `${h}h ${rm}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}

async function cmdKill(name: string): Promise<void> {
  const session = await getSession(name);

  if (!session) {
    console.error(`Session "${name}" not found.`);
    process.exit(1);
  }

  if (session.status !== "running" || !session.pid) {
    console.error(`Session "${name}" is not running. Use "pty rm ${name}" to remove it.`);
    process.exit(1);
  }

  // Strip the `strategy` tag so `pty gc` doesn't respawn the session on
  // its next tick. The `supervisor.status` tag is no longer a thing.
  const wasPermanent = session.metadata?.tags?.strategy === "permanent";
  if (wasPermanent) {
    try {
      updateTags(name, {}, ["strategy"]);
    } catch {}
  }

  try {
    process.kill(session.pid, "SIGTERM");
    console.log(`Session "${name}" killed.`);
  } catch {
    console.error(`Failed to kill session "${name}".`);
  }
  cleanupSocket(name);

  if (wasPermanent && session.metadata?.tags?.ptyfile) {
    console.error(`Note: this session is managed by ${session.metadata.tags.ptyfile}`);
    console.error("The strategy tag will be restored on the next 'pty up'.");
  }
}

function renameUsage(): void {
  console.error(
    "Usage:\n" +
    "  pty rename <new-display-name>         Inside a session: set displayName on the current session\n" +
    "  pty rename <ref> <new-display-name>   Outside: set displayName on <ref>\n" +
    "  pty rename --show <ref>               Show the current displayName for <ref>\n" +
    "  pty rename --clear                    Inside a session: clear displayName\n" +
    "  pty rename --clear <ref>              Outside: clear displayName on <ref>\n" +
    "\n" +
    "displayName is a mutable alias. The session's stable id (name) never changes."
  );
}

async function cmdRename(rawArgs: string[]): Promise<void> {
  const insideSession = !!process.env.PTY_SESSION;

  // Parse flags
  let show = false;
  let clear = false;
  const positional: string[] = [];
  for (const a of rawArgs) {
    if (a === "--show") show = true;
    else if (a === "--clear") clear = true;
    else if (a === "-h" || a === "--help") { renameUsage(); return; }
    else positional.push(a);
  }

  // --show <ref>
  if (show) {
    if (positional.length !== 1) {
      console.error("pty rename --show requires exactly one ref.");
      renameUsage();
      process.exit(1);
    }
    const session = await getSession(positional[0]);
    if (!session) {
      console.error(`Session "${positional[0]}" not found.`);
      process.exit(1);
    }
    const dn = session.metadata?.displayName;
    if (dn) {
      console.log(dn);
    } else {
      console.log(`(no displayName; session is referenced by its id: ${session.name})`);
    }
    return;
  }

  // --clear
  if (clear) {
    let targetName: string;
    if (positional.length === 0) {
      if (!insideSession) {
        console.error("pty rename --clear with no ref requires being inside a pty session (PTY_SESSION not set).");
        renameUsage();
        process.exit(1);
      }
      targetName = process.env.PTY_SESSION!;
    } else if (positional.length === 1) {
      const session = await getSession(positional[0]);
      if (!session) {
        console.error(`Session "${positional[0]}" not found.`);
        process.exit(1);
      }
      targetName = session.name;
    } else {
      console.error("pty rename --clear takes at most one ref.");
      renameUsage();
      process.exit(1);
    }
    try {
      setDisplayName(targetName, null);
      console.log(`Cleared displayName on "${targetName}".`);
    } catch (e: any) {
      console.error(e.message);
      process.exit(1);
    }
    return;
  }

  // Set form — resolve target and new display
  let targetName: string;
  let newDisplay: string;
  if (positional.length === 1) {
    if (!insideSession) {
      console.error("pty rename with a single arg is only allowed inside a pty session.");
      console.error("Outside, use: pty rename <ref> <new-display-name>");
      renameUsage();
      process.exit(1);
    }
    targetName = process.env.PTY_SESSION!;
    newDisplay = positional[0];
  } else if (positional.length === 2) {
    const session = await getSession(positional[0]);
    if (!session) {
      console.error(`Session "${positional[0]}" not found.`);
      process.exit(1);
    }
    targetName = session.name;
    newDisplay = positional[1];
  } else {
    renameUsage();
    process.exit(1);
  }

  // Display names can be arbitrary printable text (spaces, punctuation, any
  // length up to 500 chars). The on-disk id (`name`) carries the strict
  // charset and the sock-path-length constraint; display names don't.
  try {
    validateDisplayName(newDisplay);
  } catch (e: any) {
    console.error(`Invalid displayName: ${e.message}`);
    process.exit(1);
  }

  // Uniqueness across (name ∪ displayName), excluding the target session itself.
  const refs = await allRefs();
  const currentDn = (await getSession(targetName))?.metadata?.displayName;
  if (newDisplay === targetName) {
    console.error(`displayName cannot equal the session's id ("${targetName}").`);
    process.exit(1);
  }
  if (refs.has(newDisplay) && newDisplay !== currentDn) {
    console.error(`"${newDisplay}" is already in use by another session (as a name or displayName).`);
    process.exit(1);
  }

  try {
    setDisplayName(targetName, newDisplay);
    console.log(`Set displayName on "${targetName}" → "${newDisplay}".`);
  } catch (e: any) {
    console.error(e.message);
    process.exit(1);
  }
}

async function cmdRm(name: string): Promise<void> {
  const session = await getSession(name);

  if (!session) {
    console.error(`Session "${name}" not found.`);
    process.exit(1);
  }

  if (session.status === "running") {
    console.error(`Session "${name}" is still running. Use "pty kill ${name}" first.`);
    process.exit(1);
  }

  cleanupAll(name);
  console.log(`Session "${name}" removed.`);
}

async function cmdGc(dryRun: boolean, idleDays?: number): Promise<void> {
  const result = await gc({ dryRun, idleDays });
  const prunedTags = await pruneOrphanLayoutTags({ dryRun });

  const killedVerb = dryRun ? "Would kill orphan child" : "Killed orphan child";
  const abandonVerb = dryRun ? "Would abandon" : "Abandoned";
  const respawnVerb = dryRun ? "Would respawn" : "Respawned";
  const removeVerb = dryRun ? "Would remove" : "Removed";
  const prunedVerb = dryRun ? "Would prune" : "Pruned";

  for (const k of result.killedOrphanChildren) {
    console.log(`${killedVerb}: ${k.name} (parent ${k.parent} ${k.reason})`);
  }
  for (const a of result.abandoned) {
    const detail = a.reason === "idle" && a.idleDays !== undefined
      ? `idle ${a.idleDays}d`
      : a.reason;
    console.log(`${abandonVerb}: ${a.name} (${detail})`);
  }
  for (const r of result.respawned) {
    const note = r.ptyfileReread ? " (pty.toml re-read)" : "";
    console.log(`${respawnVerb}: ${r.name}${note}`);
  }
  for (const f of result.respawnFailed) {
    console.log(`Respawn failed: ${f.name} — ${f.error}`);
  }
  for (const name of result.removed) {
    console.log(`${removeVerb}: ${name}`);
  }
  for (const { name, removedKeys } of prunedTags) {
    console.log(
      `${prunedVerb} orphan tags on ${name}: ${removedKeys.map((k) => `#${k}`).join(" ")}`,
    );
  }

  const totalTags = prunedTags.reduce((sum, r) => sum + r.removedKeys.length, 0);
  const totalActions =
    result.killedOrphanChildren.length +
    result.abandoned.length +
    result.respawned.length +
    result.respawnFailed.length +
    result.removed.length +
    totalTags;

  if (totalActions === 0) {
    console.log(dryRun ? "Nothing would be cleaned up." : "Nothing to clean up.");
    return;
  }

  const parts: string[] = [];
  if (result.killedOrphanChildren.length > 0) {
    parts.push(`${result.killedOrphanChildren.length} orphan child${result.killedOrphanChildren.length === 1 ? "" : "ren"}`);
  }
  if (result.abandoned.length > 0) {
    parts.push(`${result.abandoned.length} abandoned`);
  }
  if (result.respawned.length > 0) {
    parts.push(`${result.respawned.length} respawn${result.respawned.length === 1 ? "" : "s"}`);
  }
  if (result.respawnFailed.length > 0) {
    parts.push(`${result.respawnFailed.length} respawn failure${result.respawnFailed.length === 1 ? "" : "s"}`);
  }
  if (result.removed.length > 0) {
    parts.push(`${result.removed.length} stale session${result.removed.length === 1 ? "" : "s"}`);
  }
  if (totalTags > 0) {
    parts.push(`${totalTags} orphan tag${totalTags === 1 ? "" : "s"}`);
  }
  console.log(
    dryRun
      ? `Would clean up ${parts.join(", ")}. (Dry run — no changes made.)`
      : `Cleaned up ${parts.join(", ")}.`,
  );
}

/** Print a minimal launchd plist that runs `pty gc` every `interval`
 *  seconds. Pure stdout — the caller redirects to
 *  `~/Library/LaunchAgents/com.myobie.pty.gc.plist` and `launchctl load`s
 *  it themselves. No FDA dance, no bundled binary; just node + the gc
 *  command, inheriting PATH and PTY_SESSION_DIR. If the SSD where node
 *  lives isn't mounted at boot, the invocation fails — the next tick
 *  tries again. That's the whole point. */
/** Launchd `Label` values are reverse-DNS strings; the docs require
 *  each service to have a unique label. When emitting per-network gc
 *  plists we suffix the label with the root's basename so N networks
 *  install N distinct services. Non-URL-safe characters (spaces,
 *  slashes, etc.) collapse to a single hyphen so a pathological root
 *  name can't produce a plist launchd rejects. */
function labelBasenameFromRoot(sessionDir: string): string {
  return path
    .basename(sessionDir)
    .replace(/[^A-Za-z0-9._-]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

function printLaunchdPlist(interval: number): void {
  const sessionDir = getSessionDir();
  const isDefault = sessionDir === DEFAULT_SESSION_DIR;
  // Default root keeps the pre-Phase-2 label untouched so existing
  // installs (`launchctl load ... com.myobie.pty.gc.plist`) survive
  // an upgrade. Non-default roots get a suffixed label so two networks
  // can each install their own plist without a `Label` collision.
  const suffix = isDefault ? "" : `.${labelBasenameFromRoot(sessionDir)}`;
  const label = `com.myobie.pty.gc${suffix}`;
  // Per-root log so a network's gc noise stays inside its own registry.
  const logPath = path.join(sessionDir, "gc.log");
  const ptyBin = process.argv[1];
  const envPath = process.env.PATH ?? "/usr/bin:/bin:/usr/sbin:/sbin";
  const escape = (s: string) =>
    s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

  // We point ProgramArguments at node + the resolved CLI script so the
  // plist doesn't depend on the `pty` shim staying on PATH at launchd's
  // (minimal) shell. EnvironmentVariables still carries PATH so the
  // spawned children (and any `which` inside pty itself) find the user's
  // tools. PTY_ROOT (canonical) pins the target registry.
  const plist = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${escape(label)}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${escape(process.execPath)}</string>
    <string>${escape(ptyBin)}</string>
    <string>gc</string>
  </array>
  <key>StartInterval</key>
  <integer>${interval}</integer>
  <key>RunAtLoad</key>
  <true/>
  <key>StandardOutPath</key>
  <string>${escape(logPath)}</string>
  <key>StandardErrorPath</key>
  <string>${escape(logPath)}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>${escape(envPath)}</string>
    <key>PTY_ROOT</key>
    <string>${escape(sessionDir)}</string>
  </dict>
</dict>
</plist>
`;
  process.stdout.write(plist);
}

// --- tag-multi: read/write tags across multiple sessions in one call ---
//
// Three selectors (mutually exclusive):
//   <name>...               explicit list (resolved up-front; any unresolvable
//                           name aborts before any write)
//   --filter-tag k=v ...    sessions whose tags match all listed pairs
//   --all                   every session
//
// No ops = read mode (per-session tag dump, text or --json). Any ops
// (`k=v` / `--rm k`) = write mode; --all + write mode requires --yes
// because it operates on every session in the dir.

interface TagMultiParsed {
  selector:
    | { kind: "names"; names: string[] }
    | { kind: "filter"; filterTags: Record<string, string> }
    | { kind: "all" };
  ops: { updates: Record<string, string>; removals: string[] };
  json: boolean;
  yes: boolean;
}

function parseTagMultiArgs(argv: string[]): TagMultiParsed {
  let all = false;
  const filterTags: Record<string, string> = {};
  const names: string[] = [];
  const updates: Record<string, string> = {};
  const removals: string[] = [];
  let json = false;
  let yes = false;

  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--all") { all = true; continue; }
    if (a === "--json") { json = true; continue; }
    if (a === "--yes" || a === "-y") { yes = true; continue; }
    if (a === "-h" || a === "--help") {
      printTagMultiHelp();
      process.exit(0);
    }
    if (a === "--filter-tag") {
      if (i + 1 >= argv.length) {
        console.error("pty tag-multi: --filter-tag requires k=v");
        process.exit(1);
      }
      const next = argv[++i];
      const eq = next.indexOf("=");
      if (eq === -1) {
        console.error(`pty tag-multi: --filter-tag value "${next}" must be k=v`);
        process.exit(1);
      }
      const k = next.slice(0, eq);
      if (k === "") {
        console.error("pty tag-multi: --filter-tag key must be non-empty");
        process.exit(1);
      }
      filterTags[k] = next.slice(eq + 1);
      continue;
    }
    if (a === "--rm") {
      if (i + 1 >= argv.length) {
        console.error("pty tag-multi: --rm requires a key (e.g. --rm role)");
        process.exit(1);
      }
      const k = argv[++i];
      if (k === "") {
        console.error("pty tag-multi: --rm requires a non-empty key");
        process.exit(1);
      }
      removals.push(k);
      continue;
    }
    const eq = a.indexOf("=");
    if (eq !== -1) {
      const k = a.slice(0, eq);
      if (k === "") {
        console.error(`pty tag-multi: empty key in "${a}". Tag keys must be non-empty.`);
        process.exit(1);
      }
      updates[k] = a.slice(eq + 1);
      continue;
    }
    // Anything else is a positional session name. validateName will catch
    // illegal characters at resolution time.
    names.push(a);
  }

  // Selector mutex check.
  const selectorCount =
    (all ? 1 : 0) +
    (Object.keys(filterTags).length > 0 ? 1 : 0) +
    (names.length > 0 ? 1 : 0);
  if (selectorCount === 0) {
    console.error(
      "pty tag-multi: no selector — pass session names, --filter-tag k=v, or --all",
    );
    process.exit(1);
  }
  if (selectorCount > 1) {
    console.error(
      "pty tag-multi: selectors are mutually exclusive — pick one of <names>, --filter-tag, --all",
    );
    process.exit(1);
  }

  let selector: TagMultiParsed["selector"];
  if (all) selector = { kind: "all" };
  else if (Object.keys(filterTags).length > 0) selector = { kind: "filter", filterTags };
  else selector = { kind: "names", names };

  return { selector, ops: { updates, removals }, json, yes };
}

async function cmdTagMulti(argv: string[]): Promise<void> {
  const parsed = parseTagMultiArgs(argv);
  const isWrite =
    Object.keys(parsed.ops.updates).length > 0 || parsed.ops.removals.length > 0;

  // Resolve selector → list of stable session ids. Explicit names are
  // resolved up-front so an unresolvable name aborts before any writes.
  let targets: string[];
  if (parsed.selector.kind === "names") {
    targets = [];
    for (const ref of parsed.selector.names) {
      const sess = await getSession(ref);
      if (!sess) {
        console.error(`pty tag-multi: session "${ref}" not found.`);
        process.exit(1);
      }
      targets.push(sess.name);
    }
  } else if (parsed.selector.kind === "filter") {
    const all = await listSessions();
    targets = all
      .filter((s) => matchesAllTags(s.metadata?.tags, parsed.selector.kind === "filter" ? parsed.selector.filterTags : {}))
      .map((s) => s.name);
  } else {
    // --all
    if (isWrite && !parsed.yes) {
      const all = await listSessions();
      console.error(
        `pty tag-multi: --all writes are destructive across ${all.length} session(s). Re-run with --yes to apply.`,
      );
      process.exit(1);
    }
    const all = await listSessions();
    targets = all.map((s) => s.name);
  }

  // Read mode: collect per-session tags into an object.
  if (!isWrite) {
    const out: Record<string, Record<string, string>> = {};
    for (const name of targets) {
      const meta = readMetadata(name);
      out[name] = meta?.tags ?? {};
    }
    if (parsed.json) {
      console.log(JSON.stringify(out));
      return;
    }
    if (targets.length === 0) {
      console.log("0 sessions matched.");
      return;
    }
    for (const name of targets) {
      const tags = out[name];
      const keys = Object.keys(tags);
      if (keys.length === 0) {
        console.log(`${name}: (no tags)`);
        continue;
      }
      console.log(`${name}:`);
      for (const k of keys) console.log(`  ${k}=${tags[k]}`);
    }
    return;
  }

  // Write mode: apply per-session, continue on error, exit 1 if any failed.
  if (targets.length === 0) {
    if (parsed.json) {
      console.log(JSON.stringify({}));
    } else {
      console.log("0 sessions matched. No writes performed.");
    }
    return;
  }
  const results: Record<string, Record<string, string>> = {};
  const errors: { name: string; message: string }[] = [];
  for (const name of targets) {
    try {
      updateTags(name, parsed.ops.updates, parsed.ops.removals);
      const meta = readMetadata(name);
      results[name] = meta?.tags ?? {};
    } catch (e: any) {
      errors.push({ name, message: e.message ?? String(e) });
    }
  }
  if (parsed.json) {
    console.log(JSON.stringify(results));
  } else {
    const changed = Object.keys(results).length;
    console.log(`${changed} session(s) processed.`);
  }
  if (errors.length > 0) {
    for (const e of errors) {
      console.error(`pty tag-multi: ${e.name}: ${e.message}`);
    }
    process.exit(1);
  }
}

function printTagMultiHelp(): void {
  console.log(`Usage:
  pty tag-multi <selector> [--json] [--yes] [<ops>...]

Selectors (pick one):
  <name>...                explicit list of session names or displayNames
  --filter-tag k=v         sessions matching tag (repeatable for AND)
  --all                    every session

Operations (presence flips command into write mode):
  k=v                      set tag k to v
  --rm k                   remove tag k

Flags:
  --json                   structured output (object: name → tags)
  --yes / -y               required with --all when ops are present

Examples:
  pty tag-multi --all --json
  pty tag-multi --filter-tag role=web env=prod
  pty tag-multi sess-a sess-b --rm temp-flag
  pty tag-multi --all --yes audit=2026-04-25`);
}

// --- emit: publish a user.* event to a session's events log ---

async function cmdEmit(argv: string[]): Promise<void> {
  // pty emit <type> [--json <payload>] [--text <string>]
  // pty emit <ref> <type> [--json <payload>] [--text <string>]
  // Inside a session, $PTY_SESSION provides the default ref.
  let ref: string | null = null;
  let type: string | null = null;
  let jsonStr: string | null = null;
  let textStr: string | null = null;
  const positional: string[] = [];

  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--json" && i + 1 < argv.length) { jsonStr = argv[++i]; continue; }
    if (a === "--text" && i + 1 < argv.length) { textStr = argv[++i]; continue; }
    if (a === "-h" || a === "--help") {
      printEmitHelp();
      return;
    }
    positional.push(a);
  }

  if (positional.length === 2) { ref = positional[0]; type = positional[1]; }
  else if (positional.length === 1) { ref = null; type = positional[0]; }
  else {
    printEmitHelp();
    process.exit(1);
  }

  // Default to $PTY_SESSION when no explicit ref given.
  if (ref == null) ref = process.env.PTY_SESSION ?? null;
  if (!ref) {
    console.error("pty emit: no session ref given and not running inside a pty session");
    console.error("  tip: run inside a pty session, or: pty emit <session-ref> <type>");
    process.exit(1);
  }

  const resolvedName = await resolveRef(ref);

  let data: unknown;
  if (jsonStr != null) {
    try { data = JSON.parse(jsonStr); }
    catch (e: any) {
      console.error(`pty emit: --json payload is not valid JSON: ${e.message}`);
      process.exit(1);
    }
  }

  try {
    const event = await emitUserEvent(resolvedName, type!, {
      ...(data !== undefined ? { data } : {}),
      ...(textStr != null ? { text: textStr } : {}),
    });
    // Silent by default; -v isn't implemented. Return the type so scripts
    // can chain `pty emit ... | ...` if they want.
    void event;
  } catch (e: any) {
    console.error(e.message);
    process.exit(1);
  }
}

function printEmitHelp(): void {
  console.log(`Usage:
  pty emit <type> [--json <payload>] [--text <string>]
  pty emit <session-ref> <type> [--json <payload>] [--text <string>]

Publishes a user.* event to a session's events log. Inside a pty
session, the ref defaults to $PTY_SESSION. Event types must start
with "user." — "session_*", "state.*", "bell", etc. are reserved.

Examples:
  pty emit user.build-done
  pty emit user.progress --json '{"pct": 40}'
  pty emit user.note --text "starting deploy"
  pty emit myserver user.tests-passed --json '{"n": 42}'`);
}

// --- state: per-session JSON data bag ---

async function cmdState(argv: string[]): Promise<void> {
  const sub = argv[0];
  if (!sub || sub === "-h" || sub === "--help") {
    printStateHelp();
    return;
  }

  // pty state <sub> [ref] [key] [value]
  // ref defaults to $PTY_SESSION inside a session.
  const rest = argv.slice(1);

  // Figure out whether the first positional is a session-ref or a key.
  // Heuristic: if it names an existing session, treat it as the ref;
  // otherwise fall back to $PTY_SESSION. This matches the `pty exec`,
  // `pty rename` etc. pattern.
  async function resolveStateTarget(): Promise<{ name: string; rest: string[] }> {
    const inside = process.env.PTY_SESSION;
    if (rest.length > 0) {
      const candidate = rest[0];
      try {
        validateName(candidate);
        const existing = await getSession(candidate);
        if (existing) return { name: candidate, rest: rest.slice(1) };
      } catch {
        // Not a valid session name — treat as the key instead.
      }
    }
    if (!inside) {
      console.error(`pty state ${sub}: no session ref given and not running inside a pty session`);
      console.error(`  tip: run inside a pty session, or pass the session-ref: pty state ${sub} <session-ref> ...`);
      process.exit(1);
    }
    return { name: await resolveRef(inside), rest };
  }

  try {
    switch (sub) {
      case "get": {
        const { name, rest: r } = await resolveStateTarget();
        if (r.length === 0) {
          const bag = getState(name);
          console.log(JSON.stringify(bag, null, 2));
          return;
        }
        if (r.length > 1) {
          console.error("pty state get: unexpected extra args");
          process.exit(1);
        }
        const value = getStateKey(name, r[0]);
        if (value === undefined) {
          process.exit(1); // "missing key" — silent, non-zero exit
        }
        console.log(JSON.stringify(value, null, 2));
        return;
      }
      case "set": {
        const { name, rest: r } = await resolveStateTarget();
        if (r.length < 1) {
          console.error("pty state set: expected <key> [value]. If value is omitted, JSON is read from stdin.");
          process.exit(1);
        }
        if (r.length > 2) {
          console.error("pty state set: too many positional arguments. Quote the JSON value so the shell keeps it as one argument: pty state set <ref> <key> '<json>'.");
          process.exit(1);
        }
        const key = r[0];
        let raw: string;
        if (r.length === 2) {
          raw = r[1];
        } else {
          raw = await readAllStdin();
        }
        let value: unknown;
        try { value = JSON.parse(raw); }
        catch (e: any) {
          console.error(`pty state set: value is not valid JSON: ${e.message}`);
          process.exit(1);
        }
        setState(name, key, value);
        // state.set event is emitted by setState itself.
        return;
      }
      case "delete":
      case "rm": {
        const { name, rest: r } = await resolveStateTarget();
        if (r.length !== 1) {
          console.error(`pty state ${sub}: expected <key>`);
          process.exit(1);
        }
        const key = r[0];
        deleteState(name, key);
        // state.delete event is emitted by deleteState when a key was
        // actually removed; a delete on a missing key is a silent no-op.
        return;
      }
      case "keys": {
        const { name, rest: r } = await resolveStateTarget();
        if (r.length > 0) {
          console.error("pty state keys: unexpected extra args");
          process.exit(1);
        }
        const keys = listStateKeys(name);
        for (const k of keys) console.log(k);
        return;
      }
      default:
        printStateHelp();
        process.exit(1);
    }
  } catch (e: any) {
    console.error(e.message);
    process.exit(1);
  }
}

function printStateHelp(): void {
  console.log(`Usage:
  pty state get    [<ref>] [<key>]     # print full bag or one key (JSON)
  pty state set    [<ref>] <key> [v]   # set <key> to JSON value v (or stdin)
  pty state delete [<ref>] <key>       # remove a key
  pty state keys   [<ref>]             # list keys

Inside a pty session, <ref> defaults to $PTY_SESSION. Every set/delete
emits a matching state.set / state.delete event so followers of
'pty events <ref>' see state transitions live.

Values are JSON — "42" is the number 42, '"42"' is the string "42".
Use 'pty state set foo "$(cat payload.json)"' for larger payloads,
or pipe via stdin:  cat payload.json | pty state set foo`);
}

async function readAllStdin(): Promise<string> {
  // Collect all of stdin. Used by 'pty state set <key>' when no value arg
  // is given — allows shell piping for large JSON payloads.
  return await new Promise((resolve, reject) => {
    let data = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => { data += chunk; });
    process.stdin.on("end", () => resolve(data));
    process.stdin.on("error", reject);
  });
}


function hasPtyFile(dir: string): boolean {
  try {
    return fs.statSync(path.join(path.resolve(dir), "pty.toml")).isFile();
  } catch {
    return false;
  }
}

async function cmdUp(dir: string | undefined, names: string[]): Promise<void> {
  let ptyFile;
  try {
    ptyFile = readPtyFile(dir);
  } catch (e: any) {
    console.error(e.message);
    process.exit(1);
  }

  let sessions = ptyFile.sessions;
  if (names.length > 0) {
    const nameSet = new Set(names);
    const matchesName = (s: PtySessionDef) => nameSet.has(s.displayName) || nameSet.has(s.shortName);
    const unknown = names.filter((n) => !sessions.some((s) => s.displayName === n || s.shortName === n));
    if (unknown.length > 0) {
      console.error(`Unknown session${unknown.length > 1 ? "s" : ""}: ${unknown.join(", ")}`);
      console.error(`Available: ${sessions.map((s) => s.shortName).join(", ")}`);
      process.exit(1);
    }
    sessions = sessions.filter(matchesName);
  }

  const tomlPath = path.join(ptyFile.dir, "pty.toml");
  const existing = await listSessions();
  /** Find an existing session that came from this same (ptyfile, ptyfile.session)
   *  pair. The toml-derived displayName is just a label; identity is the tag
   *  pair, so renaming a session or hitting a long-name collision doesn't lose
   *  the binding. */
  const findByTags = (shortName: string) => existing.find((s) =>
    s.metadata?.tags?.ptyfile === tomlPath &&
    s.metadata?.tags?.["ptyfile.session"] === shortName
  );
  const allRefSet = await allRefs();

  let started = 0;
  let skipped = 0;

  for (const sess of sessions) {
    const userTomlKeys = Object.keys(sess.tags ?? {}).sort();
    const ptyfileTagsValue = userTomlKeys.join(",");
    const tomlTags: Record<string, string> = {
      ...sess.tags,
      ptyfile: tomlPath,
      "ptyfile.session": sess.shortName,
      "ptyfile.tags": ptyfileTagsValue,
    };

    const bound = findByTags(sess.shortName);

    if (bound && bound.status === "running") {
      // Sync tags from toml to the running session (including ptyfile metadata).
      // Track which tag keys came from the toml via "ptyfile.tags" so that
      // removing a tag from the toml causes it to be removed here (but
      // manually-added tags — those not in "ptyfile.tags" — are preserved).
      const currentTags = bound.metadata?.tags ?? {};

      const updates: Record<string, string> = {};
      for (const [k, v] of Object.entries(tomlTags)) {
        if (currentTags[k] !== v) updates[k] = v;
      }

      const prevTomlKeys = (currentTags["ptyfile.tags"] ?? "")
        .split(",")
        .map((k) => k.trim())
        .filter((k) => k.length > 0);
      const newKeySet = new Set(userTomlKeys);
      const removals = prevTomlKeys.filter((k) => !newKeySet.has(k));

      const label = bound.metadata?.displayName ?? bound.name;
      if (Object.keys(updates).length > 0 || removals.length > 0) {
        try {
          updateTags(bound.name, updates, removals);
          const changedTagUpdates = Object.entries(updates)
            .filter(([k]) => k !== "ptyfile" && k !== "ptyfile.session" && k !== "ptyfile.tags")
            .map(([k, v]) => `${k}=${v}`);
          const changedRemovals = removals.map((k) => `-${k}`);
          const changed = [...changedTagUpdates, ...changedRemovals].join(", ");
          if (changed) {
            console.log(`  ● ${label} (already running, updated tags: ${changed})`);
          } else {
            console.log(`  ● ${label} (already running)`);
          }
        } catch {
          console.log(`  ● ${label} (already running)`);
        }
      } else {
        console.log(`  ● ${label} (already running)`);
      }
      skipped++;
      continue;
    }

    // Clean up an exited bound session so its slot can be reused.
    if (bound && isGone(bound.status)) {
      cleanupAll(bound.name);
    }

    // Pick the on-disk id: honor the pty.toml's `id = "..."` if set,
    // otherwise generate a random one. Either way validate before spawn so
    // long pinned ids fail with a clear up-front error.
    let name: string;
    if (sess.id) {
      try {
        validateName(sess.id);
      } catch (e: any) {
        console.error(`  ✗ ${sess.displayName}: ${e.message}`);
        continue;
      }
      if (allRefSet.has(sess.id)) {
        console.error(`  ✗ ${sess.displayName}: id "${sess.id}" is already in use (as a name or displayName).`);
        continue;
      }
      name = sess.id;
      allRefSet.add(sess.id);
    } else {
      let candidate: string | null = null;
      for (let attempt = 0; attempt < 8; attempt++) {
        const c = randomSessionName();
        if (!allRefSet.has(c)) { candidate = c; allRefSet.add(c); break; }
      }
      if (!candidate) {
        console.error(`  ✗ ${sess.displayName}: could not generate a unique session id after 8 attempts.`);
        continue;
      }
      name = candidate;
    }

    // Validate the toml-derived displayName once. Default `<prefix>-<short>`
    // is always safe; an explicit `display_name` field could be anything.
    try {
      validateDisplayName(sess.displayName);
    } catch (e: any) {
      console.error(`  ✗ ${sess.displayName}: ${e.message}`);
      continue;
    }

    try {
      await spawnDaemon({
        name,
        command: "/bin/sh",
        args: ["-c", commandWithEnvExports(sess)],
        displayCommand: sess.command,
        cwd: ptyFile.dir,
        tags: tomlTags,
        displayName: sess.displayName,
      });
      console.log(`  ● ${sess.displayName} (started)`);
      started++;
    } catch (e: any) {
      console.error(`  ✗ ${sess.displayName}: ${e.message}`);
    }
  }

  if (started === 0 && skipped === sessions.length) {
    console.log("All sessions already running.");
  } else if (started > 0) {
    console.log(`Started ${started} session${started === 1 ? "" : "s"}.`);
  }
}

async function cmdDown(dir: string | undefined, names: string[]): Promise<void> {
  let ptyFile;
  try {
    ptyFile = readPtyFile(dir);
  } catch (e: any) {
    console.error(e.message);
    process.exit(1);
  }

  let sessions = ptyFile.sessions;
  if (names.length > 0) {
    const nameSet = new Set(names);
    sessions = sessions.filter((s) => nameSet.has(s.displayName) || nameSet.has(s.shortName));
  }

  const tomlPath = path.join(ptyFile.dir, "pty.toml");
  const existing = await listSessions();
  const findByTags = (shortName: string) => existing.find((s) =>
    s.metadata?.tags?.ptyfile === tomlPath &&
    s.metadata?.tags?.["ptyfile.session"] === shortName
  );
  let stopped = 0;

  for (const sess of sessions) {
    const existingSession = findByTags(sess.shortName);
    if (!existingSession) continue;

    const label = existingSession.metadata?.displayName ?? existingSession.name;

    // Strip the `strategy` tag so `pty gc` doesn't respawn the session
    // on its next tick. The `supervisor.status` tag is no longer a thing.
    const wasPermanent = existingSession.metadata?.tags?.strategy === "permanent";
    if (wasPermanent) {
      try {
        updateTags(existingSession.name, {}, ["strategy"]);
      } catch {}
    }

    if (existingSession.status === "running" && existingSession.pid) {
      try {
        process.kill(existingSession.pid, "SIGTERM");
        console.log(`  ○ ${label} (stopped${wasPermanent ? ", removed from supervision" : ""})`);
        stopped++;
      } catch {
        console.error(`  ✗ ${label}: failed to stop`);
      }
      cleanupSocket(existingSession.name);
    } else if (isGone(existingSession.status)) {
      cleanupAll(existingSession.name);
      console.log(`  ○ ${label} (cleaned up)`);
      stopped++;
    }
  }

  if (stopped === 0) {
    console.log("No sessions to stop.");
  } else {
    console.log(`Stopped ${stopped} session${stopped === 1 ? "" : "s"}.`);
  }

  // Warn if any stopped sessions are toml-managed
  const anyTomlManaged = sessions.some((sess) => findByTags(sess.shortName)?.metadata?.tags?.ptyfile);
  if (anyTomlManaged && stopped > 0) {
    console.error("\nNote: strategy tags will be restored on the next 'pty up'.");
  }
}

async function cmdRestart(
  name: string,
  yes = false,
  forceNested = false,
): Promise<void> {
  const session = await getSession(name);

  if (!session) {
    console.error(`Session "${name}" not found.`);
    process.exit(1);
  }

  const meta = session.metadata;
  if (!meta) {
    console.error(`Session "${name}" has no metadata — cannot restart.`);
    cleanupAll(name);
    process.exit(1);
  }

  if (session.status === "running" && session.pid) {
    if (!yes) {
      const answer = await ask(`Session "${name}" is running. Kill and restart? [Y/n] `);
      if (answer.toLowerCase() === "n") {
        process.exit(0);
      }
    }
    try {
      process.kill(session.pid, "SIGTERM");
    } catch {}
    cleanupSocket(name);
    // Wait briefly for the process to exit
    await new Promise((r) => setTimeout(r, 200));
  }

  cleanupAll(name);
  await spawnDaemon({ name, command: meta.command, args: meta.args, displayCommand: meta.displayCommand, cwd: meta.cwd, tags: meta.tags });
  console.log(`Session "${name}" restarted.`);

  // Nesting guard: restart itself is fine, but attaching would nest a client
  // inside the current session. Print a note and bail unless --force.
  const nested = process.env.PTY_SESSION;
  if (nested && !forceNested) {
    console.log(`  (not attached: already inside pty session "${nested}". Pass --force to attach anyway.)`);
    return;
  }

  doAttach(name);
}

async function cmdEvents(
  name: string | null,
  opts: { all: boolean; recent: boolean; json: boolean; waitEventType: string | null; timeout: number }
): Promise<void> {
  if (opts.recent) {
    if (!name) {
      console.error("--recent requires a session name.");
      process.exit(1);
    }
    const events = readRecentEvents(name);
    if (events.length === 0) {
      console.log(`No recent events for "${name}".`);
      return;
    }
    for (const event of events) {
      console.log(opts.json ? JSON.stringify(event) : formatEvent(event));
    }
    return;
  }

  // Follow mode: verify session exists if a specific name was given
  if (name) {
    const session = await getSession(name);
    if (!session) {
      console.error(`Session "${name}" not found.`);
      process.exit(1);
    }
  }

  // Wait mode: block until a specific event type appears
  if (opts.waitEventType) {
    if (!name) {
      console.error("--wait requires a session name.");
      process.exit(1);
    }
    const timeoutMs = opts.timeout > 0 ? opts.timeout * 1000 : 0;
    const start = Date.now();

    return new Promise<void>((resolve) => {
      let timer: ReturnType<typeof setTimeout> | null = null;

      const follower = new EventFollower({
        names: [name!],
        onEvent: (event) => {
          if (event.type === opts.waitEventType) {
            if (timer) clearTimeout(timer);
            console.log(opts.json ? JSON.stringify(event) : formatEvent(event));
            follower.stop();
            resolve();
          }
        },
      });

      follower.start();

      if (timeoutMs > 0) {
        timer = setTimeout(() => {
          follower.stop();
          console.error(`Timed out after ${opts.timeout}s waiting for "${opts.waitEventType}" event.`);
          process.exit(1);
        }, timeoutMs);
      }

      process.on("SIGINT", () => {
        follower.stop();
        process.exit(0);
      });
    });
  }

  const follower = new EventFollower({
    names: opts.all ? undefined : name ? [name] : undefined,
    onEvent: (event) => {
      console.log(opts.json ? JSON.stringify(event) : formatEvent(event));
    },
  });

  follower.start();

  process.on("SIGINT", () => {
    follower.stop();
    process.exit(0);
  });

  // Keep the process alive while watchers are active
  setInterval(() => {}, 60_000);
}

async function cmdTest(args: string[]): Promise<void> {
  const vitestBin = path.join(
    import.meta.dirname ?? path.dirname(new URL(import.meta.url).pathname),
    "..",
    "node_modules",
    ".bin",
    "vitest"
  );
  const vitestArgs = args.length === 0 ? ["run"] : args;
  const result = spawnSync(vitestBin, vitestArgs, {
    stdio: "inherit",
    env: process.env,
  });
  process.exit(result.status ?? 1);
}

function ask(prompt: string): Promise<string> {
  const rl = readline.createInterface({
    input: process.stdin,
    output: process.stdout,
  });
  return rl.question(prompt).then((answer) => {
    rl.close();
    return answer;
  });
}

function strategyMarker(tags?: Record<string, string>): string {
  if (!tags) return "";
  if (tags.strategy === "permanent") return " \x1b[33m[permanent]\x1b[0m";
  return "";
}

function shortPath(p: string): string {
  const home = os.homedir();
  if (p === home) return "~";
  if (p.startsWith(home + "/")) return "~" + p.slice(home.length);
  return p;
}

function timeAgo(date: Date): string {
  const seconds = Math.floor((Date.now() - date.getTime()) / 1000);
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}


// ── Wrap/Unwrap ──

const DEFAULT_WRAP_DIR = path.join(os.homedir(), ".local", "pty", "bin");

function getWrapDir(): string {
  return process.env.PTY_BIN_PATH ?? DEFAULT_WRAP_DIR;
}

function ensureWrapDir(): void {
  fs.mkdirSync(getWrapDir(), { recursive: true, mode: 0o700 });
}

function checkWrapInPath(): void {
  const wrapDir = getWrapDir();
  const pathDirs = (process.env.PATH ?? "").split(":");
  if (!pathDirs.includes(wrapDir)) {
    console.error(`\nAdd this to your shell profile to use wrapped commands:\n`);
    console.error(`  export PATH="${wrapDir}:$PATH"\n`);
  }
}

/**
 * Resolve the real binary path, skipping our wrap directory.
 * Searches PATH with the wrap dir excluded so we find the original binary.
 */
function resolveRealBinary(name: string): string | null {
  const wrapDir = getWrapDir();
  const pathDirs = (process.env.PATH ?? "").split(":").filter(d => d !== wrapDir);
  for (const dir of pathDirs) {
    const candidate = path.join(dir, name);
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      return candidate;
    } catch {}
  }
  return null;
}

function cmdWrap(command: string): void {
  const cmdName = path.basename(command);
  const realPath = resolveRealBinary(cmdName);
  if (!realPath) {
    console.error(`Command not found in PATH: ${cmdName}`);
    process.exit(1);
  }

  ensureWrapDir();
  const wrapPath = path.join(getWrapDir(), cmdName);

  // The wrapper script uses pty run -a (create or attach) with auto-naming.
  // It resolves pty from PATH (excluding the wrap dir to avoid recursion).
  const script = `#!/bin/sh
# Generated by: pty wrap ${cmdName}
# Wraps ${realPath} in a pty session
exec pty run -a -- ${realPath} "$@"
`;

  fs.writeFileSync(wrapPath, script, { mode: 0o755 });
  console.log(`Wrapped: ${cmdName} → ${realPath}`);
  console.log(`Wrapper: ${wrapPath}`);
  checkWrapInPath();
}

function cmdUnwrap(command: string): void {
  const cmdName = path.basename(command);
  const wrapPath = path.join(getWrapDir(), cmdName);
  if (!fs.existsSync(wrapPath)) {
    console.error(`Not wrapped: ${cmdName}`);
    process.exit(1);
  }
  fs.unlinkSync(wrapPath);
  console.log(`Unwrapped: ${cmdName}`);
}

function cmdWrapList(): void {
  const wrapDir = getWrapDir();
  let files: string[];
  try {
    files = fs.readdirSync(wrapDir);
  } catch {
    console.log("No wrapped commands.");
    return;
  }
  if (files.length === 0) {
    console.log("No wrapped commands.");
    return;
  }
  console.log("Wrapped commands:");
  for (const file of files.sort()) {
    const content = fs.readFileSync(path.join(wrapDir, file), "utf-8");
    const match = content.match(/exec pty run -a -- (.+) "\$@"/);
    const target = match ? match[1] : "?";
    console.log(`  ${file} → ${target}`);
  }
  checkWrapInPath();
}

main().catch((err) => {
  console.error(err.message);
  process.exit(1);
});
