import { describe, it, expect } from "vitest";
import { resolveKey, parseSeqValue } from "../src/keys.ts";

describe("resolveKey", () => {
  it("resolves named keys", () => {
    expect(resolveKey("return")).toBe("\r");
    expect(resolveKey("enter")).toBe("\r");
    expect(resolveKey("tab")).toBe("\t");
    expect(resolveKey("escape")).toBe("\x1b");
    expect(resolveKey("esc")).toBe("\x1b");
    expect(resolveKey("space")).toBe(" ");
    expect(resolveKey("backspace")).toBe("\x7f");
    expect(resolveKey("delete")).toBe("\x1b[3~");
  });

  it("resolves arrow keys", () => {
    expect(resolveKey("up")).toBe("\x1b[A");
    expect(resolveKey("down")).toBe("\x1b[B");
    expect(resolveKey("right")).toBe("\x1b[C");
    expect(resolveKey("left")).toBe("\x1b[D");
  });

  it("resolves navigation keys", () => {
    expect(resolveKey("home")).toBe("\x1b[H");
    expect(resolveKey("end")).toBe("\x1b[F");
    expect(resolveKey("pageup")).toBe("\x1b[5~");
    expect(resolveKey("pagedown")).toBe("\x1b[6~");
  });

  it("resolves ctrl chords", () => {
    expect(resolveKey("ctrl+c")).toBe("\x03");
    expect(resolveKey("ctrl+a")).toBe("\x01");
    expect(resolveKey("ctrl+z")).toBe("\x1a");
    expect(resolveKey("ctrl+d")).toBe("\x04");
  });

  it("resolves alt chords", () => {
    expect(resolveKey("alt+x")).toBe("\x1bx");
    expect(resolveKey("alt+a")).toBe("\x1ba");
  });

  it("resolves shift chords for letters", () => {
    expect(resolveKey("shift+a")).toBe("A");
    expect(resolveKey("shift+z")).toBe("Z");
  });

  it("resolves shift+return via CSI u encoding", () => {
    expect(resolveKey("shift+return")).toBe("\x1b[13;2u");
    expect(resolveKey("shift+enter")).toBe("\x1b[13;2u");
  });

  it("resolves shift+tab as legacy backtab", () => {
    expect(resolveKey("shift+tab")).toBe("\x1b[Z");
  });

  it("resolves shift+escape and shift+space via CSI u", () => {
    expect(resolveKey("shift+escape")).toBe("\x1b[27;2u");
    expect(resolveKey("shift+space")).toBe("\x1b[32;2u");
    expect(resolveKey("shift+backspace")).toBe("\x1b[127;2u");
  });

  it("resolves shift+arrow keys with modifier parameter", () => {
    expect(resolveKey("shift+up")).toBe("\x1b[1;2A");
    expect(resolveKey("shift+down")).toBe("\x1b[1;2B");
    expect(resolveKey("shift+right")).toBe("\x1b[1;2C");
    expect(resolveKey("shift+left")).toBe("\x1b[1;2D");
  });

  it("resolves shift+navigation keys with modifier parameter", () => {
    expect(resolveKey("shift+home")).toBe("\x1b[1;2H");
    expect(resolveKey("shift+end")).toBe("\x1b[1;2F");
    expect(resolveKey("shift+pageup")).toBe("\x1b[5;2~");
    expect(resolveKey("shift+pagedown")).toBe("\x1b[6;2~");
    expect(resolveKey("shift+delete")).toBe("\x1b[3;2~");
  });

  it("resolves ctrl+shift combinations", () => {
    expect(resolveKey("ctrl+shift+up")).toBe("\x1b[1;6A");
    expect(resolveKey("ctrl+shift+return")).toBe("\x1b[13;6u");
  });

  it("resolves alt+shift combinations", () => {
    expect(resolveKey("alt+shift+up")).toBe("\x1b[1;4A");
    expect(resolveKey("alt+shift+return")).toBe("\x1b[13;4u");
  });

  it("resolves ctrl+alt on named keys", () => {
    expect(resolveKey("ctrl+alt+up")).toBe("\x1b[1;7A");
    expect(resolveKey("ctrl+alt+delete")).toBe("\x1b[3;7~");
  });

  it("resolves all three modifiers combined", () => {
    expect(resolveKey("ctrl+alt+shift+up")).toBe("\x1b[1;8A");
    expect(resolveKey("ctrl+alt+shift+return")).toBe("\x1b[13;8u");
  });

  it("resolves composed modifiers", () => {
    expect(resolveKey("ctrl+alt+c")).toBe("\x1b\x03");
    expect(resolveKey("alt+ctrl+c")).toBe("\x1b\x03");
  });

  it("is case insensitive", () => {
    expect(resolveKey("Ctrl+C")).toBe("\x03");
    expect(resolveKey("RETURN")).toBe("\r");
    expect(resolveKey("Alt+X")).toBe("\x1bx");
  });

  it("throws on unknown key", () => {
    expect(() => resolveKey("f99")).toThrow(/Unknown key/);
    expect(() => resolveKey("nonexistent")).toThrow(/Unknown key/);
  });

  it("throws on unknown modifier", () => {
    expect(() => resolveKey("super+c")).toThrow(/Unknown modifier/);
    expect(() => resolveKey("meta+x")).toThrow(/Unknown modifier/);
  });
});

describe("parseSeqValue", () => {
  it("resolves key: prefixed values", () => {
    expect(parseSeqValue("key:return")).toBe("\r");
    expect(parseSeqValue("key:ctrl+c")).toBe("\x03");
    expect(parseSeqValue("key:tab")).toBe("\t");
  });

  it("passes through literal strings", () => {
    expect(parseSeqValue("hello")).toBe("hello");
    expect(parseSeqValue("git status")).toBe("git status");
    expect(parseSeqValue("")).toBe("");
  });
});
