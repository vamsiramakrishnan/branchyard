# Console execution presets and source titles: contract note

Written 29 October 2026, for review before any shared configuration or route changes. It covers the two contract changes the guided console workflow needs: operator-approved execution presets, and Unicode source titles. Everything else in that workflow (previews, outputs by session and turn, the guided start form) uses routes that exist today. An adversarial review of the first draft against the code found two blockers and several wrong readiness sources; this version answers each.

## Why presets, and what they do not do

Today a workspace session's execution environment comes from the request. `StartSessionRequest.provider` carries a full `SubstrateOptions` (endpoint, router, template, bridge key and token file paths; `crates/branchyard/src/lib.rs:1584-1657`), `App::provider` checks only its kind against `allow_providers` (`src/api.rs:541`), and any secret in the server's table can be named in `provision` (`api.rs:582`). [Server](../../server.md) already records that a token holder chooses the endpoint and key path.

A preset is a named environment the operator writes into the server configuration. The browser sends its ID and the choices it permits; the server supplies the provider options, paths and secrets. A preset is not a second runtime and grants nothing: it resolves to the existing `TaskRequest` fields and then passes through the existing checks unchanged (`work::task_options` at `work.rs:803`: `allow_providers`, `allow_unapproved_tools`, provision, result contract; workspace membership and roles; quotas; scheduling). GKE is infrastructure for Substrate here, as in the domain model: a preset's provider is `local`, `substrate` or `microsandbox`.

Presets become a boundary only when the operator says so. With `require_execution_presets: true`, a workspace session start must name a preset and may not carry `provider` or `provision`; without it, the raw fields keep working as today and a preset's `tenants`, `harnesses` and `models` are guidance for the console, not authorisation.

## Configuration

Two new optional keys. Each preset's `provider` is the existing `branchyard::Provider` JSON, tagged by `kind`, so no second option format exists.

```json
"require_execution_presets": true,
"execution_presets": [
  {
    "id": "local",
    "label": "This server",
    "description": "Runs on the Branchyard server with the harnesses installed here.",
    "provider": { "kind": "local" },
    "harnesses": [
      { "harness": "antigravity", "models": ["gemini-3.8-flash-low"], "default_model": "gemini-3.8-flash-low" },
      { "harness": "claude-code" }
    ],
    "secrets": ["GEMINI_API_KEY"]
  },
  {
    "id": "substrate-gke",
    "label": "Substrate on GKE",
    "description": "Runs each session in its own Substrate actor on the team's GKE cluster.",
    "provider": {
      "kind": "substrate",
      "endpoint": "https://substrate-control.internal.example",
      "router": "https://substrate-router.internal.example",
      "template": "branchyard-harnesses",
      "key": "substrate/bridge.key",
      "api_token_file": "substrate/api.token",
      "ca": "substrate/ca.pem"
    },
    "harnesses": [ { "harness": "antigravity", "models": ["gemini-3.8-flash-low"] } ],
    "secrets": ["GEMINI_API_KEY"],
    "tenants": ["research"],
    "require_labels": ["substrate-gke"]
  }
]
```

| Field | Meaning |
|---|---|
| `id` | Stable, `[a-z0-9-]{1,64}`, unique |
| `label`, `description` | Plain language for the console |
| `provider` | Existing `Provider`. Its `kind` must be in `allow_providers` (`local` always is); `recipe` is refused, and `microsandbox` on a build without it is a warning, as for flags today (`cli.rs:927`). Relative paths resolve against the configuration file's directory when it loads, as other paths do (`config.rs:1-2`), so `App::provider`'s absolute-key rule holds |
| `harnesses` | Allowed harness or profile IDs, matched exactly: listing `claude-code` does not admit `claude-code-acp`, a different driver. Each has an optional `models` allowlist and `default_model`; without `models`, a request may not choose a model |
| `secrets` | Names in the server's `secrets` table, added to the session's `provision.secrets`. A worker resolves them from its own table (`execution.rs:153` re-runs `task_options` there), so workers need the same names |
| `tenants` | Optional allowlist; absent means every tenant |
| `require_labels` | Optional worker labels the session requires, so a Substrate preset runs only on workers holding its files. Today nothing adds labels for a non-local provider (`scheduling.rs:84`) |

All of this is validated in `Config::validate`, after flags are merged (`allow_providers` and `secrets` can come from flags, `cli.rs:776-788`), so `by serve --check` reports a bad preset. `FileConfig` denies unknown fields (`config.rs:1298`), so `schema/server.config.json` is regenerated.

With no `execution_presets`, the server offers one implicit preset, `local` (`"origin": "default"`), whose harnesses are the available ones and which supplies no secrets. Coding projects and today's behaviour are unchanged. `require_execution_presets` without any configured preset is a configuration error.

## Discovery

`GET /v1/execution-presets?workspace=<id>`, scope `read`. With `workspace`, the caller must be a member (`authorize_workspace(…, "read")`) and `can_start` reflects the caller's role; without it, presets are filtered by tenant only. Presets for other tenants are absent, not refused.

```json
{
  "required": true,
  "presets": [
    {
      "id": "substrate-gke",
      "label": "Substrate on GKE",
      "description": "Runs each session in its own Substrate actor on the team's GKE cluster.",
      "provider": "substrate",
      "origin": "operator",
      "can_start": true,
      "harnesses": [
        {
          "harness": "antigravity",
          "models": ["gemini-3.8-flash-low"],
          "default_model": "gemini-3.8-flash-low",
          "tool_approvals": false,
          "writer": true,
          "reviewer": false,
          "effort": false,
          "executable": { "state": "unknown", "reason": "The Substrate template provides the harness; discovery does not contact the cluster.", "action": null },
          "auth": { "state": "ready", "reason": "The server provides this harness's API key to each session.", "action": null }
        }
      ],
      "environment": { "state": "ready", "reason": "The Substrate settings and credential files are in place.", "action": null },
      "reachability": { "state": "unknown", "reason": "The cluster is contacted when a session starts, not before.", "action": null }
    }
  ]
}
```

Each readiness value is `{state: "ready"|"not_ready"|"unknown", reason, action}`. `reason` and `action` are fixed text chosen by the server: never an error message, file path, endpoint, command path or secret name, because a viewer reads them. Four questions are kept apart:

| Value | Local | Substrate |
|---|---|---|
| `environment` | `ready` | `SubstrateOptions::connection_config()` (`crates/branchyard/src/providers/substrate.rs:155`; `branchyard` is already a server dependency): reads the bridge key and PEM files and opens no socket. Run in `blocking`, cached per preset, so a viewer cannot make the server re-read its keys on every request. `api_token_file` is not checked (it is read on every call). `not_ready` when it fails, with "The server's Substrate settings for this environment are incomplete." and action "An operator should check this environment in the server configuration and run `by serve --check`." |
| `reachability` | omitted | Always `unknown` |
| `executable` (per harness) | The API host's PATH check (`GET /v1/harnesses`, `harness.rs:279-293`), shown as `ready` or `not_ready`. Informational: another worker may run the session, so it never refuses | Always `unknown` |
| `auth` (per harness) | `ready` when the preset supplies a secret the harness's provisioner accepts as its key (for `antigravity`, `GEMINI_API_KEY` or `GOOGLE_API_KEY`), else `unknown`. The server's own login does not count: console sessions are isolated, with a private home and credential variables removed (`lib.rs:1395-1401`) | Same rule |

Per harness, `tool_approvals` comes from the profile (`antigravity` has none, `branchyard-harness/src/antigravity.rs:244`); `effort` from a dry provisioning plan, as `check_model` does (`harness.rs:166-185`); `writer` is false when the harness lacks tool approvals and the operator has not set `allow_unapproved_tools` (`api.rs:653`); `reviewer` is false without tool approvals (`runtime.rs:490-493`). `can_start` combines these with the caller's role.

Discovery reads configuration, the PATH check, the cached offline check and stored worker beats. It never opens a provider, inspects an allocation, builds a Substrate client (`SubstrateProvider::connect` starts a runtime, `provider.rs:382-389`), runs a harness or starts a model call. It reports no cost or spend figures: nothing in the runtime enforces them.

## Starting a session with a preset

`StartSessionRequest` gains `preset: Option<String>`.

```json
POST /v1/workspaces/ws_01J…/sessions
{ "name": "Backlog analyst", "prompt": "Summarise the backlog by owner.",
  "preset": "substrate-gke", "harness": "antigravity", "model": "gemini-3.8-flash-low",
  "access": "writer", "unapproved_tools": true, "sources": ["src_01J…"],
  "result_contract": { "…": "unchanged" } }
```

Order, inside `start_once`:

1. The idempotent replay check stays first (`runtime.rs:474-483`), so a retry after a configuration change gets its recorded result. The fingerprint is the request body: it covers the preset ID, not the values the preset resolved to.
2. Workspace membership and the role for `access` (unchanged).
3. With `require_execution_presets`, a request without `preset` is `400 preset_required`.
4. The preset exists and lists the caller's tenant; otherwise `404 unknown_preset`.
5. With `preset`, the request may not carry `provider` or `provision`: `400 preset_conflict`. Model and effort travel only in the top-level `model` and `effort`, so the allowlist cannot be bypassed through `provision.model`, `logins`, `models` or `auth`.
6. `harness` is listed, `model` is in its `models` (or absent when none are listed), `effort` only where the plan accepts one: otherwise `422 preset_choice_unavailable` with `detail.field` and the allowed values.
7. `environment` known to be `not_ready` refuses with `409 preset_not_ready` and the same fixed reason and action. Nothing else refuses: `unknown` values, and an informational `executable`, let the run report its own failure.
8. The server sets `TaskRequest.provider` from the preset, adds its `secrets` and `require_labels`, and calls `work::task_options` as today.

No fallback: a session started with a remote preset never runs locally, and an older server rejects the unknown `preset` field (`deny_unknown_fields`, `workspace_api.rs:203`) rather than ignoring it. A session's JSON gains `preset`. A request without `preset` behaves exactly as today.

Recorded consequences:

- Continuations reuse the session's stored provider (`scheduling.rs:59`). Removing a tenant from a preset, or the preset itself, stops new sessions, not existing ones.
- A failed remote run's error may name server paths from the preset; workspace viewers see session errors. This depends on the managed-message redaction Codex is changing; the guided console adds no new error text of its own.
- `require_execution_presets` covers workspace sessions only. The direct task route (`POST /v1/repos/{repo}/tasks`) and delegation still accept `provider` under `allow_providers`; extending the requirement there touches `api.rs` and `work.rs`, outside this work's files, and is a follow-up.

## Source titles

`X-Source-Title` accepts visible ASCII only (`HeaderValue::to_str`, `console/sources.rs:107-111`), so the console transliterates today. Instead, uploads may send an RFC 8187 extended value in a header whose name is only letters, digits and hyphens (proxies such as nginx drop names with other characters by default, and [deploy](../../deploy.md) recommends one):

```text
POST /v1/workspaces/ws_01J…/sources
X-Source-Title-Encoded: UTF-8''%C3%89tude%20de%20march%C3%A9%20%E2%80%94%20T3.xlsx
X-Source-Title: Etude de marche - T3.xlsx
X-Workspace-Revision: 4
```

- With `X-Source-Title-Encoded`, `X-Source-Title` is ignored (not validated); it stays the form an older server reads, so clients always send an ASCII fallback beside it.
- Parsing: the raw value is at most 4 KiB; charset `UTF-8` in any case and no other; a language tag is accepted and ignored; the value part holds only RFC 8187 `attr-char` and well-formed `%XX`; decoding is strict (`percent_encoding::percent_decode_str(…).decode_utf8()`, never the lossy shared helper or a hand-written decoder, per `tools/handrolled_banlist.toml`).
- The decoded title is 1 to 512 bytes of UTF-8 without Unicode control characters (category Cc, including C1) or bidirectional overrides and isolates (U+202A to U+202E, U+2066 to U+2069), stored exactly as sent, not normalised. Anything else is `400 invalid_source_title`, naming the rule and the expected form.
- The upload fingerprint uses the decoded title (`sources.rs:118`).
- Downloads: `content()` sends only `attachment` today and serves sources and artifacts (`sources.rs:170-187`, `:522`). It will send `attachment; filename="<fallback>"; filename*=UTF-8''<encoded>`, where the fallback replaces non-ASCII, `"` and `\` with `_`.
- The bytes stay immutable and digests keep their algorithms (SHA-256 for sources, BLAKE3 for artifacts).
- The Python SDK's `upload_workspace_source` sends both headers. The console encodes with its own RFC 8187 encoder, since `encodeURIComponent` leaves `'()*` unencoded, and drops its transliteration.

## Docs, schemas and tests the change carries

`server.md` documents the new route and keys (its route list is checked, `docs_facts.py:360-369`) and updates its session start, source upload and security passages; `docs_facts.py --write` updates the route count; `schema/server.config.json` and `schema/contract.json` are regenerated; the Python SDK gains `execution_presets()` for the route-parity test (`tests/test_sdk_client.py`) and `preset` on session start. Tests cover preset resolution, tenant and member restrictions, local against remote selection, `require_execution_presets`, `preset_conflict` with `provision`, unknown and not-ready environments, no fallback, discovery without side effects, and title parsing at the boundary.

## File ownership

| Files | Owner in this work |
|---|---|
| `crates/branchyard-server/src/console/presets.rs` (new): configuration types, discovery, readiness, resolution; its tests | Guided console |
| `crates/branchyard-server/src/config.rs`: the two keys and their validation only | Guided console, reviewed here |
| `crates/branchyard-server/src/console/routes.rs`: mount discovery; `runtime.rs`: one call to `presets::resolve` after replay and before the `TaskRequest` is built; session JSON's `preset` | Guided console, reviewed here |
| `crates/branchyard-server/src/console/sources.rs`: title parsing; `content()`'s download header | Guided console, reviewed here |
| `crates/branchyard-client/src/workspace_api.rs`: `preset` on `StartSessionRequest`, discovery types and method; regenerated schemas | Guided console |
| `sdk/python`: upload headers, `preset`, `execution_presets()` | Guided console |
| `crates/branchyard-server/src/companion/assets/`, browser tests, docs | Guided console |
| `console/auth.rs`, operation observation in `api.rs`, managed-message redaction, the CLI cancellation test | Codex; not edited |

## Not in this contract

Provisioning or checking a cluster from the console, cost or spend limits, Agent Compute Engine, a console-side scheduler, presets for the direct task route, and per-tenant connector grant policy (none exists to validate against; connectors keep their current server-wide check).
