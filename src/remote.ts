import * as net from "node:net";
import * as fs from "node:fs";
import { listSessions, type SessionInfo } from "./sessions.ts";

/** ALPN / fabric protocol name under which a pty control socket is exposed and
 *  dialed. `fabric expose pty-view --socket <sock>` on the remote;
 *  `fabric dial <peer> pty-view` on the client. Matches fabric's own docs. */
export const PTY_REMOTE_ALPN = "pty-view";

/** The `fabric` CLI is the only thing pty knows about the cross-machine
 *  transport — it hands us a local Unix socket and we speak our own protocol
 *  over it, exactly like the old `--remote` shelled out to `pty-relay`. pty
 *  never imports iroh. Overridable for tests. */
export const FABRIC_BIN = process.env.PTY_FABRIC_BIN ?? "fabric";

/** One session row as sent over the control protocol. Deliberately the same
 *  shape the local `--remote` host-group renderer already consumes, so remote
 *  results render through the existing path unchanged. */
export interface RemoteSessionRow {
  name: string;
  status: string;
  command?: string;
  cwd?: string;
  tags?: Record<string, string>;
  displayName?: string;
}

/** Control-protocol request. MVP op: `list`. Designed to extend — future
 *  `peek`/`attach`/`send` ops carry a `name` and the server splices the
 *  connection through to that session's `<name>.sock`. */
export interface RemoteRequest {
  op: "list";
}

export interface RemoteListResponse {
  sessions?: RemoteSessionRow[];
  error?: string;
}

function toRow(s: SessionInfo): RemoteSessionRow {
  const m = s.metadata;
  return {
    name: s.name,
    status: s.status,
    ...(m?.displayCommand ? { command: m.displayCommand } : {}),
    ...(m?.cwd ? { cwd: m.cwd } : {}),
    ...(m?.tags ? { tags: m.tags } : {}),
    ...(m?.displayName ? { displayName: m.displayName } : {}),
  };
}

async function handleRequest(line: string, sock: net.Socket): Promise<void> {
  let req: Partial<RemoteRequest>;
  try {
    req = JSON.parse(line);
  } catch {
    sock.end(JSON.stringify({ error: "malformed request" }) + "\n");
    return;
  }
  if (req.op === "list") {
    try {
      const sessions = (await listSessions()).map(toRow);
      sock.end(JSON.stringify({ sessions }) + "\n");
    } catch (e) {
      sock.end(JSON.stringify({ error: (e as Error).message }) + "\n");
    }
    return;
  }
  sock.end(JSON.stringify({ error: `unknown op: ${String(req.op)}` }) + "\n");
}

/** Serve the pty remote-access control protocol on a plain Unix socket. pty
 *  stays transport-agnostic: fabric (or anything) exposes this socket to peers.
 *  Reads sessions from the ambient $PTY_ROOT, so run it in the same env the
 *  sessions use. Returns the listening server. */
export function serveRemoteControl(socketPath: string): net.Server {
  try {
    fs.unlinkSync(socketPath);
  } catch {
    // no stale socket to clear
  }
  const server = net.createServer((sock) => {
    let buf = "";
    let handled = false;
    sock.on("data", (chunk: Buffer) => {
      if (handled) return;
      buf += chunk.toString("utf-8");
      const nl = buf.indexOf("\n");
      if (nl === -1) return; // wait for the full request line
      handled = true;
      void handleRequest(buf.slice(0, nl), sock);
    });
    sock.on("error", () => {
      /* client vanished mid-request; nothing to do */
    });
  });
  server.listen(socketPath);
  return server;
}

/** Connect to a control socket (a local path, or one handed to us by
 *  `fabric dial`), request the session list, and return the rows. */
export function fetchRemoteList(socketPath: string, timeoutMs = 10000): Promise<RemoteSessionRow[]> {
  return new Promise((resolve, reject) => {
    const sock = net.createConnection(socketPath);
    let buf = "";
    const timer = setTimeout(() => {
      sock.destroy();
      reject(new Error("remote list timed out"));
    }, timeoutMs);
    sock.on("connect", () => {
      const req: RemoteRequest = { op: "list" };
      sock.write(JSON.stringify(req) + "\n");
    });
    sock.on("data", (chunk: Buffer) => {
      buf += chunk.toString("utf-8");
    });
    sock.on("end", () => {
      clearTimeout(timer);
      try {
        const resp = JSON.parse(buf.trim()) as RemoteListResponse;
        if (resp.error) {
          reject(new Error(resp.error));
          return;
        }
        resolve(resp.sessions ?? []);
      } catch (e) {
        reject(new Error(`bad remote response: ${(e as Error).message}`));
      }
    });
    sock.on("error", (e) => {
      clearTimeout(timer);
      reject(e);
    });
  });
}
