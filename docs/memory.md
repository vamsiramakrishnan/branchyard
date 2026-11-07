# Memory that becomes skills

Written 7 November 2026, and revised the same day after a review that read it against the code. Status: **design; nothing on this page is built.** It is written for the engineers who build it next, with types, fields, commands and routes, and for each slice the hermetic tests and the evidence the skill-discovery eval must give. What exists today is [repository knowledge](knowledge.md): short rules distilled from a finished branch, reviewed by a person, and *prompted* into every matching turn within 1,500 tokens. This page is the rest of memory: what a team learns across runs and what one person learns for themselves, written down as skills that agents **find by search**, through the [skill registry](skill-registry.md), instead of being told every turn; and, later, the consolidation ("dreaming") that proposes such memories from the record of many runs.

## The idea in four lines

- A memory is a skill: a `SKILL.md` that lists what was learned about one topic, and one reference per memory. It is indexed, searched and opened like every other skill, so a turn that needs it finds it and a turn that does not pays nothing.
- Team memory goes through git, and is reviewed as a pull request; personal memory stays with the person. Nothing is shared silently.
- A run writes down what happened: the event log already does. A scheduled job later reads many runs and *proposes* memories with provenance to the events they came from. It never approves.
- Memory that fits one line and applies to every turn of a scope is a knowledge entry and stays prompted; anything longer, conditional or procedural is memory and is found. Consolidation proposes to both.

The order of the plan follows the first two lines: memory a person writes, as a skill, first ([M1](#9-build-plan)); observations and consolidation after, once there is a shape for them to fill.

## Today

| Piece | Where | What it does |
|---|---|---|
| Knowledge entries | `crates/branchyard/src/knowledge.rs`, `knowledge` table (SQLite) and `by_knowledge` (PostgreSQL) | `KnowledgeEntry {id, scope, text ≤ 600, source: Person \| Branch{branch, turn, via}, status: Proposed \| Adopted \| Rejected, created_ms, adopted_by, decided_ms, note}`; `KnowledgeBackend` on both stores with one conformance case. **Keyed by `repo`, with no tenant**: a served repository can be on several tenants' allowlists ([server](server.md#tool-policy)) |
| The extractor | `knowledge.rs` | Deterministic: sends after the first, steers, `by review` comments and `by pr --watch` comments, read from `Activity::Feedback` records (`FeedbackItem {kind: comment \| check, path, line, author, body}`) and, for old logs, the prose. **It does not filter by author**: a comment by anyone on a public pull request is a correction |
| The harness distiller | `knowledge.rs`, any `Judge` | A read-only scratch branch, a strict JSON answer of at most five entries, fallback to the extractor, `KnowledgeActivity::Distilled {ids, duplicates, by, fallback, trigger}` |
| Triggers | `docs/triggers.md` | Cron and interval schedules, `deliver` to one branch, prechecks; a claimed run is held 11 minutes under a fence and **refired by another dispatcher when the hold expires**; exactly-once for a task comes from the operation registry's idempotency key, which an internal job does not have |
| The record of a turn | `Activity` in `crates/branchyard/src/lib.rs`; `events (id, branch, incarnation, seq, at_ms, activity)` in `sqlite.rs` | `Prompt`, `Harness(Event)`, `Decision`, `Delegation`, `Provisioned`, `Steered`, `Message`, `Checkpoint`, `PullRequest`, `Feedback`, `ConnectorCall`, `TurnCost`, `Status`, `Warning`, plan and goal activities, fleet and judge marks. Every event belongs to a branch incarnation (`append(name, …)`); the repository feed (`record::feed`) is the union of branch events, so there is no branchless event |
| Secret handling | `crates/branchyard-harness/src/lib.rs` (redacts header **names** in a recording's `Debug`), `crates/branchyard-sync/src/scrub.rs` (object integrity) | **No scrubber of secret values or shapes exists.** The one this page needs is new |
| Repository skills and trust | `crates/branchyard/src/skills.rs`, `by skills trust` | `.agents/skills/<name>/` at the base commit, trusted by digest, withheld with a reason in `provisioned.skills` otherwise ([your repository's skills](knowledge-skills.md#your-repositorys-skills)) |
| The eval | `tools/skill_eval.py`, `evals/skills/` ([skill-discovery eval](skill-eval.md)) | Scenarios with a prompt and `expect_skills`; `score` over a turn's log; `rank --tree` (R1 of the registry) as the hermetic ranking proxy |

## 1. Memory as a skill, first

A memory is one Markdown file. A topic's memories are one skill bundle, and the bundle is what a turn finds:

```text
memory-<topic>/                      e.g. memory-parser, memory-releases, memory-general
  SKILL.md                           frontmatter: name, description ("What this team learned about <topic>:
                                     N procedures, M pitfalls; found by search, approved by people"), keywords
                                     from the memories' titles; body: one line per memory, `- <title> (references/<slug>.md)`,
                                     grouped by kind (rule, procedure, fact, pitfall); ≤ 4,096 characters
  references/<slug>.md               the memory: a title line, the body (Markdown, ≤ 8,000 characters), then a footer:
                                     `Approved by <who> on <date>.` and, when consolidation proposed it, `Evidence: <branch> turn <n> (<kind>), …`
  memory.json                        {"schema": "branchyard.memory-bundle/v1", "topic", "memories": [{"id", "title", "kind", "paths", "task_kinds", "approved_by", "approved_ms"}]}
```

**The bundle is deterministic.** It holds approved content and nothing that a later run recomputes: no strength, no counts, no growing evidence list beyond what the approver saw. The same approved set written twice gives the same bytes and the same `skillset::digest`, which is what makes the digest an identity and a test (`same approved set → same digest`). A topic that would pass the skill limits (200 files, 1 MiB; `SKILL.md` over 4,096 characters) is split by the writer, `memory-<topic>-2`, and the first bundle's `SKILL.md` says so.

**Two homes, one format**, which are the two scopes a person has today:

| Scope | Where the bundle lives | How it reaches a turn | Who approves |
|---|---|---|---|
| **Team** (`Repo`) | `.agents/skills/memory-<topic>/` in a commit of the repository | As any repository skill: trusted by digest with `by skills trust`, withheld with its reason otherwise, read from the base commit | The repository's reviewers, through the pull request that adds it; `by skills trust` by whoever runs the turns |
| **Personal** (`Person`) | `~/.config/branchyard/skills/memory-<topic>/`, the local `user` source of the registry (M1 there) | As a user skill: the person's own sessions only, precedence between repository and built-in | The person; nobody else |

Nothing else is needed for a memory to be found: the registry's R1 makes each reference a search document, so a turn that searches "parser bench fixture" lands on `memory-parser/references/bench-fixture.md` and opens that file, not the bundle. A memory skill is linked into a harness's native skills directory only when it is *chosen*, as every skill is ([registry, section 5](skill-registry.md#5-delivery)).

**Commands (M1).**

```text
by memory add --title T --body-file F [--kind rule|procedure|fact|pitfall] [--path GLOB]... [--topic T] [--mine]
                                     # writes the reference and rewrites the topic's SKILL.md and memory.json;
                                     # into .agents/skills/ of the worktree (team: commit it, open the pull request),
                                     # or with --mine into ~/.config/branchyard/skills/ (approved by you, at once)
by memory list [--mine] [--json]     # every memory of the repository's bundles and your own, by topic
by memory show SLUG                  # one memory
by memory edit SLUG ... | remove SLUG  # the same files, rewritten
by memory export [--out DIR]         # the bundles as the writer would produce them, for inspection and the eval
```

`by memory` refuses to add, edit or remove with `BRANCHYARD_BRANCH` set, the rule `by knowledge` follows: a harness proposes nothing. Locally the refusal is **advisory** (the variable can be unset, and a local harness can write the folder directly); on a server it is enforced, because a turn holds no `run` token and the repository's files reach a turn only through a commit someone else reviews. A new refusal is a `Report` with a kind and a `next:`.

**What `knowledge` hands over.** `by knowledge distill` proposes entries of at most 600 characters. A proposal that a distiller writes longer than that, or that names a procedure, is today cut or dropped; with M1 the deterministic extractor's proposals longer than 600 characters become **memory drafts** under `.branchyard/memory-drafts/<slug>.md` in the worktree, for the person to read, edit and `by memory add --from DRAFT`. A draft is a file in a worktree, never a bundle: nothing a turn can find until a person adds it.

## 2. What is captured

Consolidation ([section 4](#4-consolidation-deterministic)) needs raw material: **observations**, small typed scrubbed records derived from the event log that name the events they came from. They are not memories and no harness ever sees one. This is M2.

```rust
pub struct Observation {
    pub id: u64,                       // store-assigned
    pub tenant: String,                // the session's tenant ("local" without a server)
    pub repo: String,
    pub branch: String,
    pub incarnation: i64,              // the branch incarnation the events belong to
    pub turn: u32,
    pub events: Vec<EventRef>,         // {incarnation, seq}, as the `events` table keys them; at least one
    pub visibility: Visibility,        // Shared, or Private { subject }: the session's own visibility
    pub kind: ObservationKind,
    pub text: String,                  // scrubbed (below); ≤ 4,000 characters
    pub paths: Vec<String>,            // repository paths the observation is about
    pub task_kind: Option<TaskKind>,
    pub harness: String,
    pub model: Option<String>,
    pub author: Option<Author>,        // {name, association: Member | External} for a correction
    pub at_ms: u64,
    pub scrubbed: Vec<ScrubKind>,      // what the scrubber removed, by kind, never the value
}

pub enum ObservationKind {
    Correction,      // a send after the first, a steer, a review or pull-request comment (the extractor's inputs)
    CheckFailed,     // a `--check` or CI check that failed, with the command and the tail of its output
    ToolDenied,      // Activity::Decision with allowed: false, the tool and the rule
    GoalMissing,     // GoalActivity verdict not met: the `missing` list
    JudgeVerdict,    // a judge's reasons for and against a candidate (fleet JudgeMark text)
    PlanRejected,    // PlanActivity rejected, with the reason
    Escalation,      // a Message of class escalation, or report_status blocked/failure
    Outcome,         // the branch's end: merged, failed with reason, budget_exceeded, discarded
    RepeatedCommand, // the same shell command failing twice or more in one turn
}
```

What each kind reads, and what it deliberately leaves out:

| Kind | From | Not from |
|---|---|---|
| `Correction` | The extractor's sources, as data: `Feedback` items of kind `comment` (scoped to `path`), sends and steers after the first | Branchyard's own prompts (plan mode, goal follow-ups, wakes), continuation replies (`yes`, `continue`, `lgtm`), inbox blocks: the extractor's skip list. **A comment whose author is `External`** (not a member of the repository: locally not a committer of the repository, on a server not a principal of the tenant) is recorded with its association and **excluded from candidates by default** (`[memory] external_authors = false`): a public pull request's comments and a CI tail are text anyone can write, and they would otherwise flow verbatim into a proposal and into the distiller's prompt |
| `CheckFailed` | `PullRequestActivity` check runs that failed; a `--check` whose exit was non-zero, with the last 40 lines of output | A check that passed; output beyond the tail |
| `ToolDenied` | `Decision {allowed: false, tool, source}` | The arguments of the call, which can hold a secret or a file's contents |
| `GoalMissing`, `JudgeVerdict`, `PlanRejected`, `Escalation` | Their activities' texts, as recorded | A judge's scores alone (a number teaches nothing) |
| `Outcome` | The closing `Status` and the turn's `TurnCost` | Cost as a signal of anything beyond "this took N turns and $M" |
| `RepeatedCommand` | `tool_started` for a shell tool whose command text repeats in the same turn, each followed by a failure the harness reported | Commands that succeeded on the first try |

An observation is derived **once per turn, at the turn's end**, by the engine that ran it (local or worker), from the events of that turn only, and written with an idempotency key `(branch, incarnation, turn, kind, blake3(text))`, so a turn recovered and settled twice yields one set, and a branch removed and recreated (a new incarnation) is a new source. `events` names `(incarnation, seq)`, the unique key of the `events` table, not the store's `id`; an observation keeps its own text, so a branch removed with `by rm` leaves a reference that no longer resolves, not a lost observation. Deriving is deterministic and needs no model; a turn that ends before provisioning derives nothing. `[memory] capture = false` turns it off for a repository; a server's tenant setting can turn it off for a tenant.

**Visibility.** An observation takes the visibility of the session it came from. A private session's observations are `Private { subject }` and feed only that person's `Person` scope; a shared session's are `Shared` and feed the `Repo` scope. A person promotes a private memory to the team by hand ([review](#5-review-a-person-decides-what-is-shared)); consolidation never does.

**Scrubbing happens before storage**, in the engine that ran the turn, which is the one process that still holds the turn's secret **values**: `Provisioning` names the secrets and the engine injected them, so at the turn's end it scrubs every exact value, and its base64 and URL-encoded forms, from the text, then shapes (`AKIA…`, `ghp_…`, `sk-…`, `xox…`, JWTs, PEM blocks, `Bearer …`, `password=`, 32-plus-character hex or base64 runs after a key-like word), URLs cut at `?`, and email addresses outside the repository's committers replaced by `<email>` ([open decision 4](#open-decisions)). This scrubber is **new**: nothing in the code base removes secret values today. Each removal is counted in `scrubbed` by kind. An observation that is more than a quarter scrubbed is dropped, with a warning event. The test corpus of secrets is generated; no real one is ever a fixture.

## 3. Where it lives

Two tables and a link table behind one trait, on SQLite and PostgreSQL with one conformance case each. Every row carries a **tenant**: the knowledge table is keyed by `repo` alone, and a served repository can be on several tenants' allowlists, so a scope without a tenant would make tenant A's corrections tenant B's memory.

```rust
pub trait MemoryBackend: Send + Sync {
    fn observe(&self, obs: &[NewObservation]) -> Result<Vec<u64>>;          // idempotent per key
    fn observations_since(&self, scope: &Scope, watermark: u64, limit: usize) -> Result<Vec<Observation>>;
    fn memories(&self, filter: &MemoryFilter) -> Result<Vec<Memory>>;
    fn propose(&self, m: &NewMemory) -> Result<u64>;
    fn decide(&self, id: u64, from: MemoryStatus, to: Decision) -> Result<bool>; // compare-and-swap, as knowledge
    fn merge(&self, into: u64, from: u64) -> Result<()>;                     // provenance union, `seen` summed
    fn forget(&self, id: u64) -> Result<Forgotten>;                           // hard delete, plus what to unpublish
    fn run_started(&self, scope: &Scope, watermark: u64) -> Result<Run>;      // section 4
    fn run_finished(&self, run: &Run, outcome: &RunOutcome) -> Result<bool>;  // compare-and-swap on the watermark
}

pub struct Memory {
    pub id: u64,
    pub scope: Scope,                 // exactly one owner
    pub kind: MemoryKind,             // Rule, Procedure, Fact, Pitfall
    pub title: String,                // ≤ 80 characters, one line
    pub body: String,                 // Markdown, ≤ 8,000 characters
    pub paths: Vec<String>,           // globs it applies to; empty for the whole repository
    pub task_kinds: Vec<TaskKind>,
    pub status: MemoryStatus,         // Proposed, Approved, Rejected, Forgotten
    pub provenance: Vec<u64>,         // observation ids; empty for one a person wrote
    pub seen: u32,                    // how many consolidations found evidence for it
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub decided_by: Option<String>,
    pub decided_ms: Option<u64>,
    pub note: Option<String>,         // the distiller's why, or the rejection's reason
    pub published: Option<Published>, // {skill, digest, path} once it is in a bundle
}

pub enum Scope {
    Repo { tenant: String, repo: String },                  // team memory of one repository
    Person { tenant: String, subject: String, repo: Option<String> },
}
```

The scope is the owner and the audience at once: a `Person` memory is read only by that person's turns and `by memory` run as that person (an administrator sees that it exists, its scope and its size, not its body, and the read is audited); `Repo` is the shared memory, and the only one that needs a reviewer other than the subject. Locally both live in the repository's store; on a server, `Person` rows live in the **server store** (`branchyard-server/src/store.rs`, the tenant-keyed one), because a person's memory outlives any one served repository and a per-repository backend has no place for it. A `Team` scope across repositories and a `Harness` scope are deferred ([Later](#later)). Rows outlive branches, as knowledge rows do, and are deleted with the repository's store.

## 4. Consolidation, deterministic

A **consolidation** is an internal job, not a task: no worktree, no harness turn of its own, run by the trigger dispatcher of a `by serve` or `by worker` process on a schedule. Locally, `by memory consolidate` runs one now. This is M3; the model-driven distiller is M5.

```toml
[memory]
capture = true                                   # derive observations at each turn's end (default true)
external_authors = false                         # corrections by non-members become candidates
consolidate = "0 3 * * *"                        # a cron, in the server's zone; "" for only `by memory consolidate`
keep_observations_days = 90                      # observations older than this are deleted at the next consolidation
max_candidates = 20                              # per consolidation, per scope
distiller = { harness = "claude-code", model = "small", effort = "low" }   # M5; without it, the deterministic pass only
budget_usd = 1.0                                 # the distiller's turn
```

**At least once, made safe.** A trigger's claim holds a run for 11 minutes and another dispatcher refires it when the hold expires ([triggers](triggers.md#exactly-once)); a task gets exactly-once from the operation registry's idempotency key, which an internal job does not have. So two consolidations of one scope can run at once, and the design makes that harmless rather than impossible: a run is a row in `memory_runs (id, scope, watermark_read, watermark_written, started_ms, finished_ms, outcome, by, trigger)`; its fence is renewed while it works (the distiller's turn can outlive the hold); and it commits with `UPDATE memory_watermarks SET watermark = <new> WHERE scope = ? AND watermark = <read>`: the run whose update changes no row **rolls back everything** (its proposals, its evidence) and records `outcome: lost_race`. Two runs spend twice in the worst case; neither duplicates a row.

A consolidation runs these steps for each scope that has new observations since its watermark:

1. **Gather.** Observations since the watermark, at most 500 per scope per run; older ones wait for the next run, so a backlog is worked off in order. Observations whose `author` is `External` are left out unless `external_authors` is set.
2. **Cluster, deterministically.** Normalize text (case, whitespace, numbers to `#`), group by `(kind, normalized text)`; then merge groups whose BM25 similarity over text and paths passes a threshold, using `branchyard::rank`, the Rust port of `tools.py`'s ranker that the registry's R3 ships with a golden fixture against the script ([registry, section 3](skill-registry.md#3-the-index-and-search)). Each cluster carries its observation ids and the paths and task kinds it spans.
3. **Match to existing memories.** A cluster whose text matches an existing memory (any status) the same way is **evidence**, not a new candidate: `seen` rises, `last_seen_ms` moves, `provenance` grows. A match to a `Rejected` memory stays rejected and only records the evidence, so a rejection sticks as a knowledge rejection does.
4. **Propose, verbatim.** A cluster of two or more observations of kind `Correction`, `CheckFailed` or `RepeatedCommand` becomes one candidate: title the first 80 characters; body the observations, each with its branch and turn; `note` says `deterministic`. With a distiller (M5), a `Judge` runs read-only on a scratch branch, as the knowledge distiller does, with the clusters, the memories already approved for the scope and the existing knowledge entries, and must answer one JSON object:

   ```json
   {"memories": [{"title": "Run the parser benches with the 1 MB fixture",
                  "body": "…", "kind": "procedure", "paths": ["crates/parser/**"], "task_kinds": ["perf"],
                  "confidence": 0.8, "evidence": [412, 418, 455]}],
    "knowledge": [{"text": "Run cargo fmt before you finish.", "path": null, "kind": null, "evidence": [401]}]}
   ```

   Parsed strictly: exactly `memories` and `knowledge`; at most `max_candidates` each; `evidence` non-empty and every id one of the observations given to this run, else the whole answer is refused and the deterministic candidates are used, with the reason recorded. A candidate whose `evidence` the distiller invented is the failure this rule exists for.
5. **Dedup and merge.** A candidate matching an existing memory merges into it (step 3's rule); two candidates matching each other merge; the rest are proposed. A `knowledge` candidate, and a deterministic candidate under 600 characters with repository-wide paths, goes to `KnowledgeBackend` as `Proposed` with a **new** source variant, `KnowledgeSource::Consolidation { run, observations }` (`Branch` requires a branch name, and a consolidation has none); the knowledge store's own duplicate rule applies.
6. **Expire.** Observations past `keep_observations_days` are deleted unless a memory's provenance names them. No memory fades in this version ([Later](#later)).
7. **Record.** The run row is finished in the same transaction as the proposals and the watermark's compare-and-swap, so a crash repeats nothing and loses nothing. Each **branch** whose observations the run consumed gets an `Activity::Memory(MemoryActivity::Consolidated {run, scope, observations, clusters, proposed: [ids], merged, by, trigger})` event (every event belongs to a branch; there is no repository event), and `by memory runs` reads the `memory_runs` table.

A consolidation never approves and never publishes. Its only outputs are rows in `Proposed` status and evidence on existing rows.

## 5. Review: a person decides what is shared

```text
by memory list [--scope repo|mine] [--status proposed|approved|rejected|all] [--json]
by memory show ID [--evidence]                       # the memory, and with --evidence each observation, its author and its events
by memory review [--scope …] [--editor EDITOR] [--json]   # walk proposed memories: a approve, r reject, e edit, p promote, s skip, q quit
by memory approve ID... | reject ID... [--reason TEXT]
by memory edit ID [--title T] [--body-file F] [--path GLOB]... [--kind K]
by memory promote ID                                 # a person's memory offered to the team: a new Proposed row in the Repo scope
by memory forget ID                                  # hard delete and unpublish (section 6)
by memory consolidate [--scope …] [--deterministic]  # run one now
by memory runs                                       # consolidations, with counts and outcomes
```

Who may approve:

| Scope | Approver | Default |
|---|---|---|
| `Person` | The subject, for their own memories | Self-approval; a person's own memory never needs anyone else |
| `Repo` | Anyone who may adopt the repository's knowledge (locally `$USER`; on a server a `run` token of the repository) | The same rule as `by knowledge adopt`; `[memory] approvers = ["group:maintainers"]` narrows it on a server ([open decision 2](#open-decisions)) |

Rules that do not bend: a proposed memory is never published and never given to any turn; `by memory` refuses to approve, reject, edit, promote or forget inside a harness (`BRANCHYARD_BRANCH` set; advisory locally, enforced on a server); a `promote` is a new proposal in the `Repo` scope and keeps the person's original as it was; who decided, and when, is on the row; `review` shows each memory with the count of its evidence, the branches it came from and **each correction's author and association**, so the reviewer sees what the model saw and who wrote it. The console's **Decide** view gains a Memory section with the same actions, reading `GET /v1/repos/{repo}/memory?status=proposed`.

## 6. Publication: memory becomes skills

An approved memory becomes a file in a bundle of [section 1](#1-memory-as-a-skill-first). How the bundle reaches turns depends on where it lives:

- **Through git (M1).** `approve` of a `Repo` memory on a local store writes the reference and the topic's `SKILL.md` into `.agents/skills/memory-<topic>/` of the worktree the command runs in, and prints the commit to make. The pull request is the review of what the team will find; `by skills trust` is the gate that already exists. A `Person` memory is written to `~/.config/branchyard/skills/` at once.
- **Through the server registry (M4, after the registry's R4).** On a server, `approve` queues a republish of the scope's bundles, which the dispatcher performs within a minute (or `by memory publish` performs now). The bundle goes to the **per-tenant cache** under source `memory` with `Provenance::Memory {scope, memory_ids}`; it never enters the shared tool store that guests mount. A `Person` bundle is published to that person's library (`user:<tenant>:<subject>`) and nowhere else. The republish does **not** move the policy version: a new digest reaches **new sessions**, as any new source does ([registry, section 1](skill-registry.md#1-sources)).

**Identity and undo.** A scope's bundle is *floating latest*: each turn's manifest records the digest it mounted, so what a turn saw is known from its record, and there is no lockfile to roll back. Undo is `by memory reject ID` (the memory leaves the bundle at the next republish, and the rejection sticks against new evidence) or `by memory forget ID` (the row is deleted). Concurrent approvals queue republishes that could land out of order, so each `(scope, topic)` carries a **generation** number and a republish writes `WHERE generation = <read>`; the loser requeues and reads again.

**Found, not prompted.** No memory is added to a turn's instructions, and nothing is written into a harness's managed block for memory: the only prompted memory is what consolidation proposed to the **knowledge** store and a person adopted, within knowledge's budget, as today. The turn's `INDEX.md` lists a memory skill as it lists any skill (chosen, or in the overflow count), `tools search` ranks its references like any reference, and the `SkillActivity` events record which memory a turn opened, which is how the eval and `by memory usage` learn whether a memory earned its place.

**Deletion.** `forget` deletes the memory row, drops it from its bundle and republishes (through git: rewrites the files and prints the commit to make); the observations it named are kept until `keep_observations_days` unless `--with-evidence`. It records a `MemoryActivity::Forgotten` on the branches whose observations it named, without the text. An erasure request for a person's rows is `purge`, deferred ([Later](#later)) until `Person` rows exist on a server.

## 7. Native memory: Claude Code and Codex

Both harnesses have memory of their own; this design neither replaces it nor ignores it. Written from their documentation; nothing here was run live, and the paths must be confirmed against the installed versions before anything reads them.

| | Claude Code | Codex |
|---|---|---|
| Instructions it reads | `CLAUDE.md` at the repository root and in parents and subdirectories, `.claude/rules/*.md`, the user's `~/.claude/CLAUDE.md`; a line beginning `#` in a session adds to one of them | `AGENTS.md` at the root and nested, `~/.codex/AGENTS.md` |
| Memory it writes itself | An auto-memory directory per project under `~/.claude/projects/<slug>/memory/` (a `MEMORY.md` index and topic files), which the model updates during sessions | Per-user state under `~/.codex/`; whether a given version keeps model-written memory, and where, is version-dependent and not documented in one stable place |
| In a Branchyard private home | Both are inside the branch's private `HOME` and are created fresh per branch, discarded with it; `--isolated` leaves the user's `~/.claude` out by design ([harness integration](harness-integration.md)) | The same |
| Skills it loads itself | `~/.claude/skills/<name>/SKILL.md` (frontmatter first), into which `link_native` links the **chosen** skills | `~/.codex/skills/`, the same |

What this design does with that: a memory skill the person chose for a turn is linked into the native directory like any chosen skill, so the harness's own loader sees its frontmatter and its model opens it when it decides to. Nothing is written to `CLAUDE.md` or `AGENTS.md` in the worktree: those are the repository's files, and an agent's edit to them is a candidate change in its diff, not a memory. Importing what a harness wrote for itself (diffing its memory directory at the turn's end), seeding its auto-memory directory with approved memories, and a `Harness` scope are all deferred ([Later](#later)): the first two were the one place this design prompted what it says should be found.

## 8. Surfaces

| Operation | SDK | `by` | `by --remote` | HTTP | Console | Slice |
|---|---|---|---|---|---|---|
| Add, list, show, export (files) | `add_memory`, `memories` | `memory add`, `list`, `show`, `edit`, `remove`, `export` | no (files in a worktree or the person's folder) | n/a | no | M1 |
| List, show, evidence (rows) | `Yard::memories`, `memory` | `memory list`, `show` | yes | `GET /v1/repos/{repo}/memory[?scope=&status=]`, `…/memory/{id}`, `…/memory/{id}/evidence` | Decide view, Memory section | M3 |
| Approve, reject, edit, promote, forget | `approve_memory`, `reject_memory`, `edit_memory`, `promote_memory`, `forget_memory` | `memory approve`, `reject`, `edit`, `promote`, `forget`, `review` | yes, as the token's name | `POST …/memory/{id}/approve`, `…/reject`, `…/edit`, `…/promote`; `DELETE …/memory/{id}` | yes | M3 |
| Consolidate, runs | `Yard::consolidate(scope, &dyn Judge)` | `memory consolidate`, `runs` | yes (the deterministic pass only, as `knowledge distill` is) | `POST …/memory/consolidate`, `GET …/memory/runs` | runs | M3, M5 |
| Publish | `publish_memory` | `memory publish` | yes | `POST …/memory/publish` | no | M4 |
| Settings | `Yard::use_memory(MemorySettings)` | `[memory]` | the server's | n/a | n/a | M2 |

Reading needs `read`; deciding and adding `run`. Delegation has no memory operations, and no MCP tool is added: a harness proposes nothing.

## 9. Build plan

Each slice ships alone and is tested hermetically against the fake ACP agent, in-process stores on SQLite (and PostgreSQL in CI's postgres jobs), fixture event logs and fake binaries; no model is called. The eval column names what the skill-discovery eval must show, in its real terms ([registry, the eval](skill-registry.md#the-eval)): the hermetic `rank` top-3 rate over scenarios, and, once a run with a model is recorded, `discovery_recall` from `score`. M1 depends on the registry's R1 (references as documents); M4 on its R4 (the server registry); nothing else here depends on a registry slice.

| Slice | What ships | Days | Hermetic tests | The eval must show |
|---|---|---|---|---|
| **M1. Memory as a reviewed skill** | The bundle writer (deterministic, no volatile field, split at the limits); `by memory add \| list \| show \| edit \| remove \| export`, `--mine` for the local `user` source and `.agents/skills/` otherwise; the refusal on a branch; memory drafts from long `knowledge distill` proposals; `evals/skills/scenarios/memory.toml`, whose prompts are corrections from the fixture logs under `tests/fixtures/skill_eval/logs/` and whose `expect_skills` name the memory reference a person would write from them; the eval's `files` seeding `.agents/skills/` with the runner trusting the set, and a `home_files` key for the person's folder | 4 | `crates/branchyard/tests/memory.rs`: the same approved set written twice gives the same digest; a bundle over the limits split with the note; `crates/branchyard-cli/tests/memory.rs`: each command, `add` refused with `BRANCHYARD_BRANCH` set, `--mine` landing in the private `HOME`'s folder; `crates/branchyard/tests/skills.rs`: an untrusted memory set withheld with its reason in `provisioned.skills`; a generated `SKILL.md` held under 4,096 characters by the writer's own test; `tests/test_skill_eval.py`: `memory.toml` validated, the runner's trust step | `rank` over a tree with the fixture memory bundles mounted: the `memory.toml` scenarios at a top-3 rate ≥ 0.8, and no existing scenario lower than before |
| **M2. Observations** | `Observation`, `ObservationKind`, `EventRef`, `Visibility`, `Author` with the association rule, the derivation at turn end for every kind, the value-and-shape scrubber, the idempotency key with the incarnation, `MemoryBackend::observe` and `observations_since` on both stores with the tenant key, `[memory] capture` and `external_authors`, `by memory observations` (hidden; for the tests and the curious) | 5 | `crates/branchyard/src/memory/tests.rs`: each kind from a fixture log; the extractor's skip list honoured; a turn settled twice yielding one set and a recreated branch yielding a new one; a private session's observations marked `Private`; an external author's comment recorded and excluded; every scrub pattern and every injected value (and its encodings) on a generated corpus, a quarter-scrubbed observation dropped with its warning; conformance `memory_observations` on both stores, two tenants on one repository kept apart | None (nothing new is searched); `tests/test_knowledge*.py` and `crates/branchyard/tests/knowledge.rs` unchanged |
| **M3. Memories, review and deterministic consolidation** | `Memory`, `Scope`, `MemoryStatus`, propose/decide/merge/forget on both stores (`Person` rows in the server store on a server); clustering with `branchyard::rank`, matching, the verbatim candidates, `memory_runs` with the renewed fence and the watermark compare-and-swap, `KnowledgeSource::Consolidation`, `MemoryActivity::Consolidated` per branch, `[memory] consolidate` as a dispatcher job; `by memory list \| show \| review \| approve \| reject \| edit \| promote \| forget \| consolidate \| runs`, the routes, the client, approver rules; `approve` writing the files of M1 | 7 | Two consolidations over one fixture log proposing once and recording evidence the second time; a rejected memory gaining evidence and staying rejected; a short repository-wide candidate landing in the knowledge table as `Proposed` with the `Consolidation` source; two dispatchers racing one schedule time, one `lost_race` and no duplicate row; a crash between proposal and watermark replayed without duplicates; compare-and-swap decisions (two deciders, one wins); a `Person` body hidden from another principal over HTTP; `promote` making a new proposal and leaving the original; `forget` recording no text; `review` on stdin; the golden fixture of `branchyard::rank` against `tools.py` | None new; the M1 scenarios unchanged |
| **M4. Publication through the server registry** (needs registry R4) | The republish on approve and forget into the per-tenant cache, `Provenance::Memory`, the `(scope, topic)` generation, `Person` bundles into the subject's library, `by memory publish`, `by memory usage` from `SkillActivity`, the digest per turn in the manifest | 4 | `branchyard-server/tests/memory.rs`: a `Person` memory skill mounted for its subject only; a republish with a stale generation requeued, not lost; `forget` republishing without the reference; the policy version unchanged by a publish; nothing of a memory bundle under the shared store root | Unchanged numbers; a second scenario file of organisation-wide fixture memories at a top-3 rate ≥ 0.8 |
| **M5. The harness distiller** | The `Judge`-based distiller with strict parsing and fallback, `[memory] distiller`, `budget_usd` | 5 | The distiller's protocol with the fake agent answering from files: a valid answer used, an invented evidence id refusing the whole answer and falling back, a budget exceeded falling back; the fence renewed past the hold while the fake agent waits (`wait::until`, no sleep) | **Live, when run:** one dated record under `evals/skills/records/` of a real harness and model over a repository with ten approved memories: for each of ten tasks whose fixture corrections the memories came from, whether the turn's `Searched` found and `Opened` the memory before repeating the mistake, against a control run with the memory skills untrusted. The number that says it helps is corrections per task, before and after, from the `Correction` observations themselves. Labelled live with the harness, model and date; it never stands in for the hermetic numbers |

Documentation moves with each slice: this page's status paragraph, `knowledge.md` (the hand-off and the drafts), `knowledge-skills.md` (memory bundles as repository skills), `surfaces.md`, `server.md`, `triggers.md` (the consolidation job and its at-least-once rule), `observability.md` (the `Consolidated` event), the registry page, and `python3 tools/check.py --fix` for the schemas of new `Activity` variants.

## Later

Cut from the first plan, kept with the corrections the review made to them.

- **Decay and fading.** `strength = seen × 2^(−(now − last_seen_ms)/half_life)`, a true half-life (the first draft's `e^(−t/h)` is a mean life: seen once, it would have crossed `fade_below = 0.5` at 20.8 days, not 30). A memory under `fade_below` becomes `Faded` (hidden from review, dropped at the next republish, not deleted) and keeps `faded_from` so new evidence revives it to the status it had. **Approved memories do not fade** (`approved_decays = false` by default): a person approved them, and fading one silently would break "never silently". Build it once consolidation has run long enough to show stale proposals piling up.
- **A `Team` scope across repositories**, approved by an `admin` or a group the setting names, with the group as audience; **`purge --person SUBJECT`** for erasure requests, republishing shared bundles without the subject's footers. Both need `Person` and `Repo` rows on a server first (M3, M4).
- **A `Harness` scope**, `HarnessMemory` observations imported from a harness's own memory directory in the private home (one per added or changed paragraph, a claim and labelled so), and seeding that directory with approved memories. The import is a diff of paths that must be confirmed per harness version; the export was the one place this design prompted memory, so if it returns it goes through the knowledge store, which needs a harness filter `KnowledgeScope` does not have.
- **`SelfReport`**: `by memory note "…"` run by a harness inside a turn, captured as an observation with the branch and turn, never approved by itself. Cheap, and the one path by which a model writes toward memory; everything else is derived from what it did.
- **Native memory import** for Codex, once a version documents where it keeps model-written memory.

## Open decisions

1. **Where consolidation runs locally.** On a server it is a dispatcher job. Locally, `by memory consolidate` by hand, or also a step `by run` and `by merge` take when `[memory] consolidate` is set and the last run is older than the cron's period? The second gives a laptop user dreaming without a daemon and makes a merge slower.
2. **Who approves `Repo` memories on a server.** Through git (M1) the pull request's reviewers decide. For rows a consolidation proposes (M3), knowledge lets anyone with `run` adopt; keep that, or require `approvers` on a server by default?
3. **Who is a member.** The author rule needs "member of the repository": locally a committer of the repository, on a server a principal of the tenant. A contributor whose first pull request this is counts as external under both, and their review comments would be excluded from candidates. Right default, or should a maintainer's approval of the pull request make its thread members' comments count?
4. **The scrubber's email rule.** Replace every email outside the repository's committers, or every email? Committers' addresses are already in the git history the agent reads.
5. **Topic grouping.** By `paths` prefix (one bundle per crate or directory), by `task_kind`, or by the distiller's own `topic` field? Path prefixes are deterministic and match knowledge's scopes; the distiller's topics read better. Recommendation: paths, with the distiller allowed to name a topic that the writer uses only when it names an existing bundle.
6. **Personal memory on several machines.** M1 keeps it in `~/.config/branchyard/skills/` on one machine. Until M4 publishes it to the person's library on a server, is a person expected to sync the folder themselves, or should `by memory export --mine` and `add --from` be the documented way to carry it?
