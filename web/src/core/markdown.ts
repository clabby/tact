import {
  getSharedHighlighter,
  type DiffsThemeNames,
  type SupportedLanguages,
} from "@pierre/diffs";
import katex from "katex";
import { Marked, Renderer, type TokenizerAndRendererExtension } from "marked";
import footnotes from "marked-footnote";
import { openLightbox } from "../chat/lightbox";
import type { SyntaxTheme } from "../review/review-settings";
import { glyph } from "../ui/glyphs";
import { toast } from "../ui/toast";
import { decodeDestination, fileReference } from "./local-files";

const renderer = new Renderer();
renderer.html = ({ text }) => escapeHtml(text);
renderer.link = function ({ href, title, tokens }) {
  const label = this.parser.parseInline(tokens);
  const titleAttribute = title ? ` title="${escapeHtml(title)}"` : "";

  // A fragment moves within the rendering. Other links open a new tab so the page keeps its place.
  const target = href.startsWith("#") ? "" : ' target="_blank" rel="noreferrer"';
  return `<a href="${escapeHtml(safeLink(href))}"${titleAttribute}${target}>${label}</a>`;
};

/** A heading of a parsed document. */
export type Heading = { depth: number; text: string; slug: string };

// Headings and images are recorded while parsing, so `headings` and `markdownImages` see exactly
// what the renderer emits. The parse is synchronous, so one list per parse suffices.
let parsedHeadings: Heading[] = [];
let parsedImages: { destination: string; alt: string }[] = [];

// Remember allocated names and the next suffix so repeated headings do not rescan prior collisions.
const headingSlugs = new Set<string>();
const headingSuffixes = new Map<string, number>();

renderer.image = ({ href, title, text }) => {
  parsedImages.push({ destination: href, alt: text });
  const titleAttribute = title ? ` title="${escapeHtml(title)}"` : "";
  return `<img src="${escapeHtml(href)}" alt="${escapeHtml(text)}"${titleAttribute}>`;
};

renderer.heading = function ({ tokens, depth }) {
  const text = this.parser.parseInline(tokens, this.parser.textRenderer);
  const base = slugify(text) || "section";
  let slug = base;
  let suffix = headingSuffixes.get(base) ?? 2;
  while (headingSlugs.has(slug)) slug = `${base}-${suffix++}`;
  headingSuffixes.set(base, suffix);
  headingSlugs.add(slug);
  parsedHeadings.push({ depth, text, slug });
  return `<h${depth} data-heading="${slug}">${this.parser.parseInline(tokens)}</h${depth}>\n`;
};

function slugify(text: string) {
  return text.toLowerCase().replace(/[^\p{L}\p{N}]+/gu, "-").replace(/^-|-$/g, "");
}

// Math is parsed here but typeset after sanitizing: the parser emits the TeX source as escaped text in
// a `code.language-math` element, which the sanitizer keeps, and `renderMath` replaces it with
// KaTeX output. A `pre` around it, as for a ```math fence, or a `math-display` class means display
// mode. Single dollars follow Pandoc's rule, so prices and shell variables stay text: the opening
// `$` is not followed by a space and the closing one is not preceded by a space or followed by a
// word character.
const BLOCK_MATH = /^ {0,3}(?:\$\$([\s\S]+?)\$\$|\\\[([\s\S]+?)\\\])[ \t]*(?:\n+|$)/;
const INLINE_MATH: [RegExp, boolean][] = [
  [/^\$\$([^\n]+?)\$\$/, true],
  [/^\\\[([^\n]+?)\\\]/, true],
  [/^\\\(([^\n]+?)\\\)/, false],
  [/^\$(?![\s$])((?:\\[^\n]|[^\\$\n])+?)(?<!\s)\$(?![\w$])/, false],
];
const math: TokenizerAndRendererExtension[] = [
  {
    name: "blockMath",
    level: "block",
    // Marked asks from the middle of a line, so a block can only start after a newline.
    start: (source) => {
      const index = /\n {0,3}(?:\$\$|\\\[)/.exec(source)?.index;
      return index === undefined ? undefined : index + 1;
    },
    tokenizer(source) {
      const match = BLOCK_MATH.exec(source);
      if (match) return { type: "blockMath", raw: match[0], text: (match[1] ?? match[2]!).trim() };
    },
    renderer: ({ text }) => `<pre><code class="language-math">${escapeHtml(text)}</code></pre>\n`,
  },
  {
    name: "inlineMath",
    level: "inline",
    start: (source) => /\$|\\[([]/.exec(source)?.index,
    tokenizer(source) {
      for (const [pattern, display] of INLINE_MATH) {
        const match = pattern.exec(source);
        if (match) return { type: "inlineMath", raw: match[0], text: match[1]!.trim(), display };
      }
    },
    renderer: ({ text, display }) =>
      `<code class="language-math${display ? " math-display" : ""}">${escapeHtml(text)}</code>`,
  },
];

const markdown = new Marked({ breaks: true, gfm: true, renderer, extensions: math }).use(footnotes());
const themes: DiffsThemeNames[] = [
  "pierre-light", "pierre-light-soft", "pierre-dark", "pierre-dark-soft",
];

/** Code blocks longer than this many lines start collapsed. */
const COLLAPSED_LINES = 40;

/** GitHub's alert kinds, written `> [!NOTE]` on a blockquote's first line. */
const CALLOUT = /^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\][ \t]*/i;

/**
 * A code span naming a workspace file, optionally with a line and column: `src/app.ts`,
 * `./web/index.html:12`, or `/abs/path/lib.rs:40:7`. A bare name needs a line to count, so
 * identifiers such as `markdown.ts` in prose stay code.
 */
const FILE_REFERENCE = /^((?:\.{0,2}\/)?(?:[\w.@+-]+\/)*[\w@+-][\w.@+-]*\.[A-Za-z0-9]{1,10})(?::(\d+)(?::\d+)?)?$/;

/**
 * Markdown to HTML with raw HTML escaped and links restricted to http(s), mailto, fragments, and
 * local files. The DOM pass in `renderMarkdown` additionally strips any element or attribute
 * outside a small allowlist and routes local file links through `fileLink`.
 */
export function markdownHtml(source: string) {
  parsedHeadings = [];
  parsedImages = [];
  headingSlugs.clear();
  headingSuffixes.clear();
  return markdown.parse(source, { async: false });
}

/** The headings `renderMarkdown` would anchor in `source`, with the same slugs. */
export function headings(source: string): Heading[] {
  markdownHtml(source);
  return parsedHeadings;
}

/** The images `source` embeds, in document order, as written. */
export function markdownImages(source: string) {
  markdownHtml(source);
  return parsedImages;
}

export type MarkdownOptions = {
  highlight?: boolean;
  placeholder?: string;
  /** Maps an image destination to a URL the page may load, or null to show only its alt text. */
  imageSource?: (destination: string) => string | null;
  /** Maps a link to a local file to the page that shows it. Without it, such links are inert. */
  fileLink?: (destination: string) => string | null;
  /** Opens a workspace file a code span names. Without it, file references stay plain code. */
  openFile?: (path: string, line: number | null) => void;
  /** Gives each heading a link control, called with the heading's slug. */
  linkHeading?: (slug: string) => void;
};

/**
 * Renders sanitized Markdown into `container`. Pass `highlight: false` while text is still
 * streaming: highlighting is the expensive part and re-runs on every render.
 */
export async function renderMarkdown(
  container: HTMLElement,
  source: string,
  themeName: Exclude<SyntaxTheme, "system">,
  options: MarkdownOptions = {},
) {
  const { highlight = true, placeholder = "*Nothing to preview yet.*" } = options;
  const template = document.createElement("template");
  template.innerHTML = markdownHtml(source || placeholder);
  sanitize(template.content, options);
  renderMath(template.content);
  enhance(template.content, options);
  container.replaceChildren(template.content);

  // Fragments stay within this rendering, including footnote links and back links.
  for (const link of container.querySelectorAll<HTMLAnchorElement>('a[href^="#"]')) {
    link.addEventListener("click", (event) => {
      event.preventDefault();
      const fragment = link.getAttribute("href")!.slice(1);
      const target = container.querySelector<HTMLElement>(`[data-footnote="${CSS.escape(fragment)}"]`);
      if (!target) {
        scrollToHeading(container, decodeDestination(fragment));
        return;
      }
      target.scrollIntoView({ block: "center", behavior: "smooth" });
      target.classList.remove("footnote-flash");
      void target.offsetWidth;
      target.classList.add("footnote-flash");
    });
  }
  for (const image of container.querySelectorAll("img")) {
    image.addEventListener("error", () => {
      const missing = document.createElement("span");
      missing.className = "image-missing";
      missing.textContent = image.alt ? `Image not available: ${image.alt}` : "Image not available";
      image.replaceWith(missing);
    }, { once: true });
  }
  if (!highlight) return;

  // Diagrams go first: a fence Mermaid cannot draw is then highlighted with the other code.
  await renderDiagrams(container, themeName.includes("dark"));
  const codeBlocks = [...container.querySelectorAll<HTMLElement>("pre > code")];
  await Promise.all(codeBlocks.map(async (code) => {
    const pre = code.parentElement;
    if (!pre) return;
    const text = code.textContent ?? "";
    const language = code.className.match(/(?:^|\s)language-([^\s]+)/)?.[1] ?? "text";
    let block: HTMLElement = pre;
    try {
      const highlighter = await getSharedHighlighter({
        themes,
        langs: [language as SupportedLanguages],
      });
      const highlighted = highlighter.codeToHtml(text, {
        lang: language,
        theme: themeName,
      });
      const highlightedTemplate = document.createElement("template");
      highlightedTemplate.innerHTML = highlighted;
      const replacement = highlightedTemplate.content.firstElementChild as HTMLElement | null;
      if (replacement) {
        pre.replaceWith(replacement);
        block = replacement;
      }
    } catch {
      code.className = "language-text";
    }
    if (block.isConnected) addCodeControls(block, text);
  }));
}

/** Matches an already decoded fragment against this renderer's heading slugs. */
export function scrollToHeading(container: HTMLElement, fragment: string) {
  const slug = slugify(fragment);
  if (!slug) return;
  const heading = container.querySelector<HTMLElement>(`[data-heading="${CSS.escape(slug)}"]`);
  heading?.scrollIntoView({ block: "start" });
}

function sanitize(fragment: DocumentFragment, { imageSource, fileLink }: MarkdownOptions) {
  const allowed = new Set([
    "A", "BLOCKQUOTE", "BR", "CODE", "DEL", "EM", "H1", "H2", "H3", "H4", "H5", "H6", "HR", "LI",
    "OL", "P", "PRE", "SECTION", "STRONG", "SUP", "TABLE", "TBODY", "TD", "TH", "THEAD", "TR", "UL",
  ]);
  for (const element of [...fragment.querySelectorAll<HTMLElement>("*")]) {
    if (element.tagName === "IMG") {
      const source = imageSource?.(element.getAttribute("src") ?? "");
      if (!source) {
        element.replaceWith(document.createTextNode(element.getAttribute("alt") ?? ""));
        continue;
      }
      const alt = element.getAttribute("alt") ?? "";
      const title = element.getAttribute("title");
      for (const attribute of [...element.attributes]) element.removeAttribute(attribute.name);
      element.setAttribute("src", source);
      element.setAttribute("alt", alt);
      element.setAttribute("loading", "lazy");
      if (title) element.setAttribute("title", title);
      continue;
    }
    // Task list items: a disabled checkbox and nothing else.
    if (element.tagName === "INPUT" && element.getAttribute("type") === "checkbox") {
      const checked = element.hasAttribute("checked");
      for (const attribute of [...element.attributes]) element.removeAttribute(attribute.name);
      element.setAttribute("type", "checkbox");
      element.setAttribute("disabled", "");
      if (checked) element.setAttribute("checked", "");
      continue;
    }
    if (!allowed.has(element.tagName)) {
      element.replaceWith(document.createTextNode(element.textContent ?? ""));
      continue;
    }
    // Footnote ids would collide between messages on one page, so they become scoped data.
    const footnote = element.getAttribute("id")?.startsWith("footnote-") ? element.getAttribute("id")! : null;

    // A local path would resolve against this page and miss, so it opens the file viewer instead.
    const href = element.tagName === "A" ? element.getAttribute("href") : null;
    if (href && fileReference(href)) {
      const page = fileLink?.(href);
      if (page) element.setAttribute("href", page);
      else for (const name of ["href", "target", "rel"]) element.removeAttribute(name);
    }
    for (const attribute of [...element.attributes]) {
      const keepLinkAttribute = element.tagName === "A"
        && ["href", "title", "target", "rel"].includes(attribute.name);
      const keepCodeLanguage = element.tagName === "CODE"
        && attribute.name === "class"
        && attribute.value.startsWith("language-");
      const keepHeadingSlug = /^H[1-6]$/.test(element.tagName) && attribute.name === "data-heading";
      if (!keepLinkAttribute && !keepCodeLanguage && !keepHeadingSlug) element.removeAttribute(attribute.name);
    }
    if (footnote) element.dataset.footnote = footnote;
  }
}

/** Structure on top of sanitized output: callouts, footnotes, task lists, anchors, file links. */
function enhance(fragment: DocumentFragment, { openFile, linkHeading }: MarkdownOptions) {
  for (const quote of fragment.querySelectorAll<HTMLElement>("blockquote")) {
    const first = quote.firstElementChild;
    const marker = first?.tagName === "P" ? first.firstChild : null;
    const kind = marker?.nodeType === Node.TEXT_NODE ? CALLOUT.exec(marker.textContent ?? "")?.[1]?.toLowerCase() : undefined;
    if (!kind || !first || !marker) continue;
    marker.textContent = marker.textContent!.replace(CALLOUT, "");
    if (!marker.textContent) marker.remove();
    if (first.firstChild?.nodeName === "BR") first.firstChild.remove();
    if (!first.textContent?.trim() && !first.children.length) first.remove();
    const title = document.createElement("p");
    title.className = "callout-title";
    title.textContent = kind[0]!.toUpperCase() + kind.slice(1);
    quote.prepend(title);
    quote.className = `callout callout-${kind}`;
  }
  for (const section of fragment.querySelectorAll<HTMLElement>("section")) {
    section.className = "footnotes";
    section.querySelector(":scope > h2")?.remove();
  }
  for (const box of fragment.querySelectorAll("li > input[type=checkbox]")) {
    box.parentElement!.classList.add("task");
    box.parentElement!.parentElement?.classList.add("task-list");
  }
  if (linkHeading) {
    for (const heading of fragment.querySelectorAll<HTMLElement>("[data-heading]")) {
      const anchor = document.createElement("button");
      anchor.type = "button";
      anchor.className = "heading-anchor";
      anchor.title = "Copy link to this section";
      anchor.setAttribute("aria-label", "Copy link to this section");
      anchor.innerHTML = glyph("link");
      anchor.addEventListener("click", () => linkHeading(heading.dataset.heading!));
      heading.append(anchor);
    }
  }
  if (openFile) {
    for (const code of fragment.querySelectorAll<HTMLElement>(":not(pre) > code:not([class])")) {
      const match = FILE_REFERENCE.exec(code.textContent ?? "");
      if (!match || (!match[1]!.includes("/") && !match[2])) continue;
      const [, path, line] = match;
      code.classList.add("file-link");
      code.tabIndex = 0;
      code.setAttribute("role", "link");
      code.title = "Show in Review";
      const open = () => openFile(path!, line ? Number(line) : null);
      code.addEventListener("click", open);
      code.addEventListener("keydown", (event) => {
        if (event.key === "Enter") open();
      });
    }
  }
}

function copyButton(label: string, text: string) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "copy-button";
  button.title = label;
  button.setAttribute("aria-label", label);
  button.innerHTML = glyph("copy");
  button.addEventListener("click", async (event) => {
    event.stopPropagation();
    try {
      await navigator.clipboard.writeText(text);
      button.innerHTML = glyph("check");
      button.classList.add("copied");
      setTimeout(() => {
        button.innerHTML = glyph("copy");
        button.classList.remove("copied");
      }, 1200);
    } catch {
      toast("Could not copy to the clipboard.", "warning");
    }
  });
  return button;
}

/** Wraps a code block with a copy control and, past `COLLAPSED_LINES`, a fold. */
function addCodeControls(pre: HTMLElement, text: string) {
  const block = document.createElement("div");
  block.className = "code-block";
  pre.replaceWith(block);
  block.append(pre, copyButton("Copy code", text));
  const lines = text.replace(/\n$/, "").split("\n").length;
  if (lines <= COLLAPSED_LINES) return;
  block.classList.add("collapsed");
  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.className = "code-expand";
  const label = () => {
    const collapsed = block.classList.contains("collapsed");
    toggle.textContent = collapsed ? `Show all ${lines} lines` : "Show less";
    toggle.setAttribute("aria-expanded", String(!collapsed));
  };
  toggle.addEventListener("click", () => {
    block.classList.toggle("collapsed");
    label();
  });
  label();
  block.append(toggle);
}

/** Typesets the math the parser marked. KaTeX escapes the source and ignores links and HTML. */
function renderMath(fragment: DocumentFragment) {
  for (const code of fragment.querySelectorAll<HTMLElement>("code.language-math")) {
    const block = code.parentElement?.tagName === "PRE" ? code.parentElement : null;
    const displayMode = block !== null || code.classList.contains("math-display");
    const typeset = document.createElement(block ? "div" : "span");
    typeset.className = displayMode ? "math math-display" : "math";
    const source = code.textContent ?? "";
    katex.render(source, typeset, { displayMode, throwOnError: false, strict: "ignore" });
    if (block) typeset.append(copyButton("Copy TeX", source));
    (block ?? code).replaceWith(typeset);
  }
}

// Mermaid keeps one global configuration, so renders run one at a time with their theme set first.
let diagramQueue: Promise<unknown> = Promise.resolve();
let diagramCount = 0;

/**
 * Replaces ```mermaid fences with diagrams that open enlarged on click. Mermaid is large, so it
 * loads with the first diagram. Strict security makes it sanitize labels and drop click handlers.
 * A fence that fails to parse stays as code under a note saying so.
 */
function renderDiagrams(container: HTMLElement, dark: boolean) {
  const fences = [...container.querySelectorAll<HTMLElement>("pre > code.language-mermaid")];
  if (!fences.length) return Promise.resolve();
  diagramQueue = diagramQueue.then(async () => {
    const { default: mermaid } = await import("mermaid");
    mermaid.initialize({
      startOnLoad: false,
      securityLevel: "strict",
      suppressErrorRendering: true,
      theme: dark ? "dark" : "default",
      fontFamily: getComputedStyle(container).fontFamily,
    });
    for (const code of fences) {
      const source = code.textContent ?? "";
      try {
        const { svg } = await mermaid.render(`mermaid-${++diagramCount}`, source);
        const diagram = document.createElement("div");
        diagram.className = "diagram";
        diagram.innerHTML = svg;
        diagram.title = "Enlarge diagram";
        diagram.addEventListener("click", () => openDiagram(diagram));
        diagram.append(copyButton("Copy Mermaid source", source));
        code.parentElement!.replaceWith(diagram);
      } catch {
        const note = document.createElement("p");
        note.className = "diagram-error";
        note.textContent = "This Mermaid diagram could not be drawn; its source follows.";
        code.parentElement!.before(note);
      }
    }
  }).catch(() => {});
  return diagramQueue;
}

/** Shows a diagram in the lightbox at its natural size, as an image the page cannot script. */
function openDiagram(diagram: HTMLElement) {
  const svg = diagram.querySelector("svg");
  if (!svg) return;
  const copy = svg.cloneNode(true) as SVGSVGElement;
  const { width, height } = svg.viewBox.baseVal;
  if (width && height) {
    copy.setAttribute("width", String(width));
    copy.setAttribute("height", String(height));
    copy.style.maxWidth = "none";
  }
  copy.setAttribute("xmlns", "http://www.w3.org/2000/svg");
  const background = getComputedStyle(diagram).getPropertyValue("--surface").trim();
  if (background) copy.style.background = background;
  const source = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(new XMLSerializer().serializeToString(copy))}`;
  openLightbox([{ source, alt: "Diagram" }]);
}

/** Fragments and local files stay as written, for the DOM pass to route. */
function safeLink(value: string) {
  if (fileReference(value)) return value;
  try {
    const url = new URL(value, globalThis.location?.href ?? "http://localhost/");
    if (["http:", "https:", "mailto:"].includes(url.protocol)) return value;
  } catch {
    // Invalid links render as inert anchors.
  }
  return "#";
}

function escapeHtml(value: string) {
  return value.replace(/[&<>'"]/g, (character) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;",
  })[character] ?? character);
}
