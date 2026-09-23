import { describe, expect, test } from "bun:test";
import { runInNewContext } from "node:vm";
import { overviewFrameDocument, overviewProgram } from "./overview";

class Element {
  children: (Element | string)[] = [];
  attributes: Record<string, string> = {};
  textContent = "";
  style = { setProperty: (name: string, value: string) => { this.attributes[`style:${name}`] = value; } };
  constructor(readonly tag: string) {}
  appendChild(child: Element | string) { this.children.push(child); }
  replaceChildren() { this.children = []; }
  setAttribute(name: string, value: string) { this.attributes[name] = value; }
  hasAttribute(name: string) { return name in this.attributes; }
  addEventListener() {}
}
class SvgElement extends Element {}

function renderOverview(mdx: string) {
  const html = overviewFrameDocument();
  const script = html.match(/<script>([\s\S]*?)<\/script>/)?.[1];
  expect(script).toBeDefined();
  const root = new Element("article");
  const parent = {};
  let onMessage: (event: { source: object; data: unknown }) => void = () => {};
  const document = {
    documentElement: { dataset: { theme: "system" } },
    getElementById: () => root,
    createTextNode: (text: string) => text,
    createElement: (tag: string) => new Element(tag),
    createElementNS: (_namespace: string, tag: string) => new SvgElement(tag),
  };
  runInNewContext(script!, {
    document, SVGElement: SvgElement,
    window: { parent, addEventListener: (_type: string, handler: typeof onMessage) => { onMessage = handler; } },
  });
  onMessage({ source: parent, data: { type: "tact-overview", code: overviewProgram(mdx), appearance: "dark" } });
  return { root, document, html };
}

function nodes(root: Element): Element[] {
  return root.children.flatMap((child) => child instanceof Element ? [child, ...nodes(child)] : []);
}

describe("agent overview MDX", () => {
  test("renders reusable JavaScript components, computed SVG charts and built-in components", async () => {
    const mdx = `export const Flow = ({ label }) => <section className="flow"><strong>{label}</strong><svg viewBox="0 0 200 80" preserveAspectRatio="xMidYMid meet"><rect x={10} y={10} width={120} height={30} strokeWidth={2} strokeLinecap="round" markerEnd="url(#tip)" fill="var(--accent)" /><text x={20} y={30} textAnchor="middle" dominantBaseline="middle" textLength="50">Built {2 + 1}</text></svg></section>

# Review

<Callout title="Review path" tone="idea">Inspect the **diff**.</Callout>

<Flow label="Pipeline" />
<Flow label="Second" />`;
    const { root, document } = renderOverview(mdx);
    await Bun.sleep(0);
    const rendered = nodes(root);
    expect(rendered.filter((node) => node.attributes.class === "flow")).toHaveLength(2);
    expect(rendered.filter((node) => node.tag === "svg")).toHaveLength(2);
    expect(rendered.find((node) => node.tag === "rect")?.attributes).toMatchObject({ x: "10", "stroke-width": "2", "stroke-linecap": "round", "marker-end": "url(#tip)", fill: "var(--accent)" });
    expect(rendered.find((node) => node.tag === "text")?.attributes).toMatchObject({
      "text-anchor": "middle", "dominant-baseline": "middle", textLength: "50",
    });
    expect(rendered.find((node) => node.tag === "svg")?.attributes).toMatchObject({
      viewBox: "0 0 200 80", preserveAspectRatio: "xMidYMid meet",
    });
    expect(rendered.find((node) => node.tag === "text")?.children.join("")).toBe("Built 3");
    expect(rendered.find((node) => node.tag === "aside")?.attributes.class).toBe("callout callout--idea");
    expect(document.documentElement.dataset.theme).toBe("dark");
  });

  test("compiles GFM, JSX expressions, event handlers and inline component definitions", () => {
    const program = overviewProgram(`export const Chart = ({ data }) => <svg viewBox="0 0 100 100">{data.map((value, index) => <circle key={index} cx={index * 20} cy={value} r="4" />)}</svg>

| Area | Value |
| --- | --- |
| Rendering | **MDX** |

<Chart data={[12, 34, 56]} />`);
    expect(program).toContain("const Chart");
    expect(program).toContain("data.map(");
    expect(program).toContain('"table"');
    expect(program).toContain('"circle"');
  });

  test("compilation never evaluates authored JavaScript in the review app", async () => {
    const source = `{globalThis.__tactOverviewIsolated = 42}`;
    const program = overviewProgram(source);
    expect(program).toContain("__tactOverviewIsolated");
    expect((globalThis as { __tactOverviewIsolated?: number }).__tactOverviewIsolated).toBeUndefined();
    const { root } = renderOverview(source);
    await Bun.sleep(0);
    expect(JSON.stringify(root.children)).toContain("42");
    expect((globalThis as { __tactOverviewIsolated?: number }).__tactOverviewIsolated).toBeUndefined();
  });

  test("keeps authored code inside the opaque-origin iframe with network-denying CSP", () => {
    const { html } = renderOverview("# Safe");
    expect(html).toContain("default-src 'none'");
    expect(html).toContain("connect-src 'none'");
    expect(html).toContain("script-src 'unsafe-inline' 'unsafe-eval'");
    expect(html).toContain("--accent: #315f36");
    expect(html).toContain("--accent: #9bc59e");
    expect(html).not.toContain("allow-same-origin");
    expect(html).not.toContain("<script src=");
  });

  test("reports syntax errors without interpreting malformed markup in the parent", async () => {
    const { root } = renderOverview("<Card title=\"Open\">\nBroken");
    await Bun.sleep(0);
    expect(nodes(root).find((node) => node.tag === "pre")?.children.join("")).toContain("Expected a closing tag");
  });
});
