// Ensure node-pty's prebuilt `spawn-helper` is executable. Necessary because
// node-pty's published tarball ships the file with mode 0644 and its own
// post-install never chmods it. The previous workaround used a relative path
// (`node_modules/node-pty/prebuilds/*/spawn-helper`) which silently no-ops
// under pnpm with `enableGlobalVirtualStore`, where node-pty lives in a
// sibling content-addressed link rather than nested under @compoundingtech/pty.
//
// The root-cause fix belongs in microsoft/node-pty; once that ships, this
// script can be removed entirely.

import * as fs from "node:fs";
import * as path from "node:path";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

if (process.platform === "win32") process.exit(0);

// Resolve from this package's root (package.json sibling), not from the
// script file, so that node-pty is discoverable regardless of whether
// node_modules is nested (npm/yarn/bun) or a sibling link (pnpm GVS).
const pkgDir = path.dirname(new URL(import.meta.url).pathname);
const requireFromPkg = createRequire(
  pathToFileURL(path.join(pkgDir, "..", "package.json")),
);

let nodePtyDir;
try {
  nodePtyDir = path.dirname(requireFromPkg.resolve("node-pty/package.json"));
} catch {
  console.warn("[@compoundingtech/pty] node-pty not found; skipping spawn-helper chmod");
  process.exit(0);
}

const helper = path.join(
  nodePtyDir,
  "prebuilds",
  `${process.platform}-${process.arch}`,
  "spawn-helper",
);

try {
  fs.chmodSync(helper, 0o755);
} catch {
  // Acceptable when there's no prebuild for this arch and node-pty was
  // built from source — node-gyp produces the binary executable already.
}
