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
  ".woff2": "font/woff2",
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
// Tact refuses a bundle past these limits (`bin/tact/src/web/assets.rs`), so the build fails
// first rather than a user's install.
const MAX_FILE_BYTES = 16 * 1024 * 1024;
const MAX_EXPANDED_BYTES = 32 * 1024 * 1024;
const MAX_MANIFEST_BYTES = 1024 * 1024;
const mebibytes = (bytes: number) => `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
const manifest = `${JSON.stringify({
    schema_version: 2,
    web_api: { min: 9, max: 9 },
    tact: { version: process.env.TACT_VERSION ?? "development" },
    entrypoint: "index.html",
    files,
  }, null, 2)}\n`;
const total = files.reduce((sum, file) => sum + file.bytes, 0);
const problems = [
  ...files.filter((file) => file.bytes > MAX_FILE_BYTES)
    .map((file) => `${file.path} is ${mebibytes(file.bytes)}, over the ${mebibytes(MAX_FILE_BYTES)} file limit`),
  total > MAX_EXPANDED_BYTES ? `the bundle is ${mebibytes(total)}, over the ${mebibytes(MAX_EXPANDED_BYTES)} limit` : "",
  manifest.length > MAX_MANIFEST_BYTES ? `the manifest is ${mebibytes(manifest.length)}, over the ${mebibytes(MAX_MANIFEST_BYTES)} limit` : "",
].filter(Boolean);
if (problems.length) {
  console.error(`Tact would refuse this bundle: ${problems.join("; ")}`);
  process.exit(1);
}
console.log(`Bundle: ${files.length} files, ${mebibytes(total)} of ${mebibytes(MAX_EXPANDED_BYTES)}`);
await Bun.write(join(outputDirectory, "manifest.json"), manifest);
