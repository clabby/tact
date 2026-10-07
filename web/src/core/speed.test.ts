import { expect, test } from "bun:test";
import { effectiveSpeed, speedChoices } from "./speed";
import type { ModelCatalog, ModelInfo } from "./wire";

const catalog: ModelCatalog = { models: [], efforts: [], speeds: ["standard", "fast", "ultrafast"] };
const model = (effective_speeds: ModelInfo["effective_speeds"]) => ({ effective_speeds }) as ModelInfo;

test("a model offers each distinct tier it runs, and nothing it would downgrade", () => {
  expect(speedChoices(model(["standard", "standard", "standard"]), catalog)).toEqual([{ preference: "standard", tier: "standard" }]);
  expect(speedChoices(model(["standard", "fast", "fast"]), catalog)).toEqual([
    { preference: "standard", tier: "standard" },
    { preference: "fast", tier: "fast" },
  ]);
  expect(speedChoices(model(["standard", "fast", "ultrafast"]), catalog)).toHaveLength(3);
});

test("a saved preference is shown as the tier it runs at", () => {
  expect(effectiveSpeed(model(["standard", "fast", "fast"]), catalog, "ultrafast")).toBe("fast");
  expect(effectiveSpeed(undefined, catalog, "fast")).toBe("fast");
});
