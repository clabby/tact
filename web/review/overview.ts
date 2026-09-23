import { compileSync } from "@mdx-js/mdx";
import remarkGfm from "remark-gfm";

export type OverviewAppearance = "light" | "dark" | "system";

// This function is serialized into the frame asset. It must remain self-contained: all
// authored JavaScript runs in the opaque-origin sandbox, never in the review app.
function iframeRuntime() {
  type ElementType = string | ((props: Record<string, unknown>) => unknown);
  type VNode = { type: ElementType; props: Record<string, unknown> };
  const jsx = (type: ElementType, props: Record<string, unknown> = {}): VNode => ({ type, props });
  const Fragment = ({ children }: { children?: unknown }) => children;
  const components = {
    Callout: ({ tone = "note", title, children }: Record<string, unknown>) => jsx("aside", {
      className: `callout callout--${["note", "idea", "caution", "critical"].includes(String(tone)) ? tone : "note"}`,
      children: [jsx("strong", { children: title || String(tone).replace(/^./, (c) => c.toUpperCase()) }), children],
    }),
    Card: ({ title, label, children }: Record<string, unknown>) => jsx("section", {
      className: "card", children: [title && jsx("h3", { children: title }), label && jsx("span", { children: label }), children],
    }),
    CardGrid: ({ children }: Record<string, unknown>) => jsx("div", { className: "card-grid", children }),
    Metric: ({ value, label, detail, children }: Record<string, unknown>) => jsx("div", {
      className: "metric", children: [jsx("strong", { children: value }), jsx("span", { children: label }), children,
        detail && jsx("small", { children: detail })],
    }),
    MetricGrid: ({ children }: Record<string, unknown>) => jsx("div", { className: "metric-grid", children }),
    Process: ({ children }: Record<string, unknown>) => jsx("ol", { className: "process", children }),
    ProcessStep: ({ title, children }: Record<string, unknown>) => jsx("li", {
      className: "process-step", children: [title && jsx("h3", { children: title }), children],
    }),
    Figure: ({ caption, children }: Record<string, unknown>) => jsx("figure", {
      className: "figure", children: [children, caption && jsx("figcaption", { children: caption })],
    }),
  };
  const root = document.getElementById("overview-root")!;
  function render(value: unknown, parent: Node, depth = 0) {
    if (depth > 100) throw new Error("Overview component nesting is too deep");
    if (value === null || value === undefined || typeof value === "boolean") return;
    if (Array.isArray(value)) {
      for (const child of value) render(child, parent, depth + 1);
      return;
    }
    if (typeof value !== "object") {
      parent.appendChild(document.createTextNode(String(value)));
      return;
    }
    const { type, props } = value as VNode;
    if (typeof type === "function") {
      render(type(props), parent, depth + 1);
      return;
    }
    if (typeof type !== "string") throw new Error("Invalid MDX component");
    const svg = parent instanceof SVGElement && type !== "foreignObject" || type === "svg";
    const element = svg ? document.createElementNS("http://www.w3.org/2000/svg", type) : document.createElement(type);
    for (const [name, prop] of Object.entries(props || {})) {
      if (name === "children" || name === "key" || name === "ref" || prop == null) continue;
      if (name === "dangerouslySetInnerHTML" && typeof prop === "object" && "__html" in prop) {
        element.innerHTML = String(prop.__html);
      } else if (/^on[A-Z]/.test(name) && typeof prop === "function") {
        element.addEventListener(name.slice(2).toLowerCase(), prop as EventListener);
      } else if (name === "style" && typeof prop === "object") {
        for (const [property, val] of Object.entries(prop)) {
          const cssName = property.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`);
          (element as HTMLElement).style.setProperty(cssName, String(val));
        }
      } else if (typeof prop === "string" || typeof prop === "number" || prop === true) {
        const attribute = name === "className" ? "class" : name === "htmlFor" ? "for" :
          svg && /^(?:marker(?:Start|Mid|End)|text(?:Anchor|Decoration|Rendering)|(?:alignment|baseline|clip|color|dominant|fill|flood|font|image|letter|lighting|paint|pointer|shape|stop|stroke|unicode|vector|word|writing)[A-Z])/.test(name) ?
            name.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`) : name;
        element.setAttribute(attribute, prop === true ? "" : String(prop));
      }
    }
    if (!(props && "dangerouslySetInnerHTML" in props)) render(props?.children, element, depth + 1);
    parent.appendChild(element);
  }
  window.addEventListener("message", (event: MessageEvent) => {
    if (event.source !== window.parent || !event.data || event.data.type !== "tact-overview") return;
    const { code, appearance } = event.data;
    if (typeof code !== "string" || !["light", "dark", "system"].includes(appearance)) return;
    document.documentElement.dataset.theme = appearance;
    root.replaceChildren();
    try {
      // Evaluation is confined to this opaque-origin sandbox.
      const exported = new Function(code)({ jsx, jsxs: jsx, Fragment, baseUrl: "about:blank" }) as
        { default: (props: Record<string, unknown>) => unknown } | Promise<{ default: (props: Record<string, unknown>) => unknown }>;
      Promise.resolve(exported).then((module) => render(jsx(module.default, { components }), root))
        .catch((error) => { root.textContent = `Overview error: ${String(error)}`; });
    } catch (error) {
      root.textContent = `Overview error: ${String(error)}`;
    }
  });
}

export function overviewProgram(mdx: string): string {
  try {
    return String(compileSync(mdx, { outputFormat: "function-body", remarkPlugins: [remarkGfm] }));
  } catch (error) {
    return `return {default: () => ({type: "pre", props: {children: ${JSON.stringify(String(error))}}})};`;
  }
}

// This static asset has its own CSP and runs only in an iframe with sandbox="allow-scripts".
export function overviewFrameDocument(): string {
  return `<!doctype html>
<html data-theme="system">
  <head>
    <meta charset="utf-8">
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; base-uri 'none'; form-action 'none'">
    <style>${overviewStyles("system")}</style>
  </head>
  <body><article class="prose" id="overview-root"></article><script>(${iframeRuntime.toString()})()</script></body>
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
    .prose svg { max-width: 100%; height: auto; }
    @media (max-width: 560px) { .card-grid, .metric-grid { grid-template-columns: 1fr; } }
  `;
}
