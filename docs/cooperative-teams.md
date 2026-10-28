# Coordinate native harness teams

Use Branchyard to coordinate code workers, researchers, and reviewers while
each harness keeps its own planning, task graph, and native subagents.
Branchyard manages separate workspaces, output contracts, artifact access,
messages, budgets, and checked code integration.

You can keep your current native lead session. Operator commands work without
adopting that session or starting a replacement lead. A [rig](rigs.md) is the
alternative when you want Branchyard to start and manage the lead too.

## Use an existing lead

Run commands from the repository the coworkers should use. These examples
select the `antigravity` harness profile, which runs the installed,
authenticated `agy` executable.
`--allow-unapproved-tools` acknowledges that AGY does not route individual tool
permission requests through Branchyard. Keep the native permissions appropriate
for the task. See [harness permissions](provisioning.md).

The file paths below refer to the Branchyard checkout. In another project,
copy the cooperative recipes with their `guidance` and `contracts` folders,
preserving the layout, or use absolute paths to those files.

Start a researcher with a native final-answer contract:

```sh
by run "Assess the supplied design notes. Return summary and evidence as final JSON." \
  --harness antigravity --allow-unapproved-tools --name research \
  --result-schema-file examples/rigs/contracts/research-result.json \
  --result-required --result-deliverable --json
```

No Git change or Branchyard reporting tool is needed. Branchyard accepts a valid
final JSON answer only after a successful native completion. An explicit result
accepted during the same turn takes precedence. Missing, invalid, or stale
output does not complete a deliverable contract. Branchyard does not invent a
retry or restart the native session.

Use `by compare research --json` in the repository to read the validated
current result and its provenance. `by show research` shows branch status.
Inside a managed lead, `by inspect research --json` and `by wait --json` also
expose child results.
For parallel alternatives, [run a fan](fleet.md#judging) with the same schema
and deliverable flags. Independent candidates solve the same question from the
same base. Review their evidence before selecting one.

For best of N on AGY alone, constrain the fleet in `branchyard.toml`:

```toml
[fleet.default]
candidates = [{ harness = "antigravity" }]
```

Then run independent native sessions and compare their outputs:

```sh
by fan "Compare the supplied designs; return final JSON with summary and evidence." \
  --auto --attempts 3 --name alternatives --allow-unapproved-tools \
  --result-schema-file examples/rigs/contracts/research-result.json \
  --result-deliverable --json
by judge --fan alternatives --deterministic --json
```

The fleet may select its sole harness again for another attempt. Each session
keeps its own internal planning. Deterministic ranking checks eligibility and
uses reported cost and duration; review the evidence before accepting an answer.

`by run --no-files` and `by fan --no-files` support knowledge work outside a
repository. Keep the returned task ID and use `by task show ID --json` to read
its results. `by fan --judge` supports initial selection there. Post-hoc `by judge`
and `by compare` need the owning yard context. Use
`by task open ID --attempt BRANCH --print` to obtain an attempt's private
worktree, then run those commands from that directory. Handoffs between
attempts in that same private yard use that context too.

## Use a managed lead and seats

Validate the checked-in recipes before running them:

```sh
by rig check examples/rigs/cooperative-code.toml --json
by rig check examples/rigs/cooperative-research.toml --json
by rig run examples/rigs/cooperative-research.toml \
  "Compare the supplied designs and explain the evidence." --allow-unapproved-tools
```

The research recipe gives researchers and the synthesizer required deliverable
contracts. Each returns native final JSON. The code recipe gives source and
test workers separate owned paths and a shared `cargo test` check. Adapt those
paths and that check to your project.

A managed lead fills seats with `by spawn --seat researcher` or
`by spawn --seat implementer`. A seat fixes its result contract. A spawn may
narrow its owned paths but cannot widen them or replace the contract with a
different schema. Ancestor ownership continues to bound every child.

Branchyard's depth and child limits count Branchyard branches. They do not
configure or disable native subagents. Each harness chooses its own internal
decomposition. Keep the lead's native work active until it finishes. Ending a
managed turn to await Branchyard children is optional once its own native work
is complete. An external lead has no automatic Branchyard wake.

Cost limits depend on usage the harness reports. Unknown reported dollar cost
is not a hard spending guarantee. See [cost limits](delegation.md#what-a-cost-limit-guarantees).

## Hand a settled result to a coworker

The recipient must already exist in the same yard. First let the producer
finish its current turn. Then freeze its result and choose the artifacts the
recipient needs:

```sh
by handoff research --to synthesis --artifact art1 \
  --note "Use the measured finding; preserve its uncertainty." --json
```

Replace `art1` with an ID returned by `by artifact publish`. Omit `--artifact`
when only the JSON result is needed. The message contains the copied result,
producer incarnation, turn, accepted result event, optional candidate commit,
and selected immutable artifact references. The recipient receives artifact
read grants bound to its current incarnation.

The handoff stays queued until the recipient's next turn. It neither steers
the current turn nor starts a new one. After the recipient's current turn
finishes, resume it explicitly when ready,
for example `by send synthesis "Synthesize the queued findings."`.

To bind a handoff to a version you already reviewed, pass
`--expected-version` the `handoff.version` object returned in a handoff's JSON, with
`incarnation`, `turn`, and `result_event`. A stale version is refused. Repeating
an unchanged handoff with the same sender, recipient, selected artifacts, and
note returns the original message ID. A later producer turn needs a new handoff.

A delegated lead may relay only between descendants for which it is a common
ancestor. The source and lead must both be allowed to read every selected
artifact. An operator may relay between separate coworkers, while the source
must still have artifact access. Removing and reusing a branch name does not
transfer a queued handoff or its artifact grants to the replacement.

## Review results and integrate code

A result schema checks structure, and a handoff records provenance. Neither
establishes factual accuracy. Deterministic judging ranks eligible outputs using
the available structural evidence, cost, and time. Read the cited evidence or
have the native lead review it.

`by judge --require-judge` requires an available model judge that can enforce
the read-only judge policy. An unavailable, failed, or malformed required judge
leaves no pick. Without that requirement, a failed optional judge may fall back
to deterministic scoring. An AGY-only inventory has no default model judge,
because AGY does not route per-tool approvals. A worker's
`--allow-unapproved-tools` never bypasses this judge requirement. Use
deterministic judging or review in the native lead unless you configure a suitable judge explicitly.

Result-only winners are returned as structured results. `--pick` and `--into`
are Git merge operations and refuse a winner without a candidate commit.
For code, keep checks on exact candidates and integrate through Branchyard so
the combined merge tree passes its checks before the target moves.
Remote CLI result capture is supported without a declared check. A remote
CLI deliverable with `--check` is refused before execution because that check
cannot be verified there. For checks and judging on remote compute, use a
local coordinator with a configured compute provider.

Managed automatic landing remains an opt-in coding workflow:

```sh
by spawn --seat implementer "Implement the agreed interface in src." --land
```

Use this inside a managed code lead that authorizes automatic landing. It does
not apply to final-answer research deliverables.

## Verification status

Validate the recipes and run the existing fake-agent workflow tests:

```sh
by rig check examples/rigs/cooperative-code.toml --json
by rig check examples/rigs/cooperative-research.toml --json
python3 -m unittest tests.test_schema_validation.ExampleRigsValidateAgainstTheirSchema
cargo test -p branchyard --locked --offline --test deliverables --test handoffs
```

Status: hermetic tests cover native final-answer contracts, seat constraints,
handoff transactions, next-turn delivery, and code integration. SQLite and local
PostgreSQL exercise the shared transaction tests. The recipes are validated
with `by rig check`. See `crates/branchyard/tests/deliverables.rs`,
`crates/branchyard/tests/handoffs.rs`, and the backend conformance suite.
Not done: live AGY cooperative team runs, live model judging for these recipes,
or cloud provider qualification of these workflows.
