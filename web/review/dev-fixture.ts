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

const reviewBootstrapBase = {
  protocol_version: 6,
  generation: 1,
  title: "Review feature/review-workflow",
  repository: "tact",
  trunk: "main",
  range_targets: [
    { index: 0, kind: "trunk" as const, short_id: "9d3b745", title: "main · Review foundation" },
    { index: 1, kind: "commit" as const, short_id: "a4c981e", title: "Add native review service" },
    { index: 2, kind: "commit" as const, short_id: "d2e640b", title: "Integrate review action" },
    { index: 3, kind: "working_tree" as const, short_id: "WT", title: "Uncommitted changes" },
  ],
  default_range: { from: 0, to: 3 },
  overview: null,
  questions: [],
};

const overview = `
# Native review workflow

This change introduces a browser-based review surface launched from Tact. The diff is snapshotted before the overview is generated, so comments refer to the exact code shown.

<Callout tone="idea" title="Follow the data">Trace the selected range from the browser to the snapshot and back to a diff comment.</Callout>

## How it fits together

<Process>
  <ProcessStep title="Choose">The browser chooses a range of changes.</ProcessStep>
  <ProcessStep title="Explain">Tact prepares a brief overview of the patch.</ProcessStep>
  <ProcessStep title="Review">The loopback service returns structured feedback.</ProcessStep>
</Process>

## Review focus

- Asset download and validation behavior across release and development builds.
- Diff scope semantics for tracked and untracked files.
- Whether submitted comments retain the correct file and line side.`;

export const reviewFixtures = {
  "2:3": {
    generation: 1,
    title: reviewBootstrapBase.title,
    repository: reviewBootstrapBase.repository,
    selected_range: { from: 2, to: 3 },
    scope: "Uncommitted changes",
    base: "HEAD",
    patch,
    full_context: false,
  },
  "0:3": {
    generation: 1,
    title: reviewBootstrapBase.title,
    repository: reviewBootstrapBase.repository,
    selected_range: { from: 0, to: 3 },
    scope: "Full branch",
    base: "9d3b745",
    patch: branchPatch,
    full_context: false,
  },
  "1:2": {
    generation: 1,
    title: reviewBootstrapBase.title,
    repository: reviewBootstrapBase.repository,
    selected_range: { from: 1, to: 2 },
    scope: "a4c981e → d2e640b",
    base: "a4c981e",
    patch,
    full_context: false,
  },
};

export const reviewBootstrap = {
  ...reviewBootstrapBase,
  page: reviewFixtures["0:3"],
};

export const overviewFixtures = {
  "2:3": overview,
  "0:3": `${overview}\n## Branch-only release work\n\nThe full branch also packages the browser bundle in the release workflow.`,
  "1:2": `${overview}\n## Selected commits\n\nThis overview covers only the selected commit interval.`,
};

export const reviewFixture = reviewFixtures["2:3"];
