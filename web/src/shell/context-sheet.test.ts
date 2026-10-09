import { describe, expect, test } from "bun:test";
import { Window } from "happy-dom";
import type { ContextBreakdown, ContextCategory, ContextDiagnostics } from "../core/wire";
import { contextSheet, growthChart, segmentWidths, shareLabel, shareTenths } from "./context-sheet";

const NOW = 1_800_000_000_000;
const KINDS: ContextCategory[] = ["prefix", "user", "assistant", "reasoning", "tool_calls", "tool_output", "compacted", "other"];

function breakdown(tokens: number[], toolName = "read_file"): ContextBreakdown {
  return {
    input_tokens: tokens.reduce((sum, value) => sum + value, 0),
    categories: KINDS.map((kind, index) => ({ kind, tokens: tokens[index] ?? 0, items: tokens[index] ? index + 1 : 0 })),
    tools: [
      { name: toolName, calls: 4, call_tokens: 300, output_tokens: 40_000 },
      { name: "shell", calls: 1, call_tokens: 120, output_tokens: 2_000 },
    ],
    largest: [{ kind: "tool_output", tool: toolName, turn: 3, tokens: 25_000 }, { kind: "user", tool: null, turn: 1, tokens: 900 }],
  };
}

function diagnostics(overrides: Partial<ContextDiagnostics> = {}): ContextDiagnostics {
  return {
    model_window_tokens: 272_000,
    auto_compact_token_limit: 244_800,
    active_tokens: 120_000,
    usage: { input: 118_000, cached_input: 100_000, uncached_input: 18_000, output: 2_000, total: 120_000 },
    continuation: "previous_response",
    prompt_cache: true,
    compactions_started: 2,
    compactions_completed: 1,
    last_compaction: { trigger: "automatic", started_at_unix_ms: NOW - 3_600_000, completed_at_unix_ms: NOW - 3_590_000, before_tokens: 240_000, after_tokens: 30_000 },
    breakdown: breakdown([12_000, 3_000, 5_000, 0, 1_000, 96_950, 0, 50]),
    history: [
      { call: 7, input: 40_000, cached: 30_000, output: 900, after_compaction: false },
      { call: 8, input: 60_000, cached: 40_000, output: 1_200, after_compaction: false },
      { call: 9, input: 118_000, cached: 100_000, output: 2_000, after_compaction: true },
    ],
    ...overrides,
  };
}

function render(value: ContextDiagnostics) {
  const document = new Window().document as unknown as Document;
  const root = contextSheet(value, { document, now: NOW });
  document.body.append(root);
  return root;
}

describe("context sheet", () => {
  test("enumerates every category and draws only the nonzero ones", () => {
    const root = render(diagnostics());
    const rows = [...root.querySelectorAll(".ctx-legend tbody tr")];
    expect(rows).toHaveLength(KINDS.length);
    expect(rows.map((row) => row.querySelector(".ctx-swatch")!.getAttribute("data-kind"))).toEqual(KINDS);
    expect(rows.filter((row) => row.hasAttribute("data-empty"))).toHaveLength(2);
    const segments = [...root.querySelectorAll<HTMLElement>(".ctx-stack .ctx-segment")];
    expect(segments.map((segment) => segment.dataset.kind)).toEqual(["prefix", "user", "assistant", "tool_calls", "tool_output", "other"]);
    for (const segment of segments) {
      expect(segment.getAttribute("aria-label")).toBe(segment.title);
      expect(parseFloat(segment.style.width)).toBeGreaterThanOrEqual(1.5);
    }
    expect(segments.find((segment) => segment.dataset.kind === "other")!.title).toContain("<0.1%");
    expect(root.querySelector(".ctx-stack")!.closest(".ctx-card")!.querySelector(".ctx-card-meta")!.textContent).toBe("118k input, latest call");
    expect(root.textContent).toContain("estimates");
  });

  test("shows the meter, tools, largest items, history, cache, and every detail row", () => {
    const root = render(diagnostics());
    expect(root.querySelector(".ctx-hero-figure")!.textContent).toBe("120k of 272k tokens · 44%");
    expect(root.querySelector(".ctx-meter")!.getAttribute("aria-valuenow")).toBe("120000");
    expect(root.querySelector<HTMLElement>(".ctx-meter-limit")!.style.left).toBe("90%");
    expect(root.querySelector(".ctx-hero-stats")!.textContent).toContain("Until auto-compact125k tokens");
    expect([...root.querySelectorAll(".ctx-tool-list .ctx-tool-name")].map((name) => name.textContent)).toEqual(["read_file", "shell"]);
    expect(root.querySelectorAll(".ctx-largest-list li")).toHaveLength(2);
    expect(root.querySelectorAll(".ctx-chart-hit")).toHaveLength(3);
    expect(root.querySelector(".ctx-chart-hit title")!.textContent).toBe("Call 7 · 40k input (30k cached) · 900 output");
    expect(root.querySelectorAll(".ctx-chart-compaction")).toHaveLength(1);
    expect(root.querySelector(".ctx-cache-summary strong")!.textContent).toBe("85%");
    expect([...root.querySelectorAll(".ctx-details dt")].map((term) => term.textContent)).toEqual([
      "Model window", "Auto-compact at", "Active context", "Last request input", "Last request output",
      "Last request total", "Continuation", "Prompt cache", "Compactions", "Last compaction",
    ]);
    expect(root.querySelector(".ctx-details dd:last-of-type")!.textContent).toBe("automatic · 1h ago · 240k → 30k");
  });

  test("without a breakdown it says so and keeps the meter and the details", () => {
    const root = render(diagnostics({ breakdown: null, history: [] }));
    expect(root.querySelector(".ctx-meter")).not.toBeNull();
    expect(root.textContent).toContain("Breakdown unavailable for this model.");
    expect(root.textContent).toContain("No call history");
    expect(root.querySelector(".ctx-stack, .ctx-legend, .ctx-tools, .ctx-largest, svg")).toBeNull();
    expect(root.querySelectorAll(".ctx-details dt")).toHaveLength(10);

    const fresh = render(diagnostics({ breakdown: null, history: [], usage: null, active_tokens: null, last_compaction: null }));
    expect(fresh.querySelector(".ctx-meter")).toBeNull();
    expect(fresh.textContent).toContain("no model call has reported token usage yet");
    expect(fresh.textContent).toContain("No model call has reported cache usage yet.");
    expect(fresh.textContent).not.toMatch(/\b0%/);
  });

  test("the growth chart follows the meter and the tools fold after the first eight", () => {
    const many = breakdown([1_000, 1_000]);
    many.tools = Array.from({ length: 12 }, (_, index) => ({ name: `tool_${index}`, calls: 1, call_tokens: 10, output_tokens: 1_000 - index }));
    const root = render(diagnostics({ breakdown: many }));
    const at = (match: (child: Element) => boolean) => [...root.children].findIndex(match);
    const hero = at((child) => child.classList.contains("ctx-hero"));
    expect(at((child) => child.classList.contains("ctx-growth"))).toBe(hero + 1);
    expect(at((child) => child.querySelector(".ctx-legend") !== null)).toBe(hero + 2);
    const visible = root.querySelector(".ctx-tools > .ctx-tool-list")!;
    expect(visible.children).toHaveLength(8);
    const more = root.querySelector(".ctx-more")!;
    expect(more.querySelector("summary")!.textContent).toBe("4 more tools");
    expect(more.querySelectorAll("li")).toHaveLength(4);
    expect(more.hasAttribute("open")).toBe(false);
    expect(render(diagnostics()).querySelector(".ctx-more")).toBeNull();
  });

  test("tool names are text, never markup", () => {
    const hostile = "<img src=x onerror=1>";
    const root = render(diagnostics({ breakdown: breakdown([1_000, 1_000], hostile) }));
    expect(root.querySelector("img")).toBeNull();
    expect([...root.querySelectorAll(".ctx-tool-name")].map((name) => name.textContent)).toContain(hostile);
  });
});

describe("shares", () => {
  test("tenths add up to a whole and never round a nonzero share away silently", () => {
    const values = [12_000, 3_000, 5_000, 0, 1_000, 96_950, 0, 50];
    const tenths = shareTenths(values);
    expect(tenths.reduce((sum, share) => sum + share, 0)).toBe(1000);
    expect(tenths[3]).toBe(0);
    expect(values.map((value, index) => shareLabel(value, tenths[index]!))).toEqual(["10.2%", "2.5%", "4.2%", "—", "0.9%", "82.2%", "—", "<0.1%"]);
    expect(shareTenths([1, 1, 1])).toEqual([334, 333, 333]);
    expect(shareTenths([0, 0])).toEqual([0, 0]);
  });

  test("segment widths keep small shares visible and fill the bar", () => {
    const widths = segmentWidths([1_000_000, 1, 0, 2], 2);
    expect(widths[1]).toBe(2);
    expect(widths[2]).toBe(0);
    expect(widths[3]).toBe(2);
    expect(widths.reduce((sum, width) => sum + width, 0)).toBeCloseTo(100);
    expect(segmentWidths([3, 1], 2)).toEqual([75, 25]);
    expect(segmentWidths([0, 0], 2)).toEqual([0, 0]);
  });
});

describe("growth chart", () => {
  const point = (input: number, cached: number, after_compaction = false) => ({ call: 0, input, cached, output: 0, after_compaction });

  test("steps one column per call against the window", () => {
    const chart = growthChart([point(100, 50), point(200, 100, true)], 400, 360, 200, 100);
    expect(chart.top).toBe(400);
    expect(chart.columns.map((column) => [column.x, column.width])).toEqual([[0, 100], [100, 100]]);
    expect(chart.line).toBe("M 0 75 V 75 H 100 V 50 H 200");
    expect(chart.area).toBe("M 0 100 V 75 H 100 V 50 H 200 V 100 Z");
    expect(chart.cachedArea).toBe("M 0 100 V 87.5 H 100 V 75 H 200 V 100 Z");
    expect(chart.compactions).toEqual([100]);
    expect(chart.limitY).toBe(10);
    expect(chart.windowY).toBe(0);
  });

  test("without a limit it scales a little above the peak, capped at the window", () => {
    const chart = growthChart([point(100, 0), point(300, 0)], 1_000, null, 100, 100);
    expect(chart.top).toBeCloseTo(330);
    expect(chart.windowY).toBeNull();
    expect(chart.limitY).toBeNull();
    expect(chart.columns[1]!.y).toBe(9.09);
    expect(growthChart([point(390, 0)], 400, null, 100, 100).top).toBe(400);
    // A barely used context scales to its data and leaves the distant limit off the chart.
    const early = growthChart([point(6_000, 0), point(9_000, 0)], 272_000, 244_800, 100, 100);
    expect(early.top).toBeCloseTo(13_500);
    expect(early.limitY).toBeNull();
  });

  test("without a window it scales to the peak and an empty history draws nothing", () => {
    expect(growthChart([point(1_000, 0)], 0, null, 100, 100).top).toBeCloseTo(1_100);
    const empty = growthChart([], 400, null, 100, 100);
    expect([empty.line, empty.area, empty.cachedArea]).toEqual(["", "", ""]);
    expect(empty.columns).toEqual([]);
  });
});
