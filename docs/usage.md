# Usage meters and adopting sessions

Two things about the harness logins on this machine: how much of each Claude Code and Codex login's 5-hour and weekly limits is used (`by usage`), so `by run`, `by fan` and the router can avoid one about to run out; and the sessions already in those logins' stores, one of which `by adopt` turns into a Branchyard branch that carries on where the session stopped. Both read only the harness's own session files, never a credential file. Both are ports of Orca (MIT, pinned at `2807332`; see [third-party sources](../THIRD_PARTY.md)).

## `by usage`

```text
$ by usage
LOGIN        WINDOW  USED  RESETS                              TOKENS   COST
claude-code  5-hour  50%   in 2h 13m (2026-10-01 16:00 UTC)      2.0k  $0.02
             weekly  -     rolling 7 days                        7.0k  $0.04
codex        5-hour  93%   in 1h 0m (2026-10-01 14:47 UTC)       1.1k  $0.01
             weekly  40%   in 2d 23h (2026-10-04 13:47 UTC)      1.1k  $0.01
```

`by usage --json` prints `{"now_ms", "near_percent", "logins": [...]}`; each login has `harness`, `account`, `dir`, `found`, `last_seen_ms`, `notes`, and `five_hour` and `weekly` windows, each `{window, minutes, used_percent, resets_at_ms, tokens, cost_usd, limit_reached, source}`. `source` says where the percent came from: `rate_limits` (the harness's own report), `budget` (your `[usage]`), `limit_message`, or `none`.

| Login | Read from | What it gives |
|---|---|---|
| Codex | `$CODEX_HOME` or `~/.codex`: `sessions/**/*.jsonl` and `archived_sessions/**/*.jsonl` (rollouts) | Codex writes its rate limits into every `token_count` event: `primary` and `secondary` windows with `used_percent`, `window_minutes` and `resets_at`. The newest report across the rollouts is classified as Orca does (300 minutes is the 5-hour window and 10080 the weekly one, a minute either way; otherwise primary is the 5-hour and secondary the weekly). A window whose reset has passed since is shown as 0% used. Tokens and cost are summed from `last_token_usage` (or the change in `total_token_usage`) inside each window. |
| Claude Code | `$CLAUDE_CONFIG_DIR` or `~/.claude`: `projects/**/*.jsonl` and `transcripts/**/*.jsonl` | Claude Code records token counts, not its limits. Assistant records are read as Orca reads them (records repeated while streaming deduplicated by message and request ID, keeping the fullest), and the 5-hour window is rebuilt: it opens at the hour of the first message after the previous window ended and lasts five hours, so it resets five hours after that hour. The weekly window is the last seven days (Claude's own weekly reset is not on disk). A percent needs `[usage] claude_five_hour_tokens` (and `claude_weekly_tokens`): the tokens you know a window allows. A "usage limit reached" message Claude Code wrote into a transcript marks the window full, with the reset it named. |

Tokens count input, output and cache writes; cache reads, billed at a tenth, are left out of the count but priced in the cost. Cost is an estimate from Orca's price tables, kept as data in [`catalog/pricing.toml`](../catalog/pricing.toml) (generated from the vendored TypeScript by `BRANCHYARD_BLESS=1 cargo test -p branchyard-controls catalog`, and checked against it on every test run). Files untouched for nine days are not read.

### The guard and the router

```toml
[usage]
guard = "warn"                 # or "refuse", or "off"
near_percent = 90              # a window this full counts as near its limit
skip_over = 95                 # the router passes over a candidate this full
claude_five_hour_tokens = 4000000
claude_weekly_tokens = 40000000

[usage.accounts.work]          # another login to meter, by its configuration directory
harness = "claude-code"
dir = "~/.claude-work"
```

- `by run` and `by fan` (not routed) look at each named harness's login before creating anything: near its limit (`near_percent`, default 90, of either window, or a limit message), `warn` (the default) says so on stderr and goes on; `refuse` stops with `the codex login has used 93% of its 5-hour window; it resets in 1h 0m; [usage] guard = "refuse" stops new branches on it`, and nothing is created; `off` does not look.
- Routed (`--auto`, or a `[fleet]` and no `--harness`; [fleet](fleet.md)), a candidate whose login has used more than `skip_over` of a window is excluded like an unavailable one, and the route says why (`by:   not codex: the codex login has used 99% of its 5-hour window; …, over [usage] skip_over = 95`). With `guard = "refuse"`, `near_percent` excludes too; with `warn`, a near candidate is only warned about.
- A harness maps to a login by its ID: `claude-code` and its profiles to the Claude Code login, `codex` and its profiles to Codex. The guard and the router check the default logins (the ones a harness uses unless its configuration directory is changed); `[usage.accounts]` adds named logins to `by usage`, not to the guard.
- `by watch`'s header ends with a summary, refreshed every minute: `usage claude-code 5h 50% wk 7.0k · codex 5h 93% wk 40%`, red when a window is full.

**Named account profiles** are directories: Orca's managed accounts are separate `CLAUDE_CONFIG_DIR` or `CODEX_HOME` directories, and so are these. Which account a given branch's harness used is not recorded, so the guard and router see only the default login; per-branch account selection is deferred.

**Not done.** Orca's other sources of the same numbers need credentials or a running harness (Claude's OAuth usage endpoint, Codex's app-server `account/rateLimits/read`, terminal probes of `/status`) and are not ported; nor are Gemini, Cursor, Kimi and the other providers Orca meters. Claude's weekly reset time, and its limits, are unknown on disk. Remote mode: the meters are this machine's, and `by --remote` uses none of it.

## `by adopt`

```sh
by adopt                                   # this repository's Claude Code and Codex sessions, newest first
by adopt 0b5e7a1c --name faster            # make one a branch
by send faster "now add a regression test" # resumes that session in the branch's worktree
```

```text
SESSION   HARNESS      AGE  TURNS  WHERE  TASK
0b5e7a1c  claude-code   9m      1  .      make the parser faster
019a0000  codex         1h      1  .      write the docs
```

**Finding sessions.** Read-only, from the default logins' stores. Claude Code: the project directories of `projects/` whose name is the repository root's, or a subdirectory's, encoded as Claude Code encodes it (Orca's `claude-project-dir-encoding.ts`: one `-` for every character that is not an ASCII letter or digit); each top-level transcript is a session, titled by its summary or first prompt. Codex: every rollout under `sessions/` and `archived_sessions/`, skipping threads Codex started itself (a subagent, a review, a compaction; Orca's `session-scanner-codex-non-user-origin.ts`), titled by its thread name or first message. Either way, only sessions whose recorded directory is in this repository and not one of Branchyard's own worktrees are listed. `--json` prints `id`, `harness`, `file`, `cwd`, `git_branch`, `commit`, `title`, `model`, `started_ms`, `updated_ms` and `turns`.

**Making a branch.** `by adopt SESSION [--name NAME] [--harness PROFILE] [--no-diff] [--json]` (a unique start of the ID will do) creates a branch, its worktree, and records the session as the branch's own, settled `no_changes`, so its next turn (`by send`, or `R`/`s` in `by watch`) resumes the session natively, as any branch's second turn does. The base is chosen conservatively, and the output says which and why:

1. **The commit the session recorded** (Codex writes `git.commit_hash` into its rollout), when this repository has it: `session_commit`.
2. Otherwise **the HEAD of the directory the session ran in**, now: `head`. Claude Code records no commit, so this is what a Claude session gets; that HEAD may have moved since the session, and the notes say so.
3. If that directory has **uncommitted changes to tracked files** against that same commit, they are applied to the new worktree (`git diff --binary HEAD`, then `git apply`), uncommitted: `head_with_diff`, with the files listed. They are the directory's changes, not necessarily the session's, and the notes say that too. Untracked files are never carried over (counted in the notes); changes against a different commit are left out; `--no-diff` leaves them all out. A diff that does not apply removes the new branch again and fails.

For a Claude Code session the transcript is also **copied** to where Claude Code looks for the new worktree's sessions (`~/.claude/projects/<encoded worktree>/<id>.jsonl`), since Claude Code resumes a session only from its current directory's project; the original is not touched. Codex finds a rollout by its ID from any directory. `--harness` picks the profile its turns run with (`claude-code-acp` instead of the default `claude-code-stream-json`); it must be one of the session's harness's. A session written to in the last two minutes gets a note: if it is still open, it and the branch diverge from there.

The branch's first event is `adopted` (`by log`: `adopted claude-code session 0b5e… at 1a2b3c4d5e (head_with_diff), 1 changed file applied`; `by log --json`: `{"activity": "adopted", "adopted": {harness, session, source, cwd, base, how, diff_files, notes}}`). In the SDK it is `Yard::adopt(AdoptSpec)` and `Activity::Adopted`.

**Not done.** Only the default logins' stores are searched (not `[usage.accounts]` directories, and not WSL or remote hosts Orca also scans); other harnesses' sessions (Orca reads OpenCode, Gemini, Cursor and more); a branch for an isolated or sandboxed home (the adopted branch uses your own home, where the session is); and recovering a session's own changes when they were committed elsewhere or mixed with others in the directory.

## What is tested

Hermetic, against fixture session stores, local mock servers and real local processes (`crates/branchyard-cli/tests/devex.rs`, run as root and as an unprivileged user; unit tests beside each module):

- **Meters**: Claude records deduplicated and windowed, a budget's percent, a limit message filling the window with its reset; Codex windows classified as Orca classifies them (by duration, legacy fallback, a minute's tolerance), a passed reset, tokens per window; prices for model names old and new from the generated table, the long-context tiers; the guard warning, refusing (nothing created) and off; the router passing over a candidate over `skip_over` and saying why.
- **Adopt**: a Claude and a Codex session of this repository listed, another repository's and a Codex subagent's not; a Claude session adopted with the checkout's uncommitted change applied and its untracked file left out, the transcript copied where Claude Code looks, the `adopted` event, and its next turn resuming the same session ID (the fake ACP agent standing in for `claude-agent-acp`); a Codex session at its recorded commit with `--no-diff`; refusals.
- **Not tested**: against real Claude Code or Codex files written by current releases (the fixtures follow the formats Orca parses), a real Claude Code `--resume` in the new worktree, macOS (paths normalized to NFC are not tried).
