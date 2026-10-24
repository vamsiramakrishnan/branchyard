# Harness hooks

<!-- harness sections below; one ## per harness -->

## Pi

What you get: an extension, `plugins/branchyard/harnesses/pi/branchyard.ts`, that tells a Pi session a person started which of the repository's Branchyard branches finished, failed, are blocked or ask. On a session's first prompt its `before_agent_start` handler runs `by hook --harness generic session-start` (stdin `{"cwd", "session_id"}`, the session id from `ctx.sessionManager`) and returns the reply's `context` as a custom message (`customType: "branchyard"`, shown in the transcript); on later prompts it runs `user-prompt-submit`, which says something only when there is news. On `session_start` and `agent_end` it runs `by statusline --harness generic` and puts the line in the footer with `ctx.ui.setStatus("branchyard", ...)`. It spawns `by` with node's `child_process` and nothing else; a missing `by`, a non-zero exit, output that is not JSON, or no answer within 3 seconds adds nothing and never throws. The file is plain JavaScript with a `.ts` name, so Pi loads it as it loads any extension.

Install: `by init plugin` copies it to `~/.pi/agent/extensions/branchyard.ts` (`plugins/branchyard/install/pi.toml`). Or, from a clone, `pi install ./plugins/branchyard/harnesses/pi` (its `package.json` names the extension under `pi.extensions`). Run `/reload` or start a new session afterwards.

Caveats: `pi install git:...` of the whole repository does not find the extension, because the repository root has no `package.json`; use the local path. Whether a session has had its session-start context is remembered in the extension's memory, so a resumed session after a restart is told the counts again (the cursor in `.branchyard/hooks/<session>.json` keeps that to counts, not old news).

Try it: in a repository Branchyard uses, with a branch that has finished, start `pi` and send any prompt; the transcript shows a `branchyard` message naming the branch, and the footer shows `by: ...`. With `by` off PATH nothing appears.

Tested against a fake host API under node 22 with a stub `by` (`tests/test_plugin_ts.py`); not run against the real CLI. Not done: no Pi command (`/by-status`) or skill registration; the extension does not use `pi.sendMessage` for news that arrives while the agent is idle.

## Oh My Pi

What you get: the same extension as Pi (`plugins/branchyard/harnesses/pi/branchyard.ts`), since Oh My Pi keeps Pi's extension API: `before_agent_start` returning `{ message }`, `session_start`, `agent_end` and `ctx.ui.setStatus`. Oh My Pi keeps the first message a `before_agent_start` handler returns, so another extension that returns one first wins. Bun loads the file as it is.

Install: `by init plugin` copies it to `~/.omp/agent/extensions/branchyard.ts` (`plugins/branchyard/install/oh-my-pi.toml`). Or, from a clone, `omp plugin link ./plugins/branchyard/harnesses/pi`; its `package.json` names the extension under `omp.extensions` (and `pi.extensions`, which Oh My Pi also reads). Restart Oh My Pi or start a new session afterwards.

Caveats: as for Pi. Oh My Pi may call `before_agent_start` again for the same submission when it retries policy preparation; the second call asks `by` for `user-prompt-submit`, which has nothing new, so the context is not repeated, but it is also not re-sent if the first attempt was discarded.

Try it: as for Pi, with `omp`.

Tested against a fake host API under node 22 with a stub `by`, the same run as Pi; not run against the real CLI. Not done: no `omp plugin install name@marketplace` entry, since the repository has no Oh My Pi marketplace file.

## Amp

What you get: a plugin, `plugins/branchyard/harnesses/amp/branchyard.ts`, following Amp's plugin API (`export default function (amp)`). Its `agent.start` handler runs `by hook --harness generic session-start` on a thread's first prompt and `user-prompt-submit` after, with the thread id as `session_id` and the workspace root (`amp.system.workspaceRoot`, else the process's directory) as `cwd`, and returns `{ message: { content, display: true } }`, which Amp appends after the user's text. On `session.start` and `agent.end` it runs `by statusline --harness generic` and shows the line in a status item (`amp.createStatusItem()`). The same 3-second timeout and silence on any failure as the Pi extension; Node built-ins only, which Bun provides.

Install: `by init plugin` copies it to `~/.config/amp/plugins/branchyard.ts` (`plugins/branchyard/install/amp.toml`), Amp's system plugin directory; a project can instead keep it in `.amp/plugins/`. Run "plugins: reload" from the command palette, or restart Amp.

Caveats: the plugin API page names only the two directories, so there is no `amp plugins add` step here. The workspace root is fixed for the plugin process; Amp reloads plugins when it changes. Threads that have had their session-start context are remembered in the plugin's memory, as for Pi.

Try it: in a repository Branchyard uses, with a finished branch, start a thread in Amp and send a prompt; the reply's user message carries the Branchyard lines and the status item shows `by: ...`.

Tested against a fake host API under node 22 with a stub `by` (`tests/test_plugin_ts.py`); not run against the real CLI. Not done: no Amp command palette entry and no bundled skill.

## OpenCode

What you get: a plugin, `plugins/branchyard/harnesses/opencode/branchyard.js`, for the released `@opencode-ai/plugin` 1.19.x. It default-exports `{ id: "branchyard", server }`; `server` returns a `chat.message` hook that, on the first message of a session, runs `by hook --harness generic session-start` (stdin `{"cwd": <the project directory>, "session_id": <sessionID>}`) and, on later messages, `user-prompt-submit`, which says something only when there is news. A reply's `context` becomes a synthetic text part (`synthetic: true`, an OpenCode-shaped `prt_` id) pushed onto the message's parts, so it reaches the model with the user's text. The same 3-second timeout and silence on any failure as the Pi extension.

Install: `by init plugin` copies it to `~/.config/opencode/plugins/branchyard.js` (`plugins/branchyard/install/opencode.toml`); a project can instead keep it in `.opencode/plugins/`. Or list the directory in `opencode.json`: `{"plugin": ["/path/to/branchyard/plugins/branchyard/harnesses/opencode"]}` (its `package.json` names the entry under `main` and `exports["./server"]`). Restart OpenCode afterwards.

Caveats: OpenCode offers plugins no status line (TUI slots only), so there is none. The 2.x plugin API on OpenCode's development branch differs and is not targeted. Sessions that have had their session-start context are remembered in the plugin's memory, as for Pi.

Try it: in a repository Branchyard uses, with a finished branch, run `opencode` and send a message; the model's first reply can name the branch, and the session's stored message holds a synthetic part with the Branchyard lines.

Tested against a fake host API under node 22 with a stub `by` (`tests/test_plugin_ts.py`); not run against the real CLI. Not done: no npm package is published, so `"plugin": ["branchyard-opencode"]` does not resolve.
