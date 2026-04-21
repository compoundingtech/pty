// Toast / notification queue. Ephemeral banners that auto-dismiss after
// a timeout. Safe for non-blocking feedback ("saved", "sync failed") —
// not a replacement for confirm modals or dialogs.
//
// Queue is pure: `pushToast` / `pruneExpired` return new queues, never
// mutate. The consumer owns a `signal<ToastQueue>` and an effect that
// schedules `pruneExpired` via setInterval when the queue is non-empty.

import { column, row, text } from "../builders.ts";
import type { UINode, ColumnNode, Color } from "../nodes.ts";

export type ToastKind = "info" | "success" | "warn" | "error";

export interface Toast {
  id: string;
  kind: ToastKind;
  text: string;
  /** Epoch ms when this toast should be dropped. */
  expiresAt: number;
}

export interface ToastQueue {
  toasts: Toast[];
}

let nextToastId = 0;
function genToastId(): string {
  nextToastId += 1;
  return `toast-${nextToastId}`;
}

export function createToastQueue(): ToastQueue {
  return { toasts: [] };
}

export interface PushToastOptions {
  kind?: ToastKind;
  /** Milliseconds the toast is visible. Default 3000. */
  durationMs?: number;
  now?: number;
}

export function pushToast(
  queue: ToastQueue,
  message: string,
  opts: PushToastOptions = {},
): ToastQueue {
  const now = opts.now ?? Date.now();
  const toast: Toast = {
    id: genToastId(),
    kind: opts.kind ?? "info",
    text: message,
    expiresAt: now + (opts.durationMs ?? 3000),
  };
  return { toasts: [...queue.toasts, toast] };
}

/** Drop toasts whose `expiresAt` has passed. Call from a setInterval. */
export function pruneExpired(queue: ToastQueue, now?: number): ToastQueue {
  const t = now ?? Date.now();
  const kept = queue.toasts.filter(x => x.expiresAt > t);
  if (kept.length === queue.toasts.length) return queue;
  return { toasts: kept };
}

export function dismissToast(queue: ToastQueue, id: string): ToastQueue {
  return { toasts: queue.toasts.filter(t => t.id !== id) };
}

function kindColor(kind: ToastKind): Color {
  switch (kind) {
    case "success": return "ok";
    case "warn":    return "warn";
    case "error":   return "error";
    default:        return "accent";
  }
}

function kindGlyph(kind: ToastKind): string {
  switch (kind) {
    case "success": return "\u2713"; // check
    case "warn":    return "\u26a0"; // warning sign
    case "error":   return "\u2717"; // ballot x
    default:        return "\u25cf"; // bullet
  }
}

/** Render the queue as a column of rows. Consumers typically overlay this
 *  in a corner of their app. */
export function renderToasts(queue: ToastQueue): UINode {
  if (queue.toasts.length === 0) {
    const empty: ColumnNode = { type: "column", children: [] };
    return empty;
  }
  const children: UINode[] = queue.toasts.map(t =>
    row(
      text(` ${kindGlyph(t.kind)} `, kindColor(t.kind), { bold: true }),
      text(t.text, "primary"),
    ),
  );
  const node: ColumnNode = { type: "column", children };
  return node;
}
