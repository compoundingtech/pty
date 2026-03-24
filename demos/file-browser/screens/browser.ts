// Two-pane file browser screen
import {
  screen, text, column, hstack, panel, selectable, scrollable,
  statusBar, footer,
  type KeyEvent, type ScreenContext, type UINode,
} from "../../../src/tui/index.ts";
import { updateScrollRegion } from "../../../src/tui/index.ts";
import {
  flatList, selectedIndex, treeScroll, focusPane,
  previewContent, previewScroll, previewIsBinary, previewFileName, selectedNode,
  moveUp, moveDown, expandOrOpen, collapseDir, switchPane,
  scrollPreviewUp, scrollPreviewDown, cycleTheme,
  type TreeNode,
} from "../state.ts";
import {
  buildPreviewNodes, previewContentWidth, markdownHighlight, isMarkdown,
} from "../preview.ts";

function treeIcon(node: TreeNode): string {
  if (node.type === "dir") return node.expanded ? "\u25be" : "\u25b8";
  return " ";
}

function renderTreeRow(node: TreeNode, _index: number, selected: boolean): UINode[] {
  const ind = "  ".repeat(node.depth);
  const isFocused = selected && focusPane.get() === "tree";
  const sel = isFocused ? "> " : "  ";
  const nameColor = isFocused ? "accent" : (node.type === "dir" ? "accent" : "primary");
  return [text(sel + ind + treeIcon(node) + " " + node.name, nameColor, { bold: isFocused, truncate: true })];
}

export const browserScreen = screen({
  id: "browser",

  render(ctx: ScreenContext): UINode[] {
    const list = flatList.get();
    const sel = selectedIndex.get();
    const preview = previewContent.get();
    const pScroll = previewScroll.get();
    const node = selectedNode.get();
    const binary = previewIsBinary.get();
    const fileName = previewFileName.get();
    const highlighter = isMarkdown(fileName) ? markdownHighlight : undefined;

    const treeViewport = Math.max(1, ctx.rows - 4);
    const region = updateScrollRegion(
      { ...treeScroll.get(), selectedIndex: sel, totalItems: list.length },
      list.length,
      treeViewport,
    );

    const rightInfo = node
      ? `${node.type === "dir" ? "dir" : "file"}: ${node.name}`
      : "";

    const previewViewport = Math.max(1, ctx.rows - 4);
    const visiblePreview = preview.slice(pScroll, pScroll + previewViewport);
    const treeWidth = Math.max(25, Math.floor(ctx.cols * 0.35));

    return [
      statusBar("File Browser", rightInfo),
      hstack({ gap: 0 }, [
        column({ width: treeWidth }, [
          panel("Files", [
            selectable(region, list, renderTreeRow),
          ]),
        ]),
        column({ flex: true }, [
          panel("Preview", [
            scrollable(
              binary
                ? ["(binary file)"]
                : visiblePreview.length > 0
                  ? buildPreviewNodes(visiblePreview, pScroll, previewContentWidth(ctx.cols, treeWidth), highlighter)
                  : ["(no preview)"],
              (item) => typeof item === "string" ? [text(item, "muted")] : [item],
            ),
          ]),
        ]),
      ]),
      footer("\u2191\u2193 nav  \u2190\u2192 expand/collapse  tab pane  T theme  q quit"),
    ];
  },

  handleKey(key: KeyEvent, ctx: ScreenContext): boolean {
    if (key.char === "q" || (key.name === "c" && key.ctrl)) return false;
    if (key.char === "T") { cycleTheme(); return true; }
    if (key.name === "tab") { switchPane(); return true; }

    if (focusPane.peek() === "tree") {
      if (key.name === "up") { moveUp(); return true; }
      if (key.name === "down") { moveDown(); return true; }
      if (key.name === "right" || key.name === "return") { expandOrOpen(); return true; }
      if (key.name === "left") { collapseDir(); return true; }
    } else {
      if (key.name === "up") { scrollPreviewUp(); return true; }
      if (key.name === "down") { scrollPreviewDown(); return true; }
    }

    return true;
  },
});
