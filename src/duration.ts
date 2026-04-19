/**
 * Duration utilities shared between the CLI and tests.
 *
 * `parseDuration` accepts compact `Ns|Nm|Nh|Nd` strings (e.g. `30s`, `5m`,
 * `2h`, `7d`) and returns milliseconds. It is intentionally strict — no
 * compound forms like `1h30m` — so the grammar stays trivial to document in
 * `pty --help` and unambiguous in scripts.
 *
 * `formatDuration` renders a millisecond value back into a compact string
 * (`45s`, `2h12m`, `3d2h`) for display.
 */

export function parseDuration(input: string): number | null {
  const m = /^(\d+)\s*(s|m|h|d)$/i.exec(input.trim());
  if (!m) return null;
  const n = parseInt(m[1], 10);
  if (!Number.isFinite(n) || n < 0) return null;
  const unitMs: Record<string, number> = {
    s: 1000,
    m: 60 * 1000,
    h: 60 * 60 * 1000,
    d: 24 * 60 * 60 * 1000,
  };
  return n * unitMs[m[2].toLowerCase()];
}

export function formatDuration(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    const s = seconds % 60;
    return s === 0 ? `${minutes}m` : `${minutes}m${s}s`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    const m = minutes % 60;
    return m === 0 ? `${hours}h` : `${hours}h${m}m`;
  }
  const days = Math.floor(hours / 24);
  const h = hours % 24;
  return h === 0 ? `${days}d` : `${days}d${h}h`;
}
