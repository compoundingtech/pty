#!/usr/bin/env node
// Fires a large volume of lines as fast as Node can write them, to stand
// in for a child like `find /` whose output rate can outpace naive
// signal + render loops. Used by the "stays interactive under load"
// integration test.

const TOTAL = 5_000;
for (let i = 0; i < TOTAL; i++) {
  const stream = i % 13 === 0 ? process.stderr : process.stdout;
  stream.write(`line ${i}\n`);
}
// Keep the process alive so the test can poke at the TUI after the
// firehose finishes writing.
setTimeout(() => process.exit(0), 10_000);
process.on("SIGTERM", () => process.exit(0));
