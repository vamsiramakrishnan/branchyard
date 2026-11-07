# Tools: one model for connectors, skills and helpers

Written 5 November 2026. Status: **design; section 3 is built, section 4 is partly built (the manifest and its enforced digests, the policy version and the recorded server ceiling, renewing tokens, sleep and resume, the shared tool store and host install, egress, and the credential proxy in the sandbox with the server's token fetch), section 5's console is built with the helper limit, a person's own policy and installing a helper's harness on the server, the rest is not.** It answers four questions from the console work in [#67](https://github.com/vamsiramakrishnan/branchyard/pull/67): how a person's identity and permissions reach Anvil's gateway, how a tool is installed, allowed and chosen per query, how choosing fewer tools saves context, and how all of it runs on GCE, GCE with Microsandbox, GKE and Substrate. The audits it rests on read the code at `af17285d` plus #67, and Anvil at `8e990cb`. Each claim about today names its file.

## The problem in four lines

- **The model is unclear.** A connector, a skill and a harness each have their own word for "may I use it", stored in different places: server JSON, `branchyard.toml`, presets, the browser.
- **Choosing less saves almost nothing.** About 90 KB reaches every turn whatever was chosen: Branchyard's MCP tool list (41.8 KB, 35 tools, never filtered, `crates/branchyard-mcp/src/lib.rs:857`), the delegate skill (38.5 KB) and the operator notes in the MCP instructions (7.5 KB). A turn also copies about 425 KB of skills into a fresh sandbox, every turn.
- **Remote is not closed.** A worker signs gateway tokens with its own key file, made silently if missing (`KeyRing::load_or_create`). The issuer defaults to the listen address. Tokens last at most an hour and are never refreshed. Delegation and the effects proxy are unreachable from every sandbox but Microsandbox (`reaches_host`). No sandbox enforces egress.
- **The console shows it twice.** Connections sit in the sidebar, while tools sit in a form step of their own.

## 1. The model

Every tool is one of three kinds:

- a **connector**: an upstream API through Anvil, called as code;
- a **skill**: instructions and scripts on disk;
- a **helper**: a harness a session may delegate to.

Each kind passes through the same five states. Each state narrows the one before it, and each has one owner:

| State | Means | Owner | Connector | Skill | Helper |
|---|---|---|---|---|---|
| **Available** | It exists here | Operator | The gateway serves the bundle | It is in the skill registry (built in, organisation or repository) | An environment (preset) offers it |
| **Allowed** | This person may use it, at most this much | Administrator | A ceiling grants `read` or `write` | Policy allows the skill or its source | Policy allows delegation to it |
| **Ready** | It is set up for this person | Person | An account is connected in the vault | The skill is installed in their library | It is signed in for the environment |
| **On by default** | Sessions here start with it | Workspace owner | Workspace default | Workspace default | Workspace default |
| **Chosen** | This query uses it | Person, per query | In the session's grant | Staged for the turn | In the turn's envelope |

The rule is that each state's set sits inside the one before: Chosen ⊆ Ready ⊆ Allowed ⊆ Available, and On by default only suggests within Ready. The server computes every row for the caller. A tool is shown at the first state it fails, with that state's one action:

- **Ask your administrator** (not allowed).
- **Connect**, **Install** or **Sign in** (not ready).
- A plain tick (ready).

Admission computes it again, so the browser never decides.

**One API for all three kinds.** It replaces the per-kind routes:

- `GET /v1/tools?workspace=` lists each tool with its `kind`, `state`, `max` (for example `read`), its default, `action` and `reason`.
- `PUT /v1/workspaces/{workspace}/tools` stores the workspace's defaults, guarded by a revision.
- A session start takes one field: `tools: [{kind, id, mode?}]`.

The routes added in #67 become aliases.

**One place for policy.** Policy lives in the server's store (SQLite, or Cloud SQL for PostgreSQL), keyed by tenant and group, not only by principal name. An administrator edits it through `/v1/admin/policy`. Workers do not read it from their own JSON (section 4).

## 2. Identity, end to end

| Hop | Today | Proposed |
|---|---|---|
| Person → console | A bearer token, a paired device token, or a Google ID token matched to an exact `(iss, sub)` binding (`auth.rs:58`) | **IAP** in front of the console (a GKE Gateway or Cloud Run). The server verifies `x-goog-iap-jwt-assertion`. The tenant comes from the domain or a mapping, and groups from the Cloud Identity Groups API (cached for five minutes). Bearer and paired tokens stay for the CLI and devices |
| Console → policy | Ceilings keyed by principal name; presets by tenant | Policy by tenant and group (section 1); the person's effective policy is computed per request |
| Server → gateway | A turn JWT (Ed25519) signed with `<data_dir>/gateway/key`; `iss` defaults to the listen URL | Signed by **Cloud KMS** (`EC_SIGN_ED25519`): server and workers sign through IAM, the JWKS comes from the KMS public key, and no key file exists. `issuer` is required whenever a gateway is remote |
| Gateway → vault | `by:v1:sha256([iss, tenant, sub])`, as a file per connection on one machine | Same owner key. The vault moves to Firestore, envelope-encrypted with Cloud KMS, so Cloud Run instances share it. Adds a **disconnect** route (built in Anvil's PR #91; the console does not call it yet) |
| Gateway → upstream | OAuth or a stored key | Unchanged; Anvil's `SecretManagerCredentialResolver` already serves operator-owned keys |

Two rules fall out of the owner key:

- The issuer and a person's tenant are part of their identity at the gateway. Changing either orphans their connections.
- So the issuer is set once per deployment and checked at start, and moving a person between tenants is an explicit migration.

## 3. Context: a mounted tool tree, searched, not pasted

**Status: built (5 November 2026), hermetic only.** What is built and measured is listed in "Built" at the end of this section. Not done: any run of Claude Code or Codex with a model against the tree or the native links; a package from a real Anvil placed into a turn's tree by an engine run (the search reads Anvil's `search.json` strictly, and a package without one tolerantly from `harness.json`, `compose.json`, `schemas/` and `reference/`; both are tested against the [Anvil mock](anvil-contract-v2.md#the-mock), and the strict reader was also run on 5 November 2026 on a package the built Anvil wrote, `tests/test_anvil_real.py`, which is skipped where no built Anvil exists); `/tools` on a live Microsandbox, container or GKE guest (it is built and tested against fakes, below); a `/tools` for a local harness, a Substrate actor or a recipe's machine, which keep the tree under the home.

A turn sees only what was chosen. Everything it sees, past one line, is on disk and found by search.

```text
/tools/                         read-only, per turn
  INDEX.md                      ≤ 4 KB: one line per chosen tool (name: one sentence), and how to search
  bin/tools                     `tools search WORDS` · `tools show ID` · `tools schema CONNECTOR.OPERATION`
  skills/<name>/SKILL.md        frontmatter (name, description) first; references and scripts beside it
  connectors/<id>/SKILL.md      Anvil's package: SKILL.md, reference/, bin/<id>, python/, typescript/
  connectors/_compose/          Anvil's compose SDK, when two or more connectors are chosen
  helpers/INDEX.md              each helper: what it is good at, its cost and how to delegate
```

- **The prompt gets one line:** "Your tools are under /tools: read /tools/INDEX.md, and search with `tools search` before reading a skill." Unchosen tools cost nothing. The line names `/tools` where a provider controls the guest filesystem (below) and the tree's path under the home everywhere else.
- **`/tools` is real where the provider can make it so.** The tree is written under the home, `.branchyard/tools/`, as before; a local harness uses that path. A provider that controls the guest filesystem also puts it at the root, sets `BRANCHYARD_TOOLS=/tools` in the guest, and the tree's two links then name their targets by absolute guest path (`/branchyard/home/.branchyard/capabilities`), because a relative link would climb out of `/tools` to nothing:
  - **Microsandbox:** a read-only mount of the host's `.branchyard/tools` at `/tools`, a live view of the tree the engine writes.
  - **GKE Agent Sandbox:** an `emptyDir` volume `tools` mounted at `/tools`, and **container:** a private volume at `/tools`. Both copy the home in, so the engine copies the tree into this volume after the home. It is a copy, written by the engine and not read-only (the root filesystem is, and the engine's exec writes to it); what a harness writes there is dropped with the allocation and never copied back.
  - **Substrate and recipe machines** cannot mount, and a local harness has no guest root: they keep the home path.
  The `tools` script reads `--root`, then `BRANCHYARD_TOOLS_DIR`, then `BRANCHYARD_TOOLS`, then the directory above its own `bin/`. The `find-tools` skill names the path with `$BRANCHYARD_TOOLS` first.
- **Connectors are code.** The harness calls Anvil's CLI (`bin/<id> --schema`, `--dry-run`) or its Python and TypeScript SDK through the gateway, as Anvil intends (its `harness-package.ts`). No MCP server is added per connector.
- **Operations are found, not listed.** Anvil's SKILL.md lists capabilities with operation counts; the full table is `reference/operations.md`. `tools search` ranks with BM25 over the frontmatter of the chosen skills, each connector's operations and each helper. A connector's operations come from its `search.json` (written by `anvil package harness`; [Anvil contract v2, item 3](anvil-contract-v2.md#3-operation-search-index-and-a-lean-skillmd), built in Anvil), read strictly: every field is validated, and a malformed index is refused with its file and field named, none of that connector's operations are searched, the rest still answer, and `tools` exits 3. Only a package without a `search.json` is read tolerantly, from `harness.json`, `compose.json`, `schemas/` and `reference/`. `tools schema` prints one operation's schema. A skill named `find-tools` teaches the habit in about 600 characters. This is the skill-search skill.
- **References are documents too (built 7 November 2026, hermetic).** Each `references/*.md` of a staged skill is a document of its own, `SKILL/references/FILE.md`, indexed by the skill's and the file's name, its first heading and the content words of its body (no code fences, link targets, `Source:`/`License:` lines or English function words), so a search lands on one paragraph of a skill without the model reading the skill, and `tools show pptx/references/layouts.md` prints that file alone. A reference adds nothing to the corpus statistics, so the references a tree carries never reorder the tools among themselves (a test holds this). A tool `INDEX.md` lists (chosen for the turn: a skill, a connector and its operations, a helper) scores 1.5 times one it does not, and a reference is scored on its words alone, so a tool of equal match is read before its reference; `tools search --plain` is BM25 alone, which the eval measures beside it. Ids are `[kind:][source:]name[/references/FILE.md]`. The delegate skill's references stay out of the index: `delegate` is not staged ([skill registry](skill-registry.md#3-the-index-and-search)).
- **Native discovery too.** The chosen skills are also linked where a harness discovers skills itself (Claude Code's `.claude/skills/`, Codex's skills directory), so a harness that loads only frontmatter loads only these.
- **Branchyard's own surface follows the choice:**
  - Without helpers, the turn gets `leaf.md` (6.6 KB) instead of the delegate skill's core (7.5 KB; the skill was 38.5 KB before its details moved into 14 reference files), and the MCP server lists no spawn, delegate or graph tools.
  - Without outputs, it lists no artifact or scratch tools.
  - The operator paragraphs move out of the MCP instructions into a file.
  - The Branchyard surface of a plain Work turn falls from about 90 KB to under 15 KB, enforced by `tools/model_surface_budget.json` per case.

**Measuring discovery.** Whether a model searches, finds and opens the right tool through this tree is measured by [the skill-discovery eval](skill-eval.md): 55 scenarios, a scorer over `by log --json`, a runner, the static sizes a turn pays for, and `rank`, which runs each scenario's prompt through `bin/tools` over a tool tree without a model (7 November 2026, hermetic: top-3 rate 98%, MRR 0.800 over the shipped skills and the Anvil mock's packages; [ranking without a model](skill-eval.md#ranking-without-a-model)). Not done: any live run.

**Helpers in the tree (built 5 November 2026).** A turn whose grant has helpers (`Grant::has_helpers`) gets `helpers/INDEX.md`: the envelope (most children at once, levels deep) and one `- id: Name, models a, b, c` line for each harness the envelope allows, from `catalog/harnesses.toml`; a harness the catalog does not know is listed by id. The catalog records a harness's name and models and not what it is best at, so a line says what it is and does not claim strengths. An envelope with no harnesses says helpers run the turn's own. A turn with helpers also gets the delegate skill's reference files staged at `helpers/delegate/references/NAME.md` (removed for a leaf), and `helpers/INDEX.md` points at them; the skill's table names each. They are not yet ranked by `tools search`, and `delegate` is not in `capabilities.json`, so it is not a searchable skill. Making them searchable is a separate change. The file is at most 2 KB (`HELPERS_BUDGET`; lines past it become "N more"), `INDEX.md` gains a `## Helpers` pointer when it exists, `tools search --kind helper WORDS` finds a helper, and a leaf (no children or no depth) gets no file: one an earlier turn wrote is removed. Evidence: hermetic, `cargo test -p branchyard --lib tooltree` (the file, its budget, the removal, a repeated mount unchanged, and the `tools` script finding `codex`). Not done: a model choosing a helper from the file.

**Built.** `crates/branchyard/src/tooltree.rs` mounts the tree and links native skills (`~/.claude/skills`, `~/.codex/skills`, private homes only); `plugins/branchyard/skills/find-tools/` holds the skill and the `tools` script (BM25, `search`, `show`, `schema`), tested in `tests/test_find_tools.py`; `Grant::has_helpers` picks the delegate skill or `leaf.md`; `operations::Surface` and `BRANCHYARD_MCP_SURFACE` gate `tools/list`; the operator paragraphs moved to [delegation.md](delegation.md#operator-notes). Measured with `cargo test -p branchyard-mcp` and `tools/check_model_surface.py`:

| Case | Before | After | Cap |
|---|---|---|---|
| MCP `tools/list`, leaf (8 tools) | 41,736 B (35 tools) | 5,500 B | 5,800 |
| MCP `tools/list`, leaf with a result contract (17 tools) | 41,736 B | 8,861 B | 9,300 |
| MCP `tools/list`, branch with helpers (35 tools) | 41,736 B | 41,736 B | 43,900 |
| Spawning branch text (delegate skill, instructions, note) | 45,969 chars | 42,875 chars, then 11,682 with the skill split into a core and references | 12,500 |
| Leaf branch text (`leaf.md`, instructions, note) | 14,352 chars | 8,526 chars | 9,000 |
| Work turn, no delegation: note and the tools line | note 0.4 KB, capabilities line 0.2 KB, connectors line 0.6 KB | 661 chars at the line's cap | 1,000 |
| Skills and connectors lines | two lines, 0.8 KB | one line under 300 chars | 300 |

A plain Work turn with no delegation gets no MCP server at all, so its Branchyard surface is under 1 KB of prompt plus the 4 KB index it reads on demand. A leaf is 8,526 chars plus 5,500 B of tools, about 14 KB. A spawning branch is not smaller in tools; its saving is 3.7 KB of instructions. The delegate skill's core (7.5 KB) goes to every branch with helpers; its other 31 KB is in `references/*.md`, read when a table row in the core names one, and staged under `helpers/delegate/references/` in the tree (below).

## 4. One turn, remotely: lifecycle and delivery

**Status: partly built.** Three parts, each hermetic. Hermetic (fakes only: the fake ACP agent, a mock gateway, a fake packager, an in-process server; no model, no real sandbox, no GCP), run on 5 November 2026 at the commit that adds this paragraph. Built:

- **The manifest (step 1, in part).** A session resolves a `TurnManifest` in its first turn and keeps it with the branch; every turn uses it until the person changes the session's tools. It holds the actor, grants, chosen skills (or all), each connector's bundle hash, the helpers envelope, models, network, the policy version and the time. Its blake3 digest is written to `~/.branchyard/manifest.json` (`BRANCHYARD_MANIFEST`), carried in the token as `by_manifest`, in the `provisioned` event and on the console's session view. See [the turn manifest](connectors.md#the-turn-manifest).
- **The policy version (steps 1 and 2, on the server).** Every `PUT` and `DELETE` of any policy record raises the tenant's version in the same transaction. A session records the version at admission. When its queued start or send is claimed and the version has moved, the server checks the session's tools against the policy as it stands and refuses `policy_changed` if they are no longer allowed, else records the new version in the session and its manifest. See [tool policy](server.md#tool-policy).
- **Renewing tokens (step 4, in part).** The token file holds a five-minute token that the engine replaces by rename every two and a half minutes while the turn runs, and the effects proxy calls the gateway with the live token whatever the harness presents (its own, an earlier one, or none), so a turn can outlast the hour. See [token renewal](connectors.md#token-renewal).

**Enforced, not only recorded (steps 2, 3 and 7), built 5 November 2026, hermetic only** (fakes only: the fake ACP agent, a fake packager, `FakeProvider` standing in for Microsandbox, mock HTTP for Cloud Identity and the connect routes, an in-process server; no model, no real sandbox, no GCP; run at the commit that adds this paragraph with `cargo test -p branchyard --lib -- bundles:: packager manifest toolstore`, `cargo test -p branchyard --test connectors` and `cargo test -p branchyard-server --lib`):

- **Bundle digests (step 3).** The manifest now records the BLAKE3 digest of each staged built-in skill (`skill_digests`) and, for a connector, the store index's digest of its package (`content`) beside the packager's bundle hash. Before the harness starts, a turn that would link or copy a skill or connector holds it to those. A host that serves another bundle, embeds another skill, or holds other files in the store entry it would link fails the turn `bundle_mismatch`, with the entry, the digest admitted and the one found, and a warning event with the same text. A store entry is hashed once per process, then only when a stat of its files changes. See [the turn manifest](connectors.md#the-turn-manifest).
- **Workers use the admitted policy (step 2).** Admission records the server ceiling it applied (the `ceilings` connectors and `allow_delegation`) in the session row. Whichever process claims the operation checks the session's tools against the stored policy and that recorded ceiling, narrowed by its own configuration and never widened by it. A worker whose configuration is narrower than the record refuses what it cannot hold `policy_changed` with `detail.readmit`, and `POST /v1/workspaces/{workspace}/sessions/{session}/readmit` runs the admission check again from the stored tools and records the answering process's ceiling and the current version. [Tool policy](server.md#tool-policy) lists what still comes from the worker.
- **Sleep and resume (step 7).** A turn that wakes the branch's kept sandbox (`keep = "pause"`) resolves its manifest again, so a tool deselected or withdrawn meanwhile is gone, the bundles are those the host serves now, and the connector packages and tool tree are staged for the turn's tools only. The renewer stops and the token file goes before a sandbox is paused (a copied home also has it removed in the guest), and the resumed turn mints another.
- **Fresh groups.** A queued operation of a person whose groups came from Cloud Identity reads them again once they are older than the lookup's five-minute cache, and picks the binding with the fresh groups, so leaving a group takes effect without a new sign-in; a failed lookup refuses `identity_unavailable`. See [identity](identity.md#fresh-groups-on-a-recheck).

Not done: re-admission records one process's ceiling; it does not make every worker agree, and a worker that lacks a harness, provider or tool store the session needs still fails by name. A package copied from the cache is held to its bundle hash, not read again. The sleep and resume checks ran against `FakeProvider`, not Microsandbox or a Substrate actor, and the guest-side token removal for a copied home is not exercised by a test. A paused sandbox that is resumed by a runtime action and then given no turn is not re-resolved until its next turn. No part of this ran against a real Anvil, gateway, sandbox or cluster.

**Egress (step 5), built 5 November 2026, hermetic only.** One list, in code, says what a restricted turn may reach: the connector gateway, the model gateway, the model provider's hosts (for a harness that calls it directly), and the package mirrors from `[network] mirrors`, and nothing else (`TurnHosts` and `allowlist` in `crates/branchyard/src/egress.rs`). The branch's own policy keeps what it lists; these hosts are added for the turn.

- **Local process and Microsandbox.** The engine builds the list per turn and records it in the `egress` event (`allow`). A local harness on a host that can confine it (a network namespace) is held to it by the egress proxy. A Microsandbox guest gets the runtime's default network and is not held to it (`Capabilities::egress` is false), so the event says `not applied`, and a `required` policy there is refused. Tests: `none` plus the gateway allows the gateway's `host:port` and no other host, port, mirror or model endpoint; the full list is exactly the five rules; a mirror the branch already lists is not added twice; an open policy stays open.
- **GKE Agent Sandbox.** Under `network = "deny"` an allocation gets no policy of its own unless it has something to reach; with something, the provider generates both objects from the turn, owned by the Sandbox, bound to the receipt and checked on every use:
  - a `NetworkPolicy` that allows cluster DNS; the gateway's pods on its port when `gateway_service` names an in-cluster Service; and, with a bucket, the Private Google Access ranges `199.36.153.4/30` (`restricted.googleapis.com`) and `199.36.153.8/30` (`private.googleapis.com`) on 443;
  - a GKE `FQDNNetworkPolicy` (`networking.gke.io/v1alpha1`) listing hosts on 443: `egress_hosts` (the model endpoint), `[network] mirrors`, the gateway's host when it is named by DNS, and, with a bucket, `storage.googleapis.com`.
  - **The Cloud Storage FUSE gap is closed.** The CSI sidecar runs in the pod's network namespace, so under `deny` it had no route to the bucket. With `store_bucket` set, Cloud Storage is now always allowed, `deny` included: by name (`storage.googleapis.com`) and by the two IP ranges for a subnet with Private Google Access. Nothing else is opened. `verify.rs` checks both objects against the receipt (a changed host, port, range or rule, a missing object, or a foreign policy selecting the pod is refused before exec), and removal deletes both by UID.
- Needs on the cluster, not checked: Dataplane V2 with FQDN network policy enabled, and the `FQDNNetworkPolicy` API.

Evidence: hermetic. The fake `kubectl` of `crates/branchyard-agent-sandbox/tests/fake_kubectl.py` stores and checks both objects; the engine's allowlist is tested as a pure function and through the config it reads. Not done: a pod under these policies on a live GKE cluster (Dataplane V2 enforcing the FQDN policy, the FUSE sidecar reading a bucket through the ranges), a Microsandbox guest held to a list, and Substrate egress.

**`/tools` at the guest root, built 5 November 2026, hermetic only.** See section 3. The GKE pod gets the `tools` volume at `/tools`, a container gets a private volume there, Microsandbox gets a read-only mount of the host's tree, and each guest sees `BRANCHYARD_TOOLS=/tools`. Evidence: the manifest and spec are asserted in `crates/branchyard-agent-sandbox/src/tests.rs`, `crates/branchyard-container/src/tests.rs` and `crates/branchyard/src/providers/tests.rs`; `crates/branchyard/tests/native.rs` runs a turn through the fake `kubectl` and `docker` and finds the tree and the variable. Not done: a turn on a live guest reading the tree at `/tools`.

**The credential proxy in the sandbox and the token fetch (step 4), built 5 November 2026, hermetic only:**

- **The route.** `POST /v1/turns/token` with `{manifest, branch, turn}` issues a five-minute gateway token for a *running* turn ([server](server.md#turn-tokens-for-sandboxes)). The caller is a sandbox: a **Google ID token** for the server's audience, of a service account listed in `connectors.sandbox_identities` (GKE Workload Identity, or a GCE VM's service account), verified against Google's keys; and, in every case, the turn's **per-turn secret**, whose hash the engine recorded with the turn and manifest digest. The manifest digest, branch and turn number are not secret and a Workload Identity service account is shared by every sandbox, so an identity alone would let one sandbox fetch another's turn token; the secret is required, and the identity is required *in addition* where `sandbox_identities` lists any. A sidecar's secret is written to its settings file in the home copied in for the turn (the harness can read its own turn's, and no other's) and removed when the turn ends. The server refuses a wrong manifest digest (`manifest_mismatch`), a branch that is not running (`turn_not_running`), another turn's number, an account that is not listed (`identity_not_allowed`) or is for another tenant, and a body that carries any field beyond the three; the claims come from the turn's recorded grant with the person's ceiling applied, as the turn's own token's do.
- **The proxy.** `by credential-proxy` (`connectors/sandbox_proxy.rs`) listens on the sandbox's loopback, fetches the token, renews it at half its life, and forwards the harness's calls to the gateway with it. The harness is given `ANVIL_GATEWAY_URL=http://127.0.0.1:18900/mcp` and nothing else: no token file in its home, no token in its environment.
- **Where it runs, chosen per provider** (`ProviderKind::credential_site`, one declaration each, tested for every provider; `connectors/credential.rs` joins it with the operator's `connectors.sandbox_server`): local process, container and recipe machine keep the host's proxy; Microsandbox gets a process the engine starts in the guest and kills with the turn; a GKE Agent Sandbox gets a `credential-proxy` sidecar container in the pod ([the pod](agent-sandbox-gke.md#credential-proxy-sidecar)); Substrate is configuration only (its template would run `by credential-proxy --config ~/.branchyard/credential-proxy.json --wait`). Without `sandbox_server` every provider keeps the host arrangement. See [the credential proxy in the sandbox](connectors.md#the-credential-proxy-in-the-sandbox).

Evidence: hermetic (mock HTTP servers on loopback, a fake sandbox provider running a stand-in `by` as a host process, a fake `kubectl`, a real server with Google's keys served by a mock and ID tokens signed with a test key; no model, no network beyond loopback, no GCP): `connectors/sandbox_proxy/tests.rs`, `connectors/guest/tests.rs`, `connectors/issue.rs`, `providers/tests.rs`, `crates/branchyard/tests/connectors.rs`, `crates/branchyard-agent-sandbox/src/tests.rs`, `crates/branchyard-cli/tests/credential_proxy.rs`, `crates/branchyard-server/tests/turn_token.rs`. Not done: **a live run on GKE or GCE**: Workload Identity ID tokens from a real metadata server, a pod's sidecar and the NetworkPolicy exception on a cluster (and in the gVisor and Kata runtimes), a Microsandbox guest with a real `by` in its image; a Substrate template that runs the sidecar; the effect-ledger proxy and its approvals inside the sandbox (a turn whose sandbox holds the credentials has none and says so); a cap of the issued token at the turn's deadline (a token lives five minutes whatever the turn's deadline); and a real Anvil client behind the proxy (a sandbox-held turn sets `ANVIL_GATEWAY_AUTH=proxy`, Anvil's mode for it, tested only for the variable). A harness in the same pod can still reach the metadata server, the proxy and its own turn's secret in the sidecar's settings file; what it cannot do is read a token from a file, keep one past five minutes, or fetch another turn's token (regression tests: an allow-listed identity without the turn's secret, with a guessed one and with another running turn's secret are refused; `crates/branchyard-server/tests/turn_token.rs`).

**The shared tool store and host install (step 3), built 5 November 2026, hermetic only:**

- **Store.** `[tools] store` in `branchyard.toml` (server: `tools.store`) names a read-only directory that holds `skills/<name>@<digest>/` and `connectors/<id>@<bundle-hash>/`, with a `store.json` index (`id`, `kind`, `digest`, `size`, `path`). A digest is the BLAKE3 hash of the entry's files, the hash `anvil package harness` output is cached under. When the store has an entry for a turn's chosen skill or granted connector, the turn links to it; otherwise it copies, as before. A skill keeps its own directory with a link for each file, so its references to sibling skills (`../artifact-tools/INDEX.md`) still resolve. A connector's package is one link.
- **Which providers link.** A provider links only where the harness sees the store at the same path. A local harness does. Microsandbox gets a read-only volume of the store root. A GKE Agent Sandbox gets a Cloud Storage FUSE CSI volume (`driver: gcsfuse.csi.storage.gke.io`, `readOnly: true`) when `[tools] store_bucket` (server: `tools.store_bucket`) is set, and the pod annotation `gke-gcsfuse/volumes: "true"`; see [Agent Sandbox on GKE](agent-sandbox-gke.md#mount-the-tool-store). Container, Substrate and recipe sandboxes copy the home in, so a link would dangle: they keep the copy.
- **Publishing.** `by tools publish` writes the entries for the built-in skills and for every bundle under `[connectors] bundles` (packaged by `anvil package harness`, cached by bundle hash), then `store.json`. `--gcs gs://BUCKET/PREFIX` mirrors the store with `gcloud storage rsync`. `--oci REGISTRY/REPOSITORY` pushes each entry as an OCI artifact with the `oras` CLI, tagged by its digest (`REGISTRY/REPOSITORY/skills/NAME:DIGEST`, `.../connectors/ID:BUNDLE-HASH`), and the index as `REGISTRY/REPOSITORY/index:latest`. Branchyard links no registry client; it runs `oras` and `gcloud` from `PATH`.
- **Pulling.** `by tools pull --from gs://BUCKET/PREFIX` or `--oci REGISTRY/REPOSITORY` fills a local store, fetches only the entries it lacks, and checks every entry against its digest in the index. A mismatch fails and names the path.
- **Host setup.** `by compute gce --tools-store gs://BUCKET/PREFIX` and `BRANCHYARD_TOOLS_FROM=gs://... sh tools/session_setup.sh --tools-only` pull the store to `/opt/branchyard/store` once. Both also run `anvil install --prefix /opt/anvil` once when `anvil` is on the host and has an `install` command, and say they skipped it when it is not ([Anvil contract v2](anvil-contract-v2.md), item 4). Their tests run the guarded install against the [Anvil mock](anvil-contract-v2.md#the-mock), which lays out a real version directory. [Deploying `by serve`](deploy.md#tool-store-in-an-image) shows the same two steps for an image.

Evidence: hermetic. Fake `gcloud`, `oras` and `kubectl` scripts and the Anvil mock record their arguments and move files between directories that stand for a bucket and a registry (`crates/branchyard/src/toolstore/tests.rs`, `crates/branchyard-cli/tests/tools_store.rs`, `crates/branchyard-agent-sandbox/src/tests.rs`, `tests/test_compute_gce.py`, `tests/test_session_setup.py`). Not done: any run of `oras` against Artifact Registry or `gcloud` against a bucket; the CSI volume on a live GKE cluster, in the gVisor and Kata runtimes, with a turn reading through it; the Substrate actor-image layer; an `anvil install` from a real Anvil on a host (`anvil install` exists since Anvil's PR #91 and was run on 5 November 2026 against the built CLI by `tests/test_anvil_real.py`, which is skipped where no built Anvil exists; the host-setup tests still run the guarded call against the mock, whose `ANVIL_MOCK_ROOT` is a mock-only knob; the installed launcher runs the installing Anvil's CLI, so the toolchain is not self-contained). Known limits: `gcloud storage rsync` does not keep file modes, and a bucket holds none, so a connector's `bin/` launchers are not executable in the bucket itself. The GKE mount does not depend on them: `crates/branchyard-agent-sandbox/src/config.rs` already sets `file-mode=755,dir-mode=755` in the mount options, so launchers stay runnable through the read-only mount (set in the manifest and asserted in a test; no pod mounted a bucket on a live cluster). The `deny` network no longer cuts the CSI sidecar off from Cloud Storage: with a bucket, Cloud Storage is always allowed (step 5).

Admission resolves everything once, and the turn carries the result. A worker runs exactly what was admitted.

1. **Admission (server).** It computes the effective tools (section 1). It resolves each to a content digest, writes a **turn manifest** and stores it with the operation:
   - `{actor, tenant, grants, skills: [digest], connectors: [digest], helpers, model access, network, deadline, policy version}`.
2. **Claim (worker).** The worker checks the manifest's policy version against the server's. If the version is newer, it asks the server to re-admit the turn rather than run stale policy.
3. **Tools in place.** Each digest is an immutable bundle in a content-addressed store:
   - Skills and connector packages are published once to **Artifact Registry** as OCI artifacts, mirrored to a GCS bucket.
   - The turn mounts the store read-only. Its own files are only `INDEX.md` and a tree of links, a few kilobytes.

   | Runs on | Store mounted by | Gateway reached at | Token held by |
   |---|---|---|---|
   | GCE, local process | Host cache directory, filled from GCS | `gateway` | A sidecar on the host (step 4) |
   | GCE with Microsandbox | virtiofs share of the host cache, read-only | `sandbox_gateway` (host address) | The host sidecar, through the address the guest reaches |
   | GKE Agent Sandbox | **Cloud Storage FUSE CSI** volume with the file cache, or an image volume of the toolset | Gateway's internal Service; NetworkPolicy allows only it | A sidecar container in the pod |
   | Substrate | Actor image layer or copy-in of the chosen digests only | Routed ingress URL | A sidecar in the actor |
4. **Credentials (sidecar).** The harness never holds a gateway token.
   - SDK and CLI calls go to `127.0.0.1:<port>` in the sandbox: a small proxy (today's effects proxy, moved into the sandbox).
   - The proxy attaches a five-minute token and renews it before expiry. A turn longer than an hour keeps working, and a leaked token dies in minutes.
   - The proxy fetches tokens from the server as the sandbox's identity:
     - on GKE, a Kubernetes service account through **Workload Identity**;
     - on GCE, the VM's service account.
   - Each fetch is bound to the turn by its manifest id.
   - This also fixes the unreachable delegation and effects endpoints, because the proxy is inside every sandbox.
5. **Egress.** A sandbox may reach the gateway, the model endpoint and the package mirrors, and, when the tool store is a bucket, Cloud Storage, and nothing else:
   - GKE: a `NetworkPolicy` and an FQDN network policy generated from the turn (below);
   - GCE and Microsandbox: the egress allowlist, one explicit list in code (below);
   - Substrate: its egress rules (not built here).
6. **End.** Results are copied or applied back as today. The token stops renewing, the links go, and the store stays cached for the next turn on that host or node.
7. **Sleep.** A paused Microsandbox or Substrate sandbox keeps its mounts and loses its token. On resume it gets a fresh manifest, so a tool deselected meanwhile is gone. *Built for the turn that wakes it; see the paragraph above.*

**Configuration carries over** because it is data, not files:

- The catalog, policy, defaults and bundles live in the server's store and the content store.
- Secrets are Secret Manager names, read by each worker as its own service account.
- A worker's JSON shrinks to: where the server is, who it is, and its labels.

## 5. The console

- **One Tools page.** It is under Settings (`#/settings/tools`), not a sidebar item, with one table for connectors, skills and helpers. Each row has the tool's icon, its state in words and its one action. Administrators see the Allowed column and edit it. The page reads `GET /v1/tools`.
- **One control in the composer.** A **Tools** button sits beside the message box, showing the chosen tools' icons, stacked. It opens a menu with three groups and a search field, matching what the harness itself searches. The separate step in the session form goes away.
- **Icons.** `assets/connector-icons.svg` is a sprite of 57 marks from Simple Icons (CC0), drawn in `currentColor` so they take the page's ink, and regenerated by `tools/connector_icons.py`. Skills and helpers get line icons from the console's own set.

**Status, 5 November 2026.** The first two bullets are built in the console ([the Tools page and the composer's Tools control](companion.md#tools-page)): one table under **Settings > Tools** with an Allowed column and a policy editor for an administrator, and one **Tools** button with a menu (popover wide, modal sheet at 759 pixels or less) that sends a start's choice as `tools` and saves a workspace's defaults on the server. Three things the first version left out are built, all hermetic:

- **The helper limit travels in `tools`.** The helper entry `{kind: "helper", id: "*", max_children: N}` (1 to 16) sets how many helpers a session may have at once; the composer has a number for it in the Helpers group, and the default is 4. An administrator's `max_helpers` (a new optional part of a policy, narrowing to the lesser) caps it: a larger number is `tool_not_allowed` with `detail.max_children`, the catalogue reports the caller's top as `max_children`, and a session admitted above a limit lowered later fails its claim-time recheck `policy_changed`.
- **A person's own policy.** `GET` and `PUT /v1/me/policy` store `self:<tenant>:<subject>`, the same shape as any policy plus `off` (tools switched off). It is intersected after the administrator's result in `resolve`, so it only takes away; it raises the tenant's policy version like the other records, and the administrator's list does not show it. The Tools page has a **Mine** column to switch a tool off for oneself and shows why a tool is unavailable (the person's own switch, or the administrator's, in different words and with different actions).
- **Install for a helper's harness.** `POST /v1/tools/helpers/{id}/install` (an administrator, once the operator sets `harness_install.allow`) runs the harness catalog's install command on the server's host and reports `queued`, `running`, `succeeded` or `failed` on `GET`; the row moves from `not_ready` to `ready`. It is per host: workers elsewhere keep what they have, and the response says `host: "server"`. A person cannot install for themselves, because a harness belongs to the host and not to the person. The status is kept in the server process (not in the operation queue, which is bound to repositories and branch locks), so a restart forgets it, and each attempt is written to the harness log `by harnesses` keeps.

Evidence: hermetic. A real server, a mock gateway, the fake agent and a fake `npm` on an install `PATH` of the test's own, in headless Chromium at 1440 and 390 pixels (`tests/console_connections_browser.rs`), and in-process route tests (`console/tests/people.rs`: the limit and its cap, the order tenant, groups, person, then self, the version, `off` by name, the ceiling, install success and failure, and install closed by default). Not done: a real gateway, a screen reader, a real installer (`npm`, `curl`) or a real harness binary, installing on a worker's machine or in a sandbox image (a recipe's, Microsandbox's or Substrate's harness comes from its image), a durable install queue across a server restart, and the other kinds of readiness (**Sign in** per person).

## Decisions (4 November 2026)

- **Policy store:** PostgreSQL, running on **AlloyDB**. Two ways to connect: through the AlloyDB Auth Proxy (a sidecar on GKE, with `--auto-iam-authn`), or directly over TLS (`sslmode=require` with the instance's server CA), with an IAM token from the metadata server as the password at each connection. SQLite stays for one machine.
- **Sign-in:** IAP in front of the console, plus any standard identity federated through GCP: Workforce or Workload Identity Federation tokens, and plain OIDC from a configured issuer. Groups come from a token's claim, or from Cloud Identity when the token carries none.
- **Shared store:** tool bundles are OCI artifacts in **Artifact Registry**, mirrored to a GCS bucket and mounted read-only with **Cloud Storage FUSE**.
- **Manifest:** every session or conversation starts by resolving its turn manifest (section 4). Every turn of it carries that manifest, renewed only when the person changes the session's tools.
- **Host setup:** connector bundles, Anvil's CLI and SDKs, and the compose SDK are installed on each host when it is set up (`by setup`, `by compute gce`, the worker image), not packaged per turn.
- **Anvil:** it has built the contract in [Anvil contract v2](anvil-contract-v2.md) (PR #91, merged 5 November 2026; hermetic there, not run live: no real Firestore, KMS, Cloud Run or registry, no `anvil vault migrate`, no `--from` OCI, no self-contained toolchain). Branchyard builds against it, with a mock for tests that run where Anvil is not.

## 6. Order of work

| Phase | What | Where |
|---|---|---|
| 1 | The model and its API (`/v1/tools`, server-side defaults, one start field); policy in the store; required issuer; the console's Tools page and composer control | Branchyard |
| 2 | The mounted tool tree, `tools search`, the `find-tools` skill, native skill links, Branchyard's MCP surface following the choice, budgets per case (built; see section 3) | Branchyard |
| 3 | The in-sandbox proxy with renewing tokens; turn manifests; workers re-admitting on a stale policy. **Built:** manifests with enforced bundle digests, the policy version and its check at claim against the recorded server ceiling, re-admission, renewing tokens on the host's proxy, sleep and resume, the proxy in the sandbox with the server's token fetch (hermetic). **Not built:** the effect ledger in the sandbox | Branchyard |
| 4 | GCP: KMS signing, IAP and groups, Artifact Registry and GCS store, GCS FUSE on GKE, FQDN egress, Workload Identity token fetch. **Built, hermetic:** FQDN egress, the Workload Identity token fetch and the GKE sidecar. **Not run live** | Branchyard, deployment |
| 5 | Anvil: Firestore and KMS vault, disconnect route, operation search index in the package, a fleet-gateway image for Cloud Run. **Built in Anvil (PR #91, merged 5 November 2026), hermetic; the image is generated files, not built; not run live** | Anvil |

## Not done

Everything on this page is a proposal, except section 3, the tool store and host install, egress, `/tools` and the credential proxy in the sandbox in section 4, which are built and tested hermetically. Nothing else here is built or measured beyond the sizes the audits read.
