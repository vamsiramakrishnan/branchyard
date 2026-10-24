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
| `gemini-cli` | `session_id` | `cwd` | nothing (see below) | `{"hookSpecificOutput":{"hookEventName":"BeforeAgent","additionalContext":T}}` | `cwd` (none) |
| `qwen-code` | `session_id` | `cwd` | as `claude-code` | as `claude-code` | `workspace.current_dir`, `cwd` |
| `droid` | `session_id` | `cwd` | as `claude-code` | as `claude-code` | `cwd`, `workspace.current_dir` |
| `deepseek-harness` | `session_id` | `cwd` | as `claude-code` | as `claude-code` | `cwd` (none) |
| `github-copilot` | `sessionId`, `session_id` | `cwd` | `{"additionalContext":T}` | nothing (see below) | `workspace.current_dir`, `cwd` |
| `cursor` | `session_id`, `conversation_id` | `workspace_roots[0]`, `cwd` | `{"additional_context":T}` | nothing (see below) | `cwd`, `workspace.current_dir`, `workspace.project_dir` |
| `antigravity` | `conversationId` | `workspacePaths[0]`, `cwd` | `{"injectSteps":[{"ephemeralMessage":T}]}` | the same | `cwd`, `workspacePaths[0]`, `workspace.current_dir` |
| `kiro` | `session_id` | `cwd` | `T` as plain text | `T` as plain text | `cwd` (none) |
| `hermes` | `session_id` | `cwd` | nothing (see below) | `{"context":T}` | `cwd` (none) |
| `generic` | `session_id`, `sessionId` | `cwd` | `{"context":T}` | `{"context":T}` | `cwd` |

`T` is the lines joined by newlines; every reply ends with a newline. Where an event's output is dropped by the harness, `by` prints nothing there and leaves the session's cursor alone, so the news waits for an event the harness hears instead of being marked as told: GitHub Copilot drops a config-file `userPromptSubmitted` hook's output, Cursor's `beforeSubmitPrompt` takes only `continue` and `user_message`, Hermes's `on_session_start` only observes, and headless Gemini CLI puts a `SessionStart` context in front of the prompt, where it stops `gemini -p "/branchyard:status"` being a command. Hermes and Gemini CLI therefore hear everything from `pre_llm_call` and `BeforeAgent`, whose first call in a session says what a session start would. Antigravity's documented `systemMessage` shapes disagree with each other, so both events use an ephemeral message.

## `generic`

For plugins written in code (Pi, Oh My Pi, Amp, OpenCode) and anything else that can run a command: write `{"cwd": "...", "session_id": "..."}` to `by hook --harness generic <event>` and use the `context` of what comes back, `{"context": "..."}`, or nothing when there is nothing to say. `by statusline --harness generic` reads `{"cwd": "..."}` and prints one line or nothing. A plugin should treat a missing `by`, a non-zero exit or three seconds without an answer as nothing to say.

Tested through the built `by` (`crates/branchyard-cli/tests/harness_hooks.rs`) against one payload per harness and event, with the exact expected stdout beside it in `crates/branchyard-cli/tests/fixtures/hooks/<id>/`: each event as a session's first call, then nothing the second time; the dropped events print nothing and write no cursor; the status line from each harness's field and, with none, from the process's directory; no flag is byte-for-byte `--harness claude-code`; an unknown harness prints nothing and one stderr line. The payloads are written from each harness's hook documentation, not recorded from a run. Not done: no harness has been run against the real CLI; the Copilot, Cursor, Droid and Antigravity status-line JSON is taken from third-party copies of their docs or assumed to be Claude Code's; Kiro's stdout is documented as context only for `agentSpawn`, not for a prompt submit; Goose, which documents no way to add context, has no dialect.

<!-- harness sections below; one ## per harness -->

## Codex

**What you get.** The plugin's skills (`setup`, `delegate`) and two hooks, `SessionStart` and `UserPromptSubmit`, each `command -v by >/dev/null 2>&1 && by hook --harness codex <event> || true` ([`harnesses/codex/hooks.json`](../plugins/branchyard/harnesses/codex/hooks.json)). Codex passes `session_id` and `cwd` on stdin and adds what the hook prints to the session as developer context. Codex has no status line, so there is none.

**Install.** From a clone or straight from GitHub, `codex plugin marketplace add vamsiramakrishnan/branchyard` (or the clone's path), then `codex plugin add branchyard@branchyard`. Codex reads [`.agents/plugins/marketplace.json`](../.agents/plugins/marketplace.json) (its own format: a `local` source at `./plugins/branchyard`, with a policy and a category); it also reads `.claude-plugin/marketplace.json`, the Claude Code one, as a fallback. The plugin's [`.codex-plugin/plugin.json`](../plugins/branchyard/.codex-plugin/plugin.json) names `./harnesses/codex/hooks.json` as its hooks, which replaces Codex's default `hooks/hooks.json` (Claude Code's file, which would run without `--harness codex`). Without the plugin, `by init plugin` merges the same two hooks into `~/.codex/hooks.json` ([`install/codex.toml`](../plugins/branchyard/install/codex.toml)); use one or the other, or the hooks run twice.

**Caveats.** Codex runs no plugin or user hook until you review and trust it. The interactive `codex` asks at start ("Hooks need review": choose "Trust all and continue"), or run `/hooks` and press `t`. Trust is saved in `~/.codex/config.toml` as one `[hooks.state."KEY"]` table per hook with a `trusted_hash` (KEY is `~/.codex/hooks.json:session_start:0:0` for `by init plugin`'s file, `branchyard@branchyard:harnesses/codex/hooks.json:session_start:0:0` for the plugin, which also records a `trusted_content_hash` of the plugin's files), so a changed hook or plugin asks again; `by init plugin` writes no trust for you. `codex exec` never asks: it skips an untrusted hook without a word. For an unattended run, `codex exec --dangerously-bypass-hook-trust` runs the enabled hooks for that one invocation (the `-c bypass_hook_trust=true` override is ignored). OpenAI's plugin documentation says lifecycle hooks work only for plugins installed by hand, and make a plugin ineligible for the public directory. The manifest keeps the `hooks` field at the top level of `.codex-plugin/plugin.json`, which the documentation calls the legacy place; the newer place is `extensions.com.openai.hooks` in a root `plugin.json`, which this plugin does not have.

**Try it.** In a repository Branchyard uses, start a branch (`by run --detach "say hello"`), wait until `by ls` shows it ready, then start Codex there and send any prompt: the first reply should know the branch finished. `echo '{"session_id":"s","cwd":"'$PWD'"}' | by hook --harness codex session-start` prints what Codex would get.

Run against Codex CLI 0.164.0 on 24 October 2026, in a private `HOME`, with a repository holding one ready branch, through `codex exec`. Both installs were tried, each alone: `by init plugin codex --apply` (`~/.codex/hooks.json`), and `codex plugin marketplace add <clone>` then `codex plugin add branchyard@branchyard`. Codex honours the manifest's `hooks` field: the trust key names `harnesses/codex/hooks.json`, and the hooks ran `by hook --harness codex`, once each, not the default `hooks/hooks.json`. Untrusted, `codex exec` ran no hook; with `--dangerously-bypass-hook-trust`, or after "Trust all and continue" in the interactive `codex` (driven in tmux), it ran both. `SessionStart` got `source: "startup"`, and the reply it printed reached the model as a developer message; asked to repeat a note starting with `Branchyard:`, the model answered `Branchyard: 1 ready to integrate (by ls).` A second prompt in the same session (`codex exec resume ID`) ran `SessionStart` with `source: "resume"` and `UserPromptSubmit`, and both printed nothing. The payloads in [`fixtures/hooks/codex/`](../crates/branchyard-cli/tests/fixtures/hooks/codex) are the ones recorded, paths scrubbed: they carry `transcript_path` as a string and a `permission_mode`, which the documentation-written ones lacked. The packaged files are also checked by `tests/test_plugin_claude_compat.py` (every file parses; with no `by` on `PATH` each command prints nothing and exits 0; against a stub `by` each passes `hook --harness codex` and the canonical event). Not done: the model was GLM-4.6 behind a local Responses-to-Chat-Completions shim, since the OpenAI key had no credit left, so no OpenAI model saw the note; `codex plugin marketplace add` straight from GitHub was not tried, only from a local clone; no MCP server or `mcp.json`.

## Gemini CLI

**What you get.** A Gemini CLI extension at [`plugins/branchyard/harnesses/gemini-cli/`](../plugins/branchyard/harnesses/gemini-cli/): `gemini-extension.json`, the commands `/setup`, `/status` and `/wait` (`commands/*.toml`, the same prompts as the plugin's `commands/*.md`, with `{{args}}` for `$ARGUMENTS`), and `hooks/hooks.json` with one hook, `BeforeAgent` (Gemini CLI's event before each prompt), calling `by hook --harness gemini-cli user-prompt-submit`. Gemini CLI appends the reply's `hookSpecificOutput.additionalContext` to the prompt as `<hook_context>…</hook_context>`; a session's first prompt hears what a session start would. There is no `SessionStart` hook: in headless mode (`gemini -p`) Gemini CLI puts a session-start context in front of the prompt text, so `gemini -p "/branchyard:status"` stopped being a command and reached the model as plain text, while `BeforeAgent` adds its context after the command is expanded. The `gemini-cli` dialect prints nothing for `session-start`, so a `SessionStart` hook left by an older install is harmless. Its `timeout` is in milliseconds (5000; `/hooks panel` labels it `5000s`). No status line: Gemini CLI has none.

**Install.** Gemini CLI installs an extension from a repository's root or a local directory, and this one is a subdirectory, so install it from a clone: `gemini extensions install <clone>/plugins/branchyard/harnesses/gemini-cli` (it copies the directory; `gemini extensions update branchyard` after pulling), or `gemini extensions link` the same path to follow the clone. It asks you to trust the extension's folder and the current workspace, then for consent because the extension has hooks; `--consent` skips only the last. Or `by init plugin`, which merges the hook into `~/.gemini/settings.json` and writes the commands to `~/.gemini/commands/branchyard/`, where they are `/branchyard:setup`, `/branchyard:status` and `/branchyard:wait` ([`install/gemini-cli.toml`](../plugins/branchyard/install/gemini-cli.toml)). Use one or the other. Either way the hook runs with nothing to enable, but only in a trusted folder: in an untrusted one `gemini -p` refuses to start (`--skip-trust` or `GEMINI_CLI_TRUST_WORKSPACE=true` for automation), and interactive Gemini CLI asks first.

**Caveats.** The extension carries no skills (Gemini CLI would read a `skills/` directory, but the package has no symbolic links and does not copy the plugin's), so `/setup` drives `by init` itself. Google's documentation says Gemini CLI was replaced by Antigravity CLI for unpaid and Google One users in June 2026.

**Try it.** With a finished branch in the repository, start `gemini` there and send a prompt: the reply should know the branch is ready. `/hooks panel` lists `branchyard-before-agent`; `/status` (extension) or `/branchyard:status` (`by init plugin`) runs the status command.

Run against Gemini CLI 0.65.0 on 24 October 2026 (`gemini-flash-lite-latest`, a private `HOME`, a repository with finished branches), installed both by `by init plugin gemini-cli --apply` and by `gemini extensions install` and `link` of the clone's directory. Headless and in tmux, the first prompt's reply quoted `Branchyard: 2 ready to integrate (by ls).`; a resumed session (`-r latest`) heard nothing the second time, and after another branch finished, `Branchyard: branches changed (by ls; by show NAME; by inbox --as PARENT): - fourth: finished, ready to integrate (1 file changed)`. `gemini -p "/branchyard:status"` and `/status` (extension) ran the command's prompt. The fixtures in `crates/branchyard-cli/tests/fixtures/hooks/gemini-cli/` are recorded payloads. Not done: `/setup` and `/wait` not run; no context file (`GEMINI.md`); no skills in the extension.

## Qwen Code

**What you get.** Claude-style hooks, `SessionStart` and `UserPromptSubmit`, calling `by hook --harness qwen-code`, and the status line, `by statusline --harness qwen-code`, under `ui.statusLine` (Qwen Code ignores a top-level `statusLine`), both in one settings fragment, [`harnesses/qwen-code/settings.json`](../plugins/branchyard/harnesses/qwen-code/settings.json). Qwen Code passes `session_id` and `cwd` to a hook (with `permission_mode`, `model` or `prompt_id`), and to the status line `session_id`, `model.display_name`, `context_window`, `workspace.current_dir`, `git.branch` and `metrics`, but no `cwd`. Unlike Gemini CLI, it keeps a session-start context out of the prompt, so headless commands still work. The commands come from the Gemini CLI extension, which Qwen Code converts when it installs it (`gemini-extension.json` to `qwen-extension.json`, the TOML commands to Markdown).

**Install.** `by init plugin` merges the fragment into `~/.qwen/settings.json` ([`install/qwen-code.toml`](../plugins/branchyard/install/qwen-code.toml)); the hooks and the status line run with no trust prompt and nothing to enable. For the commands, from a clone: `qwen extensions install <clone>/plugins/branchyard/harnesses/gemini-cli` (`--consent` skips its prompt). They are `/setup`, `/wait` and `/branchyard.status`: Qwen Code's own `/status` (version info) wins, and it renames the extension's.

**Caveats.** Qwen Code runs an extension's `hooks/hooks.json`, but the extension's only hook is `BeforeAgent`, which is not a Qwen Code event, so the extension alone gives Qwen Code no hooks and nothing runs twice beside the settings fragment. An extension installed before this change also has a `SessionStart` hook; in Qwen Code it runs with the `gemini-cli` dialect and prints nothing.

**Try it.** With a finished branch in the repository, start `qwen` there and send a prompt; the status line should read `by: … ready`. `echo '{"workspace":{"current_dir":"'$PWD'"}}' | by statusline --harness qwen-code` prints that line.

Run against Qwen Code 0.26.0 on 24 October 2026 with Z.ai's OpenAI-compatible endpoint (`glm-4.5-air`, a private `HOME`): headless, the first reply was `Branchyard: 1 ready to integrate (by ls).` and `qwen -c` heard nothing new; in tmux the status line read `by: 2 ready`, then `by: 3 ready` once a branch finished mid-session, the next prompt's reply quoted `Branchyard: branches changed (by ls; by show NAME; by inbox --as PARENT): - third: finished, ready to integrate (1 file changed)`, and the prompt after that heard nothing; `qwen -p "/branchyard.status"` ran the extension's command. The fixtures in `crates/branchyard-cli/tests/fixtures/hooks/qwen-code/` are recorded payloads. Not done: `/setup` and `/wait` not run.

## GitHub Copilot CLI

**What you get.** A `sessionStart` hook, `by hook --harness github-copilot session-start`, in Copilot's own hooks format (`{"version": 1, "hooks": {"sessionStart": [{"type": "command", "bash": ..., "timeoutSec": 5}]}}`, [`harnesses/github-copilot/hooks.json`](../plugins/branchyard/harnesses/github-copilot/hooks.json)): Copilot passes `sessionId` and `cwd` and reads `additionalContext` from the reply. A status line, `by statusline --harness github-copilot`, as `statusLine` in `~/.copilot/settings.json`. Through the plugin, the skills and the slash commands too.

**Install.** `copilot plugin marketplace add vamsiramakrishnan/branchyard` (Copilot falls back to `.claude-plugin/marketplace.json`), then install `branchyard`; or `copilot plugin install vamsiramakrishnan/branchyard:plugins/branchyard`. Copilot looks for a plugin's manifest in `.github/plugin/plugin.json` before `.claude-plugin/plugin.json`, so [`plugins/branchyard/.github/plugin/plugin.json`](../plugins/branchyard/.github/plugin/plugin.json) is the one it reads: it names the Copilot hooks file, so Copilot does not fall back to `hooks/hooks.json` (Claude Code's). Without the plugin, `by init plugin` writes the hook to `~/.copilot/hooks/branchyard.json` and merges the status line into `~/.copilot/settings.json` ([`install/github-copilot.toml`](../plugins/branchyard/install/github-copilot.toml)); the plugin does not set the status line, so add that by hand or with `by init plugin`.

**Caveats.** There is no hook before each prompt: Copilot drops what a `userPromptSubmitted` hook from a file prints (only SDK hooks may change a prompt), and such a hook would move the session's cursor past news nobody saw. A Copilot session therefore hears from Branchyard when it starts; mid-session, run `/status`. Copilot's documentation calls the status line's `command` a path to an executable script; if your version will not run a shell command line there, put the guarded command in a script and name it.

**Try it.** With a finished branch in the repository, start `copilot` there and ask what happened; it should know. `echo '{"sessionId":"s","cwd":"'$PWD'","source":"startup"}' | by hook --harness github-copilot session-start` prints what Copilot would get.

Tested against recorded payloads written from Copilot CLI's documentation; not run against the real CLI. Not done: no live check in Copilot CLI; nothing before each prompt; whether Copilot runs the plugin's Markdown commands as written is not checked.

## Droid

**What you get.** Droid installs the Claude Code plugin as it is: it reads `.claude-plugin/marketplace.json` and `.claude-plugin/plugin.json`, expands `${CLAUDE_PLUGIN_ROOT}`, and runs the Claude Code `hooks/hooks.json`, whose commands call `by hook` in the default `claude-code` dialect. Droid's hook payloads and replies are Claude Code's (`session_id`, `cwd`; `hookSpecificOutput.additionalContext` or plain stdout), so that works without changes. What it lacks is the status line, which [`harnesses/droid/settings.json`](../plugins/branchyard/harnesses/droid/settings.json) adds: `{"statusLine": {"command": "… by statusline --harness droid …"}}` for `~/.factory/settings.json`.

**Install.** `droid plugin marketplace add vamsiramakrishnan/branchyard`, then install `branchyard`. Or `by init plugin`, which merges hooks calling `by hook --harness droid` into `~/.factory/hooks.json` (Droid's user hooks file: the event map with no `hooks` wrapper, [`harnesses/droid/hooks.json`](../plugins/branchyard/harnesses/droid/hooks.json)) and the status line into `~/.factory/settings.json` ([`install/droid.toml`](../plugins/branchyard/install/droid.toml)). Install the hooks one way, not both; the status line comes only from `by init plugin` or by hand.

**Caveats.** Droid reads the `hooks` key of `~/.factory/settings.json` only while `~/.factory/hooks.json` does not exist, so once `by init plugin` creates that file, move any hooks you kept in the settings into it. Droid snapshots hooks at start and warns when they change; review them with `/hooks`.

**Try it.** With a finished branch in the repository, start `droid` there and send a prompt; it should know, and the status line should read `by: … ready`.

Tested against recorded payloads written from Factory's documentation; not run against the real CLI. Not done: no live check in Droid; Droid does not document the status line's stdin, which is assumed to be Claude Code's.

## DeepSeek Harness

**What you get.** DeepSeek Harness runs a Claude Code `hooks.json` through its bridge plugin, `@deepseek-ai/dsh-hooks-claude-code`, mounted by a row in a Cordis patch file. [`harnesses/deepseek-harness/hooks.json`](../plugins/branchyard/harnesses/deepseek-harness/hooks.json) has `SessionStart` and `UserPromptSubmit` calling `by hook --harness deepseek-harness`, and [`harnesses/deepseek-harness/cordis.patch.yml`](../plugins/branchyard/harnesses/deepseek-harness/cordis.patch.yml) inserts the row, `id: branchyard-hooks`, whose `configPath` is `!!js dshHomePath('branchyard-hooks.json')`: the hooks file in `DSH_HOME`, wherever that is, never a path relative to the directory `dsh` was started in. A new session hears the summary from `SessionStart` (its source is `startup`, or `resume` for `--session-id`); later prompts hear only what changed, from `UserPromptSubmit`. No status line or commands: the harness has neither for a hook to fill.

**Install.** `by init plugin` writes the hooks to `~/.dsh/branchyard-hooks.json` and merges the insert into `~/.dsh/cordis.patch.yml`, the home patch every profile applies ([`install/deepseek-harness.toml`](../plugins/branchyard/install/deepseek-harness.toml)). Then add the bridge to each profile you run, at the version of your `dsh` (`dsh --version`):

```
dsh plugin --profile headless add @deepseek-ai/dsh-hooks-claude-code@0.2.1-rc.2 @deepseek-ai/dsh-hook-protocol@0.2.1-rc.2
```

Installing the bridge next to `dsh` with npm does not help: a profile resolves plugins from its own directory, `$DSH_HOME/profiles/<name>`. The bridge's npm `latest` tag (0.0.1-rc.5) is older than `dsh` and `dsh plugin add` refuses it as incompatible, so name the version. `dsh` does not ship `@deepseek-ai/dsh-hook-protocol`, the bridge's peer, so it is added too. By hand: copy the two files to `$DSH_HOME`, merging the insert into an existing patch file rather than replacing it, and add the bridge as above.

**Caveats.** A profile without the bridge still starts, printing `dsh: warning: 1 entry did not activate` and `branchyard-hooks ... ERR_MODULE_NOT_FOUND` on stderr, and hears nothing. The bridge reads its configuration once, at start, and takes only JSON `additionalContext` (no plain stdout). In bridge 0.2.1-rc.2 the session waits for `SessionStart` (its `agent/created` listener awaits the hook), so the first request carries the summary even when `by` is slow; the bridge's README still calls it detached. The `SessionStart` context is injected as a message of its own and then shows at the start of `UserPromptSubmit`'s `prompt`. With `DSH_HOME` set elsewhere than `~/.dsh`, move both files there.

**Try it.** `dsh --profile headless --dump-config | grep -A4 branchyard-hooks` shows the row; with a finished branch in the repository, `dsh --profile headless "If your context contains a note starting with 'Branchyard:', repeat it verbatim; otherwise reply NONE."` there prints the note.

Verified 24 October 2026 against DeepSeek Harness 0.2.1-rc.2 (`dsh --profile headless`, one-shot and `--session-id` resume, with GLM-4.6 through Z.ai's OpenAI-compatible coding endpoint as a custom `llm-pi-ai` provider) and dsh-hooks-claude-code 0.2.1-rc.2: a new session's reply quoted `Branchyard: 2 ready to integrate (by ls).`, delivered by `SessionStart` (the same turn's `UserPromptSubmit` printed nothing); a resumed session with nothing new answered `QUIET` (`SessionStart` with `source: resume` and `UserPromptSubmit` both silent); after another branch finished, the resumed session quoted `Branchyard: branches changed ... - hello3: finished, ready to integrate (1 file changed)`; a `SessionStart` delayed by three seconds still reached the first request; the row worked from `~/.dsh` and with `DSH_HOME` pointing elsewhere. The fixtures in `crates/branchyard-cli/tests/fixtures/hooks/deepseek-harness/` are those recorded payloads. Not done: the TUI (`dsh --profile tui`) drew nothing under tmux, so only the headless profile ran a model; the web profile was not started; `by init plugin` does not run `dsh plugin add` and writes to `~/.dsh` even when `DSH_HOME` is set.

## Pi

What you get: an extension, `plugins/branchyard/harnesses/pi/branchyard.ts`, that tells a Pi session a person started which of the repository's Branchyard branches finished, failed, are blocked or ask. On a session's first prompt its `before_agent_start` handler runs `by hook --harness generic session-start` (stdin `{"cwd", "session_id"}`, the session id from `ctx.sessionManager`) and returns the reply's `context` as a custom message (`customType: "branchyard"`, shown in the transcript); on later prompts it runs `user-prompt-submit`, which says something only when there is news. On `session_start` and `agent_end` it runs `by statusline --harness generic` and puts the line in the footer with `ctx.ui.setStatus("branchyard", ...)`; in print mode (`pi -p`), where `ctx.hasUI` is false and there is no footer, it skips that. It spawns `by` with node's `child_process` and nothing else; a missing `by`, a non-zero exit, output that is not JSON, or no answer within 3 seconds adds nothing and never throws. The file is plain JavaScript with a `.ts` name, so Pi loads it as it loads any extension.

Install: `by init plugin` copies it to `~/.pi/agent/extensions/branchyard.ts` (`plugins/branchyard/install/pi.toml`). Or, from a clone, `pi install ./plugins/branchyard/harnesses/pi` (its `package.json` names the extension under `pi.extensions`). Run `/reload` or start a new session afterwards.

Caveats: `pi install git:...` of the whole repository does not find the extension, because the repository root has no `package.json`; use the local path. Whether a session has had its session-start context is remembered in the extension's memory, so a session resumed by a new process (`pi -c`) asks for `session-start` again; the cursor in `.branchyard/hooks/<session>.json` already exists then, so nothing is repeated.

Try it: in a repository Branchyard uses, with a branch that has finished, start `pi` and send any prompt; the transcript shows a `branchyard` message naming the branch, and the footer shows `by: ...`. With `by` off PATH nothing appears.

Run against the real CLI, Pi 0.73.1 (`@mariozechner/pi-coding-agent`), Z.ai `glm-4.5-air`, 24 October 2026: installed both by `by init plugin pi --apply` and by `pi install <clone>/plugins/branchyard/harnesses/pi`; `pi -p` sent `{"cwd", "session_id": <session uuid>}` and the model repeated "Branchyard: 2 ready to integrate (by ls)." (stored in the session as a `custom_message` with `customType: "branchyard"`); `pi -p -c` was quiet; in `--mode rpc` a second prompt after another branch finished got only that branch, and `setStatus` requests carried `by: N ready`. The interactive TUI footer was not looked at. Also tested against a fake host API shaped from the recorded `ctx` and events (`tests/test_plugin_ts.py`). Not done: no Pi command (`/by-status`) or skill registration; the extension does not use `pi.sendMessage` for news that arrives while the agent is idle.

## Oh My Pi

What you get: the same extension as Pi (`plugins/branchyard/harnesses/pi/branchyard.ts`), since Oh My Pi keeps Pi's extension API: `before_agent_start` returning `{ message }`, `session_start`, `agent_end` and `ctx.ui.setStatus`. Oh My Pi keeps the first message a `before_agent_start` handler returns, so another extension that returns one first wins. Bun loads the file as it is.

Install: `by init plugin` copies it to `~/.omp/agent/extensions/branchyard.ts` (`plugins/branchyard/install/oh-my-pi.toml`). Or, from a clone, `omp plugin link ./plugins/branchyard/harnesses/pi`; its `package.json` names the extension under `omp.extensions` (and `pi.extensions`, which Oh My Pi also reads). Restart Oh My Pi or start a new session afterwards.

Caveats: as for Pi. Oh My Pi may call `before_agent_start` again for the same submission when it retries policy preparation; the second call asks `by` for `user-prompt-submit`, which has nothing new, so the context is not repeated, but it is also not re-sent if the first attempt was discarded.

Try it: as for Pi, with `omp`.

Run against the real CLI, Oh My Pi 18.12.6 (`@oh-my-pi/pi-coding-agent`, under Bun), Z.ai `glm-4.5-air`, 24 October 2026: installed both by `by init plugin oh-my-pi --apply` and by `omp plugin link <clone>/plugins/branchyard/harnesses/pi`; `omp -p` got "Branchyard: 3 ready to integrate (by ls)." back from the model, `omp -p -c` was quiet, and in `--mode rpc` the second prompt got only the newly finished branch and `setStatus` carried `by: N ready`. Its `ctx` exposes `cwd`, `hasUI` and `sessionManager` as getters and its `systemPrompt` is an array; the extension reads neither shape-sensitive field. The interactive TUI footer was not looked at. Not done: no `omp plugin install name@marketplace` entry, since the repository has no Oh My Pi marketplace file.

## Amp

What you get: a plugin, `plugins/branchyard/harnesses/amp/branchyard.ts`, following Amp's plugin API (`export default function (amp)`). Its `agent.start` handler runs `by hook --harness generic session-start` on a thread's first prompt and `user-prompt-submit` after, with the thread id as `session_id` and the workspace root (`amp.system.workspaceRoot`, else the process's directory) as `cwd`, and returns `{ message: { content, display: true } }`, which Amp appends after the user's text. On `session.start` and `agent.end` it runs `by statusline --harness generic` and shows the line in a status item (`amp.createStatusItem()`). The same 3-second timeout and silence on any failure as the Pi extension; Node built-ins only, which Bun provides.

Install: `by init plugin` copies it to `~/.config/amp/plugins/branchyard.ts` (`plugins/branchyard/install/amp.toml`), Amp's system plugin directory; a project can instead keep it in `.amp/plugins/`. Run "plugins: reload" from the command palette, or restart Amp.

Caveats: the plugin API page names only the two directories, so there is no `amp plugins add` step here. The workspace root is fixed for the plugin process; Amp reloads plugins when it changes. Threads that have had their session-start context are remembered in the plugin's memory, as for Pi.

Try it: in a repository Branchyard uses, with a finished branch, start a thread in Amp and send a prompt; the reply's user message carries the Branchyard lines and the status item shows `by: ...`.

Tested against a fake host API under node 22 with a stub `by` (`tests/test_plugin_ts.py`); not run against the real CLI. Not done: no Amp command palette entry and no bundled skill.

## OpenCode

What you get: a plugin, `plugins/branchyard/harnesses/opencode/branchyard.js`, for the released `@opencode-ai/plugin` 1.19.x. It default-exports `{ id: "branchyard", server }`; `server` returns a `chat.message` hook that, on the first message of a session, runs `by hook --harness generic session-start` (stdin `{"cwd": <the project directory>, "session_id": <sessionID>}`) and, on later messages, `user-prompt-submit`, which says something only when there is news. A reply's `context` becomes a synthetic text part (`synthetic: true`, an OpenCode-shaped `prt_` id) pushed onto the message's parts, so it reaches the model with the user's text. The same 3-second timeout and silence on any failure as the Pi extension.

Install: `by init plugin` copies it to `~/.config/opencode/plugins/branchyard.js` (`plugins/branchyard/install/opencode.toml`); a project can instead keep it in `.opencode/plugins/`. Or list the directory in `opencode.json`: `{"plugin": ["/path/to/branchyard/plugins/branchyard/harnesses/opencode"]}` (its `package.json` names the entry under `main` and `exports["./server"]`). Restart OpenCode afterwards.

Caveats: OpenCode offers plugins no status line (TUI slots only), so there is none. The 2.x plugin API on OpenCode's development branch differs and is not targeted. Sessions that have had their session-start context are remembered in the plugin's memory, so each `opencode run -c` process asks for `session-start` again; the session's cursor keeps that quiet. `chat.message`'s input has no `messageID`; the plugin takes the id from `output.message.id`.

Try it: in a repository Branchyard uses, with a finished branch, run `opencode` and send a message; the model's first reply can name the branch, and the session's stored message holds a synthetic part with the Branchyard lines.

Run against the real CLI, OpenCode 1.19.4, `zai-coding-plan/glm-5.3-flash`, 24 October 2026: loaded from `~/.config/opencode/plugins/` (`by init plugin opencode --apply`) and as a directory path in `opencode.json` `"plugin"`; `opencode run` replied "Branchyard: 1 ready to integrate (by ls).", `opencode export` showed the synthetic user part, `opencode run -c` was quiet, and after another branch finished the next `-c` prompt got only that branch. With `by` off PATH the model replied NONE. Also tested against a fake host API shaped from the recorded hook input (`tests/test_plugin_ts.py`). Not done: no npm package is published, so `"plugin": ["branchyard-opencode"]` does not resolve.

## Hermes

What you get: before each turn, Hermes's `pre_llm_call` shell hook runs `by hook --harness hermes user-prompt-submit`, and what `by` prints (`{"context": "..."}`) is appended to that turn's user message. A session's first turn gets the session-start summary (branches asking, blocked or with a plan to approve, and how many run or are ready); later turns get only what changed. `pre_llm_call` runs once per turn, not once per model request: a turn whose tool loop makes three requests runs `by` once, and every request of the turn carries the same context. There is no status line and no commands.

There is deliberately no `on_session_start` hook. Hermes ignores that event's output, and running `by` there would record the first summary as shown before any turn saw it.

Install:

- `by init plugin hermes` merges `plugins/branchyard/harnesses/hermes/hooks.yaml` into `~/.hermes/config.yaml` under `hooks:`, beside any hooks already there; running it again changes nothing.
- By hand: copy that `hooks:` block into `~/.hermes/config.yaml` (or a profile's own `config.yaml`).

Caveats:

- Hermes asks once before it runs a new shell hook, keyed on the exact `(event, command)` pair, and keeps the answer in `~/.hermes/shell-hooks-allowlist.json`. A `hermes chat` in a terminal asks (`Allow this hook to run? [y/N]`). Without a terminal it never asks and skips the hook, writing only a warning to `~/.hermes/logs/agent.log`: that covers `hermes chat -q` run from a script as well as the gateway, cron and `hermes acp`. Approve it from a terminal first, or run once with `--accept-hooks` or `HERMES_ACCEPT_HOOKS=1`, which records the approval for later runs. `by init plugin` does not approve it for you.
- Hermes splits the command with `shlex` and runs it without a shell, so the guard runs inside `sh -c '...'`. Windows without a POSIX `sh` is not covered.
- `hermes hooks test pre_llm_call` sends a synthetic payload whose session is `test-session`, so only its first run prints the summary.
- No Hermes Python plugin is shipped: it would need its own consent (`plugins.enabled`) and do the same thing as the one shell line.

Try it: in a repository with a Branchyard branch ready, run `hermes hooks list` (the hook shows as allowlisted once approved), then `hermes hooks test pre_llm_call`; the parsed response carries a `context` naming it. A new `hermes chat` session hears about it on its first turn.

Verified 24 October 2026 against Hermes Agent 0.19.0 (pip, GLM-4.6 through its `zai` provider on Z.ai's coding endpoint): `hermes chat -q` without a terminal skipped the unapproved hook; with `HERMES_ACCEPT_HOOKS=1` the reply was `Branchyard: 1 ready to integrate (by ls).`; the same session resumed with `--resume` and no variable (the allowlist kept the approval) answered `QUIET` across a three-request tool loop with one `by` call; after another branch finished, the next resumed turn quoted `Branchyard: branches changed ... - hello2: finished, ready to integrate (1 file changed)`; in tmux, `hermes chat` asked for consent, `y` recorded it, the first prompt quoted `Branchyard: 2 ready to integrate (by ls).` and the second answered `NONE`. With an `on_session_start` hook added for the recording, `by` printed nothing there and the first `pre_llm_call` still got the summary. The fixtures in `crates/branchyard-cli/tests/fixtures/hooks/hermes/` are the recorded payloads. Not done: the gateway, cron, `hermes acp` and non-default profiles were not run.

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

## Other harnesses (Goose and the rest)

A harness with no hook that can add context to a session (Goose documents plugin hooks but no way for one to inject context), or any harness not listed above, gets Branchyard two other ways.

- **MCP tools, on a branch.** When Branchyard itself runs the harness on a branch, the engine gives it the delegation tools as an MCP server (`by mcp --root ROOT --branch NAME`, projected per driver; [delegation](delegation.md#what-a-delegated-harness-gets), [its tools](delegation.md#mcp-tools)): `inbox`, `children`, `wait` and the rest answer what the hooks would tell. `by mcp` serves a branch, so a session a person started outside Branchyard does not get it.
- **An instruction line, anywhere.** [`plugins/branchyard/harnesses/generic/AGENTS.md`](../plugins/branchyard/harnesses/generic/AGENTS.md) holds one section per harness to copy into the repository's AGENTS.md (or whatever instructions file the harness reads). Its `## Goose and the rest` section asks the agent to run `by hook --harness generic user-prompt-submit` with the working directory before a task and before reporting back, and to act on the `context` it prints. All such sessions share the `default` session cursor, so news one session was shown is not repeated to another.

There is no install manifest for these: `by init plugin` does not edit an AGENTS.md, which is yours. Try it: in a repository Branchyard uses, `printf '{"cwd": "%s"}' "$PWD" | by hook --harness generic user-prompt-submit` prints `{"context": "..."}` the first time and nothing the second.

Not done: the instruction line relies on the model following it, which no test checks beyond the `generic` dialect's own; it has not been run in Goose or any other harness.
