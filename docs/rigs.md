# Rigs

A rig declares a team of harnesses in a file: a root seat and the seats below it, each with its harness, model, limits, check, policy and standing instructions. `by rig run` lowers it onto one root branch whose harness fills the other seats at runtime with `by spawn --seat`. The file declares what may exist; the root's harness decides what to spawn, when, and how often within each seat's limit. Budgets, envelopes, permissions and validated merges are [delegation](delegation.md)'s, unchanged.

The idea is [OpenRig](https://github.com/mvschwarz/openrig)'s RigSpec; the format, planner and seats are Branchyard's own, and no OpenRig code is used ([comparison](comparison.md#openrig)). Everything here is tested against the fake ACP agent only; no real harness has run a rig.

## Example

[`examples/rigs/feature.toml`](../examples/rigs/feature.toml):

```toml
version = 1
name = "feature"
root = "lead"
description = "A lead delegates the implementation and a review, then integrates"

[startup]
files = ["guidance/team.md"]          # to every seat, as instructions

[seats.lead]
description = "Plan the change, delegate it, have it reviewed, and integrate what passes."
harness = "claude-code"
model = "large"
budget = { max_usd = 6, max_turns = 20, max_minutes = 60 }
check = "cargo test"
delegates_to = ["implementer", "reviewer"]

[seats.lead.policy]
default = "allow"
deny = ["WebFetch", "WebSearch"]
delegation_commands = true            # the lead's own `by spawn`, `by integrate`, ...

[seats.implementer]
description = "Make one well-scoped change with tests, and leave it committed on your branch."
harness = "codex"
effort = "high"
budget = { max_usd = 2, max_turns = 10 }
instances = 2                         # two may run side by side

[seats.reviewer]
description = "Review a candidate your lead names; report problems, do not edit files."
harness = "claude-code"
model = "medium"
budget = { max_usd = 1, max_turns = 4 }
policy = { deny = ["Edit", "Write", "MultiEdit", "NotebookEdit"] }
startup = { files = ["guidance/reviewer.md"] }
```

```sh
by rig check examples/rigs/feature.toml            # validate and print the plan; --json for the plan itself
by rig run examples/rigs/feature.toml "Add a --json flag to the export command"
# inside the lead's harness:
#   by spawn --seat implementer "Add the flag and its tests"      -> branch feature-implementer
#   by spawn --seat reviewer "Review by/feature-implementer" --wait
#   by integrate feature-implementer
```

[`examples/rigs/parser.toml`](../examples/rigs/parser.toml) has three levels and a pod. [`tests/golden/`](../crates/branchyard-cli/tests/golden) holds both examples' lowered plans; a test keeps them current.

## The spec

TOML, read strictly: an unknown field, a field Branchyard cannot honor, and a value of the wrong type or range are errors that name the field and its line, such as `line 12: seats.reviewer.budget.max_usd: must be a positive number, not -1`.

| Field | Meaning |
|---|---|
| `version` | Required; `1` |
| `name` | Required. The root branch's name (`by rig run --name` overrides it) and the stem of its children's names |
| `root` | Required. The root seat's name |
| `description` | Shown by `rig check` |
| `startup.files` | Files for every seat's instructions |
| `pods.NAME` | A group of seats: `description`, `startup.files`. No runtime record |
| `seats.NAME` | A seat. At least the root |

Names of the rig, pods and seats are lowercase letters, digits and inner hyphens, at most 40 characters.

A seat:

| Field | Meaning | Lowered to |
|---|---|---|
| `description` | Its role, in its own and its parent's instructions | Instructions |
| `harness` | Harness or profile ID, checked against the registry. Default: the parent seat's; the root's is `claude-code` | The root's `harness`; a child's `Seat::harness` |
| `model`, `effort`, `auth`, `telemetry` | As `--model`, `--effort` (`low` … `xhigh` or 0-100), `--auth`, `--telemetry` | `Provisioning` |
| `secrets` | Names only, such as `["OPENAI_API_KEY"]`. Locally each is read from the variable of that name; on a server, from the operator's table | `Provisioning::secrets` |
| `mcp` | `{ NAME = "/absolute/command args" }`, as `--mcp NAME=COMMAND` | `Provisioning::mcp_servers` |
| `isolated` | A private home; a child of an isolated seat is isolated too. Secrets need it on the seat or above | `isolated` |
| `budget` | `{max_usd, max_turns, max_minutes}`; `max_minutes` is per turn | The root's `budget`; a child's limits |
| `check` | `"cargo test"` (split like `--check`) or `["cargo", "test"]`. A child without one keeps its parent's | `check` |
| `policy` | The root's: `default` (`allow`, `deny` or `ask`; default `deny`), `deny`, `allow`, `delegation_commands`. A child's: `deny` only | The root's `Policy`; a child's denials |
| `delegates_to` | The seats below this one | The seats a branch in this seat may spawn |
| `escalates_to` | Ancestor seats, besides this seat's own parent (always allowed), a branch here may `escalate` to (see [delegation](delegation.md#inbox)) | `Seat::escalates_to`; carried onto the branch's own `Seats` |
| `instances` | Children of this seat one parent may have, counting finished ones until they are removed. Default 1 | `Seat::instances` and the parent's `max_children` |
| `pod` | A pod whose startup files it gets | Instructions |
| `startup.files` | Its own files | Instructions |
| `start` | `on_demand` only | — |
| `restore_policy` | `resume_if_possible` only | — |

A startup file is a path relative to the spec's directory, without `..`, or `{ path = "...", required = false }`: a missing required file is an error, a missing optional one is skipped and listed by `rig check`.

## Lowering

`rig::plan` (in [`branchyard-cli`](../crates/branchyard-cli/src/rig.rs)) is pure: it reads nothing but the parsed spec and the harness registry, and produces the root's task options and [`Seats`](../crates/branchyard/src/seats.rs).

- The root seat becomes the root branch: its harness, budget, check, isolation and provisioning, and a policy of its `deny` rules, then its `allow` rules, then the delegation command rule when `delegation_commands` is set, then its default.
- The seats below the root become `TaskOptions::seats`: the rig's name, the root's seat, the seats it delegates to, and a table of every seat below it.
- The root's envelope is derived from the tree: `max_depth` is the longest chain of seats below the root, `max_children` the sum of its seats' `instances`, and `harnesses` every harness in the tree.
- Every seat's instructions are generated, then the rig's, its pod's and its own startup files follow, each under `## From <path>`. The generated part names the seat, its role, and the seats it may spawn with their harness, instances, cost and turns, and how to spawn them (`by spawn --seat`, Python `seat=`, the MCP tool's `seat`). They reach the harness through [provisioning](provisioning.md): Claude Code's appended system prompt, Codex's `developerInstructions`, ACP's first-prompt preamble. Nothing is written to a worktree.

Planning checks, before anything starts:

- The root is a seat; every `delegates_to` names a seat other than the root and itself; no seat is below two parents; every seat is reachable from the root, so there is no cycle.
- Every harness is in the registry. A seat whose profile cannot route permission requests (Antigravity, Pi, Amp) is listed, and `rig run` refuses it without `--allow-unapproved-tools`.
- Under a parent with a cost limit, every child seat has one, and the children's limits times their instances fit in it. A child's turns and minutes are at most its parent's.
- A child seat's policy only adds denials: `default`, `allow` and `delegation_commands` are refused on it.
- Secrets have a private home: `isolated` on the seat or above it.
- `escalates_to` names a seat, and one that is an ancestor of the seat that declares it, beyond its own parent (always allowed, so redundant there); the root seat, having no ancestor, may not declare it at all.

The engine checks the seats again when the root is created (`Seats::validate`: a tree, known harnesses, positive limits, `escalates_to` an ancestor seat; each seat's provisioning), so the SDK and the server refuse a malformed table the planner would have.

## Spawning by seat

A branch in a rig spawns only by seat, and only the seats its own seat delegates to: `by spawn --seat NAME`, `branchyard.spawn(prompt, seat=NAME)`, `Spawn { seat: Some(NAME), .. }`, or the MCP `spawn` tool's `seat`. Outside a harness, `by spawn --parent BRANCH --seat NAME` does the same with your authority, locally and with `--remote`.

- The seat fixes the child's harness, check, delegation harnesses, isolation and provisioning; asking for another harness or check is refused. The request may give a smaller budget, a shallower or narrower envelope and more denials, never more.
- The child's name defaults to `<parent>-<seat>`, then `-2`, `-3`.
- A parent may have at most `instances` children in a seat, counting finished ones until they are removed.
- The child's envelope comes from its seat's subtree and is still narrowed by its parent's, so the parent's depth, width, harnesses and remaining budget bound it as they bound any child. A leaf seat gets no delegation tools.
- The child is given the seats below its own; its own spawns follow the same rules one level down.
- A spawn without a seat in a rig, a seat outside a rig, and a seat not below the caller's are refused (`denied`).

`inspect` shows a branch's `seat` and the `seats` it may spawn; `Spawned` has the child's `seat`. Outside a rig neither field appears, so existing JSON is unchanged.

## Remote mode

`by --remote URL rig run FILE PROMPT` sends one task request: the lowered root options, and the seats as `TaskRequest::seats`. The server needs `--allow-delegation`, since the root delegates (`403 delegation_not_allowed` otherwise), and checks the seats before accepting the task: seats without `delegation`, or a table that is not a tree below the root, are `400`. Each seat's provisioning is held to the rules of `provision`: secrets are names the operator defined (`403 secret_not_allowed` otherwise) and MCP servers need `--allow-client-commands`. `by rig run` reads the spec and startup files on the caller's machine and sends their text. `policy.default = "ask"` is refused remotely. `--command` is the root's executable and needs `--allow-client-commands`; children of the same profile use it, others use their profile's executable on the server's `PATH`, not the operator's `harness_commands`.

## Output

`by rig check --json` prints the plan: `{rig, description, root: {seat, name, harness, profile, budget, policy, check, isolated, provision, delegation}, seats, unapproved_tools, skipped_files}`. `by rig run --json` prints `{rig, root: BranchInfo, descendants: [BranchInfo]}` once every seat it spawned has finished, the same locally and from a server; a refused spec prints `{"error": {"kind": "invalid_rig", "message", "field", "line"}}` and exits 1.

## What is refused, and why

| Field | Why |
|---|---|
| `collaborates_with` | Branchyard has no messaging between siblings; a branch may message only its own parent (and, with `escalates_to`, an ancestor further up) and its own descendants |
| `can_observe`, `observes` | A branch reads only its own descendants; there is no read grant for peers yet |
| `spawned_by`, top-level `edges` | Delegation is declared on the parent, as `delegates_to` |
| `start = "eager"` | The root starts alone and fills seats with `by spawn --seat` |
| `restore_policy = "relaunch_fresh"`, `"checkpoint_only"` | A send resumes the branch's own session or fails; Branchyard never substitutes a fresh one |
| `continuity_policy` | No conversation is rebuilt from briefs |
| `permission_policy` | Presets are not implemented; the root's `policy` is explicit rules |
| `policy.default = "yolo"` or anything but `allow`, `deny`, `ask` | Branchyard answers every request and never bypasses permissions |
| `startup.actions`, `delivery_hint` | Every startup file is standing instructions; the prompt you give is the first message, and drivers refuse slash commands |
| `culture_file`, `services`, `managed_blocks` | Not Branchyard's; put shared guidance in `startup.files` |
| `cwd`, `command`, `runtime`, `agent_ref`, `profile`, `provider`, `prompt` on a seat | Every branch has its own worktree; a seat names a harness; a rig runs where its root runs; prompts come from `rig run` and from spawns |

## Not guaranteed

- A provisioning request a harness cannot take (a reasoning effort for Claude Code, instructions for Pi or Amp) is refused when that seat's first turn is provisioned, not at plan time: the planner does not run the per-harness provisioners.
- Everything [delegation](delegation.md#not-guaranteed) does not guarantee: cost limits for harnesses that report no cost, isolation in local mode, and sandboxed seats, which get no delegation tools.
- Any real harness running a rig.
