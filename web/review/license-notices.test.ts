import { describe, expect, test } from "bun:test";

describe("third-party notices", () => {
  test("cover every direct browser dependency", async () => {
    const packageJson = await Bun.file(new URL("package.json", import.meta.url)).json();
    const notices = await Bun.file(
      new URL("../../THIRD_PARTY_NOTICES.md", import.meta.url),
    ).text();

    for (const dependency of Object.keys(packageJson.dependencies)) {
      expect(notices, `${dependency} is missing from THIRD_PARTY_NOTICES.md`).toContain(dependency);
    }
  });

  test("cover Shiki's bundled syntax engine dependencies", async () => {
    const notices = await Bun.file(
      new URL("../../THIRD_PARTY_NOTICES.md", import.meta.url),
    ).text();
    const bundledDependencies = [
      "@shikijs/vscode-textmate",
      "oniguruma-to-es",
      "oniguruma-parser",
      "regex-recursion",
      "regex-utilities",
    ];

    for (const dependency of bundledDependencies) {
      expect(notices, `${dependency} is missing from THIRD_PARTY_NOTICES.md`).toContain(dependency);
    }
  });
});
