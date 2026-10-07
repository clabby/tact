import { expect, test } from "bun:test";

const read = (name: string) => Bun.file(new URL(name, import.meta.url)).text();
const panel = () => read("review-panel.ts");
/** Every module that implements the review panel. */
const panelModules = [
  "review-panel.ts",
  "panel-markup.ts",
  "markup.ts",
  "annotations.ts",
  "ai-review.ts",
  "changed-files-tree.ts",
  "comment-editor.ts",
  "diff-view.ts",
  "overview-panel.ts",
  "question-threads.ts",
  "range-dialog.ts",
  "review-status.ts",
  "search-bar.ts",
  "settings-popover.ts",
];
const allPanelSources = async () => (await Promise.all(panelModules.map(read))).join("\n");
const styles = () => read("review-panel.css");
const rule = (css: string, selector: string) =>
  css.match(new RegExp("(?:^|\\n)\\s*" + selector.replace(/[.*+?^$()|[\]\\]/g, "\\$&") + "\\s*{([^}]*)}"))?.[1];

test("the diff view owns its scrollable viewport", async () => {
  expect(rule(await styles(), ".diff-view")).toMatch(/overflow:\s*auto/);
});

test("the panel is scoped, themed by shared tokens, and defines no palette of its own", async () => {
  const css = await styles();
  expect(css.startsWith(".review-panel {")).toBe(true);
  expect(css).not.toContain(":root");
  expect(css).not.toContain("light-dark(");
  expect(css).not.toMatch(/--(accent-solid|accent-on|blue)\b/);
  expect(css).not.toMatch(/(^|\s)(html|body)\b/m);
});

test("the layout follows the panel's width, not the window's", async () => {
  const css = await styles();
  expect(css).toMatch(/container:\s*review\s*\/\s*inline-size/);
  expect(css).toContain("@container review (max-width: 760px)");
  expect(css).not.toContain("@media (max-width: 760px)");
});

test("review search integrates with the virtualized review lifecycle", async () => {
  const app = await panel();
  const search = await read("search-bar.ts");
  const diff = await read("diff-view.ts");
  const css = await styles();
  const shortcuts = search.slice(search.indexOf("handleShortcut"), search.indexOf("constructor("));
  const open = search.slice(search.indexOf("private open()"), search.indexOf("\n  close()"));
  const close = search.slice(search.indexOf("\n  close()"), search.indexOf("\n  reset()"));
  const dispose = search.slice(search.indexOf("\n  dispose()"), search.indexOf("\n  selectedLinesChanged("));
  const cleanup = app.slice(app.indexOf("\n  cleanUp()"), app.indexOf("private runningChanged"));
  const diffDispose = diff.slice(diff.indexOf("\n  dispose()"), diff.indexOf("\n  render("));

  expect(await read("panel-markup.ts")).toContain('id="review-search" role="search" hidden');
  expect(shortcuts.indexOf("dialog[open]")).toBeLessThan(shortcuts.indexOf('key === "f"'));
  expect(shortcuts).toContain("this.root.contains(event.target as Node)");
  expect(search).toContain("event.isComposing");
  expect(search).toContain("moveSearchTarget(");
  expect(search).toContain("this.paused = true");
  expect(search).toContain("occurrenceIndex + 1");
  expect(search).toContain('this.deps.selectMobilePanel("diff")');
  expect(search).toContain("CSS.highlights.set");
  expect(search).toContain("data-line-type");
  expect(open).not.toContain("this.reveal()");
  expect(close).not.toContain("this.match = undefined");
  expect(search).toContain("deepActiveElement(document)");
  expect(search).toContain('this.root.querySelector<HTMLElement>("#diff-view")');
  expect(search).toContain("if (itemId === this.match?.itemId) this.updateHighlight(root)");
  expect(diff).toContain('this.deps.itemRendered(context.item.id, phase === "unmount" ? null : node.shadowRoot)');
  expect(app).toContain("itemRendered: (itemId, root) => this.search.itemRendered(itemId, root)");
  expect(search).toContain("if (this.isOpen()) this.selection = null");
  expect((await allPanelSources()).match(/viewer(\(\))?\?\.clearSelectedLines\(\)/g)).toHaveLength(1);
  expect(cleanup).toContain("this.search.dispose()");
  expect(dispose).toContain('document.removeEventListener("keydown", this.handleShortcut)');
  expect(cleanup.indexOf("this.diffView.dispose()")).toBeLessThan(cleanup.indexOf("CSS.highlights?.delete"));
  expect(diffDispose).toContain("this.codeView?.cleanUp()");
  expect(css).toMatch(/\.review-search\s*{[^}]*position:\s*absolute/s);
});

test("the refresh banner keeps stable spacing around its separator", async () => {
  const css = await styles();
  expect(rule(css, ".refresh-notice")).toMatch(/gap:\s*8px/);
  expect(rule(css, ".refresh-notice")).toMatch(/white-space:\s*nowrap/);
  expect(rule(css, ".refresh-notice strong")).toMatch(/padding-left:\s*8px/);
});

test("working indicators stay visibly animated", async () => {
  const app = await allPanelSources();
  const css = await styles();
  const reducedMotion = css.match(/@media\s*\(prefers-reduced-motion:\s*reduce\)\s*{([\s\S]*)}\s*$/)?.[1];

  expect(app).toContain('class="activity-spinner overview-tab-activity" aria-hidden="true"');
  expect(app).toContain('tab.classList.toggle("loading", loading)');
  expect(app).toContain('class="activity-spinner" aria-hidden="true"></span>Reviewing…');
  expect(css).toMatch(/\.tab\.loading\s+\.overview-tab-activity\s*{/s);
  expect(await read("../shell/shell.css")).toMatch(/\.review-panel \.activity-spinner[^{]*{[^}]*animation:\s*tact-chase/s);
  expect(css).toMatch(/\.overview-spinner::before\s*{[^}]*animation:\s*tact-spin/s);
  expect(rule(css, ".thread-spinner")).toMatch(/display:\s*inline-block/);
  expect(rule(css, ".thread-spinner")).toMatch(/animation:\s*tact-thread-spin/);
  expect(rule(css, ".live-badge i")).toMatch(/animation:\s*tact-pulse/);
  expect(reducedMotion).toBeDefined();
  expect(reducedMotion).not.toMatch(/\.(thread-spinner|activity-spinner|overview-spinner|live-badge)\b[^{]*{[^}]*animation:\s*none/);
});

test("seen files are crossed out in the file tree", async () => {
  const app = await read("changed-files-tree.ts");
  expect(app).toContain('[title*="Seen"]');
  expect(app).toMatch(/text-decoration:\s*line-through/);
});

test("the comment editor keeps its actions after the comment body", async () => {
  const comments = await read("comment-editor.ts");
  const editor = comments.slice(comments.indexOf("\n  composerElement("), comments.indexOf("\n  commentElement("));
  expect(editor.indexOf('class="editor-heading"')).toBeLessThan(editor.indexOf('class="comment-input"'));
  expect(editor.indexOf('class="comment-input"')).toBeLessThan(editor.indexOf('class="editor-footer"'));
  expect(editor).toContain('data-comment-action="ask"');
  expect(editor).toContain("draft.editingId === undefined");
});

test("the comment input uses a neutral focus indicator", async () => {
  const css = await styles();
  expect(rule(css, ".comment-input:focus")).toMatch(/box-shadow:\s*0\s+0\s+0\s+1px\s+var\(--line-strong\)\s+inset/);
  expect(rule(css, ".comment-input:focus-visible")).toMatch(/outline:\s*0/);
});

test("comments stay editable while the agent works, but agent actions wait for it", async () => {
  const app = await panel();
  const editor = await read("comment-editor.ts");
  const threads = await read("question-threads.ts");
  const locked = app.slice(app.indexOf("private get commentsLocked"), app.indexOf("private get agentUnavailable"));
  const unavailable = app.slice(app.indexOf("private get agentUnavailable"), app.indexOf("private get agentUnavailableReason"));
  const comments = editor.slice(editor.indexOf("\n  open("), editor.indexOf("\n  composerElement("));
  const questions = threads.slice(threads.indexOf("\n  askDraft()"), threads.indexOf("private startQuestion"));

  expect(locked).not.toContain("this.running");
  expect(unavailable).toContain("this.running");
  expect(unavailable).toContain("this.session === null");
  expect(comments).toContain("if (!selection || this.deps.commentsLocked()) return");
  expect(questions).toContain("this.deps.agentUnavailable()");
  expect(app).toContain("this.overview.loading || this.aiReview.pending || this.questionThreads.busy");
  expect(await read("overview-panel.ts")).toContain("return this.loadingOverview !== undefined;");
  expect(await read("ai-review.ts")).toContain("return this.aiReviewPending;");
  expect(threads).toContain("return this.questionOperations.size > 0;");
  expect(app).toContain('querySelectorAll<HTMLTextAreaElement>("[data-overview-instructions], [data-thread-input]")');
});

test("independent requests retain their own pending state", async () => {
  const overviewPanel = await read("overview-panel.ts");
  const threads = await read("question-threads.ts");
  const overview = overviewPanel.slice(overviewPanel.indexOf("\n  async load("), overviewPanel.indexOf("private showError"));
  const question = threads.slice(threads.indexOf("\n  askDraft()"), threads.indexOf("\n  threadElement("));

  expect(overview).toContain("rangesEqual(this.loadingOverview, page.selected_range)");
  expect(overview).not.toContain("aiReviewPending");
  expect(question).toContain("this.questionOperations.set(thread.id");
  expect(question).toContain("this.questionOperations.get(thread.id)");
  expect(question).toContain("this.questionsToPoll.add(thread.id)");
});

test("the review is delivered to the chat, not decided in a one-shot flow", async () => {
  const app = await allPanelSources();
  const panelSource = await panel();
  const send = panelSource.slice(panelSource.indexOf("private async sendToChat"), panelSource.indexOf("\nfunction outdatedNote"));

  expect(send).toContain("this.api.compose(");
  expect(send).toContain("this.host.sendToChat(markdown)");
  expect(send.indexOf("this.api.compose(")).toBeLessThan(send.indexOf("this.host.sendToChat(markdown)"));
  expect(send).toContain("comment.outdated");
  for (const gone of ["/decision", "cancel-review", "Review submitted", "Review cancelled", "beginTerminal", "/status"]) {
    expect(app).not.toContain(gone);
  }
  expect(app).toContain("Send to chat");
});

test("a panel stays alive across session changes and releases everything on dispose", async () => {
  const app = await panel();
  const cleanup = app.slice(app.indexOf("\n  cleanUp()"), app.indexOf("private runningChanged"));

  expect(app).toContain("host.onActiveSessionChange(");
  expect(app).toContain("host.onWorkspaceChanged(");
  expect(app).toContain("host.onRunningChange(");
  expect(app).toContain("host.onThemeChange(");
  expect(cleanup).toContain("for (const stop of this.unsubscribe) stop()");
  expect(cleanup).toContain("this.refresher.dispose()");
  expect(cleanup).toContain("this.diffView.dispose()");
  const diff = await read("diff-view.ts");
  expect(diff.slice(diff.indexOf("\n  dispose()"), diff.indexOf("\n  render("))).toContain("this.workerPool.terminate()");
});

test("the changed-file wrapper owns the tree's available height", async () => {
  const navigation = rule(await styles(), ".files-navigation");
  expect(navigation).toMatch(/display:\s*grid/);
  expect(navigation).toMatch(/grid-template-rows:\s*45px\s+minmax\(0,\s*1fr\)/);
  expect(navigation).toMatch(/min-height:\s*0/);
  expect(navigation).toMatch(/height:\s*100%/);
});

test("change totals layer above long file names", async () => {
  const tree = await read("changed-files-tree.ts");
  const treeStyles = tree.slice(tree.indexOf("const TREE_STYLES"), tree.indexOf("export type ChangedFilesTreeDeps"));

  expect(treeStyles).toMatch(/\[data-item-section="decoration"\][^{]*{[^}]*position:\s*absolute/s);
  expect(treeStyles).toMatch(/\[data-item-section="decoration"\][^{]*{[^}]*z-index:\s*2/s);
  expect(treeStyles).toMatch(/inset-block:\s*var\(--trees-focus-ring-width\)/);
  expect(treeStyles).toMatch(/background-color:\s*var\(--tact-tree-row-bg\)/);
  expect(treeStyles).not.toContain("linear-gradient");
});

test("the file change totals and comment indicator preserve their spacing", async () => {
  const app = await read("changed-files-tree.ts");
  const treeStyles = app.slice(app.indexOf("const TREE_STYLES"), app.indexOf("export type ChangedFilesTreeDeps"));

  expect(app).toContain('text: "\\u00a0/\\u00a0"');
  expect(app).toContain('{ text: "\\u00a0\\u00a0" }');
  expect(app).toContain('text: `\\u00a0${count}`, color: "var(--tact-comment-indicator)"');
  expect(treeStyles).toContain("${commentIconMask}");
  expect(treeStyles).toContain("${seenIconMask}");
  expect(treeStyles).toMatch(/\[title\*="Seen"\]::after\s*{[^}]*margin-inline-start:\s*8px/s);
});

test("the file tree and diff follow the application theme", async () => {
  const app = await allPanelSources();
  const panelSource = await panel();
  const tree = await read("changed-files-tree.ts");
  const sync = tree.slice(tree.indexOf("\n  syncAppearance()"), tree.indexOf("private gitStatus"));
  const themed = /appearance: \(\) => appearance\(this\.settings\.current, this\.host\.theme\(\)\)/g;

  expect(sync).toContain("getFileTreeContainer()");
  expect(sync).toContain("this.deps.appearance()");
  expect(await read("diff-view.ts")).toContain("themeType: this.deps.appearance()");
  // The tree, the diff, and the overview frame all follow the application theme.
  expect(panelSource.match(themed)).toHaveLength(3);
  expect(app).not.toContain("prefers-color-scheme");
  expect(app).not.toContain("documentElement");
});

test("the range warning has stable vertical spacing", async () => {
  expect(rule(await styles(), ".range-warning")).toMatch(/margin:\s*14px\s+18px/);
});
