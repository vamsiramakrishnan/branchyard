# Setup

`by init` sets Branchyard up by interview. One declarative interview per topic decides the questions, detects defaults from the machine, and produces a plan of files, each diffed against what exists and checked by the loader that will read it. Two front-ends run it:

- **A person** runs `by init` in a terminal: a wizard shows what it detected, asks, shows every diff, and writes only after a final confirmation.
- **A coding harness** (Claude Code, Codex, …) runs the [setup skill](../plugins/branchyard/skills/setup/SKILL.md): it drives `by init TOPIC --json --next`, asks the person each batch with its own question tool (Claude Code's `AskUserQuestion`), shows the dry-run diff, and applies after the person agrees.

Both front-ends call the same engine, [`branchyard-setup`](../crates/branchyard-setup/src/lib.rs), so they ask the same questions and write the same bytes. Nothing in setup calls a model, and no answer, plan or output ever holds a secret's value.

## Topics

| Topic | Writes | Checked by |
|---|---|---|
| `project` | `branchyard.toml` at the repository root, or the user's `~/.config/branchyard/config.toml`: default harness, model, effort, budget, turns, duration, permissions, isolation, check, provider, secrets by name, a server to use | the configuration parser `by` uses ([below](#configuration)) |
| `server` | A server configuration (default `.branchyard/server.json`): listen address and TLS, SQLite or PostgreSQL, one credential per tenant (hash only) with its token in a 0600 file under `tokens/`, tenant quotas, allowed providers, delegation, secrets by reference, a signed webhook | `branchyard-server`'s own loader and `Config::validate`, as `by serve --check` runs them |
| `rig` | A [rig](rigs.md) spec: a lead seat and, by shape, an implementer (two may run), a reviewer denied every editing tool, or two implementers on different harnesses; budgets split so the children fit | `by rig check`'s parser and planner |
| `deploy` | `compose.yaml` (PostgreSQL 16 and the server), `server.json`, and `secrets/` (client token, database password, both 0600, and a `.gitignore`) | the server's loader with `--insecure-bind`; `docker compose config` when docker is installed |
| `plugin` | The `setup` and `delegate` skills, byte for byte as shipped, into `.claude/skills`, `~/.claude/skills` or `~/.codex/skills`; or the command that loads the whole plugin | the skill frontmatter check |

`project` and `server` are the complete ones; `rig` covers the common team shapes (see [rigs](rigs.md) for fields it does not ask about); `deploy` and `plugin` are thin.

## For a person: the wizard

```sh
by init              # choose a topic
by init project
by init server
```

It detects the repository and its default branch, the installed harnesses and their versions (`--version`, three seconds at most each), which credential variables are set (by name only), docker and the PostgreSQL client tools, the platform and KVM, a check command from the files at the root (`Cargo.toml` → `cargo test`, `package.json` → `npm test`, …), and existing configuration files, whose values become the defaults. Then it asks, reviews every file with its diff and its validator's verdict, asks before replacing a file that differs, and asks once more before writing. Ctrl-C at any point writes nothing.

Without a terminal, `by init` refuses (exit 2) and points to the protocol. `--defaults` answers every question with its default. The wizard is `cliclack`; see [what was tested](#what-is-tested).

## For a harness: the protocol

```sh
by init --json                                          # the topics, and each one's first command
by init TOPIC --json --next [--answers FILE|-]          # the next batch, or done and the plan
by init TOPIC --answers FILE|- --dry-run [--json]       # the plan: files, diffs, validation
by init TOPIC --answers FILE|- --apply [--force] [--json]
```

Answers are one flat JSON object keyed by question id. A step returns the normalized answers so far; send them back with the new ones. The loop:

1. `--next` returns at most four questions, none depending on another in the same batch, so one `AskUserQuestion` call takes the whole batch.
2. Each question has an `id`, a `kind`, a `header` of at most 12 characters, a `prompt`, `why` it is asked, at most four `choices` (label, description, the recommended one first), `more_choices` reachable as "Other", whether other text is allowed, a `default`, whether it is `optional`, the `when` condition that made it appear, and its `rules`.
3. An answer may be a choice's value or its label (what `AskUserQuestion` returns), text a question's rules accept, `"skip"` or `null` for an optional question. A refused answer comes back in `errors` and its question is asked again; the message never repeats a `secret_ref` answer.
4. When no question remains, `"done": true` and `plan` holds the files, the commands to run next, notes, and `valid`.

A step, from `crates/branchyard-setup/tests/golden/server-second.json` (facts and questions shortened):

```json
{
  "protocol": "branchyard.setup/v1",
  "topic": "server",
  "done": false,
  "facts": [
    { "id": "repository", "label": "Repository", "value": "/src/app (default branch main)" },
    { "id": "harnesses", "label": "Harnesses", "value": "claude-code 2.1.283 (Claude Code), codex codex-cli 0.157.1" }
  ],
  "answers": { "database": "postgres", "listen": "0.0.0.0:8421", "path": ".branchyard/server.json", "tenancy": "multi" },
  "errors": [],
  "questions": [
    {
      "id": "tls", "kind": "confirm", "header": "TLS",
      "prompt": "Should the server serve HTTPS with its own certificate?",
      "why": "Required off loopback, unless a proxy terminates TLS and you pass --insecure-bind.",
      "choices": [
        { "value": true, "label": "Yes", "description": "give a PEM certificate chain and key", "recommended": true },
        { "value": false, "label": "No", "description": "plain HTTP (loopback only)" }
      ],
      "allow_other": false, "default": true, "optional": false,
      "when": { "truthy": { "id": "listen" } }
    },
    {
      "id": "database.url", "kind": "text", "header": "Postgres URL",
      "prompt": "Which PostgreSQL database?",
      "why": "Stored in the configuration, so it must not hold a password: use a .pgpass file.",
      "choices": [
        { "value": "postgres://branchyard@localhost/branchyard", "label": "Local database", "description": "user branchyard on localhost", "recommended": true }
      ],
      "allow_other": true, "default": "postgres://branchyard@localhost/branchyard", "optional": false,
      "when": { "equals": { "id": "database", "value": "postgres" } },
      "rules": [ { "rule": "postgres_url" } ]
    }
  ],
  "remaining": 11
}
```

A plan's file:

```json
{
  "path": ".branchyard/server.json", "kind": "server_config", "mode": "0644",
  "action": "create", "overwrites": false, "sensitive": false,
  "content": "{\n  \"listen\": \"127.0.0.1:8421\", …", "diff": "--- /dev/null\n+++ b/.branchyard/server.json\n…",
  "validation": { "validator": "branchyard-server config (by serve --check)", "ok": true }
}
```

A generated token is a file with `"sensitive": true`, `"mode": "0600"`, and `null` content and diff. Kinds of question: `select`, `multiselect`, `text`, `confirm`, `number`, `path`, and `secret_ref`, which asks where a secret is (a variable name, or `@path`) and refuses anything that looks like a credential itself. Refusals print `{"error": {"kind", "message", "paths"}}` on stdout and exit 1: `incomplete`, `invalid_plan`, `would_overwrite`, `invalid_answers`, `io`; usage errors exit 2.

The JSON Schema of every document is [`schema/setup.protocol.json`](../schema/setup.protocol.json). Output is deterministic: maps are ordered, questions keep their topic's order, and the same answers on the same machine print the same bytes (except a freshly generated token's hash).

### A Claude Code session

With the plugin loaded (`claude --plugin-dir plugins/branchyard`, or `by init plugin`), `/branchyard:setup server` or "set up a Branchyard server for this repo" starts the skill. An illustration of the exchange (not a recorded transcript; no model was run for this document):

```text
› /branchyard:setup server
● Bash(by init server --json --next)
  Detected: repository /src/app (main); Claude Code 2.1.283 and Codex installed; docker, psql.
● AskUserQuestion
  Config file: Beside by's state (recommended) · In the repository
  Listen:      Loopback (recommended) · All interfaces · IPv6 loopback
  Database:    SQLite (recommended) · PostgreSQL
  Tenants:     Just me (recommended) · Several teams
  › Beside by's state, Loopback, PostgreSQL, Several teams
● Write(.branchyard/setup-answers.json)
● Bash(by init server --json --next --answers .branchyard/setup-answers.json)
● AskUserQuestion   (TLS, Postgres URL, Tenant names, Scopes) …
● Bash(by init server --answers .branchyard/setup-answers.json --dry-run --json)
  create .branchyard/tokens/team-a-admin.token (0600, never shown)
  create .branchyard/server.json   ✓ branchyard-server config (by serve --check)
  Apply these 5 files?  › Yes
● Bash(by init server --answers .branchyard/setup-answers.json --apply --json)
● Bash(by serve --config .branchyard/server.json --check)   configuration ok
```

## Applying

`--apply` recomputes the plan from the answers, then:

- refuses an invalid plan (`invalid_plan`) and writes nothing;
- refuses to replace any file whose content differs (`would_overwrite`, listing them) unless `--force`; a file already exactly as planned is left alone, so applying twice writes nothing the second time;
- writes each file through a temporary file and a rename, with its mode;
- creates a generated secret only when it does not exist, and never reads, replaces or prints one that does: re-running `by init server` keeps each existing token (its hash is taken from the file) so clients keep working.

## Configuration

`branchyard.toml` at the repository root, and the user file `~/.config/branchyard/config.toml` (`$XDG_CONFIG_HOME` if set; `BRANCHYARD_USER_CONFIG` names another path), share one format. The project file overrides the user file key by key; `BRANCHYARD_REMOTE`, `BRANCHYARD_TOKEN_FILE`, `BRANCHYARD_REPO` and `BRANCHYARD_CA_FILE` override both; flags override everything. Relative paths resolve against the file's directory, and `~/` against the home directory.

```toml
#:schema https://raw.githubusercontent.com/vamsiramakrishnan/branchyard/main/schema/branchyard.config.json
version = 1

[defaults]                  # new branches: by run, by fan
harness = "claude-code"     # or a profile ID
model = "large"
effort = "high"             # low, medium, high, xhigh, or 0-100
auth = "api-key"
budget_usd = 5
max_turns = 25
max_minutes = 30
permissions = "ask"         # or "yes"; unset: ask on a terminal, else deny
isolated = true
check = "cargo test"
provider = "local"          # or "microsandbox", with [microsandbox]
instructions = "docs/agents.md"

[secrets]                   # applied only with a private home (isolated or sandboxed)
ANTHROPIC_API_KEY = "ANTHROPIC_API_KEY"   # the variable of that name
CODEX_AUTH = "@~/.codex/auth.json"         # a file; never the value itself

[mcp]
docs = "/usr/local/bin/docs-mcp --stdio"

[remote]
url = "https://branchyard.example.com:8421"
token_file = ".branchyard/tokens/admin.token"
ca_file = "ca.pem"
repo = "app"

[serve]
config = ".branchyard/server.json"          # by serve --config, when none is given

[microsandbox]
image = "ghcr.io/you/claude-code:2.1"
cpus = 2
memory_mib = 4096
pass_env = ["ANTHROPIC_API_KEY"]
```

The file is read strictly: an unknown key, a harness that is not in the registry, an effort, secret source, MCP command or URL the flags would refuse, is an error naming the key and its line, and every `by` command stops on it. A secret source that looks like a credential (a known key prefix, or a long mixed-case alphanumeric string) is refused without being quoted. The `#:schema` line lets taplo and editors with TOML schema support complete and check the file against [`schema/branchyard.config.json`](../schema/branchyard.config.json).

What each command takes, in `crates/branchyard-cli/src/defaults.rs` (one call, `defaults::apply`, before dispatch):

| Command | Takes |
|---|---|
| `run`, `fan` | every `[defaults]` key the flags left unset, `[mcp]`, and `[secrets]` when the branch has a private home; `isolated = true` cannot be turned off by a flag |
| `send`, `fork`, `reincarnate`, `spawn` | `permissions` only: the rest would override what the branch, its fork parent or its seat already has |
| `serve`, `worker` | `[serve] config` as `--config`, unless one is given |
| every command but `serve`, `init`, `config`, `mcp` | `[remote]` for what `--remote`, `--token-file`, `--ca-file`, `--repo` and their variables left unset; `token_file`, `ca_file` and `repo` only when `url` is the server in use |

Neither file is read by a `by` running inside a harness on a branch (`BRANCHYARD_BRANCH` set): the engine already gave that branch its options, and a project file must not turn a local branch's delegation into remote calls.

```sh
by config show [--json]              # every effective value and where it came from
by config path [--json]              # the user and project files, and whether they exist
by config validate [FILE] [--json]   # load FILE, or every file by reads, strictly; exit 1 if invalid
by config schema                     # the JSON Schema
```

## `by serve --check`

`by serve --config FILE --check` (also `branchyard-server --check`) builds and validates the configuration exactly as serving would, prints its warnings, and exits without binding, serving, or writing a file: where serving would create a default token it only says so. `by init server` and `by init deploy` check every server configuration they propose with the same function, on a private staging copy of the plan's files.

## What is tested

- **Engine** (`crates/branchyard-setup`): conditions and batching, answer normalization for every kind, detection with a fake probe (installed harnesses, KVM, existing files as defaults), every topic walked batch by batch answering by label and finishing with a valid plan, determinism of the JSON, no generated secret in any response, a pasted secret refused without being repeated, golden first batches, both schemas' freshness, and the embedded skills byte for byte against `plugins/branchyard/skills`.
- **CLI** (`crates/branchyard-cli/tests/setup.rs`): the built `by` in temporary repositories: a scripted harness completing every topic through `--next`, `--dry-run` and `--apply`; applying twice writes nothing; every generated file passes the tool that reads it (`by serve --check`, `by rig check`, `by config validate`); a multi-tenant server with one 0600 token per tenant and only hashes in the configuration; an off-loopback server without TLS refused by the server's loader; refusal to replace a file without `--force`; a pasted secret never echoed; no generated secret in any output; the non-terminal refusal; configuration defaults under flags and variables, and not inside a harness's branch. The wizard runs on a pseudo-terminal (util-linux `script`) accepting every default and writing after review; its question-to-prompt mapping and review rendering are unit-tested.
- Not tested: a real harness running the skill (no model call was made), the wizard on macOS, and `docker compose up` of a generated deployment.
