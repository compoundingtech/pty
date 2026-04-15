import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as readline from "node:readline/promises";
import { spawnSync, execFileSync } from "node:child_process";
import { attach, peek, send, queryStats, type StatsResult } from "./client.ts";
import { parseSeqValue } from "./keys.ts";
import {
  listSessions,
  getSession,
  gc,
  cleanupAll,
  cleanupSocket,
  validateName,
  acquireLock,
  releaseLock,
  updateTags,
  readMetadata,
  writeMetadata,
  getSessionDir,
  type SessionInfo,
} from "./sessions.ts";
import { spawnDaemon, resolveCommand } from "./spawn.ts";
import { EventFollower, EventWriter, EventType, readRecentEvents, formatEvent } from "./events.ts";
import { readPtyFile, type PtySessionDef } from "./ptyfile.ts";
import { getSupervisorDir } from "./supervisor.ts";
import { extractFilterTags as extractFilterTagsImpl, matchesAllTags } from "./tags.ts";

// Lazy-load the interactive TUI so non-interactive commands don't crash when
// the caller's cwd was deleted (the TUI module evaluates process.cwd() at load).
async function runInteractive(options?: { preselectNew?: boolean; filterTags?: Record<string, string> }): Promise<void> {
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
  pty run -- <command> [args...]            Create a session and attach (auto-named)
  pty run --name <n> -- <command> [args...] Create a named session and attach
  pty run -d -- <command> [args...]        Create in the background
  pty run -a -- <command> [args...]        Create or attach if already running
  pty run --tag key=value -- <command>    Tag a session with metadata
  pty run --cwd /path -- <command>        Run in a specific directory
  pty attach <name>                        Attach to an existing session
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
  pty supervisor start                     Start the session supervisor
  pty supervisor stop                      Stop the supervisor
  pty supervisor status                    Show supervised sessions
  pty supervisor forget <name>             Stop supervising a session
  pty supervisor reset <name>              Reset a failed session for retry
  pty wrap <command>                       Auto-wrap a command in pty sessions
  pty unwrap <command>                     Remove a wrap
  pty wrap --list                          List wrapped commands
  pty test                                 Run tests (vitest)
  pty test watch                           Watch mode
  pty test -t "pattern"                    Run matching tests

Detach from a session with Ctrl+\\ (press twice to send Ctrl+\\ to the process)`);
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
  if (!subcommand || subcommand === "i" || subcommand === "interactive") {
    preselectNew = args.includes("--preselect-new");
    interactiveFilterTags = extractFilterTags(args);
  }
  const dispatchArgs = args.filter((a) => a !== "--preselect-new");

  if (dispatchArgs.length === 0) {
    await runInteractive({ preselectNew, filterTags: interactiveFilterTags });
    return;
  }

  const command = dispatchArgs[0];

  switch (command) {
    case "interactive":
    case "i": {
      await runInteractive({ preselectNew, filterTags: interactiveFilterTags });
      break;
    }

    case "run": {
      // Parse flags before the -- separator
      let detach = false;
      let attachExisting = false;
      let ephemeral = false;
      let name: string | null = null;
      let cwd: string | null = null;
      const tags: Record<string, string> = {};
      let i = 1;
      while (i < args.length && args[i] !== "--") {
        if (args[i] === "-d" || args[i] === "--detach") { detach = true; i++; }
        else if (args[i] === "-a" || args[i] === "--attach") { attachExisting = true; i++; }
        else if (args[i] === "-e" || args[i] === "--ephemeral") { ephemeral = true; i++; }
        else if (args[i] === "--name" && i + 1 < args.length) { name = args[i + 1]; i += 2; }
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
        // Anything between flags and -- that isn't a flag is a legacy positional name
        const between = args.slice(i, dashDash);
        if (between.length > 0 && !name) {
          // Backward compat: pty run myserver -- node server.js
          name = between[0];
          console.error(`Hint: use --name instead: pty run --name ${name} -- ...`);
        }
        cmd = args[dashDash + 1];
        cmdArgs = args.slice(dashDash + 2);
      } else {
        // No -- separator: legacy positional format
        // pty run myserver node server.js
        const rest = args.slice(i);
        if (!name && rest.length >= 2) {
          name = rest[0];
          cmd = rest[1];
          cmdArgs = rest.slice(2);
          console.error(`Hint: use --name instead: pty run --name ${name} -- ${cmd} ${cmdArgs.join(" ")}`.trimEnd());
        } else {
          cmd = rest[0];
          cmdArgs = rest.slice(1);
        }
      }

      if (!cmd) {
        console.error("Usage: pty run [--name <name>] [-d] [-a] -- <command> [args...]");
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

      // Nesting prevention: if inside a pty session and not detaching, exec directly
      if (process.env.PTY_SESSION && !detach) {
        console.error(
          `Already inside pty session "${process.env.PTY_SESSION}", running directly.`
        );
        const result = spawnSync(cmd, cmdArgs, {
          stdio: "inherit",
          env: process.env,
        });
        process.exit(result.status ?? 1);
      }

      // Auto-generate name if not provided
      if (!name) {
        const sessions = await listSessions();
        const existing = new Set(sessions.map(s => s.name));
        let candidate = autoName(autoNameCmd, cmdArgs);
        // Sanitize: validateName allows [a-zA-Z0-9._-]
        candidate = candidate.replace(/[^a-zA-Z0-9._-]/g, "-").replace(/-+/g, "-").replace(/^-|-$/g, "");
        // Dedup
        if (existing.has(candidate)) {
          for (let n = 2; ; n++) {
            const c = `${candidate}-${n}`;
            if (!existing.has(c)) { candidate = c; break; }
          }
        }
        name = candidate;
      }

      try {
        validateName(name);
      } catch (e: any) {
        console.error(e.message);
        process.exit(1);
      }

      await cmdRun(name, cmd, cmdArgs, detach, attachExisting, displayCmd, ephemeral, tags, cwd);
      break;
    }

    case "attach":
    case "a": {
      const autoRestart =
        args[1] === "--auto-restart" || args[1] === "-r";
      const attachName = autoRestart ? args[2] : args[1];
      if (!attachName) {
        console.error("Usage: pty attach [-r|--auto-restart] <name>");
        process.exit(1);
      }
      try {
        validateName(attachName);
      } catch (e: any) {
        console.error(e.message);
        process.exit(1);
      }
      await cmdAttach(attachName, autoRestart);
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
      try {
        validateName(peekName);
      } catch (e: any) {
        console.error(e.message);
        process.exit(1);
      }
      if (waitPatterns.length > 0) {
        await cmdPeekWait(peekName, waitPatterns, timeoutSec, plain);
      } else {
        cmdPeek(peekName, follow, plain, full);
      }
      break;
    }

    case "send": {
      const sendName = args[1];
      if (!sendName) {
        console.error('Usage: pty send <name> "text"  or  pty send <name> --seq "text" --seq key:return');
        process.exit(1);
      }
      try {
        validateName(sendName);
      } catch (e: any) {
        console.error(e.message);
        process.exit(1);
      }

      let sendArgs = args.slice(2);
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
        data = [sendArgs[0]];
      } else {
        console.error("Nothing to send.");
        process.exit(1);
      }

      send({ name: sendName, data, delayMs: delaySecs != null ? delaySecs * 1000 : undefined });
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

      if (eventsName) {
        try {
          validateName(eventsName);
        } catch (e: any) {
          console.error(e.message);
          process.exit(1);
        }
      }

      await cmdEvents(eventsName ?? null, { all, recent, json, waitEventType, timeout: eventsTimeout });
      break;
    }

    case "list":
    case "ls": {
      const listArgs = args.slice();
      const listFilterTags = extractFilterTags(listArgs);
      const jsonFlag = listArgs.includes("--json");
      const tagsFlag = listArgs.includes("--tags");
      const remoteFlag = listArgs.includes("--remote");
      await cmdList(jsonFlag, tagsFlag, remoteFlag, listFilterTags);
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
      const forceRestart = args[1] === "-y" || args[1] === "--yes";
      const restartName = forceRestart ? args[2] : args[1];
      if (!restartName) {
        console.error("Usage: pty restart [-y] <name>");
        process.exit(1);
      }
      await cmdRestart(restartName, forceRestart);
      break;
    }

    case "kill": {
      if (args.length < 2) {
        console.error("Usage: pty kill <name>");
        process.exit(1);
      }
      try {
        validateName(args[1]);
      } catch (e: any) {
        console.error(e.message);
        process.exit(1);
      }
      await cmdKill(args[1]);
      break;
    }

    case "gc": {
      await cmdGc();
      break;
    }

    case "tag": {
      const tagName = args[1];
      if (!tagName) {
        console.error("Usage: pty tag <name> [key=value...] [--rm key...]");
        process.exit(1);
      }
      try {
        validateName(tagName);
      } catch (e: any) {
        console.error(e.message);
        process.exit(1);
      }

      const updates: Record<string, string> = {};
      const removals: string[] = [];
      for (let i = 2; i < args.length; i++) {
        if (args[i] === "--rm" && i + 1 < args.length) {
          removals.push(args[i + 1]);
          i++;
        } else {
          const eq = args[i].indexOf("=");
          if (eq === -1) {
            console.error(`Invalid tag format: "${args[i]}". Use key=value or --rm key`);
            process.exit(1);
          }
          updates[args[i].slice(0, eq)] = args[i].slice(eq + 1);
        }
      }

      // No updates or removals — show current tags
      if (Object.keys(updates).length === 0 && removals.length === 0) {
        const meta = readMetadata(tagName);
        if (!meta) {
          console.error(`Session "${tagName}" not found.`);
          process.exit(1);
        }
        if (!meta.tags || Object.keys(meta.tags).length === 0) {
          console.log(`No tags on "${tagName}".`);
        } else {
          for (const [k, v] of Object.entries(meta.tags)) {
            console.log(`  ${k}=${v}`);
          }
        }
        break;
      }

      try {
        // Check if session is managed by a pty.toml before modifying
        const beforeMeta = readMetadata(tagName);
        const ptyfilePath = beforeMeta?.tags?.ptyfile;

        updateTags(tagName, updates, removals);
        const meta = readMetadata(tagName);
        if (!meta?.tags || Object.keys(meta.tags).length === 0) {
          console.log(`Tags cleared on "${tagName}".`);
        } else {
          console.log(`Tags on "${tagName}":`);
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

    case "rm":
    case "remove": {
      if (args.length < 2) {
        console.error("Usage: pty rm <name>");
        process.exit(1);
      }
      try {
        validateName(args[1]);
      } catch (e: any) {
        console.error(e.message);
        process.exit(1);
      }
      await cmdRm(args[1]);
      break;
    }

    case "supervisor": {
      const subCmd = args[1];
      if (!subCmd || subCmd === "-h" || subCmd === "--help") {
        console.log(`Usage:
  pty supervisor start            Start the supervisor
  pty supervisor stop             Stop the supervisor
  pty supervisor status           Show supervised sessions
  pty supervisor forget <name>    Stop supervising a session
  pty supervisor reset <name>     Reset a failed session for retry
  pty supervisor launchd install [--path PATH]  Register with macOS launchd (requires FDA)
  pty supervisor launchd uninstall Remove from launchd`);
        break;
      }
      switch (subCmd) {
        case "start":
          await cmdSupervisorStart();
          break;
        case "stop":
          await cmdSupervisorStop();
          break;
        case "status":
          await cmdSupervisorStatus();
          break;
        case "forget": {
          const forgetName = args[2];
          if (!forgetName) {
            console.error("Usage: pty supervisor forget <name>");
            process.exit(1);
          }
          await cmdSupervisorForget(forgetName);
          break;
        }
        case "reset": {
          const resetName = args[2];
          if (!resetName) {
            console.error("Usage: pty supervisor reset <name>");
            process.exit(1);
          }
          await cmdSupervisorReset(resetName);
          break;
        }
        case "launchd": {
          const launchdCmd = args[2];
          if (launchdCmd === "install") {
            // Parse --path flag
            let userPath: string | undefined;
            for (let li = 3; li < args.length; li++) {
              if (args[li] === "--path" && li + 1 < args.length) {
                userPath = args[li + 1];
                break;
              }
            }
            await cmdSupervisorLaunchdInstall(userPath);
          } else if (launchdCmd === "uninstall") {
            cmdSupervisorLaunchdUninstall();
          } else {
            console.error("Usage: pty supervisor launchd install|uninstall");
            process.exit(1);
          }
          break;
        }
        default:
          console.error(`Unknown supervisor command: ${subCmd}`);
          process.exit(1);
      }
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
  const previousCwd = session?.status === "exited" ? session.metadata?.cwd : undefined;
  const previousTags = session?.status === "exited" ? session.metadata?.tags : undefined;
  if (session?.status === "exited") {
    cleanupAll(name);
  }

  try {
    const tagOpt = Object.keys(tags).length > 0 ? tags : previousTags;
    const cwdOpt = explicitCwd ?? previousCwd;
    await spawnDaemon({ name, command, args, displayCommand, cwd: cwdOpt, ephemeral, tags: tagOpt });
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
  autoRestart = false
): Promise<void> {
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
  // Check if session is exited — fall back to saved lastLines
  const session = await getSession(name);
  if (session?.status === "exited") {
    const meta = session.metadata;
    if (meta?.lastLines && meta.lastLines.length > 0) {
      process.stdout.write(meta.lastLines.join("\n") + "\n");
    } else {
      console.error(`Session "${name}" has exited with no saved output.`);
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

async function cmdList(json = false, showTags = false, remote = false, filterTags: Record<string, string> = {}): Promise<void> {
  let sessions = await listSessions();
  if (Object.keys(filterTags).length > 0) {
    sessions = sessions.filter((s) => matchesAllTags(s.metadata?.tags, filterTags));
  }

  // Fetch relay hosts if --remote
  let remoteHosts: { label: string; sessions: { name: string; status: string; command?: string; cwd?: string }[]; error: string | null }[] = [];
  if (remote) {
    try {
      const relayBin = execFileSync("which", ["pty-relay"], { encoding: "utf-8" }).trim();
      const result = spawnSync(relayBin, ["ls", "--json"], { encoding: "utf-8", timeout: 5000 });
      if (result.status === 0 && result.stdout.trim()) {
        remoteHosts = JSON.parse(result.stdout);
      }
    } catch {}
  }

  if (json) {
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
    }));
    if (remote && remoteHosts.length > 0) {
      console.log(JSON.stringify({ local: localOutput, remote: remoteHosts }));
    } else {
      console.log(JSON.stringify(localOutput));
    }
    return;
  }

  if (sessions.length === 0 && remoteHosts.length === 0) {
    console.log("No active sessions.");
    return;
  }

  const running = sessions.filter((s) => s.status === "running");
  const exited = sessions.filter((s) => s.status === "exited");

  // Render tags as hashtags. When `showAll` is false, hide internal bookkeeping
  // keys (ptyfile*, supervisor.status) since those have dedicated markers or
  // aren't meaningful to users. `--tags` (showAll=true) includes everything.
  const renderTags = (tags: Record<string, string> | undefined, showAll: boolean): string => {
    if (!tags) return "";
    const entries = Object.entries(tags).filter(([k]) =>
      showAll || (k !== "ptyfile" && k !== "ptyfile.session" && k !== "ptyfile.tags" && k !== "supervisor.status" && k !== "strategy"),
    );
    return entries.length > 0 ? " " + entries.map(([k, v]) => `#${k}=${v}`).join(" ") : "";
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
      console.log(`  \x1b[1;36m${session.name}\x1b[0m${marker}${tagStr} (pid: ${session.pid}) — ${cwd} — \x1b[2m${cmd}\x1b[0m`);
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
      console.log(`  \x1b[1m${session.name}\x1b[0m${marker}${tagStr} (exited with code ${code}, ${ago}) — ${cwd} — \x1b[2m${cmd}\x1b[0m`);
    }
  }

  // Remote hosts
  for (const host of remoteHosts) {
    console.log("");
    if (host.error) {
      console.log(`\x1b[1m${host.label}\x1b[0m \x1b[31m(error: ${host.error})\x1b[0m`);
      continue;
    }
    console.log(`\x1b[1m${host.label}\x1b[0m (${host.sessions.length} sessions):`);
    for (const s of host.sessions) {
      const icon = s.status === "running" ? "\u25cf" : "\u25cb";
      const cwd = s.cwd ? shortPath(s.cwd) : "";
      const cmd = s.command ?? "";
      console.log(`  \x1b[1;36m${icon} ${s.name}\x1b[0m — ${cwd} — \x1b[2m${cmd}\x1b[0m`);
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
    if (session.status === "exited") {
      if (json) {
        console.log(JSON.stringify({
          name: session.name,
          status: "exited",
          exitCode: session.metadata?.exitCode ?? null,
          exitedAt: session.metadata?.exitedAt ?? null,
          ...(session.metadata?.tags ? { tags: session.metadata.tags } : {}),
        }));
      } else {
        const code = session.metadata?.exitCode ?? "?";
        console.log(`Session "${name}" has exited (code ${code}).`);
      }
      return;
    }

    try {
      const stats = await queryStats(name);
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
  const exited = sessions.filter((s) => s.status === "exited");

  if (running.length === 0 && (!all || exited.length === 0)) {
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
        ? exited.map((s) => ({
            name: s.name,
            status: "exited" as const,
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

  if (all && exited.length > 0) {
    if (results.length > 0) console.log("");
    console.log("Exited sessions:");
    for (const s of exited) {
      const code = s.metadata?.exitCode ?? "?";
      const ago = s.metadata?.exitedAt ? timeAgo(new Date(s.metadata.exitedAt)) : "unknown";
      console.log(`  ${s.name} (exited with code ${code}, ${ago})`);
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

  // Remove supervision tags so the supervisor doesn't restart it
  const wasSupervised = session.metadata?.tags?.strategy === "permanent" || session.metadata?.tags?.strategy === "temporary";
  if (wasSupervised) {
    try {
      const removals = ["strategy"];
      if (session.metadata?.tags?.["supervisor.status"]) removals.push("supervisor.status");
      updateTags(name, {}, removals);
    } catch {}
  }

  try {
    process.kill(session.pid, "SIGTERM");
    console.log(`Session "${name}" killed.`);
  } catch {
    console.error(`Failed to kill session "${name}".`);
  }
  cleanupSocket(name);

  if (wasSupervised && session.metadata?.tags?.ptyfile) {
    console.error(`Note: this session is managed by ${session.metadata.tags.ptyfile}`);
    console.error("The strategy tag will be restored on the next 'pty up'.");
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

async function cmdGc(): Promise<void> {
  const removed = await gc();

  if (removed.length === 0) {
    console.log("No exited sessions to clean up.");
    return;
  }

  for (const name of removed) {
    console.log(`Removed: ${name}`);
  }
  console.log(`Cleaned up ${removed.length} exited session${removed.length === 1 ? "" : "s"}.`);
}

async function cmdSupervisorStart(): Promise<void> {
  // Run the supervisor in the foreground (not in a pty session).
  // This makes it work with launchd KeepAlive since launchd owns the process.
  // Use `pty events supervisor` to follow activity, `pty supervisor status` for state.
  const { Supervisor } = await import("./supervisor.ts");

  const sup = new Supervisor("supervisor");
  sup.start();

  console.log(`[supervisor] started (pid ${process.pid})`);
  console.log(`[supervisor] watching ${getSessionDir()}`);

  process.on("SIGTERM", () => {
    console.log("[supervisor] stopping...");
    sup.stop();
    process.exit(0);
  });
  process.on("SIGINT", () => {
    console.log("[supervisor] stopping...");
    sup.stop();
    process.exit(0);
  });

  // Keep the process alive
  await new Promise(() => {});
}

async function cmdSupervisorStop(): Promise<void> {
  const pidPath = path.join(getSupervisorDir(), "supervisor.pid");
  let pid: number;
  try {
    pid = parseInt(fs.readFileSync(pidPath, "utf-8").trim(), 10);
  } catch {
    console.error("Supervisor is not running.");
    process.exit(1);
  }
  try {
    process.kill(pid, 0); // check if alive
    process.kill(pid, "SIGTERM");
    console.log("Supervisor stopped.");
  } catch {
    console.error("Supervisor is not running (stale pid file).");
    try { fs.unlinkSync(pidPath); } catch {}
    process.exit(1);
  }
}

async function cmdSupervisorStatus(): Promise<void> {
  const sessions = await listSessions();
  const supervised = sessions.filter((s) =>
    s.metadata?.tags?.strategy === "permanent" || s.metadata?.tags?.strategy === "temporary"
  );

  if (supervised.length === 0) {
    console.log("No supervised sessions.");
    return;
  }

  // Try to read supervisor state for restart counts
  let state: Record<string, any> = {};
  try {
    const content = fs.readFileSync(path.join(getSupervisorDir(), "state.json"), "utf-8");
    state = JSON.parse(content).sessions ?? {};
  } catch {}

  let supervisorRunning = false;
  try {
    const pid = parseInt(fs.readFileSync(path.join(getSupervisorDir(), "supervisor.pid"), "utf-8").trim(), 10);
    process.kill(pid, 0);
    supervisorRunning = true;
  } catch {}
  console.log(`Supervisor: ${supervisorRunning ? "\x1b[32mrunning\x1b[0m" : "\x1b[31mnot running\x1b[0m"}`);
  console.log("");

  for (const s of supervised) {
    const strategy = s.metadata!.tags!.strategy;
    const supStatus = s.metadata?.tags?.["supervisor.status"];
    const stateInfo = state[s.name];
    const restarts = stateInfo?.restartCount ?? 0;

    let status = s.status === "running" ? "\x1b[32mrunning\x1b[0m" : "\x1b[33mexited\x1b[0m";
    if (supStatus === "failed") status = "\x1b[31mfailed\x1b[0m";

    console.log(`  \x1b[1m${s.name}\x1b[0m [${strategy}] — ${status}${restarts > 0 ? ` (${restarts} restarts)` : ""}`);
  }
}

async function cmdSupervisorForget(name: string): Promise<void> {
  try {
    validateName(name);
  } catch (e: any) {
    console.error(e.message);
    process.exit(1);
  }

  const meta = readMetadata(name);
  if (!meta) {
    console.error(`Session "${name}" not found.`);
    process.exit(1);
  }

  const removals: string[] = [];
  if (meta.tags?.strategy) removals.push("strategy");
  if (meta.tags?.["supervisor.status"]) removals.push("supervisor.status");

  if (removals.length === 0) {
    console.log(`Session "${name}" is not supervised.`);
    return;
  }

  updateTags(name, {}, removals);
  console.log(`Removed supervision from "${name}".`);

  if (meta.tags?.ptyfile) {
    console.error(`\nWarning: this session is managed by ${meta.tags.ptyfile}`);
    console.error("The strategy tag will be restored on the next 'pty up'.");
    console.error("Edit the pty.toml to make this permanent.");
  }
}

async function cmdSupervisorReset(name: string): Promise<void> {
  try {
    validateName(name);
  } catch (e: any) {
    console.error(e.message);
    process.exit(1);
  }

  const meta = readMetadata(name);
  if (!meta) {
    console.error(`Session "${name}" not found.`);
    process.exit(1);
  }

  if (meta.tags?.["supervisor.status"] !== "failed") {
    console.log(`Session "${name}" is not in failed state.`);
    return;
  }

  // Remove the failed tag
  updateTags(name, {}, ["supervisor.status"]);

  // Reset restart counter in supervisor state
  const statePath = path.join(getSupervisorDir(), "state.json");
  try {
    const content = fs.readFileSync(statePath, "utf-8");
    const state = JSON.parse(content);
    if (state.sessions?.[name]) {
      state.sessions[name].restartCount = 0;
      state.sessions[name].restartWindowStart = 0;
      state.sessions[name].nextBackoffMs = 1000;
      state.sessions[name].failed = false;
      const tmp = statePath + ".tmp";
      fs.writeFileSync(tmp, JSON.stringify(state, null, 2));
      fs.renameSync(tmp, statePath);
    }
  } catch {}

  console.log(`Reset "${name}". The supervisor will try restarting it.`);
}

async function cmdSupervisorLaunchdInstall(userPath?: string): Promise<void> {
  const launchdDir = path.join(os.homedir(), ".local", "pty", "launchd");
  const wrapperPath = path.join(launchdDir, "pty-supervisor");
  const bundlePath = path.join(launchdDir, "supervisor.bundle.js");
  const logPath = path.join(os.homedir(), ".local", "state", "pty", "supervisor.log");
  const plistDir = path.join(os.homedir(), "Library", "LaunchAgents");
  const plistPath = path.join(plistDir, "com.myobie.pty.supervisor.plist");

  // Stop existing supervisor if running
  const pidPath = path.join(getSupervisorDir(), "supervisor.pid");
  try {
    const pid = parseInt(fs.readFileSync(pidPath, "utf-8").trim(), 10);
    try { process.kill(pid, "SIGTERM"); } catch {}
    try { fs.unlinkSync(pidPath); } catch {}
    console.log("Stopped existing supervisor.");
  } catch {}

  // Unload existing plist if present
  if (fs.existsSync(plistPath)) {
    spawnSync("launchctl", ["unload", plistPath], { encoding: "utf-8" });
  }

  const srcRoot = path.join(
    import.meta.dirname ?? path.dirname(new URL(import.meta.url).pathname),
    ".."
  );
  const distDir = path.join(srcRoot, "dist");
  const nodeBin = process.execPath;
  const wrapperSrc = path.join(srcRoot, "scripts", "supervisor-wrapper.c");

  // Clean and create launchd directory
  fs.rmSync(launchdDir, { recursive: true, force: true });
  fs.mkdirSync(launchdDir, { recursive: true });

  // Bundle the supervisor
  const entryPoint = path.join(distDir, "supervisor-entry.js");
  const serverModule = path.join(distDir, "server.js");

  console.log("Bundling supervisor...");
  const esbuildResult = spawnSync("npx", ["esbuild", entryPoint, "--bundle", "--platform=node", "--format=esm", `--outfile=${bundlePath}`, `--define:SERVER_MODULE_PATH="${serverModule.replace(/\\/g, "\\\\")}"`], {
    encoding: "utf-8",
    cwd: distDir,
  });
  if (esbuildResult.status !== 0) {
    console.error(`Failed to bundle supervisor: ${esbuildResult.stderr}`);
    process.exit(1);
  }

  // Compile the FDA wrapper binary with PATH baked in
  const envPath = userPath ?? process.env.PATH ?? "/usr/bin:/bin:/usr/sbin:/sbin";
  console.log("Compiling FDA wrapper...");
  const ccResult = spawnSync("cc", [
    "-O2", "-o", wrapperPath,
    `-DNODE_PATH="${nodeBin}"`,
    `-DBUNDLE_PATH="${bundlePath}"`,
    `-DUSER_PATH="${envPath}"`,
    wrapperSrc,
  ], { encoding: "utf-8" });
  if (ccResult.status !== 0) {
    console.error(`Failed to compile wrapper: ${ccResult.stderr}`);
    process.exit(1);
  }

  // Run --check via a one-shot launchd job to test under launchd's actual
  // permission scope (not the terminal's). This is the only reliable way to
  // know if the wrapper binary has FDA.
  function checkFDAViaLaunchd(): boolean {
    const checkLabel = "com.myobie.pty.fda-check";
    const checkOutput = path.join(launchdDir, "fda-check.log");
    const checkPlist = path.join(launchdDir, "fda-check.plist");

    try { fs.unlinkSync(checkOutput); } catch {}

    fs.writeFileSync(checkPlist, `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${checkLabel}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${wrapperPath}</string>
    <string>--check</string>
  </array>
  <key>StandardOutPath</key>
  <string>${checkOutput}</string>
  <key>StandardErrorPath</key>
  <string>${checkOutput}</string>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
`);

    // Unload any previous check job
    spawnSync("launchctl", ["unload", checkPlist], { encoding: "utf-8" });

    // Load — runs immediately due to RunAtLoad
    spawnSync("launchctl", ["load", checkPlist], { encoding: "utf-8" });

    // Wait for the job to finish (it's a one-shot, exits quickly)
    const start = Date.now();
    while (Date.now() - start < 5000) {
      const list = spawnSync("launchctl", ["list"], { encoding: "utf-8" });
      const line = list.stdout.split("\n").find((l: string) => l.includes(checkLabel));
      // Format: "PID\tExitCode\tLabel" — if PID is "-", the job has exited
      if (line && line.startsWith("-")) break;
      spawnSync("sleep", ["0.2"]);
    }

    // Unload the check job
    spawnSync("launchctl", ["unload", checkPlist], { encoding: "utf-8" });
    try { fs.unlinkSync(checkPlist); } catch {}

    // Read the output
    try {
      const output = fs.readFileSync(checkOutput, "utf-8");
      try { fs.unlinkSync(checkOutput); } catch {}
      return output.includes("All checks passed");
    } catch {
      return false;
    }
  }

  console.log("Checking Full Disk Access via launchd...");
  let hasFDA = checkFDAViaLaunchd();

  if (!hasFDA) {
    console.log("");
    console.log("The supervisor wrapper needs Full Disk Access to manage");
    console.log("sessions on external/removable volumes.");
    console.log("");
    console.log("1. Open System Settings > Privacy & Security > Full Disk Access");
    console.log(`2. Click + and add: ${wrapperPath}`);
    console.log("");

    // Open the folder in Finder so they can find the binary
    spawnSync("open", [launchdDir]);

    const rl = await import("node:readline/promises");
    const prompt = rl.createInterface({ input: process.stdin, output: process.stdout });
    const answer = await prompt.question("Have you granted Full Disk Access? [y/N] ");
    prompt.close();

    if (answer.trim().toLowerCase() !== "y") {
      console.error("Aborted. Full Disk Access is required for launchd.");
      process.exit(1);
    }

    // Re-check via launchd
    console.log("Verifying...");
    hasFDA = checkFDAViaLaunchd();
    if (!hasFDA) {
      console.error("");
      console.error("Full Disk Access check failed under launchd.");
      console.error("The plist was NOT loaded. Grant FDA and try again:");
      console.error(`  ${wrapperPath}`);
      process.exit(1);
    }
    console.log("Full Disk Access verified.");
  } else {
    console.log("Full Disk Access: granted.");
  }

  // Write and load the plist — FDA is confirmed
  const plist = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.myobie.pty.supervisor</string>
  <key>ProgramArguments</key>
  <array>
    <string>${wrapperPath}</string>
  </array>
  <key>StandardOutPath</key>
  <string>${logPath}</string>
  <key>StandardErrorPath</key>
  <string>${logPath}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>5</integer>
</dict>
</plist>
`;

  fs.mkdirSync(plistDir, { recursive: true });
  fs.writeFileSync(plistPath, plist);

  const result = spawnSync("launchctl", ["load", plistPath], { encoding: "utf-8" });
  if (result.status !== 0) {
    console.error(`Failed to load plist: ${result.stderr}`);
    process.exit(1);
  }

  console.log("");
  console.log(`Wrapper: ${wrapperPath}`);
  console.log(`Bundle:  ${bundlePath}`);
  console.log(`Plist:   ${plistPath}`);
  console.log(`Log:     ${logPath}`);
  console.log("Supervisor will start on login and restart if it exits.");
}

function cmdSupervisorLaunchdUninstall(): void {
  const plistPath = path.join(os.homedir(), "Library", "LaunchAgents", "com.myobie.pty.supervisor.plist");
  const launchdDir = path.join(os.homedir(), ".local", "pty", "launchd");

  if (!fs.existsSync(plistPath)) {
    console.error("Supervisor is not registered with launchd.");
    process.exit(1);
  }

  spawnSync("launchctl", ["unload", plistPath], { encoding: "utf-8" });
  try { fs.unlinkSync(plistPath); } catch {}

  // Clean up bundled files
  try { fs.rmSync(launchdDir, { recursive: true, force: true }); } catch {}

  // Stop supervisor if running
  const pidPath = path.join(getSupervisorDir(), "supervisor.pid");
  try {
    const pid = parseInt(fs.readFileSync(pidPath, "utf-8").trim(), 10);
    try { process.kill(pid, "SIGTERM"); } catch {}
    try { fs.unlinkSync(pidPath); } catch {}
  } catch {}

  console.log("Supervisor removed from launchd.");
  console.log(`Cleaned up ${launchdDir}`);
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
    const matchesName = (s: PtySessionDef) => nameSet.has(s.name) || nameSet.has(s.shortName);
    const unknown = names.filter((n) => !sessions.some((s) => s.name === n || s.shortName === n));
    if (unknown.length > 0) {
      console.error(`Unknown session${unknown.length > 1 ? "s" : ""}: ${unknown.join(", ")}`);
      console.error(`Available: ${sessions.map((s) => s.shortName).join(", ")}`);
      process.exit(1);
    }
    sessions = sessions.filter(matchesName);
  }

  const existing = await listSessions();
  const runningNames = new Set(existing.filter((s) => s.status === "running").map((s) => s.name));

  let started = 0;
  let skipped = 0;

  for (const sess of sessions) {
    const tomlPath = path.join(ptyFile.dir, "pty.toml");
    const userTomlKeys = Object.keys(sess.tags ?? {}).sort();
    const ptyfileTagsValue = userTomlKeys.join(",");
    const tomlTags: Record<string, string> = {
      ...sess.tags,
      ptyfile: tomlPath,
      "ptyfile.session": sess.shortName,
      "ptyfile.tags": ptyfileTagsValue,
    };

    if (runningNames.has(sess.name)) {
      // Sync tags from toml to the running session (including ptyfile metadata).
      // Track which tag keys came from the toml via "ptyfile.tags" so that
      // removing a tag from the toml causes it to be removed here (but
      // manually-added tags — those not in "ptyfile.tags" — are preserved).
      const currentMeta = readMetadata(sess.name);
      const currentTags = currentMeta?.tags ?? {};

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

      if (Object.keys(updates).length > 0 || removals.length > 0) {
        try {
          updateTags(sess.name, updates, removals);
          const changedTagUpdates = Object.entries(updates)
            .filter(([k]) => k !== "ptyfile" && k !== "ptyfile.session" && k !== "ptyfile.tags")
            .map(([k, v]) => `${k}=${v}`);
          const changedRemovals = removals.map((k) => `-${k}`);
          const changed = [...changedTagUpdates, ...changedRemovals].join(", ");
          if (changed) {
            console.log(`  ● ${sess.name} (already running, updated tags: ${changed})`);
          } else {
            console.log(`  ● ${sess.name} (already running)`);
          }
        } catch {
          console.log(`  ● ${sess.name} (already running)`);
        }
      } else {
        console.log(`  ● ${sess.name} (already running)`);
      }
      skipped++;
      continue;
    }

    // Clean up exited session with the same name
    const existingSession = existing.find((s) => s.name === sess.name);
    if (existingSession?.status === "exited") {
      cleanupAll(sess.name);
    }

    try {
      await spawnDaemon({
        name: sess.name,
        command: "/bin/sh",
        args: ["-c", sess.command],
        displayCommand: sess.command,
        cwd: ptyFile.dir,
        tags: tomlTags,
      });
      console.log(`  ● ${sess.name} (started)`);
      started++;
    } catch (e: any) {
      console.error(`  ✗ ${sess.name}: ${e.message}`);
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
    sessions = sessions.filter((s) => nameSet.has(s.name) || nameSet.has(s.shortName));
  }

  const existing = await listSessions();
  let stopped = 0;

  for (const sess of sessions) {
    const existingSession = existing.find((s) => s.name === sess.name);
    if (!existingSession) continue;

    // Remove supervision tags so the supervisor doesn't restart it
    const wasSupervised = existingSession.metadata?.tags?.strategy === "permanent" || existingSession.metadata?.tags?.strategy === "temporary";
    if (wasSupervised) {
      try {
        const removals = ["strategy"];
        if (existingSession.metadata?.tags?.["supervisor.status"]) removals.push("supervisor.status");
        updateTags(sess.name, {}, removals);
      } catch {}
    }

    if (existingSession.status === "running" && existingSession.pid) {
      try {
        process.kill(existingSession.pid, "SIGTERM");
        console.log(`  ○ ${sess.name} (stopped${wasSupervised ? ", removed from supervision" : ""})`);
        stopped++;
      } catch {
        console.error(`  ✗ ${sess.name}: failed to stop`);
      }
      cleanupSocket(sess.name);
    } else if (existingSession.status === "exited") {
      cleanupAll(sess.name);
      console.log(`  ○ ${sess.name} (cleaned up)`);
      stopped++;
    }
  }

  if (stopped === 0) {
    console.log("No sessions to stop.");
  } else {
    console.log(`Stopped ${stopped} session${stopped === 1 ? "" : "s"}.`);
  }

  // Warn if any stopped sessions are toml-managed
  const anyTomlManaged = sessions.some((sess) => {
    const existingSession = existing.find((s) => s.name === sess.name);
    return existingSession?.metadata?.tags?.ptyfile;
  });
  if (anyTomlManaged && stopped > 0) {
    console.error("\nNote: strategy tags will be restored on the next 'pty up'.");
  }
}

async function cmdRestart(name: string, force = false): Promise<void> {
  try {
    validateName(name);
  } catch (e: any) {
    console.error(e.message);
    process.exit(1);
  }

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
    if (!force) {
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
  const supStatus = tags["supervisor.status"];
  if (supStatus === "failed") return " \x1b[31m[failed]\x1b[0m";
  if (tags.strategy === "permanent") return " \x1b[33m[permanent]\x1b[0m";
  if (tags.strategy === "temporary") return " \x1b[2m[temporary]\x1b[0m";
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
