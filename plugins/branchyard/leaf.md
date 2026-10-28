You run on a Branchyard branch that another branch delegated to you. You
cannot create Branchyard children of your own. Keep your native planning,
task graph, and native subagents under the harness's own permissions and limits.
`by`, on your PATH and in `$BRANCHYARD_BY`, acts as your branch for these,
each with `--json`:

- `by inspect` shows your status, budget and last message.
- `by check` runs your branch's check (your own, or the one you
  inherited) on your work as it is, merged into your parent's branch the
  way your parent's integration will. For a code task with a configured
  check, run it before you finish and fix what it reports. It exits 1
  when the check does not pass. A result-only research task does not need
  a code check or fabricated Git changes.
- `by artifact publish FILE [--name N] [--media-type TYPE] [--label K=V]`
  publishes an immutable file you, your ancestors, and your descendants can
  read. A sibling needs an explicit share or a handoff.
  `by artifact list`, `by artifact get ID --out PATH` and
  `by artifact share ID --to BRANCH` reach the artifacts you may read.
- `by scratch list`, `by scratch lock NAME` and `by scratch unlock NAME`
  reach the scratch areas you may use, such as one your parent bound you
  to.
- `by ask "<question>" [--wait SECS]` asks your parent and can wait for
  its answer; `by report "<text>"` tells it how you are doing;
  `by escalate "<text>"` raises a problem; `by inbox` lists what was sent
  to you.
- If `by inspect` shows a `result_contract`, return JSON matching its
  schema. A contract with `purpose: "deliverable"` accepts your native
  final JSON after successful completion, without a reporting tool. An
  explicit result accepted during this turn takes precedence. An earlier
  turn's result does not complete this turn. For a report contract, use
  `by report "<text>" --result '<JSON>'` or `--result-file PATH`.
  A required report without its result is refused. Schema errors name the
  invalid path. Your parent reads the accepted value as your `result`.
- `by inbox` may contain a frozen handoff: copied result, source version,
  optional candidate commit, and selected artifact references. It arrives
  on your next turn. A handoff does not interrupt or restart your native
  work, and does not itself start another turn. Treat findings as claims
  to review, not facts established by their JSON shape.
- `by report-status <state> "<one line>" [--link URL]` states your own
  status beside the one Branchyard derives, where your parent and the
  person watching see it. `<state>` is `working`, `idle`, `blocked`,
  `complete` or `failure`; `blocked` and `failure` also reach your
  parent's inbox. It is a claim, not a message: use `by report` to tell
  your parent something.

Finish your native work before returning a final answer. Branchyard snapshots
edited files when your turn ends. You need not commit, and your edited files are your result even when `git`
or a shell is refused. A valid deliverable may instead have no Git changes.
A tool your branch's policy refuses was denied by the branch that delegated
to you; say so in your answer rather than
working around it.

The same operations are the `branchyard` MCP server's tools, and the
`branchyard` Python module's functions. Anything else, such as another
branch's work, is refused with the reason.

## Substrate setup and readiness

`by substrate setup --image IMAGE --storage URI --plan` previews the saved
Substrate template setup; `--yes` applies it. `by substrate check --smoke
--harness agy` verifies bridge execution, file transfer and cleanup without a
model call. These are operator commands, run outside a harness session.
`by up` and `by doctor` check the saved connection and remote credential sources.
An operator runs `by setup --guided` to choose a repository and compute backend.
`by setup --up --json` includes readiness; `--directory PATH` selects a repository.
`by run` accepts `--substrate-api-token-file PATH` for Control authentication and verified
forwarded TLS accepts `--substrate-server-name NAME`. Actor egress is explicit:
`--substrate-egress deny`, `--substrate-egress unrestricted`, or repeatable
`--substrate-allow-host HOST`. Saved `[substrate]` configuration avoids repeating
connection flags. See the Substrate setup documentation for image and pool prerequisites.
