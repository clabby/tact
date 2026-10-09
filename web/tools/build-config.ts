import { join } from "node:path";

const appEntrypoint = join(import.meta.dir, "..", "src", "app.ts");
export const workerEntrypoint = join(
  import.meta.dir,
  "..",
  "node_modules",
  "@pierre",
  "diffs",
  "dist",
  "worker",
  "worker.js",
);

/**
 * Builds the browser code into `outdir`. The app is split so heavy features load on first use;
 * every output lands at the top level so `import.meta.url` resolves the same way in each chunk. The
 * diff worker is built on its own because it runs as a classic worker and cannot import chunks.
 * Returns false after logging the errors of a failed build.
 */
export async function buildApp(
  outdir: string,
  options: { minify?: boolean; sourcemap?: "inline" | "none" } = {},
) {
  const builds = await Promise.all([
    Bun.build({
      ...options,
      entrypoints: [appEntrypoint],
      outdir,
      target: "browser",
      splitting: true,
      naming: { entry: "[name].[ext]", chunk: "[name]-[hash].[ext]", asset: "[name]-[hash].[ext]" },
    }),
    Bun.build({ ...options, entrypoints: [workerEntrypoint], outdir, target: "browser", naming: "[name].[ext]" }),
  ]);
  for (const build of builds) {
    if (!build.success) for (const message of build.logs) console.error(message);
  }
  return builds.every((build) => build.success);
}
