// Standalone entry point for the supervisor.
// Bundled by `pty supervisor launchd install` into a single portable JS file.
// SERVER_MODULE_PATH is injected at bundle time by esbuild --define.

import { Supervisor } from "./supervisor.ts";
import { getSessionDir } from "./sessions.ts";
import { setServerModulePath } from "./spawn.ts";

declare const SERVER_MODULE_PATH: string;
if (typeof SERVER_MODULE_PATH !== "undefined") {
  setServerModulePath(SERVER_MODULE_PATH);
}

const supervisor = new Supervisor("supervisor");
supervisor.start();

console.log(`[supervisor] started (pid ${process.pid})`);
console.log(`[supervisor] watching ${getSessionDir()}`);

process.on("SIGTERM", () => {
  console.log("[supervisor] stopping...");
  supervisor.stop();
  process.exit(0);
});

process.on("SIGINT", () => {
  console.log("[supervisor] stopping...");
  supervisor.stop();
  process.exit(0);
});
