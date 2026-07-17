import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { readPtyFile, commandWithEnvExports } from "../src/ptyfile.ts";

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-ptyfile-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

function makeDir(name: string, content: string): string {
  const dir = fs.mkdtempSync(path.join(testRoot, `${name}-`));
  fs.writeFileSync(path.join(dir, "pty.toml"), content);
  return dir;
}

describe("readPtyFile env", () => {
  it("parses [sessions.X.env] as a string map", () => {
    const dir = makeDir("envok", `
[sessions.worker]
command = "cat"

[sessions.worker.env]
FOO = "bar"
SECOND = "two"
`);
    const file = readPtyFile(dir);
    expect(file.sessions).toHaveLength(1);
    expect(file.sessions[0].env).toEqual({ FOO: "bar", SECOND: "two" });
  });

  it("omits env when not present", () => {
    const dir = makeDir("noenv", `
[sessions.plain]
command = "cat"
`);
    const file = readPtyFile(dir);
    expect(file.sessions[0].env).toBeUndefined();
  });

  it("accepts an empty env table", () => {
    const dir = makeDir("emptyenv", `
[sessions.empty]
command = "cat"

[sessions.empty.env]
`);
    const file = readPtyFile(dir);
    expect(file.sessions[0].env).toEqual({});
  });

  it("rejects non-string env values", () => {
    const dir = makeDir("badenv", `
[sessions.bad]
command = "cat"

[sessions.bad.env]
NOT_A_STRING = 42
`);
    expect(() => readPtyFile(dir)).toThrow(/env\.NOT_A_STRING must be a string/);
  });
});

describe("readPtyFile cwd", () => {
  it("omits cwd when not present", () => {
    const dir = makeDir("nocwd", `
[sessions.plain]
command = "cat"
`);
    expect(readPtyFile(dir).sessions[0].cwd).toBeUndefined();
  });

  it("keeps an absolute cwd as-is", () => {
    const dir = makeDir("abscwd", `
[sessions.svc]
command = "cat"
cwd = "/opt/app"
`);
    expect(readPtyFile(dir).sessions[0].cwd).toBe("/opt/app");
  });

  it("resolves a relative cwd against the manifest's directory", () => {
    // A manifest kept in a subdir points its session at the repo root via "..".
    const sub = makeDir("relcwd", `
[sessions.svc]
command = "cat"
cwd = ".."
`);
    // sub is <testRoot>/relcwd-XX.../ ; ".." resolves to <testRoot>/relcwd-XX...'s parent.
    expect(readPtyFile(sub).sessions[0].cwd).toBe(path.resolve(sub, ".."));
  });

  it("resolves '.' to the manifest directory (the default behavior, made explicit)", () => {
    const dir = makeDir("dotcwd", `
[sessions.svc]
command = "cat"
cwd = "."
`);
    expect(readPtyFile(dir).sessions[0].cwd).toBe(path.resolve(dir));
  });

  it("rejects a non-string cwd", () => {
    const dir = makeDir("badcwd", `
[sessions.bad]
command = "cat"
cwd = 42
`);
    expect(() => readPtyFile(dir)).toThrow(/"cwd" must be a non-empty string/);
  });

  it("rejects an empty-string cwd", () => {
    const dir = makeDir("emptycwd", `
[sessions.bad]
command = "cat"
cwd = ""
`);
    expect(() => readPtyFile(dir)).toThrow(/"cwd" must be a non-empty string/);
  });
});

describe("commandWithEnvExports", () => {
  it("returns the bare command when env is absent", () => {
    expect(commandWithEnvExports({
      displayName: "x", shortName: "x", id: null, command: "echo hi",
    })).toBe("echo hi");
  });

  it("returns the bare command when env is empty", () => {
    expect(commandWithEnvExports({
      displayName: "x", shortName: "x", id: null, command: "echo hi", env: {},
    })).toBe("echo hi");
  });

  it("prepends export statements", () => {
    expect(commandWithEnvExports({
      displayName: "x", shortName: "x", id: null, command: "echo $FOO",
      env: { FOO: "bar" },
    })).toBe("export FOO='bar'; echo $FOO");
  });

  it("emits one export per env entry", () => {
    const out = commandWithEnvExports({
      displayName: "x", shortName: "x", id: null, command: "do-thing",
      env: { A: "1", B: "two" },
    });
    expect(out).toContain("export A='1'");
    expect(out).toContain("export B='two'");
    expect(out).toMatch(/; do-thing$/);
  });

  it("escapes single quotes in values", () => {
    expect(commandWithEnvExports({
      displayName: "x", shortName: "x", id: null, command: "echo $MSG",
      env: { MSG: "it's a value" },
    })).toBe(`export MSG='it'\\''s a value'; echo $MSG`);
  });

  it("handles values with shell metacharacters safely", () => {
    expect(commandWithEnvExports({
      displayName: "x", shortName: "x", id: null, command: "go",
      env: { PATH_LIKE: "$HOME/bin:/usr/bin; echo pwned" },
    })).toBe(`export PATH_LIKE='$HOME/bin:/usr/bin; echo pwned'; go`);
  });
});
