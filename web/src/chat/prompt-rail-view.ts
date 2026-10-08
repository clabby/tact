import { activeIndex, lineLength, MAX_LINES, promptLabel, windowFor } from "./prompt-rail";
import type { TranscriptData } from "../core/store";

type Prompt = { id: number; text: string };

/** Pixels below the transcript's top edge that count as "where the reader is". */
const READING_LINE = 96;

/**
 * A minimap of the session's own prompts, docked on the right edge: one tick per prompt, the
 * current one highlighted. Pointing at it unfolds the ticks into a list of prompt previews;
 * choosing one scrolls the transcript there. Long sessions show a window of the list that pages.
 */
export class PromptRail {
  private readonly nav: HTMLElement;
  private prompts: Prompt[] = [];
  private active = 0;
  private manualStart: number | null = null;
  /** The prompt just chosen, kept current until the reader scrolls on their own. */
  private chosen: number | null = null;
  private frame = 0;

  constructor(host: HTMLElement, private readonly scroller: HTMLElement, private readonly data: () => TranscriptData | null) {
    const layer = document.createElement("div");
    layer.className = "rail-layer";
    this.nav = document.createElement("nav");
    this.nav.className = "prompt-rail";
    this.nav.setAttribute("aria-label", "Prompts in this session");
    layer.append(this.nav);
    host.append(layer);
    scroller.addEventListener("scroll", () => this.schedule(), { passive: true });
    for (const gesture of ["wheel", "touchstart", "keydown"]) {
      scroller.addEventListener(gesture, () => { this.chosen = null; }, { passive: true });
    }
    this.nav.addEventListener("click", (event) => this.click(event.target as HTMLElement));
  }

  /** Re-reads the prompts after the transcript changed. */
  refresh() {
    this.schedule();
  }

  private schedule() {
    this.frame ||= requestAnimationFrame(() => {
      this.frame = 0;
      this.update();
    });
  }

  private update() {
    const data = this.data();
    const prompts = (data?.order ?? []).flatMap((id) => {
      const entry = data!.entries.get(id);
      return entry?.kind === "user" && entry.parent === null ? [{ id, text: entry.text }] : [];
    });
    const changed = prompts.length !== this.prompts.length || prompts.some((prompt, index) => prompt.id !== this.prompts[index]!.id || prompt.text !== this.prompts[index]!.text);
    this.prompts = prompts;
    const active = this.measureActive();
    const activeMoved = active !== this.active;
    this.active = active;
    if (activeMoved) this.manualStart = null;
    const { start, end } = windowFor(prompts.length, active, MAX_LINES, this.manualStart);
    const shown = this.nav.dataset.window;
    if (changed || shown !== `${start}:${end}`) this.render(start, end);
    this.markActive();
  }

  private measureActive() {
    if (this.chosen !== null && this.chosen < this.prompts.length) return this.chosen;
    const top = this.scroller.getBoundingClientRect().top;
    const tops = this.prompts.map((prompt) => {
      const element = this.scroller.querySelector<HTMLElement>(`.entry[data-id="${prompt.id}"]`);
      return element ? element.getBoundingClientRect().top - top : Number.POSITIVE_INFINITY;
    });
    // At the very end the last prompt is the current one even when it cannot reach the reading line.
    const atEnd = this.scroller.scrollHeight - this.scroller.scrollTop - this.scroller.clientHeight < 8;
    const lastVisible = tops.length > 0 && tops[tops.length - 1]! < this.scroller.clientHeight;
    return atEnd && lastVisible ? tops.length - 1 : activeIndex(tops, READING_LINE);
  }

  private render(start: number, end: number) {
    this.nav.dataset.window = `${start}:${end}`;
    this.nav.hidden = this.prompts.length < 2;
    const items: HTMLElement[] = [];
    if (start > 0) items.push(this.more(-1, `${start} earlier`));
    for (const [offset, prompt] of this.prompts.slice(start, end).entries()) {
      const index = start + offset;
      const item = document.createElement("button");
      item.type = "button";
      item.className = "rail-item";
      item.dataset.index = String(index);
      item.innerHTML = `<span class="rail-label"><span class="rail-num"></span><span class="rail-text"></span></span><span class="rail-line"></span>`;
      item.querySelector(".rail-num")!.textContent = String(index + 1);
      item.querySelector(".rail-text")!.textContent = promptLabel(prompt.text);
      item.querySelector<HTMLElement>(".rail-line")!.style.width = `${lineLength(prompt.text)}px`;
      item.setAttribute("aria-label", `Prompt ${index + 1}: ${promptLabel(prompt.text)}`);
      items.push(item);
    }
    if (end < this.prompts.length) items.push(this.more(1, `${this.prompts.length - end} later`));
    this.nav.replaceChildren(...items);
  }

  private more(direction: -1 | 1, label: string) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "rail-more";
    button.dataset.direction = String(direction);
    button.innerHTML = `<span class="rail-label"></span><span class="rail-dots" aria-hidden="true">···</span>`;
    button.querySelector(".rail-label")!.textContent = label;
    button.setAttribute("aria-label", label);
    return button;
  }

  private markActive() {
    for (const item of this.nav.querySelectorAll<HTMLElement>(".rail-item")) {
      const current = Number(item.dataset.index) === this.active;
      if (current) item.setAttribute("aria-current", "true");
      else item.removeAttribute("aria-current");
    }
  }

  private click(target: HTMLElement) {
    const more = target.closest<HTMLElement>(".rail-more");
    if (more) {
      const { start } = windowFor(this.prompts.length, this.active, MAX_LINES, this.manualStart);
      this.manualStart = start + Number(more.dataset.direction) * (MAX_LINES - 2);
      this.update();
      return;
    }
    const item = target.closest<HTMLElement>(".rail-item");
    if (!item) return;
    const prompt = this.prompts[Number(item.dataset.index)];
    const element = prompt && this.scroller.querySelector<HTMLElement>(`.entry[data-id="${prompt.id}"]`);
    if (!element) return;
    this.chosen = Number(item.dataset.index);
    this.update();
    const offset = element.getBoundingClientRect().top - this.scroller.getBoundingClientRect().top;
    this.scroller.scrollTo({ top: this.scroller.scrollTop + offset - 24, behavior: "smooth" });
    const bubble = element.querySelector(".user-bubble");
    bubble?.classList.remove("rail-flash");
    void (bubble as HTMLElement | null)?.offsetWidth;
    bubble?.classList.add("rail-flash");
  }
}

