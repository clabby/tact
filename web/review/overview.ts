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
      children: [jsx("span", { className: "callout-mark", "aria-hidden": "true", children: ({ note: "i", idea: "✦", caution: "!", critical: "×" } as Record<string, string>)[String(tone)] || "i" }),
        jsx("div", { className: "callout-body", children: [jsx("strong", { children: title || String(tone).replace(/^./, (c) => c.toUpperCase()) }), children] })],
    }),
    Card: ({ title, label, children }: Record<string, unknown>) => jsx("section", {
      className: "card", children: [label && jsx("span", { className: "card-label", children: label }),
        title && jsx("h3", { children: title }), jsx("div", { className: "card-body", children })],
    }),
    CardGrid: ({ children }: Record<string, unknown>) => jsx("div", { className: "card-grid", children }),
    Metric: ({ value, label, detail, children }: Record<string, unknown>) => jsx("div", {
      className: "metric", children: [jsx("strong", { children: value }), jsx("span", { children: label }), children,
        detail && jsx("small", { children: detail })],
    }),
    MetricGrid: ({ children }: Record<string, unknown>) => jsx("div", { className: "metric-grid", children }),
    Process: ({ children }: Record<string, unknown>) => jsx("ol", { className: "process", children }),
    ProcessStep: ({ title, children }: Record<string, unknown>) => jsx("li", {
      className: "process-step", children: [jsx("span", { className: "process-marker", "aria-hidden": "true" }),
        jsx("div", { className: "process-body", children: [title && jsx("h3", { children: title }), children] })],
    }),
    Figure: ({ caption, children }: Record<string, unknown>) => jsx("figure", {
      className: "figure", children: [jsx("div", { className: "figure-body", children }), caption && jsx("figcaption", { children: caption })],
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
    <style>${overviewStyles()}</style>
  </head>
  <body><article class="prose" id="overview-root"></article><script>(${iframeRuntime.toString()})()</script></body>
</html>`;
}

function overviewStyles() {
  return `
    :root { color-scheme: light; --paper: #f7f8f5; --paper-deep: #edf0ea; --ink: #202820; --muted: #4f5c51; --faint: #657167; --rule: #dce3d9; --accent: #315f36; --accent-soft: #e5efe3; --green: #315f36; --surface: #fff; --code: #edf0ea; --idea: #61459b; --caution: #8a5b0e; --critical: #a4473f; --font-display: Georgia, "Times New Roman", serif; --font-sans: ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; --font-mono: "SFMono-Regular", Consolas, "Liberation Mono", monospace; background: var(--paper); color: var(--ink); }
    :root[data-theme="dark"] { color-scheme: dark; --paper: #0f1115; --paper-deep: #181e1b; --ink: #edf0ea; --muted: #b2beb3; --faint: #9ba99d; --rule: #334039; --accent: #a3d1a6; --accent-soft: #203327; --green: #9bc59e; --surface: #181e1b; --code: #1c2821; --idea: #c4aff6; --caution: #e1b66b; --critical: #eea59d; }
    @media (prefers-color-scheme: dark) { :root[data-theme="system"] { color-scheme: dark; --paper: #0f1115; --paper-deep: #181e1b; --ink: #edf0ea; --muted: #b2beb3; --faint: #9ba99d; --rule: #334039; --accent: #a3d1a6; --accent-soft: #203327; --green: #9bc59e; --surface: #181e1b; --code: #1c2821; --idea: #c4aff6; --caution: #e1b66b; --critical: #eea59d; } }
    * { box-sizing: border-box; }
    html, body { min-height: 100%; background: var(--paper); }
    body { margin: 0; font: 16px/1.75 var(--font-sans); }
    ::selection { background: var(--accent-soft); }
    .prose { max-width: 880px; margin: auto; padding: clamp(24px, 5vw, 64px); color: var(--ink); overflow-wrap: break-word; }
    .prose > :first-child { margin-top: 0; }
    .prose :is(h1, h2, h3, h4) { color: var(--ink); }
    .prose h1 { margin: 0 0 1em; font: 500 clamp(2.6rem, 6vw, 4.2rem)/1.05 var(--font-display); letter-spacing: -.045em; }
    .prose h2 { margin: 2.2em 0 .65em; padding-top: .7em; border-top: 1px solid var(--rule); font: 500 clamp(2rem, 5vw, 3rem)/1.08 var(--font-display); letter-spacing: -.035em; }
    .prose h3 { margin: 2em 0 .6em; font: 650 1.4rem/1.25 var(--font-sans); letter-spacing: -.02em; }
    .prose h4 { margin: 2em 0 .7em; font: 700 .75rem/1.3 var(--font-sans); letter-spacing: .1em; text-transform: uppercase; }
    .prose p { margin: 1.15em 0; } .prose strong { font-weight: 700; }
    .prose a { color: var(--accent); text-decoration-color: color-mix(in srgb, var(--accent) 45%, transparent); text-underline-offset: .18em; }
    .prose :is(ul, ol) { padding-left: 1.5em; } .prose li { margin: .45em 0; } .prose li::marker { color: var(--accent); }
    .prose hr { margin: 3em 0; border: 0; border-top: 1px solid var(--rule); }
    .prose :not(pre) > code { padding: .12em .34em; border: 1px solid var(--rule); border-radius: .28rem; background: var(--code); color: var(--ink); font: .87em/1.4 var(--font-mono); }
    .prose pre { overflow: auto; margin: 2em 0; padding: 1.25em 1.5em; border: 1px solid var(--rule); border-radius: .55rem; background: var(--code); color: var(--ink); font: .86rem/1.65 var(--font-mono); }
    .prose pre code { background: transparent; color: inherit; font: inherit; }
    .prose .astro-code { background: var(--shiki-light-bg, var(--code)) !important; color: var(--shiki-light, var(--ink)) !important; }
    .prose .astro-code span { color: var(--shiki-light, inherit) !important; }
    :root[data-theme="dark"] .prose .astro-code { background: var(--shiki-dark-bg, var(--code)) !important; color: var(--shiki-dark, var(--ink)) !important; }
    :root[data-theme="dark"] .prose .astro-code span { color: var(--shiki-dark, inherit) !important; }
    @media (prefers-color-scheme: dark) { :root[data-theme="system"] .prose .astro-code { background: var(--shiki-dark-bg, var(--code)) !important; color: var(--shiki-dark, var(--ink)) !important; } :root[data-theme="system"] .prose .astro-code span { color: var(--shiki-dark, inherit) !important; } }
    .prose blockquote { margin: 2em 0; padding: .15em 0 .15em 1.5em; border-left: 3px solid var(--accent); color: var(--muted); font: italic 1.2rem/1.65 var(--font-display); }
    .prose blockquote p { margin: .5em 0; }
    .prose table { display: block; width: 100%; overflow-x: auto; margin: 2em 0; border-collapse: collapse; font-size: .9rem; line-height: 1.5; }
    .prose th { border-bottom: 2px solid var(--ink); font-size: .7rem; letter-spacing: .08em; text-transform: uppercase; }
    .prose :is(th, td) { padding: .8em 1em; text-align: left; vertical-align: top; } .prose td { border-bottom: 1px solid var(--rule); }
    .prose img { max-width: 100%; height: auto; }
    .callout { --callout: var(--green); display: grid; grid-template-columns: 2rem minmax(0, 1fr); gap: 1rem; margin: 2em 0; padding: 1.5em; border: 1px solid color-mix(in srgb, var(--callout) 35%, transparent); border-radius: .55rem; background: color-mix(in srgb, var(--callout) 7%, var(--paper)); }
    .callout--idea { --callout: var(--idea); } .callout--caution { --callout: var(--caution); } .callout--critical { --callout: var(--critical); }
    .callout-mark { display: grid; width: 1.8rem; height: 1.8rem; place-items: center; border-radius: 50%; background: var(--callout); color: var(--paper); font: 700 .8rem/1 var(--font-mono); }
    .callout-body > strong { display: block; margin: .15rem 0 .5rem; color: var(--callout); font-size: .72rem; letter-spacing: .1em; text-transform: uppercase; }
    .callout-body { color: var(--muted); font-size: .94rem; line-height: 1.6; } .callout-body p, .card-body p, .process-body p { margin: .5em 0; }
    .card-grid, .metric-grid { display: grid; gap: 1rem; margin: 2.5em 0; }
    .card-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); } .metric-grid { grid-template-columns: repeat(3, minmax(0, 1fr)); }
    .card { min-width: 0; padding: 1.5em; border: 1px solid var(--rule); border-radius: .55rem; background: var(--surface); }
    .card-label { display: block; margin-bottom: 1rem; color: var(--accent); font-size: .65rem; font-weight: 700; letter-spacing: .1em; text-transform: uppercase; }
    .prose .card h3 { margin: 0 0 .65em; font-size: 1.1rem; } .card-body { color: var(--muted); font-size: .9rem; line-height: 1.55; }
    .metric { min-width: 0; padding: 1.5em 1.25em; border-top: 2px solid var(--ink); }
    .metric :is(strong, span, small) { display: block; } .metric strong { margin-bottom: .3em; color: var(--accent); font: 500 clamp(2.2rem, 5vw, 3.6rem)/1 var(--font-display); letter-spacing: -.04em; }
    .metric span { font-size: .7rem; font-weight: 700; letter-spacing: .08em; text-transform: uppercase; } .metric small { margin-top: .5em; color: var(--faint); font-size: .75rem; line-height: 1.4; }
    .prose .process { margin: 2.5em 0; padding: 0; list-style: none; counter-reset: steps; }
    .process-step { position: relative; display: grid; grid-template-columns: 2.5rem minmax(0, 1fr); gap: 1rem; margin: 0 !important; padding: 0 0 1.5em !important; counter-increment: steps; }
    .process-step:not(:last-child)::before { position: absolute; top: 2rem; bottom: 0; left: 1rem; width: 1px; background: var(--rule); content: ""; }
    .process-marker { display: grid; width: 2rem; height: 2rem; place-items: center; border: 1px solid var(--rule); border-radius: 50%; background: var(--paper); color: var(--accent); font: 700 .7rem/1 var(--font-mono); }
    .process-marker::before { content: counter(steps, decimal-leading-zero); }
    .prose .process-step h3 { margin: .2em 0 .5em; font-size: 1rem; } .process-body { color: var(--muted); font-size: .9rem; line-height: 1.55; }
    .figure { margin: 2.5em 0; } .figure-body { overflow: auto; border: 1px solid var(--rule); border-radius: .55rem; background: var(--paper-deep); }
    .figure figcaption { margin-top: .75em; color: var(--faint); font-size: .75rem; line-height: 1.5; }
    .prose :where(svg text:not([fill])) { fill: currentColor; }
    @media (max-width: 560px) { .card-grid, .metric-grid { grid-template-columns: 1fr; } .prose { font-size: 15px; } }
  `;
}
