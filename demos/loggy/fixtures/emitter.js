#!/usr/bin/env node
// Fixture used by loggy integration tests. Emits a paced, deterministic
// sequence of stdout and stderr lines, then stays alive long enough for
// the test to exercise the TUI.
//
// Line cadence is deliberately slower than the TUI's render tick so each
// line reliably shows up in at least one frame before the next arrives.

process.stdout.write("stdout one\n");
process.stderr.write("stderr one\n");

setTimeout(() => process.stdout.write("stdout two\n"), 60);
setTimeout(() => process.stderr.write("ERROR: something broke\n"), 120);
setTimeout(() => process.stdout.write("stdout three\n"), 180);
setTimeout(() => process.stdout.write("stdout four\n"), 240);
setTimeout(() => process.stderr.write("stderr four\n"), 300);

// Allow the test to interact (filter, search, scroll) before the child
// exits on its own.
setTimeout(() => process.exit(0), 10_000);

process.on("SIGTERM", () => process.exit(0));
