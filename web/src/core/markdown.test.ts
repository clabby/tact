import { beforeAll, describe, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { viewerUrl } from "./local-files";
import { headings, markdownHtml, markdownImages, renderMarkdown } from "./markdown";

test("raw HTML is escaped, not rendered", () => {
  const html = markdownHtml('<script>alert(1)</script>\n\n<img src=x onerror="alert(1)">');
  expect(html).not.toContain("<script");
  expect(html).not.toContain("<img");
  expect(html).toContain("&lt;script&gt;");
});

test("links with other schemes are neutralized while http(s) and mailto keep their target", () => {
  expect(markdownHtml("[x](javascript:alert(1))")).toContain('href="#"');
  expect(markdownHtml("[x](data:text/html,hi)")).toContain('href="#"');
  expect(markdownHtml("[x](https://example.com)")).toContain('href="https://example.com"');
  expect(markdownHtml("[x](mailto:a@example.com)")).toContain('href="mailto:a@example.com"');
});

test("local file links keep their path and fragments stay on the page", () => {
  expect(markdownHtml("[r](/Users/me/report.md)")).toContain('href="/Users/me/report.md" target="_blank"');
  expect(markdownHtml("[r](<docs/My Notes.md:12>)")).toContain('href="docs/My Notes.md:12" target="_blank"');
  expect(markdownHtml("[r](file:///tmp/a.txt)")).toContain('href="file:///tmp/a.txt" target="_blank"');
  expect(markdownHtml("[s](#next-steps)")).toContain('<a href="#next-steps">s</a>');
});

test("dollar and bracket delimiters mark math for typesetting", () => {
  expect(markdownHtml("Euler: $e^{i\\pi} + 1 = 0$.")).toContain('<code class="language-math">e^{i\\pi} + 1 = 0</code>');
  expect(markdownHtml("\\(a+b\\)")).toContain('<code class="language-math">a+b</code>');
  expect(markdownHtml("a $$x^2$$ b")).toContain('<code class="language-math math-display">x^2</code>');
  expect(markdownHtml("intro\n$$\nE = mc^2\n$$\nafter")).toContain('<pre><code class="language-math">E = mc^2</code></pre>');
  expect(markdownHtml("\\[\n\\int_0^1 x\\,dx\n\\]")).toContain('<pre><code class="language-math">\\int_0^1 x\\,dx</code></pre>');
  expect(markdownHtml("$a < b$")).toContain('<code class="language-math">a &lt; b</code>');
});

test("prices, shell variables, escapes, and code stay text", () => {
  for (const text of ["costs $5 and $10", "echo $HOME/$USER", "set PATH=$PATH:$HOME", "\\$x$", "`$x$`"]) {
    expect(markdownHtml(text)).not.toContain("language-math");
  }
});

test("headings get unique slugs that the outline reports in document order", () => {
  const source = "# Done\n\n## Next `steps` & risks\n\n## Done\n\n> ### Quoted";
  expect(headings(source)).toEqual([
    { depth: 1, text: "Done", slug: "done" },
    { depth: 2, text: "Next steps & risks", slug: "next-steps-risks" },
    { depth: 2, text: "Done", slug: "done-2" },
    { depth: 3, text: "Quoted", slug: "quoted" },
  ]);
  expect(markdownHtml(source)).toContain('<h2 data-heading="done-2">Done</h2>');
});

test("many repeated headings preserve suffix collisions and reset between documents", () => {
  const titles = ["Same-4", "Same", "Same", "Same-2", "Same", "Same-2", ...Array(2000).fill("Same")];
  const source = titles.map((title) => `## ${title}`).join("\n\n");
  const expected = [
    "same-4", "same", "same-2", "same-2-2", "same-3", "same-2-3",
    ...Array.from({ length: 2000 }, (_, index) => `same-${index + 5}`),
  ];

  expect(headings(source).map(({ slug }) => slug)).toEqual(expected);
  expect(headings("## Same\n\n## Same").map(({ slug }) => slug)).toEqual(["same", "same-2"]);
});

test("footnotes link to definitions collected after the text", () => {
  const html = markdownHtml("Claim[^a].\n\n[^a]: The *source*.");
  expect(html).toContain('href="#footnote-a"');
  expect(html).toMatch(/<section[^>]*>[\s\S]*<li id="footnote-a">[\s\S]*The <em>source<\/em>/);
});

test("embedded images are listed in order as written", () => {
  expect(markdownImages("![one](a.png) text\n\n- ![two](/abs/b.png)")).toEqual([
    { destination: "a.png", alt: "one" },
    { destination: "/abs/b.png", alt: "two" },
  ]);
});

describe("rendered links", () => {
  beforeAll(() => {
    const window = new Window();
    Object.assign(globalThis, { document: window.document, Node: window.Node, CSS: window.CSS });
  });

  async function render(source: string, fileLink?: (destination: string) => string | null) {
    const container = document.createElement("div");
    document.body.append(container);
    await renderMarkdown(container, source, "pierre-light", { highlight: false, fileLink });
    return container;
  }

  test("a local file link opens the viewer instead of a path on this server", async () => {
    const container = await render(
      "[report](/Users/me/scratch/report.md) and [site](https://example.com)",
      (destination) => viewerUrl(destination, { session: "s1" }),
    );
    const [report, site] = container.querySelectorAll("a");

    expect(report!.getAttribute("href")).toBe("./?file=%2FUsers%2Fme%2Fscratch%2Freport.md&session=s1");
    expect(report!.getAttribute("target")).toBe("_blank");
    expect(site!.getAttribute("href")).toBe("https://example.com");
    expect(site!.getAttribute("target")).toBe("_blank");
  });

  test("a local file link is inert where nothing can show the file", async () => {
    const container = await render("[report](/Users/me/report.md)");
    const link = container.querySelector("a")!;

    expect(link.hasAttribute("href")).toBe(false);
    expect(link.hasAttribute("target")).toBe(false);
    expect(link.textContent).toBe("report");
  });

  test("a fragment is normalized to the heading slug", async () => {
    const container = await render("[jump](#next-steps--risks)\n\n## Next steps & risks");
    const heading = container.querySelector<HTMLElement>("h2")!;
    let scrolled = false;
    heading.scrollIntoView = () => {
      scrolled = true;
    };

    const click = new (document.defaultView!.MouseEvent)("click", { bubbles: true, cancelable: true });
    container.querySelector("a")!.dispatchEvent(click);

    expect(click.defaultPrevented).toBe(true);
    expect(scrolled).toBe(true);
  });

  test("a heading whose slug starts with footnote still accepts fragment links", async () => {
    const container = await render("[jump](#footnote-details)\n\n## Footnote details");
    let scrolled = false;
    container.querySelector<HTMLElement>("h2")!.scrollIntoView = () => { scrolled = true; };

    container.querySelector<HTMLAnchorElement>("a")!.click();

    expect(scrolled).toBe(true);
  });
});
