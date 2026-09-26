# Driver qualification

Live protocol qualification of harness driver profiles against real harness binaries, run with `crates/branchyard-qualify`. Each profile's JSON report in this directory records the harness version, host, per-scenario result and the harness's own cost estimate.

**Scope.** The runner starts the harness as a local process with a scrubbed environment, a private home and its own process group, standing in for `SandboxProvider.exec`. That qualifies protocol behavior against the real binary. It does not qualify sandbox isolation, network policy, credential handling or node recovery; those gates in [harness integration](../harness-integration.md#qualification-suite) remain open.

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
| `permission_denied` | A shell command reaches Branchyard as a permission request, is denied, and does not run |
| `permission_allowed` | An allowed shell command runs |
| `interrupt_during_permission` | Interrupting while a permission request is unanswered ends the turn as interrupted, and the command does not run |
| `interrupt_during_tool` | Interrupting after allowing a 45-second foreground command ends the turn as interrupted within 30 seconds |
| `clean_close` | Closing stdin ends the session; the report names any descendants that outlived the harness |
| `resume` | A new process resumes the session under the same ID and still knows a code word from the first turn |
| `fork` | A new process forks the session under a new ID that names its parent and knows the code word, or the profile rejects fork before launch as declared |
| `connection_lost` | Killing the harness mid-turn produces `OutcomeUnknown` for that turn |

## Findings

- **Harness descendants can outlive the harness.** On an early run, Claude Code declined a foreground `sleep 60 && …`, ran it as a background task instead, and ended the turn; that task was still running after the CLI process exited. The end of a turn is not the end of its work, and harness exit is not teardown. Branchyard must terminate the sandbox or process group, and account background tasks to the attempt. The runner now kills each harness's process group and names survivors.
- **claude-agent-acp makes permission bypass available by default.** The adapter passes `--allow-dangerously-skip-permissions` unless it runs as root without `IS_SANDBOX`. That lets a later mode switch skip Branchyard's permission answers; in this container it also made Claude Code refuse to start. The `claude-code-acp` profile now sends the adapter's documented per-session opt-out, `_meta.claudeCode.options.allowDangerouslySkipPermissions: false`. With it, every tool request reached Branchyard.
- **ACP reports no stable usage.** claude-agent-acp returns `usage` on prompt responses, but in ACP schema 1.9.1 that field is behind the unstable `unstable_end_turn_token_usage` feature, so the driver does not consume it. The runner's cost cap cannot apply to ACP profiles.
- **ACP cancellation has no acknowledgment.** As designed, the turn ends with `stopReason: cancelled`, and the client answers any pending permission request as cancelled itself.
- **Resumed and forked Claude sessions report cumulative cost from the parent.** Consumers take the increment, as the runner now does.
- **Driver fix found by qualification.** ACP and Codex open failures dropped JSON-RPC `error.data`, which held the cause; failure reasons now include it.

## Cost

Scenarios make real model calls with the credentials the harness finds. In this environment, the container's proxy supplied Claude credentials. The final native run's harness-estimated cost was $0.074, excluding the killed turn, which reports none. Across all development and final runs, including the ACP runs that report no cost, estimated spend was under about $1.

## Running

```sh
cargo run -p branchyard-qualify -- --profile claude-code-stream-json \
    --workdir /tmp/qual --max-cost-usd 3 --report report.json
```

`--command` overrides the executable path. The runner strips `ANTHROPIC*`, `CLAUDE*`, `OPENAI*` and `CODEX*` variables unless kept with `--keep-env NAME`; for example, `--keep-env OPENAI_API_KEY` for `codex-app-server`. The run stops scheduling scenarios once the harness's own cost estimates pass `--max-cost-usd`.
