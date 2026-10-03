# Egress policy

A harness should reach only what its task needs: the connector gateway, a package registry, the git remote. A branch's **network policy** says which hosts those are. Branchyard applies it through an allowlisting proxy, and on Linux it confines a local harness to that proxy in a network namespace. Where it cannot confine, it says so, and a policy that must be enforced is refused.

This page also covers [permission presets](#permission-presets), the named tool policies that replace hand-written rules for common cases.

Status, 1 October 2026: built and tested hermetically against listeners on loopback and the fake ACP agent. No real harness has run under a policy.

## The policy

A policy is one of:

| Form | Meaning |
|---|---|
| `open` | Every host. The default, and what a branch without a policy has |
| `none` | No host at all, except the connector gateway when the turn has a grant |
| A list of rules | Only the hosts the rules allow |

A rule is `HOST[:PORT]`:

- `github.com` allows that name on every port. It does not allow `api.github.com`.
- `*.npmjs.org` allows every name under `npmjs.org`, such as `registry.npmjs.org`. It does not allow `npmjs.org` itself; list both if you need both.
- `*.npmjs.org:443` allows those names on port 443 only.
- `127.0.0.1:8931` and `[::1]:8080` are addresses. Only a rule naming an address (or `localhost`) reaches a loopback address; see [what is enforced](#what-is-enforced-and-what-is-not).

Rules are checked strictly: a URL, a path, a bare `*`, a wildcard anywhere but a leading `*.`, a wildcard over one label (`*.com`), a port outside 1 to 65535 and an IPv6 address without brackets are refused, naming the rule.

A policy also says what to do where it cannot be enforced: `best_effort` (the default) runs the turn with the proxy's variables only and records that it is advisory; `required` refuses to start.

### Setting it

| Where | How |
|---|---|
| `by run`, `fan`, `send`, `fork`, `reincarnate` | `--network open\|none\|HOSTS` (rules separated by commas) and `--network-enforce best-effort\|required` |
| `branchyard.toml` | `[network] allow = ["github.com", "*.npmjs.org:443"]` and `enforce = "required"`, for new branches when `--network` gives none; `allow = []` is `none` |
| A rig seat | `network = "none"`, or `network = { allow = [...], enforce = "required" }` |
| SDK | `Provisioning::network` (`branchyard::Network`, `NetworkEnforce`) in `TaskOptions::provision` |
| HTTP and `by --remote` | `provision.network` in a task, send or fork request: `"open"`, `"none"` or `{"allow": [...], "enforce": "required"}` |

The policy is stored with the branch, like the rest of its provisioning. A send or fork without one keeps the branch's.

### The connector gateway

A turn with a [connector grant](connectors.md) gets the gateway's host and port added to its rules for that turn, from the URL the harness is given (`[connectors] gateway`, or `sandbox_gateway` in a sandbox). It is not stored: the branch's own policy stays what you set. So `--network none --connector github:read` reaches the gateway and nothing else.

### The model gateway

A turn on the [model gateway](model-gateway.md) gets its gateway's host and port (`127.0.0.1:PORT`) added the same way, for that turn only, so `--network none --model-gateway` reaches the gateway and nothing else, and the provider's hosts need not be allowed: the gateway, in the engine's process, calls them. A turn whose harness is logged in by subscription runs direct, and gets its provider's hosts added instead (`api.anthropic.com:443` and `console.anthropic.com:443` for Claude Code; `api.openai.com:443`, `chatgpt.com:443` and `auth.openai.com:443` for Codex).

A person's ceiling (`ceilings` on a server, `Yard::use_ceilings`) caps the policy: each turn runs under its branch's policy within the ceiling, and an open policy takes the ceiling's rules ([one scope](model-gateway.md#one-scope)). The turn's token carries the effective policy and a digest of it (`by_network`).

### Delegated children

A child is never wider than its parent:

- A spawned child gets its seat's policy, or else its parent's.
- A seat's or a send's policy must be within the parent's: each of its rules must be covered by one of the parent's (the same or a narrower host, the same port or the parent's any-port), and `open` under a restricted parent is refused `denied`.
- The stricter enforcement of the two applies.
- `by rig check` refuses a seat whose policy is wider than its parent seat's, naming the field and line.

## How it is applied

Each turn of a branch with a restricted policy:

1. Starts a proxy in the engine's process that allows only the policy's rules (plus the gateway).
2. Gives the harness `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`, and their lower-case forms, pointing at it, and `NO_PROXY` and `no_proxy` empty: loopback goes through the proxy like any other host. These are set last, so nothing else the turn sets replaces them.
3. Records an `egress` event saying how the policy was applied.
4. Records one `egress` event for every destination the proxy decides.

### The proxy

`branchyard_runtime::egress::Proxy` is an HTTP/1 forward proxy, written for Branchyard on `std` threads.

- `CONNECT host:port` (TLS, or anything tunnelled): allowed, it connects and relays bytes both ways; it never sees inside the tunnel.
- `GET http://host/path` and the other methods with an absolute `http` URI: allowed, it sends the request to that host in origin form, with that host in `Host` and `Connection: close`, and drops `Proxy-*` headers. One connection cannot carry a request for another host.
- Denied: `403 Forbidden`, `X-Branchyard-Egress: denied`, and a body saying `Branchyard's egress policy does not allow HOST:PORT for this branch`.
- Not a proxy request (origin form, `https://` without `CONNECT`, user information in the target): `400`, not recorded.
- An allowed host that cannot be resolved or connected to: `502`, recorded as allowed with the reason.

The proxy resolves names itself, on this host. A name that resolves to a loopback or unspecified address is refused unless the rule that allowed it names an address or `localhost`.

### Confinement on Linux

A local harness on Linux runs in its own user and network namespace when this host allows unprivileged ones. Between `fork` and `exec`, the harness's process:

1. unshares a user namespace (so no privilege is needed) and a network namespace;
2. maps its user and group IDs to themselves, so files keep their owners;
3. brings the namespace's loopback up and binds a listener to `127.0.0.1:3128` there;
4. hands that listener to the engine over a socket pair, and execs the harness.

The namespace has no other interface. The harness and everything it starts reach only that listener; the engine accepts its connections in its own namespace and serves them with the proxy. A tool that ignores the proxy variables reaches nothing, not even DNS.

Support is detected at run time, once per process, by confining `/bin/sh -c 'exit 0'`. A kernel without unprivileged user namespaces, or a security module that forbids them or what they need (Ubuntu's AppArmor restriction, for example), makes that fail, and the reason is kept. `BRANCHYARD_EGRESS_NETNS=off` in `by`'s environment turns confinement off without trying.

### Per provider and system

| Where the harness runs | Applied | What holds |
|---|---|---|
| Local, Linux, unprivileged user namespaces allowed | **enforced** | Only the proxy is reachable |
| Local, Linux without them, or `BRANCHYARD_EGRESS_NETNS=off` | **advisory** | The proxy listens on this host's loopback; only tools that honor the variables are held to it |
| Local, macOS and other systems | **advisory** | As above |
| Microsandbox | **not applied** | The guest gets the runtime's default network and could not reach a proxy on this host's loopback |
| Agent Substrate | **not applied** | An actor reaches what its cluster allows; Substrate's egress policy is not vendored |
| A recipe's machine | **not applied** | Not wired to branches yet; its provider declares no confinement |

`required` refuses a turn wherever the table does not say enforced: at `run`, `send`, `fork` and spawn time when that is known, and otherwise when the turn starts, before the harness does.

### Provider capability

`SandboxProvider::exec_confined(name, spec, port)` starts a process whose only network is a listener on `127.0.0.1:port` inside its namespace, returned to the caller. `Capabilities::egress` declares it (`Operation::Egress` for `admit`). The local provider declares it exactly where confinement works; Microsandbox, Substrate and recipes declare it false, and the trait's default refuses `exec_confined` as unsupported, so no provider runs a process unconfined when asked to confine it. The [conformance suite](providers.md#conformance)'s `egress_confinement` check runs a confined Python script that must fail to reach a listener on the host and reach the returned listener, or, without the capability, requires the refusal.

## What you see

While a turn runs, its proxy is registered in the repository's [service registry](registry.md) as an `egress_proxy`: the branch, the enforcement, the rules in force, and the proxy's loopback URL (advisory) or the namespace it serves (enforced). `by services` lists it. The proxy lives in the engine's process, so there is nothing to reclaim: the record is deregistered when the turn ends, and reaped as soon as the engine is known gone.

| Where | What |
|---|---|
| `by run` and the other commands that follow a turn | `egress enforced: github.com (required)`, or `egress advisory: none (only tools that honor the proxy variables are held to it: …)` |
| `by log` | that line, then `egress allowed: CONNECT github.com:443 by github.com` and `egress denied: GET 127.0.0.1:8080 (no rule allows it)` |
| `by log --json`, event streams | `{"activity": "egress", "egress": {"kind": "applied", "policy", "allow", "enforcement", "reason"}}` and `{"kind": "decision", "method", "host", "port", "allowed", "rule", "reason"}`, with `text` |
| `by show` | `egress  enforced (github.com); 3 allowed, 1 denied`, and for an advisory turn `; not enforced: …` |
| `by show --json` | `egress: {policy, allow, enforcement, reason, allowed, denied}` for the last turn with a policy |
| `by watch` | the latest decision as what the branch is doing |

`enforcement` is `enforced`, `advisory` or `not_applied`. The SDK's types are `EgressActivity` and `EgressEnforcement`.

## What is enforced, and what is not

Enforced, on Linux with confinement:

- A harness and its descendants have no network but the proxy: no other host, no raw IP, no UDP, no ICMP, no DNS server.
- The proxy allows only the rules; every decision is recorded.
- A name an allowed rule matches cannot lead to this host's loopback unless the rule names the address.

Not enforced, or not by this:

- **Advisory and not-applied turns.** A tool that ignores the variables, or connects to an IP address, a UDP port or a DNS server itself, is not held to anything. The event and `by show` say so.
- **What an allowed host is used for.** The proxy never sees inside a tunnel: data can leave through an allowed host (a gist on `github.com`), and a client can name one host in `CONNECT` and another in its TLS server name (domain fronting through a CDN).
- **DNS.** The proxy resolves on this host. A name an allowed rule matches may resolve to an address on your private network (DNS rebinding); only loopback and unspecified addresses are refused.
- **Unix sockets.** A network namespace does not cover the filesystem. The harness can still connect to Unix sockets it can open on this host, such as a container runtime's socket or Branchyard's own delegation socket. Abstract sockets are per namespace and are not reachable.
- **The harness's own API.** Off the model gateway, the harness talks to its model provider through the same proxy: a policy must allow it, such as `api.anthropic.com:443` for Claude Code. Branchyard adds it only for a branch on the [model gateway](model-gateway.md) whose harness runs direct; on the gateway, only the gateway is added.
- **Proxy-unaware harnesses.** A harness or tool that does not honor `HTTPS_PROXY` cannot reach anything when confined.
- **What runs outside the turn.** `[workspace]` setup and teardown scripts and the merge check run in the engine's network, not the harness's.
- **Root.** Under a `by` running as root, the harness is root in its user namespace and may reconfigure its own network namespace; it still has no interface to the host's.
- **A server's operator** caps a request's policy with a principal's ceiling (`ceilings`), applied by intersection each turn; a request still chooses within it.

## Permission presets

A preset is a named permission policy that stands for explicit rules. It is usable wherever explicit rules are:

| Where | How |
|---|---|
| `by run`, `fan`, `send`, `fork`, `spawn`, `reincarnate` and the other commands that take `--yes` | `--permissions read-only\|edit-worktree\|full` (not with `--yes` or `--ask`) |
| `branchyard.toml` | `[defaults] permissions = "read-only"` (besides `ask` and `yes`) |
| A rig seat | `permission_policy = "edit-worktree"` |
| HTTP and `by --remote` | `policy: {"preset": "read-only"}` in a request |
| SDK | `PolicyPreset::policy()`, `PolicyPreset::rules()` |

| Preset | Allows | Denies | Then |
|---|---|---|---|
| `read-only` | reading and searching: `Read`, `Grep`, `Glob`, `LS`, `NotebookRead`, `TodoWrite`, `read_file`, `read_many_files`, `list_directory`, `glob`, `search_file_content`, `grep`, `view` | edits, commands and the web | deny |
| `edit-worktree` | the same, and edits: `Edit`, `Write`, `MultiEdit`, `NotebookEdit`, `fileChange`, `write_file`, `replace` | commands (`Bash`, `commandExecution`, `run_shell_command`, `shell`, …) and the web (`WebFetch`, `WebSearch`, …) | deny |
| `full` | everything, as `--yes` | nothing | allow |

`read-only` is the policy a [planning turn](plans-and-goals.md) runs under. Rules match tool names only: `edit-worktree` says a harness may edit, not where; the harness's own tools decide what path an edit touches.

How a preset combines:

- In a request, its rules follow the request's own rules, and its default replaces `mode`.
- On a rig's root seat, the seat's own `policy.deny` and `policy.allow` come first, then the preset's, and the seat's `policy.default` wins over the preset's. `by rig check` prints `policy preset edit-worktree (…)` and the seat's own rules; `--json` gives the expanded rules and `preset`.
- On a child seat, which may only add denials, a preset adds its denials: `read-only` denies the edit, command and web tools; `full` adds nothing.
- A top-level `permission_policy` in a rig is refused: a preset belongs to a seat.

## Tests

- `branchyard-provision` (6): rule parsing and printing, matching hosts, ports and subdomains, the wire form and its refusals, the flag form, the gateway rule, narrowing.
- `branchyard-runtime` (7, `tests/egress.rs`): the proxy against local listeners (plain HTTP forwarded with its host rewritten and `Proxy-*` dropped, `CONNECT` tunnelled with early data, denials with `403` and the denied listener untouched, a name leading to loopback refused, non-proxy requests refused and not reported, a dropped proxy closed), a confined Python process that reaches a listener only through the proxy, and the local provider declaring `egress` exactly where it can confine. Two unit tests of target parsing and loopback.
- `branchyard-sandbox`: the `egress_confinement` conformance check, run against the local provider.
- `branchyard` (4 unit, 6 engine): the gateway rule, the proxy variables, a sandbox provider refusing a required policy, descriptions; a confined harness (the fake ACP agent running Python) reaching an allowed listener through the proxy and nothing directly (`tests/egress.rs`); with confinement off (`tests/egress_advisory.rs`) an advisory turn, a required policy refused before anything is created, an open policy recording nothing, the gateway allowed for a granted turn and not stored, and children narrowed by seat, by inheritance and on a send. A preset test in `policy.rs` checks each preset's decisions and that `read-only` matches the planning policy.
- `branchyard-client` (1): a request's preset after its rules, with its default.
- `branchyard-server` (1): over HTTP, a request's `policy.preset` decides the fake agent's permission request both ways, and `provision.network` reaches the engine, which records how it applied it.
- `branchyard-setup` (1): `[network]` and preset permissions parsed, refused and rendered back.
- `by` (3 unit, 3 with the built `by`): `--network`, `--network-enforce` and `--permissions` parsing; rig seats' networks and presets; then an enforced turn shown by `by run`, `by show`, `by show --json`, `by log` and `by log --json`; with confinement off, a required policy refused, a best-effort policy from `branchyard.toml` running advisory, a bad rule refused, `by config validate` refusing a bad `[network]`; and presets deciding the fake agent's permission request, from the flag and from `[defaults]`.

The enforced tests need a host that allows unprivileged user and network namespaces. Elsewhere they print `SKIPPED` with the reason; they fail if `unshare -rn true` works but Branchyard's confinement does not. They pass as root and as `nobody`.

## Not done

- No real harness has run under a policy; whether each honors `HTTPS_PROXY` for everything it does is untested.
- macOS has no enforcement.
- Microsandbox and Substrate do not apply a policy at all; routing a guest to the proxy, or using the runtime's own egress controls, is future work.
- `by spawn` has no `--network` of its own: a child narrows through its seat or inherits its parent's.
- A server's operator can cap a request's policy per principal (`ceilings`), but not set a default for requests that name none.
