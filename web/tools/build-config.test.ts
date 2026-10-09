import { expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { workerEntrypoint } from "./build-config";

test("the review worker builds as a runnable browser asset", async () => {
  const outputDirectory = await mkdtemp(join(tmpdir(), "tact-web-worker-"));

  try {
    const result = await Bun.build({
      entrypoints: [workerEntrypoint],
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
