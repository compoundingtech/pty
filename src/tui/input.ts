// Raw stdin keypress parsing

export interface KeyEvent {
  name: string;
  char?: string;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
}

export function parseKey(data: Buffer): KeyEvent[] {
  const events: KeyEvent[] = [];
  const str = data.toString("utf8");
  let i = 0;

  while (i < str.length) {
    // ESC sequences
    if (str[i] === "\x1b") {
      // ESC [ ... sequences (CSI)
      if (i + 1 < str.length && str[i + 1] === "[") {
        const rest = str.slice(i + 2);

        // Arrow keys, home, end
        if (rest[0] === "A") { events.push({ name: "up", ctrl: false, alt: false, shift: false }); i += 3; continue; }
        if (rest[0] === "B") { events.push({ name: "down", ctrl: false, alt: false, shift: false }); i += 3; continue; }
        if (rest[0] === "C") { events.push({ name: "right", ctrl: false, alt: false, shift: false }); i += 3; continue; }
        if (rest[0] === "D") { events.push({ name: "left", ctrl: false, alt: false, shift: false }); i += 3; continue; }
        if (rest[0] === "H") { events.push({ name: "home", ctrl: false, alt: false, shift: false }); i += 3; continue; }
        if (rest[0] === "F") { events.push({ name: "end", ctrl: false, alt: false, shift: false }); i += 3; continue; }

        // Shift+Tab (legacy xterm encoding): ESC[Z
        if (rest[0] === "Z") { events.push({ name: "backtab", ctrl: false, alt: false, shift: true }); i += 3; continue; }

        // Delete: ESC[3~
        if (rest.startsWith("3~")) { events.push({ name: "delete", ctrl: false, alt: false, shift: false }); i += 4; continue; }
        // Page Up: ESC[5~
        if (rest.startsWith("5~")) { events.push({ name: "pageup", ctrl: false, alt: false, shift: false }); i += 4; continue; }
        // Page Down: ESC[6~
        if (rest.startsWith("6~")) { events.push({ name: "pagedown", ctrl: false, alt: false, shift: false }); i += 4; continue; }

        // Kitty keyboard protocol: ESC[<code>;<modifiers>u
        const kittyMatch = rest.match(/^(\d+);(\d+)u/);
        if (kittyMatch) {
          const codepoint = parseInt(kittyMatch[1], 10);
          // Wire format is (modifiers + 1); subtract to get the bitmask.
          // Bit 0 = shift, bit 1 = alt, bit 2 = ctrl, bit 3 = super (ignored).
          const mods = parseInt(kittyMatch[2], 10) - 1;
          const shift = (mods & 0x01) !== 0;
          const alt = (mods & 0x02) !== 0;
          const ctrl = (mods & 0x04) !== 0;
          // Shift+Tab via kitty protocol -> canonical "backtab" so consumers
          // can handle it the same as the legacy ESC[Z encoding.
          if (codepoint === 0x09 && shift) {
            events.push({ name: "backtab", ctrl, alt, shift });
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
