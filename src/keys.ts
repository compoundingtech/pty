const KEY_MAP: Record<string, string> = {
  return: "\r",
  enter: "\r",
  tab: "\t",
  escape: "\x1b",
  esc: "\x1b",
  space: " ",
  backspace: "\x7f",
  delete: "\x1b[3~",
  up: "\x1b[A",
  down: "\x1b[B",
  right: "\x1b[C",
  left: "\x1b[D",
  home: "\x1b[H",
  end: "\x1b[F",
  pageup: "\x1b[5~",
  pagedown: "\x1b[6~",
};

const MODIFIERS = new Set(["ctrl", "alt", "shift"]);

/** Keycodes for CSI u encoding (Kitty keyboard protocol). */
const CSI_U_KEYCODES: Record<string, number> = {
  return: 13,
  enter: 13,
  tab: 9,
  escape: 27,
  esc: 27,
  space: 32,
  backspace: 127,
};

/** Compute xterm modifier parameter: 1 + bitmask(shift=1, alt=2, ctrl=4). */
function modifierParam(mods: Set<string>): number {
  return (
    1 +
    (mods.has("shift") ? 1 : 0) +
    (mods.has("alt") ? 2 : 0) +
    (mods.has("ctrl") ? 4 : 0)
  );
}

/** Parse a key spec like `ctrl+c`, `return`, `alt+x` into bytes. */
export function resolveKey(spec: string): string {
  const parts = spec.toLowerCase().split("+");
  const base = parts.pop()!;
  const mods = new Set(parts);

  // Validate modifiers
  for (const mod of mods) {
    if (!MODIFIERS.has(mod)) {
      throw new Error(`Unknown modifier: "${mod}" in key spec "${spec}"`);
    }
  }

  const isLetter = base.length === 1 && base >= "a" && base <= "z";
  const hasModifiers = mods.size > 0;
  const mapped = KEY_MAP[base];

  if (mapped === undefined && !isLetter) {
    throw new Error(`Unknown key: "${base}" in key spec "${spec}"`);
  }

  // Single letter keys
  if (isLetter) {
    let result = base;

    if (mods.has("shift")) {
      result = result.toUpperCase();
    }

    if (mods.has("ctrl")) {
      const code = result.toLowerCase().charCodeAt(0);
      result = String.fromCharCode(code - 96);
    }

    if (mods.has("alt")) {
      result = "\x1b" + result;
    }

    return result;
  }

  // Named keys without modifiers: return the mapped value directly
  if (!hasModifiers) {
    return mapped;
  }

  const mod = modifierParam(mods);

  // Special case: shift+tab produces legacy backtab sequence
  if (base === "tab" && mod === 2) {
    return "\x1b[Z";
  }

  // CSI sequences: \x1b[N~ (e.g. delete, pageup) or \x1b[X (e.g. arrows, home, end)
  const csiTilde = mapped.match(/^\x1b\[(\d+)~$/);
  if (csiTilde) {
    return `\x1b[${csiTilde[1]};${mod}~`;
  }

  const csiLetter = mapped.match(/^\x1b\[([A-Z])$/);
  if (csiLetter) {
    return `\x1b[1;${mod}${csiLetter[1]}`;
  }

  // Control char keys (return, tab, escape, space, backspace): use CSI u encoding
  const keycode = CSI_U_KEYCODES[base];
  if (keycode !== undefined) {
    return `\x1b[${keycode};${mod}u`;
  }

  return mapped;
}

/** If value starts with `key:`, resolve the key name; otherwise return the literal string. */
export function parseSeqValue(value: string): string {
  if (value.startsWith("key:")) {
    return resolveKey(value.slice(4));
  }
  return value;
}
