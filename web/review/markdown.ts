import pierreDark from "@pierre/theme/pierre-dark";
import pierreDarkSoft from "@pierre/theme/pierre-dark-soft";
import pierreLight from "@pierre/theme/pierre-light";
import pierreLightSoft from "@pierre/theme/pierre-light-soft";
import { Marked, Renderer } from "marked";
import { codeToHtml, type BundledLanguage, type BundledTheme } from "shiki";
import type { SyntaxTheme } from "./review-settings";

const renderer = new Renderer();
renderer.html = ({ text }) => escapeHtml(text);
renderer.link = function ({ href, title, tokens }) {
  const label = this.parser.parseInline(tokens);
  const safeHref = safeLink(href);
  const titleAttribute = title ? ` title="${escapeHtml(title)}"` : "";
  return `<a href="${escapeHtml(safeHref)}"${titleAttribute} target="_blank" rel="noreferrer">${label}</a>`;
};

const markdown = new Marked({ breaks: true, gfm: true, renderer });
const themes = {
  "pierre-light": pierreLight,
  "pierre-light-soft": pierreLightSoft,
  "pierre-dark": pierreDark,
  "pierre-dark-soft": pierreDarkSoft,
} as const;

export async function renderMarkdown(
  container: HTMLElement,
  source: string,
  themeName: Exclude<SyntaxTheme, "system">,
) {
  const rendered = await markdown.parse(source || "*Nothing to preview yet.*");
  const template = document.createElement("template");
  template.innerHTML = rendered;
  sanitize(template.content);
  container.replaceChildren(template.content.cloneNode(true));

  const codeBlocks = [...container.querySelectorAll<HTMLElement>("pre > code")];
  await Promise.all(codeBlocks.map(async (code) => {
    const pre = code.parentElement;
    if (!pre) return;
    const language = code.className.match(/(?:^|\s)language-([^\s]+)/)?.[1] ?? "text";
    try {
      const highlighted = await codeToHtml(code.textContent ?? "", {
        lang: language as BundledLanguage,
        theme: themes[themeName] as unknown as BundledTheme,
      });
      const highlightedTemplate = document.createElement("template");
      highlightedTemplate.innerHTML = highlighted;
      const replacement = highlightedTemplate.content.firstElementChild;
      if (replacement) pre.replaceWith(replacement);
    } catch {
      code.className = "language-text";
    }
  }));
}

function sanitize(fragment: DocumentFragment) {
  const allowed = new Set([
    "A", "BLOCKQUOTE", "BR", "CODE", "DEL", "EM", "H1", "H2", "H3", "H4", "H5", "H6",
    "HR", "LI", "OL", "P", "PRE", "STRONG", "TABLE", "TBODY", "TD", "TH", "THEAD", "TR", "UL",
  ]);
  for (const element of [...fragment.querySelectorAll<HTMLElement>("*")]) {
    if (!allowed.has(element.tagName)) {
      element.replaceWith(document.createTextNode(element.textContent ?? ""));
      continue;
    }
    for (const attribute of [...element.attributes]) {
      const keepLinkAttribute = element.tagName === "A"
        && ["href", "title", "target", "rel"].includes(attribute.name);
      const keepCodeLanguage = element.tagName === "CODE"
        && attribute.name === "class"
        && attribute.value.startsWith("language-");
      if (!keepLinkAttribute && !keepCodeLanguage) element.removeAttribute(attribute.name);
    }
  }
}

function safeLink(value: string) {
  try {
    const url = new URL(value, window.location.href);
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
