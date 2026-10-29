<img src="assets/brand/icon.svg" width="56" height="56" alt="Branchyard">

# Branchyard

Run agent tasks in parallel. Review each result before you merge it.

Branchyard gives each task its own Git branch and worktree, on your machine or a remote worker. The `by` CLI tracks the work and runs your checks before merging changes. Agents keep their own planning and subagents.

[Get started](docs/getting-started.md) · [Documentation](docs/README.md) · [Status](docs/status.md)

## Install

From your repository, install the released binaries and open guided setup:

```sh
curl -fsSL https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh | sh -s -- --setup
```

The installer downloads prebuilt binaries. Setup detects installed agents, writes project defaults, and installs their Branchyard integrations. It prints any login or trust steps you need to finish. Run `by up --plan` to check readiness afterward.

See [installation options](docs/distribution.md#install-by) for version pinning, headless machines, and source builds.

## Run, review, merge

With Antigravity CLI installed and signed in, start a task in a Rust repository:

```sh
by run "Make the flaky parser test deterministic. Add a regression test." \
  --name parser-fix --harness antigravity \
  --check "cargo test" --yes --allow-unapproved-tools
by diff parser-fix
by merge parser-fix
```

Use your project's test command in `--check`. Branchyard creates `by/parser-fix` in a separate worktree. `by merge` runs the check against the combined result before moving your branch.

The permission flags let Antigravity use its configured tools. Local tasks run as your operating-system user. [Execution modes](docs/modes.md) explains private homes and sandboxed workers.

Use `by watch` to follow several tasks, or [set up a team](docs/cooperative-teams.md) with separate implementation and review work. [Harness status](docs/status.md#harness-drivers) records which agent versions have passed live checks.

## Work with documents and data

The installed skills cover source capture, table extraction, data cleaning, calculations, and editable documents. Design skills help workers share typography, chart conventions, and layouts before they draft separate sections.

Start at the [skills index](plugins/branchyard/skills/INDEX.md), then follow the [knowledge and design guide](docs/knowledge-skills.md). Skills load through topic and folder indexes, so an agent can read the instructions relevant to its task.

For a large presentation, use the [parallel deck example](examples/rigs/parallel-deck.toml). Workers produce section files; one renderer assembles the deck. The [artifact capture guide](docs/artifact-capture.md) explains how to collect and share completed files, including files produced remotely.

The artifact runtime needs explicit dependency setup. Office rendering also needs LibreOffice and Poppler. [Runtime setup](docs/artifact-toolkit.md#installed-skills-and-runtime) covers both. Google Workspace operations need a connected account with the required upstream permissions.

## Use a remote worker

With `by` and your agent installed on a worker you can SSH into:

```sh
by --remote ssh://you@worker.example/srv/project ls
```

The SSH route starts a remote Branchyard service and forwards its private socket. The same CLI runs and reviews tasks there. Follow the [SSH guide](docs/remote-ssh.md) to configure the worker, or provision one with the [GCE guide](docs/compute-gce.md). [Agent Substrate on GKE](docs/substrate-gke.md) covers Kubernetes deployment.

The [remote validation record](docs/remote-validation-2026-10-28.md) and [document-work validation](docs/atomic-knowledge-validation-2026-10-29.md) name the environments tested and the remaining gaps. Branchyard is in early development; qualification applies to the recorded versions and scenarios.

## Find the next step

- [Getting started](docs/getting-started.md) includes a walkthrough without a model.
- [Setup](docs/setup.md) covers configuration and logins. [Troubleshooting](docs/troubleshooting.md) covers failed and stuck tasks.
- [SDKs](docs/sdk.md) and [the server](docs/server.md) expose the same operations to applications.
- [Documentation index](docs/README.md) lists the task guides and references.
- [Contributing](CONTRIBUTING.md) describes the repository's development checks.

## License

Branchyard-authored code is [Apache-2.0](LICENSE). Bundled dependencies and reference material retain their own licenses. See [third-party notices](THIRD_PARTY.md).
