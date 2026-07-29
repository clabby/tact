import { watch } from "node:fs";
import { mkdir, rm } from "node:fs/promises";
import { join } from "node:path";
import { overviewFixtures, reviewBootstrap, reviewFixtures } from "./dev-fixture";
import { rangeKey, type ReviewRange } from "./range-selection";
import type { QuestionRequest } from "./protocol";

const outputDirectory = join(import.meta.dir, ".dev");
await rm(outputDirectory, { recursive: true, force: true });
await mkdir(outputDirectory, { recursive: true });

async function buildAssets() {
  const build = await Bun.build({
    entrypoints: [join(import.meta.dir, "app.ts")],
    outdir: outputDirectory,
    target: "browser",
    sourcemap: "inline",
    naming: "[name].[ext]",
  });
  if (!build.success) {
    for (const message of build.logs) console.error(message);
    return false;
  }
  const html = await Bun.file(join(import.meta.dir, "index.html")).text();
  await Bun.write(join(outputDirectory, "index.html"), html.replace(
    "</body>",
    '<script>const socket=new WebSocket(`ws://${location.host}/__reload`);socket.onmessage=()=>location.reload()</script></body>',
  ));
  return true;
}

if (!await buildAssets()) process.exit(1);

let workspaceChanged = true;
const questionCancellations = new Map<string, () => void>();

const server = Bun.serve({
  port: Number(process.env.PORT ?? 4173),
  async fetch(request) {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/api/review") {
      return Response.json(reviewBootstrap);
    }
    if (request.method === "GET" && url.pathname === "/api/status") {
      return Response.json({ generation: 1, changed: workspaceChanged });
    }
    if (request.method === "POST" && url.pathname === "/api/refresh") {
      workspaceChanged = false;
      return Response.json(reviewBootstrap);
    }
    if (request.method === "POST" && url.pathname === "/api/range") {
      const body = await request.json() as { generation?: number; range?: ReviewRange };
      const key = body.range ? rangeKey(body.range) : "";
      const fixture = reviewFixtures[key as keyof typeof reviewFixtures];
      if (!fixture) return Response.json({ error: "Unknown review range" }, { status: 422 });
      await Bun.sleep(350);
      return Response.json(fixture);
    }
    if (request.method === "POST" && url.pathname === "/api/overview") {
      const body = await request.json() as { generation?: number; range?: ReviewRange };
      const key = body.range ? rangeKey(body.range) : "";
      const overview = overviewFixtures[key as keyof typeof overviewFixtures];
      if (!overview) return Response.json({ error: "Unknown review range" }, { status: 422 });
      await Bun.sleep(900);
      return Response.json({ generation: body.generation, selected_range: body.range, overview_html: overview });
    }
    if (request.method === "POST" && url.pathname === "/api/question") {
      const body = await request.json() as QuestionRequest;
      const cancelled = await Promise.race([
        Bun.sleep(900).then(() => false),
        new Promise<boolean>((resolve) => {
          questionCancellations.set(body.operation_id, () => resolve(true));
        }),
      ]);
      questionCancellations.delete(body.operation_id);
      if (cancelled) {
        return Response.json({
          code: "operation_cancelled",
          error: "question answering was cancelled",
        }, { status: 409 });
      }
      return Response.json({
        generation: body.generation,
        selected_range: body.range,
        answer: `This thread is anchored to \`${body.path}:${body.start_line}${body.end_line === body.start_line ? "" : `-${body.end_line}`}\`. In a real review, Tact asks a sub-agent to inspect the surrounding code and answer with that context.`,
      });
    }
    if (request.method === "POST" && url.pathname === "/api/question/cancel") {
      const body = await request.json() as { operation_id: string };
      questionCancellations.get(body.operation_id)?.();
      return new Response(null, { status: 204 });
    }
    if (request.method === "POST" && url.pathname === "/api/decision") {
      console.log("\nReview result:\n", JSON.stringify(await request.json(), null, 2));
      return new Response(null, { status: 204 });
    }
    if (request.method === "POST" && url.pathname === "/api/cancel") {
      console.log("\nReview cancelled");
      return new Response(null, { status: 204 });
    }

    if (url.pathname === "/__reload" && server.upgrade(request)) return;

    const name = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
    if (!["index.html", "app.js", "app.css"].includes(name)) {
      return new Response("Not found", { status: 404 });
    }
    return new Response(Bun.file(join(outputDirectory, name)));
  },
  websocket: {
    open(socket) { socket.subscribe("reload"); },
    message() {},
  },
});

console.log(`Tact review UI: http://localhost:${server.port}`);

let rebuildTimer: ReturnType<typeof setTimeout> | undefined;
watch(import.meta.dir, { recursive: true }, (_event, filename) => {
  if (!filename || filename.startsWith(".dev/") || filename.startsWith("dist/")) return;
  clearTimeout(rebuildTimer);
  rebuildTimer = setTimeout(async () => {
    if (!await buildAssets()) return;
    server.publish("reload", "reload");
    console.log("Review UI rebuilt");
  }, 80);
});
