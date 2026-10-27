# Troubleshooting

Find the words you see, read what they mean, run the command beside them. When you do not know where to start, run `by doctor` (it checks the machine and the repository and ends with what to fix) and `by ls`. The words come from `by`'s own output; [concepts](concepts.md) has the table of branch states.

## A branch is in a bad state

### `interrupted`

The turn stopped before it finished: you cancelled it with `by cancel NAME`, or the engine running it stopped (a closed terminal, a killed process, a reboot). The session and the worktree are kept.

- `by log NAME` says which. A cancel logs ``warning: cancelled with `by cancel` `` (another canceller, by its name: `cancelled by alice`); an engine that stopped logs `recovered: its engine (pid N) is no longer running; the prompt had been submitted and the turn's outcome is unknown`.
- `by diff NAME` shows what the turn wrote before it stopped.
- `by send NAME --retry` submits the cut-off prompt again; `by send NAME "…"` sends a new one into the same session; `by rewind NAME --to N` goes back to checkpoint N first (0 is the base).

### `failed`

The turn ended in failure. The reason follows the colon in `by ls` and `by show NAME`: the harness exited or lost its connection (`the turn's outcome is unknown: the harness connection closed before the turn ended`), a plan was rejected, or the harness could not start.

- `by log NAME` has the last events and the harness's own error.
- `by harnesses` says whether the harness is installed, on `PATH` and logged in; `by harnesses login ID` signs in.
- `by send NAME "…"` tries again in the same session; `by rm NAME` throws the branch away.
- Gemini CLI, `could not resume session …: open failed: Authentication required`: Gemini CLI's `session/load` (what `by send` uses) refuses a session until an auth type is chosen, though its `session/new` takes `GEMINI_API_KEY` without one. Branchyard now authenticates once with the `gemini-api-key` method and asks again, which makes Gemini CLI write `security.auth.selectedType: "gemini-api-key"` into `~/.gemini/settings.json`, as its own `/auth` does. If it still fails, the key is not in the environment the harness sees (`by harnesses` shows `key set (GEMINI_API_KEY)` when it is), and the failure now reads `authenticate with gemini-api-key failed: …`.
- Antigravity, `open failed: authentication failed or timed out` within a second: `GEMINI_API_KEY` is set, but `~/.gemini/antigravity-cli/settings.json` does not select the gemini provider, so `agy` wants a Google sign-in. `by harnesses` shows `key set, unused`; `by harnesses login antigravity` shows the one-line change and writes it with `--yes` (and `by run` now refuses before a branch exists).

### `blocked`

A child branch that has not started because a prerequisite failed, was interrupted, hit a limit or was removed. `by ls` gives the reason, such as `its prerequisite lint failed`. `by graph show PARENT` shows the dependencies. Fix the prerequisite (`by send`, as above), or change the dependency with a graph proposal, which reopens the child. `by rm NAME` drops it.

### `awaiting plan approval`

A planning turn proposed a plan, and nothing happens until you answer. `by plan show NAME` prints it. `by plan approve NAME` runs it (`--edit` opens it in your editor first), `by plan reject NAME --reason "…"` fails the branch, and `--replan` asks for another plan instead.

### `over budget`

A limit stopped the branch. `by ls` names it: `max_usd (--budget-usd)`, `max_turns (--max-turns)` or `max_duration (--max-minutes)`. Branchyard counts the cost the harness reports, so a harness that reports none cannot hit `max_usd`. Look at `by diff NAME`, then continue with a higher limit: `by send NAME "…" --max-turns 5`. Sending without raising the limit stops again at once.

### `running`, and nothing seems to happen

- `by watch` shows activity per branch; `by log NAME -f` follows one. A turn that has had no harness activity for its stall window is marked `(stalled)`; the window is off unless you start the branch with `--stall-after`.
- `by send NAME --steer "…"` adds a hint to the running turn without stopping it. `by cancel NAME` stops it.
- A branch whose engine died stays `running` only until the next `by` command that opens the repository: `by ls` recovers it to `interrupted`, with the log line above. Then `by send NAME --retry`.
- A run started with `--detach` writes its output to `.branchyard/detached/NAME-run-*.log`.

## Messages and other errors

### `the check for <NAME> failed`

`by merge` printed `by: the check for NAME failed (exit N); nothing was merged.`, then `check:` with the command, `output:` with the last lines it printed, and `next: by check NAME (runs the check without merging)`. Nothing moved. The check ran on the exact merge result, so it can fail where the branch's own tests passed. A check that overran shows `check timed out after …`.

- `by compare NAME --check` runs the check on the candidate in a private worktree and shows `failed` or `passed`.
- `by try NAME` applies the branch's changes to your checkout (it must be clean) so you can run the check yourself; `by try --off` takes them back out.
- `by send NAME "The check fails with: … Fix it."` gives the harness the output, then `by merge NAME` again.

### `has scripts you have not trusted`

The full message:

```text
by: /path/to/repo's [workspace] in /path/to/repo/branchyard.toml has scripts you have not trusted, and there is no terminal to ask on; nothing was created. Review it with `by workspace show`, then run `by workspace trust` in the repository
```

A repository's `[workspace]` scripts run as you, so they never run before you say so; on a terminal `by run` asks instead. `by workspace show` prints them; `by workspace trust` records their digest. A change to the scripts asks again (`changed since you trusted it`). `by doctor` shows the same state as `config.workspace_trust`.

### `login not verified`

`by harnesses`, `by doctor` and `by up` show `login not verified` in the LOGIN column and a line such as `ID: login not verified: no ~/.claude/.credentials.json and no ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN; run `by harnesses login ID``. Not verified means Branchyard looked for credential files and key variables by name and found none; a login kept somewhere else, such as a keychain, a cloud provider's variables or a key helper, looks the same, and `by run --harness ID` still starts the harness. `logged out` with no qualifier means the harness's own status command said so (Claude Code is asked `claude auth status`), and `by run --harness ID` then refuses before it creates a branch. The other words are `logged in`, `key set (VAR)`, `key stored (VAR)` and `credentials found`.

- If the harness works when you start it yourself, ignore the row. `--auto` routing leaves such a harness out (`ID is likely logged out`); name it with `--harness` instead.
- Otherwise `by harnesses login ID` runs the harness's own sign-in, or stores a key (`printf %s "$KEY" | by harnesses login ID --api-key`). `by up --plan` lists what only you can do.
- Codex with a model provider of its own (a `model_provider` and a `[model_providers.NAME]` with an `env_key` in `~/.codex/config.toml`) and no OpenAI login: `codex login status` says `Not logged in`, but Codex runs on the provider. Export the variable `env_key` names and the row reads `key set (VAR)`; if it is exported and the row still says `logged out`, the line under the table says which provider and variable the configuration selects, and `by run --harness codex --no-preflight` starts it anyway. `by harnesses login codex --api-key` would store an OpenAI key that this provider never reads. Detection reads `config.toml` once a minute (`by harnesses --refresh` asks again).

### `the CLI soft-denied a …`

An Antigravity turn ended `no changes` with `warning: the CLI soft-denied a write_file (WriteFile): no permissions.allow rule in its settings lets it run`. `agy` decides its tools by its own rules and cannot ask in headless mode, and agy 1.4.7 does not allow file writes by default there (its documentation says it does). With `--isolated --secret GEMINI_API_KEY`, Branchyard writes the rules from `--yes` or `--permissions` into the branch's private home (`by log NAME` shows `permission rules: … (allow: …, write_file(…))`). In your own home it writes nothing: add `"permissions": {"allow": ["write_file(/path/to/repository/.branchyard/worktrees)"]}` to `~/.gemini/antigravity-cli/settings.json`, or use `--isolated`.

### `slow to start`

`by harnesses` shows `slow to start` as the version and `ID: version: slow to start (took >10 s); `by harnesses --refresh` asks again, and BRANCHYARD_HARNESS_TIMEOUT (seconds) sets how long to wait`. Every installed harness is asked for its version at once, with ten seconds for all of them, and a Node-based CLI can take longer than that on its first start after an install or a reboot. A harness that answered before keeps that version, with the note. Run the CLI once yourself, then `by harnesses --refresh`. Without a version the qualification note cannot be matched, but a slow start alone does not stop `by run`.

### `never heard`

`by setup` printed `hooks: by hook answers · never heard: start a new Codex session` under a harness, bare `by` a row `hooks       never heard: Codex (start a new session)`, or `by doctor` a `todo` row `hooks.codex  install current · never heard`. The hooks are installed and `by hook` answers when Branchyard calls it, but no session of the harness has called it since: `by hook` notes each call a harness makes ([heard](harness-hooks.md#heard-did-the-harness-run-it)), and there is none. `last heard 2d ago (session-start), before by setup rewrote the hooks` is the same after a reinstall: the call proved the old files, not these.

- Start a new session of the harness (in any repository; a session already open read its hooks when it started) and send a prompt, then run `by doctor` again: the row reads `heard just now (…)` once the harness ran the hook.
- Still never heard: the harness is not running it. The causes are listed under [hooks are silent](#hooks-are-silent-in-my-harness-session) (a trust prompt, a restart, `by` off the harness's `PATH`), and `by init plugin --status ID` lists the one-time steps its install left to you. `by doctor` says so first when `by` is not on `PATH`.
- A test by hand with JSON on stdin is noted like the harness's own call unless `BRANCHYARD_HOOK_CHECK=1` is set, as in the command there; a note from your own test makes `by doctor` read `heard`.
- Pi, Oh My Pi, OpenCode and Amp share one `generic` note: a call from one of them is heard for all four.

### `cannot reach`

```text
by: cannot reach http://host:8421: Connection refused (os error 111)
  next: start a server with `by serve` on that host, or fix the URL, token or CA file ([remote] url, token_file, ca_file)
by: remote mode needs a token
  next: pass --token-file FILE, or set BRANCHYARD_TOKEN_FILE, or set token_file under [remote] in branchyard.toml (`by serve` writes one at .branchyard/server/token on the server's host)
```

The first comes from `--remote` (or `BRANCHYARD_REMOTE`, or `[remote] url`) when nothing answers; the second when no token file is given. `by doctor` shows the first as `server.reachable` with the same next step, and with `--json` the failures are `{"error": {"kind": "remote_unreachable", …}}` and `{"error": {"kind": "usage", …}}`, each with its `next` list. Check the scheme and port, start `by serve` on the host, and pass `--ca-file` for an https server with a private CA. Without `--remote`, commands run here and need no server.

### `this build has no PostgreSQL support` and `this build has no Microsandbox support`

Both are cargo features that are off by default. `by serve --database` needs a build with `postgres`, and `--provider microsandbox` one with `microsandbox` (it also needs Rust 1.94 or newer and `libcap-ng-dev`). Rebuild from the clone: `cargo install --locked --force --path crates/branchyard-cli --features postgres`, or `--features microsandbox`. [Server](server.md#postgresql) and [providers](providers.md) cover each.

### `is not inside a git repository`, beside a `.git`

`by` takes a directory for a repository the way git does: its `.git` is a git directory (one holding a `HEAD` file) or a file naming one (`gitdir: …`, a linked worktree or a submodule). An empty `.git` directory does not count: a harness run without `HOME` once left one in `/tmp`, and before 27 October 2026 `by` then took `/tmp` for a repository where git did not. `git rev-parse --show-toplevel` shows what git sees; remove a stray empty `.git`, or run `git init` if the directory should be one.

### The shell cannot find `by`

`cargo install` puts `by` in `~/.cargo/bin`. Add that directory to `PATH`, then open a new shell. A harness started from an editor or a desktop launcher may have a shorter `PATH` than your shell; see the hooks entry below.

### Claude Code says it has no Bash or Write tool

Behind `ANTHROPIC_BASE_URL` on another provider's model (GLM-4.6 through Z.ai, Claude Code 2.1.310, 25 October 2026), the model can go looking for its tools instead of using them. `by log BRANCH` shows several `▸ ToolSearch` lines and then a reply such as `No Write or Bash tool is available in this session`, though `Bash`, `Edit`, `Read` and `Write` were in the request (the session transcript under `~/.claude/projects/` lists them): Claude Code's system prompt tells the model that some tools are deferred and must be found with `ToolSearch`, and this model took the empty search result for the whole tool set. The branch still ends `ready` when it finds another way (the first run wrote the file through the `Monitor` tool), but it costs turns, and `branchyard-qualify` fails `permission_denied`, `permission_allowed`, `interrupt_during_permission` and `interrupt_during_tool` (5 of 9; no permission request ever reaches the driver). Set `ENABLE_TOOL_SEARCH=false` where Claude Code starts (the environment of `by`, or `env` in `~/.claude/settings.json`; for an `--isolated` run, in the private home's `.claude/settings.json`, where the isolated run that worked had it beside the base URL): the same model then called `Write` first, and qualification passed 9 of 9. One run each way, one model; an Anthropic model was not tried.

## Hooks are silent in my harness session

A harness hook says nothing when `by` has nothing new to say, and nothing when it cannot run. First ask whether the harness ran it at all: `by doctor` has a `hooks.ID` row per harness with hooks installed, `heard 3m ago (session-start)` when a session of it called `by hook`, `never heard` when none did ([`never heard`](#never-heard)). Then test the command by hand, from the repository, with a branch ready or failed (`BRANCHYARD_HOOK_CHECK=1` keeps your own call from being noted as the harness's):

```sh
echo '{"session_id":"s1","cwd":"'"$PWD"'"}' | BRANCHYARD_HOOK_CHECK=1 by hook --harness codex session-start
```

Printing `{"hookSpecificOutput":…"Branchyard, this repository. Tell the person in one line about any question or failure below; act on nothing unless they ask:\n- NAME: ready to merge, 1 file changed: by diff NAME, then by merge NAME"}` means `by` works and the harness is not running the hook. Printing nothing with no branches in the repository is correct. The cause is then one of:

- **`by` is not on the harness's `PATH`.** Every hook is `command -v by >/dev/null 2>&1 && by hook … || true`, so a missing `by` is silent. Check from the harness's own environment, then link `by` into a directory it has (`ln -s ~/.cargo/bin/by ~/.local/bin/by`).
- **The hook is not installed.** `by init plugin ID --dry-run` shows what would be written; `by setup` installs for every harness it finds and changes nothing the second time. Start a new session afterwards; one already open may not have read them.
- **Claude Code has not restarted.** It reads `~/.claude/settings.json` when a session starts; `/hooks` lists the two Branchyard hooks (`2 hooks on 2 events`). A plugin and `by setup` together run them twice: keep one. `claude -p` needs no trust step; the interactive `claude` asks about the folder first.
- **Codex has not trusted it.** It runs no hook until you review it: choose "Trust all and continue" at the start, or run `/hooks` and press `t`. `codex exec` never asks and skips an untrusted hook; `codex exec --dangerously-bypass-hook-trust` runs it for that one run.
- **Gemini CLI is in an untrusted folder.** Its hooks run only in a trusted folder, and `gemini -p` refuses to start in an untrusted one; trust the folder when asked, or set `GEMINI_CLI_TRUST_WORKSPACE=true` for automation.
- **Hermes has not approved it.** It asks once per hook and skips an unapproved one when there is no terminal. Approve it from a terminal first, or run once with `HERMES_ACCEPT_HOOKS=1`; `hermes hooks list` shows `✓ allowed` once it has.
- **DeepSeek Harness's profile has no bridge.** The hook row starts without it (`dsh: warning: 1 entry did not activate`) and says nothing to the session. `by setup` prints the line that adds it to the `acp` and `headless` profiles; `ls ~/.dsh/profiles/*/node_modules/@deepseek-ai` shows which profiles have `dsh-hooks-claude-code`. A prompt that asks the model for the note proves nothing in a resumed session, which still has the first note in its history; read what `by hook` printed instead.

[Harness hooks](harness-hooks.md) has a section per harness with what each one needs.

Run live on 25 October 2026 (Gemini CLI 0.65.0 with a Gemini API key; Antigravity CLI 1.4.7): the `interrupted` messages for `by cancel` and for a `kill -9` of `by` during a tool call, `over budget: max_turns` after `--max-turns 1` and a `by send`, the `failed` text of a Gemini CLI resume and of an Antigravity run without its provider, and Antigravity's soft-denial warning. Not done: the recovery, cancel, over-budget, failed and trust messages of every other harness were reproduced here with the fake agent. `blocked`, `awaiting plan approval` and the hook trust steps of Gemini CLI and Hermes come from the code and from [harness hooks](harness-hooks.md), not from a run on this machine. The `--remote` text is the refused-connection case only.
