import type { MenuItem } from "../ui/menu";
import type { Checkout, Workspaces } from "./wire";

/** How many recent workspaces outside the repository a workspace menu offers. */
const RECENT_LIMIT = 3;

/** The last path component, the name a directory is known by. */
export function baseName(path: string) {
  return path.replace(/\/+$/, "").split("/").pop() || path;
}

/**
 * `to` written relative to `from` (e.g. "../tact-ws2") while that stays short; otherwise `to`
 * itself, since a long run of ".." is harder to read than the absolute path.
 */
export function relativePath(from: string, to: string) {
  const source = from.split("/").filter(Boolean);
  const target = to.split("/").filter(Boolean);
  let common = 0;
  while (common < source.length && common < target.length && source[common] === target[common]) common++;
  const ups = source.length - common;
  if (common === 0 || ups > 2) return to;
  return [...Array<string>(ups).fill(".."), ...target.slice(common)].join("/") || ".";
}

/** A workspace's short name: its checkout label when it belongs to `checkouts`, else its directory name. */
export function workspaceLabel(path: string, checkouts: readonly Checkout[] = []) {
  return checkouts.find((checkout) => checkout.path === path)?.label ?? baseName(path);
}

/**
 * What the composer's workspace chip shows for a session working in `workspace`, or null when
 * there is nothing to choose or point out: a single checkout that is also the default workspace.
 */
export function workspaceChip(workspaces: Workspaces | null, workspace: string | undefined) {
  if (!workspaces || !workspace) return null;
  if (workspaces.checkouts.length < 2 && workspace === workspaces.default) return null;
  return { path: workspace, label: workspaceLabel(workspace, workspaces.checkouts) };
}

export type CheckoutMenuOptions = {
  /** The path shown as chosen. */
  selected: string;
  /** Whether recent workspaces outside the repository are offered too. */
  recent: boolean;
  pick(path: string): void;
};

/**
 * Menu rows for choosing a checkout: the repository's checkouts with a path relative to the
 * session's workspace, changed-file counts, and an accent dot where the agent has been working;
 * then, optionally, a few recent workspaces not already listed.
 */
export function checkoutMenu(workspaces: Workspaces, options: CheckoutMenuOptions): MenuItem[] {
  const base = workspaces.checkouts.find((checkout) => checkout.current)?.path ?? workspaces.default;
  const rows = workspaces.checkouts.map((checkout): MenuItem => ({
    section: "This repository",
    label: checkout.label,
    hint: checkout.current ? "session" : relativePath(base, checkout.path),
    // Every row keeps the leading slot so labels stay aligned whether or not they carry the dot.
    swatch: checkout.touched ? "var(--accent)" : "transparent",
    detail: checkout.missing ? "missing" : checkout.changed_files ? `${checkout.changed_files} changed` : undefined,
    checked: checkout.path === options.selected,
    disabled: checkout.missing,
    run: () => options.pick(checkout.path),
  }));
  if (!options.recent) return rows;
  const listed = new Set(workspaces.checkouts.map((checkout) => checkout.path));
  const recent = workspaces.recent
    .filter((path) => !listed.has(path))
    .slice(0, RECENT_LIMIT)
    .map((path): MenuItem => ({
      section: "Recent",
      label: baseName(path),
      hint: relativePath(base, path),
      swatch: "transparent",
      checked: path === options.selected,
      run: () => options.pick(path),
    }));
  return [...rows, ...recent];
}
