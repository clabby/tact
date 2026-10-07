// Sheets for terminal features that need more room than a palette entry: context diagnostics,
// memory, the configuration editor, reflection, and the subagent tree. Each reads through the
// generic query route and acts through the generic command route.

import { ApiError, describeError, type ApiClient } from "./api-client";
import { Transcript } from "./chat";
import { formatAge, formatTokens, modelColor } from "./format";
import { glyph } from "./glyphs";
import { openSheet, sheetMessage } from "./sheet";
import { transcriptData, upsert, type SessionView } from "./store";
import type { Theme } from "./theme";
import { toast } from "./toast";
import type { ContextDiagnostics, ListedMemory, Subagent } from "./wire";

function rows(container: HTMLElement, entries: [string, string][]) {
  const list = document.createElement("dl");
  list.className = "facts";
  for (const [label, value] of entries) {
    const term = document.createElement("dt");
    term.textContent = label;
    const description = document.createElement("dd");
    description.textContent = value;
    list.append(term, description);
  }
  container.append(list);
}

export async function openContextDiagnostics(api: ApiClient, session: string) {
  const sheet = openSheet("Context");
  sheetMessage(sheet.body, "Loading…");
  let diagnostics: ContextDiagnostics;
  try {
    diagnostics = await api.query("context_diagnostics", { session });
  } catch (error) {
    sheetMessage(sheet.body, describeError(error), "danger");
    return;
  }
  sheet.body.replaceChildren();
  const window = diagnostics.model_window_tokens;
  const active = diagnostics.active_tokens;
  if (active !== null && window > 0) {
    const ratio = Math.min(1, active / window);
    const meter = document.createElement("div");
    meter.className = "context-bar";
    meter.innerHTML = `<div class="context-bar-track"><span></span>${diagnostics.auto_compact_token_limit ? "<i></i>" : ""}</div><p></p>`;
    meter.querySelector<HTMLElement>("span")!.style.width = `${ratio * 100}%`;
    const limit = meter.querySelector<HTMLElement>("i");
    if (limit && diagnostics.auto_compact_token_limit) limit.style.left = `${Math.min(100, diagnostics.auto_compact_token_limit / window * 100)}%`;
    meter.querySelector("p")!.textContent = `${formatTokens(active)} of ${formatTokens(window)} tokens · ${Math.round(ratio * 100)}%`;
    sheet.body.append(meter);
  }
  const usage = diagnostics.usage;
  const last = diagnostics.last_compaction;
  rows(sheet.body, [
    ["Model window", `${formatTokens(window)} tokens`],
    ["Auto-compact at", diagnostics.auto_compact_token_limit === null ? "off" : `${formatTokens(diagnostics.auto_compact_token_limit)} tokens`],
    ["Active context", active === null ? "unknown" : `${formatTokens(active)} tokens`],
    ...(usage ? [
      ["Last request input", `${formatTokens(usage.input)} (${formatTokens(usage.cached_input)} cached, ${formatTokens(usage.uncached_input)} uncached)`],
      ["Last request output", formatTokens(usage.output)],
      ["Last request total", formatTokens(usage.total)],
    ] as [string, string][] : []),
    ["Continuation", diagnostics.continuation?.replace("_", " ") ?? "unknown"],
    ["Prompt cache", diagnostics.prompt_cache === null ? "unknown" : diagnostics.prompt_cache ? "on" : "off"],
    ["Compactions", `${diagnostics.compactions_completed} completed of ${diagnostics.compactions_started} started`],
    ...(last ? [[
      "Last compaction",
      [
        last.trigger,
        formatAge(last.started_at_unix_ms) === "now" ? "just now" : `${formatAge(last.started_at_unix_ms)} ago`,
        last.before_tokens !== null && last.after_tokens !== null ? `${formatTokens(last.before_tokens)} → ${formatTokens(last.after_tokens)}` : "",
        last.completed_at_unix_ms === null ? "running" : "",
      ].filter(Boolean).join(" · "),
    ]] as [string, string][] : []),
  ]);
}

type MemorySort = "updated" | "used" | "created";

export async function openMemories(api: ApiClient) {
  const sheet = openSheet("Memory", { wide: true });
  sheetMessage(sheet.body, "Loading…");
  let records: ListedMemory[] = [];
  let access = "";
  const fetchRecords = async () => {
    const reply = await api.query("memories");
    records = reply.records;
    access = [reply.access.source, reply.access.namespace, reply.access.role].filter(Boolean).join(" · ");
  };
  try {
    await fetchRecords();
  } catch (error) {
    sheetMessage(sheet.body, error instanceof ApiError && error.code === "disabled" ? "Memory is turned off in the configuration." : describeError(error), "danger");
    return;
  }
  sheet.body.innerHTML = `<div class="sheet-toolbar"><label class="field-search">${glyph("search")}<input type="search" placeholder="Filter memories" aria-label="Filter memories"></label>
    <select aria-label="Sort"><option value="updated">Recently updated</option><option value="used">Most used</option><option value="created">Oldest first</option></select></div>
    <p class="sheet-caption"></p><ul class="memory-list" role="list"></ul>`;
  const filter = sheet.body.querySelector("input")!;
  const sort = sheet.body.querySelector("select")!;
  const list = sheet.body.querySelector<HTMLElement>(".memory-list")!;
  const caption = sheet.body.querySelector<HTMLElement>(".sheet-caption")!;
  const render = () => {
    const query = filter.value.trim().toLowerCase();
    const order: Record<MemorySort, (a: ListedMemory, b: ListedMemory) => number> = {
      updated: (a, b) => b.updated_at_ms - a.updated_at_ms,
      used: (a, b) => b.use_count - a.use_count || b.updated_at_ms - a.updated_at_ms,
      created: (a, b) => a.created_at_ms - b.created_at_ms,
    };
    const shown = records.filter((record) => record.content.toLowerCase().includes(query)).sort(order[sort.value as MemorySort]);
    caption.textContent = `${shown.length} of ${records.length} memories · ${access}`;
    list.replaceChildren(...shown.map((record) => {
      const item = document.createElement("li");
      item.className = "memory";
      item.innerHTML = `<p class="memory-content"></p><div class="memory-meta"><span></span>${record.deletable ? `<button type="button" class="icon-button small" aria-label="Delete memory">${glyph("trash")}</button>` : ""}</div>`;
      const content = item.querySelector<HTMLElement>(".memory-content")!;
      content.textContent = record.content;
      content.addEventListener("click", () => content.classList.toggle("expanded"));
      item.querySelector(".memory-meta span")!.textContent = [
        `updated ${formatAge(record.updated_at_ms)}`,
        `used ${record.use_count}×`,
        `scanned ${record.scan_count}×`,
        record.key.namespace ?? "",
        record.probation_until_ms && record.probation_until_ms > Date.now() ? "on probation" : "",
      ].filter(Boolean).join(" · ");
      item.querySelector("button")?.addEventListener("click", async () => {
        if (!confirm("Delete this memory?")) return;
        try {
          await api.command("delete_memory", { key: record.key });
          records = records.filter((candidate) => candidate !== record);
          render();
        } catch (error) {
          if (!(error instanceof ApiError && error.code === "stale")) {
            toast(describeError(error), "danger");
            return;
          }
          toast("That memory changed since the list loaded. The list is refreshed.", "danger");
          await fetchRecords().then(render, (refresh) => toast(describeError(refresh), "danger"));
        }
      });
      return item;
    }));
  };
  filter.addEventListener("input", render);
  sort.addEventListener("change", render);
  render();
}

export async function openConfigEditor(api: ApiClient) {
  const sheet = openSheet("Configuration", { wide: true });
  sheet.actions.innerHTML = `<button type="button" class="button reload">Reload</button><button type="button" class="button primary save">Save</button>`;
  const save = sheet.actions.querySelector<HTMLButtonElement>(".save")!;
  const reload = sheet.actions.querySelector<HTMLButtonElement>(".reload")!;
  reload.title = "Reload the configuration file into Tact";
  sheetMessage(sheet.body, "Loading…");
  let revision = "";
  const load = async () => {
    const config = await api.query("config");
    revision = config.revision;
    sheet.body.innerHTML = `<p class="sheet-caption"></p><div class="sheet-notice" role="status" hidden></div><textarea class="config-text" spellcheck="false" aria-label="Configuration file"></textarea>`;
    sheet.body.querySelector(".sheet-caption")!.textContent = config.path;
    sheet.body.querySelector("textarea")!.value = config.text;
  };
  const notice = (text: string, tone: "danger" | "success") => {
    const element = sheet.body.querySelector<HTMLElement>(".sheet-notice")!;
    element.textContent = text;
    element.dataset.tone = tone;
    element.hidden = false;
  };
  try {
    await load();
  } catch (error) {
    const credentials = error instanceof ApiError && error.code === "not_available_remotely";
    sheetMessage(sheet.body, credentials
      ? "This configuration holds credentials, so it can only be edited in the terminal."
      : describeError(error), "danger");
    save.disabled = true;
    return;
  }
  save.addEventListener("click", async () => {
    const text = sheet.body.querySelector("textarea")!.value;
    save.disabled = true;
    try {
      await api.command("write_config", { text, revision });
      revision = (await api.query("config")).revision;
      notice("Saved. Tact reloaded the configuration.", "success");
    } catch (error) {
      notice(writeConfigError(error), "danger");
    } finally {
      save.disabled = false;
    }
  });
  reload.addEventListener("click", async () => {
    try {
      await api.command("reload_config");
      notice("Configuration reloaded.", "success");
    } catch (error) {
      notice(describeError(error), "danger");
    }
  });
}

function writeConfigError(error: unknown) {
  if (!(error instanceof ApiError)) return describeError(error);
  switch (error.code) {
    case "stale": return "The file changed on disk since it was opened. Copy your edits, close, and open it again.";
    case "not_available_remotely": return "Credentials can only be added in the terminal.";
    // The server's message says why the text does not load.
    case "invalid_request": return error.message;
    default: return describeError(error);
  }
}

export function openReflect(api: ApiClient, session: string) {
  const sheet = openSheet("Reflect");
  sheet.body.innerHTML = `<p class="sheet-caption">The agent reviews this session and records what it learned. Optionally steer what it should focus on.</p>
    <textarea class="reflect-text" rows="4" placeholder="Focus (optional)" aria-label="Reflection instructions"></textarea>
    <div class="sheet-footer"><button type="button" class="button primary">Reflect</button></div>`;
  const textarea = sheet.body.querySelector("textarea")!;
  textarea.focus();
  sheet.body.querySelector(".button")!.addEventListener("click", async () => {
    try {
      await api.command("reflect", { session, instructions: textarea.value.trim() });
      sheet.close();
    } catch (error) {
      toast(describeError(error), "warning");
    }
  });
}

/** A live subagent tree and transcript viewer; the app forwards roster and entry changes. */
export type SubagentsView = { rosterChanged(): void; entryChanged(agent: number, entry: number): void };

export function openSubagents(
  api: ApiClient,
  session: () => SessionView | null,
  theme: () => Theme,
  onClose: () => void,
): SubagentsView {
  const sheet = openSheet("Subagents", { wide: true });
  sheet.actions.innerHTML = `<label class="max-agents">Max <input type="number" min="1" step="1" inputmode="numeric" aria-label="Maximum subagents"></label>`;
  sheet.body.innerHTML = `<div class="agents"><ul class="agent-tree" role="tree" aria-label="Subagents"></ul><div class="agent-view"><div class="agent-head"></div><div class="agent-scroller"></div><button type="button" class="jump-latest" hidden>${glyph("arrow-down")}Latest</button></div></div>`;
  const tree = sheet.body.querySelector<HTMLElement>(".agent-tree")!;
  const head = sheet.body.querySelector<HTMLElement>(".agent-head")!;
  const max = sheet.actions.querySelector("input")!;
  const transcript = new Transcript(
    sheet.body.querySelector(".agent-scroller")!,
    sheet.body.querySelector(".agent-view .jump-latest")!,
    theme,
    { title: "Pick a subagent", body: "Its transcript streams here while it works." },
  );
  let selected: number | null = null;
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

  const showAgent = async (agent: Subagent) => {
    const current = session();
    if (!current) return;
    selected = agent.id;
    renderTree();
    head.innerHTML = `<strong></strong><span></span>`;
    head.querySelector("strong")!.textContent = agent.role;
    head.querySelector("span")!.textContent = agent.task;
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

  const renderTree = () => {
    const roster = session()?.subagents ?? { max_subagents: 0, agents: [] };
    if (document.activeElement !== max) max.value = String(roster.max_subagents || "");
    const ids = new Set(roster.agents.map((agent) => agent.id));
    const children = new Map<number | null, Subagent[]>();
    for (const agent of roster.agents) {
      const parent = agent.parent !== null && ids.has(agent.parent) ? agent.parent : null;
      children.set(parent, [...children.get(parent) ?? [], agent]);
    }
    const items: HTMLElement[] = [];
    const walk = (parent: number | null, depth: number) => {
      for (const agent of children.get(parent) ?? []) {
        const item = document.createElement("li");
        item.className = "agent-row";
        item.setAttribute("role", "treeitem");
        item.setAttribute("aria-selected", String(agent.id === selected));
        item.dataset.state = agent.status.state;
        item.style.setProperty("--depth", String(depth));
        item.innerHTML = `<button type="button"><span class="agent-state"></span><span class="agent-text"><span class="agent-role"></span><span class="agent-meta"><span class="model-dot"></span><span></span></span></span></button>`;
        item.querySelector(".agent-role")!.textContent = `${agent.role} · #${agent.id}`;
        item.querySelector(".agent-meta span:last-child")!.textContent = `${agent.model} · ${agent.thinking} · ${agent.status.state}`;
        item.querySelector<HTMLElement>(".model-dot")!.style.background = modelColor(agent.model);
        item.querySelector("button")!.title = agent.status.state === "failed" && "error" in agent.status ? agent.status.error : agent.task;
        item.querySelector("button")!.addEventListener("click", () => void showAgent(agent));
        items.push(item);
        walk(agent.id, depth + 1);
      }
    };
    walk(null, 0);
    if (items.length === 0) tree.innerHTML = `<li class="list-empty">No subagents in this session</li>`;
    else tree.replaceChildren(...items);
  };

  renderTree();
  transcript.show(null);
  return {
    rosterChanged: () => {
      if (!closed) renderTree();
    },
    entryChanged: (agent, entry) => {
      if (!closed && agent === selected) transcript.entryChanged(entry);
    },
  };
}
