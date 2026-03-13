import * as http from "node:http";
import * as net from "node:net";
import * as fs from "node:fs";
import * as path from "node:path";
import * as crypto from "node:crypto";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import type {
  WebSocket as WSType,
  WebSocketServer as WSSType,
} from "ws";
import { PacketReader, encodePacket } from "../protocol.ts";
import { listSessions, getSocketPath } from "../sessions.ts";

// ws is CJS-only. Different ESM loaders (tsx, Playwright, Node native)
// handle CJS interop inconsistently. createRequire always works.
const require = createRequire(import.meta.url);
const { WebSocketServer } = require("ws") as { WebSocketServer: typeof WSSType };

const __dirname = path.dirname(fileURLToPath(import.meta.url));

export interface WebServerOptions {
  host?: string;
  port?: number;
  connectCode?: string;
}

export class WebServer {
  readonly ready: Promise<{ port: number; host: string }>;

  private httpServer: http.Server;
  private wss: WSSType;
  private authToken: string | undefined;
  private connectCode: string | undefined;
  private connections = new Set<{ ws: WSType; unix: net.Socket }>();

  constructor(options: WebServerOptions = {}) {
    const host = options.host ?? "127.0.0.1";
    const port = options.port ?? 7681;
    this.connectCode = options.connectCode;

    if (this.connectCode) {
      this.authToken = crypto.randomUUID();
    }

    this.httpServer = http.createServer((req, res) => this.handleHTTP(req, res));
    this.wss = new WebSocketServer({ noServer: true });

    this.httpServer.on("upgrade", (req, socket, head) => {
      if (this.connectCode && !this.checkAuth(req)) {
        socket.write("HTTP/1.1 401 Unauthorized\r\n\r\n");
        socket.destroy();
        return;
      }

      const url = new URL(req.url ?? "/", `http://${req.headers.host}`);
      const match = url.pathname.match(/^\/ws\/(.+)$/);
      if (!match) {
        socket.write("HTTP/1.1 404 Not Found\r\n\r\n");
        socket.destroy();
        return;
      }

      const name = decodeURIComponent(match[1]);

      this.wss.handleUpgrade(req, socket as net.Socket, head, (wsClient: WSType) => {
        this.handleWS(wsClient, name);
      });
    });

    this.ready = new Promise((resolve) => {
      this.httpServer.listen(port, host, () => {
        const addr = this.httpServer.address() as net.AddressInfo;
        resolve({ port: addr.port, host: addr.address });
      });
    });
  }

  close(): Promise<void> {
    return new Promise((resolve) => {
      for (const conn of this.connections) {
        conn.ws.close();
        conn.unix.destroy();
      }
      this.connections.clear();

      this.wss.close(() => {
        this.httpServer.close(() => resolve());
      });
    });
  }

  private checkAuth(req: http.IncomingMessage): boolean {
    if (!this.connectCode || !this.authToken) return true;
    const cookies = req.headers.cookie ?? "";
    const match = cookies.match(/(?:^|;\s*)pty_token=([^\s;]+)/);
    return match?.[1] === this.authToken;
  }

  private isNumericCode(): boolean {
    return this.connectCode != null && /^\d+$/.test(this.connectCode);
  }

  private async handleHTTP(
    req: http.IncomingMessage,
    res: http.ServerResponse
  ): Promise<void> {
    const url = new URL(req.url ?? "/", `http://${req.headers.host}`);

    if (req.method === "POST" && url.pathname === "/auth") {
      await this.handleAuth(req, res);
      return;
    }

    // Auth check for API routes
    if (this.connectCode && !this.checkAuth(req)) {
      if (url.pathname === "/api/sessions") {
        res.writeHead(401, { "Content-Type": "application/json" });
        res.end(
          JSON.stringify({ error: "unauthorized", numeric: this.isNumericCode() })
        );
        return;
      }
      // Serve index.html and vendor files even when unauthed —
      // the JS handles showing the login form.
      if (url.pathname === "/") {
        this.serveIndex(res);
        return;
      }
      if (url.pathname.startsWith("/vendor/")) {
        this.serveVendor(url.pathname, res);
        return;
      }
      res.writeHead(401, { "Content-Type": "application/json" });
      res.end(
        JSON.stringify({ error: "unauthorized", numeric: this.isNumericCode() })
      );
      return;
    }

    if (url.pathname === "/") {
      this.serveIndex(res);
      return;
    }

    if (url.pathname === "/api/sessions") {
      const sessions = await listSessions();
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
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify(output));
      return;
    }

    if (url.pathname.startsWith("/vendor/")) {
      this.serveVendor(url.pathname, res);
      return;
    }

    res.writeHead(404);
    res.end("Not Found");
  }

  private async handleAuth(
    req: http.IncomingMessage,
    res: http.ServerResponse
  ): Promise<void> {
    const body = await readBody(req);
    const params = new URLSearchParams(body);
    const code = params.get("code");

    if (code === this.connectCode) {
      res.writeHead(200, {
        "Content-Type": "application/json",
        "Set-Cookie": `pty_token=${this.authToken}; Path=/; HttpOnly; SameSite=Strict`,
      });
      res.end(JSON.stringify({ ok: true }));
    } else {
      res.writeHead(403, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ ok: false, numeric: this.isNumericCode() }));
    }
  }

  private serveIndex(res: http.ServerResponse): void {
    const indexPath = path.join(__dirname, "static", "index.html");
    try {
      const content = fs.readFileSync(indexPath, "utf-8");
      res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
      res.end(content);
    } catch {
      res.writeHead(500);
      res.end("Internal Server Error");
    }
  }

  private serveVendor(pathname: string, res: http.ServerResponse): void {
    const nodeModules = path.join(__dirname, "..", "..", "node_modules");

    const vendorMap: Record<string, { file: string; contentType: string }> = {
      "/vendor/xterm.mjs": {
        file: path.join(nodeModules, "@xterm", "xterm", "lib", "xterm.mjs"),
        contentType: "application/javascript",
      },
      "/vendor/xterm.css": {
        file: path.join(nodeModules, "@xterm", "xterm", "css", "xterm.css"),
        contentType: "text/css",
      },
      "/vendor/addon-fit.mjs": {
        file: path.join(
          nodeModules,
          "@xterm",
          "addon-fit",
          "lib",
          "addon-fit.mjs"
        ),
        contentType: "application/javascript",
      },
    };

    const vendor = vendorMap[pathname];
    if (!vendor) {
      res.writeHead(404);
      res.end("Not Found");
      return;
    }

    try {
      const content = fs.readFileSync(vendor.file);
      res.writeHead(200, { "Content-Type": vendor.contentType });
      res.end(content);
    } catch {
      res.writeHead(500);
      res.end("Vendor file not found");
    }
  }

  private handleWS(wsClient: WSType, name: string): void {
    const socketPath = getSocketPath(name);
    const unixSocket = net.createConnection(socketPath);
    const packetReader = new PacketReader();

    const conn = { ws: wsClient, unix: unixSocket };
    this.connections.add(conn);

    unixSocket.on("error", () => {
      wsClient.close();
      this.connections.delete(conn);
    });

    unixSocket.on("connect", () => {
      // Browser -> PtyServer: forward raw bytes
      wsClient.on("message", (data: Buffer | ArrayBuffer | Buffer[]) => {
        if (Buffer.isBuffer(data)) {
          unixSocket.write(data);
        } else if (data instanceof ArrayBuffer) {
          unixSocket.write(Buffer.from(data));
        } else {
          unixSocket.write(Buffer.concat(data));
        }
      });

      // PtyServer -> Browser: re-frame into complete packets
      unixSocket.on("data", (data: Buffer) => {
        for (const packet of packetReader.feed(data)) {
          if (wsClient.readyState === 1) {
            // OPEN
            wsClient.send(encodePacket(packet.type, packet.payload));
          }
        }
      });
    });

    unixSocket.on("close", () => {
      wsClient.close();
      this.connections.delete(conn);
    });

    wsClient.on("close", () => {
      unixSocket.destroy();
      this.connections.delete(conn);
    });

    wsClient.on("error", () => {
      unixSocket.destroy();
      this.connections.delete(conn);
    });
  }
}

function readBody(req: http.IncomingMessage): Promise<string> {
  return new Promise((resolve) => {
    let body = "";
    req.on("data", (chunk: Buffer) => {
      body += chunk.toString();
    });
    req.on("end", () => resolve(body));
  });
}
