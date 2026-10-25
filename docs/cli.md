# Command line

`by --help` lists the commands by group; `by help <command>` (or `by <command> --help`, `-h` for a summary) shows a command's options under headings such as *Checks and limits*, *Permissions*, *Launch*, *Provisioning* and *Global options*, with examples for the main commands. Nested commands have their own help: `by help graph apply`, `by artifact publish --help`. A mistyped command or flag gets a suggestion (`by mrege` → `merge`), and a usage error exits with status 2, a failed command with 1.

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

## Tool permissions at the terminal

Every tool permission request reaches Branchyard. The Antigravity, Pi and Amp profiles cannot route them, so `by` refuses them unless you pass `--allow-unapproved-tools` (see [harness integration](harness-integration.md#implemented-drivers)). `--ask` prompts on the terminal, `--yes` allows each one, and with neither flag and no terminal they are denied. `by log` shows each decision. These commands are tested end to end against a fake ACP agent. Against a real harness, one `by run` → `by diff` → `by merge` has run with Claude Code 2.1.283 ([validation](validation.md)); the rest is on the [live testing checklist](testing-live.md). Resuming or forking a session in another worktree may fail for harnesses that keep sessions per directory, such as Claude Code; the branch then reports the failure rather than starting over silently.

## Per-command options

Each command's options are in `by help <command>`; the guide for a feature lists its own flags: [pull requests](pull-requests.md), [triggers](triggers.md), [workspace](workspace.md), [fleet](fleet.md), [plans and goals](plans-and-goals.md), [knowledge](knowledge.md), [map](map.md), [observability](observability.md), [connectors](connectors.md), [effects](effects.md), [model gateway](model-gateway.md), [egress](egress.md) and [setup](setup.md). [Surfaces](surfaces.md) lists every operation against every surface, and which option is refused where.
