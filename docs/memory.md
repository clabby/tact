# Global shared memory

This document specifies Tact's first persistent agent-memory design. It implements the deliberately
minimal experiment proposed in [issue 25](https://github.com/clabby/tact/issues/25): preserve a
small set of durable conclusions across sessions without turning every prompt into a growing memory
dump. The feature is useful only if it improves repeated work while remaining quiet, bounded, and
easy to remove.

## Product contract

Memory is disabled by default. It is enabled only by explicit configuration:

```toml
[memory]
enabled = true
```

Enabling memory makes one database available at:

```text
<config-dir>/memory/v1.sqlite3
```

`<config-dir>` is the parent directory of the selected `config.toml`, including a file selected by
`--config`, `TACT_CONFIG`, or `TACT_HOME`. The path does not depend on the current workspace and
remains stable across configuration reloads.

There is exactly one corpus for every Tact process that selects that configuration directory. It
has no workspace, repository, branch, session, or agent namespace, and queries never apply an
implicit workspace filter. A preference learned in one workspace can therefore be found in any
other workspace. Workspace-specific conclusions must name their scope in their content, for
example, "In the `commonware` repository, release tags use ...". This convention helps retrieval
but is not an access-control boundary.

Memory content is data, not a higher-priority instruction layer. Current user requests, system
policy, and `AGENTS.md` continue to govern the agent.

## Explicit access

Tact exposes one tagged `memory` tool with four operations. It does not inject records, candidate
lists, summaries, or search results into the system prompt or the beginning of a turn. The prompt
may state that the tool exists, but the first stored-memory token appears only after an agent calls
`scan` or `read`.

| Operation | Callers | Contract |
| --- | --- | --- |
| `scan` | Root and child agents | Search active records and return at most five compact candidate cards. A card contains identity, version, score, and a deterministic preview. Content at 64 bytes or fewer is returned in full. Longer content is a UTF-8-safe prefix of at most 64 bytes. |
| `read` | Root and child agents | Fetch complete content and metadata by stable ID. The ID list has no separate record cap. Missing IDs are omitted from the result. |
| `put` | Root agent only | Add one atomic conclusion or replace a known record. A replacement supplies the stable ID and expected version. The tool rejects stale versions rather than overwriting concurrent work. The agent should scan before putting to avoid duplicates. |
| `delete` | Root agent only | Delete a known record using its stable ID and expected version. The tool rejects stale versions. |

When memory is enabled, Tact appends a fixed, model-only memory-review checkpoint to every accepted
user message after the first turn, including restored and forked-session continuations. It also
appends the checkpoint to every accepted in-flight steer and to a steer promoted into a new turn.
The checkpoint identifies itself as Tact control text and contains no user text, candidate memory,
path, or session identifier. It tells the root agent to review the full available conversation and
store any warranted durable conclusion before its final answer. A review that finds no durable
change makes no memory call. The original user submission remains unchanged for display and
transcript journaling.

The model-visible checkpoint becomes part of Nanocodex's conversation state and completed session
checkpoint, so a resumed model can see the same control text. It is not written as a separate user
transcript event and never copies the triggering message into the control text.

The checkpoint is a structural prompt trigger, not an independent memory manager. There is no
automatic search, read-after-scan, write at turn completion, persisted candidate queue, or
background model pass. A scan returns candidates for the agent to judge. It does not silently treat
every returned candidate as relevant. Outside a requested review, not calling the tool remains the
ordinary no-op path.

Child agents may scan and read because shared conclusions can prevent repeated work. They may not
put, replace, or delete. Root-only mutation gives the session's coordinating agent one place to
resolve scope, duplication, and contradictions. This is an authorization check in Tact, not merely
an instruction in the prompt.

## Record policy

A record is one self-contained conclusion that can be used without reconstructing the conversation
that produced it. Good records include a durable user correction, a stable preference, or an
expensive-to-rediscover operational fact. Each record should include qualifications that affect its
truth: the relevant repository or service, the observed version, and a date when freshness matters.
Repository- and code-specific conclusions are first-class candidates when they can materially help
later changes or review and would be expensive to rediscover. They must name their stable logical
scope because the store has no implicit workspace boundary.

Do not store transcripts, reasoning traces, task plans, raw tool output, credentials, transient
state, generic knowledge, or facts that a cheap repository search will recover. Do not bundle a
list of unrelated facts into one record. Atomic conclusions make replacement precise and reduce the
chance that one stale clause invalidates an otherwise useful record.

Every record carries creation and last-modification times, separate scan and read telemetry, a
stable logical ID, and a monotonically increasing version. Tact v1 intentionally does not persist
the writing model, provider, session, agent, or workspace. This keeps the surface and data model
small, but means the database cannot attribute a conclusion beyond the fact that it entered through
Tact's root-only memory tool or the trusted TUI browser.

`put` creates version 1 for a new stable ID. Replacement increments the version after atomically
checking `expected_version`; it does not append a second active fact. `delete` checks the expected
version and physically removes the current row. Version checks prevent one Tact process from
silently replacing or deleting a value that another process changed after it was scanned.

Deletion means "remove this record from Tact's database and retrieval surface," not "prove that
every byte once associated with this record has been destroyed." The privacy consequences are
described below.

## Retrieval and abstention

The first version uses an ordinary SQLite table and performs lexical ranking in process. It does not
create an FTS virtual table or an embedding index. This keeps the data model inspectable and avoids
adding a model-dependent retrieval service before a lexical baseline demonstrates utility.

Ranking uses standard BM25 with `k1 = 1.2` and `b = 0.75`. The values match SQLite FTS5's
[documented BM25 constants](https://www.sqlite.org/fts5.html#the_bm25_function), but Tact computes
the score itself; this reference does not imply that Tact uses FTS5. The probabilistic basis is
described by Robertson and Zaragoza in
[The Probabilistic Relevance Framework: BM25 and Beyond](https://www.nowpublishers.com/article/Details/INR-019).

Terms absent from a record contribute no score rather than vetoing that record. This lets a verbose
query retrieve memories that match coherent subsets of its terms. Records matching more terms or
rarer terms rank higher through BM25. A scan abstains only when the query has no searchable terms or
when no active record shares a term with it.

This recall-oriented behavior is bounded at candidate retrieval. `scan` returns no more than five
compact candidate cards. A preview returns the complete record when it is at most 64 bytes. Longer
records return a UTF-8-safe prefix of at most 64 bytes. The preview is deliberately not a semantic
summary. `read` has no separate ID cap and returns complete records for the IDs the agent selects.
For a short record, scan and read can therefore contain the same text. Read remains a deliberate-use
operation that updates separate telemetry and clears probation.

## Telemetry and probation

Candidate retrieval and deliberate consumption are different events. Tact therefore stores scan
telemetry separately from read telemetry:

- returning a record from `scan` updates its scan count and last-scanned time only;
- returning full content from `read` updates its read count and last-read time only; and
- an abstaining or failed operation does not fabricate a per-record hit.

Neither signal changes a record's truth. Keeping them separate shows whether a record is merely easy
to retrieve or was actually selected for use, and prevents a broad scan from graduating every
candidate.

A newly model-authored record enters seven days of unread probation. If it has no successful model
`read` by the end of that period, it is eligible for pruning. A scan is not a read. Replacement
changes the content and starts probation again for the new version. Seven days is an experimental
Tact constant, not a retention period established by memory research.

## Bounds and transactional behavior

All limits apply to the single global corpus, not separately to each workspace:

| Limit | Tact v1 value | Meaning |
| --- | ---: | --- |
| Record content | 1 KiB | At most 1,024 bytes of UTF-8 content in one conclusion. |
| Rows | 512 | At most 512 live memory rows in the database. Deleted rows are removed rather than retained as tombstones. |
| Total content | 256 KiB | At most 262,144 bytes of record content across stored rows. |
| Main database file | 4 MiB | Maximum size of `v1.sqlite3` itself, including tables, indexes, metadata, and free pages. SQLite sidecar files are not covered by this number. |
| Scan results | 5 | Maximum candidate cards returned by one scan. |

The store prunes expired unread probation records before rejecting a capacity-increasing mutation.
It must not silently evict a graduated active conclusion merely to make a new put succeed. If the
row, content, or file bound still cannot be met, `put` fails without a partial write. Delete remains
available at capacity.

Every put, replacement, delete, telemetry update, and related bound check is one database
transaction. SQLite provides
[atomic, consistent, isolated, and durable transactions](https://www.sqlite.org/transactional.html),
including all-or-nothing behavior across process, operating-system, and power failures. Tact also
sets a database page limit so growth beyond the main-file cap fails with `SQLITE_FULL`; SQLite
documents this mechanism under
[maximum pages in a database file](https://www.sqlite.org/limits.html#max_page_count). Application
checks remain necessary for the much smaller row and content limits.

SQLite is opened lazily. Merely enabling memory does not put corpus content into the model context,
and a session that never uses memory pays no retrieval-token cost.

## Human browser

The TUI provides a direct memory browser. It reads records from the local store without routing them
through the model or synthesizing a memory-tool event. Browsing therefore consumes no model tokens
and does not increment scan or read telemetry, graduate probation, or affect ranking.

The browser defaults to most-useful-first order, where usefulness is the deliberate read count, not
the number of times a scan returned the memory as a candidate. Press `f` to cycle through most
useful, newest, oldest, and least useful. Newest and oldest use the current record version's update
time; deterministic age ordering breaks usefulness ties.

Deleting from the browser requires explicit confirmation. The browser submits the displayed
record's expected version; if the row changed while the confirmation was open, deletion fails and
the view reloads rather than deleting the newer value. This trusted user action bypasses the
root-agent-only tool restriction but uses the same transactional store operation.

Configuration reload changes browser availability immediately. Existing agent runtimes keep the
tool surface and system instructions with which they were created; the reloaded agent-facing
memory setting takes effect when a new session starts or is restored. The reload notification calls
out this split explicitly.

## Secrets, deletion, and retained copies

Tact applies best-effort secret rejection before a put. The check rejects obvious private-key
blocks, authentication headers, credential-bearing URLs, common provider-token shapes, and values
assigned to credential-like names. The rejection error does not echo the candidate secret. This is
defense in depth around the model instruction never to store credentials; it is not a complete
secret scanner and cannot recognize every private value or encoded secret.

The memory database is local and unencrypted. Anyone who can read the selected Tact configuration
directory may be able to inspect it. File permissions reduce accidental exposure but are not
cryptographic protection.

Secret rejection applies only to the memory write. By that point, the value may already exist in the
current model context, command output, or Tact's append-only transcript and checkpoint. Deleting or
disabling memory does not rewrite those stores, provider-side records, backups, filesystem
snapshots, or other copies.

Tact zeroizes the typed put-content and secret-screening buffers owned by the memory implementation.
Nanocodex's raw tool arguments and conversation records, Serde's parser internals, SQLite's internal
bindings and pages, and operating-system copies are outside that ownership boundary and do not
provide the same guarantee.

SQL row deletion immediately removes a record from normal memory retrieval, but SQLite may retain
old bytes in free pages or transaction sidecars. SQLite's
[`secure_delete` documentation](https://www.sqlite.org/pragma.html#pragma_secure_delete) explains
both its overwrite behavior and its limitations. Rollback journals can contain prior pages, while
WAL mode creates additional `-wal` and `-shm` files and appends changes before checkpointing, as
described in SQLite's [WAL documentation](https://www.sqlite.org/wal.html). The 4 MiB limit covers
only the main database, not these temporary or quasi-persistent files.

Even physical row deletion, `secure_delete`, checkpointing, and `VACUUM` cannot promise forensic
erasure from SSD wear-leveling, snapshots, backups, or storage already copied elsewhere. Tact must
describe delete as removal from product-visible retrieval, never as proof of physical erasure.
FTS5 has additional shadow-table deletion concerns documented in its
[secure-delete option](https://www.sqlite.org/fts5.html#the_secure_delete_configuration_option);
Tact v1 does not use FTS5, but this is one reason not to casually change the storage design.

## Research-derived principles

The design borrows principles from prior work; the papers do not validate Tact's exact constants or
global coding-agent setting.

- [LongMemEval](https://arxiv.org/abs/2410.10813) separates extraction, multi-session and temporal
  reasoning, knowledge updates, and abstention. Tact consequently evaluates update correctness and
  correct refusal to retrieve, rather than reporting recall alone.
- [LongMemEval-V2](https://arxiv.org/abs/2605.12493) evaluates static and dynamic state, workflows,
  environment gotchas, and premise awareness while framing memory as compact evidence gathering.
  It motivates testing durable operational conclusions and downstream task utility. Its reported
  coding-agent method also has high latency, supporting a fixed in-turn review checkpoint rather
  than a separate background history-processing pass.
- [MemGPT](https://arxiv.org/abs/2310.08560) treats external memory as a tier accessed under a
  limited context window. Tact adopts explicit movement into active context, not automatic corpus
  injection.
- [Generative Agents](https://arxiv.org/abs/2304.03442) combines relevance, recency, importance, and
  reflection over an experience stream. It supports distinguishing retrieval signals, but its full
  experience log and reflection pipeline are broader than Tact's atomic conclusions.
- [MemoryBank](https://arxiv.org/abs/2305.10250) explores time-based forgetting and reinforcement.
  It motivates testing decay, but does not establish a seven-day TTL for coding-agent facts.
- [Mem0](https://arxiv.org/abs/2504.19413) evaluates extraction, consolidation, update, and
  retrieval of salient conversational facts, including latency and token cost. Tact keeps the
  atomic-update idea while deferring Mem0's extra model pipeline and graph representation.
- [Memory-R1](https://aclanthology.org/2026.acl-long.583/) learns `ADD`, `UPDATE`, `DELETE`, and
  `NOOP` behavior with reinforcement learning. Tact has no trained memory manager, so mutation is
  root-only, version-checked, bounded, and easy for the user to undo.
- [BEIR](https://arxiv.org/abs/2104.08663) finds BM25 to be a robust zero-shot retrieval baseline
  across heterogeneous tasks, while more expensive methods have tradeoffs. That justifies measuring
  a lexical baseline first; it does not prove BM25 is sufficient for Tact.

SQLite is selected for transactional concurrent mutation and enforceable bounds, not because this
small dataset needs a database server. Its official
[transaction](https://www.sqlite.org/lang_transaction.html),
[FTS5](https://www.sqlite.org/fts5.html), and [limits](https://www.sqlite.org/limits.html)
documentation defines the behavior Tact relies on or deliberately avoids.

## Tact constants requiring calibration

The product invariants are opt-in operation, no automatic corpus injection, one global corpus,
explicit tool access, a content-free in-turn feedback checkpoint, root-only mutation, bounded
storage, and honest deletion language. The following are initial Tact choices rather than
conclusions established by the cited research:

- seven days of unread probation.
- 1 KiB per record, 512 rows, 256 KiB of content, and a 4 MiB main database.
- five scan candidates and a 64-byte preview.
- the tokenizer and no-overlap abstention rule.
- BM25 `k1 = 1.2` and `b = 0.75` for this corpus, despite being established baseline values.

Evaluation may tune these values while the experiment remains disabled by default. Tuning must be
pre-registered against held-out scenarios; production anecdotes are not a license to increase
limits or add embeddings, graphs, or reflection.

## Evaluation and kill criteria

Evaluate memory with paired runs using the same model, effort, task, and starting state, with memory
enabled for one run and disabled for the other. Counterbalance run order. The scenario set must
cover durable corrections, rebuttals, direction-changing steers, later scope refinements,
preferences, expensive workspace gotchas, stale replacements, contradictions, cross-workspace name
collisions, irrelevant queries, secret-shaped content, and adversarial repository text.

Measure at least:

- task completion and correctness after the relevant fact crosses a session boundary.
- scan precision and recall at five, read precision, ranking quality, and correct abstention.
- duplicate puts, correct replacement, contradiction rate, forbidden writes, and probation
  survival.
- scan and read counts separately, including candidates repeatedly scanned but never read.
- p50/p95 tool latency, memory tokens added to context, main/sidecar disk use, and failure behavior
  at every bound.

Release is blocked unless deterministic tests prove zero corpus tokens without an explicit call,
child mutation denial, optimistic-version conflicts, atomic updates, all storage/output bounds, TUI
delete confirmation, and rejection of the maintained secret fixture set. The best-effort nature of
secret detection remains explicit even when all known fixtures pass.

Before running the held-out evaluation, set minimum acceptable gains, non-inferiority margins for
contradictions and false retrieval, abstention targets, and latency/context budgets. Keep the
feature disabled if results are inconclusive. Remove it if the planned paired evaluation shows flat
or negative task utility, if contradiction or cross-workspace false-memory costs exceed the
pre-registered margin, if correct abstention cannot meet its target, or if latency/context cost
exceeds its budget. A failed minimal experiment is not a reason to add embeddings, autonomous
reflection, larger limits, or more writes.

## Out of scope: checked-in memory

Tact v1 does not read, write, import, export, or commit repository-owned memory. In particular, it
does not put a mutable SQLite database in the working tree, and a repository cannot gain access to
global records by declaring colliding identifiers. Human-reviewable checked-in memory has a
different trust, provenance, instruction-injection, merge, and privacy model. The separate design
questions are recorded in [issue 25](https://github.com/clabby/tact/issues/25) and must not be added
as an incremental extension of this global private store.
