# Harness provisioning

In a sandbox, or with a private home, a harness finds none of your setup: no login, no MCP servers, no settings, no telemetry. Before each turn Branchyard prepares its home and environment from what the task asks for. The per-harness knowledge comes from [Scion](https://github.com/GoogleCloudPlatform/scion)'s harness provisioners, translated to Rust in [`branchyard-provision`](../crates/branchyard-provision/src/lib.rs). Nothing here has run against a real harness; see [what is not validated](#what-is-not-validated).

## Design

Scion runs a Python `provision.py` inside the container before start. Its host stages universal inputs (auth candidates, MCP servers, instructions, telemetry) and secret files, and the script writes the harness's native files and an environment overlay ([authoring guide](../vendor/scion/harnesses/authoring-guide.md)). Branchyard keeps that split and makes it a typed contract with no I/O in the planner:

| Piece | Type | What it is |
|---|---|---|
| Request | `Provisioning` | What a task asks for, stored with the branch: secrets by name and source (`NAME`, `NAME=VAR`, `NAME=@FILE`), never their values; an explicit auth method; stdio MCP servers; instructions; a model or size alias; a reasoning effort; telemetry (`off` or an OTLP/gRPC collector) |
| Context | `Context` | One turn's inputs, built by the engine: the harness and its profile's protocol, `HOME` and the working directory as the harness sees them, whether the home is private to the branch and whether it is a sandbox, the secrets with their values, every MCP server (the task's and the delegation server) and the instructions (the task's and the delegation skill), model, effort, telemetry, and what an earlier turn installed |
| Planner | `Provisioner::plan(&Context) -> Result<Plan, Refused>` | One per harness. Pure: no files, processes, environment or network |
| Plan | `Plan` | File edits relative to the home (replace, JSON merge, TOML reconciliation, instructions block, dotenv lines, remove), variables for the harness, the session items the driver passes itself (MCP servers, instructions, model), the auth method chosen, and the secrets it did not use |
| Executor | `apply::apply(&Plan, home)` | Applies the file edits: read, edit in memory, write through a temporary file and a rename |

Rules every provisioner follows:

1. **One path for MCP servers and instructions.** They go through the provisioner for every turn, and it keeps them on the driver's session channel where there is one: Claude Code's `--mcp-config` and plugin or appended system prompt, Codex's thread configuration and `developerInstructions`, ACP's `session/new` and first-prompt preamble. Only a harness whose driver has no channel gets them in its native configuration, in a private home. Today that is Antigravity: its driver used to refuse MCP servers and instructions, and with a private home they now go into `~/.gemini/config/mcp_config.json` and a block in `~/.gemini/GEMINI.md`. Pi and Amp still refuse them: Scion has no provisioner for either.
2. **Files only in a private home.** Local mode runs a harness in your own `HOME` unless the branch is `--isolated`; a plan that needs a file there is refused. Secrets are refused without a private home.
3. **Edit, never replace.** JSON keys are merged by path (other keys and other projects stay), TOML top-level keys and whole tables are reconciled line by line as Scion does without a TOML library, instructions sit between `<!-- BEGIN BRANCHYARD MANAGED -->` markers (Scion's markers are recognized and replaced), dotenv lines are set in place. A file that does not parse is refused and kept. An unchanged document is not rewritten, so re-provisioning is idempotent.
4. **Native entries Branchyard installed are tracked** in `~/.branchyard/provisioned.json`, so a later turn removes the MCP servers it no longer asks for and nothing a person added.
5. **Secrets stay private.** Values are read at each turn from the named variable or file of the process running the turn, and reach only the harness's environment and files in its private home, which are written with mode 0600. Planning refusals, apply errors and the recorded `provisioned` event name secrets, never values; `Debug` output redacts them. The home's path components and target files are checked for symbolic links: a link on the way is refused, and a linked target is replaced rather than written through, so a harness cannot redirect a secret into the worktree.
6. **No approval bypass.** Scion launches Claude Code with `--dangerously-skip-permissions` and Codex with `--dangerously-bypass-approvals-and-sandbox`, seeds `approval_policy = "never"` and `"yolo": true`, and sets `HERMES_YOLO_MODE`. None of that is ported: Branchyard's drivers route every permission request. A test checks that no plan, for any harness or protocol, contains such a setting.

### Each turn

In [`engine::run`](../crates/branchyard/src/engine.rs) and [`provisioning`](../crates/branchyard/src/provisioning.rs), for every provider:

1. The delegation projection issues the turn's token and describes Branchyard's MCP server and skill, as before (none for a sandboxed harness).
2. The branch's `Provisioning` and the projection become a `Context`. Secrets are resolved now; a missing or empty one fails the turn by name.
3. The profile's harness plans it. A refusal fails the turn, with the reason.
4. The plan's files are applied to the branch's private home on this host, **before** the sandbox exists, and a `provisioned` event records the method, the files written or removed, the variable names and unused secrets.
5. The placement is prepared. A Microsandbox sandbox mounts the home at `/branchyard/home`; a Substrate actor receives it with the existing home transfer (file modes are carried) and returns it at the turn's end.
6. The plan's variables are set on the harness process, after Branchyard's own; its session items go into the driver's `Open`.

A send provisions again with the branch's stored request unless it gives a new one; a fork inherits its parent's unless it gives one; a delegated child inherits its parent's.

## Harnesses

Scion's nine provisioners map onto Branchyard's harness IDs through [`branchyard_controls::harness`](../crates/branchyard-controls/src/harness.rs). Seven have Branchyard profiles and are translated; a test fails if a vendored Scion provisioner is neither translated nor explained.

| Scion | Harness | Profiles | Secrets read, in order | What is written or set | Model, effort, telemetry |
|---|---|---|---|---|---|
| `claude` | `claude-code` | `claude-code-stream-json`, `claude-code-acp` | `ANTHROPIC_API_KEY` (api-key); `CLAUDE_CODE_OAUTH_TOKEN` (oauth-token); `CLAUDE_AUTH` (auth-file); `GOOGLE_CLOUD_PROJECT` with `GOOGLE_CLOUD_LOCATION` or `GOOGLE_CLOUD_REGION` (vertex-ai) | The key or token in the environment; for an API key, its approval fingerprint added to `~/.claude.json` and the workspace marked trusted (0600); `~/.claude/.credentials.json` (0600); Vertex variables; `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST=1` | `ANTHROPIC_MODEL`, size aliases through Scion's table (`large` is `claude-opus-5-5`); effort refused; `CLAUDE_CODE_ENABLE_TELEMETRY` and `OTEL_*` |
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

### Not ported, per harness

- Scion's launch commands, hook wiring to `sciontool`, seed `home/` files, `capture_auth.py`, dialects and sidecar services (Hermes's dashboard): they serve Scion's own container, terminal and hook receiver.
- Claude: the `.claude.json` MCP merge and CLAUDE.md projection (the session channel carries both), the `claude --version` probe, the default model `opus` (no model is set unless asked), and the telemetry cloud-provider checks, which belong to Scion's collector.
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

## Using it

SDK:

```rust
use branchyard::{Effort, McpServerSpec, Provisioning, SecretSource, TaskOptions};

let options = TaskOptions {
    harness: Some("codex".into()),
    isolated: true,
    provision: Some(Provisioning {
        secrets: vec![SecretSource::parse("OPENAI_API_KEY")?],
        mcp_servers: vec![McpServerSpec::parse("docs=/usr/local/bin/docs-mcp --stdio")?],
        effort: Some(Effort::High),
        ..Provisioning::default()
    }),
    ..TaskOptions::default()
};
```

`by run`, `fan`, `send` and `fork` take `--secret NAME[=VAR|=@FILE]` (repeatable), `--auth METHOD`, `--mcp NAME=COMMAND` (repeatable; an absolute executable and its arguments), `--instructions FILE`, `--model NAME`, `--effort low|medium|high|xhigh|0-100` and `--telemetry URL|off`:

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
- MCP servers are commands the server runs, so they need `--allow-client-commands` (`403 command_not_allowed` otherwise). Instructions, model, effort and telemetry need no opt-in.
- `--instructions FILE` is read by `by` on the caller's machine and sent as text.

## Security notes

- Variables reach everything the harness starts, including its tool commands. Codex filters variables named with `KEY`, `SECRET` or `TOKEN` from its shell by default; Claude Code does not. Prefer file-based credentials (`CODEX_AUTH`, `CLAUDE_AUTH`) where a harness has them.
- Claude Code's stream-json driver passes MCP servers as a `--mcp-config` argument, so a server's variables are visible in the process list on the host that runs it. Do not put secrets in `--mcp` variables.
- Credential files stay in the branch's private home after the turn, 0600, like Scion's; the private home is under `.branchyard/homes/`. Removing the branch removes them.
- The Substrate home transfer creates files and then sets their modes, so a secret file is briefly created with the bridge's default mode inside the actor.
- Telemetry is sent where the task says: Scion routes only to its local receiver, Branchyard to the caller's endpoint. Prompt content is not exported: Codex gets `log_user_prompt = false`, Gemini CLI `logPrompts: false`, and Claude Code's `OTEL_LOG_USER_PROMPTS` is left unset.

## Tests

- 58 unit tests in `branchyard-provision`, including Scion's cases for auth selection, managed blocks, TOML helpers, Claude model aliases, Codex reasoning effort and `[otel]`, and Hermes auth and Vertex regions, translated from the vendored test files.
- 16 integration tests: plans applied to temporary homes seeded with user files and compared with [golden files](../crates/branchyard-provision/tests/golden) (rewrite them with `BY_UPDATE_GOLDEN=1`, then review the diff), a second application changing nothing, file modes, links refused or replaced, unparseable files kept, no approval bypass in any plan, and every Scion provisioner translated or explained.
- 11 engine tests: a fake ACP agent run as `codex-acp` and `claude-code-acp` sees its provisioned files and variables; the secret appears nowhere under the repository or its `.branchyard/` but the private home, nor in the event log or stored record; a send re-provisions without rewriting and keeps a user's table; requests that cannot be honored are refused before a branch exists; a missing secret fails the turn by name; Antigravity writes its MCP configuration; and a provisioned home goes into a fake Substrate actor and comes back with mode 0600.
- Server and CLI: secrets resolved from the server's table and refused when a request names a source or an unknown secret, over HTTP and through `by --remote`.

## What is not validated

Nothing here ran against a real harness. Every native path, file name and variable is Scion's (or, for Codex's `model` key over ACP, Codex's documentation), and has not been checked against the harness versions Branchyard's drivers were recorded with. [Live testing](testing-live.md#7-provisioning-model-calls) lists the checks to run.
