import { expect, test } from "bun:test";

test("the diff view owns its scrollable viewport", async () => {
  const styles = await Bun.file(new URL("styles.css", import.meta.url)).text();
  const diffViewRule = styles.match(/\.diff-view\s*{([^}]*)}/)?.[1];

  expect(diffViewRule).toBeDefined();
  expect(diffViewRule).toMatch(/overflow:\s*auto/);
});
