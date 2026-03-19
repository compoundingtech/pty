import type { Terminal } from "@xterm/headless";
import type { SerializeAddon } from "@xterm/addon-serialize";
import type { Screenshot } from "./types.ts";

export function captureScreenshot(
  terminal: Terminal,
  serialize: SerializeAddon
): Screenshot {
  const buffer = terminal.buffer.active;
  const lines: string[] = [];
  for (let i = 0; i < buffer.length; i++) {
    const line = buffer.getLine(i);
    if (line) {
      lines.push(line.translateToString(true));
    }
  }
  while (lines.length > 0 && lines[lines.length - 1].trim() === "") {
    lines.pop();
  }
  return {
    lines,
    text: lines.join("\n"),
    ansi: serialize.serialize(),
  };
}
