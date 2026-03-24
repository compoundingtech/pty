// Confirm delete overlay
import {
  overlay, text, separator,
  type UINode, type ScreenContext,
} from "../../../src/tui/index.ts";
import { selectedReminder } from "../state.ts";

export const confirmDeleteOverlay = overlay({
  id: "confirm-delete",
  title: "Confirm Delete",
  width: (cols) => Math.min(50, cols - 8),
  height: 8,
  render(_ctx: ScreenContext): UINode[] {
    const r = selectedReminder.get();
    const name = r?.title ?? "this reminder";
    return [
      text(`Delete "${name}"?`, "warn", { bold: true }),
      separator(),
      text("y = yes, n = no", "muted"),
    ];
  },
});
