// Sheets for terminal features that need more room than a palette entry: context diagnostics,
// memory, the configuration editor, reflection. Each reads through the
// generic query route and acts through the generic command route.

import { ApiError, describeError, type ApiClient } from "../core/api-client";
import { formatAge } from "../core/format";
import { glyph } from "../ui/glyphs";
import { contextSheet } from "./context-sheet";
import { qrSvg } from "./qr";
import { shareableOrigin, signInLink } from "./phone-link";
import { openSheet, sheetMessage } from "../ui/sheet";
import { toast } from "../ui/toast";
import type { ListedMemory } from "../core/wire";

export async function openContextDiagnostics(api: ApiClient, session: string) {
  const sheet = openSheet("Context", { wide: true });
  sheetMessage(sheet.body, "Loading…");
  try {
    sheet.body.replaceChildren(contextSheet(await api.query("context_diagnostics", { session })));
  } catch (error) {
    sheetMessage(sheet.body, describeError(error), "danger");
  }
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

/**
 * Shows a QR code that signs a phone in to this Tact. The address is the configured public origin,
 * else the origin this page was opened from; a link only this computer can reach is never shown.
 */
export async function openPhoneLink(api: ApiClient) {
  const sheet = openSheet("Open on your phone");
  sheetMessage(sheet.body, "Preparing the link…");
  try {
    const { public_origin, token } = await api.link();
    const origin = shareableOrigin(public_origin, location.origin);
    if (!origin) {
      sheet.body.innerHTML = `<div class="phone-link"><p class="phone-caption">This page is open at an address only this computer can reach (${location.host}), so a phone cannot use a link built from it. Open Tact through your tunnel's address (for example a Tailscale name) and try again, or set <code>web.tailscale</code> or <code>web.public_url</code>.</p></div>`;
      return;
    }
    const url = signInLink(origin, token);
    sheet.body.innerHTML = `<div class="phone-link"><div class="qr"></div>
      <p class="phone-origin"></p>
      <p class="phone-caption">Scan with your phone's camera. It signs the phone in to this Tact, so treat the code like a password.</p>
      <button type="button" class="button">Copy link</button></div>`;
    sheet.body.querySelector(".qr")!.append(qrSvg(url));
    sheet.body.querySelector(".phone-origin")!.textContent = origin;
    sheet.body.querySelector("button")!.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(url);
        toast("Copied the sign-in link. Share it carefully.");
      } catch {
        toast("Could not copy the link.", "warning");
      }
    });
  } catch (error) {
    sheetMessage(sheet.body, describeError(error), "danger");
  }
}
