# Troubleshooting

Find the words you see, read what they mean, run the command beside them. When you do not know where to start, run `by doctor` (it checks the machine and the repository and ends with what to fix) and `by ls`. The words come from `by`'s own output; [concepts](concepts.md) has the table of branch states.

## A branch is in a bad state

### `interrupted`

The turn stopped before it finished: you cancelled it with `by cancel NAME`, or the engine running it stopped (a closed terminal, a killed process, a reboot). The session and the worktree are kept.

- `by log NAME` says which. A cancel logs `warning: cancelled by by cancel`; an engine that stopped logs `recovered: its engine (pid N) is no longer running; the prompt had been submitted and the turn's outcome is unknown`.
- `by diff NAME` shows what the turn wrote before it stopped.
- `by send NAME --retry` submits the cut-off prompt again; `by send NAME "…"` sends a new one into the same session; `by rewind NAME --to N` goes back to checkpoint N first (0 is the base).

### `failed`

The turn ended in failure. The reason follows the colon in `by ls` and `by show NAME`: the harness exited or lost its connection (`the turn's outcome is unknown: the harness connection closed before the turn ended`), a plan was rejected, or the harness could not start.

- `by log NAME` has the last events and the harness's own error.
- `by harnesses` says whether the harness is installed, on `PATH` and logged in; `by harnesses login ID` signs in.
- `by send NAME "…"` tries again in the same session; `by rm NAME` throws the branch away.

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

### `check failed`

`by merge` printed `by: check failed:` followed by the check's output, and moved nothing. The check ran on the exact merge result, so it can fail where the branch's own tests passed. A check that overran shows `check timed out after …`.

- `by compare NAME --check` runs the check on the candidate in a private worktree and shows `failed` or `passed`.
- `by try NAME` applies the branch's changes to your checkout (it must be clean) so you can run the check yourself; `by try --off` takes them back out.
- `by send NAME "The check fails with: … Fix it."` gives the harness the output, then `by merge NAME` again.

### `has scripts you have not trusted`

The full message:

```text
by: /path/to/repo's [workspace] in /path/to/repo/branchyard.toml has scripts you have not trusted, and there is no terminal to ask on; nothing was created. Review it with `by workspace show`, then run `by workspace trust` in the repository
```

A repository's `[workspace]` scripts run as you, so they never run before you say so; on a terminal `by run` asks instead. `by workspace show` prints them; `by workspace trust` records their digest. A change to the scripts asks again (`changed since you trusted it`). `by doctor` shows the same state as `config.workspace_trust`.

### `likely logged out`

`by harnesses`, `by doctor` and `by up` say `ID is likely logged out (no ~/.claude/.credentials.json and no ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN)`. "Likely" means Branchyard looked for credential files and key variables by name and found none; the harness's own status command was not asked. A login kept somewhere else, such as a keychain, a cloud provider's variables or a key helper, looks the same, and `by run` still starts the harness. "Verified" means the harness itself said so.

- If the harness works when you start it yourself, ignore the row. `--auto` routing leaves such a harness out; name it with `--harness` instead.
- Otherwise `by harnesses login ID` runs the harness's own sign-in, or stores a key (`printf %s "$KEY" | by harnesses login ID --api-key`). `by up --plan` lists what only you can do.

### `its version command timed out`

`by harnesses` shows `?` as the version and `ID: no version: its version command timed out`. The probe waits a few seconds for `ID --version`, and a Node-based CLI can take longer on its first start after an install or a reboot. Run the CLI once yourself, then `by harnesses --refresh`. Without a version the qualification note cannot be matched, but the timeout alone does not stop `by run`.

### `cannot reach`

```text
by: cannot reach http://host:8421: Connection refused (os error 111)
by: remote mode needs a token: pass --token-file or set BRANCHYARD_TOKEN_FILE
```

The first comes from `--remote` (or `BRANCHYARD_REMOTE`, or `[remote] url`) when nothing answers; the second when no token file is given. `by doctor` shows the first as `server.reachable` and adds "start the server (`by serve`), or fix the URL, token or CA file". Check the scheme and port, start `by serve` on the host, and pass `--ca-file` for an https server with a private CA. Without `--remote`, commands run here and need no server.

### `this build has no PostgreSQL support` and `this build has no Microsandbox support`

Both are cargo features that are off by default. `by serve --database` needs a build with `postgres`, and `--provider microsandbox` one with `microsandbox` (it also needs Rust 1.94 or newer and `libcap-ng-dev`). Rebuild from the clone: `cargo install --locked --force --path crates/branchyard-cli --features postgres`, or `--features microsandbox`. [Server](server.md#postgresql) and [providers](providers.md) cover each.

### The shell cannot find `by`

`cargo install` puts `by` in `~/.cargo/bin`. Add that directory to `PATH`, then open a new shell. A harness started from an editor or a desktop launcher may have a shorter `PATH` than your shell; see the hooks entry below.

## Hooks are silent in my harness session

A harness hook says nothing when `by` has nothing new to say, and nothing when it cannot run. Test the command by hand, from the repository, with a branch ready or failed:

```sh
echo '{"session_id":"s1","cwd":"'"$PWD"'"}' | by hook --harness codex session-start
```

Printing `{"hookSpecificOutput":…"Branchyard: 1 ready to integrate (by ls)."}` means `by` works and the harness is not running the hook. Printing nothing with no branches in the repository is correct. The cause is then one of:

- **`by` is not on the harness's `PATH`.** Every hook is `command -v by >/dev/null 2>&1 && by hook … || true`, so a missing `by` is silent. Check from the harness's own environment, then link `by` into a directory it has (`ln -s ~/.cargo/bin/by ~/.local/bin/by`).
- **The hook is not installed.** `by init plugin ID --dry-run` shows what would be written; `by setup` installs for every harness it finds and changes nothing the second time. Start a new session afterwards; one already open may not have read them.
- **Codex has not trusted it.** It runs no hook until you review it: choose "Trust all and continue" at the start, or run `/hooks` and press `t`. `codex exec` never asks and skips an untrusted hook; `codex exec --dangerously-bypass-hook-trust` runs it for that one run.
- **Gemini CLI is in an untrusted folder.** Its hooks run only in a trusted folder, and `gemini -p` refuses to start in an untrusted one; trust the folder when asked, or set `GEMINI_CLI_TRUST_WORKSPACE=true` for automation.
- **Hermes has not approved it.** It asks once per hook and skips an unapproved one when there is no terminal. Approve it from a terminal first, or run once with `HERMES_ACCEPT_HOOKS=1`.

[Harness hooks](harness-hooks.md) has a section per harness with what each one needs.

Not done: the recovery, cancel, over-budget, failed and trust messages were reproduced here with the fake agent. `blocked`, `awaiting plan approval` and the hook trust steps of Gemini CLI and Hermes come from the code and from [harness hooks](harness-hooks.md), not from a run on this machine. The `--remote` text is the refused-connection case only.
