import { describeError, type ApiClient } from "./api-client";
import { DraftSync } from "./draft";
import { effortColor, formatTokens, modelColor, statusLabel } from "./format";
import { glyph } from "./glyphs";
import { completeMention, findMention, type Mention } from "./mentions";
import { openMenu } from "./menu";
import type { SessionView } from "./store";
import type { CommandName, Commands, ModelCatalog, ModelInfo } from "./wire";

export type ComposerHost = {
  api: ApiClient;
  catalog(): ModelCatalog | null;
  /** Whether the active session is busy, from the live summary. */
  running(): boolean;
  openRecentPrompts(): void;
  openContext(): void;
};

type SessionCommand = Exclude<CommandName, "open_session" | "reload_config" | "write_config" | "delete_memory" | "set_max_subagents">;
type SessionArgs<Name extends SessionCommand> = Omit<Commands[Name], "session">;

const MIRROR_CUE_MS = 2200;
const MENTION_LIMIT = 8;

/**
 * The shared composer of the active session: the draft (kept in step with the terminal by
 * DraftSync) and its images, mentions, the queue, the live status line, settings, the context
 * meter, and send/queue/stop. A draft starting with "!" is a shell command, exactly as in the
 * terminal.
 */
export class Composer {
  private readonly form: HTMLFormElement;
  private readonly textarea: HTMLTextAreaElement;
  private readonly sendButton: HTMLButtonElement;
  private readonly stopButton: HTMLButtonElement;
  private readonly message: HTMLElement;
  private readonly queue: HTMLElement;
  private readonly status: HTMLElement;
  private readonly mirror: HTMLElement;
  private readonly images: HTMLElement;
  private readonly mentions: HTMLElement;
  private readonly meter: HTMLButtonElement;
  private readonly fileInput: HTMLInputElement;
  private readonly controls: Record<"model" | "effort" | "mode" | "speed", HTMLButtonElement>;
  private session: SessionView | null = null;
  private sync: DraftSync | null = null;
  private mirrorTimer = 0;
  private submitting = false;
  private editingQueue: number | null = null;
  private mention: { at: Mention; items: { label: string; detail: string; value: string }[]; selected: number } | null = null;
  private mentionRequest: AbortController | null = null;
  private mentionTimer = 0;

  constructor(private readonly root: HTMLElement, private readonly host: ComposerHost) {
    root.innerHTML = `
      <div class="status-line" hidden><span class="status-pulse" aria-hidden="true"></span><span class="status-text"></span></div>
      <ol class="queue" aria-label="Queued prompts" hidden></ol>
      <form class="composer" aria-label="Message composer">
        <ul class="mention-list" role="listbox" aria-label="Suggestions" hidden></ul>
        <div class="composer-message" role="status" hidden></div>
        <div class="composer-images" hidden></div>
        <textarea rows="1" aria-label="Message" placeholder="Message Tact" enterkeyhint="send" spellcheck="true" aria-autocomplete="list"></textarea>
        <div class="composer-bar">
          <div class="composer-settings">
            <button type="button" class="chip attach-chip" aria-label="Attach image" title="Attach image">${glyph("image")}</button>
            <span class="shell-badge" hidden>${glyph("terminal")}Shell</span>
            <button type="button" class="chip model-chip" aria-haspopup="menu" aria-label="Model"><span class="chip-dot"></span><span class="chip-label"></span>${glyph("chevron-down", "glyph chip-caret")}</button>
            <button type="button" class="chip effort-chip" aria-haspopup="menu" aria-label="Reasoning effort"><span class="chip-dot"></span><span class="chip-label"></span>${glyph("chevron-down", "glyph chip-caret")}</button>
            <button type="button" class="chip mode-chip" aria-pressed="false" aria-label="Pro reasoning" title="Pro reasoning">${glyph("sparkles")}<span class="chip-label">Pro</span></button>
            <button type="button" class="chip speed-chip" aria-haspopup="menu" aria-label="Speed">${glyph("bolt")}<span class="chip-label"></span></button>
            <span class="mirror-cue" aria-live="polite" hidden>${glyph("terminal")}From terminal</span>
          </div>
          <div class="composer-actions">
            <button type="button" class="context-meter" aria-label="Context usage" hidden><svg viewBox="0 0 20 20" aria-hidden="true"><circle cx="10" cy="10" r="8"/><circle class="meter-fill" cx="10" cy="10" r="8" pathLength="100"/></svg><span class="meter-label"></span></button>
            <button type="button" class="stop-button" aria-label="Stop" title="Stop (Esc)" hidden>${glyph("stop")}</button>
            <button type="submit" class="send-button" aria-label="Send" title="Send (Enter)">${glyph("arrow-up")}</button>
          </div>
        </div>
        <input type="file" accept="image/*" multiple hidden>
      </form>`;
    this.form = root.querySelector("form")!;
    this.textarea = root.querySelector("textarea")!;
    this.sendButton = root.querySelector(".send-button")!;
    this.stopButton = root.querySelector(".stop-button")!;
    this.message = root.querySelector(".composer-message")!;
    this.queue = root.querySelector(".queue")!;
    this.status = root.querySelector(".status-line")!;
    this.mirror = root.querySelector(".mirror-cue")!;
    this.images = root.querySelector(".composer-images")!;
    this.mentions = root.querySelector(".mention-list")!;
    this.meter = root.querySelector(".context-meter")!;
    this.fileInput = root.querySelector("input[type=file]")!;
    this.controls = {
      model: root.querySelector(".model-chip")!,
      effort: root.querySelector(".effort-chip")!,
      mode: root.querySelector(".mode-chip")!,
      speed: root.querySelector(".speed-chip")!,
    };
    this.bind();
  }

  /** Binds the composer to the displayed session, or disables it when there is none. */
  show(session: SessionView | null) {
    if (session && session.id === this.session?.id && this.sync) {
      this.session = session;
      this.sync.receive(session.draft);
    } else {
      void this.sync?.flush();
      this.sync?.dispose();
      this.session = session;
      this.sync = session ? this.createSync(session) : null;
      this.textarea.value = session?.draft.text ?? "";
      this.editingQueue = null;
      this.closeMentions();
      this.hideMessage();
    }
    this.root.classList.toggle("disabled", !session);
    this.textarea.disabled = !session;
    this.textChanged();
    this.settingsChanged();
    this.queueChanged();
    this.statusChanged();
    this.contextChanged();
  }

  draftChanged() {
    if (this.session && this.sync) this.sync.receive(this.session.draft);
    this.renderImages();
  }

  connectionOpened() {
    this.sync?.retry();
  }

  /** Appends Markdown (e.g. a composed review) to the draft after a blank line and focuses it. */
  append(markdown: string) {
    const current = this.textarea.value.replace(/\s+$/, "");
    this.replaceText(current ? `${current}\n\n${markdown.trim()}` : markdown.trim());
  }

  /** Replaces the whole draft, e.g. with a recent prompt. */
  replaceText(text: string) {
    if (!this.sync) return;
    this.textarea.value = text;
    this.sync.edit(text);
    this.textChanged();
    this.focus();
    this.textarea.setSelectionRange(text.length, text.length);
  }

  focus() {
    this.textarea.focus({ preventScroll: true });
  }

  settingsChanged() {
    const session = this.session;
    const model = this.model();
    const started = (session?.order.length ?? 0) > 0;
    const { model: modelChip, effort, mode, speed } = this.controls;
    this.chip(modelChip, model?.label ?? session?.model ?? "Model", session ? modelColor(session.model) : "var(--muted)");
    modelChip.disabled = !session || started;
    modelChip.title = started ? "The model is fixed after the first turn" : "Model";
    this.chip(effort, session?.effort ?? "effort", session ? effortColor(session.effort) : "var(--muted)");
    effort.disabled = !session || (started && (model?.effort_fixed_after_start ?? false));
    mode.hidden = !model?.reasoning_modes.includes("pro");
    mode.setAttribute("aria-pressed", String(session?.reasoningMode === "pro"));
    mode.disabled = !session;
    speed.querySelector(".chip-label")!.textContent = session?.speed === "standard" ? "Standard" : capitalize(session?.speed ?? "");
    speed.dataset.speed = session?.speed ?? "standard";
    speed.disabled = !session;
    this.controls.speed.hidden = (this.host.catalog()?.speeds.length ?? 0) < 2;
  }

  contextChanged() {
    const context = this.session?.context;
    this.meter.hidden = !context || context.window_tokens === 0;
    if (!context || context.window_tokens === 0) return;
    const ratio = Math.min(1, context.active_tokens / context.window_tokens);
    const percent = Math.round(ratio * 100);
    this.meter.querySelector<SVGCircleElement>(".meter-fill")!.style.strokeDasharray = `${percent} 100`;
    this.meter.querySelector(".meter-label")!.textContent = `${percent}%`;
    this.meter.dataset.level = ratio > 0.85 ? "high" : ratio > 0.6 ? "medium" : "low";
    const label = `${percent}% of context · ${formatTokens(context.active_tokens)} of ${formatTokens(context.window_tokens)} tokens`;
    this.meter.title = label;
    this.meter.setAttribute("aria-label", `Context usage: ${label}`);
  }

  queueChanged() {
    const items = this.session?.queue ?? [];
    if (this.editingQueue !== null && !items.some((item) => item.id === this.editingQueue)) this.editingQueue = null;
    this.queue.hidden = items.length === 0;
    this.queue.replaceChildren(...items.map((item) => {
      const row = document.createElement("li");
      row.className = `queue-item${item.steering ? " steering" : ""}`;
      row.innerHTML = `<span class="queue-badge"></span><span class="queue-text"></span>
        <button type="button" class="icon-button small steer" title="Steer into the running turn" aria-label="Steer">${glyph("steer")}</button>
        <button type="button" class="icon-button small edit" title="Edit" aria-label="Edit">${glyph("pencil")}</button>
        <button type="button" class="icon-button small remove" title="Remove from queue" aria-label="Remove">${glyph("close")}</button>`;
      row.querySelector(".queue-badge")!.textContent = item.steering ? "Steering" : "Queued";
      const text = row.querySelector<HTMLElement>(".queue-text")!;
      text.textContent = item.text;
      if (this.editingQueue === item.id) this.editQueued(row, text, item.id, item.text);
      const steer = row.querySelector<HTMLButtonElement>(".steer")!;
      steer.hidden = item.steering;
      steer.addEventListener("click", () => this.run("steer", { queue_id: item.id }));
      row.querySelector(".edit")!.addEventListener("click", () => this.editQueued(row, text, item.id, item.text));
      row.querySelector(".remove")!.addEventListener("click", () => this.run("dequeue", { queue_id: item.id }));
      return row;
    }));
  }

  /** Updates the status line and the controls that depend on whether a turn is running. */
  statusChanged() {
    const running = this.session !== null && this.host.running();
    const label = statusLabel(this.session?.status ?? null) ?? (running ? "Working" : null);
    this.status.hidden = label === null;
    this.status.classList.toggle("error", this.session?.status?.kind === "error");
    this.status.querySelector(".status-text")!.textContent = label ?? "";
    this.stopButton.hidden = !running;
    this.updateSendState();
  }

  private model(): ModelInfo | undefined {
    return this.host.catalog()?.models.find((model) => model.id === this.session?.model);
  }

  private createSync(session: SessionView) {
    const id = session.id;
    return new DraftSync(session.draft, {
      origin: this.host.api.origin,
      write: (text) => this.host.api.command("set_draft", { session: id, text }).then(() => {}),
      onRemote: (text, origin) => {
        const focused = document.activeElement === this.textarea;
        const atEnd = this.textarea.selectionStart === this.textarea.value.length;
        const caret = atEnd ? text.length : Math.min(this.textarea.selectionStart, text.length);
        this.textarea.value = text;
        if (focused) this.textarea.setSelectionRange(caret, caret);
        this.textChanged();
        if (origin === "terminal") this.cueMirror();
      },
      onError: (error) => this.showMessage(`Draft not saved: ${describeError(error)}`, "danger"),
    });
  }

  private bind() {
    this.form.addEventListener("submit", (event) => {
      event.preventDefault();
      void this.submit();
    });
    this.textarea.addEventListener("input", () => {
      if (!this.sync) return;
      this.hideMessage();
      if (!this.sync.isComposing) this.sync.edit(this.textarea.value);
      this.textChanged();
      this.updateMentions();
    });
    this.textarea.addEventListener("compositionstart", () => this.sync?.compositionStart());
    this.textarea.addEventListener("compositionend", () => this.sync?.compositionEnd(this.textarea.value));
    this.textarea.addEventListener("keydown", (event) => this.onKey(event));
    this.textarea.addEventListener("blur", () => setTimeout(() => this.closeMentions(), 120));
    this.textarea.addEventListener("paste", (event) => {
      const files = [...event.clipboardData?.files ?? []].filter((file) => file.type.startsWith("image/"));
      if (files.length === 0) return;
      event.preventDefault();
      void this.attach(files);
    });
    this.form.addEventListener("dragover", (event) => {
      if (!event.dataTransfer?.types.includes("Files")) return;
      event.preventDefault();
      this.form.classList.add("dropping");
    });
    this.form.addEventListener("dragleave", () => this.form.classList.remove("dropping"));
    this.form.addEventListener("drop", (event) => {
      this.form.classList.remove("dropping");
      const files = [...event.dataTransfer?.files ?? []].filter((file) => file.type.startsWith("image/"));
      if (files.length === 0) return;
      event.preventDefault();
      void this.attach(files);
    });
    this.root.querySelector(".attach-chip")!.addEventListener("click", () => this.fileInput.click());
    this.fileInput.addEventListener("change", () => {
      void this.attach([...this.fileInput.files ?? []]);
      this.fileInput.value = "";
    });
    this.mentions.addEventListener("pointerdown", (event) => {
      const item = (event.target as HTMLElement).closest<HTMLElement>("[data-index]");
      if (!item) return;
      event.preventDefault();
      this.acceptMention(Number(item.dataset.index));
    });
    this.meter.addEventListener("click", () => this.host.openContext());
    this.stopButton.addEventListener("click", () => this.run("interrupt", {}));
    this.bindSettings();
  }

  private onKey(event: KeyboardEvent) {
    if (this.mention && this.mention.items.length) {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const count = this.mention.items.length;
        this.mention.selected = (this.mention.selected + (event.key === "ArrowDown" ? 1 : -1) + count) % count;
        this.renderMentions();
        return;
      }
      if ((event.key === "Enter" || event.key === "Tab") && !event.isComposing) {
        event.preventDefault();
        this.acceptMention(this.mention.selected);
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        this.closeMentions();
        return;
      }
    }
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing && !event.altKey) {
      // Touch keyboards keep Enter for new lines; the send button submits.
      if (matchMedia("(pointer: coarse)").matches && !event.metaKey && !event.ctrlKey) return;
      event.preventDefault();
      void this.submit();
    } else if (event.key === "ArrowUp" && this.textarea.value === "") {
      event.preventDefault();
      this.host.openRecentPrompts();
    } else if (event.key === "Escape" && this.host.running() && this.textarea.value === "") {
      event.preventDefault();
      void this.run("interrupt", {});
    }
  }

  private bindSettings() {
    const { model, effort, mode, speed } = this.controls;
    model.addEventListener("click", () => {
      const session = this.session;
      const catalog = this.host.catalog();
      if (!session || !catalog) return;
      openMenu(model, catalog.models.map((candidate) => ({
        label: candidate.label,
        swatch: modelColor(candidate.id),
        checked: candidate.id === session.model,
        run: () => this.run("set_model", { model: candidate.id }),
      })), "Model");
    });
    effort.addEventListener("click", () => {
      const session = this.session;
      const catalog = this.host.catalog();
      if (!session || !catalog) return;
      openMenu(effort, catalog.efforts.map((candidate) => ({
        label: candidate,
        swatch: effortColor(candidate),
        checked: candidate === session.effort,
        run: () => this.run("set_effort", { effort: candidate }),
      })), "Reasoning effort");
    });
    mode.addEventListener("click", () => {
      if (this.session) void this.run("set_reasoning_mode", { mode: this.session.reasoningMode === "pro" ? "standard" : "pro" });
    });
    speed.addEventListener("click", () => {
      const session = this.session;
      const catalog = this.host.catalog();
      if (!session || !catalog) return;
      const effective = this.model()?.effective_speeds;
      openMenu(speed, catalog.speeds.map((candidate, index) => {
        const runs = effective?.[index];
        return {
          label: capitalize(candidate),
          detail: runs && runs !== candidate ? `runs ${runs}` : undefined,
          checked: candidate === session.speed,
          run: () => this.run("set_speed", { speed: candidate }),
        };
      }), "Speed");
    });
  }

  private async submit() {
    const sync = this.sync;
    const session = this.session;
    this.closeMentions();
    if (!sync || !session || this.submitting || !this.textarea.value.trim()) return;
    this.submitting = true;
    this.updateSendState();
    try {
      const rev = await sync.settledRev();
      if (this.sync !== sync) return;
      await this.host.api.command("submit", { session: session.id, rev });
      this.hideMessage();
    } catch (error) {
      this.showMessage(describeError(error), "warning");
    } finally {
      this.submitting = false;
      this.updateSendState();
    }
  }

  /** Attaches images: the loop appends each image's marker to the shared draft. */
  private async attach(files: File[]) {
    const session = this.session;
    const sync = this.sync;
    if (!session || !sync) return;
    // Pending text must reach the server first, or it would overwrite the inserted marker.
    await sync.flush();
    for (const file of files) {
      if (!file.type.startsWith("image/")) {
        this.showMessage(`${file.name} is not an image.`, "warning");
        continue;
      }
      try {
        const data_url = await new Promise<string>((resolve, reject) => {
          const reader = new FileReader();
          reader.onload = () => resolve(String(reader.result));
          reader.onerror = () => reject(reader.error);
          reader.readAsDataURL(file);
        });
        await this.host.api.command("attach_image", { session: session.id, data_url });
      } catch (error) {
        this.showMessage(`Could not attach ${file.name}: ${describeError(error)}`, "danger");
      }
    }
    this.focus();
  }

  private renderImages() {
    const session = this.session;
    const text = this.textarea.value;
    const images = session?.draft.images.filter((image) => text.includes(image.marker)) ?? [];
    this.images.hidden = images.length === 0;
    this.images.replaceChildren(...images.map((image) => {
      const figure = document.createElement("figure");
      figure.className = "draft-image";
      figure.innerHTML = `<img alt=""><figcaption></figcaption><button type="button" class="draft-image-remove" aria-label="Remove image">${glyph("close")}</button>`;
      figure.querySelector("img")!.src = image.data_url.startsWith("data:image/") ? image.data_url : "";
      figure.querySelector("img")!.alt = image.marker;
      figure.querySelector("figcaption")!.textContent = image.marker;
      figure.querySelector("button")!.addEventListener("click", () => {
        // Removing the marker drops the image from the shared draft.
        this.replaceText(this.textarea.value.replace(image.marker, "").replace(/ {2,}/g, " "));
      });
      return figure;
    }));
  }

  private updateMentions() {
    const at = findMention(this.textarea.value, this.textarea.selectionStart);
    if (!at || !this.session || this.textarea.selectionStart !== this.textarea.selectionEnd) {
      this.closeMentions();
      return;
    }
    clearTimeout(this.mentionTimer);
    this.mentionTimer = window.setTimeout(() => void this.loadMentions(at), 60);
  }

  private async loadMentions(at: Mention) {
    this.mentionRequest?.abort();
    const request = new AbortController();
    this.mentionRequest = request;
    try {
      const items = await this.mentionItems(at);
      if (this.mentionRequest !== request) return;
      this.mention = { at, items: items.slice(0, MENTION_LIMIT), selected: 0 };
      this.renderMentions();
    } catch {
      if (this.mentionRequest === request) this.closeMentions();
    }
  }

  private async mentionItems(at: Mention) {
    const api = this.host.api;
    switch (at.kind) {
      case "file":
        return (await api.query("files", { query: at.query })).paths.map((path) => ({ label: path, detail: "", value: path }));
      case "skill":
        return (await api.query("skills", { query: at.query })).skills.map((skill) => ({ label: skill.name, detail: skill.description, value: skill.name }));
      case "session":
        return (await api.query("history", { query: at.query })).sessions.map((session) => ({
          label: session.preview || session.session_id, detail: session.session_id.slice(0, 8), value: session.session_id,
        }));
    }
  }

  private renderMentions() {
    const mention = this.mention;
    this.mentions.hidden = !mention || mention.items.length === 0;
    this.textarea.setAttribute("aria-expanded", String(!this.mentions.hidden));
    if (!mention) return;
    this.mentions.replaceChildren(...mention.items.map((item, index) => {
      const row = document.createElement("li");
      row.className = "mention-item";
      row.id = `mention-${index}`;
      row.dataset.index = String(index);
      row.setAttribute("role", "option");
      row.setAttribute("aria-selected", String(index === mention.selected));
      row.innerHTML = `<span class="mention-label"></span><span class="mention-detail"></span>`;
      row.querySelector(".mention-label")!.textContent = item.label;
      row.querySelector(".mention-detail")!.textContent = item.detail;
      return row;
    }));
    this.textarea.setAttribute("aria-activedescendant", `mention-${mention.selected}`);
    this.mentions.querySelector("[aria-selected=true]")?.scrollIntoView({ block: "nearest" });
  }

  private acceptMention(index: number) {
    const mention = this.mention;
    const item = mention?.items[index];
    if (!mention || !item || !this.sync) return;
    const { text, caret } = completeMention(this.textarea.value, mention.at, item.value);
    this.closeMentions();
    this.textarea.value = text;
    this.textarea.setSelectionRange(caret, caret);
    this.sync.edit(text);
    this.textChanged();
    if (item.value.endsWith("/")) this.updateMentions();
  }

  private closeMentions() {
    clearTimeout(this.mentionTimer);
    this.mentionRequest?.abort();
    this.mentionRequest = null;
    this.mention = null;
    this.mentions.hidden = true;
    this.textarea.removeAttribute("aria-activedescendant");
    this.textarea.setAttribute("aria-expanded", "false");
  }

  private editQueued(row: HTMLElement, text: HTMLElement, id: number, original: string) {
    this.editingQueue = id;
    const input = document.createElement("input");
    input.className = "queue-edit";
    input.value = original;
    input.setAttribute("aria-label", "Edit queued prompt");
    text.replaceWith(input);
    row.classList.add("editing");
    input.focus();
    const finish = (save: boolean) => {
      if (this.editingQueue !== id) return;
      this.editingQueue = null;
      const value = input.value.trim();
      if (save && value && value !== original) void this.run("edit_queued", { queue_id: id, text: value });
      this.queueChanged();
    };
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && !event.isComposing) {
        event.preventDefault();
        finish(true);
      } else if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        finish(false);
      }
    });
    input.addEventListener("blur", () => finish(true));
  }

  /** Runs a command on the displayed session, reporting a refusal inline. */
  private async run<Name extends SessionCommand>(name: Name, args: SessionArgs<Name>) {
    const session = this.session;
    if (!session) return;
    try {
      await (this.host.api.command as (name: CommandName, args: object) => Promise<unknown>)(name, { session: session.id, ...args });
    } catch (error) {
      this.showMessage(describeError(error), "warning");
    }
  }

  /** Everything derived from the text: height, shell mode, images, send state. */
  private textChanged() {
    const shell = this.textarea.value.startsWith("!");
    this.form.classList.toggle("shell", shell);
    this.root.querySelector<HTMLElement>(".shell-badge")!.hidden = !shell;
    const running = this.host.running();
    this.textarea.placeholder = running ? "Queue a message" : "Message Tact";
    this.textarea.style.height = "auto";
    this.textarea.style.height = `${Math.min(this.textarea.scrollHeight, Math.round(innerHeight * 0.4))}px`;
    this.renderImages();
    this.updateSendState();
  }

  private chip(button: HTMLButtonElement, label: string, color: string) {
    button.querySelector(".chip-label")!.textContent = label;
    button.querySelector<HTMLElement>(".chip-dot")!.style.background = color;
  }

  private updateSendState() {
    const running = this.session !== null && this.host.running();
    this.sendButton.disabled = !this.session || this.submitting || !this.textarea.value.trim();
    this.sendButton.classList.toggle("busy", this.submitting);
    this.sendButton.setAttribute("aria-label", running ? "Queue" : "Send");
    this.sendButton.title = running ? "Queue (Enter)" : "Send (Enter)";
  }

  private cueMirror() {
    this.mirror.hidden = false;
    this.mirror.classList.remove("fade");
    clearTimeout(this.mirrorTimer);
    this.mirrorTimer = window.setTimeout(() => {
      this.mirror.classList.add("fade");
      this.mirrorTimer = window.setTimeout(() => { this.mirror.hidden = true; }, 400);
    }, MIRROR_CUE_MS);
  }

  private showMessage(text: string, tone: "warning" | "danger") {
    this.message.textContent = text;
    this.message.dataset.tone = tone;
    this.message.hidden = false;
  }

  private hideMessage() {
    this.message.hidden = true;
  }
}

function capitalize(text: string) {
  return text ? text[0]!.toUpperCase() + text.slice(1) : text;
}
