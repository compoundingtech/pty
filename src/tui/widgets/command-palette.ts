// Command palette — a fuzzy-matched action runner overlay. A classic
// "ctrl+p" / "ctrl+k" pattern: type → filter → enter to run.
//
// The consumer owns a `signal<CommandPaletteState | null>` (null when
// the palette is closed). When opening, register your commands and pass
// them through on every render + key dispatch. State is pure.

import { row, column, text, panel } from "../builders.ts";
import type { UINode, ColumnNode } from "../nodes.ts";
import type { KeyEvent } from "../input.ts";
import { fuzzyMatch } from "../fuzzy.ts";
import { applyTextKey, type TextFieldState } from "./form.ts";

export interface Command {
  id: string;
  label: string;
  /** Optional one-liner shown dim next to the label. */
  hint?: string;
  /** Arbitrary keywords mixed into the fuzzy match (tags, shortcuts). */
  keywords?: string[];
  /** Invoked when the user selects this command. Up to the consumer to
   *  define — a command is just a row with an id + a callback. */
  run: () => void;
}

export interface CommandPaletteState {
  query: TextFieldState;
  selectedIndex: number;
}

export function createCommandPaletteState(): CommandPaletteState {
  return { query: { text: "", cursor: 0 }, selectedIndex: 0 };
}

export interface RankedCommand {
  cmd: Command;
  score: number;
}

/** Filter + rank commands by fuzzy match. Empty query returns all commands
 *  in their original order. */
export function filterCommands(
  commands: readonly Command[],
  query: string,
): RankedCommand[] {
  const q = query.trim();
  if (q.length === 0) return commands.map(c => ({ cmd: c, score: 0 }));
  const ranked: RankedCommand[] = [];
  for (const cmd of commands) {
    const hay = [cmd.label, cmd.hint ?? "", ...(cmd.keywords ?? [])].join(" ");
    const m = fuzzyMatch(q, hay);
    if (m.match) ranked.push({ cmd, score: m.score });
  }
  ranked.sort((a, b) => b.score - a.score);
  return ranked;
}

export interface HandleCommandPaletteResult {
  state: CommandPaletteState;
  action: "run" | "cancel" | "edited" | "moved" | "none";
  /** Present when action === "run". */
  command?: Command;
}

export function handleCommandPaletteKey(
  state: CommandPaletteState,
  commands: readonly Command[],
  key: KeyEvent,
): HandleCommandPaletteResult {
  if (key.name === "escape") {
    return { state, action: "cancel" };
  }

  const ranked = filterCommands(commands, state.query.text);

  if (key.name === "up") {
    const next = Math.max(0, state.selectedIndex - 1);
    return { state: { ...state, selectedIndex: next }, action: "moved" };
  }
  if (key.name === "down") {
    const next = Math.min(Math.max(0, ranked.length - 1), state.selectedIndex + 1);
    return { state: { ...state, selectedIndex: next }, action: "moved" };
  }
  if (key.name === "return") {
    const picked = ranked[state.selectedIndex];
    if (picked) return { state, action: "run", command: picked.cmd };
    return { state, action: "none" };
  }

  const updatedQuery = applyTextKey(state.query, key);
  if (updatedQuery) {
    return {
      state: { query: updatedQuery, selectedIndex: 0 },
      action: "edited",
    };
  }
  return { state, action: "none" };
}

/** Render the palette as an overlay panel with a query line + up to `limit`
 *  matching commands. The selected row is highlighted. */
export function renderCommandPalette(
  state: CommandPaletteState,
  commands: readonly Command[],
  opts: { title?: string; limit?: number } = {},
): UINode {
  const ranked = filterCommands(commands, state.query.text);
  const limit = opts.limit ?? 10;
  const visible = ranked.slice(0, limit);

  const queryLine = row(
    text("  > ", "accent", { bold: true }),
    text(state.query.text || "", "primary"),
    text("\u2588", "accent"),
  );

  const rows: UINode[] = [queryLine];
  if (visible.length === 0) {
    rows.push(row(text("  no matches", "muted", { dim: true })));
  } else {
    visible.forEach((r, i) => {
      const selected = i === state.selectedIndex;
      const prefix = selected ? text("  \u25b8 ", "accent", { bold: true })
                              : text("    ", "muted");
      const label = text(r.cmd.label, selected ? "accent" : "primary", { bold: selected });
      const children: UINode[] = [prefix, label];
      if (r.cmd.hint) {
        children.push(text(`  ${r.cmd.hint}`, "muted", { dim: true }));
      }
      rows.push(row(...children));
    });
  }

  return panel(opts.title ?? "command palette", rows);
}
