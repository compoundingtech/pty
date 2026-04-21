import { describe, it, expect, beforeEach } from "vitest";
import {
  registerGlobalCommand, useCommandScope, clearCommandScope,
  findCommand, runCommand, allCommands, _resetCommandRegistry,
} from "../src/tui/widgets/command-registry.ts";

beforeEach(() => { _resetCommandRegistry(); });

describe("command registry — global", () => {
  it("starts empty", () => {
    expect(allCommands.get()).toEqual([]);
  });

  it("registerGlobalCommand adds, disposer removes", () => {
    const dispose = registerGlobalCommand({ id: "a", label: "A", run() {} });
    expect(allCommands.get().map(c => c.id)).toEqual(["a"]);
    dispose();
    expect(allCommands.get()).toEqual([]);
  });

  it("multiple global commands accumulate", () => {
    registerGlobalCommand({ id: "a", label: "A", run() {} });
    registerGlobalCommand({ id: "b", label: "B", run() {} });
    expect(allCommands.get().map(c => c.id)).toEqual(["a", "b"]);
  });
});

describe("command registry — scopes", () => {
  it("useCommandScope adds under the given id", () => {
    useCommandScope("s1", [
      { id: "s1.new",    label: "New",    run() {} },
      { id: "s1.remove", label: "Remove", run() {} },
    ]);
    expect(allCommands.get().map(c => c.id)).toEqual(["s1.new", "s1.remove"]);
  });

  it("replacing a scope replaces its commands entirely", () => {
    useCommandScope("sel", [{ id: "a", label: "A", run() {} }]);
    useCommandScope("sel", [{ id: "b", label: "B", run() {} }]);
    expect(allCommands.get().map(c => c.id)).toEqual(["b"]);
  });

  it("scope disposer removes the whole batch", () => {
    const dispose = useCommandScope("sel", [
      { id: "a", label: "A", run() {} },
      { id: "b", label: "B", run() {} },
    ]);
    dispose();
    expect(allCommands.get()).toEqual([]);
  });

  it("clearing a scope by id works without holding a disposer", () => {
    useCommandScope("sel", [{ id: "a", label: "A", run() {} }]);
    clearCommandScope("sel");
    expect(allCommands.get()).toEqual([]);
  });

  it("rejects the reserved global sentinel id", () => {
    expect(() => useCommandScope("__global__", [])).toThrow();
  });

  it("compose: global + screen + focus", () => {
    registerGlobalCommand({ id: "g.quit", label: "Quit", run() {} });
    useCommandScope("screen:list", [{ id: "list.new", label: "New", run() {} }]);
    useCommandScope("focused:a", [{ id: "a.complete", label: "Complete A", run() {} }]);
    const ids = allCommands.get().map(c => c.id);
    expect(ids).toContain("g.quit");
    expect(ids).toContain("list.new");
    expect(ids).toContain("a.complete");
    expect(ids).toHaveLength(3);
  });
});

describe("command registry — lookup + run", () => {
  it("findCommand locates across scopes", () => {
    useCommandScope("s", [{ id: "x", label: "X", run() {} }]);
    expect(findCommand("x")?.id).toBe("x");
    expect(findCommand("nope")).toBeUndefined();
  });

  it("runCommand invokes by id and returns true", () => {
    let called = 0;
    useCommandScope("s", [{ id: "x", label: "X", run() { called++; } }]);
    expect(runCommand("x")).toBe(true);
    expect(called).toBe(1);
  });

  it("runCommand returns false for unknown ids", () => {
    expect(runCommand("nope")).toBe(false);
  });
});

describe("command registry — reactive", () => {
  it("allCommands recomputes on registration", () => {
    let observed = allCommands.get().length;
    expect(observed).toBe(0);

    const dispose = registerGlobalCommand({ id: "q", label: "Q", run() {} });
    expect(allCommands.get().length).toBe(1);

    useCommandScope("s", [{ id: "a", label: "A", run() {} }]);
    expect(allCommands.get().length).toBe(2);

    dispose();
    expect(allCommands.get().length).toBe(1);
  });
});
