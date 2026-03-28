import * as os from "node:os";
import * as path from "node:path";
import * as readline from "node:readline/promises";
import { spawnSync } from "node:child_process";
import { attach, peek, send } from "./client.ts";
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
  pty list                                 List active sessions
  pty list --json                          List sessions as JSON
  pty kill <name>                          Kill or remove a session
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
      let name: string | null = null;
      let i = 1;
      while (i < args.length && args[i] !== "--") {
        if (args[i] === "-d" || args[i] === "--detach") { detach = true; i++; }
        else if (args[i] === "-a" || args[i] === "--attach") { attachExisting = true; i++; }
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

      await cmdRun(name, cmd, cmdArgs, detach, attachExisting, displayCmd);
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

    case "list":
    case "ls": {
      const jsonFlag = args.includes("--json");
      await cmdList(jsonFlag);
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
  displayCommand: string
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
    await spawnDaemon(name, command, args, displayCommand, previousCwd);
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
      console.log(`  ${session.name} (exited with code ${code}, ${ago}) — ${cwd}`);
    }
  }
}

async function cmdKill(name: string): Promise<void> {
  const session = await getSession(name);

  if (!session) {
    console.error(`Session "${name}" not found.`);
    process.exit(1);
  }

  if (session.status === "running" && session.pid) {
    try {
      process.kill(session.pid, "SIGTERM");
      console.log(`Session "${name}" killed.`);
    } catch {
      console.error(`Failed to kill session "${name}".`);
    }
    cleanupSocket(name);
  }

  cleanupAll(name);
  if (session.status === "exited") {
    console.log(`Session "${name}" removed.`);
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
  await spawnDaemon(name, meta.command, meta.args, meta.displayCommand, meta.cwd);
  console.log(`Session "${name}" restarted.`);
  doAttach(name);
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

main().catch((err) => {
  console.error(err.message);
  process.exit(1);
});
