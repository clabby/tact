import { describeError, type ApiClient } from "../core/api-client";
import { isActive, layoutAgents, NODE_HEIGHT, NODE_WIDTH } from "./agent-graph";
import { Transcript } from "./chat";
import { createSphere } from "../ui/dot-sphere";
import { modelColor } from "../core/format";
import { glyph } from "../ui/glyphs";
import { openSheet } from "../ui/sheet";
import { transcriptData, upsert, type SessionView } from "../core/store";
import type { Theme } from "../core/theme";
import { toast } from "../ui/toast";
import type { Subagent } from "../core/wire";

/** A live subagent viewer; the app forwards roster, entry, and theme changes. */
export type SubagentsView = {
  rosterChanged(): void;
  entryChanged(agent: number, entry: number): void;
  themeChanged(): void;
};

const SVG = "http://www.w3.org/2000/svg";

/**
 * The subagent popup: the hierarchy as a graph, and beside it the selected agent's transcript in
 * the same renderer the main chat uses (scrolling, following, tool details).
 */
export function openSubagents(
  api: ApiClient,
  session: () => SessionView | null,
  theme: () => Theme,
  onClose: () => void,
): SubagentsView {
  const sheet = openSheet("Subagents", { size: "xl" });
  sheet.actions.innerHTML = `<label class="max-agents">Max <input type="number" min="1" step="1" inputmode="numeric" aria-label="Maximum subagents"></label>`;
  sheet.body.innerHTML = `<div class="agents">
    <section class="agent-graph-pane" aria-label="Subagent hierarchy">
      <header class="pane-head"><strong>Hierarchy</strong><span class="agent-summary"></span>
        <div class="layout-toggle agent-filter" role="group" aria-label="Show subagents">
          <button type="button" data-filter="active">Active</button><button type="button" data-filter="all">All</button>
        </div>
      </header>
      <div class="agent-graph-scroll"><div class="agent-graph"></div></div>
    </section>
    <section class="agent-view">
      <header class="agent-head"></header>
      <div class="agent-scroller"></div>
      <button type="button" class="jump-latest" hidden>${glyph("arrow-down")}Latest</button>
    </section>
  </div>`;
  const graph = sheet.body.querySelector<HTMLElement>(".agent-graph")!;
  const summary = sheet.body.querySelector<HTMLElement>(".agent-summary")!;
  const head = sheet.body.querySelector<HTMLElement>(".agent-head")!;
  const max = sheet.actions.querySelector("input")!;
  const transcript = new Transcript(
    sheet.body.querySelector(".agent-scroller")!,
    sheet.body.querySelector(".agent-view .jump-latest")!,
    theme,
    { title: "Pick a subagent", body: "Its transcript streams here while it works." },
  );
  const panes = sheet.body.querySelector<HTMLElement>(".agents")!;
  /** As in the terminal, the graph starts on active agents; "all" also shows settled ones. */
  let filter: "active" | "all" = "active";
  let selected: number | null = null;
  const setView = (view: "tree" | "split") => {
    panes.dataset.view = view;
  };
  setView("tree");
  const syncFilter = () => {
    for (const button of sheet.body.querySelectorAll<HTMLElement>("[data-filter]")) {
      button.setAttribute("aria-pressed", String(button.dataset.filter === filter));
    }
  };
  for (const button of sheet.body.querySelectorAll<HTMLElement>("[data-filter]")) {
    button.addEventListener("click", () => {
      filter = button.dataset.filter as typeof filter;
      syncFilter();
      renderGraph();
    });
  }
  syncFilter();
  let closed = false;
  sheet.onClose(() => {
    closed = true;
    onClose();
  });

  max.addEventListener("change", async () => {
    const limit = Number(max.value);
    if (!Number.isInteger(limit) || limit < 1) return;
    try {
      await api.command("set_max_subagents", { limit });
    } catch (error) {
      toast(describeError(error), "warning");
    }
  });

  const roster = () => session()?.subagents ?? { max_subagents: 0, agents: [] };

  const select = async (agent: Subagent) => {
    const current = session();
    if (!current) return;
    selected = agent.id;
    setView("split");
    renderGraph();
    renderHead();
    if (!current.agents.has(agent.id)) {
      try {
        const { entries } = await api.agentEntries(current.id, agent.id);
        const data = current.agents.get(agent.id) ?? transcriptData([]);
        for (const entry of entries) upsert(data, entry);
        current.agents.set(agent.id, data);
      } catch (error) {
        toast(describeError(error), "warning");
        return;
      }
    }
    if (closed || selected !== agent.id) return;
    transcript.show({
      key: `${current.id}/${agent.id}`,
      data: current.agents.get(agent.id)!,
      detail: (entry) => api.toolDetail(current.id, entry, agent.id),
    });
  };

  const renderGraph = () => {
    const { max_subagents, agents } = roster();
    if (document.activeElement !== max) max.value = String(max_subagents || "");
    const active = agents.filter(isActive).length;
    summary.textContent = agents.length
      ? `${agents.length} agent${agents.length === 1 ? "" : "s"}${active ? ` · ${active} running` : ""}`
      : "";
    const shown = filter === "all" ? agents : agents.filter(isActive);
    const layout = layoutAgents(shown);
    if (layout.nodes.length === 0) {
      graph.style.width = graph.style.height = "";
      const empty = document.createElement("div");
      empty.className = "agent-empty";
      empty.innerHTML = agents.length
        ? `<p>No subagents are running.</p><button type="button" class="button">Show all</button>`
        : `<p>No subagents in this session.</p>`;
      empty.querySelector("button")?.addEventListener("click", () => {
        filter = "all";
        syncFilter();
        renderGraph();
      });
      graph.replaceChildren(empty);
      return;
    }
    graph.style.width = `${layout.width}px`;
    graph.style.height = `${layout.height}px`;
    const svg = document.createElementNS(SVG, "svg");
    svg.setAttribute("width", String(layout.width));
    svg.setAttribute("height", String(layout.height));
    svg.setAttribute("aria-hidden", "true");
    const byId = new Map(layout.nodes.map((node) => [node.agent.id, node]));
    for (const edge of layout.edges) {
      const from = byId.get(edge.from)!;
      const to = byId.get(edge.to)!;
      const x1 = from.x + NODE_WIDTH / 2;
      const y1 = from.y + NODE_HEIGHT;
      const x2 = to.x + NODE_WIDTH / 2;
      const middle = (y1 + to.y) / 2;
      const path = document.createElementNS(SVG, "path");
      path.setAttribute("d", `M ${x1} ${y1} C ${x1} ${middle}, ${x2} ${middle}, ${x2} ${to.y}`);
      path.setAttribute("class", `agent-edge${isActive(to.agent) ? " active" : ""}`);
      svg.append(path);
    }
    const nodes = layout.nodes.map((node) => {
      const { agent } = node;
      const button = document.createElement("button");
      button.type = "button";
      button.className = "agent-node";
      button.dataset.state = agent.status.state;
      button.dataset.id = String(agent.id);
      button.setAttribute("aria-pressed", String(agent.id === selected));
      button.style.cssText = `left:${node.x}px;top:${node.y}px;width:${NODE_WIDTH}px;height:${NODE_HEIGHT}px`;
      button.title = agent.status.state === "failed" && "error" in agent.status ? agent.status.error : agent.task;
      button.innerHTML = `<span class="node-top"><span class="node-state" aria-hidden="true"></span><span class="node-role"></span><span class="node-id"></span></span>
        <span class="node-model"><span class="model-dot"></span><span class="node-model-text"></span></span>
        <span class="node-status"></span>`;
      if (isActive(agent)) button.querySelector(".node-state")!.append(createSphere(18, 36));
      button.querySelector(".node-role")!.textContent = agent.role;
      button.querySelector(".node-id")!.textContent = `#${agent.id}`;
      button.querySelector<HTMLElement>(".model-dot")!.style.background = modelColor(agent.model);
      button.querySelector(".node-model-text")!.textContent = `${agent.model} · ${agent.thinking}`;
      button.querySelector(".node-status")!.textContent = agent.status.state;
      button.addEventListener("click", () => void select(agent));
      return button;
    });
    graph.replaceChildren(svg, ...nodes);
  };

  const renderHead = () => {
    const agent = roster().agents.find((candidate) => candidate.id === selected);
    if (!agent) {
      head.innerHTML = "";
      return;
    }
    head.innerHTML = `<div class="agent-title"><button type="button" class="icon-button small agent-back" aria-label="Back to the hierarchy" title="Back to the hierarchy">${glyph("arrow-left")}</button><strong></strong><span class="status-pill"></span></div>
      <div class="agent-sub"><span class="model-dot"></span><span class="agent-sub-text"></span></div>
      <details class="agent-task"><summary>Task</summary><p></p></details>`;
    head.dataset.state = agent.status.state;
    head.querySelector(".agent-back")!.addEventListener("click", () => setView("tree"));
    head.querySelector("strong")!.textContent = agent.role;
    const pill = head.querySelector<HTMLElement>(".status-pill")!;
    pill.textContent = agent.status.state;
    pill.dataset.state = agent.status.state;
    head.querySelector<HTMLElement>(".model-dot")!.style.background = modelColor(agent.model);
    head.querySelector(".agent-sub-text")!.textContent = `${agent.model} · ${agent.thinking} · #${agent.id}${agent.parent === null ? "" : ` · from #${agent.parent}`}`;
    head.querySelector(".agent-task p")!.textContent = agent.task;
    if (agent.status.state === "failed") {
      const error = document.createElement("p");
      error.className = "agent-error";
      error.textContent = agent.status.error;
      head.append(error);
    } else if (agent.status.state === "completed" && agent.status.output !== null && agent.status.output !== undefined) {
      const result = document.createElement("details");
      result.className = "agent-task";
      result.innerHTML = `<summary>Result</summary><p></p>`;
      const output = agent.status.output;
      result.querySelector("p")!.textContent = typeof output === "string" ? output : JSON.stringify(output, null, 2);
      head.append(result);
    }
  };

  renderGraph();
  transcript.show(null);
  return {
    rosterChanged: () => {
      if (closed) return;
      if (selected !== null && !roster().agents.some((agent) => agent.id === selected)) {
        selected = null;
        transcript.show(null);
      }
      renderGraph();
      renderHead();
    },
    entryChanged: (agent, entry) => {
      if (!closed && agent === selected) transcript.entryChanged(entry);
    },
    themeChanged: () => {
      if (!closed) transcript.rerender();
    },
  };
}

