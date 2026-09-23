import rehypeStringify from "rehype-stringify";
import remarkGfm from "remark-gfm";
import remarkMdx from "remark-mdx";
import remarkParse from "remark-parse";
import remarkRehype from "remark-rehype";
import { unified } from "unified";

export type OverviewAppearance = "light" | "dark" | "system";

type Node = {
  type: string;
  name?: string | null;
  value?: string;
  url?: string;
  children?: Node[];
  attributes?: { type: string; name?: string; value?: unknown }[];
  data?: { hName?: string; hProperties?: { className: string } };
  [key: string]: unknown;
};

const markdown = new Set([
  "root", "paragraph", "heading", "text", "emphasis", "strong", "delete",
  "inlineCode", "code", "blockquote", "list", "listItem", "table",
  "tableRow", "tableCell", "thematicBreak", "break", "link",
]);

const components: Record<string, { tag: string; props: string[] }> = {
  Callout: { tag: "aside", props: ["title", "tone"] },
  Card: { tag: "section", props: ["title", "label"] },
  CardGrid: { tag: "div", props: [] },
  Metric: { tag: "div", props: ["value", "label", "detail"] },
  MetricGrid: { tag: "div", props: [] },
  Process: { tag: "ol", props: [] },
  ProcessStep: { tag: "li", props: ["title"] },
  Figure: { tag: "figure", props: ["caption"] },
};

function element(tag: string, children: Node[], className?: string): Node {
  return {
    type: "paragraph",
    data: { hName: tag, ...(className ? { hProperties: { className } } : {}) },
    children,
  };
}

function label(tag: string, value: string): Node {
  return element(tag, [{ type: "text", value }]);
}

function component(node: Node): Node | null {
  const spec = node.name && components[node.name];
  if (!spec) return null;
  const props: Record<string, string> = {};
  for (const attribute of node.attributes ?? []) {
    if (attribute.type === "mdxJsxAttribute" && attribute.name &&
      spec.props.includes(attribute.name) && typeof attribute.value === "string") {
      props[attribute.name] = attribute.value;
    }
  }
  const children = sanitize(node.children ?? []);
  switch (node.name) {
    case "Callout": {
      const tone = ["note", "idea", "caution", "critical"].includes(props.tone) ? props.tone : "note";
      children.unshift(label("strong", props.title || tone[0].toUpperCase() + tone.slice(1)));
      return element(spec.tag, children, `callout callout--${tone}`);
    }
    case "Card":
      if (props.title) children.unshift(label("h3", props.title));
      if (props.label) children.unshift(label("span", props.label));
      break;
    case "Metric":
      children.unshift(label("strong", props.value ?? ""), label("span", props.label ?? ""));
      if (props.detail) children.push(label("small", props.detail));
      break;
    case "ProcessStep":
      if (props.title) children.unshift(label("h3", props.title));
      break;
    case "Figure":
      if (props.caption) children.push(label("figcaption", props.caption));
      break;
  }
  const className = node.name === "ProcessStep" ? "process-step" :
    node.name.replace(/[A-Z]/g, (letter, index) => `${index ? "-" : ""}${letter.toLowerCase()}`);
  return element(spec.tag, children, className);
}

function safeUrl(url: string): boolean {
  return url.startsWith("#") || /^(https?:\/\/|mailto:)/i.test(url);
}

// Rebuild the tree from known Markdown nodes and literal component props.
// Expressions, imports, raw HTML, and unknown JSX never reach HTML serialization.
function sanitize(nodes: Node[]): Node[] {
  const result: Node[] = [];
  for (const node of nodes) {
    if (node.type === "paragraph" && node.children?.length === 1 &&
      node.children[0].type === "mdxJsxTextElement") {
      const safe = component(node.children[0]);
      if (safe) result.push(safe);
    } else if (node.type === "mdxJsxFlowElement" || node.type === "mdxJsxTextElement") {
      const safe = component(node);
      if (safe) result.push(safe);
    } else if (markdown.has(node.type) &&
      (node.type !== "link" || (node.url && safeUrl(node.url)))) {
      const { type, value, url, children, depth, ordered, start, lang, align } = node;
      result.push({ type, ...(value === undefined ? {} : { value }),
        ...(url === undefined ? {} : { url }),
        ...(children ? { children: sanitize(children) } : {}),
        ...(type === "heading" ? { depth } : {}),
        ...(type === "list" ? { ordered, start } : {}),
        ...(type === "code" ? { lang } : {}),
        ...(type === "table" ? { align } : {}),
      });
    }
  }
  return result;
}

const renderer = unified().use(remarkParse).use(remarkMdx).use(remarkGfm)
  .use(() => (tree) => { tree.children = sanitize(tree.children as Node[]) as typeof tree.children; })
  .use(remarkRehype).use(rehypeStringify);

export function overviewDocument(mdx: string, appearance: OverviewAppearance) {
  let html: string;
  try {
    html = String(renderer.processSync(mdx));
  } catch {
    // Malformed MDX is still untrusted text; present it without interpreting markup.
    html = `<pre>${mdx.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")}</pre>`;
  }
  return `<!doctype html>
<html data-theme="${appearance}">
  <head>
    <meta charset="utf-8">
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'">
    <style>${overviewStyles(appearance)}</style>
  </head>
  <body><article class="prose">${html}</article></body>
</html>`;
}

function overviewStyles(appearance: OverviewAppearance) {
  const colorScheme = appearance === "system" ? "light dark" : appearance;
  return `
    :root { color-scheme: ${colorScheme}; --paper: #f7f8f5; --ink: #222820; --muted: #586458; --rule: #dce3d9; --accent: #315f36; --soft: #eaf1e8; background: var(--paper); }
    @media (prefers-color-scheme: dark) { :root[data-theme="system"] { --paper: #0f1115; --ink: #e8eee7; --muted: #a9b8aa; --rule: #303a32; --accent: #9bc59e; --soft: #1a2820; } }
    :root[data-theme="dark"] { --paper: #0f1115; --ink: #e8eee7; --muted: #a9b8aa; --rule: #303a32; --accent: #9bc59e; --soft: #1a2820; }
    * { box-sizing: border-box; }
    html, body { min-height: 100%; background: var(--paper); }
    body { margin: 0; color: var(--ink); font: 15px/1.7 ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
    .prose { max-width: 800px; margin: auto; padding: clamp(24px, 5vw, 56px); overflow-wrap: anywhere; }
    h1, h2, h3 { line-height: 1.2; letter-spacing: -.025em; }
    h1 { font-size: clamp(2rem, 6vw, 3rem); } h2 { margin-top: 2.5em; padding-top: .7em; border-top: 1px solid var(--rule); font-size: 1.7rem; }
    h3 { font-size: 1.1rem; } p, li { color: var(--muted); } strong { color: var(--ink); }
    a { color: var(--accent); text-underline-offset: .18em; } code { padding: .1em .35em; border-radius: .25em; background: var(--soft); }
    pre { overflow: auto; padding: 1.2em; border-radius: .5em; background: var(--soft); } pre code { padding: 0; }
    blockquote { margin: 2em 0; padding-left: 1.2em; border-left: 3px solid var(--accent); }
    table { display: block; overflow-x: auto; border-collapse: collapse; } th, td { padding: .6em .9em; border-bottom: 1px solid var(--rule); text-align: left; }
    .callout, .card { margin: 1.5em 0; padding: 1.3em; border: 1px solid var(--rule); border-radius: .6em; background: var(--soft); }
    .callout { border-left: 3px solid var(--accent); } .callout > strong, .card > span { display: block; color: var(--accent); font-size: .78rem; letter-spacing: .08em; text-transform: uppercase; }
    .callout--caution { border-left-color: #b47a25; } .callout--critical { border-left-color: #c25063; }
    .card { margin: 0; background: var(--paper); } .card h3 { margin: .5em 0; }
    .card-grid, .metric-grid { display: grid; gap: 1em; margin: 2em 0; }
    .card-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); } .metric-grid { grid-template-columns: repeat(3, minmax(0, 1fr)); }
    .metric { min-width: 0; padding: 1em; border-top: 2px solid var(--accent); } .metric strong, .metric span, .metric small { display: block; }
    .metric strong { color: var(--accent); font-size: 2.3rem; line-height: 1.1; } .metric span { margin-top: .5em; font-size: .75rem; font-weight: 700; text-transform: uppercase; }
    .process { list-style: none; counter-reset: steps; padding: 0; } .process-step { counter-increment: steps; position: relative; padding: 0 0 1.5em 3em; border-left: 1px solid var(--rule); margin-left: 1em; }
    .process-step::before { content: counter(steps, decimal-leading-zero); position: absolute; left: -1.1em; top: 0; width: 2.2em; height: 2.2em; border: 1px solid var(--rule); border-radius: 50%; background: var(--paper); text-align: center; line-height: 2.2em; color: var(--accent); }
    .process-step h3 { margin-top: 0; } .figure { margin: 2em 0; padding: 1em; border: 1px solid var(--rule); border-radius: .5em; }
    .figure figcaption { color: var(--muted); font-size: .8rem; }
    @media (max-width: 560px) { .card-grid, .metric-grid { grid-template-columns: 1fr; } }
  `;
}
