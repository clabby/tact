import { join } from "node:path";

export const reviewEntrypoints = [
  join(import.meta.dir, "app.ts"),
  join(
    import.meta.dir,
    "node_modules",
    "@pierre",
    "diffs",
    "dist",
    "worker",
    "worker.js",
  ),
];

export const reviewScriptAssets = ["app.js", "worker.js"];
