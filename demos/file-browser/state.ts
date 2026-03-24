// File browser state: tree structure, signals, directory loading, preview
import * as fs from "node:fs";
import * as path from "node:path";
import {
  signal, computed, batch,
  createScrollRegion, updateScrollRegion, scrollUp, scrollDown,
  themes,
  type ScrollRegion, type Theme, type BoxStyle,
} from "../../src/tui/index.ts";

// --- Tree node ---
export interface TreeNode {
  name: string;
  path: string;
  type: "file" | "dir";
  expanded: boolean;
  children: TreeNode[] | null;
  depth: number;
}

// --- Signals ---
export const rootPath = signal(process.argv[2] || process.cwd());
export const tree = signal<TreeNode[]>([]);
export const selectedIndex = signal(0);
export const treeScroll = signal<ScrollRegion>(createScrollRegion(0, 20));
export const focusPane = signal<"tree" | "preview">("tree");
export const previewContent = signal<string[]>([]);
export const previewScroll = signal(0);
export const previewIsBinary = signal(false);
export const previewFileName = signal("");

const SKIP_DIRS = new Set([".git", "node_modules", ".DS_Store", ".hg", ".svn", "__pycache__", ".cache"]);
const MAX_DEPTH = 5;

// --- Theme ---
const themeNames = Object.keys(themes);
export const themeIndex = signal(0);
export const currentTheme = computed<Theme>(() => themes[themeNames[themeIndex.get()]] as Theme);
export const boxStyle = signal<BoxStyle>("rounded");

export function cycleTheme(): void {
  themeIndex.set((themeIndex.peek() + 1) % themeNames.length);
}

// --- Flatten tree ---
export const flatList = computed<TreeNode[]>(() => {
  const result: TreeNode[] = [];
  function walk(nodes: TreeNode[]): void {
    for (const node of nodes) {
      result.push(node);
      if (node.type === "dir" && node.expanded && node.children) {
        walk(node.children);
      }
    }
  }
  walk(tree.get());
  return result;
});

// --- Selected node ---
export const selectedNode = computed<TreeNode | null>(() => {
  const list = flatList.get();
  const idx = selectedIndex.get();
  return list[idx] ?? null;
});

// --- Load directory ---
export function loadDirectory(dirPath: string, depth: number): TreeNode[] {
  if (depth > MAX_DEPTH) return [];
  let entries: fs.Dirent[];
  try {
    entries = fs.readdirSync(dirPath, { withFileTypes: true });
  } catch {
    return [];
  }

  const dirs: TreeNode[] = [];
  const files: TreeNode[] = [];

  for (const entry of entries) {
    if (SKIP_DIRS.has(entry.name)) continue;
    if (entry.name.startsWith(".") && entry.name !== ".") continue;

    const fullPath = path.join(dirPath, entry.name);
    if (entry.isDirectory()) {
      dirs.push({
        name: entry.name,
        path: fullPath,
        type: "dir",
        expanded: false,
        children: null,
        depth,
      });
    } else if (entry.isFile()) {
      files.push({
        name: entry.name,
        path: fullPath,
        type: "file",
        expanded: false,
        children: null,
        depth,
      });
    }
  }

  // Sort: dirs first (alphabetical), then files (alphabetical)
  dirs.sort((a, b) => a.name.localeCompare(b.name));
  files.sort((a, b) => a.name.localeCompare(b.name));
  return [...dirs, ...files];
}

// --- Binary detection ---
export function isBinary(filePath: string): boolean {
  try {
    const fd = fs.openSync(filePath, "r");
    const buf = Buffer.alloc(512);
    const bytesRead = fs.readSync(fd, buf, 0, 512, 0);
    fs.closeSync(fd);
    for (let i = 0; i < bytesRead; i++) {
      if (buf[i] === 0) return true;
    }
    return false;
  } catch {
    return true;
  }
}

// --- Load preview ---
export function loadPreview(filePath: string): void {
  if (isBinary(filePath)) {
    batch(() => {
      previewContent.set(["(binary file)"]);
      previewIsBinary.set(true);
      previewFileName.set(path.basename(filePath));
      previewScroll.set(0);
    });
    return;
  }

  try {
    const content = fs.readFileSync(filePath, "utf-8");
    const lines = content.split("\n").slice(0, 1000);
    batch(() => {
      previewContent.set(lines);
      previewIsBinary.set(false);
      previewFileName.set(path.basename(filePath));
      previewScroll.set(0);
    });
  } catch {
    batch(() => {
      previewContent.set(["(unable to read file)"]);
      previewIsBinary.set(false);
      previewFileName.set("");
      previewScroll.set(0);
    });
  }
}

// --- Initialize ---
export function initTree(): void {
  const root = rootPath.peek();
  const children = loadDirectory(root, 0);
  tree.set(children);
  const count = children.length;
  treeScroll.set(updateScrollRegion(treeScroll.peek(), count, 20));
  if (count > 0) {
    selectedIndex.set(0);
    const first = children[0];
    if (first.type === "file") loadPreview(first.path);
  }
}

// --- Actions ---
export function moveUp(): void {
  const idx = selectedIndex.peek();
  if (idx <= 0) return;
  const newIdx = idx - 1;
  batch(() => {
    selectedIndex.set(newIdx);
    treeScroll.set(scrollUp(treeScroll.peek()));
  });
  autoPreview();
}

export function moveDown(): void {
  const idx = selectedIndex.peek();
  const list = flatList.get();
  if (idx >= list.length - 1) return;
  const newIdx = idx + 1;
  batch(() => {
    selectedIndex.set(newIdx);
    treeScroll.set(scrollDown(treeScroll.peek()));
  });
  autoPreview();
}

function autoPreview(): void {
  const node = selectedNode.get();
  if (node && node.type === "file") {
    loadPreview(node.path);
  }
}

export function expandOrOpen(): void {
  const node = selectedNode.get();
  if (!node) return;

  if (node.type === "dir") {
    if (!node.expanded) {
      // Expand: load children if not loaded
      if (!node.children) {
        node.children = loadDirectory(node.path, node.depth + 1);
      }
      node.expanded = true;
      // Trigger reactivity
      tree.set([...tree.peek()]);
      const list = flatList.get();
      treeScroll.set(updateScrollRegion(treeScroll.peek(), list.length));
    }
  } else {
    loadPreview(node.path);
  }
}

export function collapseDir(): void {
  const node = selectedNode.get();
  if (!node) return;

  if (node.type === "dir" && node.expanded) {
    node.expanded = false;
    tree.set([...tree.peek()]);
    const list = flatList.get();
    // Ensure selected index is still valid
    const newIdx = Math.min(selectedIndex.peek(), list.length - 1);
    batch(() => {
      selectedIndex.set(newIdx);
      treeScroll.set(updateScrollRegion(treeScroll.peek(), list.length));
    });
  }
}

export function switchPane(): void {
  focusPane.set(focusPane.peek() === "tree" ? "preview" : "tree");
}

export function scrollPreviewUp(): void {
  const s = previewScroll.peek();
  if (s > 0) previewScroll.set(s - 1);
}

export function scrollPreviewDown(): void {
  const s = previewScroll.peek();
  const lines = previewContent.peek();
  if (s < lines.length - 1) previewScroll.set(s + 1);
}
