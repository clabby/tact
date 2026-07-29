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

test("the refresh banner keeps stable spacing around its separator", async () => {
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();
  const banner = styles.match(/\.refresh-notice\s*{([^}]*)}/)?.[1];
  const action = styles.match(/\.refresh-notice\s+strong\s*{([^}]*)}/)?.[1];

  expect(banner).toMatch(/gap:\s*8px/);
  expect(banner).toMatch(/white-space:\s*nowrap/);
  expect(action).toMatch(/padding-left:\s*8px/);
});

test("the overview tab shows when its agent is working", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();

  expect(app).toContain('class="overview-tab-activity" aria-hidden="true"');
  expect(app).toContain('tab.classList.toggle("loading", loading)');
  expect(app).toContain('tab.setAttribute("aria-busy", "true")');
  expect(app).toContain("this.setOverviewLoading(true)");
  expect(app).toContain("this.setOverviewLoading(false)");
  expect(styles).toMatch(/\.tab\.loading\s+\.overview-tab-activity\s*{/s);
  expect(styles).toMatch(/\.overview-tab-activity\s+i\s*{[^}]*animation:\s*tact-shimmer/s);
  expect(styles).toMatch(/\.overview-spinner::before\s*{[^}]*animation:\s*tact-spin/s);
});

test("the inline answer spinner can visibly rotate", async () => {
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();
  const spinner = styles.match(/\.thread-spinner\s*{([^}]*)}/)?.[1];

  expect(spinner).toBeDefined();
  expect(spinner).toMatch(/display:\s*inline-block/);
  expect(spinner).toMatch(/animation:\s*tact-thread-spin/);
  const reducedMotion = styles.match(
    /@media\s*\(prefers-reduced-motion:\s*reduce\)\s*{([\s\S]*)}\s*$/,
  )?.[1];
  expect(reducedMotion).not.toContain(".thread-spinner");
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

test("the comment input uses a neutral focus indicator", async () => {
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();
  const focus = styles.match(/\.comment-input:focus\s*{([^}]*)}/)?.[1];
  const focusVisible = styles.match(/\.comment-input:focus-visible\s*{([^}]*)}/)?.[1];

  expect(focus).toMatch(/box-shadow:\s*0\s+0\s+0\s+1px\s+var\(--line-strong\)\s+inset/);
  expect(focus).not.toContain("var(--blue)");
  expect(focusVisible).toMatch(/outline:\s*0/);
});

test("new inline drafts can become agent questions", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();
  const editor = app.slice(
    app.indexOf("private commentComposerElement"),
    app.indexOf("private askDraftQuestion"),
  );

  expect(editor).toContain('data-comment-action="ask"');
  expect(editor).toContain('Ask <span aria-hidden="true">✨</span>');
  expect(editor).toContain('draft.editingId === undefined');
});

test("overview generation and question threads share one agent-operation gate", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();
  const overview = app.slice(
    app.indexOf("private async loadOverview"),
    app.indexOf("private showOverviewError"),
  );
  const question = app.slice(
    app.indexOf("private askDraftQuestion"),
    app.indexOf("private pendingCommentElement"),
  );

  expect(overview).toContain("(this.agentOperation && !restoring)");
  expect(overview).toContain('this.agentOperation = { kind: "overview", request }');
  expect(question).toContain("if (!draft || draft.editingId !== undefined || !page || this.agentOperation) return");
  expect(question).toContain('kind: "question"');
  expect(question).toContain("operationId");
  expect(app).toContain('querySelectorAll<HTMLTextAreaElement>("[data-thread-input]")');
});

test("the review can be cancelled while the agent is working", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();
  const controls = app.slice(
    app.indexOf("private setReviewControlsDisabled"),
    app.indexOf("private async submit"),
  );
  const cancel = app.slice(
    app.indexOf("private async cancel()"),
    app.indexOf("private showTerminalBusy"),
  );

  expect(controls).toContain("cancel.disabled = terminalBusy");
  expect(cancel).not.toContain("if (this.agentOperation) return");
});

test("the mobile layout reserves room for its stacked review actions", async () => {
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();
  const mobile = styles.slice(
    styles.indexOf("@media (max-width: 760px)"),
    styles.indexOf("@media (max-width: 600px)"),
  );

  expect(styles).toContain("--review-footer-height: 68px");
  expect(styles).toContain("var(--review-footer-height)");
  expect(mobile).toContain("--review-footer-height: 132px");
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

test("the file change totals and comment indicator preserve their spacing", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();
  const treeStyles = app.slice(
    app.indexOf("const TREE_STYLES"),
    app.indexOf("type AnnotationMetadata"),
  );

  expect(app).toContain('text: "\\u00a0/\\u00a0"');
  expect(app).toContain('{ text: "\\u00a0\\u00a0" }');
  expect(app).toContain('text: `\\u00a0${count}`, color: "var(--tact-comment-indicator)"');
  expect(app).not.toContain("🗨︎");
  expect(treeStyles).toContain("${commentIconMask}");
  expect(treeStyles).toContain("${seenIconMask}");
  expect(treeStyles).toMatch(/span\[style\*="--tact-comment-indicator"\]::before/);
  expect(treeStyles).toContain("--tact-seen-icon");
  expect(treeStyles).toMatch(/\[title\*="Seen"\]::after\s*{[^}]*margin-inline-start:\s*8px/s);
  expect(app).not.toContain('{ text: "  ✓"');
});

test("the file tree follows explicit appearance settings", async () => {
  const app = await Bun.file(new URL("app.ts", import.meta.url)).text();
  const sync = app.slice(
    app.indexOf("private syncTreeAppearance"),
    app.indexOf("private treeGitStatus"),
  );
  const settings = app.slice(
    app.indexOf("private applySettings"),
    app.indexOf("private selectTab"),
  );

  expect(sync).toContain("getFileTreeContainer()");
  expect(sync).toContain('selected === "system" ? "light dark" : selected');
  expect(settings).toContain("this.syncTreeAppearance()");
});

test("the range warning has stable vertical spacing", async () => {
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();
  const warning = styles.match(/\.range-warning\s*{([^}]*)}/)?.[1];

  expect(warning).toMatch(/margin:\s*14px\s+18px/);
});

test("dark surfaces are neutral while green remains an accent", async () => {
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();

  expect(styles).toContain("--bg: light-dark(#f7f8f5, #0f1115)");
  expect(styles).toContain("--surface: light-dark(#ffffff, #171a20)");
  expect(styles).toContain("--accent: light-dark(#315f36, #9bc59e)");
  expect(styles).not.toContain("#111311");
  expect(styles).not.toContain("#181b18");
});
