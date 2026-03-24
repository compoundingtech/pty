// Dashboard screen: agent tree + detail + activity log
import {
  screen, text, row, column, hstack, panel, selectable, scrollable, canvas,
  statusBar, footer, spinner, dot, progressBar, separator,
  type KeyEvent, type ScreenContext, type UINode,
} from "../../../src/tui/index.ts";
import { updateScrollRegion } from "../../../src/tui/index.ts";
import {
  flatAgents, selectedIndex, agentScroll, selectedAgent, agentCount,
  activityLog, elapsedTime, paused, formatElapsed,
  moveUp, moveDown, togglePause, cycleTheme,
  type AgentNode,
} from "../state.ts";

function statusColor(status: AgentNode["status"]): "ok" | "accent" | "error" | "warn" | "muted" {
  switch (status) {
    case "working": return "accent";
    case "done": return "ok";
    case "blocked": return "error";
    case "error": return "warn";
    default: return "muted";
  }
}

function statusIcon(status: AgentNode["status"]): UINode {
  switch (status) {
    case "working": return spinner("accent");
    case "done": return dot(true, "ok");
    case "blocked": return dot(true, "error");
    case "error": return dot(true, "warn");
    default: return dot(false, "muted");
  }
}

function renderAgentRow(agent: AgentNode, _index: number, selected: boolean): UINode[] {
  const prefix = "  ".repeat(agent.depth);
  return [
    text(prefix, "primary"),
    statusIcon(agent.status),
    text(" " + agent.name, selected ? "primary" : "secondary", { bold: selected, truncate: true }),
    text("  " + agent.status, statusColor(agent.status)),
  ];
}

function renderDetail(agent: AgentNode | null, maxHeight: number): UINode[] {
  if (!agent) return [text("(no agent selected)", "muted")];

  const nodes: UINode[] = [
    row(text(agent.name, "accent", { bold: true }), text("  "), dot(agent.status === "working", statusColor(agent.status)), text(" " + agent.status, statusColor(agent.status))),
  ];

  if (agent.task) {
    nodes.push(text("Task: " + agent.task, "primary", { truncate: true }));
  }

  if (agent.progress > 0) {
    nodes.push(row(text("Progress: ", "muted"), progressBar(agent.progress, { color: statusColor(agent.status) })));
  }

  if (agent.started) {
    nodes.push(text("Started: " + agent.started, "muted", { truncate: true }));
  }

  if (agent.body) {
    nodes.push(separator());
    const bodyLines = agent.body.split("\n").slice(0, Math.max(1, maxHeight - 8));
    for (const line of bodyLines) {
      nodes.push(text(line, "secondary", { truncate: true }));
    }
  }

  if (agent.plan) {
    nodes.push(separator());
    nodes.push(text("Plan:", "accent", { bold: true }));
    const planLines = agent.plan.split("\n").slice(0, 5);
    for (const line of planLines) {
      nodes.push(text(line, "secondary", { truncate: true }));
    }
  }

  if (agent.output) {
    nodes.push(separator());
    nodes.push(text("Output:", "accent", { bold: true }));
    const outputLines = agent.output.split("\n").slice(0, 5);
    for (const line of outputLines) {
      nodes.push(text(line, "secondary", { truncate: true }));
    }
  }

  return nodes;
}

function renderActivity(maxLines: number): UINode[] {
  const log = activityLog.get();
  const visible = log.slice(-maxLines);
  if (visible.length === 0) return [text("(no activity yet)", "muted")];
  return visible.map(entry => {
    const isMessage = entry.agent.includes("\u2192");
    const color = isMessage ? "info" : "secondary";
    return text(`[${entry.timestamp}] ${entry.agent}: ${entry.event}`, color, { truncate: true });
  });
}

export const dashboardScreen = screen({
  id: "dashboard",

  render(ctx: ScreenContext): UINode[] {
    const agents = flatAgents.get();
    const sel = selectedIndex.get();
    const agent = selectedAgent.get();
    const elapsed = elapsedTime.get();
    const isPaused = paused.get();
    const count = agentCount.get();

    const viewport = Math.max(1, ctx.rows - 12); // leave room for activity + chrome
    const region = updateScrollRegion(
      { ...agentScroll.get(), selectedIndex: sel, totalItems: agents.length },
      agents.length,
      viewport,
    );

    const treeWidth = Math.max(30, Math.floor(ctx.cols * 0.35));
    const activityHeight = Math.min(8, Math.max(3, Math.floor(ctx.rows * 0.2)));

    const pauseLabel = isPaused ? " PAUSED" : "";

    return [
      statusBar("Agent Teams", `Elapsed: ${formatElapsed(elapsed)} \u2502 ${count} agents${pauseLabel}`),
      hstack({ gap: 0 }, [
        column({ width: treeWidth }, [
          panel("Agents", [
            selectable(region, agents, renderAgentRow),
          ]),
        ]),
        column({ flex: true }, [
          panel("Detail", [
            ...renderDetail(agent, ctx.rows - activityHeight - 4),
            canvas(() => {}, {}), // flex spacer to fill remaining height
          ]),
        ]),
      ]),
      panel("Activity", [
        ...renderActivity(activityHeight - 2),
        canvas(() => {}, {}), // flex spacer
      ]),
      footer("\u2191\u2193 select  p pause  T theme  q quit"),
    ];
  },

  handleKey(key: KeyEvent, ctx: ScreenContext): boolean {
    if (key.char === "q" || (key.name === "c" && key.ctrl)) return false;
    if (key.char === "T") { cycleTheme(); return true; }
    if (key.char === "p") { togglePause(); return true; }
    if (key.name === "up") { moveUp(); return true; }
    if (key.name === "down") { moveDown(); return true; }
    return true;
  },
});
