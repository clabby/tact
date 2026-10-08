import { expect, test } from "bun:test";
import { markdownHtml } from "./markdown";

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
