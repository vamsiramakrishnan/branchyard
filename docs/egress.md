# Egress policy

A harness should reach only what its task needs: the connector gateway, a package registry, the git remote. A branch's **network policy** says which hosts those are. Branchyard applies it through an allowlisting proxy, and on Linux it confines a local harness to that proxy in a network namespace. Where it cannot confine, it says so, and a policy that must be enforced is refused.

This page also covers [permission presets](#permission-presets), the named tool policies that replace hand-written rules for common cases, and the [private temporary directory](#a-private-temporary-directory) every local harness gets.

Status, 10 October 2026: built and tested hermetically against listeners on loopback, plain and TLS, and the fake ACP agent, with Python's `urllib` as the client through the TLS interception. No real harness has run under a policy.

## The policy

A policy is one of:

| Form | Meaning |
|---|---|
| `open` | Every host. The default, and what a branch without a policy has |
| `none` | No host at all, except the connector gateway when the turn has a grant |
| A list of rules | Only the hosts the rules allow |

A rule is `[METHOD|METHOD ]HOST[:PORT][/PATH]`:

- `github.com` allows that name on every port. It does not allow `api.github.com`.
- `*.npmjs.org` allows every name under `npmjs.org`, such as `registry.npmjs.org`. It does not allow `npmjs.org` itself; list both if you need both.
- `*.npmjs.org:443` allows those names on port 443 only.
- `127.0.0.1:8931` and `[::1]:8080` are addresses. Only a rule naming an address (or `localhost`) reaches a loopback address; see [what is enforced](#what-is-enforced-and-what-is-not).
- `GET|HEAD api.github.com` allows only those methods, upper-cased and separated by `|`, with one space before the host.
- `api.github.com/repos/*` allows only paths under `/repos/`: a pattern is `/`-separated segments, a segment `*` matches any one segment, and a trailing `*` matches everything below it as well (`/repos/x` and `/repos/x/issues/1`, not `/repos`). The query string is never matched. `/` alone matches the root only.
- `POST api.anthropic.com:443/v1/messages` combines them.

A rule that names methods or a path **inspects** the host: the proxy looks inside each connection to it, so the rule can be held to. A rule that names only a host and port stays a blind tunnel. See [inspected hosts](#inspected-hosts).

Rules are checked strictly: a URL, a bare `*`, a wildcard anywhere but a leading `*.`, a wildcard over one label (`*.com`), a port outside 1 to 65535, an IPv6 address without brackets, a comma inside a rule, a wildcard that is part of a path segment (`/a*`), an empty path segment and a query string in a path are refused, naming the rule.

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
- A seat's or a send's policy must be within the parent's: each of its rules must be covered by one of the parent's (the same or a narrower host, the same port or the parent's any-port, a subset of the parent's methods or any under a parent naming none, a path at or below the parent's or any under a parent naming none), and `open` under a restricted parent is refused `denied`. So `GET api.github.com/repos/*` is within `api.github.com`, and `api.github.com` is not within it.
- The stricter enforcement of the two applies.
- `by rig check` refuses a seat whose policy is wider than its parent seat's, naming the field and line.

## How it is applied

Each turn of a branch with a restricted policy:

1. Starts a proxy in the engine's process that allows only the policy's rules (plus the gateway).
2. Gives the harness `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`, and their lower-case forms, pointing at it, and `NO_PROXY` and `no_proxy` empty: loopback goes through the proxy like any other host. These are set last, so nothing else the turn sets replaces them.
3. When a rule inspects a host, gives the proxy the branch's certificate authority and the harness the [trust variables](#inspected-hosts) naming a bundle of the system's roots and that authority.
4. Records an `egress` event saying how the policy was applied.
5. Records one `egress` event for every destination the proxy decides, and one for every request it sees.

### The proxy

`branchyard_runtime::egress::Proxy` is an HTTP/1 forward proxy, written for Branchyard on `std` threads.

- `CONNECT host:port` (TLS, or anything tunnelled) to a host whose rules name no method or path: allowed, it connects and relays bytes both ways; it never sees inside the tunnel.
- `CONNECT host:port` to a host every allowing rule inspects: allowed, it answers `200`, then terminates what comes inside (TLS with a certificate for that host, or plain HTTP) and reads one request, decided by its method and path like a plain one; see [inspected hosts](#inspected-hosts).
- `GET http://host/path` and the other methods with an absolute `http` URI: decided by host, port, method and path; allowed, it sends the request to that host in origin form, with that host in `Host` and `Connection: close`, and drops `Proxy-*` headers. One connection cannot carry a request for another host.
- Denied: `403 Forbidden`, `X-Branchyard-Egress: denied`, and a body saying `Branchyard's egress policy does not allow HOST:PORT for this branch`, or `METHOD HOST:PORT/PATH` for a request it saw.
- Not a proxy request (origin form, `https://` without `CONNECT`, user information in the target): `400`, not recorded.
- An allowed host that cannot be resolved or connected to: `502`, recorded as allowed with the reason.

The proxy resolves names itself, on this host. A name that resolves to a loopback or unspecified address is refused unless the rule that allowed it names an address or `localhost`.

### Inspected hosts

A rule naming a method or a path can only be held to by reading the request, and an HTTPS request is inside TLS. So for a host every allowing rule inspects, the proxy does what httpjail and boundary do, with the pieces Branchyard already had (`rcgen`, `rustls`):

1. **A certificate authority per branch**, made on the first inspected turn and kept under `.branchyard/egress/<branch>/`: `ca.key` (readable by its owner only; it never leaves the engine), `ca.pem`, and `ca-bundle.pem`, rewritten each turn: the system's roots (the engine's own `SSL_CERT_FILE`, else the first of `/etc/ssl/certs/ca-certificates.crt`, `/etc/pki/tls/certs/ca-bundle.crt`, `/etc/ssl/ca-bundle.pem`, `/etc/pki/tls/cacert.pem`, `/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem`, `/etc/ssl/cert.pem`), then the authority. The directory goes with the branch.
2. **Trust variables** on the harness, set only when the policy inspects, each naming the bundle: `SSL_CERT_FILE` (OpenSSL, Go, Python's `ssl`), `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS` (Node, Bun), `DENO_CERT`, `CARGO_HTTP_CAINFO` and `GIT_SSL_CAINFO`. Because the bundle holds the system's roots too, nothing the harness trusted before stops being trusted.
3. **Termination.** The proxy answers the `CONNECT`, looks at the first byte, and for TLS presents a certificate for the `CONNECT` host (a DNS name or an IP address), signed by the authority, offering HTTP/1.1 alone in ALPN. A plain HTTP request inside the tunnel is read as it is.
4. **One request**, decided by the policy: its method and path against every rule for the host. A `Host` header naming another host is refused. Denied: `403` inside the connection, with the same header and body as outside. Allowed: the request goes to the origin in origin form with `Connection: close`, over TLS of the proxy's own (verified against the Mozilla roots and the system's), its body copied whole (`Content-Length` or chunked, `Expect: 100-continue` answered), and the response is relayed until the origin closes. Nothing sent after the request is forwarded; a client's next request opens a new connection and is decided anew.
5. **Records.** The `CONNECT` is recorded allowed with the reason `inspected`, then each request inside with its method and path, allowed or denied. A handshake the harness refuses (it does not trust the authority) is recorded on the `CONNECT` with the reason.

A host with a host-only rule and an inspected rule is not inspected: the host-only rule allows every connection, so there is nothing to look inside for, and the proxy stays out of it. Narrow the host-only rule if you want the inspection.

What this does not do: HTTP/2 and WebSockets inside an inspected connection (ALPN offers HTTP/1.1 only, and a connection closes after its one response); a client that pins the origin's certificate, or a Rust tool built on the `webpki-roots` bundle alone, which reads none of the variables and will refuse the proxy's certificate; Java, which reads none of them either. Such a tool reaches an inspected host only through a host-only rule, which keeps the tunnel blind.

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
| `by log` | that line, then `egress allowed: CONNECT github.com:443 by github.com`, `egress denied: GET 127.0.0.1:8080/x (no rule allows it)`, and for an inspected host `egress allowed: CONNECT api.github.com:443 by GET api.github.com/repos/* (inspected)` followed by `egress allowed: GET api.github.com:443/repos/x by GET api.github.com/repos/*` |
| `by log --json`, event streams | `{"activity": "egress", "egress": {"kind": "applied", "policy", "allow", "enforcement", "reason", "ca_bundle"}}` (`ca_bundle` when the policy inspects) and `{"kind": "decision", "method", "host", "port", "path", "allowed", "rule", "reason"}` (`path` for a request the proxy saw), with `text` |
| `by show` | `egress  enforced (github.com); 3 allowed, 1 denied`, and for an advisory turn `; not enforced: …` |
| `by show --json` | `egress: {policy, allow, enforcement, reason, allowed, denied}` for the last turn with a policy |
| `by watch` | the latest decision as what the branch is doing |

`enforcement` is `enforced`, `advisory` or `not_applied`. The SDK's types are `EgressActivity` and `EgressEnforcement`.

## What is enforced, and what is not

Enforced, on Linux with confinement:

- A harness and its descendants have no network but the proxy: no other host, no raw IP, no UDP, no ICMP, no DNS server.
- The proxy allows only the rules; every decision is recorded.
- A name an allowed rule matches cannot lead to this host's loopback unless the rule names the address.
- To an inspected host, only requests whose method and path a rule allows, one per connection, with a `Host` header naming that host; the certificate names the `CONNECT` host alone, so a client naming another host in its TLS server name fails its own verification.

Not enforced, or not by this:

- **Advisory and not-applied turns.** A tool that ignores the variables, or connects to an IP address, a UDP port or a DNS server itself, is not held to anything. The event and `by show` say so.
- **What a host-only rule is used for.** The proxy never sees inside a tunnel it was not told to inspect: data can leave through an allowed host (a gist on `github.com`), and a client can name one host in `CONNECT` and another in its TLS server name (domain fronting through a CDN). A rule naming a method or path closes both for that host, at the price of the proxy reading the requests; what an allowed request carries in its body is still not read.
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

An ACP agent's request is matched by the tool name the agent gives (in the request, or in the call's earlier `tool_call` update), else by the call's kind under the names `acp:read`, `acp:search`, `acp:edit`, `acp:delete`, `acp:move`, `acp:execute` and `acp:fetch`, which the presets list; a request that carries only a title, such as Gemini CLI's "Writing to notes/x.md", used to match nothing and so was denied by every preset.

`read-only` is the policy a [planning turn](plans-and-goals.md) runs under. A sandboxed Claude Code turn also writes the denials of its preset that Claude Code has names for into managed settings, which its own settings cannot override ([managed settings](provisioning.md#managed-settings)).

A preset is also the last layer of [approvals](effects.md#approvals) for connector operations (`PolicyPreset::approvals`): `read-only` and `edit-worktree` block every reversible, compensable and irreversible operation, since they change nothing outside the machine; `full` keeps the class defaults (reversible allowed, compensable asked, irreversible staged, deletions asked). Approvals that name a tool only tighten what the preset allowed. Rules match tool names only: `edit-worktree` says a harness may edit, not where; the harness's own tools decide what path an edit touches.

How a preset combines:

- In a request, its rules follow the request's own rules, and its default replaces `mode`.
- On a rig's root seat, the seat's own `policy.deny` and `policy.allow` come first, then the preset's, and the seat's `policy.default` wins over the preset's. `by rig check` prints `policy preset edit-worktree (…)` and the seat's own rules; `--json` gives the expanded rules and `preset`.
- On a child seat, which may only add denials, a preset adds its denials: `read-only` denies the edit, command and web tools; `full` adds nothing.
- A top-level `permission_policy` in a rig is refused: a preset belongs to a seat.

## Tests

- `branchyard-provision` (10): rule parsing and printing, matching hosts, ports and subdomains, matching methods and paths, which hosts a policy inspects, path patterns covering one another, the wire form and its refusals, the flag form, the gateway rule, narrowing (methods and paths included), intersection.
- `branchyard-runtime` (11, `tests/egress.rs`): the proxy against local listeners (plain HTTP forwarded with its host rewritten and `Proxy-*` dropped, `CONNECT` tunnelled with early data, denials with `403` and the denied listener untouched, a name leading to loopback refused, non-proxy requests refused and not reported, a dropped proxy closed); an inspected connection against a TLS origin (a `GET` and a `POST` with a body relayed, the wrong method, a path outside the rule and a foreign `Host` refused inside, a client that does not trust the authority recorded as such); plain HTTP inside an inspected tunnel, with and without early data and a `Host` header; Python's `urllib` through `HTTPS_PROXY` trusting the authority through `SSL_CERT_FILE`; an inspected rule without an interceptor refused; a confined Python process that reaches a listener only through the proxy; and the local provider declaring `egress` exactly where it can confine. Five unit tests: target parsing, request paths and `Host` checks, a chunked body copied through its last chunk, an authority through PEM and its per-host certificates, loopback.
- `branchyard-sandbox`: the `egress_confinement` conformance check, run against the local provider.
- `branchyard` (5 unit, 7 engine): the gateway rule, the proxy variables, the trust variables and the decision the policy gives, a sandbox provider refusing a required policy, descriptions; a confined harness (the fake ACP agent running Python) reaching an allowed listener through the proxy and nothing directly (`tests/egress.rs`); with confinement off (`tests/egress_advisory.rs`) an advisory turn, a required policy refused before anything is created, an open policy recording nothing, the gateway allowed for a granted turn and not stored, children narrowed by seat, by inheritance and on a send, and an inspected rule: the harness's Python reaches a TLS origin through the proxy with `GET` and is refused `POST`, a plain request is held to the same rule, the bundle holds the engine's roots then the authority with the key owner-readable, the trust variables name it, the authority survives a second turn and goes with the branch. A preset test in `policy.rs` checks each preset's decisions and that `read-only` matches the planning policy.
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
- Inside an inspected connection: no HTTP/2, no WebSockets, one request per connection. A tool that reads none of the trust variables (a Rust tool on `webpki-roots` alone, Java) cannot reach an inspected host; give it a host-only rule.
- No real harness has run against an inspected rule; which of their tools honor the trust variables is untested. The variables are the seven httpjail documents; Claude Code (Node) reads `NODE_EXTRA_CA_CERTS`, and Codex (Rust, native roots) `SSL_CERT_FILE`.

## A private temporary directory

Branches share a host, and so its `/tmp`. A file one branch's harness leaves there can change what another runs: a stray `/tmp/inspect.py`, left by one session, shadowed Python's own `inspect` module in another that ran a script from `/tmp`. So every local harness runs with `TMPDIR`, `TMP` and `TEMP` set to a directory of its own branch, `.branchyard/tmp/<branch>`, created readable by its owner only (0700) before each turn. It lives as long as the branch, so a turn finds what the branch's earlier turns left there, and goes when the branch is removed. Each harness is also told, in its instructions or its delegation skill, to put scratch files there rather than in `/tmp`, since a harness can still name `/tmp` itself; Claude Code keeps its own temporary files (its background tasks' output among them) under `TMPDIR` as well.

A sandboxed harness has a `/tmp` of its own already and is not given one. `XDG_CACHE_HOME` is left alone on purpose: caches (pip's, npm's, cargo's) are keyed by content and safe to share, and sharing them is what makes a second branch's setup fast; a branch run with a private home (`--isolated`) has a private cache under it anyway.

`crates/branchyard/tests/claude_turns.rs` (`every_branch_has_its_own_temporary_directory`) checks the three variables, the mode, that two branches get different directories, and the removal.
