import type { FileDiffMetadata } from "@pierre/diffs";
import {
  FileTree,
  type GitStatus,
  type GitStatusEntry,
} from "@pierre/trees";
import { pendingCommentCount } from "./comment-state";
import { fileTreeChangeStats } from "./file-tree-stats";
import { commentIconMask, seenIconMask, treeIcons } from "./icons";
import type { CommentMetadata } from "./review-state";

const TREE_STYLES = `
  [data-type="item"] {
    --tact-tree-row-bg: var(--trees-bg);
    position: relative;
  }
  [data-type="item"]:hover {
    --tact-tree-row-bg: var(--trees-bg-muted);
  }
  [data-type="item"][aria-selected="true"] {
    --tact-tree-row-bg: var(--trees-selected-bg);
  }
  [data-item-section="decoration"] {
    position: absolute;
    z-index: 2;
    --tact-comment-icon: url("${commentIconMask}");
    --tact-seen-icon: url("${seenIconMask}");
    --tact-comment-indicator: #4b8cff;
    inset-block: var(--trees-focus-ring-width);
    inset-inline-end: calc(var(--trees-item-padding-x) + var(--trees-git-lane-width));
    align-items: center;
    padding-inline-start: 8px;
    pointer-events: none;
    background-color: var(--tact-tree-row-bg);
    text-align: right;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 11px;
    font-variant-numeric: tabular-nums;
  }
  [data-item-section="decoration"] span[style*="--tact-comment-indicator"] {
    display: inline-flex;
    align-items: center;
  }
  [data-item-section="decoration"] span[style*="--tact-comment-indicator"]::before {
    width: 12px;
    height: 12px;
    content: "";
    background-color: currentColor;
    -webkit-mask: var(--tact-comment-icon) center / contain no-repeat;
    mask: var(--tact-comment-icon) center / contain no-repeat;
  }
  [data-item-section="decoration"] [title*="Seen"] {
    display: inline-flex;
    align-items: center;
  }
  [data-item-section="decoration"] [title*="Seen"]::after {
    width: 13px;
    height: 13px;
    flex: none;
    margin-inline-start: 8px;
    content: "";
    background-color: var(--trees-accent);
    -webkit-mask: var(--tact-seen-icon) center / contain no-repeat;
    mask: var(--tact-seen-icon) center / contain no-repeat;
  }
  [data-type="item"]:has([data-item-section="decoration"] [title*="Seen"]) [data-item-section="content"] {
    opacity: .56;
    text-decoration: line-through;
  }
`;

export type ChangedFilesTreeDeps = {
  comments(): readonly CommentMetadata[];
  seenFiles(): ReadonlySet<string>;
  appearance(): "light" | "dark";
  /** Shows the diff of the file the reviewer picked in the tree. */
  selectFile(path: string): void;
};

/**
 * The changed-file tree beside the diff. Each row shows the file's change totals, its pending
 * comment count, and whether the reviewer marked it as seen.
 */
export class ChangedFilesTree {
  private tree?: FileTree;
  private stats = new Map<string, { additions: number; deletions: number }>();

  constructor(
    private readonly root: HTMLElement,
    private readonly deps: ChangedFilesTreeDeps,
  ) {}

  render(files: readonly FileDiffMetadata[]) {
    const container = this.root.querySelector<HTMLElement>("#file-tree");
    if (!container) return;
    this.stats = fileTreeChangeStats(files);
    if (this.tree) {
      this.tree.resetPaths(files.map((file) => file.name));
      this.tree.setGitStatus(this.gitStatus(files));
      this.refreshDecorations();
      return;
    }
    this.tree = new FileTree({
      paths: files.map((file) => file.name),
      flattenEmptyDirectories: true,
      initialExpansion: "open",
      density: "compact",
      icons: treeIcons,
      unsafeCSS: TREE_STYLES,
      gitStatus: this.gitStatus(files),
      renderRowDecoration: ({ item }) => {
        const stats = this.stats.get(item.path);
        if (!stats) return null;

        const count = item.kind === "file"
          ? pendingCommentCount(this.deps.comments(), item.path)
          : 0;
        const seen = item.kind === "file" && this.deps.seenFiles().has(item.path);
        const title = [`+${stats.additions} / -${stats.deletions}`];
        const parts: Array<{ text: string; color?: string }> = [
          { text: `+${stats.additions}`, color: "var(--trees-status-added)" },
          { text: "\u00a0/\u00a0", color: "var(--trees-fg-muted)" },
          { text: `-${stats.deletions}`, color: "var(--trees-status-deleted)" },
        ];
        if (count > 0) {
          title.push(`${count} pending ${count === 1 ? "comment" : "comments"}`);
          parts.push(
            { text: "\u00a0\u00a0" },
            { text: `\u00a0${count}`, color: "var(--tact-comment-indicator)" },
          );
        }
        if (seen) {
          title.push("Seen");
        }
        return {
          text: `+${stats.additions} / -${stats.deletions}`,
          parts,
          title: title.join(" · "),
        };
      },
      onSelectionChange: (paths) => {
        const path = paths.at(-1);
        if (path) this.deps.selectFile(path);
      },
    });
    this.tree.render({ containerWrapper: container });
    this.syncAppearance();
  }

  syncAppearance() {
    const container = this.tree?.getFileTreeContainer();
    if (!container) return;
    container.style.colorScheme = this.deps.appearance();
  }

  private gitStatus(files: readonly FileDiffMetadata[]): GitStatusEntry[] {
    return files.map((file) => ({
      path: file.name,
      status: treeStatus(file.type),
    }));
  }

  refreshDecorations() {
    this.tree?.setIcons(treeIcons);
  }

  dispose() {
    this.tree?.cleanUp();
  }
}

function treeStatus(type: FileDiffMetadata["type"]): GitStatus {
  switch (type) {
    case "new": return "added";
    case "deleted": return "deleted";
    case "rename-pure":
    case "rename-changed": return "renamed";
    case "change": return "modified";
  }
}
