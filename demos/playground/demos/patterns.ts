// Patterns: tabs, markdown, stream-view.

import {
  text, row, column, panel, separator,
  signal,
  createTabsState, nextTab, prevTab, handleTabsKey, renderTabs,
  renderMarkdown,
  createStreamView, streamScrollUp, streamScrollDown, streamPin,
  handleStreamKey, renderStreamView, streamIsPinned,
  type TabsState, type TabDef, type StreamViewState,
} from "../../../src/tui/index.ts";
import type { Demo } from "../types.ts";

// --- tabs ---
const tabs: TabDef[] = [
  { id: "inbox",  label: "Inbox"   },
  { id: "sent",   label: "Sent"    },
  { id: "drafts", label: "Drafts"  },
  { id: "trash",  label: "Trash"   },
];
const tabsState = signal<TabsState>(createTabsState(tabs));

const tabContent: Record<string, string> = {
  inbox:  "  32 unread messages",
  sent:   "  145 messages",
  drafts: "  2 drafts",
  trash:  "  empty",
};

export const tabsDemo: Demo = {
  id: "tabs",
  category: "patterns",
  name: "tabs",
  blurb: "ctrl+tab / ctrl+shift+tab cycle. Numbers 1-9 jump directly.",
  render() {
    return [
      renderTabs(tabsState.get(), tabs),
      separator(),
      row(text(tabContent[tabsState.get().activeId ?? ""] ?? "", "primary")),
    ];
  },
  handleKey(key) {
    const next = handleTabsKey(tabsState.peek(), tabs, key);
    if (next) { tabsState.set(next); return true; }
    return false;
  },
  source: String.raw`const tabs = [
  { id: "inbox", label: "Inbox" },
  { id: "sent",  label: "Sent"  },
];
const state = signal(createTabsState(tabs));
// render:
renderTabs(state.get(), tabs)
// keys:
const next = handleTabsKey(state.peek(), tabs, key);
if (next) state.set(next);`,
};

// --- markdown ---
const markdownSource = `# Markdown renderer

Renders a subset of **CommonMark** straight to UINodes. Supports:

- Headings (\`#\`, \`##\`, ...)
- Inline **bold**, *italic*, \`code\`, [links](https://example.com)
- Fenced code blocks
- Task lists
- Blockquotes
- Horizontal rules

> "Talk is cheap. Show me the code."
> — Linus Torvalds

- [ ] plumb TUI framework
- [x] ship first app
- [ ] polish the playground

\`\`\`
const out = renderMarkdown(src, { width: 80 });
for (const node of out) /* render node */;
\`\`\`

---

Paragraphs wrap at the width you pass. Omit \`width\` to keep them as-is.`;

export const markdownDemo: Demo = {
  id: "markdown",
  category: "patterns",
  name: "markdown renderer",
  blurb: "Subset of CommonMark → UINodes. Suitable for notes, emails, articles, chat.",
  render(ctx) {
    return renderMarkdown(markdownSource, { width: Math.max(30, ctx.cols - 20) });
  },
  handleKey() { return false; },
  source: String.raw`const src = \`# Hello
Some **text** with \`inline\` and a [link](https://x).
- [ ] task
\`;
renderMarkdown(src, { width: 80 });  // returns UINode[]`,
};

// --- stream view ---
const streamItems = signal<string[]>([
  "10:00  system: connected",
  "10:01  ava: hi there",
  "10:01  me: hey",
  "10:02  ava: how's the playground going?",
  "10:03  me: tabs work, stream-view up next",
  "10:04  ava: show me",
]);
const streamState = signal<StreamViewState>(createStreamView());

let streamTimer: ReturnType<typeof setInterval> | null = null;
export function startStreamTicker(): void {
  if (streamTimer) return;
  const speakers = ["ava", "me", "ben", "claude"];
  const phrases = ["cool", "what about this", "let me try", "ok works", "great", "next idea?", "hmm", "got it"];
  streamTimer = setInterval(() => {
    const speaker = speakers[Math.floor(Math.random() * speakers.length)];
    const phrase = phrases[Math.floor(Math.random() * phrases.length)];
    const now = new Date();
    const stamp = `${now.getHours().toString().padStart(2, "0")}:${now.getMinutes().toString().padStart(2, "0")}`;
    streamItems.set([...streamItems.peek(), `${stamp}  ${speaker}: ${phrase}`]);
  }, 1500);
}
export function stopStreamTicker(): void {
  if (streamTimer) { clearInterval(streamTimer); streamTimer = null; }
}

export const streamViewDemo: Demo = {
  id: "stream-view",
  category: "patterns",
  name: "sticky-bottom stream",
  blurb: "Auto-scrolls to new messages. Arrow up scrolls back. End re-pins to bottom.",
  render() {
    const items = streamItems.get();
    const pinned = streamIsPinned(streamState.get());
    const viewport = 10;
    return [
      panel(`stream  (${pinned ? "pinned" : "scrolled back"} · ${items.length} messages)`, [
        renderStreamView(items, streamState.get(), viewport, (it) =>
          row(text(it, it.includes("me: ") ? "accent" : "primary"))),
      ]),
    ];
  },
  handleKey(key) {
    const items = streamItems.peek();
    const next = handleStreamKey(streamState.peek(), key, items.length, 10);
    if (next) { streamState.set(next); return true; }
    return false;
  },
  source: String.raw`const items = signal<string[]>([]);
const state = signal(createStreamView());
// render:
panel("chat", [
  renderStreamView(items.get(), state.get(), viewport, (it) => row(text(it))),
])
// keys:
const next = handleStreamKey(state.peek(), key, items.peek().length, viewport);
if (next) state.set(next);
// new messages arrive:
items.set([...items.peek(), newMessage]);  // auto-follows if pinned`,
};
