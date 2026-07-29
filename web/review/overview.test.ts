import { describe, expect, test } from "bun:test";
import { overviewDocument } from "./overview";

describe("agent overview document", () => {
  test.each([
    ["light", "color-scheme: light;"],
    ["dark", "color-scheme: dark;"],
    ["system", "color-scheme: light dark;"],
  ] as const)("uses the %s appearance", (appearance, declaration) => {
    expect(overviewDocument("<h1>Overview</h1>", appearance)).toContain(declaration);
  });

  test("reviewer colors override colors supplied by the agent", () => {
    const document = overviewDocument(
      '<section style="color: black; background: white"><a>Details</a></section>',
      "dark",
    );

    expect(document).toContain(".agent-overview, .agent-overview *");
    expect(document).toContain("color: inherit !important;");
    expect(document).toContain("background-color: transparent !important;");
    expect(document).toContain(".agent-overview a { color: var(--overview-accent) !important; }");
  });
});
