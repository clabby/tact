import type { TranscriptData } from "../core/store";
import type { WireEntry } from "../core/wire";
import { isRoutine, type ToolEntry } from "./routine";
import { classifyFailures } from "./recovery";

/**
 * One turn of the conversation: a prompt, the work it caused, and the entry that closed it. Entries
 * before the first prompt (a fork notice, say) form a turn without a prompt, keyed -1.
 */
export type Turn = {
  /** The prompt's entry id, which stays stable while the turn grows. */
  key: number;
  user: number | null;
  /** Everything between the prompt and the end: the work and the answer, nested entries included. */
  body: number[];
  /** The `turn_completed` or `interrupted` entry; null while the turn runs or was steered into the next. */
  end: number | null;
  /** Entries after the end and before the next prompt, such as a compaction or a settings change. */
  trailing: number[];
};

export function segmentTurns(data: TranscriptData): Turn[] {
  const turns: Turn[] = [];
  const turnOf = new Map<number, Turn>();
  let turn: Turn | null = null;
  for (const id of data.order) {
    const entry = data.entries.get(id);
    if (!entry) continue;
    // A child call (of a Code Mode batch, say) belongs to its parent's turn wherever it arrives.
    const owner = entry.parent === null ? undefined : turnOf.get(entry.parent);
    if (owner) {
      owner.body.push(id);
      turnOf.set(id, owner);
      continue;
    }
    if (entry.kind === "user" && entry.parent === null) {
      turns.push(turn = { key: id, user: id, body: [], end: null, trailing: [] });
      continue;
    }
    if (!turn) turns.push(turn = { key: -1, user: null, body: [], end: null, trailing: [] });
    turnOf.set(id, turn);
    if (turn.end !== null) turn.trailing.push(id);
    else if (entry.parent === null && (entry.kind === "turn_completed" || entry.kind === "interrupted")) turn.end = id;
    else turn.body.push(id);
  }
  return turns;
}

/** One row of a turn's work log: an entry, or a run of routine calls folded into one step. */
export type LogItem =
  | { kind: "entry"; id: number }
  /** `key` is the first call's id; `members` includes thoughts between the calls. */
  | { kind: "group"; key: number; members: number[]; tools: number[] };

export type TurnPlan = {
  /** The last assistant message, unless a tool call followed it in a running turn. */
  answer: number | null;
  /** Assistant messages before the answer, which narrate the work. */
  narration: Set<number>;
  /**
   * The body without the answer, in order, with routine runs grouped. Child entries are not
   * rows of their own: they extend their parent's row, in `children`.
   */
  log: LogItem[];
  /** Each parent's child entries in order, which render under it whatever arrived in between. */
  children: Map<number, number[]>;
  /** Notices that stay in sight when the work log is folded. */
  pinned: Set<number>;
  recovered: Set<number>;
  unrecovered: number[];
  /** Tool calls, child calls included. */
  steps: number;
};

/** Kinds that report a problem the reader must see even when the work log is folded. */
const PINNED = new Set(["error", "compaction_failed"]);

/** Fewer routine calls than this keep their own rows. */
const MIN_GROUP = 2;

export function planTurn(turn: Turn, entries: ReadonlyMap<number, WireEntry>): TurnPlan {
  const body = turn.body.flatMap((id) => entries.get(id) ?? []);
  const finished = turn.end !== null;
  let answer: number | null = null;
  for (let index = body.length - 1; index >= 0; index -= 1) {
    const entry = body[index]!;
    if (entry.parent !== null) continue;
    if (entry.kind === "assistant") {
      answer = entry.id;
      break;
    }
    if (entry.kind === "tool" && !finished) break;
  }
  const narration = new Set(body.filter((entry) => entry.kind === "assistant" && entry.parent === null && entry.id !== answer).map((entry) => entry.id));
  const tools = body.filter((entry): entry is ToolEntry => entry.kind === "tool");
  const { recovered, unrecovered } = classifyFailures(tools);
  const inTurn = new Set(turn.body);
  const children = new Map<number, number[]>();
  for (const entry of body) {
    if (entry.parent === null || !inTurn.has(entry.parent)) continue;
    const siblings = children.get(entry.parent) ?? [];
    siblings.push(entry.id);
    children.set(entry.parent, siblings);
  }

  const log: LogItem[] = [];
  let run: WireEntry[] = [];
  const flushRun = () => {
    // Thoughts after the last routine call belong to whatever comes next, not to the group.
    let end = run.length;
    while (end > 0 && run[end - 1]!.kind !== "tool") end -= 1;
    const members = run.slice(0, end);
    const calls = members.filter((entry) => entry.kind === "tool");
    if (calls.length >= MIN_GROUP) {
      log.push({ kind: "group", key: members[0]!.id, members: members.map((entry) => entry.id), tools: calls.map((entry) => entry.id) });
    } else {
      for (const entry of members) log.push({ kind: "entry", id: entry.id });
    }
    for (const entry of run.slice(end)) log.push({ kind: "entry", id: entry.id });
    run = [];
  };
  for (const entry of body) {
    if (entry.parent !== null && inTurn.has(entry.parent)) continue;
    if (entry.id === answer) {
      flushRun();
      continue;
    }
    if (isRoutine(entry) || (entry.kind === "reasoning" && run.length > 0)) {
      run.push(entry);
      continue;
    }
    flushRun();
    log.push({ kind: "entry", id: entry.id });
  }
  flushRun();

  return {
    answer,
    narration,
    log,
    children,
    pinned: new Set(body.filter((entry) => PINNED.has(entry.kind)).map((entry) => entry.id)),
    recovered,
    unrecovered,
    steps: tools.length,
  };
}
