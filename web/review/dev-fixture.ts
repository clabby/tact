export const reviewFixture = {
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
@@ -18,3 +18,4 @@ const ACTIONS: [Action; 12] = [
+    Action::Review,
     Action::NewSession,
     Action::ResumeSession,
     Action::ChangeEffort,
`,
};
