import * as fs from "node:fs";
import * as path from "node:path";
import { execFileSync } from "node:child_process";
import { createHash, createHmac, timingSafeEqual } from "node:crypto";
import type { SessionMetadata } from "./sessions.ts";

export const RECOVERY_PROTOCOL = 1;
export const RECOVERY_MAX_BYTES = 1024 * 1024;

export interface RecoveryCapability {
  protocol: 1;
  secret: string;
  processStartToken: string;
  launchIdentity: string;
  rootDevice: number;
  rootInode: number;
}

export interface RecoveryRequestPayload {
  protocol: 1;
  name: string;
  daemonPid: number;
  generation: string;
  processStartToken: string;
  launchIdentity: string;
  rootDevice: number;
  rootInode: number;
  lockOwnerPid: number;
  nonce: string;
  metadata: SessionMetadata;
}

export interface RecoveryRequest extends RecoveryRequestPayload {
  auth: string;
}

export interface RecoveryResultPayload {
  protocol: 1;
  name: string;
  nonce: string;
  ok: boolean;
  error?: string;
  daemonPid?: number;
  generation?: string;
  processStartToken?: string;
  launchIdentity?: string;
}

export interface RecoveryResult extends RecoveryResultPayload {
  auth: string;
}

export function recoveryDir(root: string): string {
  return path.join(root, ".recovery");
}

export function recoveryRequestPath(root: string, name: string): string {
  return path.join(recoveryDir(root), `${name}.request.json`);
}

export function recoveryResultPath(root: string, name: string): string {
  return path.join(recoveryDir(root), `${name}.result.json`);
}

export function ensureRecoveryDir(root: string): void {
  fs.mkdirSync(recoveryDir(root), { recursive: true, mode: 0o700 });
}

export function readProcessStartToken(pid: number): string | null {
  if (!Number.isSafeInteger(pid) || pid <= 0) return null;
  try {
    if (process.platform === "linux") {
      const stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
      const tail = stat.slice(stat.lastIndexOf(")") + 2).trim().split(/\s+/);
      const startTime = tail[19];
      return startTime ? `linux:${startTime}` : null;
    }
    if (process.platform === "darwin") {
      const started = execFileSync(
        "ps",
        ["-o", "lstart=", "-p", String(pid)],
        { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] },
      ).trim();
      return started ? `darwin:${started}` : null;
    }
  } catch {}
  return null;
}

export function stableStringify(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(stableStringify).join(",")}]`;
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record).sort().filter((key) => record[key] !== undefined)
    .map((key) => `${JSON.stringify(key)}:${stableStringify(record[key])}`).join(",")}}`;
}

export function launchIdentity(metadata: Pick<
  SessionMetadata,
  "command" | "args" | "displayCommand" | "cwd" | "rows" | "cols" |
  "ephemeral" | "isolateEnv" | "extraEnv" | "env"
>): string {
  return createHash("sha256").update(stableStringify({
    command: metadata.command,
    args: metadata.args,
    displayCommand: metadata.displayCommand,
    cwd: metadata.cwd,
    rows: metadata.rows,
    cols: metadata.cols,
    ephemeral: metadata.ephemeral === true,
    isolateEnv: metadata.isolateEnv === true,
    extraEnv: metadata.extraEnv,
    env: metadata.env,
  })).digest("hex");
}

function mac(secret: string, payload: unknown): string {
  return createHmac("sha256", Buffer.from(secret, "hex"))
    .update(stableStringify(payload))
    .digest("hex");
}

export function signRecoveryRequest(
  secret: string,
  payload: RecoveryRequestPayload,
): RecoveryRequest {
  return { ...payload, auth: mac(secret, payload) };
}

export function verifyRecoveryRequest(secret: string, request: RecoveryRequest): boolean {
  const { auth, ...payload } = request;
  const expected = Buffer.from(mac(secret, payload), "hex");
  const actual = Buffer.from(typeof auth === "string" ? auth : "", "hex");
  return actual.length === expected.length && timingSafeEqual(actual, expected);
}

export function signRecoveryResult(
  secret: string,
  payload: RecoveryResultPayload,
): RecoveryResult {
  return { ...payload, auth: mac(secret, payload) };
}

export function verifyRecoveryResult(secret: string, result: RecoveryResult): boolean {
  const { auth, ...payload } = result;
  const expected = Buffer.from(mac(secret, payload), "hex");
  const actual = Buffer.from(typeof auth === "string" ? auth : "", "hex");
  return actual.length === expected.length && timingSafeEqual(actual, expected);
}

export function readBoundedJson<T>(file: string): T {
  const stat = fs.lstatSync(file);
  if (!stat.isFile() || stat.isSymbolicLink() || stat.size > RECOVERY_MAX_BYTES) {
    throw new Error("recovery file must be a bounded regular file");
  }
  return JSON.parse(fs.readFileSync(file, "utf8")) as T;
}

export function atomicWritePrivate(file: string, value: unknown): void {
  const tmp = `${file}.tmp.${process.pid}.${Math.random().toString(16).slice(2)}`;
  try {
    fs.writeFileSync(tmp, JSON.stringify(value, null, 2), { mode: 0o600 });
    fs.renameSync(tmp, file);
  } catch (error) {
    try { fs.unlinkSync(tmp); } catch {}
    throw error;
  }
}

export function publishPrivateNoReplace(file: string, value: string): void {
  const tmp = `${file}.tmp.${process.pid}.${Math.random().toString(16).slice(2)}`;
  try {
    fs.writeFileSync(tmp, value, { mode: 0o600 });
    fs.linkSync(tmp, file);
  } finally {
    try { fs.unlinkSync(tmp); } catch {}
  }
}
