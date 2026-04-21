// TUI playground — a live catalog of every widget pty/tui ships.
//
// Run:  node --experimental-strip-types demos/playground/main.ts
//       or  ./demos/run playground
//
// Layout:
//   ┌─ playground ────────────────────────────────────────┐
//   │ sidebar            │  blurb                          │
//   │ (category tree)    │  (live demo)                    │
//   │                    │ ─────────                       │
//   │                    │  source snippet                 │
//   └─────────────────────────────────────────────────────┘
//   footer: ↑↓ navigate  tab switch pane  q quit

import {
  app, screen, panel, row, column, hstack, text, separator, spacer,
  signal, computed, batch,
  type ScreenContext, type UINode, type KeyEvent, type MouseEvent,
  type Screen,
  themes, helpPanel, startSpinnerTimer, stopSpinnerTimer,
  type HelpSection,
} from "../../src/tui/index.ts";
import { demos, demosByCategory } from "./registry.ts";
import {
  registerPlaygroundScopes, makeGlobalChords,
  handleMouse as routeMouse, type RouterDeps,
} from "./router.ts";
import { startStreamTicker, stopStreamTicker } from "./demos/patterns.ts";
import {
  startMetersTicker, stopMetersTicker,
  startMeterGridTicker, stopMeterGridTicker,
} from "./demos/meters.ts";
import type { Demo } from "./types.ts";

// --- global state ---

const selectedDemoId = signal<string>(demos[0].id);
const selectedPane = signal<"sidebar" | "main">("sidebar");
const showHelp = signal(false);
const showSource = signal(true);

const selectedDemo = computed<Demo>(() =>
  demos.find(d => d.id === selectedDemoId.get()) ?? demos[0]);

// Flat list of (category, demo) rows for sidebar navigation.
interface SidebarEntry {
  kind: "header" | "demo";
  label: string;
  demoId?: string;
}
const sidebarEntries = computed<SidebarEntry[]>(() => {
  const out: SidebarEntry[] = [];
  for (const group of demosByCategory()) {
    out.push({ kind: "header", label: group.category });
    for (const d of group.demos) {
      out.push({ kind: "demo", label: d.name, demoId: d.id });
    }
  }
  return out;
});

function stepDemos(delta: number): void {
  const entries = sidebarEntries.get();
  const demoOnly = entries.filter(e => e.kind === "demo");
  const cur = demoOnly.findIndex(e => e.demoId === selectedDemoId.peek());
  const next = Math.max(0, Math.min(demoOnly.length - 1, cur + delta));
  selectedDemoId.set(demoOnly[next].demoId!);
}

function selectNextDemo(delta: 1 | -1): void { stepDemos(delta); }

const routerDeps: RouterDeps = {
  selectedDemoId,
  selectedPane,
  showHelp,
  showSource,
  sidebarEntries: () => sidebarEntries.get(),
  selectedDemo: () => selectedDemo.get(),
  selectNextDemo: (d) => stepDemos(d),
  stepDemos,
};

let scopesRegistered = false;

// --- sidebar screen ---

function renderSidebar(): UINode {
  const entries = sidebarEntries.get();
  const cur = selectedDemoId.get();
  const active = selectedPane.get() === "sidebar";

  const children: UINode[] = [];
  for (const e of entries) {
    if (e.kind === "header") {
      children.push(row(text(`  ${e.label.toUpperCase()}`, "muted", { bold: true, dim: true })));
    } else {
      const isSel = e.demoId === cur;
      const marker = isSel ? "\u25b8 " : "  ";
      const color = isSel ? "accent" : "primary";
      children.push(row(text(`  ${marker}${e.label}`, color, { bold: isSel })));
    }
  }
  return panel(active ? "\u25ba demos" : "demos", children);
}

// --- main pane ---

function renderMain(ctx: ScreenContext): UINode {
  const d = selectedDemo.get();
  const active = selectedPane.get() === "main";
  const body: UINode[] = [
    row(text(`  ${d.blurb}`, "muted", { dim: true })),
    separator(),
    ...d.render({
      ...ctx,
      // Subtract sidebar width + padding so demos know they have less space.
      cols: Math.max(20, ctx.cols - 24),
    }),
  ];

  if (showSource.get()) {
    body.push(separator());
    body.push(row(text("  source", "muted", { bold: true, dim: true })));
    for (const line of d.source.split("\n")) {
      body.push(row(text("  " + line, "muted", { dim: true })));
    }
  }

  return panel(active ? `\u25ba ${d.name}` : d.name, body);
}

// --- help overlay ---

const helpSections: HelpSection[] = [
  {
    title: "Playground",
    bindings: [
      { key: "\u2191/\u2193",        desc: "move within the active pane" },
      { key: "tab / \u21e5",          desc: "switch between sidebar and main" },
      { key: "s",                     desc: "toggle source panel" },
      { key: "?",                     desc: "toggle this help" },
      { key: "q / ctrl+c",            desc: "quit" },
    ],
  },
  {
    title: "Inside a demo",
    bindings: [
      { key: "arrows / enter / chars", desc: "each demo has its own keys — try them" },
    ],
  },
];

function helpOverlayScreen(): Screen {
  return screen({
    id: "help-overlay",
    render() {
      return [helpPanel(helpSections, "playground help")];
    },
    handleKey(key) {
      if (key.char === "?" || key.name === "escape") {
        showHelp.set(false);
        return true;
      }
      return true;
    },
  });
}

// --- root screen ---

function rootScreen(): Screen {
  return screen({
    id: "playground",
    render(ctx) {
      const inSidebar = selectedPane.get() === "sidebar";
      return [
        hstack({ gap: 1 }, [
          column({ width: 24 }, [renderSidebar()]),
          column({ flex: true }, [renderMain(ctx)]),
        ]),
        row(text(
          inSidebar
            ? "  \u2191\u2193 nav   \u2192/tab/\u23ce focus demo   ctrl+l focus demo   s source   ? help   q quit"
            : "  keys go to the demo   ctrl+h back to list   ctrl+c quit",
          "muted", { dim: true },
        )),
      ];
    },
    handleMouse(event) { return routeMouse(routerDeps, event); },
    handleKey(key, ctx) {
      // Scopes are registered on first key — ctx.focus isn't available
      // until app() is running. One-time guard so re-renders don't
      // pile up scopes.
      if (!scopesRegistered) {
        registerPlaygroundScopes(ctx.focus, routerDeps);
        scopesRegistered = true;
      }
      return ctx.focus.dispatchKey(key, ctx);
    },
  });
}

// --- lifecycle ---

startSpinnerTimer();
startStreamTicker();
startMetersTicker();
startMeterGridTicker();

const instance = app({
  screen: rootScreen,
  overlay: () => (showHelp.get() ? helpOverlayScreen() : null),
  theme: () => themes.coolBlue,
  boxStyle: () => "rounded",
  mouse: true,
  onKey: makeGlobalChords(routerDeps),
});

process.on("exit", () => {
  stopSpinnerTimer();
  stopStreamTicker();
  stopMetersTicker();
  stopMeterGridTicker();
});

instance.start();
