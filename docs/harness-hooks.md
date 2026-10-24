# Harness hooks

A session a person starts in a coding harness, not one Branchyard runs, does not see Branchyard's branches: it learns a child finished only by polling `by ls` or blocking in `by wait`. `by hook` fills that gap through the harness's own hooks, and `by statusline` through its status line, as [harness integration](harness-integration.md#claude-code) describes for Claude Code. This page is the same for every other harness: one command, a dialect per harness, and a section per harness below on how it is installed.

What is said does not change with the harness. At most twelve lines: with `BRANCHYARD_BRANCH` set, that branch's unread messages; otherwise branches that finished, failed, are blocked, wait for a plan's approval or ask since this session's last call, kept in `.branchyard/hooks/<session>.json`. A session's first call says only what needs an answer now and how many branches run or are ready. It only reads, gives up after two seconds, and always exits 0; any problem prints nothing on stdout and at most a note on stderr.

## `--harness`

```
by hook [--harness ID] session-start|user-prompt-submit
by statusline [--harness ID]
```

`<event>` is always one of the two canonical names; each harness's packaging maps its own event names onto them (Gemini CLI's `BeforeAgent`, Cursor's `sessionStart`, Hermes's `pre_llm_call`, and so on). `ID` picks the dialect: where the harness's stdin JSON keeps the session id and the working directory, and the shape of the reply. Without `--harness` it is `claude-code`, byte for byte what `by hook` printed before the flag existed. An `ID` not in the table prints nothing, one line on stderr (`by hook: unknown harness "x"; printed nothing (known: ...)`), and exits 0, so a hook written for a newer `by` never breaks a session.

The dialects are one table, `DIALECTS` in [`hook_cmd.rs`](../crates/branchyard-cli/src/hook_cmd.rs). Each path is read in order, and the first non-empty string wins; with none, the session is `default` and the directory is the process's own.

| ID | Session id | Directory | session-start prints | user-prompt-submit prints | Status-line directory |
|---|---|---|---|---|---|
| `claude-code` (default) | `session_id` | `cwd` | `{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":T}}` | the same, `"UserPromptSubmit"` | `workspace.current_dir`, `cwd` |
| `codex` | `session_id` | `cwd` | as `claude-code` | as `claude-code` | `cwd` (Codex has no status line) |
| `gemini-cli` | `session_id` | `cwd` | as `claude-code` | the same, `"hookEventName":"BeforeAgent"` | `cwd` (none) |
| `qwen-code` | `session_id` | `cwd` | as `claude-code` | as `claude-code` | `workspace.current_dir`, `cwd` |
| `droid` | `session_id` | `cwd` | as `claude-code` | as `claude-code` | `cwd`, `workspace.current_dir` |
| `deepseek-harness` | `session_id` | `cwd` | as `claude-code` | as `claude-code` | `cwd` (none) |
| `github-copilot` | `sessionId`, `session_id` | `cwd` | `{"additionalContext":T}` | nothing (see below) | `workspace.current_dir`, `cwd` |
| `cursor` | `session_id`, `conversation_id` | `workspace_roots[0]`, `cwd` | `{"additional_context":T}` | nothing (see below) | `cwd`, `workspace.current_dir`, `workspace.project_dir` |
| `antigravity` | `conversationId` | `workspacePaths[0]`, `cwd` | `{"injectSteps":[{"ephemeralMessage":T}]}` | the same | `cwd`, `workspacePaths[0]`, `workspace.current_dir` |
| `kiro` | `session_id` | `cwd` | `T` as plain text | `T` as plain text | `cwd` (none) |
| `hermes` | `session_id` | `cwd` | nothing (see below) | `{"context":T}` | `cwd` (none) |
| `generic` | `session_id`, `sessionId` | `cwd` | `{"context":T}` | `{"context":T}` | `cwd` |

`T` is the lines joined by newlines; every reply ends with a newline. Where an event's output is dropped by the harness, `by` prints nothing there and leaves the session's cursor alone, so the news waits for an event the harness hears instead of being marked as told: GitHub Copilot drops a config-file `userPromptSubmitted` hook's output, Cursor's `beforeSubmitPrompt` takes only `continue` and `user_message`, and Hermes's `on_session_start` only observes. Hermes therefore hears everything from `pre_llm_call`, whose first call in a session says what a session start would. Antigravity's documented `systemMessage` shapes disagree with each other, so both events use an ephemeral message.

## `generic`

For plugins written in code (Pi, Oh My Pi, Amp, OpenCode) and anything else that can run a command: write `{"cwd": "...", "session_id": "..."}` to `by hook --harness generic <event>` and use the `context` of what comes back, `{"context": "..."}`, or nothing when there is nothing to say. `by statusline --harness generic` reads `{"cwd": "..."}` and prints one line or nothing. A plugin should treat a missing `by`, a non-zero exit or three seconds without an answer as nothing to say.

Tested through the built `by` (`crates/branchyard-cli/tests/harness_hooks.rs`) against one payload per harness and event, with the exact expected stdout beside it in `crates/branchyard-cli/tests/fixtures/hooks/<id>/`: each event as a session's first call, then nothing the second time; the dropped events print nothing and write no cursor; the status line from each harness's field and, with none, from the process's directory; no flag is byte-for-byte `--harness claude-code`; an unknown harness prints nothing and one stderr line. The payloads are written from each harness's hook documentation, not recorded from a run. Not done: no harness has been run against the real CLI; the Copilot, Cursor, Droid and Antigravity status-line JSON is taken from third-party copies of their docs or assumed to be Claude Code's; Kiro's stdout is documented as context only for `agentSpawn`, not for a prompt submit; Goose, which documents no way to add context, has no dialect.

<!-- harness sections below; one ## per harness -->
