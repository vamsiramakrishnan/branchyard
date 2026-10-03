# Pull requests

A branch's life can start from a GitHub issue and end in a merged pull request without leaving `by`:

```sh
by run --issue 42 --check "cargo test" --yes     # issue 42 is the task; branch issue-42-<slug>
by pr issue-42-parser-crash                       # check, push, open the pull request
by pr issue-42-parser-crash --watch --yes         # CI failures and reviews go back into the branch
by show issue-42-parser-crash                     # merge readiness
by open issue-42-parser-crash --editor cursor     # or leave for your editor
```

Everything on GitHub goes through the [GitHub CLI](https://cli.github.com), `gh`, run from argument vectors in the repository, never through a shell. `gh` already handles logins, GitHub Enterprise hosts and choosing the repository from the git remotes, so Branchyard holds no GitHub token and speaks no GitHub API of its own. Issues from Linear, Jira and GitLab are fetched from their APIs ([other trackers](#other-trackers)). `by` checks `gh auth status` before its first call and stops with an explanation when `gh` is missing (install it and run `gh auth login`) or not logged in, before anything is pushed. `by pr` refuses to run inside a harness: it pushes with your git and GitHub credentials, so the person or meta-harness that started the branch runs it.

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

## Other trackers

`--issue` also takes a Linear, Jira or GitLab issue, by prefix or by URL:

| Tracker | `--issue` | Fetched with | Credentials |
|---|---|---|---|
| Linear | `linear:ENG-123`, `https://linear.app/<team>/issue/ENG-123/…` | GraphQL `issue(id:)` at `https://api.linear.app/graphql` (`[trackers.linear] url` or `LINEAR_API_URL` for another) | `LINEAR_API_KEY` (a personal API key, sent as it is) or `LINEAR_ACCESS_TOKEN` (OAuth, as `Bearer`) |
| Jira | `jira:PROJ-7`, `https://<site>/browse/PROJ-7` | REST v3 `GET /rest/api/3/issue/PROJ-7?fields=summary,description,labels`; the site is the URL's, else `[trackers.jira] url`, else `JIRA_URL` | `JIRA_EMAIL` and `JIRA_API_TOKEN` (HTTP Basic) |
| GitLab | `gitlab:group/sub/project#12`, `https://<host>/group/project/-/issues/12` | REST v4 `GET /api/v4/projects/<path>/issues/12`; the host is the URL's, else `[trackers.gitlab] url`, else `GITLAB_URL`, else `https://gitlab.com` | `GITLAB_TOKEN` (as `PRIVATE-TOKEN`) |

The prompt is the same as GitHub's, headed by the tracker and its key (`Resolve Linear issue ENG-123: Parser panics`), with the URL, labels and description; Jira's description, in Atlassian Document Format, is rendered as Markdown (Orca's `adf-markdown.ts`, ported under its MIT license: headings, lists with their markers and nesting, code blocks, quotes, rules, and images as links or a visible `*[name]*` placeholder). The branch is named after the key (`eng-123-<slug>`, `proj-7-<slug>`; a GitLab issue `issue-<n>-<slug>`), and the `issue_linked` event and `by show --json`'s `merge_readiness.issue` carry `tracker` and `key` beside the number, URL and title. The mapping of each answer to an issue follows emdash's issue plugins (`packages/plugins/src/issues/impl/{linear,jira,gitlab}/`, Apache-2.0); Linear's query is emdash's summary fields with the labels.

**The pull request names it** with the tracker's own convention: `Closes ENG-123` for Linear (its GitHub integration closes the issue when the pull request merges), `Refs PROJ-7 (<url>)` for Jira (which links a key it sees in a pull request but closes nothing from it), and `Related: group/project#12 (<url>)` for GitLab (a GitHub pull request cannot close a GitLab issue).

**Credentials** are read from the environment when the command runs and sent only in the request's header; they are never printed, logged, recorded in an event or put in the prompt (a test fetches all three and searches every output and the log for them). Without them, a tracker can be reached **through the connector gateway** instead ([connectors](connectors.md)): with `[connectors] gateway` set and

```toml
[trackers.linear]
gateway_tool = "linear__get_issue"   # the tool the gateway serves for your Linear connector
```

`by` signs a five-minute token naming you, granting only `linear:read`, with the yard's key, and calls the tool over MCP Streamable HTTP (`initialize`, then `tools/call` with `{"id": "ENG-123"}`; Jira's tool gets `{"issueIdOrKey": …}`, GitLab's `{"id": <project path>, "issue_iid": …}`). The tool's result, its structured content or its text parsed as JSON, is read as the API's own answer would be (whole, as `{"issue": …}`, or the issue itself). The upstream token stays in the gateway. The tool's name depends on how Anvil compiled the connector, so it is configuration, not built in. The environment wins when both are there.

`[trackers]` takes `url` and `gateway_tool` for each of `linear`, `jira` and `gitlab`, and never a credential; `by config validate` checks the URLs and that a `gateway_tool` has a gateway. With `by --remote`, the issue is fetched on your machine with the environment's credentials, and the server receives the prompt, as for GitHub; the gateway is used in local mode only. `by spawn --issue` inside a harness reads only the harness's environment.

## Starting from a pull request

`by run --pr N ["more instructions"]` (and `by fan --pr N`) starts the branch from GitHub pull request N's head commit: `gh pr view N --json number,title,body,url,headRefName,headRefOid,baseRefName,isCrossRepository,state`, then, if the commit is not here yet, `git fetch origin refs/pull/N/head` (GitHub keeps every pull request's head there, fork or not). The branch is named `pr-<n>-<slug of its title>`, its base is that commit, and its prompt names the pull request (`Continue GitHub pull request #7: Faster parser`, its URL, its head, its description, then your instructions). When the head is a branch of this repository, a `started` event records the pull request (`by log`: `started from pull request #7's head (feature): <url>`), so `by pr` pushes the branch's candidate to that head branch and updates that pull request instead of opening another; a fork's head is not ours to push to, so `by pr` then opens a new one, and `by run` says so. `--pr` takes no `--base` or `--issue`, and runs in local mode only.

## `by pr`

```text
by pr <BRANCH> [--git-remote NAME] [--head BRANCH] [--base BRANCH] [--title TITLE] [--draft]
               [--gh-repo OWNER/REPO] [--force] [--no-check | --allow-failing-check]
               [--allow-not-ready] [--json]
by pr <BRANCH> --watch [--interval SECS] [--max-rounds N] [--no-resolve] [--yes | --ask] [--command CMD]
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

1. **Push what is new.** If the branch settled `ready` on a candidate that was not pushed yet (the turn the last feedback started, or one someone else ran), the check runs on it, it is pushed and the pull request's body updated. If the check fails, that failure (with the end of its output) becomes feedback for the branch instead, keyed by the commit. After a push, the review threads it addressed are answered and resolved ([below](#resolving-the-threads-a-push-addressed)).
2. **Observe.** `gh pr view <n> --json number,url,state,isDraft,headRefOid,reviewDecision,mergeable,mergeStateStatus,reviews,comments`, `gh pr checks <n> --json name,state,bucket,link,workflow`, and the review threads through `gh api graphql` (`reviewThreads`, with each thread's ID, `isResolved` and each comment's path and line). A changed observation is recorded as an `observed` event.
3. **Stop** when the pull request is merged or closed (`watch_stopped`).
4. **Collect feedback** not delivered before:
   - a failed check (`bucket` `fail`), keyed `ci:<check>:<head commit>`, so the same check failing again on a new push counts as new; for a GitHub Actions check, `gh run view <run> --log-failed`, bounded to its last 80 lines and 6000 bytes, is attached (once per run);
   - a review with a body that requested changes or commented, keyed `review:<id>`;
   - a comment on the pull request, keyed `comment:<id>`;
   - each comment in an unresolved review thread, keyed `thread:<id>` (resolved threads are skipped).
5. **Deliver** everything new as one message, listing each piece with its author, file and line or its failed log, and asking the harness to address it on the branch. If the branch has a running turn, the message is steered into it (`by send --steer`'s path; the log shows `steered by by pr --watch`) and the watch waits for that turn to end. Otherwise, or if the harness does not take it, the message starts a new turn with `by send`'s path, with the watch's `--yes`, `--ask` and `--command`, and the turn's output streams to the terminal as `by send` shows it. The next poll pushes what the turn made.

Polls start `--interval` apart (default 30 seconds) and double while nothing changes, up to ten times the interval; any change, push or delivery resets it.

**Each piece is delivered once.** Before the message is sent, a `feedback_delivered` event records its keys and how (`steer` or `send`). If a send cannot start (the branch is busy, a harness is missing), a `feedback_undelivered` event takes those keys back so a later poll sends them again. A later watch, after a restart or Ctrl-C, folds the log and skips every key already delivered. If the process dies between recording and sending, that piece is not sent again: the log errs towards never repeating feedback to the harness. A turn that fails after receiving feedback stops the watch with an error.

### Resolving the threads a push addressed

Once a turn that received review-thread feedback has ended and its candidate is pushed, the watch looks at the threads it just observed. A thread is answered and resolved when all of these hold:

- it is unresolved, and the watch has not tried it before;
- every comment in it with a body was delivered as feedback (a reply the reviewer added since is not, so a thread still under discussion is left open);
- the file the thread is on is one the pushed commits changed (`git diff --name-only <previous push> <this push>`).

For each, `gh api graphql` posts a reply, `Addressed in <commit>.`, with `addPullRequestReviewThreadReply`, then resolves the thread with `resolveReviewThread` (the mutation Orca's `resolve-review-thread.ts` sends, ported under its MIT license; see [third-party sources](../THIRD_PARTY.md)). The output says `resolved the review thread on a.txt, addressed in 1a2b3c4d5e`, and a `threads_resolved` event records each thread's ID, path, whether the reply was posted and whether GitHub reports it resolved, with the error when not. A thread is tried once: a failure (a token that may not resolve threads, a thread deleted meanwhile) is recorded and said, and the watch carries on. `--no-resolve` leaves every thread as it is. Review threads only: a review body or a pull-request comment has nothing to resolve, and the watch never replies to them.

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

## `by review`

`by review <branch>` is the reviewing side of the same loop, before or without a pull request: the branch's candidate diff opens in your editor (`--editor`, else `$VISUAL`, else `$EDITOR`; a graphical editor must wait, as `code --wait` does), you write comments under the lines they are about, and on save every comment goes to the branch as one prompt.

```diff
# by review feat: comment on this branch's changes, then save and quit.
# ...
diff --git a/src/parser.rs b/src/parser.rs
>> Split this file once it passes 500 lines
--- a/src/parser.rs
+++ b/src/parser.rs
@@ -10,6 +10,8 @@ fn parse(input: &str) {
+    if input.is_empty() {
+        return Ok(Vec::new());
>> An empty input should be an error, as the docs say
```

- A comment is a line starting with `>>`; more `>>` lines continue it. It is about the diff line above it: a context or added line is that line of the branch's file; a removed line, the line above where it was removed; an `@@` line, that hunk's lines; a file header, the whole file.
- Lines starting with `#` are instructions and ignored. The diff itself must stay as it was: an edited diff line is refused with its line number (`the diff was edited here`), nothing is sent, and the file is kept, so a comment can never land on the wrong line.
- The prompt is deterministic. Each comment is written as Orca writes review notes (`src/shared/diff-comments-format.ts`, ported under its MIT license): `File:`, then `Line: N`, `Lines: A-B` or `Scope: file`, then `User comment: "..."` with the comment quoted and escaped onto one line, a blank line between comments, under one line saying how many comments on how many files, and ending `Address each comment on this branch.`
- `--print` prints that prompt and sends nothing; `--file FILE` reads an already edited review file instead of opening an editor; `--detach` starts the `by send` in the background (its output in `.branchyard/review/`) and returns. Otherwise it is `by send`, with its `--yes`, `--ask`, `--command` and limits.
- With no comments nothing is sent. A review that was not sent (`--print`, a refused edit, a failed send) is kept in `.branchyard/review/<branch>.diff` and reopened by the next `by review` of the same diff.
- In `by watch`, `v` runs `by review --detach` on the selected branch in the terminal the dashboard leaves to it, and comes back when the editor closes. It works with `--remote`: the diff comes from the server, the editor runs here.

## `by open`

`by open <branch> [--editor EDITOR] [--print]` starts an editor on the branch's worktree: `--editor`, else `$VISUAL`, else `$EDITOR`. `--editor` takes a known short name (`code`, `cursor`, `windsurf`, `zed`, `subl`, `idea` and the other JetBrains launchers, `fleet`, `xcode` for `xed`, `vim`, `nvim`, `emacs`, `hx`, `nano`, `micro`, `kak`, with aliases such as `vscode` and `sublime`) or any command line, split like a shell, with the worktree added as its last argument. With no editor set it refuses and lists the names; a named editor not on `PATH` is refused by name. `--print` prints the worktree's path instead, for `cd "$(by open b --print)"`. A branch without a worktree (waiting, or removed) is refused.

## The events

Every step is an `Activity::PullRequest` event on the branch's log, shown by `by log` and `by log --json` (`{"activity": "pull_request", "pull_request": {"kind": …}}`) and tagged by `kind`:

| `kind` | Fields | `by log` |
|---|---|---|
| `issue_linked` | `number`, `url`, `title`, and for another tracker `tracker`, `key` | `issue #42 linked: … (url)`, `issue ENG-123 linked: …` |
| `started` | `number`, `url`, `head`, `base`, `draft` (`--pr`) | `started from pull request #7's head (feature): url` |
| `checked` | `commit`, `argv`, `passed`, `timed_out`, `output_tail` | ``check `cargo test` passed on 1a2b3c4d5e`` |
| `pushed` | `remote`, `remote_branch`, `commit`, `forced` | `pushed 1a2b3c4d5e to origin by/feat` |
| `opened`, `updated` | `number`, `url`, `head`, `base`, `draft` | `pull request #7 opened: url` |
| `observed` | the observation above | `pull request #7 open, CI 3 passed, 1 failed (lint); 2 unresolved threads; mergeable` |
| `feedback_delivered` | `keys`, `via`, `summary` | `pull request feedback sent into the branch (send), 2 pieces: …` |
| `feedback_undelivered` | `keys`, `reason` | `pull request feedback not delivered (…)` |
| `watch_stopped` | `reason` | `stopped watching the pull request: the pull request was merged` |
| `threads_resolved` | `commit`, `threads` (`id`, `path`, `replied`, `resolved`, `error`) | `1 review thread addressed in 1a2b3c4d5e resolved (a.txt)` |

The CLI folds these (`pr::state`) into the branch's issue, last check, last push, pull request, last observation and delivered keys. In the SDK, `Branch::record_pull_request` appends one, `Branch::verify_candidate` runs the check on the candidate in a temporary worktree, and `Branch::push_candidate` pushes it (`Repository::verify` and `Repository::push` in `branchyard-workspace`). In `by watch`, `p` runs `by pr` after a yes (waited for, its output in a pane), `P` runs `by pr --watch` in the background, `o` opens the worktree through `open::plan` and `open::launch` (leaving the dashboard's screen for a terminal editor, whose `Plan::terminal` says so, and redrawing it after), and the detail pane shows the merge-readiness line folded from the branch's `pull_request` events with `pr::state` and `pr::readiness`; all three keys are refused remotely.

## Not done

- Pull requests only on GitHub, through `gh`. Issues may come from Linear, Jira or GitLab ([other trackers](#other-trackers)), fetched hermetically from local mock servers and a mock gateway in the tests and never from the real services; a GitLab merge request (`glab`) would fit the same events.
- The watch replies only to the review threads a push addressed, and only to resolve them; it does not answer reviews or comments, request re-review, or merge the pull request; merging stays a person's decision (or `gh pr merge`). Whether a thread was really addressed is judged by the file it is on, not by reading the change.
- `--draft` applies when the pull request is created; turning a draft ready is `gh pr ready`.
- Comments from bots are fed back like anyone's; there is no filter by author yet.
- Remote mode: a server would need its own git remote and `gh` login per repository, and an operator's decision to let clients push through it. Not built.
- Tested only against a fake `gh` and a bare local remote ([validation](validation.md)), never against GitHub itself.
