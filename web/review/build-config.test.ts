import { expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { reviewEntrypoints } from "./build-config";

test("the review worker builds as a runnable browser asset", async () => {
  const outputDirectory = await mkdtemp(join(tmpdir(), "tact-review-worker-"));
  const workerEntrypoint = reviewEntrypoints.find((path) => basename(path) === "worker.js");

  try {
    expect(workerEntrypoint).toBeDefined();
    const result = await Bun.build({
      entrypoints: [workerEntrypoint!],
      outdir: outputDirectory,
      target: "browser",
      minify: true,
    });

    expect(result.success).toBe(true);
    expect(Bun.file(join(outputDirectory, "worker.js")).size).toBeGreaterThan(100_000);
  } finally {
    await rm(outputDirectory, { recursive: true, force: true });
  }
});
