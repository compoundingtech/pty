import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { validateName, acquireLock, releaseLock, getSessionDir } from "../src/sessions.ts";

// These tests pin BUG-1 (socket path length) and BUG-2 (acquireLock race).

const origSessionDir = process.env.PTY_SESSION_DIR;
let tmp: string;

beforeEach(() => {
  tmp = fs.mkdtempSync(path.join(os.tmpdir(), "pty-sec-"));
  process.env.PTY_SESSION_DIR = tmp;
});

afterEach(() => {
  if (origSessionDir === undefined) delete process.env.PTY_SESSION_DIR;
  else process.env.PTY_SESSION_DIR = origSessionDir;
  try { fs.rmSync(tmp, { recursive: true, force: true }); } catch {}
});

describe("BUG-1: validateName rejects oversized socket paths", () => {
  it("accepts ordinary names", () => {
    expect(() => validateName("myserver")).not.toThrow();
  });

  it("rejects names that would overflow the 104-byte sun_path limit", () => {
    // Session dir is a reasonably long tmpdir path; append a 100-char name.
    const longName = "a".repeat(100);
    expect(() => validateName(longName)).toThrow(/socket path.*exceeds/);
  });

  it("rejects names whose path hits the limit exactly +1", () => {
    const dir = getSessionDir();
    const overhead = Buffer.byteLength(path.join(dir, ".sock"), "utf-8");
    const overshootName = "a".repeat(Math.max(1, 104 - overhead + 1));
    expect(() => validateName(overshootName)).toThrow(/socket path/);
  });

  it("still rejects bad characters before checking length", () => {
    expect(() => validateName("has/slash")).toThrow(/Invalid session name/);
  });
});

describe("BUG-2: acquireLock atomic semantics", () => {
  it("first caller wins, second returns false while holder is alive", () => {
    expect(acquireLock("race1")).toBe(true);
    expect(acquireLock("race1")).toBe(false);
    releaseLock("race1");
  });

  it("steals a stale lock whose holder pid is dead", () => {
    // Write a lock file with a dead PID.
    const lockPath = path.join(tmp, "race2.lock");
    fs.writeFileSync(lockPath, "1"); // PID 1 exists, but...
    // Use a pid we know is dead: INT_MAX
    fs.writeFileSync(lockPath, "2147483646");

    expect(acquireLock("race2")).toBe(true);
    // Now holder should be us
    const pid = parseInt(fs.readFileSync(lockPath, "utf-8"), 10);
    expect(pid).toBe(process.pid);
    releaseLock("race2");
  });

  it("garbage lock content is treated as stale", () => {
    fs.writeFileSync(path.join(tmp, "race3.lock"), "not a pid");
    expect(acquireLock("race3")).toBe(true);
    releaseLock("race3");
  });

  it("release is idempotent (unlinking a missing lock is fine)", () => {
    expect(() => releaseLock("never-locked")).not.toThrow();
  });

  it("acquireLock uses O_EXCL — concurrent wx opens can't both win", () => {
    // Simulate two concurrent stealers: create stale lock, then race two
    // acquireLock calls synchronously. We can't truly parallelize here, but we
    // can prove that after one win, a subsequent call to acquireLock against
    // the live pid returns false.
    fs.writeFileSync(path.join(tmp, "race4.lock"), "2147483646");
    const a = acquireLock("race4");
    const b = acquireLock("race4");
    expect([a, b].filter(Boolean).length).toBe(1);
    releaseLock("race4");
  });
});
