import { expect, test } from "bun:test";
import { headings, markdownHtml, markdownImages } from "./markdown";

test("raw HTML is escaped, not rendered", () => {
  const html = markdownHtml('<script>alert(1)</script>\n\n<img src=x onerror="alert(1)">');
  expect(html).not.toContain("<script");
  expect(html).not.toContain("<img");
  expect(html).toContain("&lt;script&gt;");
});

test("only http(s) and mailto links keep their target", () => {
  expect(markdownHtml("[x](javascript:alert(1))")).toContain('href="#"');
  expect(markdownHtml("[x](data:text/html,hi)")).toContain('href="#"');
  expect(markdownHtml("[x](https://example.com)")).toContain('href="https://example.com"');
  expect(markdownHtml("[x](mailto:a@example.com)")).toContain('href="mailto:a@example.com"');
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
