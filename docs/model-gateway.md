# Model gateway

A harness normally calls its model provider itself, with a key in its environment or a login in its home. On the model gateway it calls Branchyard instead. The gateway speaks the provider's own API, so the harness works unmodified: its base URL points at the gateway, and its API key is the turn's token. The gateway checks the token, picks a backend, adds the real key, streams the answer back and records what the call cost. The key never enters the harness's environment, home or sandbox.

The same token carries the turn's other scopes: its connector grant, its network policy and what it may delegate. What a turn may reach is the intersection of its seat's ceiling, its person's ceiling and what the person approved. See [one scope](#one-scope).

Status, 1 October 2026: built and tested hermetically against mock Anthropic and OpenAI servers on loopback, with a fake harness (a Python script the fake ACP agent runs). No real provider was called and no real harness has used it.

## Turning it on

| Where | How |
|---|---|
| `by run`, `fan`, `send`, `fork`, `reincarnate` | `--model-gateway` (every model a route serves) or `--model-gateway='claude-sonnet-*,gpt-5'` (only these, as globs) |
| `branchyard.toml` | `[models] allow = ["claude-*"]` puts new branches on the gateway when the flag gives none |
| A rig seat | `models = ["claude-haiku-*"]`, within its parent seat's |
| SDK | `Provisioning::models` (`branchyard::models::ModelAccess`) in `TaskOptions::provision` |
| HTTP and `by --remote` | `provision.models` in a task, send or fork request: `{"allow": ["claude-*"]}` |

The access is stored with the branch. A send that names none keeps the branch's: a branch on the gateway stays on it. A delegated child of a branch on the gateway stays on it too, within its parent's models; a seat or a send that asks for a model its parent may not call is refused `denied`.

A branch on the gateway may not be given a provider's key as a secret (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `CODEX_API_KEY`): the turn fails naming it. `[secrets]` entries with those names, or named by a backend's `key`, are not given to a new branch on the gateway.

## Configuration

`branchyard.toml` (or the user file):

```toml
[secrets]
anthropic = "ANTHROPIC_API_KEY"          # where the key is: a variable, or "@path"

[models]
allow = ["claude-*", "gpt-5*"]           # new branches use the gateway, for these models
# listen = "127.0.0.1"                   # what each turn's gateway binds (default)
# sandbox_host = "192.168.127.1"         # how a sandboxed harness reaches it
# seed = 7                               # a reproducible weighted choice

[models.backends.anthropic]
api = "anthropic"                        # anthropic, openai or generic
key = "anthropic"                        # an entry of [secrets], else a variable of that name
# url = "https://api.anthropic.com"      # the default for anthropic

[models.backends.spare]
api = "anthropic"
url = "https://proxy.example.com/anthropic"
key = "SPARE_KEY"

[models.backends.openai]
api = "openai"                           # url defaults to https://api.openai.com
key = "OPENAI_API_KEY"

[models.backends.local]
api = "generic"
url = "http://127.0.0.1:11434"
key = "LOCAL_KEY"
header = "x-api-key"                     # default: authorization, as Bearer <key>

[[models.routes]]
model = "claude-*"                       # a glob over the requested model; first match wins
backends = ["anthropic", "spare"]        # chosen among by weight
weights = [3, 1]
fallbacks = []                           # tried in order after the weighted ones
requests_per_minute = 120                # per route, in this process

[models.budget]                          # over every branch of the repository, UTC
daily_usd = 20.0
monthly_usd = 300.0
daily_tokens = 50000000
monthly_tokens = 1000000000
alert_at = 0.8                           # record an alert at this share of a limit

[models.prices."llama-*"]                # for models the catalog does not price, per million tokens
input = 0.2
output = 0.6
```

A key is named, never written: `key` names an entry of `[secrets]`, whose value says the variable (`"VAR"`) or file (`"@path"`) that holds it; without such an entry it is a variable of that name. Keys are read when a turn's gateway starts, in the process that runs the turn, so a rotated key is used from the next turn. `by config validate` checks `[models]`; `by` checks the rest (a backend named in a route that does not exist, a weight of 0, a bad URL) when it opens the repository.

On a server, the configuration file takes the same table as `"models"`, with keys named in its `"secrets"` or else variables of the server's own environment, and `"ceilings"` (below). See [server](server.md).

## Each turn

When a turn of a branch on the gateway starts, before its harness does:

1. Its access is narrowed by the person's ceiling ([one scope](#one-scope)).
2. If its harness is logged in by subscription, or speaks an API no backend serves, it runs [direct](#subscription-logins-and-direct-mode) and nothing below happens.
3. A gateway for the turn listens on a port of its own on `[models] listen` (default `127.0.0.1`).
4. The turn's token is the connector token when the turn has a connector grant (one token carries both), or one minted for the gateway with an empty grant. It is signed with the yard's Ed25519 key, the connector gateway's when there is one ([keys and tokens](connectors.md#keys-and-tokens)), and lives at most an hour and not past the turn's deadline.
5. The harness gets `ANTHROPIC_BASE_URL` (`http://127.0.0.1:PORT/anthropic`) and `ANTHROPIC_API_KEY` when a backend speaks Anthropic's API, `OPENAI_BASE_URL` (`…/openai/v1`) and `OPENAI_API_KEY` when one speaks OpenAI's, and `BRANCHYARD_MODEL_GATEWAY` (the gateway's address). Both keys are the token. `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, `CODEX_API_KEY`, `OPENAI_ORG_ID` and `AZURE_OPENAI_API_KEY` are taken out of its environment.
6. A restricted network policy gets the gateway's host and port for this turn ([egress](egress.md#the-connector-gateway)); the provider's hosts need not be allowed.
7. A `model` event says where its models go: `models through the gateway at http://127.0.0.1:PORT (claude-*)`.
8. The gateway registers in the [registry](registry.md) as `model_gateway`, with its branch, `apis` and `models`, and leaves it when the turn ends.

When the turn ends the gateway stops taking calls, waits up to five seconds for calls in flight, and what the turn's calls cost is added to the branch's cost.

### Paths

| Path | API |
|---|---|
| `/anthropic/...` | Anthropic's (`/anthropic/v1/messages`, and the rest of its API as it is) |
| `/openai/...` | OpenAI's (`/openai/v1/chat/completions`, `/openai/v1/responses`, and the rest) |
| `/backends/NAME/...` | passed through to backend `NAME` as it is, with its key |

The rest of the path and the query follow the backend's base URL. A request's model is its JSON body's `model`.

### Each call

1. **The token.** Sent as `x-api-key` or `Authorization: Bearer`, as the harness's SDK sends its key. It must be this turn's token, verify against the yard's public keys, be unexpired, and carry `by_models`. Otherwise `401`.
2. **The scope.** A model outside `by_models` is refused `403` and never forwarded. A body that says it is JSON and is not is refused `400`. A request that names no model, such as listing models, is forwarded without this check.
3. **Budgets.** The branch's (`--budget-usd`: what the branch had spent, with what its children reserved, plus this turn's calls) and the repository's daily and monthly cost and tokens, from the stored usage. Over any: `403`, and nothing is forwarded.
4. **The route.** The first route whose `model` glob matches and that has a backend of the request's API. Its `requests_per_minute` is held over a sliding minute in this process: over it, `429` with `Retry-After`. Without a matching route, every backend of that API, in name order.
5. **The backend.** One of the route's backends chosen by weight (seeded by `[models] seed`, else at random), then its other backends in order, then its fallbacks. The next is tried when a backend refuses the connection, has no key, or answers `5xx` or `429`. When every one fails, the harness gets the last one's answer, as it would from the provider. A connection that breaks after the request was sent is not retried: the request may have been billed.
6. **The request.** The harness's headers, without its credentials, hop-by-hop headers and `Accept-Encoding`, plus the backend's key (`x-api-key` for Anthropic, `Authorization: Bearer` for OpenAI, the configured header for a generic backend) and `Accept-Encoding: identity`, so usage can be read. An OpenAI chat completion that streams is asked for its usage (`stream_options.include_usage`) unless it already says. Nothing else in the body changes.
7. **The response.** Streamed to the harness as it arrives (`Transfer-Encoding: chunked`, `Connection: close`), server-sent events included, while its usage is read as it passes.
8. **The record.** Before the response's last chunk reaches the harness, the call is metered, stored as a usage record, and recorded on the branch as a `model` event, so the next call is held to what this one cost.

Refusals are in the provider's own error shape (`permission_error`, `rate_limit_error`, `authentication_error`), with `X-Branchyard-Gateway` naming the decision.

### Usage and cost

Tokens come from the provider's own usage fields:

| API | Fields | Streamed |
|---|---|---|
| Anthropic Messages | `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens` and `cache_creation.ephemeral_1h_input_tokens` | `message_start` for the input side, the last `message_delta` for the output |
| OpenAI chat completions | `prompt_tokens` (with `prompt_tokens_details.cached_tokens`), `completion_tokens` | the final chunk's `usage` |
| OpenAI responses | `input_tokens` (with `input_tokens_details.cached_tokens`), `output_tokens` | `response.completed`'s `response.usage` |

A cached input token is counted once, as a cache read. Cost is `[models.prices]` for the model (the longest matching glob), else `catalog/pricing.toml`, the table `by usage` estimates with: Claude's long-context tier and one-hour cache writes, OpenAI's cached-input and long-context rates. A model neither prices is recorded with no cost and counted as unpriced.

A usage record has the time, branch (`<repo>/<branch>` on a server), turn, person, model, API, backend, the four token counts, cost, latency, status and whether it streamed. It is kept in the repository's store (SQLite or PostgreSQL) when its branch is removed: it is the repository's spending, which period budgets count.

A branch on the gateway costs exactly what its calls cost: each turn adds its calls' metered cost to the branch's `cost_usd`, and the harness's own estimate is not used. While a turn runs, its limit is held on the metered cost, as it is on a harness's estimate otherwise. A turn recovered after its engine stopped adds the calls it recorded.

### Budgets

- **The branch's.** `--budget-usd`, a delegated child's limits, and what its children reserved, as for any branch ([budgets](design.md)). A call that would start over it is refused before it is forwarded; a call already running is not cut short, so a branch can end slightly over by its last call.
- **The repository's.** `[models.budget]`: daily and monthly (UTC) cost and tokens over every branch, removed ones included, read from the stored usage before each call. A call is refused when a limit is reached. When a call takes spending past `alert_at` of a limit (default 0.8), a `model budget alert` event is recorded, once per period in each process.

## Subscription logins and direct mode

A harness logged in by subscription (Claude Code's or Codex's OAuth login, a ChatGPT login) sends a token tied to that login, which only the provider accepts; the gateway cannot replace it with a key. Branchyard recognizes this when the branch's secrets include `CLAUDE_CODE_OAUTH_TOKEN` or `CODEX_AUTH`, or its `--auth` is `oauth-token`. A `CODEX_AUTH` file is taken for a login even when it holds a key. Such a turn runs **direct**: no gateway, its harness calls its provider itself, and when its harness is Claude Code or Codex its provider's hosts are added to its egress policy for the turn (`api.anthropic.com:443` and `console.anthropic.com:443`; `api.openai.com:443`, `chatgpt.com:443` and `auth.openai.com:443`). A harness whose API no backend serves (Codex with only Anthropic backends) runs direct the same way.

Direct is recorded (`models direct: claude-code is logged in by subscription (CLAUDE_CODE_OAUTH_TOKEN), which cannot take an injected key`) and shown by `by show`. Its calls are not metered: its cost is the harness's own estimate.

A login Branchyard does not see, such as a local branch whose harness uses the login in your own home, is not recognized: the harness gets the gateway's variables, and a harness that prefers its login to `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` goes around the gateway. On Linux, an egress policy that allows nothing but the gateway stops it.

## One scope

A turn's token carries every scope the turn has. Besides the contract's claims ([token](connectors.md#token-branchyard-mints-anvil-verifies)), unchanged:

| Claim | Value |
|---|---|
| `by_models` | The models the turn may call through the gateway, as globs; absent off the gateway |
| `by_network` | The turn's network policy: `policy` (`open`, `none` or its rules), `allow` (the rules, absent when open), `enforce`, and `digest` (`blake3:` over the policy's wire form) |
| `by_delegation` | `depth`, `max_depth` (0 when it may not delegate), `max_children`, and `harnesses` when limited |

All three are optional and plain JSON; Anvil's verifier reads only the contract's claims and ignores them. A turn's token always carries `by_network` and `by_delegation`, and `by_models` on the gateway; a person's connect token carries none. The contract's claims are unchanged, and a token without the three is exactly what it was.

What a turn may reach is the intersection of three things:

- **The seat's ceiling.** A delegated child's connectors, models and network are narrowed to its parent's when it is spawned or sent, and a rig's seats are checked against their parent seats when the rig is planned ([delegation](delegation.md), [rigs](rigs.md)). What is stored with the branch is already within it.
- **The person's ceiling.** A `Ceiling` per person caps every branch acting for them, whatever a request asks for. On a server it is `"ceilings"`, by principal name; in the SDK, `Yard::use_ceilings`, by subject (`local:<user>` locally).
- **What the person approved.** The branch's stored request: what the person asked for with `by run` or a request, a send that changed it, or a plan they approved.

At the start of every turn the person's ceiling is applied to the stored request: connectors by the grant intersection ([grants](connectors.md#grants)), models by glob intersection, the network by keeping the rules each side covers of the other (an open request takes the ceiling's rules; the stricter enforcement applies). The connector grant, the model access, the network policy the egress proxy enforces, and the token's claims are that one intersection. When the ceiling narrowed anything, an `access` event says what: `access narrowed by ci's ceiling: models * to claude-haiku-*`. The stored request is not changed, so a raised ceiling applies from the next turn.

```json
"ceilings": {
  "ci": {
    "connectors": ["github:read"],
    "models": ["claude-haiku-*"],
    "network": ["api.github.com:443", "*.npmjs.org:443"]
  }
}
```

A part left out leaves that scope as requested. Glob intersection is sound but not complete: when Branchyard cannot tell that one glob covers another it keeps neither, so a turn can get less than strictly allowed, never more.

## Where it runs

Each turn gets its own gateway in the process that runs the turn: `by run` locally, `by serve` or a `by worker` on a server. It is started and stopped with the turn, like the [egress proxy](egress.md#the-proxy).

Why per turn, not one long-lived daemon:

- The keys stay in the process that already holds the turn's other secrets, and nowhere else. There is no second process to secure, supervise or find.
- The token it takes is exactly its turn's, and its events and cost go straight to its branch, under the turn's lease.
- It works the same locally, on a server and on a worker, with nothing to configure but `[models]`.

What turns share is shared through the store, not a process: usage records, so period budgets hold across processes and servers on one database. Rate windows and the weighted choice's state are per process (per yard in it). A sandboxed harness reaches its turn's gateway at `[models] sandbox_host` with the gateway listening there (`listen = "0.0.0.0"` or that address); without `sandbox_host`, a sandboxed branch on the gateway fails its turn naming it. That address is not checked on a KVM host or a Substrate cluster.

## What you see

| Where | What |
|---|---|
| `by log` | `models through the gateway at http://127.0.0.1:41203 (claude-*)`, then `model: claude-sonnet-4-6 allowed via anthropic (200, 1210 in / 80 out, $0.0048, 812 ms)` per call, refusals with their reason, budget alerts, `models direct: …`, and `access narrowed …` |
| `by log --json`, event streams | `{"activity": "model", "model": {"kind": "gateway"\|"direct"\|"call"\|"alert", ...}, "text"}`; `{"activity": "access", ...}` |
| `by show` | `models  gateway (claude-*); 3 calls, 4.2k in / 310 out tokens, $0.0123; refused 1 denied`, or `models  direct (…)` |
| `by show --json` | `models: {mode, models, reason, calls, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd, unpriced, refused}`; `cost_usd` is the metered cost |
| `by watch` | the latest call as what the branch is doing |
| `by models [--period day\|month\|all] [--json]` | the backends (each key's source and whether it is set, never its value), routes, budgets with today's and this month's spending, and usage by model |
| `by stats` | `models  N calls (allowed …, denied …), T tokens, $X metered`; `--json`: `model_calls`, `model_tokens`, `model_cost_usd` |
| `/metrics` | `branchyard_model_calls_total{model,decision}`, `branchyard_model_tokens_total{model,kind}`, `branchyard_model_cost_usd_total{model}` |
| SDK | `Yard::use_models`, `Yard::models`, `Yard::model_usage(since_ms)`, `Yard::use_ceilings`; `models::{Gateway, Config, ModelAccess, ModelActivity, ModelCall, UsageRecord, Tokens, summarize, period_starts}` |

`by models` reads this repository's configuration and store; with `--remote` it is refused (a server's usage is on its `/metrics`).

## What is not done

- **No real provider or harness.** Every test uses mock upstreams on loopback. Whether Claude Code, Codex or any other harness honors the base URL variables for everything it does, and with which paths, is untested.
- **Subscription logins** cannot go through the gateway (above). A login Branchyard does not see is not detected.
- **Provider features not proxied.** The gateway passes the body through unchanged except for `stream_options.include_usage`; it does not translate between APIs, rewrite models per backend, or pass batch, file, realtime or WebSocket APIs anything special. A backend must speak the same API as the request (a route never sends an Anthropic request to an OpenAI backend).
- **Prompt caching** passes through: the harness's `cache_control` blocks and headers go to the backend as they are, and cache reads and writes are metered and priced. A weighted choice can send a conversation's next call to another backend, whose cache is cold; give a route one backend (and fallbacks) to keep caches warm. No responses are cached by the gateway.
- **Rate limits** are per process; several `by` processes or servers each hold their own window. Tokens per minute are not limited.
- **Budget alerts** are recorded once per period in each process, as events; nothing is sent.
- **Cost after a turn ends.** A call still running five seconds after its turn ends is stored and recorded, but not added to the branch's cost.
- **Upstream proxies.** The gateway connects to backends directly; it does not use `HTTPS_PROXY` itself.
- **Gemini, Bedrock and Vertex** are generic pass-through only: no usage is read from their shapes unless they match the ones above, and their paths name the model, which the scope does not check.
- **A server** fails a turn on the gateway when it has no `models` backends, rather than refusing the request at admission.
- **The pricing tables** are a copy of the ones `by usage` uses, read from the same file; the model-name rules are duplicated in `branchyard::models::pricing`.

## Tests

- `branchyard-provision` (3): model access flags, globs and the wire form; narrowing for children and ceiling intersection; network ceiling intersection.
- `branchyard` (15 unit, and 1 conformance check): usage from each provider's fields, streamed across chunk boundaries; catalog pricing; the seeded weighted order and its 3:1 share over 400 calls; the route rules; rate windows freeing a slot after a minute; direct mode for subscription logins and unserved APIs and the hosts it allows; path routing; the usage request for OpenAI chat streams; errors in each provider's shape; base URL parsing; refused configurations; UTC day and month starts; the network digest; token claims (the contract's unchanged without the new scopes, old claims parsed, the new ones signed and verified). The storage conformance suite's `usage` check runs on SQLite and on PostgreSQL.
- `branchyard` engine (`tests/models.rs`, 7, mock upstreams and the fake ACP agent running a Python harness): both APIs, streamed and not, through the gateway with the real key injected, the token never upstream, the key nowhere in the harness's environment or the event log, exact tokens and costs, one usage row per call and the branch's cost equal to their sum, and the token's claims; scope, wrong-token and unreadable-body refusals with nothing forwarded; fallback over a refused connection, a 503 and a 429, and every backend failing; a rate limit with `Retry-After`, a daily budget refusal with one alert, and the branch's own budget; a person's ceiling narrowing the token; a required egress policy under network-namespace confinement reaching the gateway and not the upstream; a delegated child staying on the gateway within its parent's models, and refused outside them.
- `by` (4 unit, 1 with the built `by`): `--model-gateway` and `by models` parsing; `[models]` defaults and provider keys kept from the harness; key sources through `[secrets]`; rig seats' models; then `[models]` putting a branch on the gateway, `by show`, `by show --json`, `by log`, `by log --json`, `by models`, `by models --json`, `by stats --json`, a flag narrowing a branch, and a bad `[models]` refused by `by config validate`.
- `branchyard-setup` (1): `[models]` parsed, refused and rendered back.
- `branchyard-server` (2): `models` and `ceilings` loaded, the repository's gateway built with keys from the server's secrets, bad ones refused; model calls as metrics.

The confinement test needs a host that allows unprivileged user and network namespaces. Elsewhere it prints `SKIPPED` with the reason, and fails if `unshare -rn true` works but Branchyard's confinement does not.
