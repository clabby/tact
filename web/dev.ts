// Development server: serves a live-rebuilt bundle against the in-memory MockTact, so the whole
// web app can be developed and screenshotted without a Rust build. Type a line into this process's
// stdin to simulate typing it in the terminal's composer.

import { watch } from "node:fs";
import { mkdir, rm } from "node:fs/promises";
import { join } from "node:path";
import { reviewEntrypoints, reviewScriptAssets } from "./build-config";
import { overviewFixtures, reviewBootstrap, reviewFixtures } from "./dev-fixture";
import { MockRefusal, MockTact } from "./dev-mock";
import { overviewFrameDocument } from "./overview";
import type { QuestionRequest, ReviewDecision, ReviewPage, StoredOverview, StoredQuestionThread } from "./protocol";
import { rangeKey, type ReviewRange } from "./range-selection";
import type { CommandName, QueryName } from "./wire";

const token = process.env.TACT_DEV_TOKEN ?? "dev";
const port = Number(process.env.PORT ?? 4173);
const outputDirectory = join(import.meta.dir, ".dev");
const staticFiles = new Set(["index.html", "overview-frame.html", "app.css", "favicon.svg", ...reviewScriptAssets]);

async function buildAssets() {
  const build = await Bun.build({
    entrypoints: reviewEntrypoints,
    outdir: outputDirectory,
    target: "browser",
    sourcemap: "inline",
    naming: "[name].[ext]",
  });
  if (!build.success) {
    for (const message of build.logs) console.error(message);
    return false;
  }
  await Bun.write(join(outputDirectory, "overview-frame.html"), overviewFrameDocument());
  await Bun.write(join(outputDirectory, "favicon.svg"), Bun.file(join(import.meta.dir, "..", "assets", "favicon.svg")));
  const html = await Bun.file(join(import.meta.dir, "index.html")).text();
  await Bun.write(join(outputDirectory, "index.html"), html.replace(
    "</body>",
    "<script>new WebSocket(`ws://${location.host}/__reload`).onmessage=()=>location.reload()</script></body>",
  ));
  return true;
}

await rm(outputDirectory, { recursive: true, force: true });
await mkdir(outputDirectory, { recursive: true });
if (!await buildAssets()) process.exit(1);

const tact = new MockTact();
tact.startAmbientWork();
setInterval(() => tact.touchWorkspace(), 20_000);

const review = {
  page: reviewBootstrap.page as ReviewPage,
  overview: null as StoredOverview | null,
  questions: [] as StoredQuestionThread[],
  cancellations: new Map<string, () => void>(),
};

const encoder = new TextEncoder();

function stream(request: Request) {
  let close = () => {};
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      const send = (name: string, data: unknown) => {
        controller.enqueue(encoder.encode(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`));
      };
      for (const { name, data } of tact.greeting()) send(name, data);
      const unsubscribe = tact.subscribe(send);
      const keepAlive = setInterval(() => controller.enqueue(encoder.encode(": keep-alive\n\n")), 15_000);
      close = () => {
        unsubscribe();
        clearInterval(keepAlive);
      };
      request.signal.addEventListener("abort", close);
    },
    cancel() {
      close();
    },
  });
  return new Response(body, {
    headers: { "content-type": "text/event-stream", "cache-control": "no-store" },
  });
}

const failure = (code: string, message: string, status: number) => Response.json({ code, message }, { status });

function authorized(request: Request) {
  return request.headers.get("cookie")?.split(/;\s*/).includes(`tact=${token}`) ?? false;
}

async function api(request: Request, url: URL): Promise<Response> {
  const path = url.pathname.slice("/api/".length);
  const post = request.method === "POST";
  if (post && request.headers.get("x-tact") !== "1") {
    return failure("invalid_request", "Missing the X-Tact header.", 400);
  }
  if (post && path === "login") {
    const body = await request.json() as { token?: string };
    if (body.token !== token) return failure("unauthorized", "That link is no longer valid.", 401);
    return new Response(null, {
      status: 204,
      headers: { "set-cookie": `tact=${token}; HttpOnly; SameSite=Strict; Path=/` },
    });
  }
  if (!authorized(request)) return failure("unauthorized", "Open the login link printed by Tact.", 401);

  if (!post) {
    if (path === "stream") return stream(request);
    if (path === "instance") return Response.json(tact.instance());
    if (path === "instances") {
      const current = tact.instance();
      return Response.json({
        instances: [
          { pid: process.pid, port, workspace: current.workspace, live: current.live, running: current.running, current: true },
          { pid: process.pid + 1, port: port + 1, workspace: "/Users/dev/src/commonware", live: 2, running: 1, current: false },
        ],
      });
    }
    const agentEntries = path.match(/^sessions\/([^/]+)\/agents\/(\d+)\/entries$/);
    if (agentEntries) return Response.json(tact.agentTranscript(decodeURIComponent(agentEntries[1]!), Number(agentEntries[2])));
    if (path === "file") {
      return new Response(
        '<svg xmlns="http://www.w3.org/2000/svg" width="480" height="260"><rect width="480" height="260" fill="hsl(150 55% 90%)"/><circle cx="240" cy="130" r="70" fill="hsl(150 50% 50%)"/></svg>',
        { headers: { "content-type": "image/svg+xml" } },
      );
    }
    const image = path.match(/^sessions\/[^/]+\/entries\/\d+\/images\/(\d+)$/);
    if (image) {
      const hue = 200 + Number(image[1]) * 70;
      return new Response(
        '<svg xmlns="http://www.w3.org/2000/svg" width="480" height="260"><rect width="480" height="260" fill="hsl(' + hue + ' 60% 92%)"/><rect x="30" y="30" width="200" height="14" rx="4" fill="hsl(' + hue + ' 55% 55%)"/><rect x="30" y="64" width="420" height="10" rx="4" fill="hsl(' + hue + ' 30% 75%)"/><rect x="30" y="86" width="360" height="10" rx="4" fill="hsl(' + hue + ' 30% 75%)"/><rect x="30" y="140" width="130" height="80" rx="8" fill="hsl(' + hue + ' 50% 80%)"/></svg>',
        { headers: { "content-type": "image/svg+xml" } },
      );
    }
    const detail = path.match(/^sessions\/([^/]+)(?:\/agents\/\d+)?\/entries\/(\d+)$/);
    if (detail) {
      await Bun.sleep(180);
      return Response.json(tact.toolDetail(decodeURIComponent(detail[1]!), Number(detail[2])));
    }
    if (path === "link") return Response.json({ public_origin: process.env.TACT_DEV_PUBLIC_ORIGIN ?? null, token });
    if (path === "review") return Response.json(reviewSession());
    return failure("invalid_request", `Unknown endpoint ${path}.`, 404);
  }

  const body = await request.json() as Record<string, unknown>;
  if (path === "cmd") {
    await Bun.sleep(25);
    return Response.json(tact.command(body.cmd as CommandName, body.args, body.client as number));
  }
  if (path === "query") {
    await Bun.sleep(40);
    return Response.json(tact.query(body.query as QueryName, body.args));
  }
  return reviewCommand(path, body);
}

function reviewSession() {
  return {
    ...reviewBootstrap,
    page: review.page,
    overview: review.overview,
    questions: review.questions,
    turn_running: tact.instance().running > 0,
  };
}

async function reviewCommand(path: string, body: Record<string, unknown>): Promise<Response> {
  switch (path) {
    case "refresh":
      review.page = reviewBootstrap.page;
      review.overview = null;
      return Response.json(reviewSession());
    case "range": {
      const fixture = reviewFixtures[rangeKey(body.range as ReviewRange) as keyof typeof reviewFixtures];
      if (!fixture) return failure("invalid_range", "Unknown review range.", 422);
      await Bun.sleep(350);
      review.page = fixture;
      return Response.json(fixture);
    }
    case "overview": {
      const range = body.range as ReviewRange;
      const overview = overviewFixtures[rangeKey(range) as keyof typeof overviewFixtures];
      if (!overview) return failure("invalid_range", "Unknown review range.", 422);
      review.overview = { selected_range: range, status: "generating" };
      await Bun.sleep(900);
      review.overview = { selected_range: range, status: "ready", overview_mdx: overview };
      return Response.json({ generation: body.generation, selected_range: range, overview_mdx: overview });
    }
    case "ai-review":
      await Bun.sleep(900);
      return Response.json({
        generation: body.generation,
        selected_range: body.range,
        comments: [{
          path: "src/review/mod.rs", side: "additions", start_line: 3, end_line: 3,
          body: "[P2] Confirm the snapshot remains valid if the workspace changes while the review is open.",
        }],
      });
    case "question":
      return question(body as unknown as QuestionRequest);
    case "questions":
      return Response.json({ generation: reviewBootstrap.generation, questions: review.questions });
    case "question/cancel":
      review.cancellations.get(body.operation_id as string)?.();
      return new Response(null, { status: 204 });
    case "review/compose":
      return Response.json({ markdown: composeReview(body as unknown as ReviewDecision) });
  }
  return failure("invalid_request", `Unknown endpoint ${path}.`, 404);
}

async function question(body: QuestionRequest) {
  const thread: StoredQuestionThread = {
    ...body,
    messages: body.messages.map((message) => ({ ...message })),
    status: "asking",
  };
  const existing = review.questions.findIndex((candidate) => candidate.thread_id === body.thread_id);
  if (existing >= 0) review.questions[existing] = thread;
  else review.questions.push(thread);

  const cancelled = await Promise.race([
    Bun.sleep(900).then(() => false),
    new Promise<boolean>((resolve) => review.cancellations.set(body.operation_id, () => resolve(true))),
  ]);
  review.cancellations.delete(body.operation_id);
  if (cancelled) {
    thread.status = "cancelled";
    return failure("operation_cancelled", "Question answering was cancelled.", 409);
  }
  const lines = body.end_line === body.start_line ? `${body.start_line}` : `${body.start_line}-${body.end_line}`;
  const answer = `This thread is anchored to \`${body.path}:${lines}\`. In a real review, Tact asks the session's agent to inspect the surrounding code and answer with that context.`;
  thread.messages.push({ role: "agent", body: answer });
  thread.status = "idle";
  return Response.json({ generation: body.generation, selected_range: body.range, answer });
}

function composeReview(decision: ReviewDecision) {
  const heading = decision.decision === "approve" ? "Review: approved" : "Review: changes requested";
  const comments = decision.comments.map((comment) => `- \`${comment.path}:${comment.start_line}\` ${comment.body}`);
  return [heading, decision.summary.trim(), comments.join("\n")].filter(Boolean).join("\n\n");
}

const server = Bun.serve({
  port,
  idleTimeout: 0,
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname.startsWith("/api/")) {
      try {
        return await api(request, url);
      } catch (error) {
        if (error instanceof MockRefusal) return failure(error.code, error.message, error.status);
        console.error(error);
        return failure("failed", String(error), 500);
      }
    }
    if (url.pathname === "/__reload" && server.upgrade(request)) return;
    const name = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
    if (!staticFiles.has(name)) return new Response("Not found", { status: 404 });
    return new Response(Bun.file(join(outputDirectory, name)));
  },
  websocket: {
    open(socket) { socket.subscribe("reload"); },
    message() {},
  },
});

console.log(`Tact web (mock): http://localhost:${server.port}/#k=${token}`);
console.log("Type a line and press Enter to set the active draft as the terminal.");

let rebuildTimer: ReturnType<typeof setTimeout> | undefined;
watch(import.meta.dir, { recursive: true }, (_event, filename) => {
  if (!filename || /^(\.dev|dist|node_modules)\b/.test(filename)) return;
  clearTimeout(rebuildTimer);
  rebuildTimer = setTimeout(async () => {
    if (!await buildAssets()) return;
    server.publish("reload", "reload");
    console.log("Rebuilt");
  }, 80);
});

for await (const line of console) tact.terminalDraft(line);
