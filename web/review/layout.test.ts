import { expect, test } from "bun:test";

test("the diff view owns its scrollable viewport", async () => {
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();
  const diffViewRule = styles.match(/\.diff-view\s*{([^}]*)}/)?.[1];

  expect(diffViewRule).toBeDefined();
  expect(diffViewRule).toMatch(/overflow:\s*auto/);
});

test("new changes appear before the range selector", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();

  expect(app.indexOf('id="refresh-notice"')).toBeLessThan(
    app.indexOf('id="range-button"'),
  );
});

test("seen files are crossed out in the file tree", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();

  expect(app).toContain('[title*="Seen"]');
  expect(app).toMatch(/text-decoration:\s*line-through/);
});

test("the comment editor keeps its actions after the comment body", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();
  const editor = app.slice(
    app.indexOf("private commentComposerElement"),
    app.indexOf("private pendingCommentElement"),
  );

  expect(editor.indexOf('class="editor-heading"')).toBeLessThan(
    editor.indexOf('class="comment-input"'),
  );
  expect(editor.indexOf('class="comment-input"')).toBeLessThan(
    editor.indexOf('class="editor-footer"'),
  );
});

test("the changed-file wrapper owns the tree's available height", async () => {
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();
  const navigation = styles.match(/\.files-navigation\s*{([^}]*)}/)?.[1];

  expect(navigation).toBeDefined();
  expect(navigation).toMatch(/display:\s*grid/);
  expect(navigation).toMatch(/grid-template-rows:\s*45px\s+minmax\(0,\s*1fr\)/);
  expect(navigation).toMatch(/min-height:\s*0/);
  expect(navigation).toMatch(/height:\s*100%/);
});

test("the desktop file tree has room for paths and change totals", async () => {
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();
  const changesPanel = styles.match(/\.changes-panel\.active\s*{([^}]*)}/)?.[1];

  expect(changesPanel).toBeDefined();
  expect(changesPanel).toMatch(/grid-template-columns:\s*352px\s+minmax\(0,\s*1fr\)/);
});

test("change totals layer above long file names", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();
  const treeStyles = app.slice(
    app.indexOf("const TREE_STYLES"),
    app.indexOf("type AnnotationMetadata"),
  );

  expect(treeStyles).toMatch(/\[data-item-section="decoration"\][^{]*{[^}]*position:\s*absolute/s);
  expect(treeStyles).toMatch(/\[data-item-section="decoration"\][^{]*{[^}]*z-index:\s*2/s);
  expect(treeStyles).toMatch(/inset-block:\s*var\(--trees-focus-ring-width\)/);
  expect(treeStyles).toMatch(/background-color:\s*var\(--tact-tree-row-bg\)/);
  expect(treeStyles).not.toContain("linear-gradient");
});

test("dark surfaces are neutral while green remains an accent", async () => {
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();

  expect(styles).toContain("--bg: light-dark(#f7f8f5, #0f1115)");
  expect(styles).toContain("--surface: light-dark(#ffffff, #171a20)");
  expect(styles).toContain("--accent: light-dark(#315f36, #9bc59e)");
  expect(styles).not.toContain("#111311");
  expect(styles).not.toContain("#181b18");
});
