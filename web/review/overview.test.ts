import { describe, expect, test } from "bun:test";
import { overviewDocument } from "./overview";

describe("agent overview document", () => {
  test.each([
    ["light", "color-scheme: light;"],
    ["dark", "color-scheme: dark;"],
    ["system", "color-scheme: light dark;"],
  ] as const)("uses the %s appearance", (appearance, declaration) => {
    const document = overviewDocument("# Overview", appearance);
    expect(document).toContain(declaration);
    expect(document).toContain(`data-theme="${appearance}"`);
    expect(document).toContain("<h1>Overview</h1>");
  });

  test("renders GFM and the supported static components", () => {
    const mdx = `# Review

| Area | Change |
| --- | --- |
| UI | **Overview** |

<Callout tone="idea" title="Inspect first">
Review the **diff**.
</Callout>

<CardGrid>
<Card title="API" label="Changed">
A detail.
</Card>
</CardGrid>

<MetricGrid>
<Metric value="3" label="Files" detail="Touched" />
</MetricGrid>

<Process>
<ProcessStep title="Read">
Start here.
</ProcessStep>
</Process>

<Figure caption="Overview figure">
A captioned block.
</Figure>`;
    const document = overviewDocument(mdx, "light");
    for (const fragment of [
      "<table>", "<strong>Overview</strong>", '<aside class="callout callout--idea">',
      "<strong>Inspect first</strong>", '<div class="card-grid">', "<h3>API</h3>",
      '<div class="metric-grid">', "<strong>3</strong>", "<small>Touched</small>",
      '<ol class="process">', '<li class="process-step">', "<figcaption>Overview figure</figcaption>",
    ]) expect(document).toContain(fragment);
    expect(document).toContain("--accent: #315f36");
    expect(document).toContain("--accent: #9bc59e");
  });

  test("renders inline component contents used by the development overview", () => {
    const document = overviewDocument(`<Callout tone="idea" title="Follow the data">Trace the selected range.</Callout>

<Process>
  <ProcessStep title="Choose">The browser chooses a range.</ProcessStep>
</Process>`, "light");
    expect(document).toContain('<aside class="callout callout--idea"><strong>Follow the data</strong>Trace the selected range.</aside>');
    expect(document).toContain('<ol class="process"><li class="process-step"><h3>Choose</h3>The browser chooses a range.</li></ol>');
  });

  test("strips execution, raw markup, unknown components, unsafe links and images", () => {
    const document = overviewDocument(`import Bad from "./bad"

export const value = 1

# Safe {window.evil()}

<script>alert(1)</script>

<Bad onClick={alert(2)}>malicious</Bad>

<Callout title={alert(3)} tone="note" onClick="evil()">Safe text</Callout>

[bad](javascript:alert(4)) [good](https://example.org)

![remote](https://example.org/track.png)

<div style="color:red">Raw HTML</div>`, "dark");
    for (const fragment of ["<script", "alert(", "onClick", "<img", "<div style", "<Bad", "javascript:", "malicious", "Raw HTML", "import Bad", "export const"]) {
      expect(document).not.toContain(fragment);
    }
    expect(document).toContain('<a href="https://example.org">good</a>');
    expect(document).toContain("<aside class=\"callout callout--note\">");
    expect(document).toContain("Safe text");
    expect(document).toContain("default-src 'none'; style-src 'unsafe-inline'");
  });

  test("escapes literal props and falls back safely for malformed MDX", () => {
    const document = overviewDocument('<Card title="&lt;img src=x onerror=alert(1)&gt;" />', "light");
    expect(document).toContain("&#x3C;img src=x onerror=alert(1)>");
    expect(document).not.toContain("<img");
    const malformed = overviewDocument("<style>body { background: red; }</style>", "light");
    expect(malformed).not.toContain("<style>body");
    expect(malformed).toContain("&lt;style&gt;");
    const unclosed = overviewDocument("# Review\n\n<Callout title=\"Open\">\nAgent text", "dark");
    expect(unclosed).toContain('<article class="prose"><pre># Review');
    expect(unclosed).toContain('&lt;Callout title="Open">');
    expect(unclosed).not.toContain('<aside class="callout');
  });
});
