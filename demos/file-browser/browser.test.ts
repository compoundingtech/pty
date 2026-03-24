// Unit tests for file browser demo
import { describe, it, expect, beforeEach } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import {
  loadDirectory, isBinary, loadPreview,
  tree, flatList, selectedIndex, selectedNode,
  previewContent, previewIsBinary, focusPane, treeScroll,
  moveUp, moveDown, expandOrOpen, collapseDir, switchPane,
  initTree, rootPath,
  type TreeNode,
} from "./state.ts";
import { browserScreen } from "./screens/browser.ts";
import { batch, themes, type ScreenContext, type Theme } from "../../src/tui/index.ts";

// --- Test fixtures ---
let testDir: string;

function makeTestDir(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "fb-test-"));
  // Create structure:
  // testDir/
  //   alpha/
  //     nested.txt
  //   beta/
  //   hello.txt
  //   binary.bin
  fs.mkdirSync(path.join(dir, "alpha"));
  fs.writeFileSync(path.join(dir, "alpha", "nested.txt"), "nested content\nline 2\n");
  fs.mkdirSync(path.join(dir, "beta"));
  fs.writeFileSync(path.join(dir, "hello.txt"), "hello world\nline 2\nline 3\n");
  // Binary file with null bytes
  const binBuf = Buffer.alloc(64);
  binBuf.write("MZ");
  binBuf[10] = 0;
  fs.writeFileSync(path.join(dir, "binary.bin"), binBuf);
  return dir;
}

function testCtx(rows = 35, cols = 120): ScreenContext {
  const theme = themes.coolBlue as Theme;
  return {
    rows, cols,
    theme,
    boxStyle: "rounded",
    navigate: () => {},
    back: () => {},
    openOverlay: () => {},
    closeOverlay: () => {},
    isTextInputActive: () => false,
    setTextInputActive: () => {},
  };
}

beforeEach(() => {
  testDir = makeTestDir();
  rootPath.set(testDir);
  initTree();
});

// --- loadDirectory ---
describe("loadDirectory", () => {
  it("returns directories first, then files, sorted alphabetically", () => {
    const entries = loadDirectory(testDir, 0);
    const names = entries.map(e => e.name);
    expect(names).toEqual(["alpha", "beta", "binary.bin", "hello.txt"]);
  });

  it("marks dirs and files correctly", () => {
    const entries = loadDirectory(testDir, 0);
    expect(entries[0].type).toBe("dir");
    expect(entries[1].type).toBe("dir");
    expect(entries[2].type).toBe("file");
    expect(entries[3].type).toBe("file");
  });

  it("sets depth on entries", () => {
    const entries = loadDirectory(testDir, 2);
    expect(entries[0].depth).toBe(2);
  });

  it("skips .git and node_modules", () => {
    fs.mkdirSync(path.join(testDir, ".git"));
    fs.mkdirSync(path.join(testDir, "node_modules"));
    const entries = loadDirectory(testDir, 0);
    const names = entries.map(e => e.name);
    expect(names).not.toContain(".git");
    expect(names).not.toContain("node_modules");
  });

  it("returns empty array for non-existent directory", () => {
    const entries = loadDirectory(path.join(testDir, "nonexistent"), 0);
    expect(entries).toEqual([]);
  });
});

// --- Binary detection ---
describe("isBinary", () => {
  it("detects binary files via null bytes", () => {
    expect(isBinary(path.join(testDir, "binary.bin"))).toBe(true);
  });

  it("detects text files as non-binary", () => {
    expect(isBinary(path.join(testDir, "hello.txt"))).toBe(false);
  });

  it("returns true for non-existent files", () => {
    expect(isBinary(path.join(testDir, "nope.xyz"))).toBe(true);
  });
});

// --- loadPreview ---
describe("loadPreview", () => {
  it("loads text file lines", () => {
    loadPreview(path.join(testDir, "hello.txt"));
    const lines = previewContent.peek();
    expect(lines[0]).toBe("hello world");
    expect(lines[1]).toBe("line 2");
    expect(previewIsBinary.peek()).toBe(false);
  });

  it("marks binary files", () => {
    loadPreview(path.join(testDir, "binary.bin"));
    expect(previewIsBinary.peek()).toBe(true);
    expect(previewContent.peek()).toEqual(["(binary file)"]);
  });
});

// --- Flat list ---
describe("flatList", () => {
  it("flattens only expanded dirs", () => {
    const list = flatList.get();
    // Initially nothing expanded: just top-level entries
    expect(list.length).toBe(4);
  });

  it("includes children when dir is expanded", () => {
    // Expand alpha
    selectedIndex.set(0); // alpha
    expandOrOpen();
    const list = flatList.get();
    // alpha + alpha/nested.txt + beta + binary.bin + hello.txt
    expect(list.length).toBe(5);
    expect(list[1].name).toBe("nested.txt");
  });
});

// --- Navigation ---
describe("navigation", () => {
  it("moveDown increments selectedIndex", () => {
    selectedIndex.set(0);
    moveDown();
    expect(selectedIndex.peek()).toBe(1);
  });

  it("moveUp decrements selectedIndex", () => {
    selectedIndex.set(2);
    moveUp();
    expect(selectedIndex.peek()).toBe(1);
  });

  it("moveUp at 0 stays at 0", () => {
    selectedIndex.set(0);
    moveUp();
    expect(selectedIndex.peek()).toBe(0);
  });

  it("moveDown at end stays at end", () => {
    const list = flatList.get();
    selectedIndex.set(list.length - 1);
    moveDown();
    expect(selectedIndex.peek()).toBe(list.length - 1);
  });
});

// --- Expand/Collapse ---
describe("expand/collapse", () => {
  it("expandOrOpen expands a directory", () => {
    selectedIndex.set(0); // alpha
    expandOrOpen();
    const node = selectedNode.get();
    expect(node?.expanded).toBe(true);
  });

  it("collapseDir collapses an expanded directory", () => {
    selectedIndex.set(0);
    expandOrOpen();
    expect(selectedNode.get()?.expanded).toBe(true);
    collapseDir();
    expect(flatList.get()[0].expanded).toBe(false);
  });

  it("expandOrOpen on file loads preview", () => {
    // Select hello.txt (index 3)
    selectedIndex.set(3);
    expandOrOpen();
    const lines = previewContent.peek();
    expect(lines[0]).toBe("hello world");
  });
});

// --- Pane switching ---
describe("pane switching", () => {
  it("switchPane toggles focus", () => {
    expect(focusPane.peek()).toBe("tree");
    switchPane();
    expect(focusPane.peek()).toBe("preview");
    switchPane();
    expect(focusPane.peek()).toBe("tree");
  });
});

// --- Screen rendering ---
describe("browser screen", () => {
  it("renders without crashing", () => {
    const ctx = testCtx();
    const buf = browserScreen.renderToBuffer(ctx);
    expect(buf.rows).toBe(35);
    expect(buf.cols).toBe(120);
  });

  it("renders the status bar and footer", () => {
    const ctx = testCtx();
    const rendered = browserScreen.render(ctx);
    expect(rendered).toContain("File Browser");
  });

  it("shows tree entries in the buffer output", () => {
    const ctx = testCtx();
    const buf = browserScreen.renderToBuffer(ctx);
    // Extract all text from the buffer
    const allText = buf.cells.map(row => row.map(c => c.char).join("")).join("\n");
    expect(allText).toContain("alpha");
    expect(allText).toContain("hello.txt");
  });
});

// --- Wrap & Highlight ---
describe("soft wrap and markdown highlighting", () => {
  it("preview lines use wrap instead of truncate", () => {
    // Select hello.txt and load preview
    selectedIndex.set(3); // hello.txt
    expandOrOpen();
    const ctx = testCtx(35, 120);
    const buf = browserScreen.renderToBuffer(ctx);
    const allText = buf.cells.map(r => r.map(c => c.char).join("")).join("\n");
    // hello.txt content should appear in the preview
    expect(allText).toContain("hello world");
  });

  it("markdown headings are bold in preview", () => {
    // Create a markdown file in the test dir
    const mdPath = path.join(testDir, "readme.md");
    fs.writeFileSync(mdPath, "# Main Title\n\nSome text here.\n\n## Section\n\nMore text.\n");
    // Reinitialize to pick up the new file
    rootPath.set(testDir);
    initTree();

    // Find readme.md in the flat list
    const list = flatList.get();
    const mdIdx = list.findIndex(n => n.name === "readme.md");
    expect(mdIdx).toBeGreaterThanOrEqual(0);
    selectedIndex.set(mdIdx);
    expandOrOpen(); // load preview

    const ctx = testCtx(35, 120);
    const buf = browserScreen.renderToBuffer(ctx);
    const allText = buf.cells.map(r => r.map(c => c.char).join("")).join("\n");

    // The heading text should appear
    expect(allText).toContain("Main Title");

    // Find the row containing "# Main Title" and check that chars are bold
    for (let r = 0; r < buf.rows; r++) {
      const rowText = buf.cells[r].map(c => c.char).join("");
      if (rowText.includes("# Main Title")) {
        // Find the # character in this row
        const hashCol = rowText.indexOf("#");
        expect(buf.cells[r][hashCol].bold).toBe(true);
        // The "M" in "Main" should also be bold
        const mCol = rowText.indexOf("Main");
        expect(buf.cells[r][mCol].bold).toBe(true);
        break;
      }
    }
  });

  it("non-markdown files have no highlight bolding", () => {
    // hello.txt is not markdown — no bolding
    selectedIndex.set(3); // hello.txt
    expandOrOpen();
    const ctx = testCtx(35, 120);
    const buf = browserScreen.renderToBuffer(ctx);

    // Find the row with "hello world"
    for (let r = 0; r < buf.rows; r++) {
      const rowText = buf.cells[r].map(c => c.char).join("");
      if (rowText.includes("hello world")) {
        const col = rowText.indexOf("hello");
        expect(buf.cells[r][col].bold).toBe(false);
        break;
      }
    }
  });
});
