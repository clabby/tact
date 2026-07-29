import { mkdir, rm } from "node:fs/promises";
import { join } from "node:path";

const outputDirectory = join(import.meta.dir, "dist");
await rm(outputDirectory, { recursive: true, force: true });
await mkdir(outputDirectory, { recursive: true });

const result = await Bun.build({
  entrypoints: [join(import.meta.dir, "app.ts")],
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
  join(outputDirectory, "THIRD_PARTY_NOTICES.md"),
  Bun.file(join(import.meta.dir, "..", "..", "THIRD_PARTY_NOTICES.md")),
);
await Bun.write(
  join(outputDirectory, "LICENSE.md"),
  Bun.file(join(import.meta.dir, "..", "..", "LICENSE.md")),
);

const files = await Promise.all(
  ["index.html", "app.css", "app.js", "LICENSE.md", "THIRD_PARTY_NOTICES.md"].map(async (name) => {
    const file = Bun.file(join(outputDirectory, name));
    const bytes = await file.bytes();
    return {
      name,
      bytes: bytes.byteLength,
      sha256: new Bun.CryptoHasher("sha256").update(bytes).digest("hex"),
    };
  }),
);
await Bun.write(
  join(outputDirectory, "manifest.json"),
  `${JSON.stringify({ version: 1, files }, null, 2)}\n`,
);
