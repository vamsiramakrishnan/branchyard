# Driver qualification

Live protocol qualification of harness driver profiles against real harness binaries, run with `crates/branchyard-qualify`. Each profile's JSON report in this directory records the harness version, host, per-scenario result and the harness's own cost estimate.

**Scope.** The runner starts the harness as a local process with a scrubbed environment, a private home and its own process group, standing in for `SandboxProvider.exec`. That qualifies protocol behavior against the real binary. It does not qualify sandbox isolation, network policy, credential handling or node recovery; those gates in [harness integration](../harness-integration.md#qualification-suite) remain open.

## Results, 16 October 2026

Two profiles that had failed scenarios on 11 and 12 October ran again against the real binaries, after the driver and provisioner changes those failures pointed at. The report files in this directory are each profile's latest run.

| Profile | Harness version | Result | Report |
|---|---|---|---|
| `codex-app-server` | codex-cli 0.157.1, the same ChatGPT login (`~/.codex/auth.json` placed in the runner's private home) | 9 of 9, up from 6 of 9. The thread now starts with `approvalPolicy: "untrusted"` (it was `on-request`): `echo qualified > deny-marker.txt` reached the driver as an approval request and was denied, the marker stayed absent, and the turn completed; the interrupt during a pending approval and the interrupt during `python3 -c "import time; time.sleep(45)"` each stopped the turn at once, acknowledged by `turn/interrupt`'s answer. Fork and resume remembered the code word; the killed turn was reported unknown | [report](codex-app-server.json) |
| `antigravity-stream-json` | Antigravity CLI 1.4.0 (`agy`, updated in place since the 1.3.3 run) | 9 of 9, up from 4 of 9. The home held `"modelProvider": "gemini"` and `permissions.allow: ["command(*)"]`, which is what the provisioner now writes under `full`: the allowed command ran, and `sleep 60` ran long enough for the kill, which the driver reported as `OutcomeUnknown`. `permission_denied`, `interrupt_during_permission` and `interrupt_during_tool` are recorded `declared`: the profile has no tool approvals and no cancellation, so the runner checks the driver rejects an interrupt and does not spend a turn on them | [report](antigravity-stream-json.json) |

The 11 October finding that Antigravity "reports a killed turn as completed" was wrong, and that run's transcript shows why: under the seeded prefix rules the CLI soft-denied `sleep 60`, went on without it and ended the turn `SUCCESS` with `denied_actions: [{"action": "command", "display_name": "RunCommand"}]` in the result, all before the runner killed the process. The driver now reports each `denied_actions` entry as a warning on the turn (`the CLI soft-denied a command (RunCommand): no permissions.allow rule in its settings lets it run; the turn went on without it`), so a silent soft-denial is visible in `by log`.

**Z.ai (GLM).** A standard Z.ai key of the account listed the models (`GET /models` answered `200` on the Coding Plan chat endpoint `https://api.z.ai/api/coding/paas/v4`, the standard `…/paas/v4` and the Anthropic `…/anthropic/v1`: 11 models, `glm-4.5` to `glm-5.3-flashx`) but answered every completion `429` `1113 Insufficient balance or no resource package`; the GLM Coding Plan's own key, created under the plan, answered all three endpoints `200`. On it:

| Profile | Harness version | Result | Report |
|---|---|---|---|
| `oh-my-pi-acp` on `zai/glm-5.3` | Oh My Pi 18.10.3 | 9 of 9 in 67 seconds, with `ZAI_API_KEY` alone kept in the runner's environment: omp picked its `zai` provider and `glm-5.3` by itself. The report in this directory is this run (the Gemini run of 12 October was also 9 of 9) | [report](oh-my-pi-acp.json) |

Through the gateway in the scratch clone: DeepSeek Harness on `glm-5.3` via a `zai` backend (OpenAI chat completions, the `versioned` layer) completed the one-line README edit in five calls, and Claude Code 2.1.301 on `glm-5.3` via a `zai-anthropic` backend (Anthropic Messages, passed through) in six; Claude Code routed through the OpenAI backend instead first failed on a message with role `system` among its messages, which the gateway's translation now carries ([model gateway](../model-gateway.md#zai-glm)). Headless `omp -p --model zai/glm-5.3` answered `pong` in 5 seconds (the first try hung on `readPipedInput`: with stdin a pipe that never closes, omp waits for piped input, so a headless probe runs it with stdin from `/dev/null`).

**DeepSeek Harness through the model gateway.** In the scratch clone, `by run --harness deepseek-harness --isolated --model-gateway --model gemini-2.5-flash-lite --permissions edit-worktree` with a `gemini` backend configured: the new provisioner wrote `~/.dsh/cordis.patch.yml` with a `branchyard` provider at the turn's gateway, dsh called it, and the gateway metered ten calls `allowed via gemini (200, …)` through its Gemini compatibility layer before the turn completed and the branch went `ready`. The first attempt failed on every call with `could not connect (TLS: invalid peer certificate: UnknownIssuer)`: the gateway connected to backends directly, trusting the Mozilla roots alone, where this machine's outbound TLS goes through an intercepting proxy named by `HTTPS_PROXY` and `SSL_CERT_FILE`; it now reaches backends the way `by models list` does ([upstream proxies](../model-gateway.md#what-is-not-done)). The model itself (`gemini-2.5-flash-lite`) did badly: its `edit` calls missed their anchor five times and it finished by rewriting the whole README with `write` (+401 −473 for a one-line append), which is the model, not the plumbing.

**A routed fan with a judge.** The same clone ran `by fan --auto --judge` twice over Gemini CLI, Oh My Pi, Codex and DeepSeek Harness on a code task: Codex completed it both times (check passed, judged 95, the pick), Oh My Pi hit Google's quota on `gemini-3.5-flash`, DeepSeek Harness was first taken off the gateway by the Codex login secret the fan handed every candidate (fixed) and then failed on a `MALFORMED_FUNCTION_CALL` from `gemini-2.5-flash-lite` after editing through the gateway, and the judge harness failed on the same quota error so the deterministic judge stood in. The router's second pick leaned on the first run's outcomes. Details in [fleet](../fleet.md#a-routed-fan-on-real-harnesses-16-october-2026).

**The workflow.** `.github/workflows/qualify.yml` gained legs for `gemini-cli-acp` (`@google/gemini-cli@0.63.0`) and `oh-my-pi-acp` (`bun@1.4.3 @oh-my-pi/pi-coding-agent@18.10.3`), both on `GEMINI_API_KEY`, with `GEMINI_CLI_TRUST_WORKSPACE=true` kept through the runner's scrub. Its summary job still fails the run for every leg whose secret is missing unless the repository variable `QUALIFY_ALLOW_SKIPS` is `true`; the repository had none of `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `GEMINI_API_KEY` configured as Actions secrets when this was written, and setting them, or the variable, is a repository setting.

## Results, 11 October 2026

Three more profiles ran against real binaries installed that day, all on one Gemini API key; the runner provisions nothing, so each harness's private home was prepared by hand as the notes say.

| Profile | Harness version | Result | Report |
|---|---|---|---|
| `antigravity-stream-json` | Antigravity CLI 1.3.3 (`agy`, its install script) | 4 of 9 pass: `fresh_turn`, `clean_close`, `resume`, `fork` rejected as declared. `permission_denied`, `permission_allowed`, `interrupt_during_permission` and `interrupt_during_tool` fail because the stream carries no permission requests and the CLI soft-denies shell commands headless unless `permissions.allow` rules name them; `connection_lost` fails because the driver reported the killed turn as completed | [report](antigravity-stream-json.json) |
| `gemini-cli-acp` | Gemini CLI 0.63.0 (`npm @google/gemini-cli`) | 8 of 9 pass; `resume` fails to open with `Authentication required` although the key authenticated every other process | [report](gemini-cli-acp.json) |
| `deepseek-harness-acp` | DeepSeek Harness 0.2.0-rc.2 (`npm @deepseek-ai/dsh`), on Gemini | 5 of 9 pass: `fresh_turn`, `clean_close`, `resume`, `fork` rejected as declared, `connection_lost`. No permission request reached the driver, and the first tool call failed `400` at Gemini: a Gemini 3 model requires a `thought_signature` on every replayed function call, which the harness's OpenAI client drops | [report](deepseek-harness-acp.json) |
| `codex-app-server` | codex-cli 0.157.1 | Not run: the account logged in with `codex login --device-auth` (ChatGPT Pro) and the `CODEX_API_KEY` present both answer `Quota exceeded. Check your plan and billing details.` on every turn | — |

What the private homes held: Antigravity's `~/.gemini/antigravity-cli/settings.json` with `"modelProvider": "gemini"` (the key alone makes `agy` ask for a Google sign-in, and `GOOGLE_API_KEY` is not read); a second Antigravity run with `command(echo)`, `command(sleep)` and `command(sh)` under `permissions.allow` changed nothing, because a prefix rule does not match a command with a redirect, while `command(*)`, `command(regex:echo .*)` and the exact line do (probed directly, not in the runner). Gemini CLI ran with `GEMINI_CLI_TRUST_WORKSPACE=true`, without which a headless run refuses an untrusted directory. DeepSeek Harness ran with `DSH_HOME` pointing at a home whose `cordis.patch.yml` adds a provider for Gemini's OpenAI-compatible endpoint (`api: openai-completions`, `apiKeyEnv: GEMINI_API_KEY`, `compat: {supportsStore: false}`, since the endpoint rejects the OpenAI-only `store` field) and routes `agent-default-model` and `acp` to it; with `gemini-3.5-flash` every tool call fails on the missing thought signature, and with `gemini-2.5-flash-lite` a tool call ran in one of two direct tries, the other ending `MALFORMED_FUNCTION_CALL`. The endpoint accepts the documented dummy signature `skip_thought_signature_validator` in `extra_content.google.thought_signature` of a replayed tool call, which is how the model gateway can front Gemini for OpenAI-speaking harnesses.

### A fanned task on three harnesses, 12 October 2026

The same key, in a scratch clone, `by fan --harness gemini-cli,antigravity,deepseek-harness --isolated --secret GEMINI_API_KEY --permissions edit-worktree --allow-unapproved-tools --stall-after 8m --stall-action interrupt`: a knowledge task (read three docs, write a note on the five biggest friction points) and a code task (add a `--list` flag to `tools/check.py` with a test, checked by running the tests). DeepSeek Harness ran on `gemini-2.5-flash-lite` through its own Gemini provider.

| Harness | Knowledge task | Code task | What it showed |
|---|---|---|---|
| Gemini CLI 0.63.0 | ready, one file (+33) | interrupted after 11 minutes of searching; an earlier run stalled 22 minutes on `google_web_search` | Permission requests are now named by tool kind, so `edit-worktree` lets its writes through; its grounding search needs no permission and can hang a turn, so a `--stall-after` limit matters |
| Antigravity 1.3.3 | no changes | no changes | `run_command` is denied by `edit-worktree`, which the CLI soft-denies headless; its provisioned rules follow the policy, and under `full` they allow only `git` and `by`, so a test run needs a policy rule of its own (`command(regex:python3 .*)`) |
| DeepSeek Harness 0.2.0-rc.2 | failed: the model answered with no content | ready, two files (+235), tests passing | Before the generic provisioner handed over `GEMINI_API_KEY`, every isolated turn failed for want of the key; `gemini-2.5-flash-lite` answers empty now and then |

Costs were not reported by any of the three. The run also exposed that `by cancel` on a wedged ACP harness takes effect only once the 30-second interrupt grace has passed and the process is killed, which it does.

### Codex on a ChatGPT login, 12 October 2026

`codex-app-server` (codex-cli 0.157.1, `npm @openai/codex@0.157.1`, a Pro ChatGPT login made with `codex login --device-auth`): 6 of 9. The login's `auth.json` was placed in the runner's private home; the runner's scrubbed environment carries no `CODEX*` variable. Passing: fresh turn, permission allowed, clean close, resume, fork, connection lost. Failing:

- `permission_denied` and `interrupt_during_permission`: no permission request reached the driver. The driver starts threads with `approvalPolicy: "on-request"` and `sandbox: "workspace-write"`, and under those Codex runs a write inside the workspace (`echo qualified > deny-marker.txt`) without asking, so Branchyard's policy never gets to deny it. A policy that denies the shell needs the thread started with `approvalPolicy: "untrusted"` or a read-only sandbox, which the driver does not do yet.
- `interrupt_during_tool`: the driver sent no `turn/interrupt` while `python3 -c "import time; time.sleep(45)"` ran; the command ran to completion and the scenario timed out.

The day before, every Codex turn had failed with `Quota exceeded. Check your plan and billing details.` on an account with 98% of its weekly limit left. The cause was a `CODEX_API_KEY` variable in the environment, from an API organization without credit: Codex sends its requests with that key whatever `codex login status` says. `by harnesses` now names the variable beside a verified Codex login ([harness lifecycle](../harness-lifecycle.md#login-states)); isolated branches drop it and take the login as `CODEX_AUTH`. On that login Codex completed the knowledge task above in a minute (one file, +43) under `edit-worktree`, and the code task (the `--list` flag in `tools/check.py` with a test: two files, +21 −1, the 24 tests passing) in four minutes under `full`, each with `--isolated --secret CODEX_AUTH=@~/.codex/auth.json`.

### Oh My Pi on the Gemini key, 12 October 2026

`oh-my-pi-acp` (`omp acp`, Oh My Pi 18.10.3, `npm @oh-my-pi/pi-coding-agent@18.10.3`): 9 of 9 in 95 seconds, on `GEMINI_API_KEY` alone in the runner's private home, with nothing else prepared ([report](oh-my-pi-acp.json)). Shell commands reached the driver as permission requests and the denied one did not run; both interrupts stopped the turn at once; the session resumed under its own ID and remembered the code word; `session/fork` was rejected before launch as declared; the killed turn was reported unknown. omp reports no cost, so the cap did not apply. It picked `gemini-3.1-pro-preview`, its default for the Google provider, since nothing chose a model: `modelRoles.default` in `~/.omp/agent/config.yml` does, and Branchyard's provisioner now writes it from `--model` ([provisioning](../provisioning.md#oh-my-pi-tool-policies)).

The two tasks of the fanned run above, each `by run --harness oh-my-pi --isolated --secret GEMINI_API_KEY --stall-after 8m --stall-action interrupt`: the knowledge task under `edit-worktree` ended ready in about a minute (one file, +28; the note's five points were the unapproved-tools refusal, Gemini CLI's workspace trust, Amp's device login on stdout, Antigravity's `modelProvider` setting and the Codex login file as a secret), and the code task under `full` ended ready in four minutes (two files, +32: the `--list` flag, two tests, the 25 tests passing under the branch's check).

Setup friction found, fixed in the same change: the npm package is a Bun script that needs Bun 1.3.14 or newer (`engines.bun`), and under the machine's Bun 1.3.11 `omp --version` died with a `SyntaxError`, which `by harnesses` showed only as `its version command exited with status 1` (now the note names Bun, its version and the installer that upgrades it); the catalog had no key variables or login command for it, so `by harnesses` could not tell whether it could run (now `GEMINI_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, `omp login`, and the stored logins of `~/.omp/agent/agent.db` by provider name), and its resume flag was recorded as `--session`, which omp 18.10.3 does not have (`--resume`); `--model` on the profile was refused as unsupported over ACP (now written into its config); and under `read-only` omp would have written files without asking, since its ACP gate covers commands, deletions and moves only (now per-tool `deny` policies in its config, verified headless: `Tool "write" is blocked by user policy`).

## Results, 26 September 2026

| Profile | Harness version | Result | Report |
|---|---|---|---|
| `claude-code-stream-json` | Claude Code 2.1.283 | 9 of 9 pass | [report](claude-code-stream-json.json) |
| `claude-code-acp` | claude-agent-acp 0.81.2 | 9 of 9 pass; fork rejected as declared | [report](claude-code-acp.json) |
| `codex-app-server` | codex-cli 0.157.1 | Not run: no Codex credentials in the environment | — |
| Other ACP profiles | — | Not run: harnesses and credentials unavailable | — |

## Scenarios

| Scenario | Passes when |
|---|---|
| `fresh_turn` | A fresh session completes a turn, identifies its session, and reports usage where the profile declares it |
| `permission_denied` | A shell command reaches Branchyard as a permission request, is denied, and does not run; or the profile declares no tool approvals (Antigravity, whose own rules decide and whose profile needs `--allow-unapproved-tools`), recorded as `declared` without a turn |
| `permission_allowed` | An allowed shell command runs |
| `interrupt_during_permission` | Interrupting while a permission request is unanswered ends the turn as interrupted, and the command does not run; or the profile declares no tool approvals, recorded as `declared` |
| `interrupt_during_tool` | Interrupting after allowing a 45-second foreground command ends the turn as interrupted within 30 seconds; or the profile declares no cancellation and its driver rejects the interrupt (Antigravity, where a signal ends the process, which `connection_lost` exercises), recorded as `declared` |
| `clean_close` | Closing stdin ends the session; the report names any descendants that outlived the harness |
| `resume` | A new process resumes the session under the same ID and still knows a code word from the first turn |
| `fork` | A new process forks the session under a new ID that names its parent and knows the code word, or the profile rejects fork before launch as declared |
| `connection_lost` | Killing the harness mid-turn produces `OutcomeUnknown` for that turn |

## Findings

- **Antigravity routes no approvals.** Headless, every shell command is soft-denied until `permissions.allow` names it; the result event then carries `denied_actions` and stderr says which permission was missing. Branchyard's provisioner must therefore write rules, never pass `--dangerously-skip-permissions`. (The 11 October report that the driver reported a killed turn as completed was a misreading: the CLI had soft-denied the command and ended the turn before the kill; see the 16 October results.)
- **Gemini CLI resumes under another credential path.** A fresh process with the same `GEMINI_API_KEY` opened `session/load` and failed `Authentication required`; the first process, and every other scenario, authenticated with the key.
- **DeepSeek Harness on Gemini needs two shims.** Gemini's OpenAI-compatible endpoint rejects `store` and, on Gemini 3 models, replayed function calls without `thought_signature`; the harness's own `compat.supportsStore` covers the first, nothing on its side covers the second.
- **No cost from any of the three.** The ACP profiles report no usage the driver consumes, and Antigravity reports tokens without a price, so the `--max-cost-usd` cap did not apply; the key's own billing is the only limit.

- **claude-agent-acp leaves its Claude Code child running when it exits.** On `clean_close`, after the adapter exited, a `claude` process was still alive in its process group; the runner killed it and named it. The native profile left no survivors. Earlier reports could not have seen this: the runner's first survivor check used `ps -o comm= -g <pgid>`, which selects by session on procps. `branchyard-runtime` lists by process group and always signals it; the reports here come from that runner.
- **Harness descendants can outlive the harness.** On an early run, Claude Code declined a foreground `sleep 60 && …`, ran it as a background task instead, and ended the turn; that task was still running after the CLI process exited. The end of a turn is not the end of its work, and harness exit is not teardown. Branchyard must terminate the sandbox or process group, and account background tasks to the attempt. The runner, now built on `branchyard-runtime`, kills each harness's process group and names survivors.
- **claude-agent-acp makes permission bypass available by default.** The adapter passes `--allow-dangerously-skip-permissions` unless it runs as root without `IS_SANDBOX`. That lets a later mode switch skip Branchyard's permission answers; in this container it also made Claude Code refuse to start. The `claude-code-acp` profile now sends the adapter's documented per-session opt-out, `_meta.claudeCode.options.allowDangerouslySkipPermissions: false`. With it, every tool request reached Branchyard.
- **ACP reports no stable usage.** claude-agent-acp returns `usage` on prompt responses, but in ACP schema 1.9.1 that field is behind the unstable `unstable_end_turn_token_usage` feature, so the driver does not consume it. The runner's cost cap cannot apply to ACP profiles.
- **ACP cancellation has no acknowledgment.** As designed, the turn ends with `stopReason: cancelled`, and the client answers any pending permission request as cancelled itself.
- **Resumed and forked Claude sessions report cumulative cost from the parent.** Consumers take the increment, as the runner now does.
- **Driver fix found by qualification.** ACP and Codex open failures dropped JSON-RPC `error.data`, which held the cause; failure reasons now include it.

## Cost

Scenarios make real model calls with the credentials the harness finds. In this environment, the container's proxy supplied Claude credentials. The latest native run's harness-estimated cost was $0.11, excluding the killed turn, which reports none. Across all development, final and re-verification runs, including the ACP runs that report no cost, estimated spend was about $1.

## Running

```sh
cargo run -p branchyard-qualify -- --profile claude-code-stream-json \
    --workdir /tmp/qual --max-cost-usd 3 --report report.json
```

`--command` overrides the executable path. The runner strips <!-- fact:runtime.strip_prefixes -->`ANTHROPIC*`, `CLAUDE*`, `OPENAI*` and `CODEX*`<!-- /fact --> variables unless kept with `--keep-env NAME`; for example, `--keep-env OPENAI_API_KEY` for `codex-app-server`. The run stops scheduling scenarios once the harness's own cost estimates pass `--max-cost-usd`.

## Scheduled workflow

`.github/workflows/qualify.yml` runs the three profiles its matrix lists (`claude-code-stream-json` and `claude-code-acp` with `ANTHROPIC_API_KEY`, `codex-app-server` with `OPENAI_API_KEY`) every Monday at 06:17 UTC and on demand. A profile whose secret is not configured is recorded as `skipped: <secret> not configured`, is not run and is not qualified. The last job, `summary`, writes one table to the run's summary (profile, qualified, harness version, score) and fails the run when a profile was skipped or failed, naming the missing secrets. Set the repository variable `QUALIFY_ALLOW_SKIPS` to `true` to accept skipped profiles; it does not accept failures.
