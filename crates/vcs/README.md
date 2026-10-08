# tact-vcs

`tact-vcs` reads version-controlled checkouts without writing to them. It shells out to `git`, and
to `jj` when a checkout is a jj workspace without a `.git` directory.

The crate exposes three integration boundaries:

- `Checkout` finds the checkout that contains a directory and lists every other checkout of the
  same repository: git worktrees and jj workspaces.
- `ReviewContext` lists the reviewable points from the trunk merge base through each commit to the
  working tree, and captures any interval between two of them as one full-context
  `DiffSnapshot`.
- `WorkspaceVersion` is a content hash of a checkout's trunk, head, and working-tree changes. Two
  equal versions mean nothing reviewable changed between the two readings.

`FilePatch::parse_all` splits a captured patch into files and hunks, so callers can validate line
anchors or inspect a change without re-implementing unified-diff parsing.

A jj workspace has no `.git`, so git is pointed at the repository's git store with an explicit
work tree and a temporary index. jj is always run with `--ignore-working-copy`, so reading a
checkout never snapshots it.
