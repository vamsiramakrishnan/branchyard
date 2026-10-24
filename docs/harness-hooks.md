# Harness hooks

<!-- harness sections below; one ## per harness -->

## Hermes

What you get: before each turn, Hermes's `pre_llm_call` shell hook runs `by hook --harness hermes user-prompt-submit`, and what `by` prints (`{"context": "..."}`) is appended to that turn's user message. A session's first turn gets the session-start summary (branches asking, blocked or with a plan to approve, and how many run or are ready); later turns get only what changed. There is no status line and no commands.

There is deliberately no `on_session_start` hook. Hermes ignores that event's output, and running `by` there would record the first summary as shown before any turn saw it.

Install:

- `by init plugin hermes` merges `plugins/branchyard/harnesses/hermes/hooks.yaml` into `~/.hermes/config.yaml` under `hooks:`.
- By hand: copy that `hooks:` block into `~/.hermes/config.yaml` (or a profile's own `config.yaml`).

Caveats:

- Hermes asks once before it runs a new shell hook, keyed on the exact `(event, command)` pair. A foreground `hermes chat` asks on its terminal; the gateway, cron and `hermes acp` never ask, so approve it from a terminal first, start them with `HERMES_ACCEPT_HOOKS=1`, or add the pair to `~/.hermes/shell-hooks-allowlist.json` yourself. `by init plugin` does not approve it for you.
- Hermes splits the command with `shlex` and runs it without a shell, so the guard runs inside `sh -c '...'`. Windows without a POSIX `sh` is not covered.
- No Hermes Python plugin is shipped: it would need its own consent (`plugins.enabled`) and do the same thing as the one shell line.

Try it: in a repository with a Branchyard branch waiting on a question, run `hermes hooks list` (the hook shows as approved), then `hermes hooks test pre_llm_call`; the parsed response carries a `context` naming that branch. A new `hermes chat` session hears about it on its first turn.

Not done: tested against recorded payloads written from Hermes's documentation; not run against the real CLI.

## Cursor

What you get: when a Cursor agent session starts, the `sessionStart` hook runs `by hook --harness cursor session-start`, and its `additional_context` is added to the conversation's initial context. The Cursor plugin also brings the `setup` and `delegate` skills and the `/status`, `/wait` and `/setup` commands. The Cursor CLI can show `by statusline --harness cursor` as its status line.

Cursor hears from Branchyard only when a session starts. `beforeSubmitPrompt` can allow or block a prompt but has no field for added context, so there is no per-prompt hook.

Install:

- Plugin: in Cursor open Customize, add this repository as a marketplace (`.cursor-plugin/marketplace.json` names `plugins/branchyard`) and install `branchyard`. Its manifest, `plugins/branchyard/.cursor-plugin/plugin.json`, points `hooks` at `harnesses/cursor/hooks.json`, not at the Claude Code `hooks/hooks.json`.
- `by init plugin cursor` merges the same hook into `~/.cursor/hooks.json` and the status line into `~/.cursor/cli-config.json`.

Caveats:

- Use the plugin or the user hooks, not both, or each session hears the news twice.
- Cursor's hook input has no `cwd`; `by` reads the repository from `workspace_roots`. Plugin hooks run from the plugin's directory and user hooks from `~/.cursor/`, so the working directory says nothing about the repository.
- `sessionStart` is fire-and-forget: Cursor does not wait for it, so a slow `by` can miss the first turn.
- The Cursor CLI's `statusLine` replaces its built-in footer rather than adding a row. Remove it from `~/.cursor/cli-config.json` to keep the footer.
- The commands carry only a `description` in their front matter; Cursor's plugin template also asks for `name`.

Try it: in a repository with a Branchyard branch waiting on a question, start a new agent chat; ask "what did the session-start hook tell you?" and it names that branch. In the CLI (`cursor-agent`), the status line shows the branch counts.

Not done: tested against recorded payloads written from Cursor's documentation; not run against the real CLI or editor.

## Antigravity

What you get: a `SessionStart` hook (`by hook --harness antigravity session-start`) and a `PreInvocation` hook (`by hook --harness antigravity user-prompt-submit`), each answering with `injectSteps` that add Branchyard's lines to the conversation. The Antigravity CLI shows `by statusline --harness antigravity` below its own status line (`stack_with_default`).

Install:

- Plugin (CLI): `agy plugin install <clone>/plugins/branchyard/harnesses/antigravity`, from a local clone of this repository; the directory holds `plugin.json` and `hooks.json` at its root, as Antigravity requires. For the App or IDE, copy that directory to `.agents/plugins/branchyard` or `~/.gemini/config/plugins/branchyard`.
- `by init plugin antigravity` merges the hooks into `~/.gemini/config/hooks.json` (under a `branchyard` key) and the status line into `~/.gemini/antigravity-cli/settings.json`.

Caveats:

- Use the plugin or the global hooks, not both.
- The plugin holds hooks only; the skills and commands are not packaged for Antigravity.
- Antigravity's hook input has no `cwd`; `by` reads the repository from `workspacePaths` and the session from `conversationId`.
- `PreInvocation` runs before each invocation of the agent loop, which can be more than once per prompt; the session cursor keeps repeats silent.

Try it: in a repository with a Branchyard branch waiting on a question, start `agy` and ask "what did the hooks tell you about Branchyard?"; the answer names that branch, and the status line under Antigravity's own shows the counts.

Not done: tested against recorded payloads written from Antigravity's documentation; not run against the real CLI, App or IDE.

## Kiro

What you get: one hook file with a `SessionStart` hook (`by hook --harness kiro session-start`) and a `UserPromptSubmit` hook (`by hook --harness kiro user-prompt-submit`). Kiro adds a hook's standard output to the agent's context when it exits 0, and `by` always exits 0. There is no status line and no commands.

Install:

- `by init plugin kiro` writes `plugins/branchyard/harnesses/kiro/branchyard.json` to `~/.kiro/hooks/branchyard.json`, which Kiro CLI V3 reads for every workspace.
- For the Kiro IDE, copy the same file to `.kiro/hooks/branchyard.json` in the repository.

Caveats:

- The prompt trigger is spelt `UserPromptSubmit`, as in Kiro's CLI V3 hook files; the IDE's trigger list calls it Prompt Submit. If the IDE refuses the file, that name is the first suspect.
- Kiro CLI 2.x read hooks from agent configuration (`agentSpawn`, `userPromptSubmit`), not from these files; run `/upgrade-agent` or move to V3.

Try it: in a repository with a Branchyard branch waiting on a question, start `kiro-cli chat` and ask "what did the session-start hook say?"; the answer names that branch.

Not done: tested against recorded payloads written from Kiro's documentation; not run against the real CLI or IDE.
