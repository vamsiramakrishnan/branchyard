# Real-harness delegation qualification

Fake agents can't find the problems that appear only when a real model drives Branchyard. This
directory runs real Claude Code as a meta-harness that delegates to real Claude Code children. Each
run is a scenario aimed at one mechanism, and each one records the evidence. Every run costs model
usage: a repository scenario costs about $0.3–1.5 and a knowledge-work campaign about $1–3. Nothing
here runs in CI.

Each meta prompt ends by asking the harness to list every piece of "Branchyard friction" it hit
(error, refusal, confusing message, missing verb, workaround), with exact commands and messages.
Treat that list as a lead to verify against the event logs, not as evidence.

## Repository scenarios

`scenarios.py` creates 12 small repositories under `$QUALIFY_OUT/repos/`. The default is
`target/qualify-delegation`. Each one has stubbed modules, tests, and a meta prompt:

| Scenario | Mechanism under test |
|---|---|
| `calc4` | Four children on one shared test suite: integration of partial work |
| `conflict3` | Three children editing the same files: conflicts on integration |
| `graph` | Dependencies with `--depends-on … --after integrated`, started by the graph |
| `depth2` | Two levels of delegation, with budgets narrowed per level |
| `recovery` | An over-budget child, cancelling and respawning it, and `by ask`/`by answer` |
| `compete` | Three competing candidates: comparing them, integrating one, discarding the rest |
| `steer` | `by send --steer` into a running turn |
| `artifacts` | Sharing an artifact between siblings without a merge |
| `pysdk` | Orchestrating entirely from the Python module |
| `mcponly` | Delegating through the MCP tools with the shell denied |
| `envelope` | Refusals from the envelope (budget, harness) and adapting to them |
| `crash` | Killing the engine mid-run, then recovering with `by send` |

```sh
cargo build -p branchyard-cli
python3 tools/qualify_delegation/scenarios.py            # all, or name some
tools/qualify_delegation/run-repo.sh calc4               # one; run several at once with xargs -P 4
```

For each scenario, `$QUALIFY_OUT/out/<name>/` holds:
- `run.log`: the whole run, including the meta's final report and its friction list;
- `ls.txt`, plus `inspect.*.json`, `log.*.txt` and `events.*.jsonl` (every event the branch recorded,
  one JSON object per line, from `events.py`) for each branch;
- `git.txt`;
- `verify.txt`: the scenario's tests, run on `by/meta` checked out separately.

### Scoring a run

`run-battery.sh` runs every scenario (or the ones named), `QUALIFY_JOBS` at a time (default 3), and
scores the run with `score.py`. Give it a fresh `QUALIFY_OUT` and the `by` under test as `BY`;
`QUALIFY_MODEL` puts every scenario's meta on one model. Run inside a Branchyard turn it works too:
`run-repo.sh` clears the inherited `BRANCHYARD_*` variables, so each scenario is a yard of its own.

```sh
QUALIFY_OUT=/tmp/q1 BY=target/debug/by tools/qualify_delegation/run-battery.sh
python3 tools/qualify_delegation/score.py /tmp/q2 --compare /tmp/q1/scores.json
```

`score.py` reports, per scenario and in total:
- whether the check passed on the meta branch;
- the tree's cost and the run's wall time;
- the branches and the meta's turns;
- protocol violations across every branch;
- the Branchyard calls refused across every branch: the refused `delegation` events, each counted
  once however often the meta quotes it. Runs from before `events.*.jsonl` was saved count
  `refused:` in `run.log` and `resume.log` instead, and `--compare` says when two runs were
  counted differently;
- the items in the meta's friction list.

These are the fixed evaluator for a delegation change. A change holds the pass count and lowers
violations, refusals or friction without raising cost by more than 10%. Models vary from run to
run, so compare totals across all twelve scenarios, not single ones.

## Knowledge-work campaigns (Worldloom)

These campaigns test delegation on work that isn't code. The tasks are multi-step dependency chains
over a company's connectors (ServiceNow, Jira, Salesforce, SharePoint, Drive, Confluence) that
search, read, create, update, patch and delete records and files. Each chain has a failure built in
(a permission denial, a version conflict or a partial write). [Worldloom](https://github.com/vamsiramakrishnan/synthetic-foundry)
builds the company and the cases. It serves them over MCP, keeping one shared state per run across
every connector, and grades each run from the company's facts: the plan, the trajectory, the effect
on state, and the answer.

```sh
pip install -e '/path/to/worldloom[all]'
worldloom build --seed 8128 --incident --out world
worldloom enterprise-evals build world cases --exhaustive --limit 40 --dag-shape '*' \
  --profile examples/enterprise-evals/financial-services.json --drop-unsolvable
worldloom evalrun run cases -o runs/reference            # the ceiling
worldloom enterprise-evals serve cases --port 8771 --max-runs 30 --run-store runs.8771.jsonl &

export WL_URL=http://127.0.0.1:8771/mcp WORLDLOOM_PY=$(which python) WORLDLOOM_CASES=$PWD/cases
tools/qualify_delegation/worldloom/run-campaign.sh campaign-tools tools 7f250824 0041fd0a e3f6d310 56c97279
```

| Mode | What the harness does |
|---|---|
| `solo` | One session does every task with MCP tool calls |
| `tools` | The meta delegates one child per task; the children call MCP tools |
| `code` | The meta delegates; the children write Python programs against `wlsdk.py` (code mode) |

The Worldloom server allows four open runs per principal, so give each concurrent campaign its own
server (port) and at most four cases.

The pieces:
- **`bridge.py`** turns the server's HTTP MCP endpoint into the stdio server that `--mcp` takes.
- **`kw.py`** is the evaluator. It opens runs, scores answers and ends runs; the agents never get
  these calls, because the campaign's `branchyard.toml` blocks the `eval_*` tools.
- **`summarize.py`** prints the mean score, calls, cost and wall time, and flags answers that never
  reached `by/meta`.

## Results that shaped Branchyard

The first full run, against main at f5f379e, found ten mechanism-level problems. They included:
- a delegating turn that ends while its children run loses their work;
- integrating one child at a time can't pass a shared suite;
- status isn't reconciled with git;
- the surfaces have drifted apart;
- depth-0 children have no storage or messaging;
- budget reservations are never released;
- `/tmp` is shared between branches.

The knowledge-work runs scored 0.85 (solo), 0.79 (delegated, tools) and 0.75 (delegated, code),
against Worldloom's reference ceiling of 1.00. See `docs/code-mode.md` for what that means for the
SDK design.
