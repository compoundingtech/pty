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
  cleanupAll,
  cleanupSocket,
  validateName,
  acquireLock,
  releaseLock,
  type SessionInfo,
} from "./sessions.ts";
import { spawnDaemon, resolveCommand } from "./spawn.ts";
import { runInteractive } from "./tui/interactive.ts";
import { EventFollower, readRecentEvents, formatEvent } from "./events.ts";

function usage(): void {
  console.log(`Usage:
  pty                                       Interactive session manager
  pty run -- <command> [args...]            Create a session and attach (auto-named)
  pty run --name <n> -- <command> [args...] Create a named session and attach
  pty run -d -- <command> [args...]        Create in the background
  pty run -a -- <command> [args...]        Create or attach if already running
  pty attach <name>                        Attach to an existing session
  pty attach -r <name>                     Attach, auto-restart if exited
  pty peek <name>                          Print current screen and exit
  pty peek --plain <name>                  Print current screen as plain text (no ANSI)
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
  pty list                                 List active sessions
  pty list --json                          List sessions as JSON
  pty kill <name>                          Kill or remove a session
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

  if (args.length === 0) {
    await runInteractive();
    return;
  }

  const command = args[0];

  switch (command) {
    case "interactive":
    case "i": {
      await runInteractive();
      break;
    }

    case "run": {
      // Parse flags before the -- separator
      let detach = false;
      let attachExisting = false;
      let ephemeral = false;
      let name: string | null = null;
      let i = 1;
      while (i < args.length && args[i] !== "--") {
        if (args[i] === "-d" || args[i] === "--detach") { detach = true; i++; }
        else if (args[i] === "-a" || args[i] === "--attach") { attachExisting = true; i++; }
        else if (args[i] === "-e" || args[i] === "--ephemeral") { ephemeral = true; i++; }
        else if (args[i] === "--name" && i + 1 < args.length) { name = args[i + 1]; i += 2; }
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

      const displayCmd = cmd;
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
        let candidate = autoName(displayCmd, cmdArgs);
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

      await cmdRun(name, cmd, cmdArgs, detach, attachExisting, displayCmd, ephemeral);
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

    case "peek": {
      let follow = false;
      let plain = false;
      let pi = 1;
      while (pi < args.length && args[pi].startsWith("-")) {
        if (args[pi] === "-f" || args[pi] === "--follow") follow = true;
        else if (args[pi] === "--plain") plain = true;
        else break;
        pi++;
      }
      const peekName = args[pi];
      if (!peekName) {
        console.error("Usage: pty peek [-f] [--plain] <name>");
        process.exit(1);
      }
      try {
        validateName(peekName);
      } catch (e: any) {
        console.error(e.message);
        process.exit(1);
      }
      cmdPeek(peekName, follow, plain);
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
      let ei = 1;
      while (ei < args.length && args[ei].startsWith("-")) {
        if (args[ei] === "--all") { all = true; ei++; }
        else if (args[ei] === "--recent") { recent = true; ei++; }
        else if (args[ei] === "--json") { json = true; ei++; }
        else break;
      }
      const eventsName = args[ei];

      if (!all && !eventsName) {
        console.error("Usage: pty events [--all] [--recent] [--json] [<name>]");
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

      await cmdEvents(eventsName ?? null, { all, recent, json });
      break;
    }

    case "list":
    case "ls": {
      const jsonFlag = args.includes("--json");
      await cmdList(jsonFlag);
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

  // Clean up any dead session with the same name, but preserve cwd
  // so that `run -a` re-creates the session in the original directory.
  const previousCwd = session?.status === "exited" ? session.metadata?.cwd : undefined;
  if (session?.status === "exited") {
    cleanupAll(name);
  }

  try {
    await spawnDaemon(name, command, args, displayCommand, previousCwd, ephemeral);
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

  const cmd = [meta.displayCommand, ...meta.args].join(" ");
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
  await spawnDaemon(session.name, meta.command, meta.args, meta.displayCommand, meta.cwd);
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

function cmdPeek(name: string, follow: boolean, plain: boolean): void {
  peek({
    name,
    follow,
    plain,
    onDetach: () => process.exit(0),
    onExit: (code) => process.exit(code),
  });
}

async function cmdList(json = false): Promise<void> {
  const sessions = await listSessions();

  if (json) {
    const output = sessions.map((s) => ({
      name: s.name,
      status: s.status,
      pid: s.pid,
      command: s.metadata
        ? [s.metadata.displayCommand, ...s.metadata.args].join(" ")
        : null,
      cwd: s.metadata?.cwd ?? null,
      createdAt: s.metadata?.createdAt ?? null,
      exitCode: s.metadata?.exitCode ?? null,
      exitedAt: s.metadata?.exitedAt ?? null,
    }));
    console.log(JSON.stringify(output));
    return;
  }

  if (sessions.length === 0) {
    console.log("No active sessions.");
    return;
  }

  const running = sessions.filter((s) => s.status === "running");
  const exited = sessions.filter((s) => s.status === "exited");

  if (running.length > 0) {
    console.log("Active sessions:");
    for (const session of running) {
      const cmd = session.metadata
        ? [session.metadata.displayCommand, ...session.metadata.args].join(" ")
        : "unknown";
      const cwd = session.metadata?.cwd
        ? shortPath(session.metadata.cwd)
        : "";
      console.log(`  ${session.name} (pid: ${session.pid}) — ${cwd} — ${cmd}`);
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
        ? [meta.displayCommand, ...meta.args].join(" ")
        : "";
      console.log(`  ${session.name} (exited with code ${code}, ${ago}) — ${cwd} — ${cmd}`);
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
    ? [meta.displayCommand, ...meta.args].join(" ")
    : "unknown";
  const cwd = meta?.cwd ? shortPath(meta.cwd) : "unknown";

  console.log(`Session: ${stats.name}`);
  console.log(`  Command:    ${cmd}`);
  console.log(`  CWD:        ${cwd}`);
  console.log(`  Uptime:     ${formatUptime(stats.uptimeSeconds)}`);
  console.log(`  Process:    ${stats.process.alive ? "running" : `exited (code ${stats.process.exitCode})`}${stats.process.pid ? ` (pid ${stats.process.pid})` : ""}`);
  if (stats.process.resources) {
    console.log(`  CPU:        ${stats.process.resources.cpuPercent.toFixed(1)}%`);
    console.log(`  Memory:     ${formatMemory(stats.process.resources.rssKb)}`);
  }
  console.log(`  Daemon:     pid ${stats.daemon.pid}${stats.daemon.resources ? `, ${formatMemory(stats.daemon.resources.rssKb)}` : ""}`);
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

  try {
    process.kill(session.pid, "SIGTERM");
    console.log(`Session "${name}" killed.`);
  } catch {
    console.error(`Failed to kill session "${name}".`);
  }
  cleanupSocket(name);
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
  await spawnDaemon(name, meta.command, meta.args, meta.displayCommand, meta.cwd);
  console.log(`Session "${name}" restarted.`);
  doAttach(name);
}

async function cmdEvents(
  name: string | null,
  opts: { all: boolean; recent: boolean; json: boolean }
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
