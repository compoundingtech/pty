// Meters: sparkline + bar chart + panel footer-title.
// Everything you need to rebuild a mactop-style UI.

import {
  text, row, column, panel, separator, hstack, progressBar,
  signal,
  sparkline, sparklineString, barChart,
} from "../../../src/tui/index.ts";
import type { Demo } from "../types.ts";

// --- sparkline ---
const cpuSeries = signal<number[]>(
  Array.from({ length: 60 }, (_, i) => 30 + 20 * Math.sin(i / 3) + Math.random() * 5),
);
const memSeries = signal<number[]>(
  Array.from({ length: 60 }, (_, i) => 60 + 10 * Math.cos(i / 5) + Math.random() * 2),
);

let tickerId: ReturnType<typeof setInterval> | null = null;
export function startMetersTicker(): void {
  if (tickerId) return;
  tickerId = setInterval(() => {
    const c = cpuSeries.peek();
    const m = memSeries.peek();
    cpuSeries.set([...c.slice(1), 20 + Math.random() * 60]);
    memSeries.set([...m.slice(1), 55 + Math.random() * 15]);
  }, 500);
}
export function stopMetersTicker(): void {
  if (tickerId) { clearInterval(tickerId); tickerId = null; }
}

export const sparklineDemo: Demo = {
  id: "sparkline",
  category: "data",
  name: "sparkline",
  blurb: "Compact inline chart. One cell per sample, height via unicode blocks. Updates live.",
  render() {
    const cpu = cpuSeries.get();
    const mem = memSeries.get();
    return [
      row(
        text("  CPU  ", "muted"),
        sparkline(cpu, { width: 40, min: 0, max: 100, color: "accent" }),
        text(`  ${cpu[cpu.length - 1].toFixed(0)}%`, "primary"),
      ),
      row(
        text("  MEM  ", "muted"),
        sparkline(mem, { width: 40, min: 0, max: 100, color: "info" }),
        text(`  ${mem[mem.length - 1].toFixed(0)}%`, "primary"),
      ),
      separator(),
      row(text("  stacked in a panel with bottom caption:", "muted", { dim: true })),
      panel("live CPU", [
        row(sparkline(cpu, { width: 50, min: 0, max: 100, color: "accent" })),
      ], { footerTitle: "last 60 samples · 2Hz" }),
    ];
  },
  handleKey() { return false; },
  source: String.raw`const series = signal<number[]>([]);
// push new samples as they arrive:
series.set([...series.peek().slice(1), latestValue]);

// render:
sparkline(series.get(), { width: 40, min: 0, max: 100, color: "accent" })

// or get the string directly:
const glyphs = sparklineString(series.get(), { width: 40 });`,
};

// --- bar chart ---
const cores = signal<number[]>(Array.from({ length: 10 }, () => Math.random() * 100));

export const barChartDemo: Demo = {
  id: "bar-chart",
  category: "data",
  name: "bar chart",
  blurb: "Vertical histogram — CPU per core, power by component, any small-cardinality metric.",
  render() {
    const values = cores.get();
    const items = values.map((v, i) => ({
      label: String(i),
      value: v,
      color: v > 70 ? ("warn" as const) : v > 90 ? ("error" as const) : ("accent" as const),
    }));
    return [
      row(text("  CPU per core (0-9), height = 6 rows:", "muted", { dim: true })),
      barChart(items, { height: 6, min: 0, max: 100, barWidth: 3, gap: 1, showLabels: true }),
      separator(),
      row(text("  thinner bars, no labels:", "muted", { dim: true })),
      barChart(items, { height: 4, min: 0, max: 100, barWidth: 1, gap: 1 }),
    ];
  },
  handleKey(key) {
    if (key.char === "r") {
      cores.set(Array.from({ length: 10 }, () => Math.random() * 100));
      return true;
    }
    return false;
  },
  source: String.raw`const items: BarChartItem[] = [
  { label: "0", value: 23, color: "accent" },
  { label: "1", value: 87, color: "warn" },
  ...
];
// render:
barChart(items, { height: 6, min: 0, max: 100, barWidth: 3, showLabels: true })`,
};

// --- panel footer-title / kitchen-style meter grid (mactop-ish) ---
const meterCpu = signal<number>(42);
const meterMem = signal<number>(68);
const meterGpu = signal<number>(15);

let meterId: ReturnType<typeof setInterval> | null = null;
export function startMeterGridTicker(): void {
  if (meterId) return;
  meterId = setInterval(() => {
    meterCpu.set(Math.max(0, Math.min(100, meterCpu.peek() + (Math.random() - 0.5) * 10)));
    meterMem.set(Math.max(0, Math.min(100, meterMem.peek() + (Math.random() - 0.5) * 4)));
    meterGpu.set(Math.max(0, Math.min(100, meterGpu.peek() + (Math.random() - 0.5) * 20)));
  }, 400);
}
export function stopMeterGridTicker(): void {
  if (meterId) { clearInterval(meterId); meterId = null; }
}

export const meterGridDemo: Demo = {
  id: "meter-grid",
  category: "patterns",
  name: "mactop-style meter grid",
  blurb: "Grid of panels with progress bars + sparklines + a bottom-border caption. Basically the shape of mactop or htop.",
  render() {
    const cpu = meterCpu.get();
    const mem = meterMem.get();
    const gpu = meterGpu.get();
    return [
      panel("system", [
        hstack({ gap: 1 }, [
          column({ flex: true }, [
            panel("CPU", [
              row(text(`  ${cpu.toFixed(0)}%  `, "primary"), progressBar(cpu / 100, 20, "accent")),
              row(sparkline(cpuSeries.get(), { width: 26, min: 0, max: 100, color: "accent" })),
            ]),
            panel("Memory", [
              row(text(`  ${mem.toFixed(0)}%  `, "primary"), progressBar(mem / 100, 20, "info")),
              row(sparkline(memSeries.get(), { width: 26, min: 0, max: 100, color: "info" })),
            ]),
          ]),
          column({ flex: true }, [
            panel("GPU", [
              row(text(`  ${gpu.toFixed(0)}%  `, "primary"), progressBar(gpu / 100, 20, "warn")),
            ]),
            panel("per-core", [
              barChart(
                cores.get().map((v, i) => ({ label: String(i), value: v,
                  color: v > 80 ? ("warn" as const) : ("accent" as const),
                })),
                { height: 4, min: 0, max: 100, barWidth: 2, gap: 1 },
              ),
            ]),
          ]),
        ]),
      ], { footerTitle: "4/17 layout (skyblue) · -/+ 400ms · live" }),
    ];
  },
  handleKey() { return false; },
  source: String.raw`panel("system", [
  hstack({ gap: 1 }, [
    column({ flex: true }, [
      panel("CPU", [row(progressBar(cpu/100, 20)), row(sparkline(series))]),
      panel("Memory", [/* ... */]),
    ]),
    column({ flex: true }, [
      panel("GPU", [/* ... */]),
      panel("per-core", [barChart(cores, { height: 4 })]),
    ]),
  ]),
], { footerTitle: "4/17 layout (skyblue) · -/+ 400ms" })`,
};
