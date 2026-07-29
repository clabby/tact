import { describe, expect, test } from "bun:test";
import { overviewDocument } from "./overview";

describe("agent overview document", () => {
  test.each([
    ["light", "color-scheme: light;"],
    ["dark", "color-scheme: dark;"],
    ["system", "color-scheme: light dark;"],
  ] as const)("uses the %s appearance", (appearance, declaration) => {
    const document = overviewDocument("<h1>Overview</h1>", appearance);
    expect(document).toContain(declaration);
    expect(document).toContain(`data-theme="${appearance}"`);
  });

  test("agent-authored visual identity is preserved", () => {
    const document = overviewDocument(
      '<section style="color: black; background: white"><a>Details</a></section>',
      "dark",
    );

    expect(document).toContain('<section style="color: black; background: white">');
    expect(document).not.toContain("!important");
    expect(document).toContain("default-src 'none'; style-src 'unsafe-inline'");
  });

  test("agent styles take precedence over the host fallback", () => {
    const document = overviewDocument(
      "<style>body { background: rebeccapurple; }</style>",
      "dark",
    );

    expect(document.indexOf("--overview-bg")).toBeLessThan(
      document.indexOf("rebeccapurple"),
    );
  });
});
