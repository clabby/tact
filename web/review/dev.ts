import { mkdir, rm } from "node:fs/promises";
import { join } from "node:path";

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

const review = {
  title: "Review feature/review-workflow",
  repository: "tact",
  scope: "Changes from main",
  base: "9d3b745",
  overview_html: `
    <h1>Native review workflow</h1>
    <p>This change introduces a browser-based review surface launched from Tact. The diff is snapshotted before the overview is generated, so comments always refer to the exact code shown.</p>
    <h2>How it fits together</h2>
    <ol>
      <li>The user selects a commit range in the terminal.</li>
      <li>A private agent prepares this overview from the immutable patch.</li>
      <li>The loopback service serves the review and returns structured feedback.</li>
    </ol>
    <h2>Review focus</h2>
    <ul>
      <li>Asset download and validation behavior across release and development builds.</li>
      <li>Diff range semantics for tracked and untracked files.</li>
      <li>Whether submitted comments retain the correct file and line side.</li>
    </ul>`,
  patch: `diff --git a/src/review/mod.rs b/src/review/mod.rs
new file mode 100644
index 0000000..124ca58
--- /dev/null
+++ b/src/review/mod.rs
@@ -0,0 +1,13 @@
+pub async fn run(workspace: &Path) -> Result<String, ReviewError> {
+    let snapshot = diff::collect(workspace).await?;
+    let overview = generate_overview(&snapshot).await?;
+    let server = ReviewServer::start(snapshot, overview).await?;
+
+    browser::open(&server.url())?;
+    let decision = server.wait().await?;
+
+    Ok(decision.to_markdown())
+}
+
+// The browser reviews an immutable snapshot.
+// Live workspace changes do not alter the displayed diff.
diff --git a/src/tui/components/actions.rs b/src/tui/components/actions.rs
index d1c4e91..44b0c57 100644
--- a/src/tui/components/actions.rs
+++ b/src/tui/components/actions.rs
@@ -18,6 +18,7 @@ const ACTIONS: [Action; 12] = [
+    Action::Review,
     Action::NewSession,
     Action::ResumeSession,
     Action::ChangeEffort,
`,
};

const server = Bun.serve({
  port: Number(process.env.PORT ?? 4173),
  async fetch(request) {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/api/review") {
      return Response.json(review);
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
