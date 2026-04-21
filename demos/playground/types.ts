// Shape every playground demo exports.
//
// A demo is a bite-sized interactive example: its own state, its own
// render + key handler, and a source snippet shown under the live view.
// The main app routes input to whichever demo is currently selected.

import type { UINode, ScreenContext, KeyEvent } from "../../src/tui/index.ts";

export interface Demo {
  /** Stable id — used as the tree node key in the sidebar. */
  id: string;
  /** Sidebar category label. */
  category: string;
  /** Display name inside the category. */
  name: string;
  /** One-line blurb shown above the live area. */
  blurb: string;
  /** Source snippet printed under the live view. */
  source: string;
  /** Render the live view. Return an array of nodes for the main pane. */
  render(ctx: ScreenContext): UINode[];
  /** Handle a key. Return true if consumed. */
  handleKey(key: KeyEvent, ctx: ScreenContext): boolean;
}
