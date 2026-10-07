import { FileDiff } from "@pierre/diffs";
import { ApiError, errorMessage } from "./api-client";
import { firstLine, formatDuration } from "./format";
import { glyph } from "./glyphs";
import { renderMarkdown } from "./markdown";
import type { TranscriptData } from "./store";
import type { Theme } from "./theme";
import { promptParts } from "./user-prompt";
import { presentDetail, TOOL_DEFAULT_OPEN, TOOL_LABELS } from "./tool-detail";
import type { ToolDetail, WireEntry } from "./wire";

type Rendered = { element: HTMLElement; revision: number };

type ToolEntry = Extract<WireEntry, { kind: "tool" }>;

/** What a transcript view shows: a session's or a subagent's entries, and where details live. */
export type TranscriptSource = {
  /** Identifies the transcript; a different key starts a fresh view. */
  key: string;
  data: TranscriptData;
  detail(entry: number): Promise<ToolDetail>;
  /** The URL of an image attached to a user entry, when this transcript has them. */
  image?(entry: number, index: number): string;
};

/** Distance from the bottom, in pixels, within which the transcript keeps following new output. */
const FOLLOW_THRESHOLD = 72;

/**
 * The transcript of the active session. DOM nodes are keyed by entry id and only entries whose
 * revision changed are re-rendered, batched into one animation frame. Markdown of settled entries
 * is rendered when the entry nears the viewport, so opening a long session costs little more than
 * its visible part.
 */
export class Transcript {
  private readonly list: HTMLElement;
  private rendered = new Map<number, Rendered>();
  private dirty = new Set<number>();
  private frame = 0;
  private source: TranscriptSource | null = null;
  private following = true;
  private readonly lazy: IntersectionObserver;
  /** Tool calls whose open state differs from their default, and patches shown untruncated. */
  private readonly toggled = new Set<number>();
  private readonly showAll = new Set<number>();
  private readonly diffs = new Map<number, FileDiff[]>();
  private readonly lazyDetail: IntersectionObserver;
  private readonly details = new Map<number, { revision: number; detail: Promise<ToolDetail> }>();

  constructor(
    private readonly scroller: HTMLElement,
    private readonly jump: HTMLButtonElement,
    private readonly theme: () => Theme,
    private readonly empty = { title: "No live session", body: "Start a new chat or pick one from the sidebar." },
  ) {
    this.list = document.createElement("div");
    this.list.className = "transcript";
    this.list.setAttribute("role", "log");
    this.list.setAttribute("aria-live", "off");
    this.list.setAttribute("aria-label", "Transcript");
    scroller.append(this.list);

    this.lazy = new IntersectionObserver((records) => {
      for (const record of records) {
        if (!record.isIntersecting) continue;
        const element = record.target as HTMLElement;
        this.lazy.unobserve(element);
        const entry = this.source?.data.entries.get(Number(element.closest<HTMLElement>(".entry")?.dataset.id));
        if (entry) this.renderBody(element, entry, true);
      }
    }, { root: scroller, rootMargin: "1200px 0px" });

    this.lazyDetail = new IntersectionObserver((records) => {
      for (const record of records) {
        if (!record.isIntersecting) continue;
        const container = record.target as HTMLElement;
        this.lazyDetail.unobserve(container);
        const entry = this.source?.data.entries.get(Number(container.closest<HTMLElement>(".entry")?.dataset.id));
        if (entry?.kind === "tool") void this.renderDetail(container, entry);
      }
    }, { root: scroller, rootMargin: "1200px 0px" });

    // Following stops only when the reader scrolls up, so programmatic and smooth scrolls toward
    // the end never cancel it; reaching the end again resumes it.
    let lastTop = 0;
    scroller.addEventListener("scroll", () => {
      const top = scroller.scrollTop;
      const distance = scroller.scrollHeight - top - scroller.clientHeight;
      if (distance < FOLLOW_THRESHOLD) this.setFollowing(true);
      else if (top < lastTop) this.setFollowing(false);
      lastTop = top;
    }, { passive: true });
    // Late layout (Markdown upgrades, highlighting, images) must not leave a follower short of the end.
    new ResizeObserver(() => {
      if (this.following) scroller.scrollTop = scroller.scrollHeight;
    }).observe(this.list);
    jump.addEventListener("click", () => this.scrollToEnd("smooth"));
    this.list.addEventListener("click", (event) => this.handleClick(event));
  }

  /** Shows `source`, reusing nodes whose revision is unchanged when it is the same transcript. */
  show(source: TranscriptSource | null) {
    const same = source !== null && source.key === this.source?.key;
    this.source = source;
    if (!same) {
      this.rendered.clear();
      this.toggled.clear();
      this.showAll.clear();
      for (const id of [...this.diffs.keys()]) this.releaseDiffs(id);
      this.lazyDetail.disconnect();
      this.details.clear();
      this.lazy.disconnect();
      this.list.replaceChildren();
      this.following = true;
    }
    if (!source) {
      this.list.replaceChildren(emptyState(this.empty.title, this.empty.body));
      return;
    }
    const data = source.data;
    for (const [id, rendered] of this.rendered) {
      if (!data.entries.has(id)) {
        rendered.element.remove();
        this.rendered.delete(id);
      }
    }
    for (const id of data.order) this.dirty.add(id);
    this.flush();
    if (!same) this.scrollToEnd("instant");
  }

  entryChanged(id: number) {
    this.dirty.add(id);
    if (!this.frame) this.frame = requestAnimationFrame(() => this.flush());
  }

  /** Re-renders settled Markdown, e.g. after the theme changed code highlighting. */
  rerender() {
    if (!this.source) return;
    for (const rendered of this.rendered.values()) rendered.revision = -1;
    this.show(this.source);
  }

  private flush() {
    cancelAnimationFrame(this.frame);
    this.frame = 0;
    const data = this.source?.data;
    if (!data || this.dirty.size === 0) return;
    if (this.list.querySelector(".empty-state")) this.list.replaceChildren();

    const dirty = [...this.dirty].sort((a, b) => a - b);
    this.dirty.clear();
    for (const id of dirty) {
      const entry = data.entries.get(id);
      if (!entry) continue;
      const held = this.rendered.get(id);
      if (held && held.revision === entry.revision) continue;
      const element = held?.element ?? document.createElement("article");
      this.renderEntry(element, entry);
      this.rendered.set(id, { element, revision: entry.revision });
      if (!held) this.place(element, id, data);
    }
    if (this.following) this.scrollToEnd("instant");
    else this.jump.hidden = false;
  }

  /** Inserts a new node in transcript order; appending is the common case. */
  private place(element: HTMLElement, id: number, data: TranscriptData) {
    const index = data.order.indexOf(id);
    for (let next = index + 1; next < data.order.length; next += 1) {
      const sibling = this.rendered.get(data.order[next]!);
      if (sibling?.element.isConnected) {
        this.list.insertBefore(element, sibling.element);
        return;
      }
    }
    this.list.append(element);
  }

  private renderEntry(element: HTMLElement, entry: WireEntry) {
    element.className = `entry entry-${entry.kind}${entry.parent === null ? "" : " nested"}`;
    element.dataset.id = String(entry.id);
    switch (entry.kind) {
      case "user": {
        const bubble = child(element, "div", "user-bubble");
        const image = this.source?.image;
        bubble.replaceChildren(...promptParts(entry.text, image ? entry.images ?? 0 : 0).map((part) => {
          const block = document.createElement("div");
          if (part.kind === "text") {
            block.className = "user-text";
            block.textContent = part.text;
          } else {
            block.className = "user-image";
            const picture = document.createElement("img");
            picture.alt = part.marker;
            picture.loading = "lazy";
            picture.src = image!(entry.id, part.index);
            block.append(picture);
          }
          return block;
        }));
        return;
      }
      case "assistant": {
        element.classList.toggle("commentary", entry.commentary);
        element.classList.toggle("streaming", !entry.complete);
        this.renderBody(child(element, "div", "markdown"), entry, !entry.complete);
        return;
      }
      case "reasoning": {
        let details = element.querySelector<HTMLDetailsElement>(":scope > details");
        if (!details) {
          details = document.createElement("details");
          details.className = "reasoning";
          details.innerHTML = `<summary>${glyph("brain")}<span class="reasoning-label">Thought</span><span class="reasoning-preview"></span>${glyph("chevron-right", "glyph chevron")}</summary><div class="markdown reasoning-body"></div>`;
          element.replaceChildren(details);
        }
        details.querySelector(".reasoning-preview")!.textContent = firstLine(entry.text);
        const body = details.querySelector<HTMLElement>(".reasoning-body")!;
        if (details.open) this.renderBody(body, entry, true);
        else body.dataset.stale = "true";
        return;
      }
      case "tool":
        this.renderTool(element, entry);
        return;
      case "directed_message": {
        element.innerHTML = `<div class="directed"><div class="directed-route">${glyph("message")}<span></span></div><div class="markdown"></div></div>`;
        element.querySelector(".directed-route span")!.textContent = `${entry.from} → ${entry.to} · ${entry.delivery}`;
        this.renderBody(element.querySelector<HTMLElement>(".markdown")!, entry, false);
        return;
      }
      case "turn_completed":
        marker(element, `Worked for ${formatDuration(entry.duration_ns)}`);
        return;
      case "context_compacted":
        marker(element, `Context compacted · ${formatDuration(entry.duration_ns)}`, "compact");
        return;
      case "interrupted":
        notice(element, "Interrupted", entry.count > 1 ? `${entry.count} pending prompts discarded` : "", "warning");
        return;
      case "compaction_failed":
        notice(element, "Compaction failed", entry.message, "danger");
        return;
      case "error":
        notice(element, "Error", entry.message, "danger");
        return;
      case "effort_changed":
        quiet(element, `Effort set to ${entry.to}`);
        return;
      case "fast_mode_changed":
        quiet(element, entry.enabled ? "Fast mode on" : "Fast mode off");
        return;
      case "forked_from":
        quiet(element, `Forked from ${entry.session.slice(0, 8)}`);
        return;
      case "reflection_started":
        quiet(element, "Reflecting on the session");
        return;
      default: {
        const kind = (entry as { kind: string }).kind;
        element.className = "entry entry-unknown";
        quiet(element, `Unsupported entry · ${kind}`);
      }
    }
  }

  /**
   * Renders an entry's Markdown body. Streaming text renders at once without highlighting;
   * settled text shows as plain text until it nears the viewport.
   */
  private renderBody(container: HTMLElement, entry: WireEntry, now: boolean) {
    const text = "text" in entry ? entry.text : "body" in entry ? entry.body : "";
    if (!now) {
      container.textContent = text;
      container.classList.add("plain");
      this.lazy.observe(container);
      return;
    }
    container.classList.remove("plain");
    delete container.dataset.stale;
    const streaming = entry.kind === "assistant" && !entry.complete;
    void renderMarkdown(container, text, this.theme() === "dark" ? "pierre-dark" : "pierre-light", {
      imageSource: localImageSource,
      highlight: !streaming,
      placeholder: streaming ? "" : " ",
    });
  }

  private isOpen(entry: ToolEntry) {
    return TOOL_DEFAULT_OPEN.has(entry.name) !== this.toggled.has(entry.id);
  }

  private renderTool(element: HTMLElement, entry: ToolEntry) {
    const open = this.isOpen(entry);
    element.classList.toggle("open", open);
    element.dataset.state = entry.state;
    element.dataset.tool = entry.name;
    this.releaseDiffs(entry.id);
    const extra = [
      entry.child_count ? `${entry.child_count} agent${entry.child_count === 1 ? "" : "s"}` : "",
      entry.substeps.length ? `${entry.substeps.length} step${entry.substeps.length === 1 ? "" : "s"}` : "",
    ].filter(Boolean).join(" · ");
    element.innerHTML = `<button class="tool-row" type="button" aria-expanded="${open}">
      <span class="tool-state" aria-label="${entry.state}"></span>
      <span class="tool-name"></span>
      <span class="tool-summary"></span>
      <span class="tool-meta"></span>
      ${glyph("chevron-right", "glyph chevron")}
    </button>`;
    const name = element.querySelector<HTMLElement>(".tool-name")!;
    name.textContent = TOOL_LABELS[entry.name] ?? entry.name;
    name.title = entry.name;
    element.querySelector(".tool-summary")!.textContent = entry.summary;
    element.querySelector(".tool-meta")!.textContent = [extra, entry.duration_ns === null ? "" : formatDuration(entry.duration_ns)]
      .filter(Boolean).join(" · ");
    if (!open) return;
    const body = child(element, "div", "tool-body");
    if (entry.substeps.length) {
      const steps = child(body, "ol", "tool-steps");
      for (const step of entry.substeps) child(steps, "li", "").textContent = step;
    }
    if (entry.has_detail) {
      const detail = child(body, "div", "tool-io");
      detail.innerHTML = `<div class="tool-loading"><span class="spinner"></span>Loading</div>`;
      this.lazyDetail.observe(detail);
    }
  }

  private async renderDetail(container: HTMLElement, entry: ToolEntry) {
    const source = this.source;
    if (!source) return;
    let cached = this.details.get(entry.id);
    if (!cached || cached.revision !== entry.revision) {
      cached = { revision: entry.revision, detail: source.detail(entry.id) };
      this.details.set(entry.id, cached);
    }
    try {
      const detail = await cached.detail;
      if (!container.isConnected) return;
      presentDetail(container, entry.name, detail, {
        theme: this.theme(),
        full: this.showAll.has(entry.id),
        track: (instances) => this.diffs.set(entry.id, instances),
        toggleFull: () => {
          if (!this.showAll.delete(entry.id)) this.showAll.add(entry.id);
          this.rerenderEntry(entry.id);
        },
      });
      const patch = container.querySelector<HTMLElement>(".patch");
      const meta = container.closest(".entry")?.querySelector(".tool-meta");
      if (patch && meta) {
        const stats = `<span class="add">+${patch.dataset.additions}</span><span class="del">−${patch.dataset.deletions}</span>`;
        meta.innerHTML = meta.textContent ? `${stats} · ${meta.innerHTML}` : stats;
      }
    } catch (error) {
      this.details.delete(entry.id);
      container.innerHTML = `<p class="tool-error"></p>`;
      container.firstElementChild!.textContent = error instanceof ApiError && error.status === 404
        ? "Details are no longer available."
        : `Could not load details: ${errorMessage(error)}`;
    }
  }

  private releaseDiffs(id: number) {
    for (const instance of this.diffs.get(id) ?? []) instance.cleanUp();
    this.diffs.delete(id);
  }

  private rerenderEntry(id: number) {
    const rendered = this.rendered.get(id);
    if (rendered) rendered.revision = -1;
    this.entryChanged(id);
  }

  private handleClick(event: MouseEvent) {
    const target = event.target as HTMLElement;
    const row = target.closest<HTMLElement>(".tool-row");
    if (row) {
      const id = Number(row.closest<HTMLElement>(".entry")!.dataset.id);
      if (!this.toggled.delete(id)) this.toggled.add(id);
      this.rerenderEntry(id);
      return;
    }
    const summary = target.closest("summary");
    if (summary) {
      // Render the reasoning body on first open; the toggle itself is native.
      const body = summary.parentElement!.querySelector<HTMLElement>(".reasoning-body")!;
      const entry = this.source?.data.entries.get(Number(summary.closest<HTMLElement>(".entry")!.dataset.id));
      if (entry && (body.dataset.stale || body.childElementCount === 0)) this.renderBody(body, entry, true);
    }
  }

  private setFollowing(following: boolean) {
    if (following === this.following) return;
    this.following = following;
    if (following) this.jump.hidden = true;
  }

  private scrollToEnd(behavior: ScrollBehavior) {
    this.following = true;
    this.jump.hidden = true;
    this.scroller.scrollTo({ top: this.scroller.scrollHeight, behavior });
  }
}

/**
 * Local image destinations (absolute, file://, or workspace-relative) are served by the instance;
 * remote ones are never loaded by the page.
 */
function localImageSource(destination: string) {
  if (!destination || /^([a-z][a-z0-9+.-]*:(?!\/\/\/)|\/\/)/i.test(destination)) return null;
  let path = destination;
  try {
    path = decodeURIComponent(destination);
  } catch {
    // A destination that is not percent-encoded is used as written.
  }
  return `./api/file?path=${encodeURIComponent(path)}`;
}

function child<K extends keyof HTMLElementTagNameMap>(parent: HTMLElement, tag: K, className: string) {
  const existing = className ? parent.querySelector<HTMLElementTagNameMap[K]>(`:scope > ${tag}.${className.split(" ")[0]}`) : null;
  if (existing) return existing;
  const element = document.createElement(tag);
  if (className) element.className = className;
  parent.append(element);
  return element;
}

function marker(element: HTMLElement, text: string, variant = "") {
  element.innerHTML = `<div class="marker ${variant}"><span></span></div>`;
  element.querySelector("span")!.textContent = text;
}

function quiet(element: HTMLElement, text: string) {
  element.innerHTML = `<p class="quiet"></p>`;
  element.firstElementChild!.textContent = text;
}

function notice(element: HTMLElement, title: string, body: string, tone: "warning" | "danger") {
  element.innerHTML = `<div class="notice ${tone}" role="note">${glyph("alert")}<div><strong></strong><p></p></div></div>`;
  element.querySelector("strong")!.textContent = title;
  const paragraph = element.querySelector("p")!;
  if (body) paragraph.textContent = body;
  else paragraph.remove();
}

function emptyState(title: string, body: string) {
  const element = document.createElement("div");
  element.className = "empty-state";
  element.innerHTML = `<div class="empty-mark">${glyph("sparkles")}</div><h2></h2><p></p>`;
  element.querySelector("h2")!.textContent = title;
  element.querySelector("p")!.textContent = body;
  return element;
}
