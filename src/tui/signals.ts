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

export function computed<T>(fn: () => T): { get(): T } {
  const c = preactComputed(fn);
  return {
    get() { return c.value; },
  };
}

export { preactEffect as effect, preactBatch as batch };
