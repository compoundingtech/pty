// Reactive signals — wraps @preact/signals-core with .get()/.set() API
import {
  signal as preactSignal,
  computed as preactComputed,
  effect as preactEffect,
  batch as preactBatch,
  type Signal as PreactSignal,
  type ReadonlySignal as PreactReadonlySignal,
} from "@preact/signals-core";

export interface Signal<T> {
  get(): T;
  set(value: T): void;
  peek(): T;
}

export function signal<T>(initial: T): Signal<T> {
  const s = preactSignal(initial);
  return {
    get() { return s.value; },
    set(value: T) { s.value = value; },
    peek() { return s.peek(); },
  };
}

export interface Computed<T> {
  /** Read and subscribe (inside an effect) to this computed's value. */
  get(): T;
  /** Read the current value without subscribing. Safe to call from
   *  non-reactive contexts. */
  peek(): T;
}

export function computed<T>(fn: () => T): Computed<T> {
  const c = preactComputed(fn);
  return {
    get() { return c.value; },
    peek() { return c.peek(); },
  };
}

export { preactEffect as effect, preactBatch as batch };

/** Create a signal that debounces its own change notifications. Writers
 *  `bump()` as often as they like; the underlying preact signal receives
 *  at most one `.set(v+1)` per `setImmediate`-scheduled tick. Effects that
 *  depend on it run at most once per tick regardless of bump rate.
 *
 *  Use for high-volume producers that would otherwise saturate the
 *  reactive graph (e.g., a log store fed by a firehose). Consumers read
 *  the current state from whatever mutable structure the producer owns;
 *  the signal itself is purely a change notifier.
 *
 *  Returns:
 *  - `get()` — subscribe to change events (returns the monotonic tick
 *    counter; consumers don't need the value itself — they re-read the
 *    producer's source-of-truth data on each update).
 *  - `peek()` — read the current tick without subscribing.
 *  - `bump()` — mark a change; may schedule a tick if one isn't already
 *    pending. Does nothing extra if a tick is in flight.
 *  - `flush()` — force the pending tick to fire synchronously (useful
 *    in tests so `await flush()` yields a consistent observable state).
 */
export interface DebouncedSignal {
  get(): number;
  peek(): number;
  bump(): void;
  flush(): void;
}

export function debouncedSignal(): DebouncedSignal {
  const s = preactSignal(0);
  let pending = false;
  let pendingTicks = 0;
  let scheduled: ReturnType<typeof setImmediate> | null = null;

  const flushNow = (): void => {
    if (!pending) return;
    pending = false;
    if (scheduled) { clearImmediate(scheduled); scheduled = null; }
    s.value = s.peek() + pendingTicks;
    pendingTicks = 0;
  };

  return {
    get() { return s.value; },
    peek() { return s.peek(); },
    bump() {
      pendingTicks++;
      if (pending) return;
      pending = true;
      scheduled = setImmediate(flushNow);
    },
    flush: flushNow,
  };
}
