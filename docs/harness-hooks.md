# Branchyard in every harness: hooks, commands and the status line

<!-- harness sections below; one ## per harness -->

## Codex

**What you get.** The plugin's skills (`setup`, `delegate`) and two hooks, `SessionStart` and `UserPromptSubmit`, each `command -v by >/dev/null 2>&1 && by hook --harness codex <event> || true` ([`harnesses/codex/hooks.json`](../plugins/branchyard/harnesses/codex/hooks.json)). Codex passes `session_id` and `cwd` on stdin and adds what the hook prints to the session as developer context. Codex has no status line, so there is none.

**Install.** From a clone or straight from GitHub, `codex plugin marketplace add vamsiramakrishnan/branchyard`, then install `branchyard` from that marketplace. Codex reads [`.agents/plugins/marketplace.json`](../.agents/plugins/marketplace.json) (its own format: a `local` source at `./plugins/branchyard`, with a policy and a category); it also reads `.claude-plugin/marketplace.json`, the Claude Code one, as a fallback. The plugin's [`.codex-plugin/plugin.json`](../plugins/branchyard/.codex-plugin/plugin.json) names `./harnesses/codex/hooks.json` as its hooks, which replaces Codex's default `hooks/hooks.json` (Claude Code's file, which would run without `--harness codex`). Without the plugin, `by init plugin` merges the same two hooks into `~/.codex/hooks.json` ([`install/codex.toml`](../plugins/branchyard/install/codex.toml)); use one or the other, or the hooks run twice.

**Caveats.** Codex runs no plugin or user hook until you review and trust it: open Codex and run `/hooks`. OpenAI's plugin documentation says lifecycle hooks work only for plugins installed by hand, and make a plugin ineligible for the public directory. The manifest keeps the `hooks` field at the top level of `.codex-plugin/plugin.json`, which the documentation calls the legacy place; the newer place is `extensions.com.openai.hooks` in a root `plugin.json`, which this plugin does not have.

**Try it.** In a repository Branchyard uses, start a branch (`by run --detach "say hello"`), wait until `by ls` shows it ready, then start Codex there and send any prompt: the first reply should know the branch finished. `echo '{"session_id":"s","cwd":"'$PWD'"}' | by hook --harness codex session-start` prints what Codex would get.

Tested against recorded payloads written from Codex's documentation (`tests/test_plugin_claude_compat.py`: every file parses; with no `by` on `PATH` each command prints nothing and exits 0; against a stub `by` each passes `hook --harness codex` and the canonical event); not run against the real CLI. Not done: no live check in Codex; `codex plugin marketplace add` reading `.agents/plugins/marketplace.json` from a GitHub repository is the documented path, not one tried; no MCP server or `mcp.json`.

## Gemini CLI

**What you get.** A Gemini CLI extension at [`plugins/branchyard/harnesses/gemini-cli/`](../plugins/branchyard/harnesses/gemini-cli/): `gemini-extension.json`, the commands `/setup`, `/status` and `/wait` (`commands/*.toml`, the same prompts as the plugin's `commands/*.md`, with `{{args}}` for `$ARGUMENTS`), and `hooks/hooks.json` with `SessionStart` and `BeforeAgent` (Gemini CLI's event before each prompt), calling `by hook --harness gemini-cli session-start` and `user-prompt-submit`. Gemini CLI reads `hookSpecificOutput.additionalContext` from the reply: at session start it becomes the first turn, before a prompt it is added to that turn. Its `timeout` is in milliseconds (5000). No status line: Gemini CLI has none.

**Install.** Gemini CLI installs an extension from a repository's root or a local directory, and this one is a subdirectory, so install it from a clone: `gemini extensions install <clone>/plugins/branchyard/harnesses/gemini-cli` (it copies the directory; `gemini extensions update branchyard` after pulling). Or `by init plugin`, which merges the hooks into `~/.gemini/settings.json` and writes the commands to `~/.gemini/commands/branchyard/`, where they are `/branchyard:setup`, `/branchyard:status` and `/branchyard:wait` ([`install/gemini-cli.toml`](../plugins/branchyard/install/gemini-cli.toml)). Use one or the other.

**Caveats.** The extension carries no skills (Gemini CLI would read a `skills/` directory, but the package has no symbolic links and does not copy the plugin's), so `/setup` drives `by init` itself. Gemini CLI asks for consent when an extension with hooks is installed; `/hooks panel` lists `branchyard-session-start` and `branchyard-before-agent`. Google's documentation says Gemini CLI was replaced by Antigravity CLI for unpaid and Google One users in June 2026.

**Try it.** With a finished branch in the repository, start `gemini` there: the session's first turn should name it. `/hooks panel` shows both hooks; `/status` runs the status command.

Tested against recorded payloads written from Gemini CLI's documentation and its extension loader's source (the extension's `hooks/hooks.json` must hold a `hooks` object); not run against the real CLI. Not done: no live check in Gemini CLI; no context file (`GEMINI.md`); no skills in the extension.

## Qwen Code

**What you get.** Claude-style hooks, `SessionStart` and `UserPromptSubmit`, calling `by hook --harness qwen-code`, and the status line, `by statusline --harness qwen-code`, under `ui.statusLine` (Qwen Code ignores a top-level `statusLine`), both in one settings fragment, [`harnesses/qwen-code/settings.json`](../plugins/branchyard/harnesses/qwen-code/settings.json). Qwen Code passes `session_id` and `cwd` to a hook and the working directory as `workspace.current_dir` to the status line. The commands come from the Gemini CLI extension, which Qwen Code converts when it installs it (`gemini-extension.json` to `qwen-extension.json`, the TOML commands to Markdown).

**Install.** `by init plugin` merges the fragment into `~/.qwen/settings.json` ([`install/qwen-code.toml`](../plugins/branchyard/install/qwen-code.toml)). For the commands, from a clone: `qwen extensions install <clone>/plugins/branchyard/harnesses/gemini-cli`.

**Caveats.** Qwen Code's extension documentation does not say whether it runs an extension's `hooks/hooks.json`. If it does, the extension's `SessionStart` hook (dialect `gemini-cli`, whose reply Qwen Code also reads) runs beside the settings one; both share the session's cursor in `.branchyard/hooks/`, so the second says only what is new since the first. Its `BeforeAgent` is not a Qwen Code event and does not run.

**Try it.** With a finished branch in the repository, start `qwen` there and send a prompt; the status line should read `by: … ready`. `echo '{"workspace":{"current_dir":"'$PWD'"}}' | by statusline --harness qwen-code` prints that line.

Tested against recorded payloads written from Qwen Code's documentation; not run against the real CLI. Not done: no live check in Qwen Code; whether its extension loader runs extension hooks is unknown.

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

**What you get.** DeepSeek Harness runs a Claude Code `hooks.json` through its bridge plugin, `@deepseek-ai/dsh-hooks-claude-code`, mounted by a row in a Cordis patch file. [`harnesses/deepseek-harness/hooks.json`](../plugins/branchyard/harnesses/deepseek-harness/hooks.json) has `SessionStart` and `UserPromptSubmit` calling `by hook --harness deepseek-harness`, and [`harnesses/deepseek-harness/cordis.patch.yml`](../plugins/branchyard/harnesses/deepseek-harness/cordis.patch.yml) inserts the row, `id: branchyard-hooks`, whose `configPath` is `~/.dsh/branchyard-hooks.json`, written as a `!!js` expression (`os.homedir()`) because the bridge resolves a relative path against the directory `dsh` was started in. No status line or commands: the harness has neither for a hook to fill.

**Install.** `by init plugin` writes the hooks to `~/.dsh/branchyard-hooks.json` and merges the insert into `~/.dsh/cordis.patch.yml`, the home patch every profile applies ([`install/deepseek-harness.toml`](../plugins/branchyard/install/deepseek-harness.toml)). By hand: copy the two files there, merging the insert into an existing patch file rather than replacing it.

**Caveats.** The bridge reads its configuration once, at start, and takes only JSON `additionalContext` (no plain stdout); its `SessionStart` runs detached, so the first request can go before the context arrives. With `DSH_HOME` set elsewhere than `~/.dsh`, move the insert into `$DSH_HOME/cordis.patch.yml`. The bridge package must resolve in the profile; `dsh --profile <name> --dump-config` shows whether the `branchyard-hooks` row loaded.

**Try it.** `dsh --profile web --dump-config | grep -A3 branchyard-hooks` shows the row; with a finished branch in the repository, start a DSH session there and send a prompt: the reply should know.

Tested against recorded payloads written from the bridge's source and README (its supported events, its `hooks.json` parser, its `!!js` patch idiom); not run against the real harness. Not done: no live check in DeepSeek Harness; whether `dsh plugin` needs to add the bridge package before the row resolves is not checked.
