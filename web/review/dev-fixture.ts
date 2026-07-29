const patch = `diff --git a/src/review/mod.rs b/src/review/mod.rs
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
@@ -18,3 +18,4 @@ const ACTIONS: [Action; 12] = [
+    Action::Review,
     Action::NewSession,
     Action::ResumeSession,
     Action::ChangeEffort,
`;

const branchPatch = `${patch}diff --git a/.github/workflows/release.yml b/.github/workflows/release.yml
index 82a06c1..90c03be 100644
--- a/.github/workflows/release.yml
+++ b/.github/workflows/release.yml
@@ -21,2 +21,4 @@ jobs:
       - run: cargo build --release
+      - run: cd web/review && bun install --frozen-lockfile
+      - run: cd web/review && bun run build
       - run: cargo test
`;

export const reviewBootstrap = {
  title: "Review feature/review-workflow",
  repository: "tact",
  trunk: "main",
  default_scope: "uncommitted" as const,
};

const overview = `
  <h1>Native review workflow</h1>
  <p>This change introduces a browser-based review surface launched from Tact. The diff is snapshotted before the overview is generated, so comments always refer to the exact code shown.</p>
  <h2>How it fits together</h2>
  <ol>
    <li>The browser chooses uncommitted changes or the full branch.</li>
    <li>A private agent prepares this overview from the immutable patch.</li>
    <li>The loopback service serves the review and returns structured feedback.</li>
  </ol>
  <h2>Review focus</h2>
  <ul>
    <li>Asset download and validation behavior across release and development builds.</li>
    <li>Diff scope semantics for tracked and untracked files.</li>
    <li>Whether submitted comments retain the correct file and line side.</li>
  </ul>`;

export const reviewFixtures = {
  uncommitted: {
    title: reviewBootstrap.title,
    repository: reviewBootstrap.repository,
    selected_scope: "uncommitted" as const,
    scope: "Uncommitted changes",
    base: "HEAD",
    patch,
  },
  full_branch: {
    title: reviewBootstrap.title,
    repository: reviewBootstrap.repository,
    selected_scope: "full_branch" as const,
    scope: "Full branch",
    base: "9d3b745",
    patch: branchPatch,
  },
};

export const overviewFixtures = {
  uncommitted: overview,
  full_branch: `${overview}<h2>Branch-only release work</h2><p>The full branch also packages the browser bundle in the release workflow.</p>`,
};

export const reviewFixture = reviewFixtures.uncommitted;
