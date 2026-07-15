import { describe, it, expect } from "vitest";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");

// `--print-systemd-unit` just prints and exits — no PTY spawn — so this is a
// fast, parallel test (deliberately NOT in the serial heavy-PTY project).
function run(args: string[], env: Record<string, string> = {}) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_ROOT_LEGACY_SILENT: "1", ...env },
    encoding: "utf8",
    timeout: 15000,
  });
}

describe("pty remote-serve --print-systemd-unit", () => {
  it("prints a Type=simple unit with resolved PTY_ROOT + ExecStart, without serving", () => {
    const r = run(["remote-serve", "--print-systemd-unit"], { PTY_ROOT: "/tmp/pr-sysd-root" });
    expect(r.status).toBe(0); // prints + exits, does not serve
    expect(r.stdout).toContain("[Service]");
    expect(r.stdout).toContain("Type=simple");
    expect(r.stdout).toContain("Environment=PTY_ROOT=/tmp/pr-sysd-root");
    // node + resolved CLI + the serve command with the default runtime socket.
    expect(r.stdout).toMatch(/ExecStart=\S+ \S+ remote-serve --socket %t\/pty-remote\.sock/);
    expect(r.stdout).toContain("Restart=on-failure");
    expect(r.stdout).toContain("[Install]");
    // Reminder that fabric exposure is a separate unit (stderr, so stdout stays clean).
    expect(r.stderr).toMatch(/fabric expose pty-view/);
  });

  it("warns (stderr) when --socket is inside PTY_ROOT (phantom-session footgun)", () => {
    const r = run(
      ["remote-serve", "--print-systemd-unit", "--socket", "/tmp/pr-sysd-root/ctrl.sock"],
      { PTY_ROOT: "/tmp/pr-sysd-root" },
    );
    expect(r.status).toBe(0);
    expect(r.stderr).toMatch(/inside PTY_ROOT/);
    expect(r.stdout).toContain("--socket /tmp/pr-sysd-root/ctrl.sock"); // still emitted in ExecStart
  });
});
