// The context sheet: how full the model window is, what fills it, and how it grew. Every figure is
// a count; tool names are the only server-provided strings and are only ever set as text.

import { formatAge, formatTokens } from "../core/format";
import type { CallPoint, ContextBreakdown, ContextCategory, ContextDiagnostics } from "../core/wire";

const SVG_NS = "http://www.w3.org/2000/svg";

const CATEGORIES: Record<ContextCategory, { label: string; description: string }> = {
  prefix: { label: "Instructions & tools", description: "System instructions, tool definitions, and the rest of the fixed prefix" },
  user: { label: "User prompts", description: "Prompts and steering messages" },
  assistant: { label: "Assistant text", description: "Assistant message text" },
  reasoning: { label: "Reasoning", description: "Model reasoning kept in the context" },
  tool_calls: { label: "Tool calls", description: "Tool call arguments" },
  tool_output: { label: "Tool output", description: "Tool results" },
  compacted: { label: "Compacted summary", description: "The summary that replaced earlier history at the last compaction" },
  other: { label: "Other", description: "Input the transcript cannot account for, such as images" },
};

/** The smallest width, in percent, of a nonzero segment in the composition bar. */
const MINIMUM_SEGMENT = 1.5;

/** The growth chart's drawing box; the SVG stretches to the card, so only the ratio matters. */
const CHART_WIDTH = 600;
const CHART_HEIGHT = 140;

type Builder = { document: Document; now: number };

/**
 * Each value's share of the total in tenths of a percent. Rounding is by largest remainder, so the
 * shares of a nonzero total add up to exactly 1000 and a zero value always gets 0.
 */
export function shareTenths(values: number[]): number[] {
  const total = values.reduce((sum, value) => sum + value, 0);
  if (total <= 0) return values.map(() => 0);
  const exact = values.map((value) => value * 1000 / total);
  const shares = exact.map(Math.floor);
  let missing = 1000 - shares.reduce((sum, share) => sum + share, 0);
  const byRemainder = exact.map((_, index) => index).sort((a, b) => (exact[b]! - shares[b]!) - (exact[a]! - shares[a]!));
  for (const index of byRemainder) {
    if (missing <= 0) break;
    shares[index]! += 1;
    missing -= 1;
  }
  return shares;
}

/** A share for display: a dash for nothing, and "<0.1%" for a nonzero value too small to round up. */
export function shareLabel(value: number, tenths: number) {
  if (value <= 0) return "—";
  if (tenths === 0) return "<0.1%";
  return `${(tenths / 10).toFixed(1)}%`;
}

/**
 * Segment widths in percent: proportional to the values, except that every nonzero value gets at
 * least `minimum` so a small share stays visible, and the larger values give up the difference.
 * The widths of a nonzero total add up to 100.
 */
export function segmentWidths(values: number[], minimum: number): number[] {
  if (values.every((value) => value <= 0)) return values.map(() => 0);
  const pinned = new Set<number>();
  for (;;) {
    const free = values.reduce((sum, value, index) => pinned.has(index) ? sum : sum + Math.max(0, value), 0);
    const room = 100 - minimum * pinned.size;
    const widths = values.map((value, index) => value <= 0 ? 0 : pinned.has(index) ? minimum : value * room / free);
    const narrow = widths.flatMap((width, index) => values[index]! > 0 && !pinned.has(index) && width < minimum ? [index] : []);
    if (narrow.length === 0) return widths;
    for (const index of narrow) pinned.add(index);
  }
}

export type GrowthChart = {
  /** The token count at the top edge. */
  top: number;
  /** One column per call, left to right. */
  columns: { x: number; width: number; y: number; cachedY: number }[];
  /** The step line of input tokens, and the areas under input and under cached input. */
  line: string;
  area: string;
  cachedArea: string;
  /** The left edges of calls that follow a compaction. */
  compactions: number[];
  /** Heights of the auto-compact limit and of the window, when they fall within the scale. */
  limitY: number | null;
  windowY: number | null;
};

/** The share of the auto-compact limit below which a context counts as barely used. */
const BARELY_USED = 0.25;

/**
 * Lays out the input-size history as a step chart in a `width` by `height` box. Once usage is
 * meaningful against the auto-compact limit the scale is the whole window, so the chart reads as
 * progress towards compaction. A barely used context, and one with no limit, scale to the data
 * instead, capped at the window, so the shape of early growth stays visible.
 */
export function growthChart(history: CallPoint[], window: number, limit: number | null, width: number, height: number): GrowthChart {
  const round = (value: number) => Math.round(value * 100) / 100;
  const peak = Math.max(0, ...history.map((point) => point.input));
  let top = peak * 1.1;
  if (window > 0) top = limit === null || peak < limit * BARELY_USED ? Math.min(peak * (limit === null ? 1.1 : 1.5), window) : window;
  top = Math.max(top, peak, 1);
  const y = (tokens: number) => round(height - Math.min(1, Math.max(0, tokens) / top) * height);
  const step = width / Math.max(1, history.length);
  const columns = history.map((point, index) => ({ x: round(index * step), width: round(step), y: y(point.input), cachedY: y(point.cached) }));
  const steps = (key: "y" | "cachedY") => columns.map((column, index) => `V ${column[key]} H ${round((index + 1) * step)}`).join(" ");
  const empty = columns.length === 0;
  return {
    top,
    columns,
    line: empty ? "" : `M 0 ${columns[0]!.y} ${steps("y")}`,
    area: empty ? "" : `M 0 ${height} ${steps("y")} V ${height} Z`,
    cachedArea: empty ? "" : `M 0 ${height} ${steps("cachedY")} V ${height} Z`,
    compactions: columns.filter((_, index) => history[index]!.after_compaction).map((column) => column.x),
    limitY: limit !== null && limit <= top ? y(limit) : null,
    windowY: window > 0 && window <= top ? y(window) : null,
  };
}

function percent(fraction: number) {
  const value = fraction * 100;
  if (value > 0 && value < 0.1) return "<0.1%";
  return value < 10 ? `${value.toFixed(1)}%` : `${Math.round(value)}%`;
}

function element<Tag extends keyof HTMLElementTagNameMap>(builder: Builder, tag: Tag, className?: string, text?: string) {
  const node = builder.document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function svgElement(builder: Builder, tag: string, attributes: Record<string, string | number>) {
  const node = builder.document.createElementNS(SVG_NS, tag);
  for (const [name, value] of Object.entries(attributes)) node.setAttribute(name, String(value));
  return node;
}

/** A titled card; `meta` is a short muted figure beside the title. */
function card(builder: Builder, title: string, meta: string, modifier: string) {
  const section = element(builder, "section", `ctx-card ${modifier}`);
  const head = element(builder, "header", "ctx-card-head");
  head.append(element(builder, "h3", undefined, title));
  if (meta) head.append(element(builder, "span", "ctx-card-meta", meta));
  section.append(head);
  return section;
}

function muted(builder: Builder, text: string) {
  return element(builder, "p", "ctx-muted", text);
}

function swatch(builder: Builder, kind: ContextCategory) {
  const node = element(builder, "span", "ctx-swatch");
  node.dataset.kind = kind;
  return node;
}

/** The headline: how much of the window is in use and how far it is to compaction. */
function hero(builder: Builder, diagnostics: ContextDiagnostics) {
  const section = element(builder, "section", "ctx-hero");
  const window = diagnostics.model_window_tokens;
  const limit = diagnostics.auto_compact_token_limit;
  const active = diagnostics.active_tokens;
  if (active === null || window <= 0) {
    section.append(
      element(builder, "p", "ctx-hero-figure", window > 0 ? `${formatTokens(window)} token window` : "Window size unknown"),
      muted(builder, "The context size is unknown until a model call reports it."),
    );
    return section;
  }
  const level = limit !== null && active >= limit ? "over" : active >= (limit ?? window) * 0.9 ? "near" : "calm";
  section.dataset.level = level;
  const figure = element(builder, "p", "ctx-hero-figure");
  figure.append(
    element(builder, "strong", undefined, formatTokens(active)),
    ` of ${formatTokens(window)} tokens · `,
    element(builder, "span", "ctx-hero-percent", percent(active / window)),
  );
  const meter = element(builder, "div", "ctx-meter");
  meter.setAttribute("role", "meter");
  meter.setAttribute("aria-label", "Active context");
  meter.setAttribute("aria-valuemin", "0");
  meter.setAttribute("aria-valuemax", String(window));
  meter.setAttribute("aria-valuenow", String(Math.min(active, window)));
  meter.setAttribute("aria-valuetext", figure.textContent ?? "");
  const fill = element(builder, "span", "ctx-meter-fill");
  fill.style.width = `${Math.min(100, active / window * 100)}%`;
  meter.append(fill);
  if (limit !== null) {
    const marker = element(builder, "span", "ctx-meter-limit");
    marker.style.left = `${Math.min(100, limit / window * 100)}%`;
    marker.title = `Auto-compact at ${formatTokens(limit)}`;
    marker.append(element(builder, "span", "ctx-meter-limit-label", "auto-compact"));
    meter.append(marker);
  }
  const stats = element(builder, "dl", "ctx-hero-stats");
  const stat = (label: string, value: string) => {
    const group = element(builder, "div");
    group.append(element(builder, "dt", undefined, label), element(builder, "dd", undefined, value));
    stats.append(group);
  };
  stat("Headroom", `${formatTokens(Math.max(0, window - active))} tokens`);
  stat("Until auto-compact", limit === null ? "off" : active >= limit ? "due now" : `${formatTokens(limit - active)} tokens`);
  stat("Compactions", String(diagnostics.compactions_completed));
  section.append(figure, meter, stats);
  return section;
}

/** The categories as one stacked bar and an enumerated legend. */
function composition(builder: Builder, diagnostics: ContextDiagnostics) {
  const breakdown = diagnostics.breakdown;
  const section = card(builder, "Composition", breakdown ? `${formatTokens(breakdown.input_tokens)} input, latest call` : "", "ctx-wide");
  if (!breakdown) {
    section.append(muted(builder, diagnostics.usage
      ? "Breakdown unavailable for this model."
      : "Breakdown unavailable: no model call has reported token usage yet."));
    return section;
  }
  const categories = breakdown.categories;
  const tokens = categories.map((category) => category.tokens);
  const shares = shareTenths(tokens);
  const widths = segmentWidths(tokens, MINIMUM_SEGMENT);
  const bar = element(builder, "div", "ctx-stack");
  bar.setAttribute("role", "group");
  bar.setAttribute("aria-label", "Context composition");
  const table = element(builder, "table", "ctx-legend");
  const head = element(builder, "tr");
  for (const label of ["Source", "Tokens", "Share", "Items"]) head.append(element(builder, "th", undefined, label));
  table.append(element(builder, "thead"), element(builder, "tbody"));
  table.tHead!.append(head);
  categories.forEach((category, index) => {
    const { label, description } = CATEGORIES[category.kind] ?? CATEGORIES.other;
    const share = shareLabel(category.tokens, shares[index]!);
    if (category.tokens > 0) {
      const segment = element(builder, "span", "ctx-segment");
      segment.dataset.kind = category.kind;
      segment.style.width = `${widths[index]}%`;
      segment.title = `${label} · ${formatTokens(category.tokens)} tokens · ${share}`;
      segment.setAttribute("role", "img");
      segment.setAttribute("aria-label", segment.title);
      bar.append(segment);
    }
    const row = element(builder, "tr");
    if (category.tokens === 0) row.dataset.empty = "";
    const name = element(builder, "td", "ctx-legend-name");
    name.title = description;
    name.append(swatch(builder, category.kind), label);
    row.append(
      name,
      element(builder, "td", undefined, formatTokens(category.tokens)),
      element(builder, "td", undefined, share),
      element(builder, "td", undefined, String(category.items)),
    );
    table.tBodies[0]!.append(row);
  });
  if (bar.childElementCount === 0) bar.dataset.empty = "";
  section.append(
    bar,
    table,
    muted(builder, "The total is the exact input of the latest call. The shares are estimates: each measured growth between calls is split by size among the items that caused it."),
  );
  return section;
}

/** Tools shown before the rest fold away; the list is already ordered by output tokens. */
const VISIBLE_TOOLS = 8;

/** One tool as a single line: name, share of the largest tool as a bar, call count, and total. */
function toolRow(builder: Builder, tool: ContextBreakdown["tools"][number], largest: number) {
  const item = element(builder, "li");
  item.title = `${tool.calls} ${tool.calls === 1 ? "call" : "calls"} · ${formatTokens(tool.call_tokens)} in calls · ${formatTokens(tool.output_tokens)} output`;
  const bar = element(builder, "div", "ctx-tool-bar");
  for (const [kind, value] of [["tool_calls", tool.call_tokens], ["tool_output", tool.output_tokens]] as const) {
    if (value <= 0) continue;
    const segment = element(builder, "span", "ctx-segment");
    segment.dataset.kind = kind;
    segment.style.width = `${value / largest * 100}%`;
    bar.append(segment);
  }
  item.append(
    element(builder, "span", "ctx-tool-name", tool.name),
    bar,
    element(builder, "span", "ctx-tool-calls", `${tool.calls}×`),
    element(builder, "span", "ctx-row-figure", formatTokens(tool.call_tokens + tool.output_tokens)),
  );
  return item;
}

/** Per-tool bars: argument tokens and output tokens, scaled to the largest tool. */
function tools(builder: Builder, breakdown: ContextBreakdown) {
  const calls = breakdown.tools.reduce((sum, tool) => sum + tool.calls, 0);
  const section = card(builder, "Tools", calls ? `${calls} calls` : "", "ctx-tools");
  if (breakdown.tools.length === 0) {
    section.append(muted(builder, "No tool output in the active context."));
    return section;
  }
  const largest = Math.max(1, ...breakdown.tools.map((tool) => tool.call_tokens + tool.output_tokens));
  const list = (rows: ContextBreakdown["tools"]) => {
    const node = element(builder, "ul", "ctx-tool-list");
    node.append(...rows.map((tool) => toolRow(builder, tool, largest)));
    return node;
  };
  section.append(list(breakdown.tools.slice(0, VISIBLE_TOOLS)));
  const rest = breakdown.tools.slice(VISIBLE_TOOLS);
  if (rest.length > 0) {
    const more = element(builder, "details", "ctx-more");
    more.append(element(builder, "summary", undefined, `${rest.length} more ${rest.length === 1 ? "tool" : "tools"}`), list(rest));
    section.append(more);
  }
  return section;
}


/** The single largest items, ranked. */
function largestItems(builder: Builder, breakdown: ContextBreakdown) {
  const section = card(builder, "Largest items", "", "ctx-largest");
  if (breakdown.largest.length === 0) {
    section.append(muted(builder, "No items in the active context yet."));
    return section;
  }
  const list = element(builder, "ol", "ctx-largest-list");
  for (const item of breakdown.largest) {
    const row = element(builder, "li");
    const name = element(builder, "span", "ctx-largest-name");
    name.append(swatch(builder, item.kind), (CATEGORIES[item.kind] ?? CATEGORIES.other).label);
    if (item.tool !== null) name.append(element(builder, "span", "ctx-tool-name", item.tool));
    row.append(
      name,
      element(builder, "span", "ctx-largest-turn", `turn ${item.turn}`),
      element(builder, "span", "ctx-row-figure", formatTokens(item.tokens)),
      element(builder, "span", "ctx-largest-share", breakdown.input_tokens > 0 ? percent(item.tokens / breakdown.input_tokens) : ""),
    );
    list.append(row);
  }
  section.append(list);
  return section;
}

/** Input size per call as a step chart, with compactions, the auto-compact limit, and the window. */
function growth(builder: Builder, diagnostics: ContextDiagnostics) {
  const history = diagnostics.history;
  const section = card(builder, "Growth", history.length ? `last ${history.length} ${history.length === 1 ? "call" : "calls"}` : "", "ctx-wide ctx-growth");
  if (history.length === 0) {
    section.append(muted(builder, "No call history: the model has not reported per-call usage yet."));
    return section;
  }
  const chart = growthChart(history, diagnostics.model_window_tokens, diagnostics.auto_compact_token_limit, CHART_WIDTH, CHART_HEIGHT);
  const latest = history[history.length - 1]!;
  const frame = element(builder, "div", "ctx-chart");
  const svg = svgElement(builder, "svg", { viewBox: `0 0 ${CHART_WIDTH} ${CHART_HEIGHT}`, preserveAspectRatio: "none", role: "img", tabindex: 0,
    "aria-label": `Input tokens over the last ${history.length} calls, now ${formatTokens(latest.input)}, peak ${formatTokens(Math.max(...history.map((point) => point.input)))}` });
  const rule = (y: number, className: string) => svgElement(builder, "line", { x1: 0, x2: CHART_WIDTH, y1: y, y2: y, class: className, "vector-effect": "non-scaling-stroke" });
  if (chart.windowY !== null) svg.append(rule(chart.windowY, "ctx-chart-window"));
  if (chart.limitY !== null) svg.append(rule(chart.limitY, "ctx-chart-limit"));
  svg.append(
    svgElement(builder, "path", { d: chart.area, class: "ctx-chart-area" }),
    svgElement(builder, "path", { d: chart.cachedArea, class: "ctx-chart-cached" }),
    svgElement(builder, "path", { d: chart.line, class: "ctx-chart-line", "vector-effect": "non-scaling-stroke" }),
  );
  for (const x of chart.compactions) {
    svg.append(svgElement(builder, "line", { x1: x, x2: x, y1: 0, y2: CHART_HEIGHT, class: "ctx-chart-compaction", "vector-effect": "non-scaling-stroke" }));
  }
  chart.columns.forEach((column, index) => {
    const point = history[index]!;
    const hit = svgElement(builder, "rect", { x: column.x, y: 0, width: column.width, height: CHART_HEIGHT, class: "ctx-chart-hit" });
    const title = svgElement(builder, "title", {});
    title.textContent = [
      `Call ${point.call}`,
      `${formatTokens(point.input)} input (${formatTokens(point.cached)} cached)`,
      `${formatTokens(point.output)} output`,
      point.after_compaction ? "after compaction" : "",
    ].filter(Boolean).join(" · ");
    hit.append(title);
    svg.append(hit);
  });
  const scale = element(builder, "div", "ctx-chart-scale");
  scale.append(element(builder, "span", undefined, formatTokens(Math.round(chart.top))), element(builder, "span", undefined, "0"));
  frame.append(svg, scale);
  const axis = element(builder, "div", "ctx-chart-axis");
  axis.append(element(builder, "span", undefined, `call ${history[0]!.call}`), element(builder, "span", undefined, `call ${latest.call}`));
  const key = element(builder, "ul", "ctx-chart-key");
  const entries: [string, string][] = [["input", "Input"], ["cached", "Cached"]];
  if (chart.compactions.length) entries.push(["compaction", "Compaction"]);
  if (chart.limitY !== null) entries.push(["limit", "Auto-compact"]);
  if (chart.windowY !== null) entries.push(["window", "Window"]);
  for (const [kind, label] of entries) {
    const entry = element(builder, "li", undefined, label);
    entry.dataset.key = kind;
    key.append(entry);
  }
  section.append(frame, axis, key);
  return section;
}

/** How much of the input the provider served from its prompt cache. */
function cache(builder: Builder, diagnostics: ContextDiagnostics) {
  const section = card(builder, "Prompt cache", diagnostics.prompt_cache === false ? "off" : "", "ctx-wide ctx-cache");
  const usage = diagnostics.usage;
  if (!usage) {
    section.append(muted(builder, "No model call has reported cache usage yet."));
    return section;
  }
  const fraction = usage.input > 0 ? usage.cached_input / usage.input : 0;
  const bar = element(builder, "div", "ctx-cache-bar");
  const fill = element(builder, "span");
  fill.style.width = `${fraction * 100}%`;
  bar.title = `${formatTokens(usage.cached_input)} cached · ${formatTokens(usage.uncached_input)} uncached`;
  bar.append(fill);
  const summary = element(builder, "p", "ctx-cache-summary");
  summary.append(
    element(builder, "strong", undefined, percent(fraction)),
    ` of the latest call's input was cached: ${formatTokens(usage.cached_input)} cached, ${formatTokens(usage.uncached_input)} uncached.`,
  );
  const input = diagnostics.history.reduce((sum, point) => sum + point.input, 0);
  const cached = diagnostics.history.reduce((sum, point) => sum + point.cached, 0);
  section.append(bar, summary);
  if (input > 0) {
    const calls = diagnostics.history.length;
    section.append(muted(builder, `Average over the last ${calls} ${calls === 1 ? "call" : "calls"}: ${percent(cached / input)} cached.`));
  }
  return section;
}

/** Every raw figure, for when the visual summary is not enough. */
function details(builder: Builder, diagnostics: ContextDiagnostics) {
  const window = diagnostics.model_window_tokens;
  const active = diagnostics.active_tokens;
  const usage = diagnostics.usage;
  const last = diagnostics.last_compaction;
  const entries: [string, string][] = [
    ["Model window", `${formatTokens(window)} tokens`],
    ["Auto-compact at", diagnostics.auto_compact_token_limit === null ? "off" : `${formatTokens(diagnostics.auto_compact_token_limit)} tokens`],
    ["Active context", active === null ? "unknown" : `${formatTokens(active)} tokens`],
  ];
  if (usage) {
    entries.push(
      ["Last request input", `${formatTokens(usage.input)} (${formatTokens(usage.cached_input)} cached, ${formatTokens(usage.uncached_input)} uncached)`],
      ["Last request output", formatTokens(usage.output)],
      ["Last request total", formatTokens(usage.total)],
    );
  }
  entries.push(
    ["Continuation", diagnostics.continuation?.replace("_", " ") ?? "unknown"],
    ["Prompt cache", diagnostics.prompt_cache === null ? "unknown" : diagnostics.prompt_cache ? "on" : "off"],
    ["Compactions", `${diagnostics.compactions_completed} completed of ${diagnostics.compactions_started} started`],
  );
  if (last) {
    const age = formatAge(last.started_at_unix_ms, builder.now);
    entries.push(["Last compaction", [
      last.trigger,
      age === "now" ? "just now" : `${age} ago`,
      last.before_tokens !== null && last.after_tokens !== null ? `${formatTokens(last.before_tokens)} → ${formatTokens(last.after_tokens)}` : "",
      last.completed_at_unix_ms === null ? "running" : "",
    ].filter(Boolean).join(" · ")]);
  }
  const section = element(builder, "details", "ctx-details ctx-wide");
  section.append(element(builder, "summary", undefined, "Details"));
  const list = element(builder, "dl", "facts");
  for (const [label, value] of entries) list.append(element(builder, "dt", undefined, label), element(builder, "dd", undefined, value));
  section.append(list);
  return section;
}

/**
 * Builds the whole context view. It is a pure function of the diagnostics, the document to build
 * in, and the clock, so it renders the same in a test document as in the page.
 */
export function contextSheet(diagnostics: ContextDiagnostics, builder: Builder = { document, now: Date.now() }): HTMLElement {
  const root = element(builder, "div", "context-sheet");
  root.append(hero(builder, diagnostics), growth(builder, diagnostics), composition(builder, diagnostics));
  if (diagnostics.breakdown) root.append(tools(builder, diagnostics.breakdown), largestItems(builder, diagnostics.breakdown));
  root.append(cache(builder, diagnostics), details(builder, diagnostics));
  return root;
}
