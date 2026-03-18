import * as tty from "node:tty";
import { attach } from "../client.ts";
import { listSessions, validateName, acquireLock, releaseLock, cleanupAll, getSession } from "../sessions.ts";
import { spawnDaemon, resolveCommand } from "../spawn.ts";
import { parseKey } from "./input.ts";
import {
  enterAltScreen,
  leaveAltScreen,
  clearScreen,
  hideCursor,
  showCursor,
} from "./render.ts";
import {
  createListState,
  handleListKey,
  renderList,
  updateSessions,
  type ListState,
} from "./screen-list.ts";
import {
  createCreateState,
  handleCreateKey,
  renderCreate,
  type CreateState,
} from "./screen-create.ts";

type Screen = "list" | "create";

const stdout = process.stdout as tty.WriteStream;
const stdin = process.stdin;

export async function runInteractive(): Promise<void> {
  let currentScreen: Screen = "list";
  let listState: ListState;
  let createState: CreateState | null = null;

  // Load sessions
  const sessions = await listSessions();
  const width = stdout.columns ?? 80;
  const height = stdout.rows ?? 24;
  listState = createListState(sessions, width, height);

  // Enter TUI mode
  stdout.write(enterAltScreen() + hideCursor() + clearScreen());

  if (stdin.isTTY) {
    stdin.setRawMode(true);
  }
  stdin.resume();

  // Render current screen
  function render(): void {
    stdout.write(clearScreen());
    if (currentScreen === "list") {
      stdout.write(renderList(listState));
    } else if (currentScreen === "create" && createState) {
      stdout.write(renderCreate(createState));
    }
  }

  // Handle resize
  function onResize(): void {
    const w = stdout.columns ?? 80;
    const h = stdout.rows ?? 24;
    listState.termWidth = w;
    listState.termHeight = h;
    if (createState) {
      createState.termWidth = w;
      createState.termHeight = h;
    }
    render();
  }
  stdout.on("resize", onResize);

  // Exit TUI mode
  function exitTui(): void {
    stdout.removeListener("resize", onResize);
    stdin.removeAllListeners("data");
    if (stdin.isTTY && stdin.isRaw) {
      stdin.setRawMode(false);
    }
    stdout.write(showCursor() + leaveAltScreen());
  }

  // Pause TUI for attach (leave alt screen, raw mode off, hand off stdin)
  function pauseTui(): void {
    stdout.removeListener("resize", onResize);
    stdin.removeAllListeners("data");
    if (stdin.isTTY && stdin.isRaw) {
      stdin.setRawMode(false);
    }
    stdin.pause();
    stdout.write(showCursor() + leaveAltScreen());
  }

  // Resume TUI after attach returns
  async function resumeTui(): Promise<void> {
    // Reload sessions
    const sessions = await listSessions();
    const w = stdout.columns ?? 80;
    const h = stdout.rows ?? 24;
    listState.termWidth = w;
    listState.termHeight = h;
    updateSessions(listState, sessions);
    currentScreen = "list";
    createState = null;

    stdout.write(enterAltScreen() + hideCursor() + clearScreen());
    if (stdin.isTTY) {
      stdin.setRawMode(true);
    }
    stdin.resume();
    stdout.on("resize", onResize);
    setupInput();
    render();
  }

  // Attach to a session and return when detach/exit happens
  function doAttach(name: string): void {
    pauseTui();

    attach({
      name,
      onDetach: async () => {
        await resumeTui();
      },
      onExit: async (_code) => {
        await resumeTui();
      },
    });
  }

  // Create a new session and attach
  async function doCreate(dir: string, name: string, command: string): Promise<void> {
    pauseTui();

    try {
      validateName(name);
    } catch (e: any) {
      console.error(e.message);
      await resumeTui();
      return;
    }

    // Check if session already exists
    let existing;
    try {
      existing = await getSession(name);
    } catch {
      // Corrupted metadata — proceed as if no session exists
      existing = null;
    }

    if (existing?.status === "running") {
      // Attach to existing
      attach({
        name,
        onDetach: async () => { await resumeTui(); },
        onExit: async () => { await resumeTui(); },
      });
      return;
    }

    if (!acquireLock(name)) {
      console.error(`Session "${name}" is being created by another process.`);
      await resumeTui();
      return;
    }

    // Clean up dead session with same name
    if (existing?.status === "exited") {
      cleanupAll(name);
    }

    // Parse command into cmd + args
    const parts = command.split(/\s+/);
    const cmd = parts[0];
    const args = parts.slice(1);

    let resolvedCmd: string;
    try {
      resolvedCmd = resolveCommand(cmd);
    } catch (e: any) {
      releaseLock(name);
      console.error(e.message);
      await resumeTui();
      return;
    }

    try {
      await spawnDaemon(name, resolvedCmd, args, cmd, dir);
    } catch (e: any) {
      releaseLock(name);
      console.error(e.message);
      await resumeTui();
      return;
    } finally {
      releaseLock(name);
    }

    attach({
      name,
      onDetach: async () => { await resumeTui(); },
      onExit: async () => { await resumeTui(); },
    });
  }

  function setupInput(): void {
    stdin.on("data", (data: Buffer) => {
      const keys = parseKey(data);
      for (const key of keys) {
        if (currentScreen === "list") {
          const action = handleListKey(listState, key);
          switch (action.type) {
            case "attach":
              if (action.session) {
                doAttach(action.session.name);
                return;
              }
              break;
            case "create": {
              currentScreen = "create";
              const names = listState.sessions.map((s) => s.name);
              createState = createCreateState(
                listState.termWidth,
                listState.termHeight,
                names
              );
              render();
              return;
            }
            case "quit":
              exitTui();
              process.exit(0);
              return;
            case "none":
              render();
              break;
          }
        } else if (currentScreen === "create" && createState) {
          const action = handleCreateKey(createState, key);
          switch (action.type) {
            case "create":
              if (action.dir && action.name && action.command) {
                doCreate(action.dir, action.name, action.command);
                return;
              }
              break;
            case "cancel":
              currentScreen = "list";
              createState = null;
              render();
              return;
            case "none":
              render();
              break;
          }
        }
      }
    });
  }

  setupInput();
  render();
}
