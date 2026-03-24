// Spinner animation: reference-counted timer driving a spinnerFrame signal

import { signal, computed } from "./signals.ts";

const SPINNER_CHARS = "\u280b\u2819\u2839\u2838\u283c\u2834\u2826\u2827\u2807\u280f";
const SPINNER_INTERVAL = 80;

const spinnerFrame = signal(0);
let spinnerTimer: ReturnType<typeof setInterval> | null = null;
let spinnerRefCount = 0;

export const spinnerChar = computed(() => {
  const frame = spinnerFrame.get();
  return SPINNER_CHARS[frame % SPINNER_CHARS.length]!;
});

export function startSpinnerTimer(): void {
  spinnerRefCount++;
  if (spinnerTimer === null) {
    spinnerTimer = setInterval(() => {
      spinnerFrame.set(spinnerFrame.peek() + 1);
    }, SPINNER_INTERVAL);
  }
}

export function stopSpinnerTimer(): void {
  spinnerRefCount = Math.max(0, spinnerRefCount - 1);
  if (spinnerRefCount === 0 && spinnerTimer !== null) {
    clearInterval(spinnerTimer);
    spinnerTimer = null;
  }
}

export function isSpinnerRunning(): boolean {
  return spinnerTimer !== null;
}
