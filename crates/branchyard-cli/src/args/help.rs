//! The help text of `by`: the command groups, each command's examples,
//! and the listings `by --help`, `by help all` and `by help agents` print.

use super::*;

pub(crate) const PLAN_EXAMPLES: &str = "\
Examples:
  by run \"migrate the config loader\" --plan
  by plan show migrate-the-config-loader
  by plan approve migrate-the-config-loader --edit
  by plan reject migrate-the-config-loader --reason \"keep the old flag\" --replan

See docs/plans-and-goals.md.";

pub(crate) const APPROVALS_EXAMPLES: &str = "\
Examples:
  by approvals
  by approvals allow 7Q2M9K4D
  by approvals deny 7Q2M9K4D --reason \"not before the board meets\"
  by approvals ls --all --json

See docs/effects.md.";

pub(crate) const EFFECTS_EXAMPLES: &str = "\
Examples:
  by effects
  by effects --branch board-update --json
  by effects show 5H3XK2PA
  by effects promote 5H3XK2PA
  by effects reconcile

See docs/effects.md.";

pub(crate) const UNDO_EXAMPLES: &str = "\
Examples:
  by undo board-update --to 3 --plan
  by undo board-update --to 3
  by undo board-update --to 3 --only 5H3XK2PA 9TQ0WZ1R
  by undo board-update --yes

See docs/effects.md.";

pub(crate) const KNOWLEDGE_EXAMPLES: &str = "\
Examples:
  by knowledge review
  by knowledge add \"Run cargo fmt before finishing\" --path \"crates/**\"
  by knowledge distill fix-parser
  by knowledge export --out AGENTS.md

See docs/knowledge.md.";

/// The groups of `by --help`'s command list, by `display_order / 100`.
pub const GROUPS: &[(usize, &str)] = &[
    (1, "Work on branches"),
    (2, "Inspect"),
    (3, "Delegate and coordinate (usually from inside a harness)"),
    (4, "Rigs and shared storage"),
    (5, "Servers"),
    (6, "Shell and setup"),
];

pub(crate) const GENERAL_AFTER_HELP: &str = "\
Examples:
  by run \"fix the flaky test\" --check \"cargo test\" --yes
  by fan \"speed up the parser\" --harness claude-code,codex
  by ls && by diff fix-the-flaky-test && by merge fix-the-flaky-test
  by --remote https://ci.example:8421 --token-file ~/.by-token ls

Run 'by help <command>' or 'by <command> --help' for its options.
Local mode: harnesses run as your operating-system user, with no other
isolation. State lives in .branchyard/ at the repository root. Remote mode:
harnesses run as the server's user, with no other isolation.
To isolate: --isolated (a private HOME, not a sandbox) or --provider
microsandbox (a microVM; needs a build with that feature). Where harnesses run
and where state lives, rung by rung: docs/modes.md.";

pub(crate) const RUN_EXAMPLES: &str = "\
Examples:
  by run \"fix the flaky test\" --check \"cargo test -p core\" --yes
  by run \"fix the flaky test\" --auto --kind bugfix     # routed by [fleet]
  by run \"add a --verbose flag\" -n verbose --harness codex --budget-usd 2
  by run \"port the build\" --provider microsandbox --image ghcr.io/me/claude:1 \\
      --pass-env ANTHROPIC_API_KEY
  by run -- \"--explain is a prompt here\"
  by run \"fix the flaky test\" --detach --yes     # in the background; prints the branch

--detach (local mode) starts the turn in its own process group with no
terminal and returns once the turn has started: follow it with by log -f
BRANCH or by watch, wait with by wait BRANCH, stop it with by cancel BRANCH.
Its output goes to a log under .branchyard/detached/. With no terminal, tool
permission requests are denied unless --yes, --permissions or branchyard.toml
allows them; --ask is refused. See docs/surfaces.md.";

pub(crate) const FAN_EXAMPLES: &str = "\
Examples:
  by fan \"speed up the parser\" --harness claude-code,codex,gemini-cli --check \"cargo test\"
  by fan \"fix the parser crash\" --auto --attempts 3 --judge
  by ls

--auto picks the harnesses from the [fleet] table in branchyard.toml (the
default when there is a [fleet] and no --harness); --judge then scores the
attempts and proposes one. See docs/fleet.md.";

pub(crate) const MAP_EXAMPLES: &str = "\
Examples:
  by map \"Find the license and stars of {{item.repo}}\" --items repos.csv \\
      --schema answer.schema.json --out results.csv --concurrency 8 --yes
  git ls-files '*.md' | by map \"Fix spelling in {{item}}\" --rm --yes
  by map \"Summarize issue {{item.number}}\" --from-command \"gh issue list --json number\" \\
      --input-format json --reduce \"Group these by theme\" --yes
  by map resume find-the-license-and-stars-of-item-repo
  by map show find-the-license-and-stars-of-item-repo

Each item runs on its own branch, <map>-<item id>. With --schema, each branch
must end its reply with JSON matching the schema; an invalid answer gets one
follow-up turn. Running the same command again (or by map resume) skips the
items done. See docs/map.md.";

pub(crate) const JUDGE_EXAMPLES: &str = "\
Examples:
  by judge speed-up-the-parser                  # a fan's branches
  by judge a b c --harness claude-code --json
  by judge speed-up-the-parser --pick --discard-others --yes

Runs each attempt's check on its exact candidate, scores it without a model
(check, diff size, cost, time), and, with a judge harness (--harness, or the
[fleet] entry's judge), asks it for a JSON verdict on a read-only scratch
branch. An answer that is not a strict verdict falls back to the
deterministic score. Local mode only. See docs/fleet.md.";

pub(crate) const FLEET_EXAMPLES: &str = "\
Examples:
  by fleet stats
  by fleet stats --kind bugfix --json
  by fleet route \"fix the flaky parser test\" --seed 7

The table lives in branchyard.toml as [fleet.<kind>] and [fleet.default];
see docs/fleet.md.";

pub(crate) const SEND_EXAMPLES: &str = "\
Examples:
  by send fix-the-flaky-test \"now add a regression test\"
  by send fix-the-flaky-test \"also cover Windows\" --steer
  by send fix-the-flaky-test --prompt-file review.md     # a long prompt, from a file (- for stdin)
  by send fix-the-flaky-test \"now the docs\" --detach --yes   # in the background, as by run --detach";

pub(crate) const STEER_EXAMPLES: &str = "\
Examples:
  by steer fix-the-flaky-test \"also cover Windows\"
  by steer fix-the-flaky-test --prompt-file note.md     # from a file (- for stdin)";

pub(crate) const REVIEW_EXAMPLES: &str = "\
Opens the branch's diff in your editor. Write a comment on its own line
starting with >> under the line it is about (under a file's header: the whole
file; under an @@ line: the hunk). On save, every comment goes to the branch
as one prompt, formatted File / Line / User comment, as by send would send it.
With no comments nothing is sent; an unsent review is kept and reopened.

Examples:
  by review fix-the-flaky-test
  by review fix-the-flaky-test --editor \"code --wait\"
  by review fix-the-flaky-test --print
  by review fix-the-flaky-test --file review.diff --yes";

pub(crate) const FORK_EXAMPLES: &str = "\
Examples:
  by fork fix-the-flaky-test \"try a lock instead\" -n with-lock
  by fork fix-the-flaky-test \"try a lock instead\" --detach --yes   # in the background, as by run --detach";

pub(crate) const TASK_EXAMPLES: &str = "\
Every run is a task: `by run` starts one with one attempt, `by fan` one with an
attempt per harness, `by map` one whose items are its attempts. Beside each
checkpoint, the task's record is committed: the files with .task/ (what was
asked, each turn's conversation), never merged or diffed. A folder task keeps its git
directory in ~/.branchyard/tasks/<id>/ and writes the folder only on accept,
refusing to overwrite a file changed outside the task.

Examples:
  by task new --folder ~/Documents/board \"Update the Q3 deck from the new numbers\"
  by task new --no-files \"Draft a reply to the vendor\"
  by task ls
  by task show 01JA2B3C
  by task rewind 01JA2B3C --to 1 --yes
  by task fork 01JA2B3C --at 1 \"Try a shorter version\"
  by task accept 01JA2B3C --attempt update-the-q3-deck-2";

pub(crate) const REWIND_EXAMPLES: &str = "\
Each turn ends with a checkpoint, refs/branchyard/<branch>/<incarnation>/turn-<N>;
`by show` lists them. Later checkpoints are kept until the branch is removed, so
a rewind is undone by rewinding forward again. The next turn resumes the
harness's session only if it ended at that checkpoint; otherwise it starts a
fresh session with a summary of the turns before, and says so.

Examples:
  by rewind fix-the-flaky-test --to 2
  by rewind fix-the-flaky-test --to 4 --yes      # forward again
  by fork fix-the-flaky-test --at 1 \"try a lock instead\"";

pub(crate) const TRY_EXAMPLES: &str = "\
Applies the branch's diff against its base to this checkout, only when it is
clean, and records what it changed under .branchyard/try/ so --off restores
it exactly. Local mode only.

Examples:
  by try fix-the-flaky-test
  by try --status
  by try other-attempt      # swaps: the first try is turned off
  by try --off";

pub(crate) const COMPARE_EXAMPLES: &str = "\
Examples:
  by compare --fan speed-up-the-parser
  by compare a b c --check --json
  by compare --fan speed-up-the-parser --diff speed-up-the-parser-codex speed-up-the-parser-claude-code
  by compare --fan speed-up-the-parser --pick speed-up-the-parser-codex --discard-others";

pub(crate) const SPAWN_EXAMPLES: &str = "\
Examples (inside a harness, the parent is the harness's own branch):
  by spawn \"write the tokenizer\" --harness codex --budget-usd 1 --wait
  by spawn \"write the parser\" --depends-on tokenizer --after integrated
  by spawn \"fix it\" --parent root --yes            # outside a harness
  by spawn --prompt-file task.md --name parser       # a long task, from a file (- for stdin)
  by spawn \"list the unsafe blocks\" --result-schema '{\"type\":\"object\",\"required\":[\"files\"],\"properties\":{\"files\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}}}' --result-required

The child's check (--check) defaults to its parent's, and the spawn says which it
inherits. It runs on the merge when the child is integrated, so siblings that share
one test suite are integrated together: by integrate a b (checked once).";

pub(crate) const DELEGATE_EXAMPLES: &str = "\
Example, inside a harness (outside one, add --parent BRANCH):
  by delegate --plan plan.json        # then end your turn
  cat plan.json
  {\"brief\": \"Parse with the Token type in src/token.rs; add no crates.\",
   \"tasks\": [
     {\"name\": \"lexer\", \"prompt\": \"Write the lexer\", \"owns\": [\"src/lexer\"]},
     {\"name\": \"parser\", \"prompt\": \"Write the parser\", \"owns\": [\"src/parser\"],
      \"depends_on\": [\"lexer\"]}]}

The children are created all or none. Those that wait for nothing start at once, and
you are woken with a summary of each when they have all settled. by brief TEXT adds a
decision every child is given; by steer corrects one that is running.";

pub(crate) const DISCARD_EXAMPLES: &str = "\
Examples:
  by discard lru-linkedlist --reason \"the ordered-dict version won\"
  by discard flaky-fix --json

A running child is refused: by cancel it first. by cancel stops a turn; by
discard settles what a stopped or finished child is. Its worktree stays until
by rm.";

/// The edit format of `by graph apply`, from `branchyard::GraphEdit`.
pub(crate) const GRAPH_APPLY_HELP: &str = "\
A proposal is {\"expected_revision\": N, \"edits\": [EDIT, ...]}; --edits takes the
array alone. Each edit is an object tagged by \"kind\":

  {\"kind\": \"spawn\", \"prompt\": \"...\", ...}   a new child; it takes what by spawn
      does, by the MCP tool's names: name, harness, base, budget {max_usd,
      max_turns, max_minutes}, check [\"cmd\", \"arg\", ...] (a literal argv
      array, not a string to shell-split like by spawn --check), max_depth,
      max_children, harnesses [ids], deny [tools], seat, depends_on [names],
      after (settled or integrated), bindings [{scratch, access}], connectors
      [grants], plan, model
  {\"kind\": \"add_dependency\", \"dependent\": \"B\", \"prerequisite\": \"A\",
   \"after\": \"settled\"}   B waits for A; B must not have started
  {\"kind\": \"remove_dependency\", \"dependent\": \"B\", \"prerequisite\": \"A\"}

Examples:
  by graph show --json          # the revision to propose against
  by graph apply --expected-revision 3 --edits '[
    {\"kind\": \"spawn\", \"name\": \"schema\", \"prompt\": \"Add the migration\"},
    {\"kind\": \"spawn\", \"name\": \"api\", \"prompt\": \"Use the column\",
     \"depends_on\": [\"schema\"], \"after\": \"integrated\"}]'
  by graph apply proposal.json --parent root --yes      # outside a harness

All or nothing: a stale revision is the error stale_revision; run by graph
show and propose again. See docs/graph.md.";

pub(crate) const MERGE_EXAMPLES: &str = "\
Examples:
  by merge fix-the-flaky-test
  by merge fix-the-flaky-test --into release
  by merge fix-the-flaky-test --rm";

pub(crate) const WORKSPACE_EXAMPLES: &str = "\
Examples:
  by workspace show
  by workspace trust
  by workspace run fix-the-flaky-test dev
  by workspace show fix-the-flaky-test --json

[workspace] in branchyard.toml copies untracked files into each new worktree,
runs setup before its first turn and teardown when it is removed. Its scripts
never run until you trust them; see docs/workspace.md.";

pub(crate) const ENV_EXAMPLES: &str = "\
Examples:
  by env list
  by env show
  by env rebuild
  by env prune --keep 2 --older-than 7
  by env pool fill

With prepare = true in [workspace], setup runs once per environment key (the
setup commands, copy globs and lockfiles) and new branches start from what it
produced. A failed build never replaces the last good one. See
docs/environments.md. With [workspace.pool], ready worktrees wait for new
branches; by serve and by worker refill them. See docs/pools.md.";

pub(crate) const PR_EXAMPLES: &str = "\
Examples:
  by pr fix-the-flaky-test                       # push, then open or update the PR
  by pr fix-the-flaky-test --draft --base release
  by pr fix-the-flaky-test --watch --yes         # feed CI failures and reviews back
  by run --issue 42 --check \"cargo test\" && by pr issue-42-parser-crash

Needs the GitHub CLI, gh, logged in (gh auth login). The branch must be
ready and its check must pass on its candidate; --allow-not-ready and
--allow-failing-check override that. Local mode only. See docs/pull-requests.md.";

pub(crate) const HARNESSES_EXAMPLES: &str = "\
Examples:
  by harnesses                                  # installed here: version, login, quota
  by harnesses --on ssh://me@build.example      # on another machine
  by harnesses --remote https://by.internal     # on a server's live workers
  by harnesses install codex                    # the catalog's command, under [harnesses] install
  by harnesses update codex --version 0.200.0 --yes
  by harnesses login codex                      # its own login flow, once
  by harnesses login antigravity --yes          # with GEMINI_API_KEY set: select the gemini provider
  printf %s \"$KEY\" | by harnesses login claude-code --api-key
  by harnesses --profiles                       # the profiles Branchyard drives
  by harnesses --all --json | jq '.[] | select(.id == \"codex\") | .install'

Detection never reads a secret. LOGIN says what is known: logged in (the harness's
own status command said so), key set (VAR) or key stored (VAR) (a key, by name, not
checked), credentials found (a file exists, not read), logged out, login not
verified (nothing Branchyard looks for is there), unknown. Every installed harness
is asked its version at once, ten seconds at most (BRANCHYARD_HARNESS_TIMEOUT sets
the seconds); one that is slower is slow to start, not unknown. Installs run only
as [harnesses] install allows (ask on a terminal, never elsewhere, unless your user
file says otherwise) and are logged. --all reads catalog/harnesses.toml, generated
from emdash's and Orca's agent registries. See docs/harness-lifecycle.md.

login exits 0 when the login ran or a key was stored, 2 when it only printed
what you must run (no terminal here), and 1 when it failed; --json adds
\"status\": done, stored, manual or failed.";

pub(crate) const USAGE_EXAMPLES: &str = "\
Examples:
  by usage
  by usage --json | jq '.logins[] | {harness, five_hour: .five_hour.used_percent}'

Read from each login's own session files, never a credential: Codex records
its rate limits there; Claude Code records tokens only, so its percent needs
[usage] claude_five_hour_tokens. [usage] guard = \"refuse\" stops by run and
by fan near a limit; skip_over makes the router pass a candidate over. See
docs/usage.md.";

pub(crate) const ADOPT_EXAMPLES: &str = "\
Examples:
  by adopt                         # this repository's Claude Code and Codex sessions
  by adopt 3f2a9c --name parser    # make one a branch
  by send parser \"now add a test\"  # resumes the adopted session

The branch's worktree starts at the commit the session recorded (Codex), else
the HEAD of the directory it ran in, with that directory's uncommitted changes
to tracked files applied (--no-diff leaves them out). See docs/usage.md.";

pub(crate) const OPEN_EXAMPLES: &str = "\
Examples:
  by open fix-the-flaky-test
  by open fix-the-flaky-test --editor cursor
  cd \"$(by open fix-the-flaky-test --print)\"";

pub(crate) const WATCH_EXAMPLES: &str = "\
Examples:
  by watch
  by watch --interval 250ms
  by watch --once | less";

pub(crate) const SETUP_EXAMPLES: &str = "\
Steps, in order, each skipped when there is nothing to do:
  detect     the repository, the harnesses on PATH with their versions, whether each is
             logged in, and the check command the repository's files suggest
  config     branchyard.toml with the detected defaults, when there is none; on a terminal
             it asks only what cannot be detected (the default harness when several are
             logged in, a check when none is found, isolation)
  hooks      each harness on PATH that has an install (by init plugin): its hooks,
             commands and skills, with every file listed first; then a model-free check
             that its hook command answers
  logins     for each harness that is logged out or whose login cannot be told, the
             command to run; setup never starts a login flow
Prints a short card: what was detected, written and installed, \"Only you can do:\" lines,
and the next command. A second run writes nothing and says so. Exit 0 when done, even with
things left for you; 1 only on a real error. Without a terminal and without --yes it
behaves as --dry-run.

Examples:
  by setup                       # asks once before writing, on a terminal
  by setup --yes                 # take the defaults, ask nothing
  by setup --dry-run             # show everything, write nothing
  by setup --yes --harness claude-code,codex
  by setup --yes --no-hooks      # branchyard.toml only
  by setup --json --dry-run      # detected, written, installed, todo, next

See docs/setup.md#one-command.";

pub(crate) const INIT_EXAMPLES: &str = "\
Examples:
  by init                                            # the wizard, on a terminal
  by init server --defaults                          # the wizard, every default taken
  by init --json                                     # the topics
  by init project --json --next --answers answers.json
  by init project --answers answers.json --dry-run
  by init project --answers - --apply --json < answers.json
  by init server --defaults --apply --force
  by init plugin                                     # each harness Branchyard installs for, detected first
  by init plugin codex hermes                        # their hooks: every diff shown, then asks
  by init plugin --all-detected --dry-run            # the plan for every harness on PATH
  by init plugin cursor --apply                      # write without asking (--force replaces)

Answers name where a secret is (a variable, or @file), never its value.
Generated tokens are written 0600 and printed nowhere. See docs/setup.md.";

pub(crate) const CONFIG_EXAMPLES: &str = "\
Files: ~/.config/branchyard/config.toml (BRANCHYARD_USER_CONFIG overrides the
path), then branchyard.toml at or above the current directory, up to the
repository root. The project file overrides the user file key by key;
BRANCHYARD_REMOTE, BRANCHYARD_TOKEN_FILE, BRANCHYARD_REPO and
BRANCHYARD_CA_FILE override both; flags override everything. Neither file is
read inside a harness running on a branch (BRANCHYARD_BRANCH set).
Write one with `by init project`.

Examples:
  by config show
  by config show --json
  by config validate
  by config validate ~/.config/branchyard/config.toml
  by config schema > branchyard.config.json";

pub(crate) const UP_EXAMPLES: &str = "\
Steps, each ready, planned, needs_person or blocked, with a line on what to do:
  store            .branchyard/, made by opening the repository
  harness.<id>     each harness the configuration names ([defaults], [fleet], [knowledge];
                   claude-code when none): installed, or installed here when [harnesses]
                   in your user file allows it (ask on a terminal, --yes, or auto)
  login.<id>       logged in, or how to (the login command, the key variable)
  secret.<NAME>    each [secrets] entry and [models] key: its variable set or file present
  models           the model gateway [models] describes
  connectors       the connector gateway: listening, started here, or without Anvil
  provider         [defaults] provider: build and host access, Substrate connection,
                   or a recipe you trust
  workspace        [workspace] scripts trusted (by workspace trust)
Exit 0 when every step is ready, 1 otherwise. Nothing prints a secret's value.

Examples:
  by up                    # install what is allowed, start the gateway, list the rest
  by up --plan             # inspect readiness without installing or starting services
  by up --yes              # confirm the installs the policy asks about
  by up --json             # the steps as JSON, for a script or the Python SDK

See docs/up.md.
";

pub(crate) const HOOK_EXAMPLES: &str = "\
Reads Claude Code's hook JSON (session_id, cwd) on stdin and prints its hookSpecificOutput
with additionalContext, or nothing. With BRANCHYARD_BRANCH set: that branch's unread messages.
Otherwise: branches that finished, failed, became blocked or started asking since this
session's last hook call (remembered in .branchyard/hooks/<session_id>.json). Reads local
state only, gives up after two seconds, and always exits 0.

The plugin's hooks/hooks.json runs it; to run it from settings.json instead:
  \"hooks\": {\"UserPromptSubmit\": [{\"hooks\": [{\"type\": \"command\",
    \"command\": \"by hook user-prompt-submit\"}]}]}

Other harnesses: --harness ID reads that harness's hook JSON and answers in its shape, for
example `by hook --harness gemini-cli user-prompt-submit` (a BeforeAgent hook). An unknown
ID prints nothing and exits 0. --harness generic reads {\"cwd\", \"session_id\"} and prints
{\"context\": \"...\"}, for plugins written in code.

See docs/harness-integration.md and docs/harness-hooks.md.
";

pub(crate) const STATUSLINE_EXAMPLES: &str = "\
Prints, for example:  by: feat-x · 2 running · 1 asking · 3 ready
(the branch whose worktree Claude Code runs in, then the counts that are not zero).
Set it in ~/.claude/settings.json or .claude/settings.json; by never edits them:
  \"statusLine\": {\"type\": \"command\", \"command\": \"by statusline\"}

See docs/harness-integration.md.
";

pub(crate) const DOCTOR_EXAMPLES: &str = "\
Checks, each ok, todo, warn or fail with a line on what to do; a Mode: line above the
table says where branches run:
  repository       a repository; .branchyard/ and its store (none yet is a todo: by
                   setup); the store's schema
                   against this binary's; services whose lease ran out
  configuration    branchyard.toml and the user file load and validate (a refusal is
                   shown with its line); [workspace] scripts trusted, as they are now
  harnesses        what `by harnesses` detects, a row for each installed harness
                   (version, login, live run); [defaults] harness installed;
                   credential variables, by name only
  provider         the configured provider as the engine checks it; kept sandboxes
                   and snapshots against max_kept_minutes
  host             unprivileged user namespaces (egress confinement), a loopback
                   listener, /dev/kvm
  server           a configured [remote] answers GET /v1/repos with its token
  model gateway    [models] backends' key variables set; routed models priced

Exit status: 0 when nothing failed (a todo and a warning do not fail), 1 when something
did. Offline apart from a configured server, which gets 5 seconds. It opens an existing
.branchyard/ as every command does, creates none, and prints no secret.

Examples:
  by doctor
  by doctor --json
  by doctor --path ~/src/app
  by doctor --remote https://ci.example:8421 --token-file ~/.by-token

See docs/doctor.md.";

pub(crate) const MODELS_EXAMPLES: &str = "\
Examples:
  by models                        # routes, backends, budgets, and this month's usage
  by models --period day --json
  by models list                   # the models your keys can call, from the providers' own APIs
  by models list --refresh         # ask them again (otherwise the cache, with its age)
  by models list --harness codex   # the catalog's list beside what OpenAI lists now
  by run --model-gateway 'fix the bug'              # this branch's model calls go through the gateway
  by run --model-gateway='claude-sonnet-*' 'fix it' # and only to these models

[models] in branchyard.toml names the backends (their API, URL and the secret
holding each key), the routes by model, and daily and monthly budgets. A
branch on the gateway gets its own gateway each turn, on the turn's token; the
key never reaches the harness. See docs/model-gateway.md.";

pub(crate) const MODELS_LIST_EXAMPLES: &str = "\
Examples:
  by models list                   # every source whose key is set: Gemini, OpenAI, Anthropic, [models.backends]
  by models list --refresh         # list them again and cache the answer
  by models list --harness gemini-cli
  by models list --json

Each source whose key is in the environment (GEMINI_API_KEY, OPENAI_API_KEY or
CODEX_API_KEY, ANTHROPIC_API_KEY, or the key a [models.backends] entry names) is
asked for its models; one without a key is skipped, one that fails says its
status. A model neither catalog/pricing.toml nor [models.prices] can price is
marked: a dollar budget cannot count its calls. The answer is cached under
~/.cache/branchyard/catalog (or $BRANCHYARD_CATALOG_DIR) and shown, with its
age, until --refresh. HTTPS_PROXY, NO_PROXY and SSL_CERT_FILE are honored.
Keys are never printed. See docs/model-gateway.md.";

pub(crate) const SYNC_EXAMPLES: &str = "\
Examples:
  by sync                          # push and pull every branch that changed
  by sync fix-login                # one branch
  by sync status                   # the remote, each task's state and lag, the counters
  by sync pull 3f2a9c01be47.fix-login
  by sync gc --dry-run
  by sync scrub --sample 500
  by sync hold fix-login --reason \"audit 42\"

[sync] in your user configuration (~/.config/branchyard/config.toml) says where:
remote = \"gs://bucket/prefix\" (or s3://, az://, file:///, git+https://),
encrypt = \"passphrase\" or \"kms://...\", interval, bandwidth, retention. Objects are
written first and each task's manifest is swapped by compare-and-swap; two
machines that moved one branch apart keep both, the second as
refs/heads/conflict/<device>/<n>. See docs/sync.md.";

pub(crate) const SERVICES_EXAMPLES: &str = "\
Examples:
  by services                      # this repository's live services
  by services --kind connector_gateway --json
  by services --all                # with those that left or were reclaimed
  by services gc                   # reclaim what a stopped owner left
  by --remote https://by.example services   # a server's fleet

Services register themselves with what they can do and a lease their owner
renews; consumers find them by capability. A service whose owner stopped is
reclaimed: a leaked process stopped, a sandbox or recipe machine recovered
and destroyed, pool slots removed. Nothing Branchyard did not start is ever
stopped. See docs/registry.md.";

pub(crate) const CATALOG_EXAMPLES: &str = "\
Examples:
  by catalog refresh               # the MCP registry and npm, cached and verified
  by catalog refresh --only harnesses
  by catalog status --json

Nothing is fetched unless you ask: catalog/connectors.toml and
catalog/harnesses.toml stay the pinned baseline, and what a refresh adds is
cached with its ETag and checksum under ~/.cache/branchyard/catalog (or
$BRANCHYARD_CATALOG_DIR). A cache whose checksum does not verify is refused.
See docs/registry.md.";

pub(crate) const GATEWAY_EXAMPLES: &str = "\
Examples:
  by gateway start                 # in the background; its log in .branchyard/gateway/
  by gateway status --json
  by gateway start --foreground    # in this terminal, until interrupted
  by gateway rotate-key            # a new signing key; the previous one stays valid
  by gateway stop

[connectors] in branchyard.toml says where the gateway is (gateway), which
bundles it serves (bundles) and how to run Anvil (anvil). The gateway reads
this yard's public keys from .branchyard/gateway/jwks.json and writes its audit
log to .branchyard/gateway/audit.jsonl, which by log shows. See
docs/connectors.md.";

pub(crate) const CONNECT_EXAMPLES: &str = "\
Examples:
  by connect github
  by connect github --account work --open
  by connect github --api-key-stdin < ~/.config/github-token

Runs `anvil connect` against the gateway as you: it opens (or prints) the
connector's authorization URL, and the gateway keeps the upstream token. A
harness never sees it. See docs/connectors.md.";

pub(crate) const COMPLETIONS_EXAMPLES: &str = "\
Examples:
  by completions bash > ~/.local/share/bash-completion/completions/by
  by completions zsh > \"${fpath[1]}/_by\"
  by completions fish > ~/.config/fish/completions/by.fish
  by completions powershell >> $PROFILE";

/// `by`'s command, with the first-run command list in its help. Built on a
/// thread with [`PARSE_STACK`], like parsing, so help, completions and the
/// man page never depend on the caller's stack either.
pub fn command() -> clap::Command {
    on_parse_stack(build_command)
}

/// [`command`] with the commands that are hidden from `by --help` shown, as
/// the man page lists them.
pub fn command_with_hidden() -> clap::Command {
    on_parse_stack(|| {
        let mut cmd = build_command();
        let names: Vec<String> = cmd
            .get_subcommands()
            .filter(|c| c.is_hide_set())
            .map(|c| c.get_name().to_owned())
            .collect();
        for name in names {
            cmd = cmd.mut_subcommand(name, |c| c.hide(false));
        }
        cmd
    })
}

pub(crate) fn build_command() -> clap::Command {
    let cmd = crate::operations::annotate(Cli::command());
    let header = *cmd.get_styles().get_header();
    let listing = command_listing(&cmd, Listing::Front, true);
    cmd.help_template(format!(
        "{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{listing}\
         {header}Options:{header:#}\n{{options}}{{after-help}}"
    ))
}

/// What `by --help` and `by help all|agents` list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listing {
    /// The first-run commands, [`FRONT`], and where to find the rest.
    Front,
    /// Every command, hidden or not, under [`GROUPS`]: `by help all`.
    All,
    /// What a harness runs for itself: `by help agents`.
    Agents,
}

/// The commands of `by --help`, in the order a first hour uses them. A name
/// no command has (a command another change adds) is left out.
pub const FRONT: &[(&str, &[&str])] = &[
    ("Start", &["setup", "init", "doctor"]),
    (
        "Work",
        &[
            "run", "fan", "send", "steer", "wait", "cancel", "merge", "pr",
        ],
    ),
    ("Look", &["ls", "show", "diff", "log", "watch"]),
    ("Clean up", &["rm", "rewind"]),
];

/// The commands of `by help agents` besides the group of delegation: what
/// the engine and the plugins run.
pub(crate) const AGENT_PLUMBING: &[&str] = &["mcp", "hook", "statusline"];

/// The listing `by help NAME` prints when NAME is a topic and not a
/// command: `all` and `agents`. `args` are `by`'s, after the program name;
/// global options may come first.
pub fn help_listing(args: &[OsString]) -> Option<Listing> {
    let mut at = 0;
    while let Some(arg) = args.get(at).and_then(|a| a.to_str()) {
        let Some(long) = arg.strip_prefix("--") else {
            break;
        };
        let name = long.split_once('=').map_or(long, |(name, _)| name);
        if name == "no-notify" {
            at += 1;
        } else if ["remote", "token-file", "repo", "ca-file"].contains(&name) {
            at += if long.contains('=') { 1 } else { 2 };
        } else {
            return None;
        }
    }
    let rest: Vec<&str> = args
        .get(at..)?
        .iter()
        .map(|a| a.to_str())
        .collect::<Option<_>>()?;
    match rest[..] {
        ["help", "all"] => Some(Listing::All),
        ["help", "agents"] => Some(Listing::Agents),
        _ => None,
    }
}

/// The text of `by help all` or `by help agents`; `color` for a terminal.
pub fn listing_text(listing: Listing, color: bool) -> String {
    on_parse_stack(move || {
        let cmd = build_command();
        let mut text = command_listing(&cmd, listing, color);
        text.push_str("Run 'by help <command>' or 'by <command> --help' for its options.\n");
        text
    })
}

/// The command list of a help screen: `listing`'s, with ANSI styles when
/// `color` (clap strips them again when the help goes to a pipe).
pub(crate) fn command_listing(cmd: &clap::Command, listing: Listing, color: bool) -> String {
    let mut built = cmd.clone();
    built.build();
    let styles = built.get_styles();
    let (header, literal): (Style, Style) = match color {
        true => (*styles.get_header(), *styles.get_literal()),
        false => (Style::new(), Style::new()),
    };
    let find = |name: &str| built.get_subcommands().find(|c| c.get_name() == name);
    let titled = |title: &str, rows: &[&clap::Command]| -> String {
        let width = rows
            .iter()
            .map(|c| c.get_name().len())
            .max()
            .unwrap_or(0)
            .max(10);
        let mut text = format!("{header}{title}:{header:#}\n");
        for c in rows {
            let about = c.get_about().map(|a| a.to_string()).unwrap_or_default();
            let name = c.get_name();
            let pad = " ".repeat(width - name.len());
            text.push_str(&format!("  {literal}{name}{literal:#}{pad}  {about}\n"));
        }
        text.push('\n');
        text
    };
    let mut text = String::new();
    match listing {
        Listing::Front => {
            for (title, names) in FRONT {
                let rows: Vec<&clap::Command> = names.iter().filter_map(|n| find(n)).collect();
                if !rows.is_empty() {
                    text.push_str(&titled(title, &rows));
                }
            }
            let shown: Vec<&str> = FRONT
                .iter()
                .flat_map(|(_, names)| names.iter().copied())
                .collect();
            let others: Vec<&str> = built
                .get_subcommands()
                .filter(|c| !c.is_hide_set() && c.get_name() != "help")
                .map(|c| c.get_name())
                .filter(|name| !shown.contains(name))
                .collect();
            text.push_str(&format!("{header}More:{header:#}\n"));
            for (what, says) in [
                ("by help all", "Every command, with what agents use"),
                ("by help agents", "What a harness runs for itself"),
                ("by <command> --help", "Options for one command"),
            ] {
                let pad = " ".repeat(22 - what.len());
                text.push_str(&format!("  {literal}{what}{literal:#}{pad}{says}\n"));
            }
            text.push_str(&wrapped("Also", &others));
            text.push('\n');
        }
        Listing::All => {
            let group = |c: &clap::Command| match c.get_name() {
                "help" => GROUPS.last().map_or(9, |(group, _)| *group),
                _ => c.get_display_order() / 100,
            };
            let mut groups: Vec<(usize, &str)> = GROUPS.to_vec();
            groups.push((usize::MAX, "Other commands"));
            for (number, title) in groups {
                let mut members: Vec<&clap::Command> = built
                    .get_subcommands()
                    .filter(|c| {
                        let g = group(c);
                        g == number
                            || (number == usize::MAX && !GROUPS.iter().any(|(n, _)| *n == g))
                    })
                    .collect();
                if members.is_empty() {
                    continue;
                }
                members.sort_by_key(|c| (c.get_display_order(), c.get_name()));
                text.push_str(&titled(title, &members));
            }
        }
        Listing::Agents => {
            let mut members: Vec<&clap::Command> = built
                .get_subcommands()
                .filter(|c| {
                    c.get_display_order() / 100 == 3 || AGENT_PLUMBING.contains(&c.get_name())
                })
                .collect();
            members.sort_by_key(|c| (c.get_display_order(), c.get_name()));
            text.push_str(&titled(
                "Commands a harness runs for itself (usually from inside its branch)",
                &members,
            ));
        }
    }
    text
}

/// `label: a, b, c` wrapped to 78 columns under the two-space indent.
pub(crate) fn wrapped(label: &str, names: &[&str]) -> String {
    let mut out = String::new();
    let mut line = format!("  {label}: ");
    let indent = " ".repeat(line.len());
    for (n, name) in names.iter().enumerate() {
        let piece = match n + 1 == names.len() {
            true => (*name).to_owned(),
            false => format!("{name}, "),
        };
        if line.trim_end().len() + piece.len() > 78 && line.trim().len() > label.len() + 1 {
            out.push_str(line.trim_end());
            out.push('\n');
            line = indent.clone();
        }
        line.push_str(&piece);
    }
    out.push_str(line.trim_end());
    out.push('\n');
    out
}
