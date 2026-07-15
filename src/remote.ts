import * as net from "node:net";
import * as fs from "node:fs";
import { execFileSync } from "node:child_process";
import { listSessions, getSocketPath, type SessionInfo } from "./sessions.ts";

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

/** Control-protocol request (one JSON line, then for `route` the raw per-session
 *  protocol). `list` returns the session set; `route` splices the connection
 *  through to a session's `<name>.sock` so the caller speaks the ordinary
 *  per-session protocol over the fabric hop — this is how peek/send/attach
 *  `--remote` reuse the existing client code unchanged. */
export type RemoteRequest =
  | { op: "list" }
  | { op: "route"; name: string };

export interface RemoteListResponse {
  sessions?: RemoteSessionRow[];
  error?: string;
}

/** Route handshake ack line the server sends once it has connected to the target
 *  session, right before it starts splicing. `dialAndRoute` waits for this so a
 *  routed command reliably knows the route succeeded (or, on `{error}`, failed).*/
const ROUTE_OK = JSON.stringify({ ok: true });

/** The dial reached the peer's control server but it REFUSED the route — the
 *  host is reachable and reports the session is gone/absent. Distinct from a
 *  transport failure (dial/connect/timeout = host unreachable): `attach --remote`
 *  reconnect gives up cleanly on this (the session truly ended) but retries
 *  forever on a transport failure (the outage is recoverable). */
export class RouteRefusedError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "RouteRefusedError";
  }
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

async function handleRequest(line: string, residual: Buffer, sock: net.Socket): Promise<void> {
  let req: { op?: string; name?: unknown };
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
  if (req.op === "route" && typeof req.name === "string") {
    routeToSession(req.name, residual, sock);
    return;
  }
  sock.end(JSON.stringify({ error: `unknown op: ${String(req.op)}` }) + "\n");
}

/** Splice a routed client connection through to a session's local `<name>.sock`,
 *  so the caller can speak the ordinary per-session protocol over the fabric
 *  hop. `residual` is any bytes that already arrived AFTER the request line —
 *  they must be forwarded before the pipe or the first protocol frame is lost. */
function routeToSession(name: string, residual: Buffer, sock: net.Socket): void {
  const target = net.createConnection(getSocketPath(name));
  target.on("connect", () => {
    // ACK the route BEFORE splicing so the client knows it succeeded — the
    // per-session protocol has no ack, so a fire-and-forget `send --remote` to a
    // missing session would otherwise exit 0 with the bytes silently dropped.
    // The client reads this one line, then everything after is the raw splice.
    sock.write(ROUTE_OK + "\n");
    if (residual.length > 0) target.write(residual);
    sock.pipe(target);
    target.pipe(sock);
    sock.resume(); // was paused at request-line boundary; now the pipe is wired
  });
  target.on("error", () => {
    // Route failed (session gone/unreachable): report it as the route response
    // (still a line, before any splice), then close. sock is paused but writes
    // are unaffected.
    try { sock.end(JSON.stringify({ error: `session "${name}" not found` }) + "\n"); } catch {}
  });
  target.on("close", () => { try { sock.destroy(); } catch {} });
  sock.on("error", () => { try { target.destroy(); } catch {} });
  sock.on("close", () => { try { target.destroy(); } catch {} });
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
    // Accumulate BYTES (not a string): a `route` request is one JSON line
    // followed by raw per-session protocol bytes, and string round-tripping
    // would corrupt those. Read up to the first newline as the request line;
    // everything after it is `residual` handed to the handler (e.g. the first
    // protocol frame that arrived in the same chunk).
    let buf: Buffer = Buffer.alloc(0);
    let handled = false;
    const onData = (chunk: Buffer) => {
      if (handled) return;
      buf = Buffer.concat([buf, chunk]);
      const nl = buf.indexOf(0x0a); // '\n'
      if (nl === -1) return; // wait for the full request line
      handled = true;
      sock.pause(); // hold further bytes until the handler dispatches/splices
      sock.removeListener("data", onData);
      const line = buf.subarray(0, nl).toString("utf-8");
      const residual = buf.subarray(nl + 1);
      void handleRequest(line, residual, sock);
    };
    sock.on("data", onData);
    sock.on("error", () => {
      /* client vanished mid-request; nothing to do */
    });
  });
  server.listen(socketPath);
  return server;
}

/** Dial a fabric peer's exposed pty control socket and route it to a specific
 *  remote session. The resolved socket is a transparent pipe to that session's
 *  daemon socket, ready for the ordinary per-session protocol (attach/peek/send
 *  client code runs over it unchanged). */
export function dialAndRoute(peer: string, name: string, timeoutMs = 10000): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    let dialSock: string;
    try {
      dialSock = execFileSync(FABRIC_BIN, ["dial", peer, PTY_REMOTE_ALPN], {
        encoding: "utf-8",
        timeout: timeoutMs,
      }).trim();
    } catch (e) {
      reject(e instanceof Error ? e : new Error(String(e)));
      return;
    }
    if (!dialSock) {
      reject(new Error(`fabric dial ${peer} returned no socket`));
      return;
    }
    const sock = net.createConnection(dialSock);
    let acked = false;
    let buf: Buffer = Buffer.alloc(0);
    const timer = setTimeout(() => {
      if (!acked) { sock.destroy(); reject(new Error("route handshake timed out")); }
    }, timeoutMs);
    const onData = (chunk: Buffer) => {
      buf = Buffer.concat([buf, chunk]);
      const nl = buf.indexOf(0x0a);
      if (nl === -1) return; // wait for the full ack line
      acked = true;
      clearTimeout(timer);
      sock.removeListener("data", onData);
      const line = buf.subarray(0, nl).toString("utf-8");
      const rest = buf.subarray(nl + 1); // bytes after the ack (normally none)
      let resp: { ok?: boolean; error?: string };
      try {
        resp = JSON.parse(line);
      } catch {
        sock.destroy();
        reject(new Error(`bad route response: ${line.slice(0, 80)}`));
        return;
      }
      if (resp.error || !resp.ok) {
        // Reached the host; it refused the route (session gone). Distinct from a
        // transport failure so reconnect can give up cleanly vs retry forever.
        sock.destroy();
        reject(new RouteRefusedError(resp.error ?? "route refused"));
        return;
      }
      // Hand back anything that arrived after the ack line so the caller's
      // per-session protocol sees it (defensive — the server splices only after
      // acking, so in practice `rest` is empty).
      if (rest.length > 0) sock.unshift(rest);
      resolve(sock);
    };
    sock.once("connect", () => {
      sock.write(JSON.stringify({ op: "route", name }) + "\n");
    });
    sock.on("data", onData);
    sock.on("error", (e) => { clearTimeout(timer); if (!acked) reject(e); });
    sock.on("close", () => {
      clearTimeout(timer);
      if (!acked) reject(new Error(`remote session "${name}" not reachable`));
    });
  });
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
