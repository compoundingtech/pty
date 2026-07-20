// "Atoms" — the smallest display primitives.

import {
  text, row, column, dot, checkbox, progressBar, spinner, icon,
  separator, gap, indent,
  signal,
} from "../../../src/tui/index.ts";
import type { Demo } from "../types.ts";

const progress = signal(30);
const checked = signal(false);

function atomsDemo(): Demo {
  return {
    id: "atoms",
    category: "atoms",
    name: "text / dot / checkbox / progress",
    blurb: "The smallest display primitives. Press +/- to change progress, space to toggle.",
    render() {
      return [
        row(text("  plain ", "primary")),
        row(text("  accent ", "accent"), text(" bold ", "accent", { bold: true }), text(" dim", "muted", { dim: true })),
        row(text("  error ", "error"), text(" warn ", "warn"), text(" ok", "ok")),
        separator(),
        row(text("  dot: ", "muted"), dot(true, "accent"), dot(false, "muted")),
        row(text("  checkbox: ", "muted"), checkbox(checked.get(), "accent"),
            text(`  ${checked.get() ? "checked" : "unchecked"}`, "primary")),
        row(text("  spinner: ", "muted"), spinner("accent"),
            text(" (animates when plugged into startSpinnerTimer)", "muted", { dim: true })),
        row(text("  icon: ", "muted"), icon("\u2605", "accent"), text(" star", "primary")),
        separator(),
        row(text("  progress ", "muted"), progressBar(progress.get() / 100, 30, "accent")),
        row(text(`  value = ${progress.get()}%`, "muted", { dim: true })),
      ];
    },
    handleKey(key) {
      if (key.char === "+") { progress.set(Math.min(100, progress.peek() + 5)); return true; }
      if (key.char === "-") { progress.set(Math.max(0, progress.peek() - 5)); return true; }
      if (key.char === " ") { checked.set(!checked.peek()); return true; }
      return false;
    },
  };
}

const source = String.raw`import { text, dot, checkbox, progressBar, spinner } from "@compoundingtech/pty/tui";

row(text("accent", "accent", { bold: true }))
row(dot(true, "accent"))
row(checkbox(true, "accent"))
row(progressBar(0.3, 30, "accent"))
row(spinner("accent"))`;

export const atoms: Demo = { ...atomsDemo(), source };
