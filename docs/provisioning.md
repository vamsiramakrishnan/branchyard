# Harness provisioning

In a sandbox, or with a private home, a harness finds none of your setup: no login, no MCP servers, no settings, no telemetry. Before each turn Branchyard prepares its home and environment from what the task asks for. The per-harness knowledge comes from [Scion](https://github.com/GoogleCloudPlatform/scion)'s harness provisioners, translated to Rust in [`branchyard-provision`](../crates/branchyard-provision/src/lib.rs). Only Claude Code's delivery of an API key, MCP servers and HTTP MCP headers has been checked against a real harness binary, offline and without a model call; the rest follows Scion; see [what is not validated](#what-is-not-validated).

## Design

Scion runs a Python `provision.py` inside the container before start. Its host stages universal inputs (auth candidates, MCP servers, instructions, telemetry) and secret files, and the script writes the harness's native files and an environment overlay ([authoring guide](../vendor/scion/harnesses/authoring-guide.md)). Branchyard keeps that split and makes it a typed contract with no I/O in the planner:

| Piece | Type | What it is |
|---|---|---|
| Request | `Provisioning` | What a task asks for, stored with the branch: secrets by name and source (`NAME`, `NAME=VAR`, `NAME=@FILE`), never their values; an explicit auth method; stdio MCP servers, whose variables may come from secrets (`secret_env`, by name); HTTP and SSE MCP servers, every header a secret by name; instructions; a model or size alias; a reasoning effort; telemetry (`off` or an OTLP/gRPC collector) |
| Context | `Context` | One turn's inputs, built by the engine: the harness and its profile's protocol, `HOME` and the working directory as the harness sees them, whether the home is private to the branch and whether it is a sandbox, the secrets with their values, every MCP server (the task's and the delegation server) with its resolved variables or headers and which of them are secrets, the instructions (the task's and the delegation skill), model, effort, telemetry, and what an earlier turn installed |
| Planner | `Provisioner::plan(&Context) -> Result<Plan, Refused>` | One per harness. Pure: no files, processes, environment or network |
| Plan | `Plan` | File edits relative to the home (replace, JSON merge, TOML reconciliation, instructions block, dotenv lines, remove), variables for the harness, the session items the driver passes itself (MCP servers, instructions, model, and for Claude Code's stream-json driver the MCP configuration file), the auth method chosen, **how each secret reaches the harness and whether its tool commands inherit it** (`Plan::secrets`, one `Delivery` per secret: a variable, a file, a helper-printed file, or an MCP server's variable or header), and the secrets it did not use |
| Executor | `apply::apply(&Plan, home)`, `apply::remove_credentials(home)` | Applies the file edits: read, edit in memory, write through a temporary file and a rename; removes the credentials a plan recorded |

Rules every provisioner follows:

1. **One path for MCP servers and instructions.** They go through the provisioner for every turn, and it keeps them on the driver's session channel where there is one: Claude Code's `--mcp-config` (a 0600 file, see rule 6) and plugin or appended system prompt, Codex's thread configuration and `developerInstructions`, ACP's `session/new` and first-prompt preamble. Only a harness whose driver has no channel gets them in its native configuration, in a private home. Today that is Antigravity: its driver used to refuse MCP servers and instructions, and with a private home they now go into `~/.gemini/config/mcp_config.json` and a block in `~/.gemini/GEMINI.md`. Pi and Amp still refuse them: Scion has no provisioner for either.
2. **Files only in a private home.** Local mode runs a harness in your own `HOME` unless the branch is `--isolated`; a plan that needs a file there is refused. Secrets are refused without a private home.
3. **Edit, never replace.** JSON keys are merged by path (other keys and other projects stay), TOML top-level keys and whole tables are reconciled line by line as Scion does without a TOML library, instructions sit between `<!-- BEGIN BRANCHYARD MANAGED -->` markers (Scion's markers are recognized and replaced), dotenv lines are set in place. A file that does not parse is refused and kept. An unchanged document is not rewritten, so re-provisioning is idempotent.
4. **What Branchyard installed is tracked** in `~/.branchyard/provisioned.json`: native MCP entries, so a later turn removes the MCP servers it no longer asks for and nothing a person added, and every credential it wrote (a whole file, or its variables in a dotenv file or keys in a JSON file), so removing the branch removes them.
5. **Secrets stay private.** Values are read at each turn from the named variable or file of the process running the turn, and reach only what the plan's deliveries say: the harness's environment, files in its private home written with mode 0600, or an MCP server's configuration. The variable a secret was read from is taken out of the harness's environment, so `--secret ANTHROPIC_API_KEY=MY_VAR` does not also hand it `MY_VAR`. Planning refusals, apply errors and the recorded `provisioned` event name secrets, never values; `Debug` output redacts them. The home's path components and target files are checked for symbolic links: a link on the way is refused, and a linked target is replaced rather than written through, so a harness cannot redirect a secret into the worktree.
6. **Nothing secret on a command line.** Every process on the host can read another's command line (`/proc/<pid>/cmdline`). Claude Code's stream-json driver, the one driver that passes MCP servers as arguments, is given the path of a 0600 file instead: `~/.branchyard/claude-mcp.json` in the private home, or, when the branch has none (then it has no secrets, but the delegation token is still a server variable), `.branchyard/turns/<random>/mcp-config.json` in a 0700 directory outside the worktree, removed when the turn ends. The driver refuses servers with variables or headers without a file. No other driver puts a server, a variable or a header in its arguments. A test spawns the fake agent in Claude Code's place and reads its `/proc/self/cmdline`.
7. **No approval bypass.** Scion launches Claude Code with `--dangerously-skip-permissions` and Codex with `--dangerously-bypass-approvals-and-sandbox`, seeds `approval_policy = "never"` and `"yolo": true`, and sets `HERMES_YOLO_MODE`. None of that is ported: Branchyard's drivers route every permission request. A test checks that no plan, for any harness or protocol, contains such a setting.

### Each turn

In [`engine::run`](../crates/branchyard/src/engine.rs) and [`provisioning`](../crates/branchyard/src/provisioning.rs), for every provider:

1. The delegation projection issues the turn's token and describes Branchyard's MCP server and skill, as before (none for a sandboxed harness).
2. The branch's `Provisioning` and the projection become a `Context`. Secrets are resolved now, and MCP server variables and headers from them; a missing or empty one fails the turn by name.
3. The profile's harness plans it. A refusal fails the turn, with the reason.
4. The plan's files are applied to the branch's private home on this host, **before** the sandbox exists, and a `provisioned` event records the method, the files written or removed, the variable names, how each secret was delivered and whether the harness's tool commands inherit it (`by log` says `… in the environment of its tool commands`), and unused secrets. Without a private home, Claude Code's MCP file is written for the turn.
5. The placement is prepared. A Microsandbox sandbox mounts the home at `/branchyard/home`; a Substrate actor receives it with the existing home transfer (each file is owner-only while it arrives, then gets its own mode) and returns it at the turn's end.
6. The variables secrets were read from are taken out of the harness's environment; the plan's variables are set on the harness process, after Branchyard's own; its session items, and the MCP file's path, go into the driver's `Open`.

A send provisions again with the branch's stored request unless it gives a new one; a fork inherits its parent's unless it gives one; a delegated child inherits its parent's unless its seat gives one (see `docs/rigs.md`).

## Harnesses

Scion's nine provisioners map onto Branchyard's harness IDs through [`branchyard_controls::harness`](../crates/branchyard-controls/src/harness.rs). Seven have Branchyard profiles and are translated; a test fails if a vendored Scion provisioner is neither translated nor explained.

| Scion | Harness | Profiles | Secrets read, in order | What is written or set | Model, effort, telemetry |
|---|---|---|---|---|---|
| `claude` | `claude-code` | `claude-code-stream-json`, `claude-code-acp` | `ANTHROPIC_API_KEY` (api-key); `CLAUDE_CODE_OAUTH_TOKEN` (oauth-token); `CLAUDE_AUTH` (auth-file); `GOOGLE_CLOUD_PROJECT` with `GOOGLE_CLOUD_LOCATION` or `GOOGLE_CLOUD_REGION` (vertex-ai) | An API key in `~/.branchyard/credentials/anthropic-api-key` (0600), printed by `apiKeyHelper` in `~/.claude/settings.json`, not in the environment; the token in the environment; `~/.claude/.credentials.json` (0600); Vertex variables; the workspace marked trusted in `~/.claude.json`; `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1` except with an API key; for stream-json, MCP servers in `~/.branchyard/claude-mcp.json` (0600) | `ANTHROPIC_MODEL`, size aliases through Scion's table (`large` is `claude-opus-5-5`); effort refused; `CLAUDE_CODE_ENABLE_TELEMETRY` and `OTEL_*` |
| `codex` | `codex` | `codex-app-server`, `codex-acp` | `CODEX_API_KEY` or `OPENAI_API_KEY` (api-key); `CODEX_AUTH` (auth-file) | `~/.codex/auth.json` (0600); `CODEX_HOME` | App Server: the thread's `model`; ACP: `model` in `config.toml`; `model_reasoning_effort` at the top of `config.toml`; an `[otel]` table |
| `gemini-cli` | `gemini-cli` | `gemini-cli-acp` | `GEMINI_API_KEY` or `GOOGLE_API_KEY`; `GEMINI_OAUTH_CREDS`; `GOOGLE_CLOUD_PROJECT` (with `GOOGLE_CLOUD_REGION`, `GOOGLE_CLOUD_LOCATION`) | The key in the environment; `~/.gemini/oauth_creds.json` (0600); `security.auth.selectedType` in `~/.gemini/settings.json` | `model.name` in `settings.json`; effort refused; `telemetry` in `settings.json` and `GEMINI_TELEMETRY_*` |
| `opencode` | `opencode` | `opencode-acp` | `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`; `OPENCODE_AUTH` | The key in the environment; `~/.local/share/opencode/auth.json` (0600) | All three refused |
| `copilot` | `github-copilot` | `github-copilot-acp` | `COPILOT_GITHUB_TOKEN`, `GH_TOKEN` or `GITHUB_TOKEN`; `COPILOT_CONFIG` | `COPILOT_GITHUB_TOKEN`; `~/.copilot/config.json` (the workspace added to `trustedFolders`, comment lines allowed); `autoUpdate` and `banner` defaults in `settings.json` | Model and effort refused; enabled telemetry sets `COPILOT_TELEMETRY_ENABLED` and `OTEL_*`, `off` writes nothing, as in Scion |
| `hermes` | `hermes` | `hermes-acp` | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `GOOGLE_API_KEY`; `VERTEX_PROJECT_ID` or `GOOGLE_CLOUD_PROJECT` (with `VERTEX_REGION`, `GOOGLE_CLOUD_REGION` or `GOOGLE_CLOUD_LOCATION`, default `us-central1`) | Lines in `~/.hermes/.env` (0600); `HERMES_HOME` | `HERMES_INFERENCE_MODEL` (Vertex defaults to Scion's `google/gemini-2.5-flash`); effort and telemetry refused |
| `antigravity` | `antigravity` | `antigravity-stream-json` | `GEMINI_API_KEY` or `GOOGLE_API_KEY` | The key in the environment; MCP servers in `~/.gemini/config/mcp_config.json`; instructions in `~/.gemini/GEMINI.md` | The driver's `--model`; effort and telemetry refused |
| `grok-build` | `grok-build` | none | | Not ported: Branchyard has no Grok Build profile or driver, so a provisioner would have nothing to launch or test | |
| `muse-code` | `muse-code` | none | | Not ported: Branchyard has no Muse Code profile or driver | |

Every other harness (Pi, Amp, Goose, Cursor, Qwen Code, Kimi CLI, Oh My Pi, DeepSeek Harness) has no provisioner: MCP servers, instructions and a model pass to its driver, which accepts or refuses them; secrets are reported unused; effort and telemetry are refused.

**Model.** Branchyard had no per-task model option. `Provisioning::model` (`--model`) is one, set where each harness takes it, as the table says. An ACP profile without a native setting refuses it, because ACP v1 has no model parameter.

### Secrets and the harness's tools

A variable in the harness's environment reaches every command its tools run (its shell tool, the MCP servers it starts) without anyone asking for it: `env`, a build script or a crash report can print it. A file in the private home does not travel with the environment, though a tool running as the same user could still open it: nothing but a harness that sandboxes its tools prevents that. So each provisioner prefers a file, or a helper that reads one, wherever the harness is known to take it, and otherwise says in its plan that the secret is in the tools' environment. The `provisioned` event carries that, per secret, by name.

| Harness | Secret | Delivered as | In the tools' environment | How this was established |
|---|---|---|---|---|
| Claude Code, both profiles | `ANTHROPIC_API_KEY` | `~/.branchyard/credentials/anthropic-api-key` (0600), which `apiKeyHelper` in `~/.claude/settings.json` prints with `cat` | No | Checked against the installed Claude Code 2.1.283 and the 2.1.280 that claude-agent-acp 0.81.2 runs (it loads user settings, and overrides `apiKeyHelper` only for a client-set provider), both offline against a local stand-in for the Messages API, no model call: requests carry the helper's key, a helper from user settings needs no workspace trust or key approval, and the Bash tool's `env` lacks the key. With `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1`, 2.1.283 ignores the helper and reports it is not logged in, so that variable is not set with a key. Also run end to end through `by run --isolated --secret ANTHROPIC_API_KEY=VAR --mcp … --delegate` against 2.1.283: neither the key nor `VAR` was in the Bash tool's or the MCP server's environment |
| | `CLAUDE_CODE_OAUTH_TOKEN` | the variable | **Yes** | Claude Code has no file or helper for a `claude setup-token` token: `apiKeyHelper` supplies an API key, `.credentials.json` a whole claude.ai login (use `CLAUDE_AUTH` for that), and `CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR` needs an inherited descriptor Branchyard's runtime does not pass. 2.1.283's Bash tool inherits the environment (checked with a variable); it scrubs credentials only under `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB`, which on Linux requires bubblewrap and is not set |
| | `CLAUDE_AUTH` | `~/.claude/.credentials.json` (0600) | No | Scion's path. Not checked: in the environment this was built in, Claude Code also found the host session's own login, so an offline check could not tell which it used |
| | `GOOGLE_CLOUD_PROJECT`, a region | `ANTHROPIC_VERTEX_PROJECT_ID`, `CLOUD_ML_REGION` | **Yes** | Scion's; these name a project and region, the credential is ADC, outside Branchyard |
| Codex | `CODEX_API_KEY` or `OPENAI_API_KEY`; `CODEX_AUTH` | `~/.codex/auth.json` (0600) | No | Scion's. Were a key a variable, codex-cli 0.157.1 would filter it anyway: its binary holds the default `shell_environment_policy` excludes `*KEY*`, `*SECRET*`, `*TOKEN*` (not exercised) |
| Gemini CLI | `GEMINI_API_KEY` or `GOOGLE_API_KEY` | the variable | **Yes**, assumed | No Gemini CLI binary here; no file-based key mechanism could be confirmed, so none is used. `GEMINI_OAUTH_CREDS` is the file-based choice |
| | `GEMINI_OAUTH_CREDS` | `~/.gemini/oauth_creds.json` (0600) | No | Scion's |
| | `GOOGLE_CLOUD_PROJECT`, region | the variables | **Yes** | Scion's |
| OpenCode | `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` | the variable | **Yes**, assumed | OpenCode's `auth.json` can hold API keys, but that could not be checked against a binary, so `OPENCODE_AUTH` (the whole file) is the file-based choice |
| | `OPENCODE_AUTH` | `~/.local/share/opencode/auth.json` (0600) | No | Scion's |
| GitHub Copilot CLI | `COPILOT_GITHUB_TOKEN`, `GH_TOKEN` or `GITHUB_TOKEN` | `COPILOT_GITHUB_TOKEN` | **Yes**, assumed | No binary here. `COPILOT_CONFIG` is the file-based choice |
| | `COPILOT_CONFIG` | `~/.copilot/config.json` (0600) | No | Scion's |
| Hermes | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `GOOGLE_API_KEY` | a line of `~/.hermes/.env` (0600) | **Yes**, assumed | Hermes loads that file into its own environment; whether its terminal tool filters it could not be checked |
| | `VERTEX_PROJECT_ID` or `GOOGLE_CLOUD_PROJECT`, region | the variables and `.env` lines | **Yes** | Scion's |
| Antigravity | `GEMINI_API_KEY` or `GOOGLE_API_KEY` | the variable | **Yes**, assumed | The `agy` 1.2.11 binary names only the variable; no file mechanism was found |
| Any | a secret in an MCP server's `secret_env` | that server's variable, in its configuration (a 0600 file or the session request on stdin) | No: only that server's process | Claude Code 2.1.283 started a server from a `--mcp-config` file with its variables |
| Claude Code, ACP agents | a secret in a remote MCP server's `headers` | that header, in the same way | No | Claude Code 2.1.283 sent the headers from a `--mcp-config` file to a local HTTP MCP server |

"Assumed" means the harness was not available to check and the conservative answer is recorded. Isolated mode strips only `ANTHROPIC*`, `CLAUDE*`, `OPENAI*` and `CODEX*` from the host's environment, so a host `GEMINI_API_KEY` or `GH_TOKEN` still reaches an isolated harness unless it is given with `--secret` (then its source variable is removed and only the plan's delivery remains).

### Not ported, per harness

- Scion's launch commands, hook wiring to `sciontool`, seed `home/` files, `capture_auth.py`, dialects and sidecar services (Hermes's dashboard): they serve Scion's own container, terminal and hook receiver.
- Claude: the API key in the environment with its fingerprint approved in `.claude.json` (a helper-printed file replaces both), the `.claude.json` MCP merge and CLAUDE.md projection (the session channel carries both), the `claude --version` probe, the default model `opus` (no model is set unless asked), and the telemetry cloud-provider checks, which belong to Scion's collector.
- Codex: the `[mcp_servers.*]` writer and AGENTS.md projection (session channel), and the OpenTelemetry environment from Scion's variables (always `production`).
- Gemini CLI: the native system prompt (`GEMINI_SYSTEM_MD`) and GEMINI.md projection (session channel).
- OpenCode: Vertex AI, the model setting (Scion writes `~/.config/opencode/.opencode.json`, which the `opencode acp` build may not read; a model that silently does not apply is worse than a refusal), the MCP writer, and the models.dev catalog download (network).
- Copilot: the MCP writer and instructions (session channel), header and CA overrides for telemetry, and the hooks file.
- Hermes: `HERMES_YOLO_MODE`, `HERMES_QUIET`, `HERMES_ACCEPT_HOOKS`, the `mcp.json` writer and AGENTS.md (session channel).
- Antigravity: Vertex AI (ADC) and `AGY_TOKEN`, which need `agy --version` and a generated launch wrapper; the thinking tier; onboarding files and hooks for Scion's interactive launch.
- Everywhere: Scion's explicit-type and no-auth gates become Branchyard's `auth` field and "no secrets, no authentication"; environment fallback and on-disk credential detection are dropped, because planning does no I/O.

### Found while porting

- Scion's Codex provisioner appends `model_reasoning_effort` after the last table of `config.toml`; with its own seed file that is `[projects."/workspace"]`, so TOML reads it as a key of that table. The port inserts top-level keys before the first table.
- At the previously pinned revision, `54b9387`, Scion's Claude tests expected a Python alias resolver the provisioner did not have (11 of 12 failed). At `d9b9e6a` the provisioner has it and all 13 tests pass; see [validation](validation.md).

## Connectors

A branch with a connector grant (`--connector`, `Provisioning::connectors`; see [connectors](connectors.md)) is provisioned one more thing each turn, on the same path and before the sandbox exists: the granted connectors' packages and `INDEX.md` replace `~/.branchyard/connectors/` in its private home, a token for the turn is written to `~/.branchyard/gateway-token` (0600, removed when the turn ends), `ANVIL_GATEWAY_URL` and `ANVIL_GATEWAY_TOKEN_FILE` are set after the plan's variables, and one line pointing at `INDEX.md` is added after the task's instructions, so it reaches the harness on the same session channel. The `provisioned` event adds `connectors` (`by log`: `provisioned: connectors github; ...`); it never holds the token. A grant needs a private home, like secrets, and a connector the gateway does not serve fails the turn by name. A delegated child's grant is narrowed to its parent's ([delegation](delegation.md#connectors)).

## Using it

SDK:

```rust
use branchyard::{
    Effort, McpServerSpec, Provisioning, RemoteMcpSpec, RemoteMcpTransport, SecretSource,
    TaskOptions,
};

// A stdio server whose DOCS_TOKEN variable is the task's DOCS secret, and
// an HTTP server whose Authorization header is its SEARCH secret. Only the
// secrets' names are stored with the branch.
let mut docs = McpServerSpec::parse("docs=/usr/local/bin/docs-mcp --stdio")?;
docs.secret_env.insert("DOCS_TOKEN".into(), "DOCS".into());
let search = RemoteMcpSpec {
    name: "search".into(),
    transport: RemoteMcpTransport::Http,
    url: "https://mcp.example.com/mcp".into(),
    headers: [("Authorization".into(), "SEARCH".into())].into(),
};
let options = TaskOptions {
    harness: Some("claude-code".into()),
    isolated: true,
    provision: Some(Provisioning {
        secrets: vec![
            SecretSource::parse("ANTHROPIC_API_KEY")?,
            SecretSource::parse("DOCS=DOCS_TOKEN_VAR")?,
            SecretSource::parse("SEARCH=@/run/secrets/search-bearer")?,
        ],
        mcp_servers: vec![docs],
        remote_mcp_servers: vec![search],
        ..Provisioning::default()
    }),
    ..TaskOptions::default()
};
```

`McpServerSpec::env` is stored with the branch like the rest of the request; put secrets in `secret_env`. Remote MCP servers are taken by Claude Code's stream-json driver and by ACP agents that advertise `mcpCapabilities` (claude-agent-acp 0.81.2 does); Codex, Antigravity, Pi and Amp refuse them. They have no `by` flag yet: use the SDK or the HTTP API (`"remote_mcp_servers": [{"name", "url", "transport": "http" | "sse", "headers": {"Header": "SECRET"}}]`).

`by run`, `fan`, `send` and `fork` take `--connector GRANT` (repeatable; [connectors](connectors.md#grants)), `--secret NAME[=VAR|=@FILE]` (repeatable), `--auth METHOD`, `--mcp NAME=COMMAND` (repeatable; an absolute executable and its arguments), `--instructions FILE`, `--model NAME`, `--effort low|medium|high|xhigh|0-100` and `--telemetry URL|off`:

```sh
by run "Fix the flaky parser test" --harness codex --isolated \
  --secret OPENAI_API_KEY --effort high --telemetry http://127.0.0.1:4317 --yes
by run "Same, in a microVM" --provider microsandbox --image ghcr.io/you/codex:0.157 \
  --secret CODEX_AUTH=@$HOME/.codex/auth.json --yes
```

`--secret` needs `--isolated` or a sandbox provider. `--pass-env` still copies variables into a sandbox by name; `--secret` is the one to use for credentials, because it also writes the harness's native credential files and records which method it chose.

### Through a server

`by --remote` and the HTTP API send the same request as a `provision` object on task, send and fork requests. The server decides where secrets come from:

- A request names secrets only. `by --remote` refuses `--secret NAME=VAR` or `=@FILE`; the API answers `400` to a request that names a source.
- The operator defines each secret a request may name, with `by serve --secret NAME[=VAR|=@FILE]` (repeatable; relative files resolve against the working directory) or `"secrets": {"NAME": "VAR" | "@FILE"}` in the configuration (relative to the file). A request naming any other secret gets `403 secret_not_allowed`. Values are read from the server's environment and files at each turn.
- MCP servers are commands the server runs, so they need `--allow-client-commands` (`403 command_not_allowed` otherwise). So do remote MCP servers: their headers are the server's secrets, sent to a URL the request chooses. Instructions, model, effort and telemetry need no opt-in.
- `--instructions FILE` is read by `by` on the caller's machine and sent as text.

### Removing a branch

`by rm <branch>` and `Yard::remove` delete the branch's private home when no other branch uses it, and in any case first remove the credentials provisioning recorded there: whole files (API key, `auth.json`, `.credentials.json`, the MCP file), and Branchyard's variables or keys in shared files (Hermes's `.env`, `apiKeyHelper` in `settings.json`, Antigravity's MCP entries), keeping everything else. That matters for a forked session, which shares its parent's home: before, removing the parent left its credentials there for the fork. The fork's next turn provisions them again from its own stored request. A branch still running in the shared home keeps them until its turn ends. `by rm --keep-credentials` and `Yard::remove_with(branch, &RemoveOptions { keep_credentials: true })` leave them; `by --remote rm` refuses the flag, since the server decides what stays on its disk.

## Security notes

- Variables reach everything the harness starts, including its tool commands. Which secrets are variables, per harness, is in [the table above](#secrets-and-the-harnesss-tools) and in each `provisioned` event; Claude Code's API key is not one of them. Prefer file-based credentials (`CLAUDE_AUTH`, `CODEX_AUTH`, `GEMINI_OAUTH_CREDS`, `OPENCODE_AUTH`, `COPILOT_CONFIG`) where the harness has them.
- No secret is on a command line (rule 6). MCP servers' variables and headers are in a 0600 file or on the harness's stdin.
- Credential files stay in the branch's private home between turns, 0600, like Scion's, because a send resumes the session there; the private home is under `.branchyard/homes/`. Removing the branch removes them (see [removing a branch](#removing-a-branch)).
- Without a private home, Claude Code's per-turn MCP file lives in `.branchyard/turns/<random>/` (0700) and is removed when the turn ends; if the engine itself dies mid-turn, the directory stays until removed by hand. It holds the delegation token, which the turn's end or recovery revokes, and any plain `env` of the task's servers.
- Telemetry is sent where the task says: Scion routes only to its local receiver, Branchyard to the caller's endpoint. Prompt content is not exported: Codex gets `log_user_prompt = false`, Gemini CLI `logPrompts: false`, and Claude Code's `OTEL_LOG_USER_PROMPTS` is left unset.

## Tests

- 59 unit tests in `branchyard-provision`, including Scion's cases for auth selection, managed blocks, TOML helpers, Claude model aliases, Codex reasoning effort and `[otel]`, and Hermes auth and Vertex regions, translated from the vendored test files, and dotenv variables removed.
- 22 integration tests: plans applied to temporary homes seeded with user files and compared with [golden files](../crates/branchyard-provision/tests/golden) (rewrite them with `BY_UPDATE_GOLDEN=1`, then review the diff; `env.txt` lists each secret's delivery), a second application changing nothing, file modes, links refused or replaced, unparseable files kept, no approval bypass in any plan, and every Scion provisioner translated or explained. Also: Claude Code's key in a 0600 file behind `apiKeyHelper` and gone, with its helper, when a later turn authenticates another way; a quote in the helper's path; Claude Code's MCP file in the home, and removed when a turn has no servers; every method of every harness says how each secret is delivered, with a variable only when the delivery says so and no value in anything not marked secret; secrets given to MCP servers as variables or headers; remote MCP servers refused where unsupported; and credential removal that leaves user lines, keys and MCP entries.
- 16 engine tests (12 in `tests/provision.rs`, 1 in `tests/substrate.rs`, 3 unit): a fake ACP agent run as `codex-acp` and `claude-code-acp` sees its provisioned files and variables, and neither Claude Code's API key nor the variable it came from is in its environment; the secret appears nowhere under the repository or its `.branchyard/` but the private home, nor in the event log or stored record; the fake agent standing in for Claude Code's stream-json binary records its `/proc/self/cmdline`: no API key, MCP variable or header there, `--mcp-config` names the 0600 file in the private home, or, without one, a 0600 per-turn file under `.branchyard/turns/` that is gone after the turn; removing a branch removes its credentials from a home another branch keeps, unless asked to keep them; a send re-provisions without rewriting and keeps a user's table; requests that cannot be honored are refused before a branch exists; a missing secret fails the turn by name; Antigravity writes its MCP configuration; and a provisioned home goes into a fake Substrate actor and comes back with mode 0600.
- Drivers: Claude Code's passes a file's path and refuses inline variables or headers; ACP sends HTTP and SSE servers that conform to the schema only to an agent that advertises them; Codex refuses them.
- Server and CLI: secrets resolved from the server's table and refused when a request names a source or an unknown secret, and remote MCP servers refused without `allow_client_commands`, over HTTP and through `by --remote`; `by rm --keep-credentials` parses.

## What is not validated

Checked against a real binary, offline, with a local stand-in for the Messages API and no model call (see [live testing](testing-live.md#7-provisioning-model-calls) for how): Claude Code 2.1.283 takes an API key from `apiKeyHelper` in user settings, sends it, and keeps it out of its Bash tool's environment (so does the 2.1.280 that claude-agent-acp 0.81.2 runs); it ignores the helper under `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1`; it reads `--mcp-config` from a file path, starts a stdio server from it with its variables, and sends an HTTP server's headers from it; and, through `by run --isolated --secret … --mcp … --delegate`, neither the key nor its source variable reached the Bash tool or the MCP server, and the command line held only the file's path.

Everything else is Scion's, or for Codex's `model` key over ACP, Codex's documentation: every other native path, file name and variable, and whether Gemini CLI, OpenCode, Copilot, Hermes and Antigravity pass variables to their tools (recorded as yes). Claude Code's `.credentials.json` was not checked: the host this was built on gave Claude Code another login to fall back on. Model calls themselves were not made. [Live testing](testing-live.md#7-provisioning-model-calls) lists the checks to run.
