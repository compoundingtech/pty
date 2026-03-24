// FPS counter: tracks frame timestamps and computes rolling FPS

import { signal } from "./signals.ts";

const showFPS = signal(false);

const frameTimes: number[] = [];
const FPS_WINDOW = 60;
let currentFPS = 0;

export function recordFrame(): void {
  const now = performance.now();
  frameTimes.push(now);
  while (frameTimes.length > FPS_WINDOW) frameTimes.shift();

  if (frameTimes.length >= 2) {
    const elapsed = now - frameTimes[0]!;
    if (elapsed > 0) {
      currentFPS = Math.round(((frameTimes.length - 1) / elapsed) * 1000);
    }
  }
}

export function getCurrentFPS(): number {
  return currentFPS;
}

export function isFPSVisible(): boolean {
  return showFPS.get();
}

export function toggleFPS(): void {
  showFPS.set(!showFPS.peek());
}
