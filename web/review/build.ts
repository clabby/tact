import { mkdir, rm } from "node:fs/promises";
import { join } from "node:path";
import { reviewEntrypoints, reviewScriptAssets } from "./build-config";

const outputDirectory = join(import.meta.dir, "dist");
await rm(outputDirectory, { recursive: true, force: true });
await mkdir(outputDirectory, { recursive: true });

const result = await Bun.build({
  entrypoints: reviewEntrypoints,
  outdir: outputDirectory,
  target: "browser",
  minify: true,
  naming: "[name].[ext]",
});
if (!result.success) {
  for (const message of result.logs) console.error(message);
  process.exit(1);
}

await Bun.write(
  join(outputDirectory, "index.html"),
  Bun.file(join(import.meta.dir, "index.html")),
);
await Bun.write(
  join(outputDirectory, "LICENSE.md"),
  Bun.file(join(import.meta.dir, "..", "..", "LICENSE.md")),
);
await Bun.write(
  join(outputDirectory, "favicon.svg"),
  Bun.file(join(import.meta.dir, "..", "..", "assets", "favicon.svg")),
);

const contentTypes: Record<string, string> = {
  "index.html": "text/html; charset=utf-8",
  "app.css": "text/css; charset=utf-8",
  "favicon.svg": "image/svg+xml",
  "LICENSE.md": "text/markdown; charset=utf-8",
};
for (const asset of reviewScriptAssets) {
  contentTypes[asset] = "text/javascript; charset=utf-8";
}
const files = await Promise.all(
  Object.keys(contentTypes).map(async (path) => {
    const file = Bun.file(join(outputDirectory, path));
    const bytes = await file.bytes();
    return {
      path,
      content_type: contentTypes[path],
      bytes: bytes.byteLength,
      sha256: new Bun.CryptoHasher("sha256").update(bytes).digest("hex"),
    };
  }),
);
await Bun.write(
  join(outputDirectory, "manifest.json"),
  `${JSON.stringify({
    schema_version: 2,
    review_api: { min: 4, max: 4 },
    tact: { version: process.env.TACT_VERSION ?? "development" },
    entrypoint: "index.html",
    files,
  }, null, 2)}\n`,
);
