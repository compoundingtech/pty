import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
  MACHINE_PROTOCOL_VERSION,
  MachineFrameReader,
  decodeMachineResponse,
  encodeMachineRequest,
  type MachineOpenV2,
  type MachineResponse,
  type MachineWireFrame,
} from "../src/machine-protocol.ts";
import { PtyServer } from "../src/server.ts";
import { readMetadata } from "../src/sessions.ts";

const servers: PtyServer[] = [];
const roots: string[] = [];
const adapters: ChildProcessWithoutNullStreams[] = [];
const originalRoot = process.env.PTY_ROOT;
const originalLegacyRoot = process.env.PTY_SESSION_DIR;

afterEach(async () => {
  for (const adapter of adapters.splice(0)) {
    if (adapter.exitCode === null && adapter.signalCode === null) adapter.kill("SIGKILL");
  }
  for (const server of servers.splice(0)) await server.close();
  for (const root of roots.splice(0)) fs.rmSync(root, { recursive: true, force: true });
  if (originalRoot === undefined) delete process.env.PTY_ROOT;
  else process.env.PTY_ROOT = originalRoot;
  if (originalLegacyRoot === undefined) delete process.env.PTY_SESSION_DIR;
  else process.env.PTY_SESSION_DIR = originalLegacyRoot;
});

function createRoot(): string {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "pty-machine-v2-e2e-"));
  roots.push(root);
  process.env.PTY_ROOT = root;
  delete process.env.PTY_SESSION_DIR;
  return root;
}

async function startServer(
  name: string,
  generation: string,
  command: string,
  args: readonly string[],
): Promise<PtyServer> {
  const server = new PtyServer({
    name,
    generation,
    command,
    args: [...args],
    displayCommand: command,
    cwd: process.cwd(),
    rows: 24,
    cols: 80,
  });
  servers.push(server);
  await server.ready;
  return server;
}

function openRequest(name: string, expectedGeneration: string): MachineOpenV2 {
  return {
    _tag: "Open",
    protocol: MACHINE_PROTOCOL_VERSION,
    sessionId: name,
    expectedGeneration,
    rows: 24,
    cols: 80,
    requiredCapabilities: [
      "framed-utf8-input",
      "typed-outcome",
      "input-mode-snapshot",
    ],
  };
}

function spawnAdapter(root: string): ChildProcessWithoutNullStreams {
  const child = spawn(
    process.execPath,
    [path.join(process.cwd(), "dist", "cli.js"), "machine-attach-v2"],
    {
      env: { ...process.env, PTY_ROOT: root, PTY_SESSION_DIR: undefined },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  adapters.push(child);
  return child;
}

function observeAdapter(child: ChildProcessWithoutNullStreams): {
  readonly responses: MachineResponse[];
  readonly stdout: Buffer[];
  readonly stderr: Buffer[];
  waitFor: (predicate: (responses: readonly MachineResponse[]) => boolean) => Promise<void>;
  exited: Promise<{ code: number | null; signal: NodeJS.Signals | null }>;
} {
  const reader = new MachineFrameReader();
  const responses: MachineResponse[] = [];
  const stdout: Buffer[] = [];
  const stderr: Buffer[] = [];
  const waiters = new Set<() => void>();
  child.stdout.on("data", (chunk: Buffer) => {
    const bytes = Buffer.from(chunk);
    stdout.push(bytes);
    responses.push(...reader.feed(bytes).map(decodeMachineResponse));
    for (const wake of waiters) wake();
  });
  child.stderr.on("data", (chunk: Buffer) => stderr.push(Buffer.from(chunk)));
  const exited = new Promise<{ code: number | null; signal: NodeJS.Signals | null }>((resolve) => {
    child.once("exit", (code, signal) => resolve({ code, signal }));
  });
  return {
    responses,
    stdout,
    stderr,
    async waitFor(predicate) {
      if (predicate(responses)) return;
      await new Promise<void>((resolve, reject) => {
        const timeout = setTimeout(() => {
          waiters.delete(check);
          reject(new Error(`Timed out waiting for adapter response; received ${responses.map((response) => response._tag).join(", ")}`));
        }, 5_000);
        const check = () => {
          if (!predicate(responses)) return;
          clearTimeout(timeout);
          waiters.delete(check);
          resolve();
        };
        waiters.add(check);
      });
    },
    exited,
  };
}

function decodeCompleteStream(bytes: Buffer): MachineResponse[] {
  const frames: MachineWireFrame[] = [];
  let offset = 0;
  while (offset < bytes.length) {
    if (bytes.length - offset < 5) throw new Error("Truncated machine frame header at EOF");
    const length = bytes.readUInt32BE(offset + 1);
    const end = offset + 5 + length;
    if (end > bytes.length) throw new Error("Truncated machine frame payload at EOF");
    frames.push({ type: bytes.readUInt8(offset), payload: bytes.subarray(offset + 5, end) });
    offset = end;
  }
  return frames.map(decodeMachineResponse);
}

function terminalBytes(responses: readonly MachineResponse[]): Buffer {
  return Buffer.concat(responses.flatMap((response) => {
    if (response._tag === "Ready") return [response.screen];
    if (response._tag === "Data") return [response.bytes];
    return [];
  }));
}

function expectCausalDataStream(responses: readonly MachineResponse[]): void {
  const ready = responses.find((response) => response._tag === "Ready");
  if (ready?._tag !== "Ready") throw new Error("Expected READY in accepted machine stream");
  let outputRevision = ready.outputRevision;
  let inputModeRevision = ready.inputModes.revision;
  for (const response of responses.slice(responses.indexOf(ready) + 1)) {
    if (response._tag !== "Data") continue;
    expect(response.outputRevision).toBe(outputRevision + 1);
    expect(response.inputModeRevision).toBeGreaterThanOrEqual(inputModeRevision);
    expect(response.inputModes !== undefined).toBe(response.inputModeRevision > inputModeRevision);
    if (response.inputModes !== undefined) {
      expect(response.inputModes.revision).toBe(response.inputModeRevision);
    }
    outputRevision = response.outputRevision;
    inputModeRevision = response.inputModeRevision;
  }
}

describe.sequential("machine-attach-v2 process and socket integration", () => {
  it("preserves terminal input bytes and reserves detach for the explicit frame", async () => {
    const root = createRoot();
    const name = `machine-e2e-${process.pid}-bytes`;
    const generation = "generation-bytes";
    const childScript = [
      "process.stdin.setRawMode?.(true)",
      "process.stdin.resume()",
      "process.stdout.write('RAW_READY\\r\\n')",
      "let received = Buffer.alloc(0)",
      "process.stdin.on('data', (chunk) => {",
      "  received = Buffer.concat([received, chunk])",
      "  if (received.length >= 6) process.stdout.write(`HEX:${received.subarray(0, 6).toString('hex')}\\r\\n`)",
      "})",
      "setInterval(() => {}, 1000)",
    ].join(";");
    await startServer(name, generation, process.execPath, ["-e", childScript]);

    const adapter = spawnAdapter(root);
    const observed = observeAdapter(adapter);
    adapter.stdin.write(encodeMachineRequest(openRequest(name, generation)));
    await observed.waitFor((responses) => responses.some((response) => response._tag === "Ready"));
    expect(observed.responses.slice(0, 2).map((response) => response._tag)).toEqual(["Hello", "Ready"]);

    const input = Buffer.from([0x31, 0x32, 0x33, 0x1b, 0x62, 0x1c]);
    adapter.stdin.write(encodeMachineRequest({ _tag: "Input", bytes: input }));
    await observed.waitFor((responses) => terminalBytes(responses).includes(Buffer.from("HEX:3132331b621c")));
    expect(observed.responses.some((response) => response._tag === "Detached")).toBe(false);

    adapter.stdin.end(encodeMachineRequest({ _tag: "Detach" }));
    await observed.waitFor((responses) => responses.at(-1)?._tag === "Detached");
    await expect(observed.exited).resolves.toEqual({ code: 0, signal: null });

    const complete = decodeCompleteStream(Buffer.concat(observed.stdout));
    expect(complete.slice(0, 2).map((response) => response._tag)).toEqual(["Hello", "Ready"]);
    expect(complete.at(-1)).toEqual({ _tag: "Detached" });
    expect(complete.some((response) => response._tag === "Exited" || response._tag.endsWith("Failure"))).toBe(false);
    expectCausalDataStream(complete);
    expect(terminalBytes(complete).includes(Buffer.from("HEX:3132331b621c"))).toBe(true);
    expect(Buffer.concat(observed.stderr).toString()).toBe("");
  });

  it("rejects a stale generation without mutating the live session", async () => {
    const root = createRoot();
    const name = `machine-e2e-${process.pid}-stale`;
    const generation = "generation-current";
    await startServer(name, generation, process.execPath, ["-e", "setInterval(() => {}, 1000)"]);
    const before = readMetadata(name);

    const adapter = spawnAdapter(root);
    const observed = observeAdapter(adapter);
    adapter.stdin.write(encodeMachineRequest(openRequest(name, "generation-stale")));
    await expect(observed.exited).resolves.toEqual({ code: 0, signal: null });

    const complete = decodeCompleteStream(Buffer.concat(observed.stdout));
    expect(complete).toHaveLength(1);
    expect(complete[0]).toMatchObject({
      _tag: "AdmissionFailure",
      reason: "generation-mismatch",
    });
    expect(readMetadata(name)).toEqual(before);
    expect(Buffer.concat(observed.stderr).toString()).toBe("");
  });
});
