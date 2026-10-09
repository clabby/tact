import { mkdir, readdir, rm } from "node:fs/promises";
import { extname, join } from "node:path";
import { overviewFrameDocument } from "../src/review/overview";
import { buildApp } from "./build-config";

const outputDirectory = join(import.meta.dir, "..", "dist");
await rm(outputDirectory, { recursive: true, force: true });
await mkdir(outputDirectory, { recursive: true });

if (!await buildApp(outputDirectory, { minify: true })) process.exit(1);

await Bun.write(join(outputDirectory, "overview-frame.html"), overviewFrameDocument());
await Bun.write(
  join(outputDirectory, "index.html"),
  Bun.file(join(import.meta.dir, "..", "index.html")),
);
await Bun.write(
  join(outputDirectory, "LICENSE.md"),
  Bun.file(join(import.meta.dir, "..", "..", "LICENSE.md")),
);
await Bun.write(
  join(outputDirectory, "FONT-AWESOME-LICENSE.txt"),
  Bun.file(join(
    import.meta.dir,
    "..",
    "node_modules",
    "@fortawesome",
    "free-solid-svg-icons",
    "LICENSE.txt",
  )),
);
await Bun.write(
  join(outputDirectory, "favicon.svg"),
  Bun.file(join(import.meta.dir, "..", "..", "assets", "favicon.svg")),
);

// Every file in the bundle is listed, since the server serves nothing else. Chunk names carry
// content hashes, so the list comes from the output rather than a fixed set.
const contentTypes: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".svg": "image/svg+xml",
  ".txt": "text/plain; charset=utf-8",
  ".md": "text/markdown; charset=utf-8",
};
const entries = await readdir(outputDirectory, { withFileTypes: true });
const paths = entries.map((entry) => entry.name).sort();
const unservable = entries
  .filter((entry) => !entry.isFile() || !contentTypes[extname(entry.name)])
  .map((entry) => entry.name);
if (unservable.length) {
  console.error(`the bundle must be flat files with known content types: ${unservable.join(", ")}`);
  process.exit(1);
}
const files = await Promise.all(
  paths.map(async (path) => {
    const file = Bun.file(join(outputDirectory, path));
    const bytes = await file.bytes();
    return {
      path,
      content_type: contentTypes[extname(path)],
      bytes: bytes.byteLength,
      sha256: new Bun.CryptoHasher("sha256").update(bytes).digest("hex"),
    };
  }),
);
await Bun.write(
  join(outputDirectory, "manifest.json"),
  `${JSON.stringify({
    schema_version: 2,
    web_api: { min: 9, max: 9 },
    tact: { version: process.env.TACT_VERSION ?? "development" },
    entrypoint: "index.html",
    files,
  }, null, 2)}\n`,
);
