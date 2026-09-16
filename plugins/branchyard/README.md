# Branchyard plugin

A portable skill for the Branchyard remote control client. One canonical skill
is included in both the plugin and standalone archives. Install one form per
harness. There are no hooks, automatic worker launches, or hidden MCP servers.

## Prerequisites

Build the CLI from the Branchyard repository:

```sh
cargo install --locked --path crates/branchyard-cli
branchyard --version
```

The CLI is not yet published to crates.io or included as a platform binary in
this archive. Rust 1.90 and the platform C/CMake build tools for its TLS dependency
are needed for a source build. A compatible server and a scoped credential are
required only for remote calls. Production server execution is not implemented
in this repository yet; offline validation works today.

Supply `BRANCHYARD_ENDPOINT` and `BRANCHYARD_TOKEN` through your deployment's
credential mechanism. Do not store the token in plugin metadata or command files.

## Load as a plugin

This directory contains Codex and Claude plugin manifests sharing `skills/`.
Use your host's local plugin loader. The metadata and distribution are validated;
live installation across every host/version has not been qualified. The [Claude plugin documentation](https://code.claude.com/docs/en/plugins)
describes the local development loader, which accepts `claude --plugin-dir /absolute/path/branchyard`.
For Codex, register this directory with your team's plugin distribution mechanism.

## Load as a standalone skill

For hosts with a skill-directory convention, explicitly choose that directory:

```sh
python3 scripts/install_skill.py --destination /path/to/project/.agents/skills
python3 scripts/install_skill.py --destination /path/to/project/.agents/skills --apply
```

The first command previews; the second copies. An existing `branchyard` directory
is never replaced. No global host configuration is changed. Do not install the
standalone form when the same host already loads this plugin's skill.

The standalone archive can also be extracted directly under the chosen skill
root. Hosts without skills can call the CLI using their normal terminal tool.
The optional Python launcher forwards arguments without a shell and accepts
`BRANCHYARD_BIN` as a single executable path.

## Use

```sh
branchyard describe
branchyard validate --file request.json
branchyard doctor
branchyard call --file request.json
branchyard reconcile --file request.json
```

Keep the saved request across uncertain submissions. Exit 4 requires
reconciliation, not a replacement operation ID. All managed execution belongs
on the server; the plugin cannot create a local sandbox or launch a harness.

See the included [skill](skills/branchyard/SKILL.md) and
[operation reference](skills/branchyard/references/operations.md).
