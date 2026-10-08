# Session workspaces and review checkouts

Each session runs in its own workspace directory, and the web review panel can read any checkout of
that workspace's repository. This document describes both.

## Terms

- **Default workspace.** The directory Tact resolved at startup (the current directory, or
  `--workspace` / `TACT_WORKSPACE`).
- **Session workspace.** The directory a session's agent runs in. It is chosen when the session is
  created and does not change afterwards. Sessions persist it, and resuming a session uses it.
- **Checkout.** A directory holding a working copy: a git repository, a git worktree, a colocated jj
  repository, or a jj workspace.
- **Repository family.** Every checkout of one repository. It is derived on demand and never
  stored: `git worktree list --porcelain` for git, and `jj workspace list` followed by
  `jj workspace root --name <name>` for jj. A colocated jj repository reports both lists, merged by
  path. The main checkout comes first.
- **Review target.** The checkout the review panel diffs. It defaults to the session workspace and
  can be any other member of the session's family.

The session workspace decides where the agent works. The review target only decides what the
reviewer looks at. They differ when an agent running in one checkout edits another.

## Starting a session in a checkout

A new chat starts in the default workspace. In the web composer, the workspace chip picks another
checkout before the first turn; `open_session` carries it as `new.workspace`, and forks inherit
their parent's workspace. The server accepts a workspace only if it is a checkout of the default
workspace's repository or of a repository a live session runs in. Anything else is refused with
`invalid_request`.

The session's agent and hooks run in the session workspace, file links resolve against it, and the
terminal reports the focused session's workspace as its working directory. Memory is the
exception: one memory store is selected from the default workspace for the whole process, so all
checkouts of a repository share it.

History lists the persisted sessions of every checkout in the focused session's repository family.

## Listing checkouts

The `workspaces` query (`POST /api/query`, `{ query: "workspaces", args: { session? } }`) is
answered by the web server without the terminal loop. It returns:

- `default`: the default workspace.
- `checkouts`: the family of the session's workspace (of the default workspace when no session is
  named), at most 24 entries. Each has `path`, `name` (directory name), `label` (branch or jj
  workspace name, or a short commit id for a detached head), `kind` (`git` or `jj`),
  `changed_files` (tracked changes plus untracked files, or `null` if counting failed or took longer
  than 3 seconds), `current`, `missing` (the directory no longer exists), and `touched`.
- `recent`: up to 5 workspaces of live sessions outside this family.

`touched` is true when the arguments of the session's recent tool calls, or its subagents', name a
path inside the checkout. It is a hint for the review panel, not a permission.

A family listing is cached for 5 seconds per workspace.

## Reading a checkout

Review reads every kind of checkout through git (the `vcs` module):

| | git repository or worktree | colocated jj | jj workspace without `.git` |
| --- | --- | --- | --- |
| git directory | found by git | found by git | `jj git root` |
| work tree | `git rev-parse --show-toplevel` | same | the workspace directory |
| head | `HEAD` | `HEAD` | `jj log -r @- -T commit_id` |

For a jj workspace without `.git`, git runs with `GIT_DIR`, `GIT_WORK_TREE`, and a temporary
`GIT_INDEX_FILE` seeded with `git read-tree <head>`. jj always runs with `--ignore-working-copy`.
Reading a checkout writes nothing to the user's repository. Untracked listings skip `.jj` and
`.git`. A jj workspace cannot be reviewed on a machine without `jj`; the error says jj is missing.

## Review requests

Every review request may name a `checkout` (an absolute path). Without one, the target is the
session's workspace, or the default workspace when no session is named. A checkout is accepted
only if it is in the family of the session's workspace, the default workspace, or a live session's
workspace; anything else is `invalid_checkout` (400).

The server keeps the review of the four most recently used checkouts. Each kept checkout has one
workspace watcher, which polls only while a browser stream is connected and emits a `workspace`
event naming the checkout when its content changes. Overviews, AI reviews, and question threads are
kept per session within a checkout's review generation.

When the review target is not the session's workspace, the composed review (Send to chat) adds a
`**Checkout:**` line with its path so the agent knows where the comments apply.

See [web.md](web.md#review) for the endpoints and error codes.

## Review panel

- A checkout selector beside the range selector lists the family with labels and changed-file
  counts.
- When the session's tool calls touch a checkout other than the review target, a notice reads
  "Agent is also working in <label>". It never switches the target by itself.
- If the selected checkout disappears, the panel returns to the session workspace.
- One checkout is reviewed at a time. A combined view across checkouts would need path namespacing
  in the file tree and in comment anchors.
