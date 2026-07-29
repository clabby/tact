import { mkdir, rm } from "node:fs/promises";
import { join } from "node:path";
import { reviewBootstrap, reviewFixtures } from "./dev-fixture";

const outputDirectory = join(import.meta.dir, ".dev");
await rm(outputDirectory, { recursive: true, force: true });
await mkdir(outputDirectory, { recursive: true });

const build = await Bun.build({
  entrypoints: [join(import.meta.dir, "app.ts")],
  outdir: outputDirectory,
  target: "browser",
  sourcemap: "inline",
  naming: "[name].[ext]",
});
if (!build.success) {
  for (const message of build.logs) console.error(message);
  process.exit(1);
}

await Bun.write(
  join(outputDirectory, "index.html"),
  Bun.file(join(import.meta.dir, "index.html")),
);

const server = Bun.serve({
  port: Number(process.env.PORT ?? 4173),
  async fetch(request) {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/api/review") {
      return Response.json(reviewBootstrap);
    }
    if (request.method === "POST" && url.pathname === "/api/scope") {
      const body = await request.json() as { scope?: keyof typeof reviewFixtures };
      const fixture = body.scope ? reviewFixtures[body.scope] : undefined;
      if (!fixture) return Response.json({ error: "Unknown review scope" }, { status: 422 });
      await Bun.sleep(350);
      return Response.json(fixture);
    }
    if (request.method === "POST" && url.pathname === "/api/decision") {
      console.log("\nReview result:\n", JSON.stringify(await request.json(), null, 2));
      return new Response(null, { status: 204 });
    }
    if (request.method === "POST" && url.pathname === "/api/cancel") {
      console.log("\nReview cancelled");
      return new Response(null, { status: 204 });
    }

    const name = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
    if (!["index.html", "app.js", "app.css"].includes(name)) {
      return new Response("Not found", { status: 404 });
    }
    return new Response(Bun.file(join(outputDirectory, name)));
  },
});

console.log(`Tact review UI: http://localhost:${server.port}`);
