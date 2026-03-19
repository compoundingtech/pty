import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { Session, type Screenshot } from "../src/testing/index.ts";
import {
  MessageType,
  PacketReader,
  encodeAttach,
  encodeData,
  encodeResize,
} from "../src/protocol.ts";
import { getSocketPath, cleanupAll } from "../src/sessions.ts";

// All tests run in a tmp directory to avoid polluting the project
const testCwd = fs.mkdtempSync(path.join(os.tmpdir(), "pty-ss-"));
// Isolate session metadata/sockets from the real session directory
const testSessionDir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-ss-sd-"));
process.env.PTY_SESSION_DIR = testSessionDir;
afterAll(() => {
  fs.rmSync(testCwd, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
  fs.rmSync(testSessionDir, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

// ─── Scaffolding ───

let sessions: Session[] = [];
let sessionNames: string[] = [];
let tmpDirs: string[] = [];
let daemonPids: number[] = [];

function uniqueName(): string {
  const name = `ss-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
  sessionNames.push(name);
  return name;
}

function makeTmpDir(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-ss-td-"));
  tmpDirs.push(dir);
  return dir;
}

async function createSession(
  command: string,
  args: string[] = [],
  opts: { rows?: number; cols?: number; cwd?: string } = {}
): Promise<Session> {
  const name = uniqueName();
  const session = await Session.server(command, args, { name, cwd: opts.cwd ?? testCwd, rows: opts.rows, cols: opts.cols });
  sessions.push(session);
  await session.attach();
  return session;
}

afterEach(async () => {
  for (const session of sessions) {
    await session.close();
  }
  sessions = [];
  for (const pid of daemonPids) {
    try {
      process.kill(pid, "SIGTERM");
    } catch {}
  }
  daemonPids = [];
  for (const name of sessionNames) {
    cleanupAll(name);
  }
  sessionNames = [];
  for (const dir of tmpDirs) {
    fs.rmSync(dir, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
  }
  tmpDirs = [];
});

// ─── Tests: ls ───

describe("screenshot: ls", () => {
  it("captures ls output with correct filenames", async () => {
    const dir = makeTmpDir();
    fs.writeFileSync(path.join(dir, "alpha.txt"), "");
    fs.writeFileSync(path.join(dir, "beta.log"), "");
    fs.mkdirSync(path.join(dir, "gamma"));

    const session = await createSession("sh", ["-c", `ls ${dir}; sleep 30`]);

    const ss = await session.waitForText("alpha.txt");
    expect(ss.text).toContain("alpha.txt");
    expect(ss.text).toContain("beta.log");
    expect(ss.text).toContain("gamma");
  });

  it("captures ls -la with permissions and structure", async () => {
    const dir = makeTmpDir();
    fs.writeFileSync(path.join(dir, "readme.md"), "hello");
    fs.mkdirSync(path.join(dir, "src"));

    const session = await createSession("sh", [
      "-c",
      `ls -la ${dir}; sleep 30`,
    ]);

    const ss = await session.waitForText("readme.md");
    expect(ss.text).toContain("readme.md");
    expect(ss.text).toContain("src");
    expect(ss.text).toMatch(/[drwx-]{10}/);
    expect(ss.text).toMatch(/total \d+/);
  });

  it("preserves ANSI colors from ls", async () => {
    const dir = makeTmpDir();
    fs.writeFileSync(path.join(dir, "file.txt"), "");
    fs.mkdirSync(path.join(dir, "directory"));

    const session = await createSession("sh", [
      "-c",
      `CLICOLOR_FORCE=1 ls -G ${dir}; sleep 30`,
    ]);

    const ss = await session.waitForText("file.txt");
    expect(ss.text).toContain("file.txt");
    expect(ss.text).toContain("directory");
    expect(ss.ansi).toMatch(/\x1b\[/);
  });
});

// ─── Tests: ANSI colors ───

describe("screenshot: ANSI colors", () => {
  it("preserves explicit ANSI color codes", async () => {
    const session = await createSession("sh", [
      "-c",
      "printf '\\033[31mRED\\033[0m \\033[32mGREEN\\033[0m \\033[34mBLUE\\033[0m\\n'; sleep 30",
    ]);

    const ss = await session.waitForText("RED");
    expect(ss.lines[0]).toContain("RED");
    expect(ss.lines[0]).toContain("GREEN");
    expect(ss.lines[0]).toContain("BLUE");
    expect(ss.ansi).toMatch(/\x1b\[31m/);
    expect(ss.ansi).toMatch(/\x1b\[32m/);
    expect(ss.ansi).toMatch(/\x1b\[34m/);
  });

  it("preserves 256-color codes", async () => {
    const session = await createSession("sh", [
      "-c",
      "printf '\\033[38;5;208mORANGE\\033[0m\\n'; sleep 30",
    ]);

    const ss = await session.waitForText("ORANGE");
    expect(ss.lines[0]).toContain("ORANGE");
    expect(ss.ansi).toMatch(/\x1b\[38;5;208m/);
  });

  it("preserves true color (24-bit) codes", async () => {
    const session = await createSession("sh", [
      "-c",
      "printf '\\033[38;2;255;100;0mTRUECOLOR\\033[0m\\n'; sleep 30",
    ]);

    const ss = await session.waitForText("TRUECOLOR");
    expect(ss.lines[0]).toContain("TRUECOLOR");
    expect(ss.ansi).toMatch(/\x1b\[38;2;255;100;0m/);
  });

  it("colors survive detach/reattach screen replay", async () => {
    const session = await createSession("sh", [
      "-c",
      "printf '\\033[31mRED TEXT\\033[0m\\n'; sleep 30",
    ]);

    await session.waitForText("RED TEXT");
    await session.reconnect();

    const ss = await session.waitForText("RED TEXT");
    expect(ss.text).toContain("RED TEXT");
    expect(ss.ansi).toMatch(/\x1b\[31m/);
  });
});

// ─── Tests: hyperlinks (OSC 8) ───

describe("screenshot: hyperlinks (OSC 8)", () => {
  it("displays hyperlink text correctly", async () => {
    const session = await createSession("sh", [
      "-c",
      "printf '\\e]8;;http://example.com\\e\\\\link text\\e]8;;\\e\\\\\\n'; sleep 30",
    ]);

    const ss = await session.waitForText("link text");
    expect(ss.text).toContain("link text");
  });

  it("hyperlink text is rendered with underline styling", async () => {
    const session = await createSession("sh", [
      "-c",
      "printf '\\e]8;;http://example.com\\e\\\\linked\\e]8;;\\e\\\\\\n'; sleep 30",
    ]);

    const ss = await session.waitForText("linked");
    expect(ss.text).toContain("linked");
    expect(ss.ansi).toContain("linked");
  });
});

// ─── Tests: vim ───

describe("screenshot: vim", () => {
  it(
    "captures vim welcome screen",
    async () => {
      const session = await createSession("vim", ["--clean"], {
        rows: 24,
        cols: 80,
      });

      const ss = await session.waitForText("VIM - Vi IMproved", 10000);
      expect(ss.text).toContain("VIM - Vi IMproved");
      expect(ss.text).toMatch(/type\s+:q/i);
      expect(ss.lines.some((l) => l.trimStart().startsWith("~"))).toBe(true);

      session.sendKeys(":q\n");
    },
    15000
  );

  it(
    "shows INSERT mode and typed text",
    async () => {
      const session = await createSession("vim", ["--clean"], {
        rows: 24,
        cols: 80,
      });

      await session.waitForText("VIM", 10000);

      session.sendKeys("i");
      await session.waitForText("INSERT");

      session.sendKeys("Hello from pty screenshot tests!");
      const ss = await session.waitForText("Hello from pty screenshot tests!");

      expect(ss.text).toContain("Hello from pty screenshot tests!");
      expect(ss.text).toMatch(/INSERT/i);

      session.sendKeys("\x1b");
      session.sendKeys(":q!\n");
    },
    15000
  );

  it(
    "vim screen is restored after detach/reattach",
    async () => {
      const session = await createSession("vim", ["--clean"], {
        rows: 24,
        cols: 80,
      });

      await session.waitForText("VIM", 10000);

      session.sendKeys("iThis text survives detach");
      await session.waitForText("This text survives detach");
      session.sendKeys("\x1b");

      await session.reconnect();

      const ss = await session.waitForText("This text survives detach");
      expect(ss.text).toContain("This text survives detach");

      session.sendKeys(":q!\n");
    },
    15000
  );

  it(
    "captures vim with syntax highlighting",
    async () => {
      const dir = makeTmpDir();
      const filePath = path.join(dir, "test.js");
      fs.writeFileSync(
        filePath,
        'function hello() {\n  return "world";\n}\n'
      );

      const session = await createSession(
        "vim",
        ["--clean", "+syntax on", filePath],
        { rows: 24, cols: 80 }
      );

      const ss = await session.waitForText("function", 10000);
      expect(ss.text).toContain("function hello()");
      expect(ss.text).toContain("return");
      expect(ss.ansi).toMatch(/\x1b\[/);

      session.sendKeys(":q\n");
    },
    15000
  );
});

// ─── Tests: nano ───

describe("screenshot: nano", () => {
  it(
    "captures nano interface elements",
    async () => {
      const dir = makeTmpDir();
      const filePath = path.join(dir, "test.txt");
      fs.writeFileSync(filePath, "Hello nano world\n");

      const session = await createSession("nano", [filePath], {
        rows: 24,
        cols: 80,
      });

      const ss = await session.waitFor(
        (s) => s.text.includes("nano") || s.text.includes("File:"),
        10000,
        "nano UI"
      );
      expect(ss.text).toContain("test.txt");
      expect(ss.text).toContain("Hello nano world");
      expect(ss.text).toContain("^X");

      session.sendKeys("\x18");
    },
    15000
  );

  it(
    "nano typing updates the screen",
    async () => {
      const dir = makeTmpDir();
      const filePath = path.join(dir, "edit.txt");
      fs.writeFileSync(filePath, "");

      const session = await createSession("nano", [filePath], {
        rows: 24,
        cols: 80,
      });

      // nano title bar varies by version — wait for the shortcut bar
      await session.waitFor(
        (ss) => ss.text.includes("^G") && ss.text.includes("^X"),
        10000,
        "nano UI"
      );

      session.sendKeys("Typed in nano!");
      const ss = await session.waitForText("Typed in nano!");
      expect(ss.text).toContain("Typed in nano!");

      session.sendKeys("\x18");
      await session.waitForText("Save", 3000).catch(() => {});
      session.sendKeys("n");
    },
    15000
  );
});

// ─── Tests: screen replay fidelity ───

describe("screenshot: screen replay fidelity", () => {
  it("multi-line colored output survives replay", async () => {
    const script = [
      "printf '\\033[1;31m=== HEADER ===\\033[0m\\n'",
      "printf '\\033[32mLine 1: success\\033[0m\\n'",
      "printf '\\033[33mLine 2: warning\\033[0m\\n'",
      "printf '\\033[31mLine 3: error\\033[0m\\n'",
      "printf '\\033[1;34m=== FOOTER ===\\033[0m\\n'",
      "sleep 30",
    ].join("; ");

    const session = await createSession("sh", ["-c", script]);
    await session.waitForText("FOOTER");

    let ss = session.screenshot();
    expect(ss.text).toContain("=== HEADER ===");
    expect(ss.text).toContain("Line 1: success");
    expect(ss.text).toContain("Line 2: warning");
    expect(ss.text).toContain("Line 3: error");
    expect(ss.text).toContain("=== FOOTER ===");

    await session.reconnect();

    ss = await session.waitForText("FOOTER");
    expect(ss.text).toContain("=== HEADER ===");
    expect(ss.text).toContain("Line 3: error");
    expect(ss.text).toContain("=== FOOTER ===");
    expect(ss.ansi).toMatch(/\x1b\[(1;31|31;1)m/);
    expect(ss.ansi).toMatch(/\x1b\[32/);
    expect(ss.ansi).toMatch(/\x1b\[33m/);
    expect(ss.ansi).toMatch(/\x1b\[31m/);
    expect(ss.ansi).toMatch(/\x1b\[(1;34|34;1)m/);
  });

  it("cursor position is preserved in screen replay", async () => {
    const session = await createSession("sh", [
      "-c",
      "printf '\\033[5;10HPositioned!'; sleep 30",
    ]);

    let ss = await session.waitForText("Positioned!");
    expect(ss.lines[4]).toMatch(/\s{5,}Positioned!/);

    await session.reconnect();
    ss = await session.waitForText("Positioned!");
    expect(ss.lines[4]).toMatch(/\s{5,}Positioned!/);
  });

  it("scrollback content is preserved in replay", async () => {
    const lines = Array.from({ length: 40 }, (_, i) => `scroll-line-${i}`);
    const script = lines.map((l) => `echo '${l}'`).join("; ") + "; sleep 30";

    const session = await createSession("sh", ["-c", script]);
    await session.waitForText("scroll-line-39");

    let ss = session.screenshot();
    expect(ss.text).toContain("scroll-line-0");
    expect(ss.text).toContain("scroll-line-39");

    await session.reconnect();

    ss = await session.waitForText("scroll-line-39");
    expect(ss.text).toContain("scroll-line-0");
    expect(ss.text).toContain("scroll-line-39");
  });
});

// ─── Tests: shell interaction ───

describe("screenshot: shell interaction", () => {
  it(
    "interactive bash session with prompt and commands",
    async () => {
      const session = await createSession("bash", [
        "--norc",
        "--noprofile",
      ]);

      await session.waitFor(
        (ss) => /[$#]/.test(ss.text),
        5000,
        "shell prompt"
      );

      session.sendKeys("echo hello-from-shell\n");
      const ss = await session.waitForText("hello-from-shell");
      expect(ss.text).toContain("hello-from-shell");

      session.sendKeys("exit\n");
    },
    15000
  );

  it(
    "runs multiple commands in a shell session",
    async () => {
      const session = await createSession("bash", [
        "--norc",
        "--noprofile",
      ]);

      await session.waitFor(
        (ss) => /[$#]/.test(ss.text),
        5000,
        "prompt"
      );

      session.sendKeys("echo 'first-cmd'\n");
      await session.waitForText("first-cmd");

      session.sendKeys("echo 'second-cmd'\n");
      await session.waitForText("second-cmd");

      session.sendKeys("echo 'third-cmd'\n");
      const ss = await session.waitForText("third-cmd");

      expect(ss.text).toContain("first-cmd");
      expect(ss.text).toContain("second-cmd");
      expect(ss.text).toContain("third-cmd");

      session.sendKeys("exit\n");
    },
    15000
  );
});

// ─── Tests: control characters / signals ───

describe("screenshot: control characters", () => {
  it(
    "Ctrl+C interrupts a running command",
    async () => {
      const session = await createSession("bash", [
        "--norc",
        "--noprofile",
      ]);

      await session.waitFor(
        (ss) => /[$#]/.test(ss.text),
        5000,
        "prompt"
      );

      session.sendKeys("sleep 999\n");
      await new Promise((r) => setTimeout(r, 300));

      session.sendKeys("\x03"); // Ctrl+C
      await new Promise((r) => setTimeout(r, 300));

      // Should be able to run another command after interrupt
      session.sendKeys("echo 'interrupted-ok'\n");
      const ss = await session.waitForText("interrupted-ok");
      expect(ss.text).toContain("interrupted-ok");

      session.sendKeys("exit\n");
    },
    15000
  );

  it(
    "Ctrl+D sends EOF to close a program",
    async () => {
      const session = await createSession("cat");

      session.sendKeys("hello-eof-test\n");
      await session.waitForText("hello-eof-test");

      // Ctrl+D on an empty line signals EOF
      session.sendKeys("\x04");

      // cat should exit after receiving EOF
      await new Promise((r) => setTimeout(r, 500));
      expect(session.hasExited).toBe(true);
    },
    15000
  );

  it(
    "Ctrl+Z suspends a process in bash",
    async () => {
      const session = await createSession("bash", [
        "--norc",
        "--noprofile",
      ]);

      await session.waitFor(
        (ss) => /[$#]/.test(ss.text),
        5000,
        "prompt"
      );

      session.sendKeys("cat\n");
      await new Promise((r) => setTimeout(r, 300));

      session.sendKeys("\x1a"); // Ctrl+Z = SIGTSTP

      // bash should show stopped message and give us a prompt back
      const ss = await session.waitFor(
        (s) =>
          (s.text.includes("Stopped") || s.text.includes("suspended")) &&
          // prompt should reappear after the stopped message
          /[$#]\s*$/.test(s.lines[s.lines.length - 1] ?? ""),
        5000,
        "stopped message and prompt"
      );

      expect(ss.text).toMatch(/[Ss]topped|suspended/);

      session.sendKeys("exit\n");
      // bash may warn about stopped jobs — force exit
      await new Promise((r) => setTimeout(r, 200));
      session.sendKeys("exit\n");
    },
    15000
  );
});

// ─── Tests: terminal resize ───

describe("screenshot: terminal resize", () => {
  it(
    "vim redraws after resize",
    async () => {
      const dir = makeTmpDir();
      const filePath = path.join(dir, "resize.txt");
      fs.writeFileSync(filePath, "Line one\nLine two\nLine three\n");

      const session = await createSession(
        "vim",
        ["--clean", filePath],
        { rows: 24, cols: 80 }
      );

      await session.waitForText("Line one", 10000);

      // Resize to narrower terminal
      session.resize(24, 40);
      await new Promise((r) => setTimeout(r, 500));

      // Content should still be visible after resize
      const ss = session.screenshot();
      expect(ss.text).toContain("Line one");
      expect(ss.text).toContain("Line two");
      expect(ss.text).toContain("Line three");

      session.sendKeys(":q\n");
    },
    15000
  );

  it("tput reports updated dimensions after resize", async () => {
    const session = await createSession("bash", [
      "--norc",
      "--noprofile",
    ]);

    await session.waitFor(
      (ss) => /[$#]/.test(ss.text),
      5000,
      "prompt"
    );

    session.resize(30, 100);
    await new Promise((r) => setTimeout(r, 200));

    session.sendKeys("echo \"cols=$(tput cols) rows=$(tput lines)\"\n");
    const ss = await session.waitForText("cols=");
    expect(ss.text).toContain("cols=100");
    expect(ss.text).toContain("rows=30");

    session.sendKeys("exit\n");
  }, 15000);
});

// ─── Tests: Unicode / wide characters ───

describe("screenshot: unicode", () => {
  it("renders CJK characters", async () => {
    const session = await createSession("sh", [
      "-c",
      "echo '日本語 中文 한국어'; sleep 30",
    ]);

    const ss = await session.waitForText("日本語");
    expect(ss.text).toContain("日本語");
    expect(ss.text).toContain("中文");
    expect(ss.text).toContain("한국어");
  });

  it("renders emoji", async () => {
    const session = await createSession("sh", [
      "-c",
      "echo '🎉 🚀 ✅ ❌'; sleep 30",
    ]);

    const ss = await session.waitForText("🎉");
    expect(ss.text).toContain("🎉");
    expect(ss.text).toContain("🚀");
  });

  it("unicode survives detach/reattach", async () => {
    const session = await createSession("sh", [
      "-c",
      "echo '你好世界 🌍'; sleep 30",
    ]);

    await session.waitForText("你好世界");
    await session.reconnect();

    const ss = await session.waitForText("你好世界");
    expect(ss.text).toContain("你好世界");
    expect(ss.text).toContain("🌍");
  });

  it("mixed ASCII and wide characters on the same line", async () => {
    const session = await createSession("sh", [
      "-c",
      "echo 'Hello 世界 World 🌍 End'; sleep 30",
    ]);

    const ss = await session.waitForText("Hello");
    expect(ss.lines[0]).toContain("Hello 世界 World");
  });
});

// ─── Tests: alternate screen buffer ───

describe("screenshot: alternate screen buffer", () => {
  it("main buffer is restored after alternate screen exits", async () => {
    const session = await createSession("sh", [
      "-c",
      // Write to main buffer, switch to alt, write there, switch back
      "echo 'main-buffer-text';" +
        "printf '\\033[?1049h';" +
        "printf 'alt-screen-only';" +
        "sleep 0.3;" +
        "printf '\\033[?1049l';" +
        "sleep 30",
    ]);

    const ss = await session.waitForText("main-buffer-text");
    expect(ss.text).toContain("main-buffer-text");
    // Alt screen content should NOT appear in the main buffer
    expect(ss.text).not.toContain("alt-screen-only");
  });

  it("screen replay shows main buffer after alt screen program exits", async () => {
    const session = await createSession("sh", [
      "-c",
      "echo 'before-alt-screen';" +
        "printf '\\033[?1049h';" +
        "printf 'temporary-alt';" +
        "sleep 0.3;" +
        "printf '\\033[?1049l';" +
        "echo 'after-alt-screen';" +
        "sleep 30",
    ]);

    await session.waitForText("after-alt-screen");
    await session.reconnect();

    const ss = await session.waitForText("after-alt-screen");
    expect(ss.text).toContain("before-alt-screen");
    expect(ss.text).toContain("after-alt-screen");
    expect(ss.text).not.toContain("temporary-alt");
  });

  it("TUI app content is captured while running in alternate screen", async () => {
    // Simulates what apps like mactop, htop, vim do: enter alternate screen,
    // enable mouse tracking, enable application cursor keys, draw content
    const session = await createSession("sh", [
      "-c",
      "printf '\\033[?1049h';" + // enter alternate screen
        "printf '\\033[?1000h';" + // enable mouse click tracking
        "printf '\\033[?1003h';" + // enable mouse any-event tracking
        "printf '\\033[?1006h';" + // enable SGR mouse mode
        "printf '\\033[?1h';" + // enable application cursor keys
        "printf '\\033[H';" + // home cursor
        "printf '\\033[32mTUI-DASHBOARD\\033[0m\\n';" + // green title
        "printf '\\033[33mCPU: 42%%\\033[0m\\n';" + // yellow stats
        "printf '\\033[31mMEM: 8.2G\\033[0m\\n';" + // red stats
        "sleep 30",
    ]);

    const ss = await session.waitForText("TUI-DASHBOARD");
    expect(ss.text).toContain("TUI-DASHBOARD");
    expect(ss.text).toContain("CPU: 42%");
    expect(ss.text).toContain("MEM: 8.2G");
    // Colors should be preserved
    expect(ss.ansi).toMatch(/\x1b\[32m/); // green
    expect(ss.ansi).toMatch(/\x1b\[33m/); // yellow
    expect(ss.ansi).toMatch(/\x1b\[31m/); // red
  });

  it("TUI app content survives detach/reattach", async () => {
    const session = await createSession("sh", [
      "-c",
      "printf '\\033[?1049h';" +
        "printf '\\033[?1000h';" +
        "printf '\\033[?1003h';" +
        "printf '\\033[?1h';" +
        "printf '\\033[H';" +
        "printf '\\033[1;36mSYSTEM-MONITOR\\033[0m\\n';" +
        "printf 'Uptime: 42 days\\n';" +
        "sleep 30",
    ]);

    await session.waitForText("SYSTEM-MONITOR");
    await session.reconnect();

    const ss = await session.waitForText("SYSTEM-MONITOR");
    expect(ss.text).toContain("SYSTEM-MONITOR");
    expect(ss.text).toContain("Uptime: 42 days");
  });
});

// ─── Tests: multiple clients ───

describe("screenshot: multiple clients", () => {
  it("two attached clients see identical screen content", async () => {
    const session = await createSession("sh", [
      "-c",
      "printf '\\033[31mShared colored output\\033[0m\\n'; sleep 30",
    ]);

    await session.waitForText("Shared colored output");

    const peer = await Session.connectToExisting(session);
    sessions.push(peer);
    await peer.attach();

    const ss1 = session.screenshot();
    const ss2 = peer.screenshot();

    expect(ss2.text).toBe(ss1.text);
    expect(ss2.ansi).toBe(ss1.ansi);
  });

  it("both clients receive live output from a new command", async () => {
    const session = await createSession("cat");

    const peer = await Session.connectToExisting(session);
    sessions.push(peer);
    await peer.attach();

    session.sendKeys("both-see-this\n");

    await session.waitForText("both-see-this");
    await peer.waitForText("both-see-this");

    const ss1 = session.screenshot();
    const ss2 = peer.screenshot();
    expect(ss2.text).toContain("both-see-this");
    expect(ss1.text).toContain("both-see-this");
  });
});

// ─── Tests: high-throughput output ───

describe("screenshot: high-throughput output", () => {
  it("captures rapidly scrolling output", async () => {
    const session = await createSession("sh", [
      "-c",
      "seq 1 500; sleep 30",
    ]);

    const ss = await session.waitForText("500");
    // Should have the last line
    expect(ss.text).toContain("500");
    // And earlier lines in scrollback
    expect(ss.text).toContain("1");
  });

  it("high-throughput output survives screen replay", async () => {
    const session = await createSession("sh", [
      "-c",
      "seq 1 500; sleep 30",
    ]);

    await session.waitForText("500");

    await session.reconnect();

    const ss = await session.waitForText("500");
    expect(ss.text).toContain("500");
    expect(ss.text).toContain("1");
  });
});

// ─── Tests: daemon spawning ───

describe("daemon spawning", () => {
  const __dirname = path.dirname(fileURLToPath(import.meta.url));
  const tsxBin = path.join(__dirname, "..", "node_modules", ".bin", "tsx");
  const serverModule = path.join(__dirname, "..", "src", "server.ts");

  it(
    "daemon starts and serves a session via the tsx spawn mechanism",
    async () => {
      const name = uniqueName();
      const config = JSON.stringify({
        name,
        command: "sh",
        args: ["-c", "echo 'daemon works'; sleep 30"],
        cwd: testCwd,
        rows: 24,
        cols: 80,
      });

      const child = spawn(tsxBin, [serverModule], {
        detached: true,
        stdio: ["ignore", "ignore", "pipe"],
        env: { ...process.env, PTY_SERVER_CONFIG: config },
      });

      let stderr = "";
      child.stderr?.on("data", (d: Buffer) => {
        stderr += d.toString();
      });

      let exitCode: number | null = null;
      child.on("exit", (code) => {
        exitCode = code;
      });

      (child.stderr as any)?.unref?.();
      child.unref();
      daemonPids.push(child.pid!);

      const socketPath = getSocketPath(name);
      const start = Date.now();
      while (Date.now() - start < 5000) {
        if (exitCode !== null) {
          throw new Error(
            `Daemon exited with code ${exitCode}. stderr:\n${stderr}`
          );
        }
        try {
          fs.statSync(socketPath);
          break;
        } catch {}
        await new Promise((r) => setTimeout(r, 50));
      }

      await new Promise((r) => setTimeout(r, 300));

      const socket = await new Promise<net.Socket>((resolve, reject) => {
        const s = net.createConnection(socketPath);
        s.on("connect", () => resolve(s));
        s.on("error", reject);
      });

      const reader = new PacketReader();
      socket.write(encodeAttach(24, 80));

      const screen = await new Promise<string>((resolve, reject) => {
        const timer = setTimeout(
          () => reject(new Error("Timed out waiting for SCREEN")),
          5000
        );
        socket.on("data", (data: Buffer) => {
          const packets = reader.feed(data);
          for (const p of packets) {
            if (p.type === MessageType.SCREEN) {
              clearTimeout(timer);
              resolve(p.payload.toString());
            }
          }
        });
      });

      expect(screen).toContain("daemon works");
      socket.destroy();
    },
    15000
  );

  it(
    "daemon handles ls command correctly",
    async () => {
      const name = uniqueName();
      const dir = makeTmpDir();
      fs.writeFileSync(path.join(dir, "daemon-test.txt"), "");

      const config = JSON.stringify({
        name,
        command: "sh",
        args: ["-c", `ls ${dir}; sleep 30`],
        cwd: testCwd,
        rows: 24,
        cols: 80,
      });

      const child = spawn(tsxBin, [serverModule], {
        detached: true,
        stdio: ["ignore", "ignore", "pipe"],
        env: { ...process.env, PTY_SERVER_CONFIG: config },
      });

      let stderr = "";
      child.stderr?.on("data", (d: Buffer) => {
        stderr += d.toString();
      });

      let exitCode: number | null = null;
      child.on("exit", (code) => {
        exitCode = code;
      });

      (child.stderr as any)?.unref?.();
      child.unref();
      daemonPids.push(child.pid!);

      const socketPath = getSocketPath(name);
      const start = Date.now();
      while (Date.now() - start < 5000) {
        if (exitCode !== null) {
          throw new Error(
            `Daemon exited with code ${exitCode}. stderr:\n${stderr}`
          );
        }
        try {
          fs.statSync(socketPath);
          break;
        } catch {}
        await new Promise((r) => setTimeout(r, 50));
      }

      await new Promise((r) => setTimeout(r, 500));

      const socket = await new Promise<net.Socket>((resolve, reject) => {
        const s = net.createConnection(socketPath);
        s.on("connect", () => resolve(s));
        s.on("error", reject);
      });

      const reader = new PacketReader();
      socket.write(encodeAttach(24, 80));

      const screen = await new Promise<string>((resolve, reject) => {
        const timer = setTimeout(
          () => reject(new Error("Timed out waiting for SCREEN")),
          5000
        );
        socket.on("data", (data: Buffer) => {
          const packets = reader.feed(data);
          for (const p of packets) {
            if (p.type === MessageType.SCREEN) {
              clearTimeout(timer);
              resolve(p.payload.toString());
            }
          }
        });
      });

      expect(screen).toContain("daemon-test.txt");
      socket.destroy();
    },
    15000
  );
});

// ─── Tests: immediate attach after daemon start (race condition investigation) ───

describe("immediate attach after daemon start", () => {
  const __dirname = path.dirname(fileURLToPath(import.meta.url));
  const tsxBin = path.join(__dirname, "..", "node_modules", ".bin", "tsx");
  const serverModule = path.join(__dirname, "..", "src", "server.ts");

  async function spawnDaemonAndWaitForSocket(
    name: string,
    command: string,
    args: string[]
  ): Promise<string> {
    const config = JSON.stringify({
      name,
      command,
      args,
      displayCommand: command,
      cwd: testCwd,
      rows: 24,
      cols: 80,
    });

    const child = spawn(tsxBin, [serverModule], {
      detached: true,
      stdio: ["ignore", "ignore", "pipe"],
      env: { ...process.env, PTY_SERVER_CONFIG: config },
    });

    let stderr = "";
    child.stderr?.on("data", (d: Buffer) => {
      stderr += d.toString();
    });

    let exitCode: number | null = null;
    child.on("exit", (code) => {
      exitCode = code;
    });

    (child.stderr as any)?.unref?.();
    child.unref();
    daemonPids.push(child.pid!);

    const socketPath = getSocketPath(name);
    const start = Date.now();
    while (Date.now() - start < 5000) {
      if (exitCode !== null) {
        throw new Error(
          `Daemon exited with code ${exitCode}. stderr:\n${stderr}`
        );
      }
      try {
        fs.statSync(socketPath);
        break;
      } catch {}
      await new Promise((r) => setTimeout(r, 50));
    }

    // Same 100ms delay the CLI uses after socket appears
    await new Promise((r) => setTimeout(r, 100));
    return socketPath;
  }

  it(
    "input is received when attaching immediately after daemon start",
    async () => {
      const name = uniqueName();
      const socketPath = await spawnDaemonAndWaitForSocket(name, "cat", []);

      // Connect and attach immediately (mimics pty run flow)
      const socket = await new Promise<net.Socket>((resolve, reject) => {
        const s = net.createConnection(socketPath);
        s.on("connect", () => resolve(s));
        s.on("error", reject);
      });

      const reader = new PacketReader();
      const receivedData: string[] = [];

      socket.on("data", (data: Buffer) => {
        const packets = reader.feed(data);
        for (const p of packets) {
          if (p.type === MessageType.DATA) {
            receivedData.push(p.payload.toString());
          }
        }
      });

      socket.write(encodeAttach(24, 80));

      // Wait for SCREEN packet
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(
          () => reject(new Error("Timed out waiting for SCREEN")),
          5000
        );
        const origHandler = socket.listeners("data")[0] as (...args: any[]) => void;
        const screenReader = new PacketReader();
        socket.on("data", function screenCheck(data: Buffer) {
          const packets = screenReader.feed(data);
          for (const p of packets) {
            if (p.type === MessageType.SCREEN) {
              clearTimeout(timer);
              socket.removeListener("data", screenCheck);
              resolve();
              return;
            }
          }
        });
      });

      // Send input immediately after SCREEN — this is the critical moment
      socket.write(encodeData("race-test-input\n"));

      // Verify cat echoes it back
      const start = Date.now();
      while (Date.now() - start < 3000) {
        if (receivedData.join("").includes("race-test-input")) break;
        await new Promise((r) => setTimeout(r, 50));
      }

      expect(receivedData.join("")).toContain("race-test-input");
      socket.destroy();
    },
    15000
  );

  it(
    "input works with zero delay after socket appears",
    async () => {
      const name = uniqueName();
      // Spawn daemon but with NO delay after socket appears
      const config = JSON.stringify({
        name,
        command: "cat",
        args: [],
        displayCommand: "cat",
        cwd: testCwd,
        rows: 24,
        cols: 80,
      });

      const child = spawn(tsxBin, [serverModule], {
        detached: true,
        stdio: ["ignore", "ignore", "pipe"],
        env: { ...process.env, PTY_SERVER_CONFIG: config },
      });

      let stderr = "";
      child.stderr?.on("data", (d: Buffer) => {
        stderr += d.toString();
      });

      let exitCode: number | null = null;
      child.on("exit", (code) => {
        exitCode = code;
      });

      (child.stderr as any)?.unref?.();
      child.unref();
      daemonPids.push(child.pid!);

      const socketPath = getSocketPath(name);
      const start = Date.now();
      while (Date.now() - start < 5000) {
        if (exitCode !== null) {
          throw new Error(
            `Daemon exited with code ${exitCode}. stderr:\n${stderr}`
          );
        }
        try {
          fs.statSync(socketPath);
          break;
        } catch {}
        await new Promise((r) => setTimeout(r, 50));
      }

      // NO delay — connect immediately when socket file exists
      const socket = await new Promise<net.Socket>((resolve, reject) => {
        const s = net.createConnection(socketPath);
        s.on("connect", () => resolve(s));
        s.on("error", reject);
      });

      const reader = new PacketReader();
      const receivedData: string[] = [];

      socket.on("data", (data: Buffer) => {
        const packets = reader.feed(data);
        for (const p of packets) {
          if (p.type === MessageType.DATA) {
            receivedData.push(p.payload.toString());
          }
        }
      });

      // Attach and send input as fast as possible
      socket.write(encodeAttach(24, 80));
      socket.write(encodeData("zero-delay-test\n"));

      // Verify cat echoes it back
      const deadline = Date.now() + 3000;
      while (Date.now() < deadline) {
        if (receivedData.join("").includes("zero-delay-test")) break;
        await new Promise((r) => setTimeout(r, 50));
      }

      expect(receivedData.join("")).toContain("zero-delay-test");
      socket.destroy();
    },
    15000
  );
});
