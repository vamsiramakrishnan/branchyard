<img src="assets/brand/icon.svg" width="56" height="56" alt="Branchyard">

# Branchyard

Run agent tasks in parallel, then review what comes back.

Branchyard gives each task its own Git branch and worktree: a separate working copy of your repository. The `by` command tracks progress and runs your checks before merging code. For knowledge work, agents can return documents, data and reports without changing code. You choose which results to keep.

## Install and check setup

From your repository, install the released binaries and start guided setup:

```sh
curl -fsSL https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh | sh -s -- --setup
```

Setup finds installed agents, writes project settings and connects them to Branchyard. It tells you which login or trust steps remain. Run `by up --plan` to check readiness without installing tools or starting services. See [installation options](docs/distribution.md#install-by) for version pinning and headless machines.

## Run a task and review it

With Antigravity CLI installed and signed in, try this in a Rust repository:

```sh
by run "Make the flaky parser test deterministic. Add a regression test." \
  --name parser-fix --harness antigravity \
  --check "cargo test" --yes --allow-unapproved-tools
by diff parser-fix
by merge parser-fix
```

Replace `cargo test` with your project's test command. The task runs on `by/parser-fix`. After you inspect the diff, `by merge` checks the combined changes before merging them.

`--allow-unapproved-tools` lets Antigravity use its own tool permissions. `--yes` approves requests that reach Branchyard. Local agents run as your operating-system user, with access to your files. See [execution modes](docs/modes.md) for isolation options.

Use `by watch` to follow tasks, or [organize a team](docs/cooperative-teams.md) with separate implementation and review work.

## Work with documents and data

Open a browser console from a directory that does not need to be a Git repository:

```sh
by serve --app --knowledge
```

Open the printed URL, create a knowledge project and upload your sources. Start named sessions, inspect their conversations, preview the documents they produce and accept the outputs you keep. Each session has its own working copy. See the [console guide](docs/companion.md#projects-and-workspaces) for credentials, permissions and preview setup.

The [skills](plugins/branchyard/skills/INDEX.md) cover sources, tables, data cleaning, calculations and editable documents. Design skills guide layouts and charts. The [parallel deck example](examples/rigs/parallel-deck.toml) lets workers draft separate sections, then assembles one presentation.

Install the required packages before using the [document tools](docs/artifact-toolkit.md#installed-skills-and-runtime). Office previews require LibreOffice and Poppler. Google Workspace work needs a connected account and your permission to access the documents. See [knowledge work](docs/knowledge-skills.md) and [collecting output files](docs/artifact-capture.md).

## Use a remote worker

With `by` and your agent installed on an SSH worker:

```sh
by --remote ssh://you@worker.example/srv/project ls
```

Follow the [SSH guide](docs/remote-ssh.md), [GCE setup](docs/compute-gce.md) or [GKE setup](docs/substrate-gke.md). Branchyard is in early development; [status](docs/status.md) records tested versions and remaining gaps.

## Documentation and license

[Getting started](docs/getting-started.md) · [All guides](docs/README.md) · [SDKs](docs/sdk.md) · [Contributing](CONTRIBUTING.md)

Branchyard's own code is [Apache-2.0](LICENSE). Bundled dependencies retain their own licenses; see [third-party notices](THIRD_PARTY.md).
