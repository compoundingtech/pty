// Overlays: confirm, toast, command palette, help. Confirm and command
// palette are shown inline here (not as real overlays on the whole app) so
// the playground can demo them without nesting overlays — the pattern is
// identical when wired through `AppConfig.overlay`.

import {
  text, row, column, panel, separator,
  signal,
  createConfirm, handleConfirmKey, confirmPanel,
  createToastQueue, pushToast, pruneExpired, renderToasts,
  createCommandPaletteState, handleCommandPaletteKey, renderCommandPalette,
  helpPanel,
  type ConfirmState, type ToastQueue, type CommandPaletteState, type Command, type HelpSection,
} from "../../../src/tui/index.ts";
import type { Demo } from "../types.ts";

// --- confirm ---
const confirmState = signal<ConfirmState>(createConfirm({
  title: "Delete this reminder?",
  message: "This cannot be undone.",
  yesLabel: "Delete",
  noLabel: "Keep",
}));
const confirmResult = signal<string>("(press y / n / tab / return / esc)");

export const confirmDemo: Demo = {
  id: "confirm",
  category: "overlays",
  name: "confirm modal",
  blurb: "Yes/no dialog with safe default on 'no'. Shortcut keys work.",
  render() {
    return [
      confirmPanel(confirmState.get()),
      row(text(`  last action: ${confirmResult.get()}`, "muted", { dim: true })),
    ];
  },
  handleKey(key) {
    const r = handleConfirmKey(confirmState.peek(), key);
    confirmState.set(r.state);
    if (r.action !== "pending") confirmResult.set(r.action);
    return true;
  },
  source: String.raw`const state = signal(createConfirm({
  title: "Delete?", message: "cannot be undone", yesLabel: "Delete", noLabel: "Keep",
}));
// in app overlay:
overlay: () => confirmState.get() ? confirmScreen : null
// in keys:
const r = handleConfirmKey(state.peek(), key);
state.set(r.state);
if (r.action === "yes") /* do the thing */`,
};

// --- toast ---
const toastQueue = signal<ToastQueue>(createToastQueue());

export const toastDemo: Demo = {
  id: "toast",
  category: "overlays",
  name: "toast notifications",
  blurb: "s: success toast   i: info   w: warning   e: error. They auto-expire after 3s.",
  render() {
    const q = pruneExpired(toastQueue.peek());
    if (q !== toastQueue.peek()) toastQueue.set(q);
    return [
      row(text("  press s / i / w / e to emit a toast. Queue:", "muted")),
      renderToasts(toastQueue.get()),
      separator(),
      row(text(`  live: ${toastQueue.get().toasts.length}`, "muted", { dim: true })),
    ];
  },
  handleKey(key) {
    const kinds: Record<string, "success" | "info" | "warn" | "error"> = {
      s: "success", i: "info", w: "warn", e: "error",
    };
    if (key.char && kinds[key.char]) {
      const kind = kinds[key.char];
      const msg = `${kind} at ${new Date().toLocaleTimeString()}`;
      toastQueue.set(pushToast(toastQueue.peek(), msg, { kind }));
      return true;
    }
    return false;
  },
  source: String.raw`const queue = signal(createToastQueue());
// push a toast anywhere in your app:
queue.set(pushToast(queue.peek(), "saved", { kind: "success" }));
// overlay: render the queue in a corner — corner-anchor layout is up to you
// periodically prune expired ones (every 500ms):
setInterval(() => queue.set(pruneExpired(queue.peek())), 500);`,
};

// --- command palette (dogfooding the registry) ---
import {
  registerGlobalCommand, useCommandScope, allCommands, findCommand,
} from "../../../src/tui/widgets/command-registry.ts";

// "Global" commands live for the app's lifetime. Register once at module
// load. These are the "always available" commands.
registerGlobalCommand({ id: "help",  label: "Help",  keywords: ["shortcuts"],           run() {} });
registerGlobalCommand({ id: "quit",  label: "Quit",  keywords: ["exit"],                run() {} });

// "Screen" commands — scoped to this demo screen. In a real app you'd
// register these inside onMount / onUnmount hooks. For the demo we just
// leave them registered; the scope isolation story is covered by the
// widgets-command-registry tests.
useCommandScope("screen:palette-demo", [
  { id: "new-reminder", label: "New reminder", hint: "create in the current list", run() {} },
  { id: "switch-list",  label: "Switch list",  hint: "jump to another folder",    run() {} },
]);

// "Focus" commands change with the currently-focused item. A selected-item
// signal drives which focus scope is active — setting a new item REPLACES
// the previous scope's commands in one call, which is exactly the semantic
// contextual commands want.
const focusedItem = signal<string | null>("Buy milk");

function registerFocusScope(title: string | null): void {
  if (!title) {
    useCommandScope("focus:item", []);
    return;
  }
  useCommandScope("focus:item", [
    { id: "item.complete", label: `Complete "${title}"`, keywords: ["done"],   run() {} },
    { id: "item.delete",   label: `Delete "${title}"`,                            run() {} },
    { id: "item.move",     label: `Move "${title}"`,                              run() {} },
  ]);
}
registerFocusScope(focusedItem.peek());

const paletteState = signal<CommandPaletteState>(createCommandPaletteState());
const paletteMessage = signal<string>("type to filter; tab to switch focused item; enter to run");

export const commandPaletteDemo: Demo = {
  id: "command-palette",
  category: "overlays",
  name: "command palette (registry)",
  blurb: "Fuzzy-matched action runner, but commands come from a signal-backed registry: global + screen scope + focused-item scope. Tab to change the focused item — notice the matching commands swap in.",
  render() {
    return [
      row(text(`  focused item: ${focusedItem.get() ?? "(none)"}   total commands: ${allCommands.get().length}`, "muted", { dim: true })),
      renderCommandPalette(paletteState.get(), allCommands.get(), { title: "what do you want to do?" }),
      row(text(`  ${paletteMessage.get()}`, "muted", { dim: true })),
    ];
  },
  handleKey(key) {
    // Tab rotates the "focused item" to demonstrate scope swap.
    if (key.name === "tab") {
      const next = focusedItem.peek() === "Buy milk" ? "Refactor auth"
                 : focusedItem.peek() === "Refactor auth" ? null
                 : "Buy milk";
      focusedItem.set(next);
      registerFocusScope(next);
      return true;
    }
    const r = handleCommandPaletteKey(paletteState.peek(), allCommands.get(), key);
    paletteState.set(r.state);
    if (r.action === "run" && r.command) paletteMessage.set(`ran: ${r.command.id}`);
    else if (r.action === "cancel") paletteMessage.set("cancelled");
    return r.action !== "none";
  },
  source: String.raw`// Register commands from wherever they conceptually belong.
registerGlobalCommand({ id: "quit", label: "Quit", run: () => { ... } });
useCommandScope("screen:list", [{ id: "list.new", label: "New", run: () => {...} }]);

// When focus changes, replace the focus scope:
useCommandScope("focus:item", [
  { id: "item.complete", label: 'Complete "' + item.title + '"', run: () => {...} },
]);

// The palette just reads the aggregated view:
renderCommandPalette(state.get(), allCommands.get());`,
};

// --- help overlay ---
const helpSections: HelpSection[] = [
  {
    title: "Navigation",
    bindings: [
      { key: "j / k or ↑ / ↓", desc: "move selection" },
      { key: "g / G",         desc: "top / bottom" },
      { key: "enter",         desc: "open selected" },
    ],
  },
  {
    title: "Actions",
    bindings: [
      { key: "n", desc: "new" },
      { key: "d", desc: "delete" },
      { key: "/", desc: "search" },
      { key: "ctrl+k", desc: "command palette" },
    ],
  },
];

export const helpDemo: Demo = {
  id: "help",
  category: "overlays",
  name: "help overlay",
  blurb: "A cheat sheet overlay bound to ?. Keys align visually across sections.",
  render() {
    return [helpPanel(helpSections, "help")];
  },
  handleKey() { return false; },
  source: String.raw`const sections: HelpSection[] = [
  { title: "Navigation", bindings: [{ key: "↑/↓", desc: "move" }, ...] },
  { title: "Actions",    bindings: [{ key: "n",   desc: "new" },  ...] },
];
const showHelp = signal(false);
// in overlay:
overlay: () => showHelp.get() ? helpScreen(sections) : null
// in keys:
if (key.char === "?") { showHelp.set(!showHelp.peek()); return true; }`,
};
