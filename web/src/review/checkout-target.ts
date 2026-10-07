import type { Checkout } from "../core/wire";

const CHOICES_KEY = "tact.web.review-checkout";
/** Old sessions' choices are dropped beyond this many, oldest first. */
const CHOICES_LIMIT = 50;

type ChoiceStorage = Pick<Storage, "getItem" | "setItem">;

function readChoices(storage: ChoiceStorage): [string, string][] {
  try {
    const parsed: unknown = JSON.parse(storage.getItem(CHOICES_KEY) ?? "[]");
    return Array.isArray(parsed) ? parsed.filter((entry): entry is [string, string] =>
      Array.isArray(entry) && typeof entry[0] === "string" && typeof entry[1] === "string") : [];
  } catch {
    return [];
  }
}

/** The checkout this browser last reviewed for `session`, or null for the session's workspace. */
export function loadCheckoutChoice(storage: ChoiceStorage, session: string): string | null {
  return readChoices(storage).find(([candidate]) => candidate === session)?.[1] ?? null;
}

/** Remembers the reviewed checkout for `session`; null returns it to the session's workspace. */
export function saveCheckoutChoice(storage: ChoiceStorage, session: string, checkout: string | null) {
  const choices = readChoices(storage).filter(([candidate]) => candidate !== session);
  if (checkout !== null) choices.push([session, checkout]);
  storage.setItem(CHOICES_KEY, JSON.stringify(choices.slice(-CHOICES_LIMIT)));
}

/**
 * Whether a change in `changed` (null: the session's workspace) affects the review of `target`
 * (null: the session's workspace). `sessionWorkspace` is null until the session's path is known,
 * and then any change may concern the session's workspace.
 */
export function changeAffectsTarget(changed: string | null, target: string | null, sessionWorkspace: string | null) {
  const changedPath = changed ?? sessionWorkspace;
  const targetPath = target ?? sessionWorkspace;
  if (changedPath === null || targetPath === null) return target === null || changed === target;
  return changedPath === targetPath;
}

/**
 * Checkouts the agent works in besides the review target. A checkout stays acknowledged, and so
 * out of the notice, only while it remains touched; touching it again later announces it again.
 */
export class TouchedCheckouts {
  private touched: Checkout[] = [];
  private acknowledged = new Set<string>();

  update(checkouts: readonly Checkout[], target: string) {
    this.touched = checkouts.filter((checkout) => checkout.touched && !checkout.missing && checkout.path !== target);
    const touched = new Set(this.touched.map((checkout) => checkout.path));
    this.acknowledged = new Set([...this.acknowledged].filter((path) => touched.has(path)));
  }

  /** The touched checkouts the reviewer has not seen announced yet. */
  get unseen() {
    return this.touched.filter((checkout) => !this.acknowledged.has(checkout.path));
  }

  acknowledge() {
    for (const checkout of this.touched) this.acknowledged.add(checkout.path);
  }

  reset() {
    this.touched = [];
    this.acknowledged.clear();
  }
}

/** The notice's sentence for the unseen touched checkouts. */
export function touchedNotice(unseen: readonly Checkout[]) {
  if (unseen.length === 0) return "";
  return unseen.length === 1
    ? `Agent is also working in ${unseen[0]!.label}`
    : `${unseen.length} other checkouts changed by the agent`;
}
