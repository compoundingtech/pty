// Raw stdin input parsing — keyboard + mouse.

export interface KeyEvent {
  /** Discriminator for InputEvent union. Implicit on the existing API so
   *  pre-mouse consumers keep working — the default is "key" when absent. */
  kind?: "key";
  name: string;
  char?: string;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
}

export type MouseButton = "left" | "middle" | "right" | "none";
export type MouseAction = "press" | "release" | "drag" | "move" | "scrollUp" | "scrollDown";

export interface MouseEvent {
  kind: "mouse";
  action: MouseAction;
  button: MouseButton;
  /** 0-based column of the cell the pointer is over. */
  x: number;
  /** 0-based row. */
  y: number;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
}

export type InputEvent = KeyEvent | MouseEvent;

// Kitty keyboard-protocol codepoints that must decode to a NAMED key event
// (`{ name: "escape" }`) rather than the raw control char (`{ name: "\x1b" }`),
// so consumers matching `key.name === "escape"` / "return" / … work the same
// under CSI-u as under the legacy encoding. Mirrors the legacy bare-key parsing
// below (0x1b→escape, 0x0d→return, 0x09→tab, 0x7f→backspace) and the inverse
// of `CSI_U_KEYCODES` in keys.ts. Space (0x20) is intentionally absent — it
// decodes to a printable " " char via the default path, which types correctly.
const KITTY_CODEPOINT_NAMES: Record<number, string> = {
  27: "escape",
  13: "return",
  9: "tab",
  127: "backspace",
};

/** Type guard: narrows an InputEvent to a MouseEvent. */
export function isMouseEvent(e: InputEvent): e is MouseEvent {
  return (e as MouseEvent).kind === "mouse";
}

/** ANSI sequences to enable / disable SGR mouse reporting. Use these when
 *  your app wants to receive mouse events. `parseInput` knows how to
 *  decode them once enabled. */
export const MOUSE_ENABLE_SGR = "\x1b[?1002h\x1b[?1006h";
export const MOUSE_DISABLE_SGR = "\x1b[?1006l\x1b[?1002l";

function decodeMouse(
  buttonCode: number, x: number, y: number, isRelease: boolean,
): MouseEvent | null {
  // SGR mouse button-code encoding:
  //   low 2 bits:   button (0=left, 1=middle, 2=right, 3=none for motion)
  //   bit 2 (0x04): shift
  //   bit 3 (0x08): alt
  //   bit 4 (0x10): ctrl
  //   bit 5 (0x20): motion flag (drag when a button is down)
  //   bit 6 (0x40): wheel (64 = scroll-up, 65 = scroll-down)
  const shift = (buttonCode & 0x04) !== 0;
  const alt   = (buttonCode & 0x08) !== 0;
  const ctrl  = (buttonCode & 0x10) !== 0;
  const motion = (buttonCode & 0x20) !== 0;
  const wheel = (buttonCode & 0x40) !== 0;
  const low = buttonCode & 0x03;

  const common = { x: Math.max(0, x - 1), y: Math.max(0, y - 1), ctrl, alt, shift };

  if (wheel) {
    return {
      kind: "mouse",
      action: low === 0 ? "scrollUp" : "scrollDown",
      button: "none",
      ...common,
    };
  }

  const button: MouseButton =
    low === 0 ? "left" : low === 1 ? "middle" : low === 2 ? "right" : "none";

  if (isRelease) {
    return { kind: "mouse", action: "release", button, ...common };
  }
  if (motion) {
    return { kind: "mouse", action: button === "none" ? "move" : "drag", button, ...common };
  }
  return { kind: "mouse", action: "press", button, ...common };
}

/** Legacy entry point — keyboard only. Existing consumers keep using this;
 *  new code that wants mouse should call `parseInput` instead. */
export function parseKey(data: Buffer): KeyEvent[] {
  return parseInput(data).filter((e): e is KeyEvent => !isMouseEvent(e));
}

/** Parse a stdin chunk into an ordered list of keyboard + mouse events. */
export function parseInput(data: Buffer): InputEvent[] {
  const events: InputEvent[] = [];
  const str = data.toString("utf8");
  let i = 0;

  while (i < str.length) {
    // ESC sequences
    if (str[i] === "\x1b") {
      // ESC [ ... sequences (CSI)
      if (i + 1 < str.length && str[i + 1] === "[") {
        const rest = str.slice(i + 2);

        // SGR mouse: ESC[<b;x;y;(M|m)
        // Recognised BEFORE the generic-arrow branches because the leading
        // `<` disambiguates unambiguously and any other CSI starts with a
        // parameter or a letter (never `<`).
        if (rest[0] === "<") {
          const mouseMatch = rest.match(/^<(\d+);(\d+);(\d+)([Mm])/);
          if (mouseMatch) {
            const b = parseInt(mouseMatch[1], 10);
            const x = parseInt(mouseMatch[2], 10);
            const y = parseInt(mouseMatch[3], 10);
            const release = mouseMatch[4] === "m";
            const ev = decodeMouse(b, x, y, release);
            if (ev) events.push(ev);
            i += 2 + mouseMatch[0].length;
            continue;
          }
        }

        // Arrow keys, home, end
        if (rest[0] === "A") { events.push({ name: "up", ctrl: false, alt: false, shift: false }); i += 3; continue; }
        if (rest[0] === "B") { events.push({ name: "down", ctrl: false, alt: false, shift: false }); i += 3; continue; }
        if (rest[0] === "C") { events.push({ name: "right", ctrl: false, alt: false, shift: false }); i += 3; continue; }
        if (rest[0] === "D") { events.push({ name: "left", ctrl: false, alt: false, shift: false }); i += 3; continue; }
        if (rest[0] === "H") { events.push({ name: "home", ctrl: false, alt: false, shift: false }); i += 3; continue; }
        if (rest[0] === "F") { events.push({ name: "end", ctrl: false, alt: false, shift: false }); i += 3; continue; }

        // Arrows with modifiers: ESC[1;<mods><letter>.
        // mods - 1 is a bitmask: bit0=shift, bit1=alt, bit2=ctrl (same scheme
        // as the kitty keyboard protocol). Terminal.app and kitty both emit
        // this form for option+arrow (alt) and shift+arrow.
        const modArrow = rest.match(/^1;(\d+)([ABCDHF])/);
        if (modArrow) {
          const mods = parseInt(modArrow[1], 10) - 1;
          const shift = (mods & 0x01) !== 0;
          const alt   = (mods & 0x02) !== 0;
          const ctrl  = (mods & 0x04) !== 0;
          const letterToName: Record<string, string> = {
            A: "up", B: "down", C: "right", D: "left", H: "home", F: "end",
          };
          events.push({ name: letterToName[modArrow[2]], ctrl, alt, shift });
          i += 2 + modArrow[0].length;
          continue;
        }

        // Shift+Tab (legacy xterm encoding): ESC[Z
        if (rest[0] === "Z") { events.push({ name: "backtab", ctrl: false, alt: false, shift: true }); i += 3; continue; }

        // Delete: ESC[3~
        if (rest.startsWith("3~")) { events.push({ name: "delete", ctrl: false, alt: false, shift: false }); i += 4; continue; }
        // Page Up: ESC[5~
        if (rest.startsWith("5~")) { events.push({ name: "pageup", ctrl: false, alt: false, shift: false }); i += 4; continue; }
        // Page Down: ESC[6~
        if (rest.startsWith("6~")) { events.push({ name: "pagedown", ctrl: false, alt: false, shift: false }); i += 4; continue; }

        // Kitty keyboard protocol: ESC[<code>[;<modifiers>]u. The modifiers
        // param is OPTIONAL — kitty OMITS it when no modifiers are held, so a
        // bare Escape arrives as `ESC[27u` (not `ESC[27;1u`). Requiring the `;`
        // made that form fall through to the "unknown CSI" skip below and the
        // key was silently lost (the two-stage esc "did nothing" bug).
        const kittyMatch = rest.match(/^(\d+)(?:;(\d+))?u/);
        if (kittyMatch) {
          const codepoint = parseInt(kittyMatch[1], 10);
          // Wire format is (modifiers + 1); absent = 1 (no modifiers).
          // Bit 0 = shift, bit 1 = alt, bit 2 = ctrl, bit 3 = super (ignored).
          const mods = (kittyMatch[2] ? parseInt(kittyMatch[2], 10) : 1) - 1;
          const shift = (mods & 0x01) !== 0;
          const alt = (mods & 0x02) !== 0;
          const ctrl = (mods & 0x04) !== 0;
          // Shift+Tab via kitty protocol -> canonical "backtab" so consumers
          // can handle it the same as the legacy ESC[Z encoding.
          if (codepoint === 0x09 && shift) {
            events.push({ name: "backtab", ctrl, alt, shift });
          } else if (KITTY_CODEPOINT_NAMES[codepoint]) {
            // Escape / return / tab / backspace must decode to their NAMED event
            // (consumers match key.name), not the raw control char.
            events.push({ name: KITTY_CODEPOINT_NAMES[codepoint], ctrl, alt, shift });
          } else {
            const ch = String.fromCodePoint(codepoint);
            events.push({ name: ch, char: ch, ctrl, alt, shift });
          }
          i += 2 + kittyMatch[0].length;
          continue;
        }

        // Unknown CSI sequence — skip to end
        let j = 0;
        while (j < rest.length && !(rest[j] >= "@" && rest[j] <= "~")) j++;
        i += 2 + j + 1;
        continue;
      }

      // Alt+<char>: ESC followed by printable character
      if (i + 1 < str.length && str[i + 1] >= " ") {
        const ch = str[i + 1];
        events.push({ name: ch, char: ch, ctrl: false, alt: true, shift: false });
        i += 2;
        continue;
      }

      // Bare ESC
      events.push({ name: "escape", ctrl: false, alt: false, shift: false });
      i++;
      continue;
    }

    // Control characters
    const code = str.charCodeAt(i);

    if (code === 0x0d) { events.push({ name: "return", ctrl: false, alt: false, shift: false }); i++; continue; }
    if (code === 0x09) { events.push({ name: "tab", ctrl: false, alt: false, shift: false }); i++; continue; }
    if (code === 0x7f) { events.push({ name: "backspace", ctrl: false, alt: false, shift: false }); i++; continue; }
    if (code === 0x1c) { events.push({ name: "\\", ctrl: true, alt: false, shift: false }); i++; continue; }

    // Ctrl+A through Ctrl+Z (0x01–0x1a)
    if (code >= 0x01 && code <= 0x1a) {
      const letter = String.fromCharCode(code + 0x60); // a-z
      events.push({ name: letter, ctrl: true, alt: false, shift: false });
      i++;
      continue;
    }

    // Regular printable character
    if (code >= 0x20) {
      events.push({ name: str[i], char: str[i], ctrl: false, alt: false, shift: false });
      i++;
      continue;
    }

    // Unknown — skip
    i++;
  }

  return events;
}
