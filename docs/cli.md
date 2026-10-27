# Command line

`by --help` lists the commands by group; `by help <command>` (or `by <command> --help`, `-h` for a summary) shows a command's options under headings such as *Checks and limits*, *Permissions*, *Launch*, *Provisioning* and *Global options*, with examples for the main commands. Nested commands have their own help: `by help graph apply`, `by artifact publish --help`. A mistyped command or flag gets a suggestion (`by mrege` → `merge`), and a usage error exits with status 2; every other failure exits with the status of its kind ([exit status](#exit-status)).

The global options choose where commands run, and may come before or after the command; each falls back to its variable, and a flag wins over its variable. Under both sits the configuration, `branchyard.toml` in the repository and `~/.config/branchyard/config.toml` ([setup](setup.md#configuration)): it fills only what the command line and the variables left unset, so the order is flags, then variables, then the project file, then the user file:

| Option | Variable |
|---|---|
| `--remote URL` | `BRANCHYARD_REMOTE` |
| `--token-file FILE` | `BRANCHYARD_TOKEN_FILE` |
| `--repo NAME` | `BRANCHYARD_REPO` |
| `--ca-file FILE` | `BRANCHYARD_CA_FILE` |

Short forms: `-n` for `--name`, `-b` for `--base`, `-y` for `--yes`, `-f` for `log --follow`, `-o` for `artifact get|export --out`, `-V` for `--version`. Durations take a unit: `--max-minutes 90s`, `--stall-after 2h`, `watch --interval 250ms` (a bare number keeps its old unit); `--budget-usd` also takes `$2.50`. `--check` and `--command` are split like a POSIX shell, including `#` comments.

Shell completions and a man page are generated from the same definitions, including `init`'s topics (`by init <Tab>` offers `project`, `server`, `rig`, `deploy` and `plugin`) and `config`'s actions:

```sh
by completions bash > ~/.local/share/bash-completion/completions/by
by completions zsh > "${fpath[1]}/_by"
by completions fish > ~/.config/fish/completions/by.fish
by completions powershell >> $PROFILE      # or elvish
by man | man -l -
```

`by serve` and `by worker` hand everything after them to the server's own parser, so `by serve --help` is the server's help and `--repo NAME=PATH` there is the server's flag, not the global one; `branchyard.toml`'s `[serve] config` becomes their `--config` unless they name one (`-c FILE` too), and `by serve --config FILE --check` validates a configuration without serving. `branchyard-server`, `branchyard-bridge`, `branchyard-herdr`, `branchyard-mcp` and `branchyard-qualify` parse their command lines the same way (clap), each with `--help`.

## Exit status

A failure says what failed and the command to run next (`by: …` then `  next: …` on standard error, or with `--json` the envelope `{"error": {"kind", "message", "next", "detail", "exit"}}` on standard output), and `by` exits with the status of its kind. A script can tell the classes apart without reading the text: 2 means change the command line, 3 means change something around it (install, log in, start a server, `cd` into a repository), 4 means a policy or limit said no. The `exit` member of the envelope is the status the process ends with. The table is generated from `crates/branchyard-cli/src/errors.rs` (`Exit`, `EXITS`, `KINDS`), where a unit test walks every error of the engine and checks that its kind, and the kind a server gives the same error, has one status:

<!-- fact:cli.exit_codes -->

| Status | Meaning | `--json` kinds |
|---|---|---|
| 0 | the command did what it was asked | — |
| 1 | the operation ran and failed: a branch failed, a check failed, a merge conflicted, the branch was busy; also a failure not yet given a kind (`unclassified`), and any kind not listed here | `already_merged`, `check_failed`, `check_not_started`, `check_timed_out`, `command_failed`, `conflict`, `dirty_target`, `fenced`, `git`, `harness`, `incomplete`, `internal`, `invalid_answers`, `invalid_candidate`, `invalid_plan`, `io`, `no_candidate`, `no_plan`, `not_running`, `running`, `stale_revision`, `state`, `steer_refused`, `target_moved`, `unclassified`, `would_overwrite` |
| 2 | the command line asks for what cannot be: a bad flag or value (clap's own usage errors too), an unknown branch, harness or repository, a name already taken | `branch_exists`, `invalid_name`, `invalid_request`, `invalid_rig`, `not_found`, `unknown_branch`, `unknown_harness`, `unknown_knowledge`, `unknown_message`, `unknown_operation`, `unknown_repo`, `unknown_trigger`, `usage` |
| 3 | something around the command must change first: not a repository, a harness (or gh, ssh) not installed, a configuration file that does not load, a server that does not answer or refuses the token, a login only you can do | `config`, `harness_unavailable`, `manual`, `not_a_repository`, `remote_protocol`, `remote_unreachable`, `shutting_down`, `tool_unavailable`, `unauthorized` |
| 4 | refused before anything ran: a policy, a delegation envelope, a budget or quota, or a capability the harness lacks | `command_not_allowed`, `delegation_not_allowed`, `denied`, `provider_not_allowed`, `quota_exceeded`, `rate_limited`, `repo_not_allowed`, `scope_required`, `secret_not_allowed`, `unapproved_tools_not_allowed`, `unsupported` |
| 130 | interrupted with Ctrl-C: `by` installs no handler, so the shell reports 128 + SIGINT (`by serve` and `by worker` catch it and shut down) | — |

<!-- /fact -->

`by run`, `by fan` and `by send` exit 1 when the branch (every branch, for `fan`) ends `failed`, after printing what happened; a branch that ends `ready`, `no changes`, `budget exceeded` or `interrupted` is not a failure of the command. A pre-flight refusal before a harness starts (not installed, logged out) is `harness_unavailable`, exit 3. A step only you can do, such as a login that needs a terminal (`by harnesses login` without one), is `manual`, exit 3. Before 27 October 2026 every failure but clap's usage errors and `manual` (then 2) exited 1; a script that checked for 1 should check for a status other than 0, or for the one it means. A bare message not yet given a kind is reported as `unclassified` (exit 1, as before); `tools/failure_message_ratchet.json` counts the places that still build one, and the count only falls.

## Tool permissions at the terminal

Every tool permission request reaches Branchyard. The Antigravity, Pi and Amp profiles cannot route them, so `by` refuses them unless you pass `--allow-unapproved-tools` (see [harness integration](harness-integration.md#implemented-drivers)). `--ask` prompts on the terminal, `--yes` allows each one, and with neither flag and no terminal they are denied. `by log` shows each decision. These commands are tested end to end against a fake ACP agent. Against a real harness, one `by run` → `by diff` → `by merge` has run with Claude Code 2.1.283 ([validation](validation.md)); the rest is on the [live testing checklist](testing-live.md). Resuming or forking a session in another worktree may fail for harnesses that keep sessions per directory, such as Claude Code; the branch then reports the failure rather than starting over silently.

## Per-command options

Each command's options are in `by help <command>`, and all of them in one page in the [command reference](cli-reference.md), generated from the same definitions (`by man --markdown`); the guide for a feature lists its own flags: [pull requests](pull-requests.md), [triggers](triggers.md), [workspace](workspace.md), [fleet](fleet.md), [plans and goals](plans-and-goals.md), [knowledge](knowledge.md), [map](map.md), [observability](observability.md), [connectors](connectors.md), [effects](effects.md), [model gateway](model-gateway.md), [egress](egress.md) and [setup](setup.md). [Surfaces](surfaces.md) lists every operation against every surface, and which option is refused where.
