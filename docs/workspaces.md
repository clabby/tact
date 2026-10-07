# Per-session workspaces and review checkouts

How a session gets its own working directory and how the web review panel reads any checkout of
its repository.

## Problem

Tact resolves one working directory at startup and uses it for every session in the process: agent
tools, instruction files, memory scope, `@` mentions, `!` commands, history, and the web review
panel. Two things follow:

- A session cannot be started in a git worktree or a jj workspace without starting another Tact.
- An agent rooted in the main checkout that edits one or several other checkouts cannot be reviewed:
  the review panel only shows the process directory.

## Concepts

- **Session workspace.** The directory a session's agent runs in. It is chosen when the session is
  created and fixed for its life, like the model. Sessions already persist it.
- **Checkout.** A directory that holds a working copy of the repository: the main checkout, a git
  worktree, or a jj workspace. A checkout has a path, a kind, a label (branch or workspace name) and
  a head commit.
- **Repository family.** All checkouts that share one repository. It is derived, never stored:
  `git worktree list --porcelain` for git, and `jj workspace list -T 'name ++ "\n"'` followed by
  `jj workspace root --name <name>` for jj. A colocated jj repository reports both lists; they are
  merged by path.
- **Review target.** The checkout the review panel diffs. It defaults to the session workspace and
  can be switched to any other member of the session's family.

The session workspace decides where the agent works. The review target only decides what the human
looks at. They differ exactly when an agent rooted in one checkout works in another.

## Reading a checkout (one git path)

Review keeps its single git implementation. A `Checkout` value says how to point git at a
directory, and every `git` call takes one:

| | git repository or worktree | colocated jj | jj workspace without `.git` |
| --- | --- | --- | --- |
| git directory | `git rev-parse --git-dir` | same | `jj git root` |
| work tree | `--show-toplevel` | same | the workspace directory |
| head | `git rev-parse HEAD` | same | `jj --ignore-working-copy log -r @- --no-graph -T commit_id` |

For the jj case git runs with `GIT_DIR`, `GIT_WORK_TREE` and a throwaway `GIT_INDEX_FILE` seeded by
`git read-tree <head>`, so nothing in the user's repository is written and no `.git` is created.
Untracked listings drop `.jj/` and `.git`. A directory with `.jj` and no `.git` and no `jj` binary is
reported as unreviewable with an instruction to install jj. The `Checkout` is re-detected when its
head changes.

## Core and terminal

- A pane owns its session's workspace. The process default remains the workspace for new sessions
  started without a choice.
- Agent construction takes the workspace as an argument instead of reading it from the config.
- Everything that reads the process directory today reads the pane's instead: shell commands,
  editor and link opening, mention search, instruction and skill discovery, hooks, reflection, the
  memory store, and the terminal's reported working directory (the focused pane's).
- Memory is keyed by the repository family, not the checkout path, so worktrees of one repository
  share memory. Open question below.
- Resuming a session uses its recorded workspace. If the directory no longer exists, the session
  opens read-only with an error that names the missing path.
- The terminal's status line shows the workspace name when it differs from the default. Starting a
  session with another workspace from the terminal is out of scope for the first cut; the web UI
  and `--workspace` cover it.

## Web protocol

- `workspaces` query: `{ session?: string }` returns `{ checkouts: Checkout[], recent: Path[] }`. A
  checkout carries `path`, `name`, `kind`, `label`, `head`, `changed_files`, `current`, `missing`
  and `touched`. `touched` is true when the session's recent tool calls refer to a path inside the
  checkout; it is a hint, not a guarantee.
- `open_session` gains `workspace?: string`, used for new chats and forks (forks inherit by
  default). The path must be a canonical directory in the default workspace's family or in the
  recent list.
- Session summaries already include `workspace`.
- Review endpoints take `session` and `checkout?`. The server accepts a checkout only if it is in
  the family returned for that session, and answers 404 otherwise.
- One review context exists per checkout, kept in a small LRU (four) so a stream of switches does not
  accumulate watchers. The `workspace` stream event carries `checkout`. A watcher runs only for
  checkouts a connected client is viewing.
- Overviews, AI reviews and question threads are keyed by session, checkout and range generation.
- Review compose (Send to chat) adds a `Checkout: <path>` line when the target is not the session
  workspace, so the agent knows where the comments apply.

## Web UI

- **Workspace chip** in the composer, after the model and effort chips. It shows the checkout label,
  and opens a menu of the family (current marked, changed-file counts, a dot for `touched`), then
  recent directories. It is enabled only before the first turn, with the same tooltip as the model
  chip. On a phone the chip is icon-only with a title.
- **Session rows and the header** show the workspace name only when it differs from the default.
- **Checkout selector** in the review panel's top bar, beside the range selector, using the same
  popover as the chip. Each row shows its label, changed-file count and a touched dot. The choice is
  remembered per session in the browser.
- **Touched notice.** When the session's tool calls start touching a checkout other than the review
  target, a pill like the existing "New changes available" appears: "Agent is working in <name> ·
  Review". It never switches the target on its own.
- **Many checkouts.** One target at a time, switched from the selector or the pill. A combined view
  across checkouts is not in this design; the file tree and comment anchors would need path
  namespacing.

## Failure modes

- A checkout is deleted or pruned while selected: the panel shows the missing path and returns to the
  session workspace.
- `jj` is missing: the selector lists the checkout as unreviewable with the reason.
- A very large family: the list is capped (fifty) and the rest is reachable by typing a path in a
  later iteration.
- Paths outside the family are never accepted by review endpoints, even though the token already
  grants shell access; this keeps the surface small and predictable.

## Decisions

- History lists sessions started in any checkout of the focused session's repository, and each row
  shows its workspace when it differs from the default.
- Memory stays process-wide: one store, selected from the default workspace, so worktrees of one
  repository share it.
- A new chat starts in the default workspace. The workspace chip chooses another checkout before
  the first turn, and a session's workspace never changes afterwards.
- Only checkouts of the repositories behind the default workspace and the live sessions can be
  started in or reviewed, and a request that names anything else is refused.
- The touched marker is a heuristic over the arguments of recent tool calls of the session and its
  subagents. It is a hint for the review pane, never a permission.
