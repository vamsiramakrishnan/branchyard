# Model gateway

A harness normally calls its model provider itself, with a key in its environment or a login in its home. On the model gateway it calls Branchyard instead. The gateway speaks the provider's own API, so the harness works unmodified: its base URL points at the gateway, and its API key is the turn's token. The gateway checks the token, picks a backend, adds the real key, streams the answer back and records what the call cost. The key never enters the harness's environment, home or sandbox.

The same token carries the turn's other scopes: its connector grant, its network policy and what it may delegate. What a turn may reach is the intersection of its seat's ceiling, its person's ceiling and what the person approved. See [one scope](#one-scope).

Status, 16 October 2026: built and tested hermetically against mock Anthropic and OpenAI servers on loopback, with a fake harness (a Python script the fake ACP agent runs), including a Claude request translated for an OpenAI backend both ways and the per-call [interception records](#interception-records), and against a mock of [Gemini's OpenAI-compatible endpoint](#gemini-through-the-openai-compatible-endpoint) as strict as the real one was measured to be. Two real harnesses have used it against real providers: DeepSeek Harness 0.2.0-rc.2, provisioned with a `branchyard` provider at the turn's gateway ([provisioning](provisioning.md#harnesses)), completed file-editing turns on `gemini-2.5-flash-lite` through a `gemini` backend (ten calls metered `allowed via gemini (200, …)` with the compatibility layer on) and on `glm-5.3` through a `zai` backend (five calls, the `versioned` layer), on a machine whose outbound TLS goes through an intercepting proxy ([upstream proxies](#what-is-not-done)); and Claude Code 2.1.301 completed the same edit on `glm-5.3` through an `anthropic` backend at Z.ai's endpoint, six calls passed through ([Z.ai](#zai-glm)).

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
# audit = "digests"                      # the default; "contents" also writes each call's request and response to a file

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

[models.backends.gemini]                 # Gemini's OpenAI-compatible endpoint, adapted
api = "openai"                           # by its host (compat = "gemini"); see below
url = "https://generativelanguage.googleapis.com/v1beta/openai"
key = "GEMINI_API_KEY"

[models.backends.zai]                    # Z.ai's GLM Coding Plan (glm-5.3, glm-5.3-flash);
api = "openai"                           # its URL names the API's version, so the path's
url = "https://api.z.ai/api/coding/paas/v4"   # /v1 is dropped (compat = "versioned", by its host)
key = "ZAI_API_KEY"

[[models.routes]]
model = "claude-*"                       # a glob over the requested model; first match wins
backends = ["anthropic", "spare"]        # chosen among by weight
weights = [3, 1]
fallbacks = []                           # tried in order after the weighted ones
requests_per_minute = 120                # per route, in this process

[[models.routes]]
model = "claude-haiku-*"                 # a Claude request served by an OpenAI backend:
backends = ["openai"]                    # translated, and asked for this model
backend_model = "gpt-5-mini"

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

`by init project` offers backends and routes from the provider keys set when it runs, by name (the answer is no unless you say yes, so `--defaults` and `by setup` write none, only a comment pointing here): `anthropic` for `ANTHROPIC_API_KEY`, `openai` for `OPENAI_API_KEY` or `CODEX_API_KEY`, and the `gemini` backend above for `GEMINI_API_KEY`, each with a route by model, and optionally Claude models on Gemini ([setup](setup.md#routing-and-the-gateway-from-what-is-present)).

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
4. **The route.** The first route whose `model` glob matches and that has a backend of the request's API, or, when the route names a `backend_model`, a backend of an API the request [translates](#translation) to. Its `requests_per_minute` is held over a sliding minute in this process: over it, `429` with `Retry-After`. Without a matching route, every backend of that API, in name order; with AIPlex backends among them, only the plain ones when there are any ([the route rule](#the-route-rule)).
5. **The backend.** One of the route's backends chosen by weight (seeded by `[models] seed`, else at random), then its other backends in order, then its fallbacks. The next is tried when a backend refuses the connection, has no key, or answers `5xx` or `429`, unless the answer is an [AIPlex backend's own refusal](#what-a-refusal-looks-like), which is final; one call is never offered to both an AIPlex backend and a plain one ([the route rule](#the-route-rule)). When every one fails, the harness gets the last one's answer, as it would from the provider. A connection that breaks after the request was sent is not retried: the request may have been billed.
6. **The request.** The harness's headers, without its credentials, hop-by-hop headers and `Accept-Encoding`, plus the backend's key (`x-api-key` for Anthropic, `Authorization: Bearer` for OpenAI and for an [AIPlex backend](#what-is-sent) on either API, the configured header for a generic backend) and `Accept-Encoding: identity`, so usage can be read. An OpenAI chat completion that streams is asked for its usage (`stream_options.include_usage`) unless it already says. Nothing else in the body changes, unless the backend speaks another API than the harness (then the body is [translated](#translation)) or has a compatibility layer (then the path and a chat completion's body are adapted, as for [Gemini](#gemini-through-the-openai-compatible-endpoint)).
7. **The response.** Streamed to the harness as it arrives (`Transfer-Encoding: chunked`, `Connection: close`), server-sent events included, while its usage is read as it passes; a translated response is turned back into the harness's API on the way.
8. **The record.** Before the response's last chunk reaches the harness, the call is metered, stored as a usage record, and recorded on the branch as a `model` event, with its [interception record](#interception-records), so the next call is held to what this one cost.

The gateway reads the harness's request and each backend's response with the workspace's one HTTP/1.1 codec, [`branchyard-wire`](../crates/branchyard-wire), which refuses what it cannot frame instead of guessing. A request with both `Content-Length` and `Transfer-Encoding`, a duplicate or non-decimal `Content-Length`, a transfer coding that does not end in `chunked`, a chunk size that is not hexadecimal digits (or overflows), a chunk not followed by CRLF, a body shorter than it said, or a head over 64 KiB is refused `400` and never forwarded. A backend's response broken the same way, or cut off partway through its body, fails the call (`502` before the response starts, a broken-off stream after) rather than passing a short body to the harness. A response with both lengths follows `Transfer-Encoding`, as RFC 9112 says a client must. The corpus of valid and malformed messages in `crates/branchyard-wire/src/corpus.rs` runs against the gateway's request reader, the backend response reader, the SDK client and the sync mock server.

Refusals are in the provider's own error shape (`permission_error`, `rate_limit_error`, `authentication_error`), with `X-Branchyard-Gateway` naming the decision.

### Translation

A route that names a `backend_model` may carry a request to a backend speaking another API: today, Anthropic's Messages API (what Claude Code speaks) onto OpenAI's chat completions (what OpenAI, and the servers that imitate it, speak: vLLM, Ollama, LiteLLM, OpenRouter, each an `openai` backend with its own `url`). So a Claude Code branch can be routed to any such backend:

```toml
[models.backends.local]
api = "openai"
url = "http://127.0.0.1:8000"
key = "LOCAL_KEY"

[[models.routes]]
model = "claude-*"
backends = ["local"]
backend_model = "qwen3-coder"            # what the backend is asked for
```

Coder's anyclaude (MIT, vendored under `vendor/anyclaude`) is the reference for what maps to what; `crates/branchyard/src/models/translate.rs` is the port. What maps:

| Anthropic | OpenAI chat |
|---|---|
| `system` (text or text blocks) | a `system` message |
| a message with role `system` among the messages (text or text blocks; Claude Code 2.1.301 sends one, though Anthropic's API does not document it) | a `system` message in its place |
| `messages`: text, `image` (base64 or URL), `tool_use`, `tool_result` | `content` (string, or text and `image_url` parts), `tool_calls`, `tool` messages placed before the rest of their user turn, as OpenAI requires |
| `tools` with an `input_schema`; `tool_choice` `auto`, `any`, `none`, `tool`; `disable_parallel_tool_use` | functions; `auto`, `required`, `none`, that function; `parallel_tool_calls: false` |
| `max_tokens`, `stop_sequences`, `temperature`, `top_p`, `metadata.user_id`, `stream` | `max_completion_tokens`, `stop`, the same, `user`, `stream` with `stream_options.include_usage` |
| `thinking` (by `budget_tokens`) | `reasoning_effort` `low` under 4096, `medium` under 16384, else `high` |
| a completion's `content`, `tool_calls`, `reasoning_content`; `finish_reason` `stop`, `length`, `tool_calls`, `content_filter` | text, `tool_use` (arguments parsed) and `thinking` blocks; `stop_reason` `end_turn`, `max_tokens`, `tool_use`, `refusal` |
| a stream's chunks | `message_start`, a `content_block_start`/`delta`/`stop` per text, thinking or tool block, `message_delta` with the stop reason and usage, `message_stop` |
| `usage`: `prompt_tokens`, `completion_tokens`, `prompt_tokens_details.cached_tokens` | `input_tokens` (uncached), `output_tokens`, `cache_read_input_tokens` |
| an error body | `{"type": "error", "error": {"type", "message"}}`, the type by the status, the status kept |

Dropped: `thinking` blocks on the way out (the backend never saw them), `top_k`, server tools (no `input_schema`), Anthropic's own headers (`anthropic-version`, `anthropic-beta`). Refused `400`: a `document` block, an unknown block type, a message with a role other than `user` or `assistant`. Not translated, so a route with only such a backend answers `502` for them: `/v1/messages/count_tokens` and the rest of Anthropic's API. A `thinking` block comes back without a signature, so it cannot be sent back to Anthropic in a later turn. The response's `model` is the one the harness asked for; the call's `translated` names the one the backend served, and usage is priced by that model (the usage row records it).

A route without `backend_model` never crosses APIs: a request's API must match the backend's, as before. A harness speaking an API no backend speaks runs [direct](#subscription-logins-and-direct-mode) unless a translating route covers one of its allowed models.

### Gemini through the OpenAI-compatible endpoint

Google serves Gemini on an endpoint that imitates OpenAI's chat completions, `https://generativelanguage.googleapis.com/v1beta/openai`, with the Gemini API key as its bearer token. One backend puts it behind the gateway for every harness that speaks OpenAI's chat completions (DeepSeek Harness, OpenCode, Codex with a custom provider), and, through [translation](#translation), for Claude Code:

```toml
[models.backends.gemini]
api = "openai"
url = "https://generativelanguage.googleapis.com/v1beta/openai"
key = "GEMINI_API_KEY"                   # a [secrets] entry, else this variable
# compat = "gemini"                      # implied by the host; "none" turns it off

[[models.routes]]
model = "gemini-*"                       # an OpenAI harness asking for gemini-3.5-flash
backends = ["gemini"]

[[models.routes]]
model = "claude-*"                       # Claude Code on Gemini, translated
backends = ["gemini"]
backend_model = "gemini-3.5-flash"
```

The harness is set up as for any OpenAI backend (`OPENAI_BASE_URL` and `OPENAI_API_KEY` from the gateway) and asks for a `gemini-*` model; Claude Code asks for its own and is answered by `backend_model`. The catalog does not price Gemini: without `[models.prices."gemini-*"]` (dollars per million tokens, from Google's price list) its calls are recorded unpriced, free to a cost limit, while token budgets still count them.

The endpoint differs from OpenAI's in two ways that break those harnesses, so the backend gets a compatibility layer: `compat = "gemini"` on `[models.backends.NAME]`, on by default for an `openai` backend whose URL's host is `generativelanguage.googleapis.com`, and settable for one that is not (a proxy in front of the same endpoint). `compat = "none"` turns it off; every other backend is sent what it always was. With it, on that backend:

| | What the gateway does |
|---|---|
| **The path** | The endpoint's base URL stands for OpenAI's `/v1`: a harness's `/v1/chat/completions` is sent as `/v1beta/openai/chat/completions`, `/v1/models` as `…/models`. |
| **Fields it rejects** | The endpoint refuses a field it does not know: `"store": false`, which DeepSeek Harness sends, is answered `400` `Unknown name "store": Cannot find field.` A chat completion is sent without `store`, `metadata`, `prediction`, `prompt_cache_key`, `safety_identifier` and `user`. Only `store` was seen refused; the rest are OpenAI's own platform features (stored completions, predicted outputs, prompt-cache routing, end-user and safety identifiers), which neither [Google's page for the endpoint](https://ai.google.dev/gemini-api/docs/openai) nor the [parameters its Vertex AI counterpart supports](https://cloud.google.com/vertex-ai/generative-ai/docs/migrate/openai/overview) name, and which Gemini has nothing to act on, so stripping them loses nothing. A translated Claude request carries `user`, from Claude Code's `metadata.user_id`. Fields Gemini can act on are left as they are. The list is `UNSUPPORTED` in `crates/branchyard/src/models/gemini.rs`. |
| **Thought signatures** | Gemini 3 models (`gemini-3.5-flash` and the rest) sign the first tool call of each step with an encrypted record of their reasoning, `"extra_content": {"google": {"thought_signature": "…"}}` on the call, in a whole answer and in a stream's delta, and refuse a request that replays a step of the current turn without it: `400` `Function call is missing a thought_signature in functionCall parts`. A harness that speaks OpenAI's API does not keep a field OpenAI does not define. So the gateway keeps each signature as the answer passes, by the call's id and the person the turn acts for (a stream's as each event arrives, a whole answer's before its last byte reaches the harness), and when a request replays an assistant message's `tool_calls`, puts the kept signature on each call that has none. A step's first call that still has none gets Google's documented placeholder, `skip_thought_signature_validator`, which the endpoint accepts where no signature is refused, at some cost to the model's reasoning ([thought signatures](https://cloud.google.com/vertex-ai/generative-ai/docs/thought-signatures)). The step's other calls are left unsigned, as Gemini sends them, and a call the harness kept its signature on is left as it is. |

A Claude request translated for the endpoint gets the same. A tool call's id is the id of the `tool_use` block Claude Code is given, and Claude Code sends it back as the `tool_use` id that becomes the call's id again, so the kept signature is found by it. The signature itself never reaches Claude Code.

### Z.ai (GLM)

Z.ai serves its GLM models on three endpoints, each for the same key: Anthropic Messages at `https://api.z.ai/api/anthropic`, OpenAI chat completions at `https://api.z.ai/api/coding/paas/v4` (the GLM Coding Plan) or `https://api.z.ai/api/paas/v4` (the standard, pay-as-you-go API), and OpenAI Responses at `https://api.z.ai/api/v1`. A Coding Plan key and a standard key are not interchangeable: each answers `401` on the other's chat endpoint. The OpenAI-shaped URLs already name the API's version, so a harness's `/v1/chat/completions` cannot be appended to them; an `openai` backend whose URL's host is `api.z.ai` gets the `versioned` compatibility layer by default (`compat = "versioned"` sets it for a proxy in front of the same endpoints; `compat = "none"` turns it off), which drops the path's leading `/v1` and changes nothing else. The Anthropic endpoint is an ordinary `anthropic` backend, with no layer:

```toml
[models.backends.zai]                    # OpenAI chat completions, for Codex, OpenCode, DeepSeek Harness
api = "openai"
url = "https://api.z.ai/api/coding/paas/v4"
key = "ZAI_API_KEY"

[models.backends.zai-anthropic]          # Anthropic Messages, for Claude Code
api = "anthropic"
url = "https://api.z.ai/api/anthropic"
key = "ZAI_API_KEY"

[models.backends.zai-responses]          # OpenAI Responses, for Codex
api = "openai"
url = "https://api.z.ai/api/v1"
key = "ZAI_API_KEY"

[[models.routes]]
model = "glm-*"
backends = ["zai"]                       # one backend per route: pick the one the harness's API needs
```

A route names one backend (with fallbacks of the same kind), so a yard serving several harnesses on GLM gives each its route by model name: the harnesses send the model they are given, and `--model glm-5.3` on every candidate would meet the same route. Give Codex and DeepSeek Harness distinct names where both run (`glm-5.3` and `glm-5.3-flash`, say), or run them from configurations of their own.

`by init project` proposes the `zai` backend and the `glm-*` route when `ZAI_API_KEY` is set. Oh My Pi reads the same variable for its `zai` provider (`--model zai/glm-5.3`) and needs no gateway for it.

**Without a gateway.** One harness on GLM needs no gateway either: `printf %s "$KEY" | by harnesses login claude-code --provider zai` (or `qwen-code`, `codex`, `pi`, `oh-my-pi`, `opencode`) stores the key once as `ZAI_API_KEY` and records the login, and an isolated or sandboxed branch of that harness gets the endpoint above that its API needs, the key in the variable it reads and a GLM model ([harness lifecycle](harness-lifecycle.md#through-a-gateway-provider)). The harness then calls Z.ai itself: no metering, routes, budgets or interception records, which are the gateway's. A branch on the gateway gets no provider login.

**Status, 16 October 2026.** Run against the real endpoints with a GLM Coding Plan key (a key created under the plan: a standard key of the same account listed the models but answered every completion `429` `1113 Insufficient balance or no resource package`, since the two kinds of key are not interchangeable):

- `by models list` lists the `zai` source: 11 models, `glm-4.5` to `glm-5.3-flashx`.
- **DeepSeek Harness on `glm-5.3` through a `zai` backend** (`api = "openai"`, the Coding Plan chat endpoint, the `versioned` layer implied by the host): `by run --harness deepseek-harness --isolated --model-gateway --model glm-5.3` completed a one-line README edit in five calls, each metered `allowed via zai (200, …)` and sent to `/chat/completions` under the base URL.
- **Claude Code on `glm-5.3` through a `zai-anthropic` backend** (`api = "anthropic"`, `https://api.z.ai/api/anthropic`, a route `glm-*` to it): the same edit in six calls (`Bash`, `Read`, `Edit`, `end_turn`), passed through with the key in `x-api-key`. Routed to the `openai` backend with `backend_model = "glm-5.3"` instead, so that the [translation](#translation) carries it: the first request failed on a message with role `system` among Claude Code's messages, which the translation now carries as a system message in place, after which the same edit completed in three translated calls (`allowed via zai as glm-5.3`); Claude Code's `POST /v1/messages/count_tokens` is answered `502` `not translated` on an OpenAI backend, twice per turn, and Claude Code goes on without it.
- **Codex on `glm-5.3` through a `zai-responses` backend** (`api = "openai"`, `https://api.z.ai/api/v1`, Z.ai's Responses endpoint, the `versioned` layer by its host): the same edit in four calls (`exec_command` three times, then `completed`), each `allowed via zai-responses (200, …)`, sent to `/responses` under the base URL. Codex reaches the gateway through the `[model_providers.branchyard]` entry its [provisioner writes](provisioning.md#harnesses) (`wire_api = "responses"`, the only one codex-cli 0.157.1 accepts); before that entry existed, Codex ignored the gateway's environment and called `api.openai.com` with no key. Codex warns `Model metadata for glm-5.3 not found` and runs on fallback metadata.
- **Oh My Pi on `zai/glm-5.3`** needs no gateway: `ZAI_API_KEY` alone, and the `oh-my-pi-acp` profile qualified 9 of 9 on it in 67 seconds ([qualification](qualification/README.md#results-16-october-2026)).

GLM-5.3 spends its first tokens on reasoning (`reasoning_tokens` in the usage; a `max_tokens` of 20 returns no text), which the gateway's metering records as output tokens.

The signatures are kept in the process that runs the turns, by the repository's gateway, so a conversation keeps them from turn to turn: at most 4096 of them, and 32 MiB together, the least recently used forgotten first. What the layer changed is logged once per call at debug level (`BRANCHYARD_LOG=branchyard::models=debug`), with the backend and model: `gemini compatibility: stripped store; thought signatures attached: 1 recorded, 0 placeholder`. The log names fields and counts calls; it never holds a signature.

Limits:

- **A conversation started outside the gateway has no recorded signatures**, and nor does one the gateway forgot, or whose earlier calls another process served (another `by` process, server or worker, or this one before a restart). Its steps get the placeholder: the requests succeed, and the model works without the reasoning the real signatures carried. So does a streamed call that came without an id (translated, the gateway names it `toolu_N`).
- **Only chat completions are adapted**; any other path only loses its `/v1`.
- **The signature on an answer's text** (Gemini may sign the last text part of an answer that calls no tool) is not kept: Google documents leaving it out as allowed.
- **What was measured.** On 11 October 2026, against the endpoint itself: `store` refused, signatures returned and accepted back in `extra_content.google`, the placeholder accepted where no signature was refused. The other stripped fields, the `/v1` path and the whole layer were tested against a mock of the endpoint, not the endpoint.

### Usage and cost

Tokens come from the provider's own usage fields:

| API | Fields | Streamed |
|---|---|---|
| Anthropic Messages | `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens` and `cache_creation.ephemeral_1h_input_tokens` | `message_start` for the input side, the last `message_delta` for the output |
| OpenAI chat completions | `prompt_tokens` (with `prompt_tokens_details.cached_tokens`), `completion_tokens` | the final chunk's `usage` |
| OpenAI responses | `input_tokens` (with `input_tokens_details.cached_tokens`), `output_tokens` | `response.completed`'s `response.usage` |

A cached input token is counted once, as a cache read. Cost is `[models.prices]` for the model (the longest matching glob), else `catalog/pricing.toml`, the table `by usage` estimates with: Claude's long-context tier and one-hour cache writes, OpenAI's cached-input and long-context rates. A model neither prices is recorded with no cost and counted as unpriced.

A usage record has the time, branch (`<repo>/<branch>` on a server), turn, person, model, API, backend, the four token counts, cost, latency, status and whether it streamed. It is kept in the repository's store (SQLite or PostgreSQL) when its branch is removed: it is the repository's spending, which period budgets count.

A branch on the gateway costs exactly what its calls cost: each turn adds its calls' metered cost to the branch's `cost_usd`, and the harness's own estimate is not used. While a turn runs, its metered cost is written to the branch's record as the engine sees each call end (within a tenth of a second), so its parent, the cost limits above it and `by show` see it as it is spent, and its limit is held on the metered cost, as it is on a harness's estimate otherwise. A turn recovered after its engine stopped adds the calls it recorded to what the branch had spent before the turn, not to the figure the running turn had written.

### Interception records

Each call that reached a backend carries an interception record in its `model` event: who asked (the event's branch and turn, and the usage row's `subject`), what was offered and what was called, how big each side was, and a digest of the request. It holds counts, names and one hash, never the text of a prompt or an answer. It is computed as the bytes pass, whatever the provider and whether the response streamed:

| Field | What |
|---|---|
| `request_bytes` | The size of the request body the harness sent. |
| `response_bytes` | The size of the response body the backend sent, counted as it passed. For a [translated](#translation) call it is the backend's body, before it is turned back into the harness's API. |
| `prompt_digest` | The BLAKE3 hash of the request body, 64 lowercase hex digits. It is of the body exactly as the harness sent it: the bytes after the HTTP framing is removed (a chunked body is joined) and before the gateway changes anything, so it does not cover the `stream_options.include_usage` the gateway adds to an OpenAI chat stream, or a translated body. The bytes are hashed, not the JSON they mean, so the same request always has the same digest and the same request spaced differently has another. A request with no body has the hash of the empty input. |
| `messages` | The items in the request's `messages` (Anthropic, OpenAI chat) or `input` (OpenAI responses; a string is one item). 0 when it has neither, or is not JSON. |
| `tools_offered` | The names in the request's `tools`, in order: Anthropic's `name`, chat's `function.name`, the responses API's `name`. A tool with none, such as OpenAI's built-in `web_search_preview`, is named by its `type`. |
| `tool_uses` | The tools or functions the response called, in order, once per call. Anthropic: `tool_use` blocks of `content[]`, or, streamed, `content_block_start` of such a block. OpenAI chat: `choices[].message.tool_calls[]`, or, streamed, `choices[].delta.tool_calls[]`, a call counted once, by the delta that carries its name. OpenAI responses: `output[]` items of type `function_call` (and `custom_tool_call`, which Codex's `apply_patch` is), or, streamed, `response.output_item.done`. |
| `stop_reason` | Why the response stopped, in the provider's own word: Anthropic's `stop_reason` (`end_turn`, `tool_use`, `max_tokens`), OpenAI chat's `finish_reason` of the first choice (`stop`, `tool_calls`, `length`), the responses API's `status` (`completed`, `incomplete`). Absent when the response gave none, such as an error body or a stream cut off. |

The response is read in the backend's shape, not the harness's: for a call translated for an OpenAI backend, `tool_uses` and `stop_reason` come from the chat completion the backend sent (`tool_calls`, not Anthropic's `tool_use`), while `tools_offered`, `messages` and `prompt_digest` are of the Anthropic request the harness sent.

`by log` adds the tools a call used and why it stopped when it has them: `model: claude-sonnet-4-6 allowed via anthropic (200, 1210 in / 80 out, $0.0048, tools: Read, Edit, stop: tool_use, 812 ms)` (each tool once, in the order first called). `by log --json` has all of it as `model.interception`, and `by show --json` totals the calls as `models.tool_uses`.

Which calls have one: every call forwarded to a backend, whatever status it got, and a call every backend failed (the record is of the last backend's answer, at most the 1 MiB the gateway keeps of it). A call the gateway refused itself (`401`, `403`, `429`, no backend, a request it cannot translate), and one whose backend connection broke before any response started (`502`), has none; its decision and reason are on its event.

#### The `audit` key

```toml
[models]
audit = "digests"        # the default, and what leaving it out means
# audit = "contents"     # also keep each call's request and response
```

`digests` writes nothing but the record above. `contents` also appends one JSON line per forwarded call to `.branchyard/models/audit/<branch>/<turn>.jsonl` in the repository (on a server, the served repository), after the response has passed and before the call is recorded:

```json
{"at_ms": 1790000000000, "model": "claude-sonnet-4-6", "backend": "anthropic", "request": {...}, "response": {...}}
```

- `request` is the request body as the harness sent it, as JSON (a body that is not JSON is its text; no body is `null`).
- `response` is the body the backend sent, as JSON when it parses, else its text; for a stream (server-sent events) it is the raw text of the stream, not parsed. A response over 64 MiB is cut there and the line says `"response_truncated": true`.
- `at_ms` is when the call ended (milliseconds since the epoch). Headers are never written, so neither the turn's token nor the backend's key is.
- The directories are created 0700 and the file 0600. A turn's calls are appended to its own file, so a branch has one file per turn. The file is never in the event log: the call's event gets `audit_file` (the path from the repository root) and the digests, nothing more. If the line cannot be written, the call goes on and its `reason` says so.
- Branchyard does not remove the files with the branch. Delete `.branchyard/models/audit/` to discard them.

Any value other than `digests` and `contents` is refused when the configuration is read, naming the value and the two there are (`by config validate`); on a server, the configuration file takes the same key as `"audit"`. `by models` shows which is set.

#### What is not recorded

- **Text, by default.** In `digests` mode no prompt, answer, tool input or tool result is kept anywhere; names of tools are, their arguments are not.
- **A hash of the response**, and the model's reasoning text. The digest is of the request only.
- **Tool use the provider ran itself** (Anthropic's `server_tool_use`, OpenAI's `web_search_call` and the like) and the deprecated chat `functions`/`function_call`: they are not counted in `tool_uses`. A built-in tool in the request is named in `tools_offered`.
- **Anything about a refused call's request** beyond the decision and reason of its event.
- **Tamper evidence.** The lines are not signed or chained, and the digests are in the branch's event log, which a person with write access to the repository's `.branchyard` directory can change.

Privacy: `contents` writes prompts, the source files and tool results a harness put in them, and the answers, as plain text, unredacted, readable by the user Branchyard runs as (and root). Whatever secret a harness read into a prompt is in the file. Keep it off shared disks, out of backups you do not trust and out of any support bundle, and turn it on for a reason. The digest in `digests` mode is an unsalted hash of the request: it proves that a request was this one to anyone who has the request, and says nothing otherwise, but a short or predictable prompt can be guessed from it, so do not treat it as anonymization.

### Budgets

- **The branch's.** `--budget-usd`, a delegated child's limits, and what its children hold, as for any branch ([budgets](delegation.md#budgets)). **A call that would start with the branch's spend already at or over the limit is refused** (`403`, nothing forwarded). The spend is the branch's cost before the turn, what its children hold (read for each call, since the branch may have spawned them in this very turn) and the turn's calls so far, each counted when it ends. A call already running is not cut short and is counted when it ends, so **the branch can end over by at most the calls in flight when it reached the limit**: one call for a harness that calls one model at a time, more for one that calls in parallel. When a call's end takes the spend to the limit, the engine also interrupts the turn (at its next look, within a tenth of a second), so a harness is not left to make calls that will all be refused. The harness is not given a spending limit of its own (Claude Code's `--max-budget-usd`, [harness integration](harness-integration.md)): the gateway holds the branch's limit on the metered cost, and the harness's estimate is not used.
- **Above the branch's.** The cost limit of every branch above it bounds the tree under that branch. Once that tree has spent the limit, a call is refused with `the cost limit above this branch is spent (meta and everything below it has spent $… of its max_usd (--budget-usd) $…)`, although the branch's own limit has room: a sibling's overshoot, or the parent's own spending, used what the branch was given ([what a cost limit guarantees](delegation.md#what-a-cost-limit-guarantees)). The tree's spend is the sum of its branches' costs, and a branch on the gateway writes its metered cost to its record within a tenth of a second of each call's end, so the calls of the branch making the call are counted exactly and another branch's latest call within that moment is not.
- **The repository's.** `[models.budget]`: daily and monthly (UTC) cost and tokens over every branch, removed ones included, read from the stored usage before each call. A call is refused when a limit is reached. When a call takes spending past `alert_at` of a limit (default 0.8), a `model budget alert` event is recorded, once per period in each process.

## AIPlex as the backend

An AIPlex access point (`{AIPLEX}/ap/{id}`) is a control plane in front of the providers: it verifies who is calling, applies its admission and quota, forwards in the provider's own API and writes a ledger record. It can be a backend of the gateway, so a turn's calls go harness → the turn's gateway → AIPlex → provider. The harness never holds the AIPlex credential nor sees AIPlex's URL, and AIPlex's refusal, or AIPlex being unreachable, is never worked around by sending the call to a provider directly. The integration contract between the two products fixes the names below; the Branchyard side is this section.

### Configuring it

```toml
[secrets]
aiplex = "AIPLEX_TOKEN"                      # a variable, or "@path", as for any key

[models.backends.aiplex-anthropic]
api = "anthropic"
url = "https://aiplex.example.com/ap/ide"
key = "aiplex"
control_plane = "aiplex"                     # this backend is an AIPlex access point

[models.backends.aiplex-openai]
api = "openai"
url = "https://aiplex.example.com/ap/ide"
key = "aiplex"
control_plane = "aiplex"

[[models.routes]]
model = "claude-*"
backends = ["aiplex-anthropic"]

[[models.routes]]
model = "gpt-*"
backends = ["aiplex-openai"]
```

One backend per API, both at the access point's URL: AIPlex serves Anthropic's Messages API and OpenAI's chat completions, responses and model list under it, and does not translate between them. `control_plane` takes one value, `"aiplex"`; any other is refused when the configuration is read, naming the backend, the value and `"aiplex"`. Such a backend must be `anthropic` or `openai` (not `generic`), must have a `key` (the AIPlex token: a person's from `aiplex auth print-token`, an agent's, or the developer token of `aiplex dev`), and takes no `compat`. On a server, the `"models"` table takes the same key. `by config validate` checks all of it, `by models` shows `control plane aiplex` beside the backend (`control_plane` in `--json`), and `by models list` lists its models with the token as a bearer.

**The `url` is required, and it is where the credential goes.** An AIPlex backend has no default address. A plain `anthropic` or `openai` backend that names no `url` is the provider's public API, and an AIPlex backend read the same way would send every call there with the AIPlex token, or a token the server signed, as its bearer. So an AIPlex backend without a `url` is refused when the configuration is read, naming the backend and what a `url` looks like (`url = "https://<aiplex>/ap/<access point>"`): by `by config validate` and the server's configuration file, and again by the engine when it builds the gateway (`models::backend_url`, the one place a backend's address is decided; `by models list` reads the same function, so it never lists such a backend at the provider either). A `url` that is empty, has whitespace in it or names no host is refused the same way. A `url` that is written out is the operator's choice and is used as written, whatever host it names: `url = "https://api.anthropic.com"` on an AIPlex backend is accepted, and sends the AIPlex token to Anthropic. Over plain `http://` the token crosses the network unencrypted; that is accepted too, without a warning (a development AIPlex on loopback, an AIPlex behind a private network's own encryption), so write `https://` anywhere else.

### The route rule

A control plane that can be stepped around is not one, so the gateway never offers one call to both kinds of backend:

- **A route that names a control-plane backend names only control-plane backends**, in `backends` and `fallbacks`. A route that mixes them is refused when the configuration is read, naming the route and both backends: otherwise a refusal or an outage of AIPlex would move the call to a provider directly. The weighted choice, a route's other backends and its fallbacks are then all AIPlex backends, and so is the backend a [translating](#translation) route offers (a `backend_model` route to `aiplex-openai` is sent to AIPlex's chat completions).
- **A call that no route takes** (its model matches no route, or it names none, such as `GET /v1/models`) goes to the plain backends of its API when it has any, in name order, exactly as it did before control planes existed, and to the API's AIPlex backends only when the API has no plain backend. It never goes to both.
- **A control-plane backend that no route names**, in a configuration whose same API also has a plain backend, would never be used by that rule, so it is refused when the configuration is read, naming the backend and saying to add a route that names it: nobody should believe calls are governed when they are not.
- The rule is held again at run time, whatever the routes say (`Gateway::confine`): a list of backends for one call that holds both kinds is cut to its AIPlex backends for a route and to its plain backends for a call no route takes. The pass-through path `/backends/NAME/...` names one backend, and never an AIPlex one: it is refused `404` (`NAME is a control-plane backend, which the pass-through path never reaches`) by a check of its own, before anything is sent.

A route that sends a model to a plain backend stays the operator's to write: `[[models.routes]] model = "o3*" backends = ["openai"]` beside the AIPlex routes sends `o3` directly. And a harness whose API no backend serves runs [direct](#subscription-logins-and-direct-mode), as before: give a yard that must send every call through AIPlex an AIPlex backend for each API its harnesses speak, and an [egress policy](egress.md) that allows only the gateway.

### The credential

The backend's `key` is read as any backend's is: a `[secrets]` entry naming a variable or an `@path` file, else a variable of that name. The harness never gets it. A variable that holds an AIPlex backend's key (`AIPLEX_TOKEN` in the example above, which is what AIPlex's `connect` command writes) is taken out of the harness's environment for every turn on the gateway, with the provider credentials the gateway always takes out. A plain backend's variable is left as it was: a harness may read its own provider's variable itself, and nothing changes for a configuration without an AIPlex backend.

The same variable is kept out of every other child the engine starts for a turn or its preparation: the repository's setup and teardown commands, its check, git (whose hooks Branchyard's own git commands run with off), the connector gateway, the tool store's installs and the connector packager. They are all made by one constructor, `branchyard_support::child::command`, which removes every variable a gateway with an AIPlex backend registered when it was built (`child::hide`); the `by` process itself keeps it, since the gateway reads the key from it for each call. The two places that capture this process's environment and replay it onto a cleared one leave the same variables out: `shell_setup`, whose captured environment reaches setup and the harness, and the runtime's placement environment, which every harness starts with, whether or not its branch is on the gateway. The recipe provider's commands and its ssh and exec transports (`branchyard-recipe`) go through the constructor too.

The hidden set belongs to the process and only grows: a variable one yard's AIPlex backend names is hidden from every child of every yard that process serves, for as long as it runs. So the token's variable may not be one a child needs or Branchyard reads: a backend whose key would be read from `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `PWD`, `TMPDIR`, `TMP`, `TEMP`, `TERM`, `LANG`, `LANGUAGE`, `LC_*`, `SSH_AUTH_SOCK`, `GIT_*`, `*_PROXY`, `NO_PROXY`, `SSL_CERT_*`, `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE` or `BRANCHYARD_*` (in any letter case, directly or through `[secrets]`) is refused when the configuration is read, naming the backend and the variable: give the token a variable of its own. An AIPlex backend's key must be at least 16 characters (it is the token, and a shorter one would be found in answers that do not hold it); a shorter key is not used, and the call's `failed_over` names the backend, never the value.

Not routed through that constructor, so a hidden variable still reaches them: the processes the `by` command line and the server start themselves (`branchyard-cli`, `branchyard-server`, `branchyard-sync`: commands a person or operator runs, not a turn's), and the tool processes the sandbox providers run to start a sandbox (`branchyard-container`, `-microsandbox`, `-agent-sandbox`, `-substrate`: Branchyard's own tools, not the repository's code; whether each passes a variable into the sandbox has not been checked). The harness itself is started with an explicit environment from which the variable is removed per turn.

Only the AIPlex key's variable is removed. Anything else sensitive in the environment of the process that runs the turn, such as a database URL, reaches a local harness as it always did.

A copy of the token under another name is such a thing, and Branchyard does not know it is one: it goes by the variable the backend names, not by the value. AIPlex's own development setup makes copies: `eval "$(aiplex dev env)"` exports the developer token as `AIPLEX_TOKEN` and, for the SDKs, as `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` and `GEMINI_API_KEY` too (which ones depends on the provider `aiplex dev` runs), with their base URL variables pointed at the access point. Run from that shell, a harness on the gateway starts without `AIPLEX_TOKEN` and with `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` replaced by the turn's own token for the APIs the gateway serves, but it inherits `GEMINI_API_KEY`, and the key variable of an API the gateway does not serve, with the AIPlex token in it; a plain backend whose `key` names one of those variables is sent it at its own `url`; and [`by models list`](#listing-models) sends it to the providers those variables are named for. None of that is checked. Run `by` from a shell that holds the token only in the variable the AIPlex backend names.

That is a scrub of the environment, not a boundary. A local turn's harness runs as the same user as `by`: it can read an `@path` key file, and the environment of its parent process in `/proc`. `--isolated` gives the harness a scrubbed environment and a private `HOME`, still as the same user, so the same holds. A real boundary is a [sandboxed](agent-sandbox.md) turn, whose harness runs in a sandbox that holds neither the file nor the process; a sandboxed turn reaches the gateway at `[models] sandbox_host` ([where it runs](#where-it-runs)). A server's turns run in the server's process, with the same rule per placement. On a server, an AIPlex backend can instead be called with [a token Branchyard signs](#a-token-branchyard-signs) for each turn, and then no AIPlex secret is stored at all.

### What is sent

For each call to an AIPlex backend, in the one place the gateway builds a backend's headers:

| Header | Value |
|---|---|
| `Authorization` | `Bearer <key>`, on both APIs; never `x-api-key`. The harness's own `Authorization`, `x-api-key` and `api-key` are dropped, as for any backend. |
| `X-Client-Request-Id` | The id of the call's usage record, made before the call is forwarded (16 random bytes, base64url: 22 of `A-Z a-z 0-9 - _`), the same on every AIPlex backend the call is tried on. AIPlex stores it as `client_request_id`. |
| `X-AIPlex-Tags` | `by_branch=<branch>,by_turn=<turn>,by_harness=<harness>`: the branch as its usage record names it (`<repo>/<branch>` on a server), the turn's number, the harness's catalog id. A value that is not 1 to 64 of `A-Z a-z 0-9 _ . : / @ + ( ) -` and space is left out with its key, never cut short; keys are dropped from the end while the header is over 512 bytes; no header when nothing is left. |
| `traceparent` | The turn's W3C trace context, the one its harness gets as `TRACEPARENT`, when the turn has one and it is a well-formed version-`00` context; otherwise none. The gateway makes no span of its own. A turn has one when a server runs it (the operation's span) or when it is a delegated child of a traced turn (its parent's span, with tracing on); a top-level local `by run` turn has none, so AIPlex starts a trace of its own for each of its calls. |

Of the harness's own headers an AIPlex backend is sent only these, as the harness sent them, in any letter case: `Content-Type`, `Accept`, `User-Agent`, `anthropic-version`, `anthropic-beta` (AIPlex refuses a beta value its access point does not list, so it goes exactly as sent), `OpenAI-Beta` and `Idempotency-Key` (`CONTROL_PLANE_REQUEST_HEADERS` in `crates/branchyard/src/models/gateway.rs`), each only when its value is at most 256 bytes (a longer one is dropped). A request whose body is in a content coding (`Content-Encoding` other than `identity`) is refused `400` when it would go to an AIPlex backend: the gateway must read the body for its model, and sends a control plane only a body it read. Everything else the harness sent is dropped, so it cannot speak for itself: not `X-Client-Request-Id`, `X-AIPlex-*`, `traceparent`, `tracestate` or `X-Request-Id` (AIPlex would take it as its own request id), not an identity AIPlex reads ahead of `Authorization` (`X-Goog-Iap-Jwt-Assertion`), not a client address (`Forwarded`, `X-Forwarded-For`, `X-Real-IP`), not a cookie, key or tenant header of any name. A header folded over continuation lines is refused `400` with the rest of a malformed request, by the [wire codec](#each-call). No person's name or email is sent. A plain backend is sent exactly what it was before: the harness's headers, these included, less what the gateway always drops.

**Only the integration's routes.** An access point serves more under its URL than this integration uses: its other wires (Gemini's and Bedrock's, whose paths name the model, which the turn's scope does not see and whose usage Branchyard does not read), embeddings, moderation and media routes, a stored response or one model by id, its tools endpoint (`/mcp`, JSON-RPC over `POST`) and its skill and server registries. The harness's path would take the backend's credential to any of them. So an AIPlex backend is asked only these, by method and exact path (`CONTROL_PLANE_ROUTES` in `gateway.rs`):

| Backend | Routes |
|---|---|
| `api = "anthropic"` | `POST /v1/messages`, `POST /v1/messages/count_tokens`, `GET /v1/models` |
| `api = "openai"` | `POST /v1/chat/completions`, `POST /v1/responses`, `GET /v1/models` |

A query is the API's own and passes (`/v1/messages?beta=true`). Any other request that would go to an AIPlex backend, by a route or because no route takes it, is refused by the gateway with its own `403` before anything is sent, its event's decision `denied`: `POST /mcp is not a route a control-plane backend is asked (POST /v1/messages, POST /v1/messages/count_tokens, GET /v1/models)`. The path is compared as written and never normalized, so `/v1/messages/`, `//v1/messages` and `/v1/messages/../mcp` are refused too; the refusal shows the method and path only when they are short and printable, since the harness chose them. A plain backend is sent whatever path the harness asks for, as before.

AIPlex keeps a tag only for keys its operator allowed (`AIPLEX_TAG_KEYS_ALLOWED=by_branch,by_turn,by_harness`) and stores at most `AIPLEX_TAG_MAX_VALUES` distinct values per key (default 50; then `(other)`): tags are labels for grouping, and `X-Client-Request-Id` is the join.

### What is recorded

From each answer of an AIPlex backend the gateway reads `X-Aiplex-Request-Id` (AIPlex's request id, which is the `request_id` of its ledger record when the call wrote one: a call AIPlex refused before admission, such as one with no credential, writes none; kept when it is 1 to 128 of `A-Z a-z 0-9 - _ . :`) and `X-Aiplex-Error-Code` (kept when it is 1 to 64 of `A-Z 0-9 _`). A value with an ESC byte or a raw newline breaks the response head, and the call fails `502` with Branchyard's own error; a byte that is not UTF-8 is read as U+FFFD, outside the alphabet, and not kept. The call's `model` event carries `control_plane` from the moment an AIPlex backend is first tried, so a call refused at connection, cut off after it was sent, or timed out still names the id it was sent under: `plane` (`aiplex`), `call_id` (the id sent as `X-Client-Request-Id`), `request_id` and `error_code` when an answer gave them; a backend tried before the one that answered keeps AIPlex's id in `failed_over` (`aiplex-anthropic: 503 (aiplex request req-17)`). `by log` ends the call's line with AIPlex's id, then the call's own (`aiplex request req-17, call AAECAwQFBgcICQoLDA0ODw`), so a `client_request_id` found in AIPlex's ledger leads to the Branchyard call; `by log --json` has the fields. Nothing changes in the stored usage table: the usage record's `id` is the `call_id`.

So a call has two records: Branchyard's (its usage record and `model` event, metered from the answer and priced from Branchyard's own tables, which its budgets count, as for any backend) and AIPlex's ledger record, joined by the call's id (`client_request_id` there) and AIPlex's `request_id` (on the event). AIPlex's record is the one a team is charged from; Branchyard's budgets keep holding the branch and the repository to what Branchyard metered. A call every backend failed has no usage record, as before; its event's `control_plane` carries the `call_id` it was sent under.

### What a refusal looks like

An answer that carries `X-Aiplex-Error-Code` is AIPlex's decision. AIPlex sets it whenever it wrote the body itself: its refusals, and its own availability answers when it could not reach a provider or no backend of a routed model was available, by which time it has done its own retries. Whatever its status (`401`, `403`, `404`, `429`, `503`, …) it is returned to the harness as it is (status, body, `Retry-After`), streamed or not, and no other backend is tried. Its event's decision is `refused`, and `by log` reads:

```text
model: claude-sonnet-4-6 refused via aiplex-anthropic (429, refused by aiplex (QUOTA_BUDGET_EXHAUSTED), 4 ms, aiplex request req-17, call AAECAwQFBgcICQoLDA0ODw)
```

`by show` counts it as `refused 1 refused`. The header present with a value that does not fit is still final, its reason `refused by aiplex (an error code that is not one)`. An answer without the header (the provider's own error relayed by AIPlex, a `5xx`, a refused connection) follows the [rules above](#each-call), which can now only move the call to another AIPlex backend. A backend that goes quiet after the call was sent (ten minutes by default, `Gateway::quiet_after`) fails the call `502` and no other backend is tried, since the call may have been billed; the reason says so, for how long and which backend (`aiplex-anthropic: timed out: no answer for 10m`).

Whatever an AIPlex answer says, the harness gets only what it needs of it:

- **Headers by allow-list.** Of an AIPlex answer's headers the harness gets only what its SDK reads: `Content-Type`, `Cache-Control`, `Retry-After`, `Retry-After-Ms`, `X-Should-Retry`, the provider's request id (`Request-Id`, `X-Request-Id`) and rate-limit headers (`anthropic-ratelimit-*`, `x-ratelimit-*`, `x-codex-*`) (`CONTROL_PLANE_RESPONSE_HEADERS` in `gateway.rs`). The list is what the Anthropic and OpenAI SDKs (which Claude Code, Codex and the OpenAI-compatible harnesses use) read from an answer, as their published sources do; no SDK source is vendored in this repository to check it against. Nothing else arrives: no `Set-Cookie`, `Location`, `Content-Location`, `Link`, `Via`, `Server` or `WWW-Authenticate` (a challenge for a credential the harness does not hold, naming AIPlex's URL), and not `X-Aiplex-Request-Id` or `X-Aiplex-Error-Code`, which Branchyard records and the harness has no use for. The values are checked too, as a body is: a header is dropped when its name or value holds the key, its value is over 256 bytes or has a byte outside printable ASCII, names a URL (`://`) or the backend's host, or breaks its header's grammar (`Retry-After` a number of seconds up to a day or an HTTP date still to come (a date already past says nothing and is dropped), `Retry-After-Ms` whole milliseconds up to a day, `Request-Id` and `X-Request-Id` 1 to 128 of `A-Z a-z 0-9 _ . : -` that do not read as an IP address or a socket address, `X-Should-Retry` `true` or `false`, `Content-Type` a `type/subtype` media type), and a name the answer sends twice is dropped. When that drops the content type, the answer is unusable, and it is a failure of that backend, not relayed unchecked. A limit: these checks look at one header at a time, so a token split across two relayed values (half in `Request-Id`, half in `X-Request-Id`) is not found; with ids held to that alphabet and 128 characters, that is all such a split can carry.
- **No coding the gateway did not ask for.** The gateway asks every backend for `Accept-Encoding: identity`. An AIPlex answer in another coding (`gzip`, `deflate`, …) cannot be checked for the key or metered, so it is not relayed: a refusal is withheld with its status kept, a success becomes the call's failure (`502`, recorded without usage). A plain backend's encoded answer is relayed with its coding, as before.
- **No redirect.** A `3xx` (other than `304`) from an AIPlex backend is neither followed (it would carry the token elsewhere) nor relayed. Without `X-Aiplex-Error-Code`, that backend failed (`aiplex-anthropic: the control plane answered with a redirect (302), not followed`), and the call moves to the route's next AIPlex backend under the rule for an answer without the code, else fails `502`. With it, the redirect is AIPlex's refusal and final like any other: no other backend is tried, the event's decision is `refused` with AIPlex's code and request id, and the harness gets the gateway's own error in its API's shape with status `502` (a `3xx` cannot be relayed), never the `Location`.
- **Never the credential.** An AIPlex answer that is not a success is read whole, up to 1 MiB (`MAX_ERROR_BODY`; exactly 1 MiB is read and checked, one byte more is too large, on every path) and within 30 seconds in all (`ERROR_BODY_TIME`: each read waits at most what is left of it, so a backend that trickles bytes cannot hold the harness), before any of it reaches the harness. If it holds the key the gateway sent (an upstream that echoes its request), is longer than that, does not arrive in time, or stops, the harness gets Branchyard's own error in its API's shape with the status kept, and the event's reason says `the control plane's answer was withheld: it held the credential the gateway sent` (or that it was too large to check). The body is not shown, stored, written to the audit file or logged. A successful answer is streamed as it arrives and is not scanned: a stream is the provider's output relayed by AIPlex, scanning every one costs a copy of each call, and holding a stream back to scan it would break streaming. That is a stated limit: a control plane that put the token into a successful answer would reach the harness.

**Status, 14 November 2026.** Two kinds of evidence. Hermetic: `MockHttp` servers play AIPlex access points, shaped as the contract's response headers say, and a provider reached directly; a fake harness (a Python script the fake ACP agent runs) calls through a real turn's gateway on both APIs, streamed and not, forging attribution, identity, address and cookie headers in other letter cases, repeated and folded, and reporting its environment; mocks echo the token, send AIPlex-only headers, redirect, close, hang and send malformed headers; and the built `by` shows the calls and refuses the bad configurations. Live, with a dated record: the real `by` 0.5.0 (Branchyard commit 8d6c52f, stated on the command line and not verified, because `by --version` prints none) against a real `aiplex dev` (AIPlex commit f4e3991, read from the binary, clean), on one machine over loopback with AIPlex's mock providers, a stand-in direct provider served by the script and Branchyard's fake agent running a script as the harness; no model and no real coding agent. `tools/qualify_aiplex.py` ran the stored-token scenario on 14 November 2026, twice in a row with the same result, and wrote [aiplex-control-plane-2026-11-14.json](evidence/aiplex-control-plane-2026-11-14.json), which holds each of its 24 checks with what was observed, and the SHA-256 of `by`, the fake agent, `aiplex` and `aiplex-api`. All 24 passed, each as its check states it. The file `aiplex connect local --client branchyard --write` wrote is accepted by `by config validate` (exit 0 and its `ok` line), as written and with a second AIPlex backend and a plain backend added. One turn made seven calls through the gateway and the streamed answer ran to its end. Every call to AIPlex has one `model` event with AIPlex's request id and the call id, and the one call the script expects AIPlex to refuse is the only refusal (`MODEL_NOT_FOUND`, with a refusal record in AIPlex). AIPlex holds one record for each call routed to it, found by its request id and by `--client-request-id`, with `by_branch`, `by_turn` and `by_harness` equal to Branchyard's own record of the branch. The refusal was final: its route named two AIPlex backends, the event names one and lists none tried, and AIPlex holds one record under the call's id. The forged `X-Client-Request-Id` and `X-AIPlex-Tags` the harness sent (the harness records the headers it sent) reached AIPlex as neither an id nor a tag. A call for a model with a plain route and a call for a model no route takes were answered by the stand-in, which never received the AIPlex token, while AIPlex holds records only for the calls routed to it. With AIPlex stopped, the call for an AIPlex-routed model failed with the gateway's own 502, the event naming only the AIPlex backend, and no request reached the stand-in. Every response header the harness received on an AIPlex answer was on the gateway's allow-list or the server's own framing (`date`, `content-length`, `transfer-encoding`, `connection`, `keep-alive`), so none of `WWW-Authenticate`, `Set-Cookie`, `Location`, `Link`, `Content-Location`, `Via`, `Server` and no `X-Aiplex-*` header reached it. No variable of the harness's environment held the token (whole or its signature segment, as a substring of a value) and `AIPLEX_TOKEN` was not set. The token is in no file under the run's scratch directory, as plain text, and in no output the script captured. `aiplex dev stop` ran and nothing of the run was left alive. The `by log` text line ends with the call id (`call <id>`), the value AIPlex stores as `client_request_id`, so a person can join from Branchyard's log to AIPlex's record. To run it again: `python3 tools/qualify_aiplex.py --aiplex PATH --by PATH --fake-agent PATH --branchyard-commit SHA` with the built binaries of both products (`--help` lists the rest and the exit statuses; the status is 0 only when every check passed). The signed token ran in a second scenario, below. Not done here: a stored token is still a secret a local harness running as the same user can read from disk; what the token check cannot see (an encoded or otherwise transformed copy of the token, or a copy outside the harness's environment); a successful answer scanned for the token; tools through AIPlex's MCP endpoint; harness telemetry sent to AIPlex; `traceparent` live (a local turn has none); a real model, a real coding agent, a cloud AIPlex, a Branchyard server or another operating system in this scenario.

### A token Branchyard signs

On a Branchyard server an AIPlex backend can be called without storing any AIPlex secret: the gateway presents a token the server signs for each turn, and AIPlex verifies it against the server's published keys (AIPlex calls this agent identity federation). The backend names how, instead of a `key`:

```toml
[models.backends.aiplex-anthropic]
api = "anthropic"
url = "https://aiplex.example.com/ap/ide"
control_plane = "aiplex"
auth = "turn-token"                      # instead of key
audience = "https://aiplex.example.com"  # the audience the AIPlex binding expects
```

Each of these is refused when the configuration is read, naming the backend and what to write instead (the backend's shape by `by config validate` and the server's configuration file; the issuer, which only a yard or a server knows, by `by serve --check` and when `by` or the server opens the repository): `auth` with another value than `turn-token`; `auth` on a backend that is not `control_plane = "aiplex"` (a token Branchyard signs never goes to a plain provider); `auth = "turn-token"` without an `audience`, or with an empty one; `auth` together with `key` (one credential, not two); `audience` without `auth`; and `auth = "turn-token"` where the yard's tokens have an issuer AIPlex cannot fetch keys from. The issuer must be `https://host[:port]`, or `http://` on a loopback host (`127.0.0.1`, `[::1]`, `localhost`) for a development AIPlex on the same machine, with no path, query, fragment or trailing slash, since AIPlex fetches `{issuer}/.well-known/jwks.json` and compares the issuer byte for byte with the one it was bound to. It must name an address something can be reached at: `0.0.0.0` and `[::]`, which a server listens on, are refused as unspecified. An IPv6 address is written in brackets and a port is not left empty (`https://host:`); each is refused saying so (`models::federable_issuer` reads the issuer as it was written before the URL parser does, so these are the refusals an operator sees, not the parser's `invalid port`). And it is written in one spelling. AIPlex's binding takes an issuer as it is written, an upper-case host, a trailing dot or the scheme's default port included, and then matches a token's `iss` against it byte for byte, so the two sides must spell the issuer identically; Branchyard takes exactly one spelling so that they can (`models::canonical_issuer`): a lower-case host, no trailing dot, no default port (`:443` for `https`, `:80` for `http`) and no leading zero in a port. An issuer in another spelling is refused with what is different and the spelling to write (`the issuer "https://by.example.com:443" writes out the default port of https; … write "https://by.example.com"`), and that same string is what AIPlex is bound to. A local yard's issuer is `branchyard:local:<id>`, so a local yard cannot be federated: it uses `key`. So is a tenant AIPlex cannot name: the subject is `branchyard:<tenant>`, and the tenant must be 1 to 128 of `A-Z a-z 0-9 . _ @ + -`; the server's default tenant is refused when the configuration is read, and a branch acting for another such tenant has its call fail closed, saying why.

On a server, the issuer is `connectors.issuer` when `connectors` is configured, else `public_url` when a backend uses `auth = "turn-token"`, else `{scheme}://{listen}`. So with `connectors`, set `connectors.issuer` (the connector gateway verifies its own tokens against that issuer, which the model gateway's tokens share, so `public_url` does not move it); without `connectors`, set `public_url` (a trailing `/` is dropped). A `listen` port of 0 without the setting that applies is refused, because the port is only chosen when the server starts. So is a server that listens on every interface (`0.0.0.0`, `[::]`) without it, with or without TLS: its issuer would be the address it listens on, which nothing can be reached at. Every refusal names the setting that gives the issuer. Where the issuer is that setting's own value, it says so (`it is public_url's value: change it there`). Where the issuer came from `listen`, it names the setting to set, and the value when only the spelling was wrong: a server on `127.0.0.1:80` without TLS has the issuer `http://127.0.0.1:80`, and is told to `set public_url to "http://127.0.0.1"`; one on `0.0.0.0:8443` is told to set it `to the URL AIPlex reaches this server at`, which only its operator knows. A configuration without such a backend keeps the issuer it always had. The server then serves its public keys at `GET /.well-known/jwks.json` even without `connectors` (from the model gateway's key file, `<data_dir>/gateway/key`, made on first use), the keys its tokens are signed with: each `kty: "OKP"`, `crv: "Ed25519"`, `x`, `kid`, `alg: "EdDSA"`, never the private `d`. With neither `connectors` nor such a backend the route stays `404`.

The token is separate from the one the harness holds; neither ever stands for the other. For each turn and audience the gateway signs one compact JWS (header `alg: "EdDSA"`, `typ: "JWT"`, `kid`) and presents it as `Authorization: Bearer` on every call to that backend, with no `x-api-key`:

| Claim | Value |
|---|---|
| `iss` | The server's issuer, as above, the value its other tokens carry |
| `aud` | The backend's `audience`, a string |
| `sub` | `branchyard:<tenant>` |
| `iat`, `exp` | `exp` at most an hour after `iat` (AIPlex refuses more) and not past the turn's deadline |
| `jti` | Unique per token |
| `by_purpose` | `"aiplex"` (`models::AIPLEX_PURPOSE`) |
| `by_tenant`, `by_branch`, `by_turn` | As in the turn's other tokens (`by_branch` is `<repo>/<branch>` on a server) |
| `by_grants` | `[]`: it carries none of the turn's scopes (no grants, models, network, delegation or manifest); the claims type cannot leave this one key out |

Signing may be a network call (a Cloud KMS signer), so the token is signed once per turn and audience and reused, and replaced only when fewer than 120 seconds of it are left; a token the turn's deadline cut short is kept until it expires, since a new one would expire no later. No call is sent with an expired token, and none once the turn's deadline has passed: then no token is signed and the call fails closed like a failed signature. If the token cannot be signed, the call fails closed with Branchyard's own `502` (`could not sign the token it is called with: …`), nothing is sent upstream, and the call does not fall back to a key or another backend. The token never reaches the harness: not its environment, files or answers, not an event, a usage record, the audit file or a log line.

Branchyard's own gateways refuse it: the model gateway takes only the turn's own token, byte for byte, and refuses any token with a `by_purpose`; the delegation endpoint refuses any token with a `by_purpose` or another audience; Anvil, the connector gateway, refuses a token whose `aud` is not its own URL.

Everything the sections above say about an AIPlex backend holds for one called this way: the request and response allow-lists, the redirect rule, the withheld refusal (the credential looked for is this token), the id on every event. There is no key variable to take out of the harness's environment in this mode: nothing of the token is in a variable.

The `audience` is a name and need not be a URL, but the token is good at that audience for up to an hour, so whoever is at the backend's `url` could replay it there; when the audience is an `http(s)` URL its host is normally the `url`'s, the AIPlex the token is for. A mismatch is not refused (an AIPlex may name its audience differently), but it is noted where an operator looks: as a warning when a server reads its configuration file (`by serve --check` prints it) and as `models.audience` in `by doctor` (one row whenever a backend is called with a signed token: `ok` when no audience names another host, else a warning naming each backend; [doctor](doctor.md#the-checks)).

The header checks above look at one value at a time. A hostile AIPlex could put this token's signature segment, or its parts spread over several allow-listed headers, in a relayed id, and the check for the whole token does not see a part; the signature alone is not a credential, but the checks are no stronger than that.

What AIPlex has to be told, on its side (the `branchyard` preset of `aiplex agents identity bind`): the server (the issuer, from which the key set URL `{issuer}/.well-known/jwks.json` follows), the tenant (so the subject `branchyard:<tenant>`), and the audience the backend names; the preset requires `by_purpose = "aiplex"`. `by doctor` reports such a backend as called with the server's signed token, not as missing a key, and `by models` names the audience. `by models list` skips it, saying so: outside a turn there is no turn to name in a token.

**Status, 14 November 2026 (the signed token).** Hermetic: an engine turn against `MockHttp` playing the access point, with the yard's key ring behind a counting signer, receives one token on both calls, verified against the yard's key set with every claim of the table checked; a signer that fails fails the call with nothing sent upstream; the gateway refuses the token presented as a turn token; and a real `by serve` process in a test, without `connectors` and with `public_url` on loopback, serves the key set and a turn's token verifies against it. Live, with a dated record: the real `by` 0.5.0 (Branchyard commit 8d6c52f, stated on the command line and not verified) running `by serve` and the `by` client, against a real `aiplex dev` (AIPlex commit f4e3991, read from the binary, clean), on one machine over loopback with AIPlex's mock providers and Branchyard's fake agent running a script as the server's harness command for `gemini-cli`; no model, no real coding agent, no TLS, no `connectors`, no worker. `tools/qualify_aiplex.py --federated` ran the second scenario twice in a row with the same result and wrote [aiplex-signed-token-2026-11-14.json](evidence/aiplex-signed-token-2026-11-14.json) (24 checks, each with what was observed, and the SHA-256 of `by`, the fake agent, `aiplex` and `aiplex-api`). All 24 passed, each as its check states it. `by serve --check` accepts a configuration whose only model backends are AIPlex backends with `auth = "turn-token"` (no `key`, no `secrets`), with `public_url` and `listen` on one loopback port. The AIPlex developer token is not in the running server's environment (not `AIPLEX_TOKEN`, not its value), its command line, its configuration, or any file under the run's scratch directory, as plain text; the `by` client was given only the server's own static credential. In AIPlex an agent was registered and bound with the `branchyard` preset (`server` the server's URL, `tenant` `default`, the backends' audience), and `aiplex agents identity plan` showed that issuer, the subject `branchyard:default` and that audience. A first call, before the agent had any grant, was refused by AIPlex with `403 ACTION_DENIED` and not `401`, and its record had `caller_kind` `agent` and `client_id` the bound agent (the token was verified against the server's key set and the agent resolved); the agent's principal was then granted the two models and their surfaces. One turn on the server made four calls (Anthropic, OpenAI chat, streamed, and one for a model AIPlex does not serve, refused with `MODEL_NOT_FOUND`); each has one AIPlex record, found by its request id and by `--client-request-id`, with `caller_kind` `agent`, `client_id` the bound agent, the principal `branchyard:default`, and the tags `by_branch` (`<repo>/<branch>`), `by_turn` and `by_harness`. The server's `/.well-known/jwks.json` answered `200` with one OKP Ed25519 key and no private member, and the server's log shows it was served once before the script asked for it (AIPlex's fetch). A negative control on the same server, a backend whose `audience` the binding does not have, was refused by AIPlex with `401 UNAUTHENTICATED` as the event shows it, was final (one backend, none tried before it) and left no AIPlex record, while the other calls of the server were accepted. No variable of the harness's environment held a JWT whose claims carry `by_purpose` (the two JWT-shaped variables it held were the gateway's own turn token, which has none), the AIPlex token, or `AIPLEX_TOKEN`; no file under the scratch directory holds a signed token; the server and `aiplex dev` were stopped and nothing of the run was left alive. To run it again: `python3 tools/qualify_aiplex.py --federated --aiplex PATH --by PATH --fake-agent PATH --branchyard-commit SHA`. Not done: a Cloud KMS signer, a Branchyard worker or sandbox, TLS and an `https` issuer, a Branchyard tenant other than the server's default; a token with an expired or over-long lifetime, another `by_purpose`, `sub` or `iss` (only the audience was changed); a real model, a real coding agent, a cloud AIPlex, `traceparent` live and another operating system; what the token checks cannot see (an encoded or otherwise transformed copy, or a copy outside the harness's environment).

## Subscription logins and direct mode

A harness logged in by subscription (Claude Code's or Codex's OAuth login, a ChatGPT login) sends a token tied to that login, which only the provider accepts; the gateway cannot replace it with a key. Branchyard recognizes this when the branch's secrets include the login its harness reads, `CLAUDE_CODE_OAUTH_TOKEN` for Claude Code or `CODEX_AUTH` for Codex, or its `--auth` is `oauth-token`; a login meant for another harness (a fan hands every candidate the same secrets, and the other harness's provisioner reports it unused) leaves the branch on the gateway. A `CODEX_AUTH` file is taken for a login even when it holds a key. Such a turn runs **direct**: no gateway, its harness calls its provider itself, and when its harness is Claude Code or Codex its provider's hosts are added to its egress policy for the turn (`api.anthropic.com:443` and `console.anthropic.com:443`; `api.openai.com:443`, `chatgpt.com:443` and `auth.openai.com:443`). A harness whose API no backend serves (Codex with only Anthropic backends) runs direct the same way.

Direct is recorded (`models direct: claude-code is logged in by subscription (CLAUDE_CODE_OAUTH_TOKEN), which cannot take an injected key`) and shown by `by show`. Its calls are not metered: its cost is the harness's own estimate.

A cost limit on a direct turn is held on that estimate, with less than the gateway can promise ([what a cost limit guarantees](delegation.md#what-a-cost-limit-guarantees) has all three modes side by side). When the harness can limit itself (Claude Code's stream-json, `capabilities().budget`), it is started with its own limit set to what is left of the branch's budget, and Branchyard still interrupts the turn at the first usage report that takes the branch's spend to or past the limit. When it only reports (Pi), Branchyard interrupts at that report, not when the turn ends. The overshoot is the call the report is of, plus what the harness spends before it handles the interrupt. A harness that reports no cost it can be priced from (Codex, Amp, Antigravity, every ACP agent) is not stopped by a cost limit when it runs direct.

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
| `by log` | `models through the gateway at http://127.0.0.1:41203 (claude-*)`, then `model: claude-sonnet-4-6 allowed via anthropic (200, 1210 in / 80 out, $0.0048, tools: Read, Edit, stop: tool_use, 812 ms)` per call (`via local as qwen3-coder` when translated; the tools and stop reason when the response has them, see [interception records](#interception-records)), refusals with their reason, budget alerts, `models direct: …`, and `access narrowed …` |
| `by log --json`, event streams | `{"activity": "model", "model": {"kind": "gateway"\|"direct"\|"call"\|"alert", ...}, "text"}`, a call's `interception` (`request_bytes`, `response_bytes`, `prompt_digest`, `messages`, `tools_offered`, `tool_uses`, `stop_reason`) and, with `audit = "contents"`, its `audit_file`; `{"activity": "access", ...}` |
| `by show` | `models  gateway (claude-*); 3 calls, 4.2k in / 310 out tokens, $0.0123; refused 1 denied`, or `models  direct (…)` |
| `by show --json` | `models: {mode, models, reason, calls, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd, unpriced, tool_uses, refused}`; `cost_usd` is the metered cost, `tool_uses` the tools the calls' responses called |
| `by watch` | the latest call as what the branch is doing |
| `by models [--period day\|month\|all] [--json]` | the backends (each key's source and whether it is set, never its value), routes, budgets with today's and this month's spending, the `audit` mode, and usage by model |
| `by models list [--refresh] [--harness ID] [--json]` | the models each key can call, from the providers' own APIs, cached with their age; the models nothing prices; a harness's catalog list beside the live one ([below](#listing-models)) |
| `by stats` | `models  N calls (allowed …, denied …), T tokens, $X metered`; `--json`: `model_calls`, `model_tokens`, `model_cost_usd` |
| `/metrics` | `branchyard_model_calls_total{model,decision}`, `branchyard_model_tokens_total{model,kind}`, `branchyard_model_cost_usd_total{model}` |
| SDK | `Yard::use_models`, `Yard::models`, `Yard::model_usage(since_ms)`, `Yard::use_ceilings`; `models::{Gateway, Config, Audit, ModelAccess, ModelActivity, ModelCall, Interception, UsageRecord, Tokens, summarize, period_starts}` |

`by models` reads this repository's configuration and store; with `--remote` it is refused (a server's usage is on its `/metrics`).

## Listing models

`catalog/harnesses.toml` carries each harness's models as the pinned catalog knew them, which goes stale the day a provider ships a model. `by models list [--refresh] [--harness ID] [--json]` asks the providers instead, for every source whose key is present:

| Source | Key (a `[secrets]` entry of that name, else the variable) | Request |
|---|---|---|
| Gemini | `GEMINI_API_KEY`, else `GOOGLE_API_KEY` | `GET https://generativelanguage.googleapis.com/v1beta/models?pageSize=100` with `x-goog-api-key`, following `nextPageToken`; only models whose `supportedGenerationMethods` has `generateContent`, named without `models/` |
| OpenAI | `OPENAI_API_KEY`, else `CODEX_API_KEY` | `GET https://api.openai.com/v1/models` with a bearer token |
| Anthropic | `ANTHROPIC_API_KEY` | `GET https://api.anthropic.com/v1/models?limit=1000` with `x-api-key` and `anthropic-version: 2023-06-01`, following `last_id` while `has_more` (a page holds 20 models unless asked for more) |
| Z.ai | `ZAI_API_KEY` | `GET https://api.z.ai/api/coding/paas/v4/models` with a bearer token (OpenAI's list shape; the same list is served at `…/paas/v4/models` and `…/anthropic/v1/models`) |
| A `[models.backends]` entry | its `key` | `GET {url}/v1/models`, or `{url}/models` when the URL already ends in `/v1` or `/openai` or the backend has the `versioned` layer, with the key where the gateway sends it (`x-api-key` for an `anthropic` backend; a bearer token, or the generic backend's `header`, otherwise); an entry that repeats a source above (same endpoint, same key) is skipped as a repeat |

The same Gemini key serves Gemini's OpenAI-compatible endpoint, `https://generativelanguage.googleapis.com/v1beta/openai` (chat completions only; it rejects the OpenAI-only `store` field), which `by models list` and its JSON (`openai_compatible`) name beside Gemini's models. A backend at that URL lists at `/v1beta/openai/models`.

Each source prints its models, each with what prices it: `catalog` (`catalog/pricing.toml`), `[models.prices] PATTERN`, or `unpriced`. An unpriced model is one whose calls the gateway records without a cost, so a dollar budget cannot count them; the catalog prices no Gemini model, so a Gemini model needs a `[models.prices]` entry to count. A source without a key is one line (`skipped: no credential: …`), and one that fails is one line with the status it answered (`failed: answered 401`), never its body. A key is never printed or cached; the output names the variable (`$GEMINI_API_KEY`).

The answer is cached as `models.json` in the live catalogs' cache (`$BRANCHYARD_CATALOG_DIR`, else `~/.cache/branchyard/catalog`), named in `manifest.json` with its SHA-256 and the time, and refused when it does not match ([live catalogs](registry.md#live-catalogs); `by catalog status` shows it). Without `--refresh`, `by models list` shows the cache with its age and asks only for a source it has never listed; `--refresh` asks them all again. A source that failed is not cached.

`--harness ID` asks only the providers of that catalog harness's keys (`auth_env`, and the keys its provisioning reads: Gemini CLI and Antigravity take a Gemini key) and sets the catalog's `models` beside what they list: each name is `listed`, an alias (a family word such as `sonnet`, shown with the live models it stands for) or `not listed`, then come the live models the catalog does not name, so a stale static list shows. `--json` has all of it under `harness`.

Requests go through `HTTPS_PROXY` (then `https_proxy`, `ALL_PROXY`) as a `CONNECT` tunnel to the `https` host, unless `NO_PROXY` lists it (a host or a domain, with an optional port; address ranges are not read) or it is loopback, and trust the bundle `SSL_CERT_FILE` (then `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`) names beside the Mozilla roots. `--gemini-url`, `--openai-url` and `--anthropic-url` (`BRANCHYARD_GEMINI_URL` and the like) point the three providers at another base URL, such as a mirror. The command is refused inside a branch, where the environment holds the turn's token and not a provider's key, and with `--remote`.

## What is not done

- **Which harnesses honor the base URL variables.** Claude Code 2.1.301 does (`ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`: its turns on GLM above, `count_tokens` included); codex-cli 0.157.1 does not read `OPENAI_BASE_URL` or `OPENAI_API_KEY` for its own calls, so its [provisioner writes the gateway as a provider](provisioning.md#harnesses) in `config.toml`; DeepSeek Harness takes its endpoint from `cordis.patch.yml`, which its provisioner writes. Every other harness is untested: whether it honors the variables for everything it does, and with which paths.
- **Subscription logins** cannot go through the gateway (above). A login Branchyard does not see is not detected.
- **Provider features not proxied.** The gateway passes the body through unchanged except for `stream_options.include_usage`, and [translation](#translation) when a route asks for it; it does not pass batch, file, realtime or WebSocket APIs anything special. Translation goes one way, Anthropic's Messages API onto OpenAI's chat completions: an OpenAI request is never translated for an Anthropic backend, and the `/v1/responses` API is not a translation target. No real Claude Code has run through it; what Claude Code sends that the table above does not cover (a `document` block, `count_tokens`) is refused rather than guessed.
- **Prompt caching** passes through: the harness's `cache_control` blocks and headers go to the backend as they are, and cache reads and writes are metered and priced. A weighted choice can send a conversation's next call to another backend, whose cache is cold; give a route one backend (and fallbacks) to keep caches warm. No responses are cached by the gateway.
- **Interception records** are of the calls the gateway forwards, as counted above: not of a [direct](#subscription-logins-and-direct-mode) turn's calls, which do not pass through it, and not of what a provider ran on its own side. They are not tamper-evident, and `contents` has no rotation or retention: a file grows with its turn's calls until it is deleted.
- **Rate limits** are per process; several `by` processes or servers each hold their own window. Tokens per minute are not limited.
- **Budget alerts** are recorded once per period in each process, as events; nothing is sent.
- **Cost after a turn ends.** A call still running five seconds after its turn ends is stored and recorded, but not added to the branch's cost.
- **A call in flight.** The gateway cannot know what a call costs before it ends, so the call that takes a branch to its limit is always paid for, and calls made in parallel can each start under it. A limit is a bound on when calls may start, not a cap on one call's cost.
- **A cost limit on a direct turn** is only as good as the harness's reports; see above.
- **Upstream proxies.** The gateway reaches its backends the way [`by models list`](#listing-models) does: through `HTTPS_PROXY` (then `https_proxy`, `ALL_PROXY`) as a `CONNECT` tunnel unless `NO_PROXY` or loopback exempts the host, trusting the bundle `SSL_CERT_FILE` (then `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`) names beside the Mozilla roots. The variables are read once, when the yard's gateway is built (`Gateway::new`), from the process that runs the turns. Until 16 October 2026 the gateway connected directly and ignored them, which a machine behind a TLS-intercepting proxy showed as `could not connect (TLS: invalid peer certificate: UnknownIssuer)` on every backend.
- **Gemini's own API, Bedrock and Vertex** are generic pass-through only: no usage is read from their shapes unless they match the ones above, and their paths name the model, which the scope does not check. Gemini through its [OpenAI-compatible endpoint](#gemini-through-the-openai-compatible-endpoint) is an `openai` backend, metered as one.
- **An access point's other routes** are not reached through the gateway. An AIPlex backend is asked only [the integration's routes](#what-is-sent), so a harness that asks it for embeddings, a moderation, a stored response or one model by id, another wire, or AIPlex's tools gets the gateway's `403`. Which of those each harness asks for has not been surveyed; AIPlex's tools are not part of this integration.
- **A server** fails a turn on the gateway when it has no `models` backends, rather than refusing the request at admission.
- **The pricing tables** are a copy of the ones `by usage` uses, read from the same file; the model-name rules are duplicated in `branchyard::models::pricing`.

## Tests

- `branchyard-provision` (3): model access flags, globs and the wire form; narrowing for children and ceiling intersection; network ceiling intersection.
- `branchyard` (53 unit, and 1 conformance check): usage from each provider's fields, streamed across chunk boundaries; tool calls and stop reasons read from Anthropic, OpenAI chat and OpenAI responses bodies and streams, cut in two at every byte and a byte at a time (names once per call, in order; an error body, a body that is not JSON and one too large to keep read as nothing); the request's messages, tools and digest in each API (the digest equals the BLAKE3 hash of the bytes sent, is the same for the same bytes and not for the same JSON spaced differently, and for no body is BLAKE3's published empty-input value); a call's `by log` line with its tools and stop reason, an interception round-tripping and an event recorded before there were any reading back; the `audit` mode defaulting to `digests` and refusing other values by name; audit lines (JSON or text, a stream's raw text, `null`, truncation) and their file and directories 0600 and 0700; a response kept for the audit only when asked, and cut once at a chunk; catalog pricing; the seeded weighted order and its 3:1 share over 400 calls; the route rules; a route with a `backend_model` offering its OpenAI backend to an Anthropic request, keeping the harness on the gateway for the models it covers and no other, and never bridging without one; rate windows freeing a slot after a minute; direct mode for subscription logins and unserved APIs and the hosts it allows; path routing; the usage request for OpenAI chat streams; errors in each provider's shape; base URL parsing; refused configurations; UTC day and month starts; the network digest; token claims (the contract's unchanged without the new scopes, old claims parsed, the new ones signed and verified); translation of a Messages request (system, text, image, tool use and result ordering, tools and server tools, every tool choice, sampling, thinking, streaming; documents and other roles refused), of a completion (text, tool calls with parsed and unparsable arguments, reasoning, every finish reason, usage) and of a chat stream into Anthropic's events, identical whole, byte by byte and in uneven pieces, with reasoning, an error chunk and a stream that ends without `[DONE]`; Gemini's compatibility layer: on by host and where set, off with `none`, refused on a backend that is not `openai`; the path's `/v1` dropped and nothing else; only the listed fields stripped; replayed calls given their recorded signature, the first call of a step with none the placeholder, the rest and a call already signed left alone, another person's signature never used; signatures kept from a whole answer when it ends and from a stream as each event completes, in any chunking, an id and its signature in one delta or two; the store bounded by count and bytes, least recently used out first; the debug log naming fields and counts and never a signature; and, through a turn's running gateway against a mock of the endpoint that refuses `store` and an unsigned step as the real one does, an OpenAI harness's call and stream stripped and sent to `/v1beta/openai/chat/completions` with the real key, its replayed call signed with the recorded signature and one from elsewhere with the placeholder, a translated Claude request (streamed, then whole) whose `tool_use` ids carry the signatures back and whose `user` is stripped, and a backend without the layer sent the harness's bytes as they were and keeping nothing. The storage conformance suite's `usage` check runs on SQLite and on PostgreSQL.
- `branchyard` engine (`tests/models.rs`, 13, mock upstreams and the fake ACP agent running a Python harness): both APIs, streamed and not, through the gateway with the real key injected, the token never upstream, the key nowhere in the harness's environment or the event log, exact tokens and costs, one usage row per call and the branch's cost equal to their sum, and the token's claims; scope, wrong-token and unreadable-body refusals with nothing forwarded; fallback over a refused connection, a 503 and a 429, and every backend failing; a rate limit with `Retry-After`, a daily budget refusal with one alert, and the branch's own budget; a person's ceiling narrowing the token; a required egress policy under network-namespace confinement reaching the gateway and not the upstream; a delegated child staying on the gateway within its parent's models, and refused outside them; a Claude request on a route to an OpenAI backend, translated both ways (the harness reads `hello` from the message and six events from the stream, the backend sees chat completions for `gpt-5` with the real key and none of Anthropic's headers, the stream asked for its usage), recorded as the harness's model translated as `gpt-5`, the backend's usage priced by that model and its rows naming it, and `count_tokens` refused `502` as not translated; the interception of calls in each API (the mock answers a request that offers tools by calling Read and then Edit: an Anthropic message and stream, a chat completion and delta stream, a responses object and stream), with the tools offered, the tools called, the stop reason, sizes equal to what the harness sent and read, the digest equal to the hash of the bytes the harness wrote while the backend saw other bytes for a chat stream, the same digest for the same request, none for a refused call and the last backend's answer for a call every backend failed, and nothing written under `digests`; `contents` writing one line per forwarded call to a 0600 file in a 0700 directory, the request as sent and the response as JSON or raw stream text, `audit_file` on the calls and none for a refused one, the prompt nowhere in the branch's events, and a bad value refused by name; a translated call read in the backend's shape (the harness's tools offered, the chat completion's tool calls and `tool_calls` stop reason, the digest of the Anthropic request); a branch limited to exactly what one call costs making that call and having the next two refused with nothing forwarded, its cost the one call and the turn over budget; and a child whose parent spent half of the limit its tree shares, so that its own limit has room, having its calls stop at the one that takes the tree to the parent's limit, the fourth call refused or never made, three forwarded.
- `by` (4 unit, 2 with the built `by`): `--model-gateway` and `by models` parsing; `[models]` defaults and provider keys kept from the harness; key sources through `[secrets]`; rig seats' models; then `[models]` putting a branch on the gateway, `by show`, `by show --json`, `by log`, `by log --json`, `by models`, `by models --json`, `by stats --json`, a flag narrowing a branch, the tools a call used in `by log` and `by show --json`, and a bad `[models]` refused by `by config validate`; then `audit = "contents"` in `branchyard.toml` writing the file and naming it in `by log --json`, `by models` showing it, and `audit = "full"` refused naming it and the two values.
- Listing models (`branchyard` unit tests `models::discover` and `models::upstream`, 16, mock servers on loopback; `branchyard-cli/tests/models_list.rs`, 6, the built `by` against them): Gemini's two pages with only the models that generate, the key in `x-goog-api-key` and never the query; OpenAI's bearer token and `CODEX_API_KEY` as the fallback; Anthropic's `limit` and `has_more` paging with `x-api-key` and `anthropic-version`; a backend's path at the URL, with `/`, `/v1`, `/openai` and a prefix, and its own header; a repeated page cursor ending the walk; a source without a key skipped with nothing sent; a 401, a page that is not JSON, one that is not a list and a refused connection each one reason, with no body and no key in it; the cache shown until `--refresh`, another URL's not used, a lost key winning over it, a corrupted file refused and listed again; duplicate sources; a harness's keys and its static list against a live one (listed, alias, not listed, only live); `https` through a `CONNECT` proxy with a login (a mock proxy and an origin with a certificate for `provider.test`, trusted through the bundle given), a refused tunnel named by its status, an origin the bundle does not vouch for, a bundle with no certificate failing `https` and not `http`; `NO_PROXY` and loopback going direct; the table with `unpriced` and `[models.prices]` marks and the JSON; the cache in `by catalog status`; `--harness` for Codex, Gemini CLI and a harness with no key; refusal inside a branch; `by models` unchanged without a subcommand. No real provider was called.
- AIPlex as the backend (`branchyard` unit tests in `models::gateway` and `models`, `tests/control_plane.rs` 8, `tests/control_plane_env.rs` 1, `tests/control_plane_signed.rs` 2, `branchyard-cli/tests/models.rs` 1 and `tests/doctor.rs` 1, `branchyard-config` 3, `branchyard-server` in its models test and `tests/models_signed.rs` 4, `models::discover` 2): the headers a plain backend is sent pinned exactly as they were before control planes existed; a control-plane backend's exact headers on both APIs, the bearer and no `x-api-key`, the harness's attribution dropped in every case and repeat; the tags with every allowed character, 64 and 65 characters, characters outside the alphabet, the 512-byte limit; every id the gateway makes fitting the contract; the answer's id and code checked, a code that does not fit still final; the configuration refusals (another value, `generic`, no key, `compat`, a mixed route in `backends` or `fallbacks`, a backend no route names beside a plain one) by name, in the engine, `by config validate` and the server; every path's backends one kind (the weighted choice over 200 calls, fallbacks, a translated route, a call without a route with and without plain backends, a route changed after it was checked); then through real turns against mocks: forged, repeated and folded headers never reaching AIPlex, the same id in the header, the usage record and the event, every answer's `X-Aiplex-Request-Id` on the event, refusals `401`, `403`, `404`, `429`, `503` (streamed too, and on a call no route takes) final with status, body and `Retry-After`, relayed `429` and `503` failing over between AIPlex backends only under one id, an unrouted call whose plain backend answers `503` or refuses the connection reaching no AIPlex, `GET /v1/models` in both configurations, a 65-character branch's tag left out, no `traceparent` made up, and neither key in an event, `by log`, `by models` or an audit file; then the round-one fixes: a key variable absent from the harness's environment (name and value) while a plain backend's stays, the harness's IAP, key, cookie, address, tenant and user headers never reaching AIPlex while its API headers do, exactly; an answer echoing the token on `401` and `403` on both wires, in a stream split across chunks and on a relayed `503` withheld; AIPlex-only response headers kept from the harness and an SDK's kept; a redirect neither followed nor relayed; the call's id on the event of a call refused at connection, closed after sending, timed out (with and without a second AIPlex backend, which hears nothing) and answered with an ESC byte, a raw newline or a byte that is not UTF-8 in AIPlex's headers; the pass-through path refusing an AIPlex backend; a turn number past 2^31 in the tags; then the round-two fixes: gzip and deflate refusals on both wires withheld and a gzip success failed while a plain backend's encoded answer is relayed as before; header values holding the token, naming a URL or the backend's host, too long, off their grammar, a header named after the token and a content type sent twice dropped, the answer failed when its content type goes, an ordinary stream's content type kept; exactly 1 MiB relayed on both paths and one byte more withheld; a stalled refusal withheld; a one-character key never used and never shown; a timeout named with its backend and duration; harness header values over 256 bytes and an encoded request body never sent to AIPlex; a setup command run without the key's variable (`branchyard-support`'s `tests/child.rs` for the constructor itself); a harness that goes away during a refusal leaving it `refused`, with its code, id and one usage row, sent once; then the round-three fixes: `shell_setup` run without the key's variable; a token variable that children need (`PATH`, `HOME`, `LC_ALL`, `HTTPS_PROXY` and `https_proxy`, `NO_PROXY`, `GIT_DIR`, `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `SSH_AUTH_SOCK`, `BRANCHYARD_*`) refused and an ordinary one accepted; a redirect carrying AIPlex's code final, `refused`, never relayed and never sent on to the second AIPlex backend; ids that are a path, a `data:` URL or an address, fractional milliseconds and a past `Retry-After` date dropped; then the required address: an AIPlex backend without a `url` refused with a stored token and with a signed one, on both APIs, in the engine, `by config validate` and the server's file, a `url` that is empty, blank or names no host with it, one written out (the provider's own included) accepted, a plain backend keeping its default, and such a backend never listed at the provider by `by models list`; and the issuer of a signed token: each refusal's exact words (not a URL, a path, a query, an IPv6 address without brackets, an empty port, an unspecified address in four spellings, plain http off loopback, and every other spelling than the one taken, with the spelling to write), the server's refusal naming the setting whose value is wrong or the one to set (`0.0.0.0:8443` and `[::]:8443` under TLS, `127.0.0.1:80` told `"http://127.0.0.1"`, which then validates), and the `models.audience` row of `by doctor` (none without a signed backend, `ok` naming the backends, one warning for every mismatched backend); then the routes: each of the six with and without a query, and what an access point serves beside them, the other API's routes, other methods and other spellings of a route refused, with the refusal's words; and through a real turn, the tools endpoint, another wire, an unnormalized path and the other API's route refused `403` as `denied` on a routed call and on one no route takes, AIPlex hearing only the five routes the harness also asked for, and a plain backend sent `/mcp` as the harness wrote it.
- `branchyard-setup` (2): `[models]` parsed, refused and rendered back, `compat` with it (`gemini` on an `openai` backend kept, another value or another API refused); `audit` defaulting to `digests`, rendered back when it is `contents`, and other values refused by name.
- `branchyard-server` (2): `models` and `ceilings` loaded, the repository's gateway built with keys from the server's secrets, bad ones refused; model calls as metrics.

The confinement test needs a host that allows unprivileged user and network namespaces. Elsewhere it prints `SKIPPED` with the reason, and fails if `unshare -rn true` works but Branchyard's confinement does not.
