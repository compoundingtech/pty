import { describe, it, expect } from "vitest";
import {
  createCommandPaletteState, filterCommands,
  handleCommandPaletteKey, renderCommandPalette,
  type Command,
} from "../src/tui/widgets/command-palette.ts";
import type { KeyEvent } from "../src/tui/input.ts";

function k(name: string, opts: Partial<KeyEvent> = {}): KeyEvent {
  return { name, char: opts.char, ctrl: opts.ctrl ?? false, alt: opts.alt ?? false, shift: opts.shift ?? false };
}

const commands: Command[] = [
  { id: "open", label: "Open file", hint: "in the editor", keywords: ["edit"], run() {} },
  { id: "save", label: "Save", hint: "current file", run() {} },
  { id: "quit", label: "Quit", keywords: ["exit"], run() {} },
  { id: "new", label: "New reminder", run() {} },
];

describe("command palette — filtering", () => {
  it("empty query returns all commands in order", () => {
    const ranked = filterCommands(commands, "");
    expect(ranked.map(r => r.cmd.id)).toEqual(["open", "save", "quit", "new"]);
  });

  it("fuzzy match across label + hint + keywords", () => {
    expect(filterCommands(commands, "ex").map(r => r.cmd.id)).toContain("quit"); // via "exit" keyword
    expect(filterCommands(commands, "edi").map(r => r.cmd.id)).toContain("open"); // via "edit" keyword
  });

  it("ranks exact prefix matches above mid-string ones", () => {
    const r = filterCommands(commands, "sa");
    // "Save" should beat "reminder" (no "sa" in reminder)
    expect(r[0].cmd.id).toBe("save");
  });

  it("no match when the query cannot be formed", () => {
    expect(filterCommands(commands, "xyzq")).toHaveLength(0);
  });
});

describe("command palette — keys", () => {
  const s0 = createCommandPaletteState();

  it("printable keys edit the query and reset selection", () => {
    const r = handleCommandPaletteKey({ ...s0, selectedIndex: 3 }, commands, k("o", { char: "o" }));
    expect(r.action).toBe("edited");
    expect(r.state.query.text).toBe("o");
    expect(r.state.selectedIndex).toBe(0);
  });

  it("up/down walks the ranked list", () => {
    // Empty query returns all commands — walk through them.
    const down1 = handleCommandPaletteKey(s0, commands, k("down"));
    expect(down1.state.selectedIndex).toBe(1);
    expect(down1.action).toBe("moved");
    const down2 = handleCommandPaletteKey(down1.state, commands, k("down"));
    expect(down2.state.selectedIndex).toBe(2);
    const up = handleCommandPaletteKey(down2.state, commands, k("up"));
    expect(up.state.selectedIndex).toBe(1);
    // Up at top clamps.
    const clamped = handleCommandPaletteKey(s0, commands, k("up"));
    expect(clamped.state.selectedIndex).toBe(0);
  });

  it("return emits 'run' with the command", () => {
    const r = handleCommandPaletteKey(s0, commands, k("return"));
    expect(r.action).toBe("run");
    expect(r.command?.id).toBe("open");
  });

  it("escape emits 'cancel'", () => {
    const r = handleCommandPaletteKey(s0, commands, k("escape"));
    expect(r.action).toBe("cancel");
  });
});

describe("command palette — rendering", () => {
  it("renders a panel with query + limited commands", () => {
    const s = createCommandPaletteState();
    const node = renderCommandPalette(s, commands, { limit: 2 });
    expect((node as any).type).toBe("panel");
    // 1 query row + 2 command rows = 3 rows in the panel body.
    expect((node as any).children).toHaveLength(3);
  });

  it("shows 'no matches' when the query excludes everything", () => {
    const s = { query: { text: "xyzq", cursor: 4 }, selectedIndex: 0 };
    const node = renderCommandPalette(s, commands);
    const children = (node as any).children;
    // row 0 is query; row 1 is the no-matches message.
    const msg = children[1].children[0].text;
    expect(msg).toMatch(/no matches/);
  });
});
