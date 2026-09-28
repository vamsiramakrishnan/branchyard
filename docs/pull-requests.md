# Pull requests

A branch's life can start from a GitHub issue and end in a merged pull request without leaving `by`:

```sh
by run --issue 42 --check "cargo test" --yes     # issue 42 is the task; branch issue-42-<slug>
by pr issue-42-parser-crash                       # check, push, open the pull request
by pr issue-42-parser-crash --watch --yes         # CI failures and reviews go back into the branch
by show issue-42-parser-crash                     # merge readiness
by open issue-42-parser-crash --editor cursor     # or leave for your editor
```

Everything goes through the [GitHub CLI](https://cli.github.com), `gh`, run from argument vectors in the repository, never through a shell. `gh` already handles logins, GitHub Enterprise hosts and choosing the repository from the git remotes, so Branchyard holds no GitHub token and speaks no GitHub API of its own. `by` checks `gh auth status` before its first call and stops with an explanation when `gh` is missing (install it and run `gh auth login`) or not logged in, before anything is pushed. `by pr` refuses to run inside a harness: it pushes with your git and GitHub credentials, so the person or meta-harness that started the branch runs it.

These commands work in local mode only. `by --remote` refuses `by pr`, `by open` and `by show --refresh` with an `unsupported` error naming the reason: a server's branches live in the server's repository, whose git remote and `gh` login are the server's, and a server's worktrees are on the server. `by run --issue` works remotely: `gh` runs on your machine and the server receives the prompt. See [surfaces](surfaces.md#added-with-pull-requests).

## Starting from an issue

`by run --issue <URL|#N|N> ["more instructions"]` (and `by fan`, `by spawn`) fetch the issue with `gh issue view <ref> --json number,title,body,url,labels`. The prompt is the issue under a header, then any prompt you gave:

```text
Resolve GitHub issue #42: Parser crash on empty input
https://github.com/acme/widgets/issues/42
Labels: bug

<the issue's body>

Additional instructions:
<your prompt, if any>
```

The branch is named `issue-<n>-<slug of the title>` (`issue-<n>-<slug>-2` if that is taken; a fan-out appends each harness as usual), unless `--name` says otherwise. The link is recorded on the branch as an `issue_linked` event, and the header itself is the durable fallback where no event can be recorded (a child spawned inside a harness, a run on a server). The pull request that `by pr` opens for the branch says `Closes #<n>`, or `Closes owner/repo#<n>` when the issue is in another repository.

## `by pr`

```text
by pr <BRANCH> [--git-remote NAME] [--head BRANCH] [--base BRANCH] [--title TITLE] [--draft]
               [--gh-repo OWNER/REPO] [--force] [--no-check | --allow-failing-check]
               [--allow-not-ready] [--json]
by pr <BRANCH> --watch [--interval SECS] [--max-rounds N] [--yes | --ask] [--command CMD]
```

1. **Readiness.** The branch must be `ready`. A running branch is refused (wait for its turn), a locally merged one too. `--allow-not-ready` pushes the candidate of a branch that failed, was interrupted or stopped at a limit.
2. **The check.** If the branch has a check (`--check` when it was created), it runs on the candidate alone, checked out in a temporary worktree, with `by merge`'s timeout; nothing is merged. A failed check stops `by pr` with the end of its output; `--allow-failing-check` pushes anyway, and the body says the check failed. `--no-check` skips it. A check that already ran on the same candidate is not run again.
3. **The push.** The candidate commit is pushed to `--git-remote` (default `origin`) as `refs/heads/<head>`, where `<head>` is `--head`, else the branch the pull request already uses, else the branch's git branch `by/<name>`. git runs without a terminal prompt and without the repository's hooks, so credentials come from your credential helper or SSH agent. A remote branch the candidate does not descend from is refused unless `--force`. The option is `--git-remote`, not `--remote`: `--remote` is `by`'s global option naming a Branchyard server.
4. **The pull request.** `gh pr list --head <head> --state open` finds an open pull request for the branch; if there is one it is updated with `gh pr edit` (the body always; the title and base only when `--title` or `--base` are given), otherwise `gh pr create --head <head> --title … --body-file -` opens one (`--base`, `--draft`). Running `by pr` again therefore updates the same pull request. `--gh-repo OWNER/REPO` is passed to `gh` as `-R` when it cannot tell the repository from the remotes.

The body is regenerated on every push (edits to it are replaced): the task (the prompt, quoted), `Closes #n` for a linked issue, then a table with the git branch and harness, turns, cost (the harness's own estimate), the check and its result on which commit, the candidate's diffstat, and `git diff --stat` in a collapsed block. The title defaults to the issue's title, or the prompt's first line.

`--json` prints `{"branch", "created", "pull_request": {number, url, head, base, draft}, "pushed": {remote, remote_branch, commit, forced}, "check": {commit, argv, passed, timed_out, output_tail} | null}`.

## `by pr --watch`

`--watch` does what `by pr` does, then follows the pull request until it is merged or closed, `--max-rounds N` rounds of feedback were delivered, or you press Ctrl-C (nothing is lost: what was delivered is in the log). A branch that is running when the watch starts, and already has a pull request, is watched as it is.

Each poll:

1. **Push what is new.** If the branch settled `ready` on a candidate that was not pushed yet (the turn the last feedback started, or one someone else ran), the check runs on it, it is pushed and the pull request's body updated. If the check fails, that failure (with the end of its output) becomes feedback for the branch instead, keyed by the commit.
2. **Observe.** `gh pr view <n> --json number,url,state,isDraft,headRefOid,reviewDecision,mergeable,mergeStateStatus,reviews,comments`, `gh pr checks <n> --json name,state,bucket,link,workflow`, and the review threads through `gh api graphql` (`reviewThreads`, with `isResolved` and each comment's path and line). A changed observation is recorded as an `observed` event.
3. **Stop** when the pull request is merged or closed (`watch_stopped`).
4. **Collect feedback** not delivered before:
   - a failed check (`bucket` `fail`), keyed `ci:<check>:<head commit>`, so the same check failing again on a new push counts as new; for a GitHub Actions check, `gh run view <run> --log-failed`, bounded to its last 80 lines and 6000 bytes, is attached (once per run);
   - a review with a body that requested changes or commented, keyed `review:<id>`;
   - a comment on the pull request, keyed `comment:<id>`;
   - each comment in an unresolved review thread, keyed `thread:<id>` (resolved threads are skipped).
5. **Deliver** everything new as one message, listing each piece with its author, file and line or its failed log, and asking the harness to address it on the branch. If the branch has a running turn, the message is steered into it (`by send --steer`'s path; the log shows `steered by by pr --watch`) and the watch waits for that turn to end. Otherwise, or if the harness does not take it, the message starts a new turn with `by send`'s path, with the watch's `--yes`, `--ask` and `--command`, and the turn's output streams to the terminal as `by send` shows it. The next poll pushes what the turn made.

Polls start `--interval` apart (default 30 seconds) and double while nothing changes, up to ten times the interval; any change, push or delivery resets it.

**Each piece is delivered once.** Before the message is sent, a `feedback_delivered` event records its keys and how (`steer` or `send`). If a send cannot start (the branch is busy, a harness is missing), a `feedback_undelivered` event takes those keys back so a later poll sends them again. A later watch, after a restart or Ctrl-C, folds the log and skips every key already delivered. If the process dies between recording and sending, that piece is not sent again: the log errs towards never repeating feedback to the harness. A turn that fails after receiving feedback stops the watch with an error.

## Merge readiness

`by show <branch>` adds a `merge readiness` line once a branch has been checked or pushed for a pull request, computed from the last recorded check, push and observation, without a network call:

```text
merge readiness  not ready: 1 CI check failed, 2 unresolved threads · check passed · PR #7 open ·
                 CI 3 passed, 1 failed (lint) · 2 unresolved threads · mergeable (observed 4m ago)
```

It combines the local check (passed, failed, or stale when it ran on an older candidate), the pull request's state and draft flag, CI counts with the failing checks' names, unresolved review threads, mergeability, GitHub's merge state (`behind`, `blocked`) and review decision. The verdict is `ready to merge`, `not ready: <blockers>`, `merged`, `closed`, or `unknown` until the pull request has been observed. `by show --refresh` observes it now (the same three `gh` calls as a watch poll) and records the observation first.

`by show --json` carries it as `merge_readiness` (`null` for a branch that never went towards a pull request):

```json
{
  "verdict": "not_ready",
  "blockers": ["1 CI check failed", "2 unresolved threads"],
  "local_check": {"state": "passed", "commit": "…", "argv": ["cargo", "test"]},
  "issue": {"number": 42, "url": "…", "title": "…"},
  "pushed": {"remote": "origin", "remote_branch": "by/feat", "commit": "…", "forced": false},
  "pull_request": {"number": 7, "url": "…", "head": "by/feat", "base": null, "draft": false},
  "observation": {"number": 7, "url": "…", "state": "open", "draft": false, "head_commit": "…",
                  "review_decision": null, "mergeable": "mergeable", "merge_state": "unstable",
                  "ci": {"passed": 3, "failed": 1, "pending": 0, "skipped": 0, "failing": ["lint"]},
                  "unresolved_threads": 2},
  "observed_at_ms": 1790000000000,
  "feedback_rounds": 1
}
```

## `by open`

`by open <branch> [--editor EDITOR] [--print]` starts an editor on the branch's worktree: `--editor`, else `$VISUAL`, else `$EDITOR`. `--editor` takes a known short name (`code`, `cursor`, `windsurf`, `zed`, `subl`, `idea` and the other JetBrains launchers, `fleet`, `xcode` for `xed`, `vim`, `nvim`, `emacs`, `hx`, `nano`, `micro`, `kak`, with aliases such as `vscode` and `sublime`) or any command line, split like a shell, with the worktree added as its last argument. With no editor set it refuses and lists the names; a named editor not on `PATH` is refused by name. `--print` prints the worktree's path instead, for `cd "$(by open b --print)"`. A branch without a worktree (waiting, or removed) is refused.

## The events

Every step is an `Activity::PullRequest` event on the branch's log, shown by `by log` and `by log --json` (`{"activity": "pull_request", "pull_request": {"kind": …}}`) and tagged by `kind`:

| `kind` | Fields | `by log` |
|---|---|---|
| `issue_linked` | `number`, `url`, `title` | `issue #42 linked: … (url)` |
| `checked` | `commit`, `argv`, `passed`, `timed_out`, `output_tail` | ``check `cargo test` passed on 1a2b3c4d5e`` |
| `pushed` | `remote`, `remote_branch`, `commit`, `forced` | `pushed 1a2b3c4d5e to origin by/feat` |
| `opened`, `updated` | `number`, `url`, `head`, `base`, `draft` | `pull request #7 opened: url` |
| `observed` | the observation above | `pull request #7 open, CI 3 passed, 1 failed (lint); 2 unresolved threads; mergeable` |
| `feedback_delivered` | `keys`, `via`, `summary` | `pull request feedback sent into the branch (send), 2 pieces: …` |
| `feedback_undelivered` | `keys`, `reason` | `pull request feedback not delivered (…)` |
| `watch_stopped` | `reason` | `stopped watching the pull request: the pull request was merged` |

The CLI folds these (`pr::state`) into the branch's issue, last check, last push, pull request, last observation and delivered keys. In the SDK, `Branch::record_pull_request` appends one, `Branch::verify_candidate` runs the check on the candidate in a temporary worktree, and `Branch::push_candidate` pushes it (`Repository::verify` and `Repository::push` in `branchyard-workspace`). `by watch` can bind keys to `pr::publish`, `pr::readiness`, `open::plan` and `open::launch`, which print nothing.

## Not done

- Only GitHub, through `gh`. GitLab (`glab`) and others would fit the same events.
- The watch does not reply to or resolve review threads, request re-review, or merge the pull request; merging stays a person's decision (or `gh pr merge`).
- `--draft` applies when the pull request is created; turning a draft ready is `gh pr ready`.
- Comments from bots are fed back like anyone's; there is no filter by author yet.
- Remote mode: a server would need its own git remote and `gh` login per repository, and an operator's decision to let clients push through it. Not built.
- Tested only against a fake `gh` and a bare local remote ([validation](validation.md)), never against GitHub itself.
