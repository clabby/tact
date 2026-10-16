import type { ApiClient } from "../core/api-client";
import { describeError } from "../core/api-client";
import { imageSource, isMarkdown, viewerUrl, type ViewerTarget } from "../core/local-files";
import { renderMarkdown, scrollToHeading } from "../core/markdown";
import { ThemeController } from "../core/theme";
import type { LocalDocument } from "../core/wire";
import { glyph } from "../ui/glyphs";
import "./file-viewer.css";

type Mode = "preview" | "source";

/**
 * Shows one local file in its own tab. Markdown renders as the transcript renders it and any other
 * file shows as numbered text. The file's own links and images resolve against its directory.
 */
export async function showFileViewer(root: HTMLElement, api: Pick<ApiClient, "document">, target: ViewerTarget) {
  const theme = new ThemeController();
  const name = target.path.split("/").filter(Boolean).pop() ?? target.path;
  document.title = `${name} · Tact`;
  root.innerHTML = `<div class="file-viewer">
    <header class="file-viewer-header">
      <div class="file-viewer-title"><h1></h1><p></p></div>
      <nav class="segmented compact" aria-label="Display" hidden>
        <button type="button" class="segment" data-mode="preview">Preview</button>
        <button type="button" class="segment" data-mode="source">Source</button>
      </nav>
    </header>
    <main class="file-viewer-body"><p class="file-viewer-loading">Loading…</p></main>
  </div>`;
  root.querySelector("h1")!.textContent = name;
  const subtitle = root.querySelector<HTMLElement>(".file-viewer-title p")!;
  subtitle.textContent = target.path;
  const body = root.querySelector<HTMLElement>(".file-viewer-body")!;

  let file: LocalDocument;
  try {
    file = await api.document(target.path, target.session);
  } catch (error) {
    showProblem(body, error);
    return;
  }
  subtitle.textContent = file.path;
  if (!isMarkdown(target.path)) {
    showSource(body, file.text, target.line);
    return;
  }

  // A line number points into the source, so it opens there rather than in the preview.
  let mode: Mode = target.line === null ? "preview" : "source";
  const modes = root.querySelector<HTMLElement>(".segmented")!;
  modes.hidden = false;
  const show = async (scroll: boolean) => {
    for (const button of modes.querySelectorAll<HTMLButtonElement>("button")) {
      button.setAttribute("aria-pressed", String(button.dataset.mode === mode));
    }
    if (mode === "source") {
      showSource(body, file.text, scroll ? target.line : null);
      return;
    }
    await showPreview(body, file, theme.current === "dark");
    if (scroll && target.fragment) scrollToHeading(body, target.fragment);
  };
  modes.addEventListener("click", (event) => {
    const chosen = (event.target as HTMLElement).closest<HTMLButtonElement>("button")?.dataset.mode as Mode | undefined;
    if (!chosen || chosen === mode) return;
    mode = chosen;
    void show(false);
  });
  theme.subscribe(() => {
    if (mode === "preview") void show(false);
  });
  await show(true);
}

/** Renders Markdown with links and images relative to the file, which are on the same machine. */
async function showPreview(body: HTMLElement, file: LocalDocument, dark: boolean) {
  const article = document.createElement("article");
  article.className = "markdown file-viewer-document";
  body.replaceChildren(article);
  const base = { directory: file.path.slice(0, file.path.lastIndexOf("/")) || "/" };
  await renderMarkdown(article, file.text.replace(/^\uFEFF/, ""), dark ? "pierre-dark" : "pierre-light", {
    imageSource: (destination) => imageSource(destination, base),
    fileLink: (destination) => viewerUrl(destination, base),
  });
}

/**
 * Shows text with line numbers, scrolled to `line` when given. The numbers are generated content,
 * so copying a selection copies only the file's text.
 */
function showSource(body: HTMLElement, text: string, line: number | null) {
  const lines = text.replace(/\r?\n$/, "").split(/\r?\n/);
  // Keep layout below browser scroll-height limits even for files containing mostly newlines.
  const pageSize = 10_000;
  const source = document.createElement("pre");
  source.className = "file-viewer-source";
  body.replaceChildren(source);
  let pages: HTMLElement | null = null;
  if (lines.length > pageSize) {
    pages = document.createElement("nav");
    pages.className = "file-viewer-pages";
    pages.setAttribute("aria-label", "Source pages");
    pages.innerHTML = '<button type="button">Previous</button><span></span><button type="button">Next</button>';
    body.prepend(pages);
  }
  const show = (start: number) => {
    const page = lines.slice(start, start + pageSize);
    source.style.setProperty("--digits", String(String(start + page.length).length));

    // One text layout and one generated gutter avoid a DOM row for every line.
    source.dataset.numbers = page.map((_, index) => start + index + 1).join("\n");
    const content = page.join("\n");
    if (pages) {
      pages.querySelector("span")!.textContent = `Lines ${start + 1}–${start + page.length} of ${lines.length}`;
      const [previous, next] = pages.querySelectorAll<HTMLButtonElement>("button");
      previous!.disabled = start === 0;
      next!.disabled = start + pageSize >= lines.length;
      previous!.onclick = () => show(start - pageSize);
      next!.onclick = () => show(start + pageSize);
    }
    body.scrollTop = 0;
    if (line === null || line <= start || line > start + page.length) {
      source.textContent = content;
      return;
    }

    const index = line - start - 1;
    const offset = page.slice(0, index).reduce((sum, value) => sum + value.length + 1, 0);
    const target = document.createElement("span");
    target.className = "file-viewer-target";
    target.dataset.line = String(line);
    target.textContent = page[index]!;
    source.replaceChildren(content.slice(0, offset), target, content.slice(offset + target.textContent.length));
    target.scrollIntoView({ block: "center" });
  };
  const initial = line !== null && line > 0 && line <= lines.length ? line - 1 : 0;
  show(Math.floor(initial / pageSize) * pageSize);
}

function showProblem(body: HTMLElement, error: unknown) {
  body.innerHTML = `<div class="gate"><div class="gate-card" role="alert">
    <div class="gate-mark">${glyph("alert")}</div><h1></h1><p></p>
  </div></div>`;
  body.querySelector("h1")!.textContent = "Cannot open this file";
  body.querySelector(".gate-card p")!.textContent = describeError(error);
}
