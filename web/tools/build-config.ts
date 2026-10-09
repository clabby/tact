import { copyFile, readdir } from "node:fs/promises";
import { join } from "node:path";

const modules = join(import.meta.dir, "..", "node_modules");
const appEntrypoint = join(import.meta.dir, "..", "src", "app.ts");
export const workerEntrypoint = join(modules, "@pierre", "diffs", "dist", "worker", "worker.js");

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
  if (!builds.every((build) => build.success)) return false;
  await copyKatexStyles(outdir);
  return true;
}

/**
 * Copies KaTeX's stylesheet and its WOFF2 fonts. Bundling the stylesheet would inline every font as
 * a data URL, about 1.5 MB of CSS on every page load; as files, a browser fetches only the fonts a
 * formula uses. Every supported browser reads WOFF2, so the WOFF and TrueType fallbacks are dropped.
 */
async function copyKatexStyles(outdir: string) {
  const katex = join(modules, "katex", "dist");
  const css = await Bun.file(join(katex, "katex.min.css")).text();
  const woff2Only = css
    .replace(/,url\(fonts\/[^)]+\.(?:woff|ttf)\) format\("(?:woff|truetype)"\)/g, "")
    .replaceAll("url(fonts/", "url(");
  if (/url\((?!KaTeX_[\w-]+\.woff2\))/.test(woff2Only)) throw new Error("unexpected url() in katex.min.css");
  await Bun.write(join(outdir, "katex.css"), woff2Only);
  const fonts = (await readdir(join(katex, "fonts"))).filter((name) => name.endsWith(".woff2"));
  await Promise.all(fonts.map((name) => copyFile(join(katex, "fonts", name), join(outdir, name))));
}
