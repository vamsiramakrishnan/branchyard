# Checkpoints, rewind, try and compare

Every turn leaves a checkpoint. A branch can be forked from any of them, or rewound to one and forward again. A branch's changes can be tried in the repository's own checkout and taken back out exactly. Several attempts at one task can be put side by side, and one of them merged. These answer items 5 and 6 of the [developer experience plan](devex.md#plan): §3 (compare), §6 (undo one step) and §7 (try in the running app).

> **Status.** Implemented in local mode. Tested hermetically against the fake ACP agent, including real crashes (a child process aborted between an intent and its effect) for rewind and try. `by compare` also works with `by --remote`; checkpoints are listed remotely from the event log. Rewind, `fork --at`, `compare --diff`, `compare --check` and `try` are local only for now (see [what is not done](#not-done-yet)). Not yet run with a real harness.

## Checkpoints

A turn that submitted its prompt ends with a snapshot: a commit on `by/<name>` holding everything in the worktree that `.gitignore` does not exclude. After it, the engine points a ref at the branch's head:

```
refs/branchyard/<branch>/<incarnation>/turn-<N>
```

This is a journaled step (`checkpoint`, after `snapshot`), so a turn that recovery finishes gets its checkpoint too, and a replayed step does not record it twice. The ref names a commit the branch already has; nothing is copied. A turn that produced no change gets a checkpoint at the same commit as the one before. Turn 0 is the branch's base and has no ref. The incarnation keeps a removed and recreated branch's refs apart.

Each checkpoint is also an event, `Activity::Checkpoint`:

| Field | Meaning |
|---|---|
| `turn` | The turn's number, from 1 |
| `commit`, `git_ref` | The branch's head after the turn, and the ref naming it |
| `after` | The checkpoint the turn started from: the previous one, or the rewound-to one after a rewind; `Some(0)` for the base; `None` for turns recorded before checkpoints existed |
| `session` | The harness session when the turn ended |
| `files_changed`, `insertions`, `deletions` | Against the branch's base |

`by show` lists them, marking the one the branch is at (`*`) and flagging a ref that no longer names its commit; `by show --json` adds a `checkpoints` object. `by log` prints each as `checkpoint N at <commit> (...)`. In the SDK, `Branch::checkpoints` returns them with each turn's prompt, and `branchyard::recorded_checkpoints` reads them from any event list, which is how `by --remote show` lists them.

**Turn numbers only grow.** A rewind does not renumber: after rewinding from turn 3 to turn 1, the next turn is 4, with `after: 1`. Nothing is overwritten, so every checkpoint stays reachable until the branch is removed. `by rm` deletes the branch's refs with it (for every incarnation of the name); a merged branch keeps its git branch, as before, but not its checkpoint refs. There is no other garbage collection: the refs are small, and the commits are the branch's own.

**Decided: no checkpoint at turn start.** After a turn the worktree is clean, because the snapshot committed everything. Edits made between turns (by a person, in the worktree) are in the next turn's checkpoint rather than one of their own. A rewind refuses to discard such edits (below), so nothing is lost.

## Sessions: native where it ended, otherwise a summary

A harness's session cannot be cut back to an earlier turn: resuming it resumes everything it saw. So Branchyard continues the harness's own session from checkpoint N only when that session *ended* at N, meaning no later turn continued it, and the harness can (resume for a rewind, fork for a fork). Otherwise the next turn starts a fresh session whose prompt begins with a generated summary of the turns that led to N: for each, what it was asked, what the harness last replied (quoted up to 1500 characters) and what it had changed; then `## Your task now` and the prompt itself. A later summary strips an earlier one from the prompts it quotes.

The decision is recorded as a `SessionContinuity`, on the `Rewound` or `ForkedAt` event and in the command's output:

| `mode` | When |
|---|---|
| `native` (`session`) | The session ended at the checkpoint and the harness supports it |
| `summary` (`turns`, `reason`) | Otherwise; `turns` is the checkpoint's lineage (following `after`), and `reason` says why: the harness cannot resume or fork, no session was recorded there, or "its session S went on to turn 3 and cannot be cut back to turn 2" |
| `fresh` (`reason`) | Checkpoint 0: nothing ran before it |

A fresh session is also stated as a warning when the next turn starts. Rewinding back and then forward again returns to the native session, because the session that ended at the later checkpoint is still intact: rewind from 3 to 1 (summary), then from 1 to 3, and turn 4 resumes the session of turns 1 to 3.

## Fork at a checkpoint

```
by fork <branch> --at N "<prompt>" [task options]
Branch::fork_at(N, prompt, options)
```

A new branch whose base is checkpoint N's commit (the branch's base for 0), leaving the original untouched. Everything else is `by fork`'s: its name, parent, check, command, provider, provisioning, delegation envelope and budget, and a new incarnation. The session forks natively only as above (the fork's cost then counts from the parent's, as a native fork's does); otherwise the new branch starts a fresh session with the summary, without `--fresh-session` (which `--at` does not take). A `ForkedAt` event is the new branch's first, and `by` prints `forked from <branch> at checkpoint N; <new> starts a fresh session with a summary of turn 1: ...`.

## Rewind

```
by rewind <branch> --to N [--yes] [--json]
Branch::rewind(N)
```

Resets the branch itself (`by/<name>`, its worktree and its candidate) to checkpoint N. It is refused while a turn runs (the branch's lease is held, `Error::Running`), for a branch that has not started, for a merged branch (fork it instead), when the worktree is not on its branch, and when the worktree holds changes no checkpoint has; the refusal lists them. `by rewind` asks for confirmation on a terminal and is refused without `--yes` elsewhere. Afterwards the branch is `ready` (or `no_changes` at the base), its candidate is the checkpoint's commit, and its next `send` continues as the session decision says. Later checkpoints stay, so `by rewind <branch> --to <later>` undoes a rewind.

A rewind runs under the branch's lease, like a merge or a removal, as the journaled step `rewind`. Its intent is recorded before anything changes, and holds everything the rewind will do: the target checkpoint and commit, the branch's head before, the session decision and the summary. Then `git reset --hard` to the commit and `git clean -fd` in the worktree (ignored files stay), then the record, the outcome, a `Rewound` event and the lease release. An engine that stops anywhere in between leaves a stale lease with that intent; the next `Yard::open`, `send`, `merge` or server recovery pass takes the lease over and finishes the rewind from the intent (removing a lock a killed git left), recording `Recovered` ("a rewind to checkpoint N had begun, and recovery finished it") and `Rewound`. A crash cannot leave the worktree half-reset, and never undoes the rewind: the intent says the person asked for it.

## Try a branch in this checkout

```
by try <branch>       # apply it; another branch's try is turned off first
by try --status [--json]
by try --off [--force]
Yard::try_on, try_off, try_status, try_recover
```

Like Conductor's Spotlight: the branch's candidate diff against its base is applied to the checkout `by` runs in, where the dev server and hot reload already run.

- **Only on a clean checkout.** Anything staged, modified or untracked (ignored files aside) refuses it, listing what, because then `HEAD` is exactly what the checkout held and `--off` can restore it exactly.
- **All or nothing.** The diff is applied with `git apply` (never three-way), which writes nothing when any hunk does not apply: a checkout that moved on where the branch changed a file is refused with nothing changed.
- **Recorded before it is written.** `.branchyard/try/state.json` holds the branch, candidate, base, the checkout's `HEAD`, and for each path the diff touches its entry before (git mode, blob, and the file's permission bits on disk, or absent), then, once applied, after. It is written (synced, renamed) in the phase `applying` before `git apply`, and `applied` after. `HEAD` and the candidate are pinned by `refs/branchyard-try/head` and `refs/branchyard-try/candidate`, so the blobs outlive the branch. It is not `git stash`, and the index is never written: it stays at `HEAD`, so `git status` shows the try as unstaged changes.
- **`--off` restores byte for byte**: every path gets its saved blob's bytes and permission bits back, a symbolic link its target, and a path that did not exist is removed, with the directories applying created. It refuses when a tried file was changed since it was applied, or `HEAD` moved, naming them; `--force` restores anyway. The phase is `restoring` while it writes.
- **Swapping**: `by try <other>` turns the current try off (with the same refusal) and applies the other. Trying the same branch at the same candidate again changes nothing.
- **A try cut short** (a state left `applying` or `restoring`) is rolled back to the saved entries by the next `try` call of any kind before it does anything else, and `by` says so.
- **One at a time**: a lock in `.branchyard/try/` keeps two `by try` processes apart.
- **Local mode only.** A server's checkout is not the one where your app runs; `by --remote try` is refused.

## Compare attempts

```
by compare <branch>... | --fan <name>   [--check] [--json]
by compare --diff <a> <b>
by compare ... --pick <branch> [--into <target>] [--discard-others] [--yes]
Yard::compare, fan_branches, diff_between
```

`--fan <name>` compares the branches one `by fan` started, named `<name>-<harness>` (top-level branches with one prompt). The table has, per attempt: status, turns, cost, tokens (input and output the harness reported; running totals are counted by their growth per session), time (from each prompt to the status that ended its turn), the check, files changed and lines added and removed against its base, and **unique files**, those no other compared attempt changed. `--json` prints the same as `Attempt` objects, including every changed file.

The check column is `none` for a branch without a check, `passed` for one merged (its check passed at the merge), and otherwise `not run`, unless `--check` runs each branch's check on its exact candidate in a private temporary worktree (removed afterwards), with the merge's 30-minute timeout. `--diff A B` prints the diff from A's candidate to B's.

`--pick` merges the chosen attempt through the same validated merge as `by merge` (its check on the exact merge result, compare-and-swap of the target), into `--into` or the current branch. `--discard-others` then removes the other compared attempts with `by rm`, after a confirmation on a terminal, or with `--yes`. Remotely, `compare`, `--json`, `--pick` and `--discard-others` work through the server's records, events, diffs, merge and removal; `--check` and `--diff` are local only.

## For `by watch`

The SDK calls are what a cockpit binds keys to: `Branch::checkpoints`, `Branch::rewind`, `Branch::fork_at`, `Yard::try_on`/`try_off`/`try_status`/`try_recover`, `Yard::compare`/`fan_branches`/`diff_between`, and `branchyard::compare_attempt`, `mark_unique`, `diff_files` and `recorded_checkpoints` for a remote view built from events. In the CLI, `attempts::compare_table`, `checkpoint_lines`, `try_text` and `confirm` render and confirm.

## Not done yet

- **Remote rewind, `fork --at`, `try`, `compare --diff` and `--check`.** Rewind, `fork --at` and the diff need new API operations (the events and the checkpoint refs are already on the server); `by --remote` refuses them with a message. `try` stays local by design.
- **Native fork or resume at a checkpoint was tested against the rule, not a harness**: the fake agent's ACP profile cannot fork, so `fork --at` is tested in its summary mode and rewind in both. A native fork's cost baseline is the parent's cost when forked, which overstates it if later sessions of the parent added cost.
- **A try's `HEAD` guard is coarse**: a commit made in the checkout while a branch is tried refuses `--off` without `--force`, even when it touched nothing tried.
- **Checkpoint refs of branch names that nest** (`a` and `a/5` would share a ref directory) are not possible in local mode, whose names are single segments.
- **Summaries are not model-written**: they quote prompts and replies; no model call is made.

## Code and tests

| Where | What |
|---|---|
| [`checkpoint.rs`](../crates/branchyard/src/checkpoint.rs) | Refs, the `checkpoint` step, lineage, the session decision, summaries, the journaled rewind and its recovery |
| [`run.rs`](../crates/branchyard/src/run.rs) | `fork --at`; a send after a rewind starting fresh with the summary |
| [`spotlight.rs`](../crates/branchyard/src/spotlight.rs) | `try` |
| [`compare.rs`](../crates/branchyard/src/compare.rs), `Repository::check_commit` in [`integrate.rs`](../crates/branchyard-workspace/src/integrate.rs) | Attempts, fan siblings, checks in a private worktree |
| [`attempts.rs`](../crates/branchyard-cli/src/attempts.rs) | `by rewind`, `by try`, `by compare`, the checkpoints in `by show` |
| [`tests/checkpoints.rs`](../crates/branchyard/tests/checkpoints.rs) | A ref per turn, after a restart and gone with the branch; `fork_at` content equal to turn N, with a summarized fresh session; rewind back and forward, native resume only where the session ended, fresh with a summary otherwise, to the base; rewind refused while running and over uncommitted changes; a rewind aborted after its intent and after its reset, finished by recovery; try on a clean checkout and `--off` byte for byte (changed, deleted, added in a new directory, executable bit removed, symbolic link retargeted, permission bits git does not keep); refused on a dirty checkout; a conflict applying nothing; swapping; edits since kept unless forced; a try aborted after its intent, after applying and mid-restore, rolled back exactly; compare with unique files, checks and a diff |
| [`cli.rs`](../crates/branchyard-cli/tests/cli.rs), [`remote.rs`](../crates/branchyard-cli/tests/remote.rs) | `by show`/`log` listing checkpoints, `by rewind` confirmation, back and forward, `by fork --at`; `by compare --fan`, `--json`, `--diff`, `by try`/swap/`--status`/`--off`, dirty refusal, `--pick --discard-others`; `by --remote compare`, checkpoints in `by --remote show`, and remote refusals |
