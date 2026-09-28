# Distribution

Two canonical skills: [`plugins/branchyard/skills/setup`](../plugins/branchyard/skills/setup/SKILL.md) teaches a harness to set Branchyard up by interviewing the user through `by init --json` ([setup](setup.md)), and [`plugins/branchyard/skills/delegate`](../plugins/branchyard/skills/delegate/SKILL.md) teaches it to use `by`'s delegation commands (`docs/delegation.md`). The Claude plugin also has a `/branchyard:setup` command ([`commands/setup.md`](../plugins/branchyard/commands/setup.md)) that starts the setup skill. Two plugin manifests, [`.claude-plugin/plugin.json`](../plugins/branchyard/.claude-plugin/plugin.json) and [`.codex-plugin/plugin.json`](../plugins/branchyard/.codex-plugin/plugin.json), point at the same `./skills/` directory, so Claude Code and Codex load an identical skill; a host with neither plugin loader installs an exact standalone copy with the shipped installer. Nothing here downloads or builds a binary: install the `by` CLI separately (`cargo install --locked --path crates/branchyard-cli`, or a released archive of your own), the same binary the skill's instructions call by name.

## Install for Claude Code

Point Claude Code's plugin loader at the checkout or an extracted archive:

```sh
claude --plugin-dir plugins/branchyard
```

## Install for Codex

Register the packaged directory (`plugins/branchyard`, containing `.codex-plugin/plugin.json`) through Codex's own plugin mechanism. Both manifests are validated against real installed `.codex-plugin/plugin.json` and `.claude-plugin/plugin.json` files (this account's own plugins, and public plugins such as Superpowers and OpenRig Core) for their field shapes; there is no vendored schema to check them against mechanically, so a live install is the final confirmation, same as PR #1 left it.

## Install with `by`

`by init plugin` installs the skills `by` was built with (embedded byte for byte from `plugins/branchyard/skills/`, so no checkout is needed) into a project's `.claude/skills`, `~/.claude/skills`, or `~/.codex/skills`, as a diffed plan like every `by init` topic, or prints the `claude --plugin-dir` command for the whole plugin.

## Install anywhere else with a skill directory

```sh
python3 plugins/branchyard/scripts/install_skill.py --destination /path/to/host/skills
python3 plugins/branchyard/scripts/install_skill.py --destination /path/to/host/skills --apply
python3 plugins/branchyard/scripts/install_skill.py --destination /path/to/host/skills --skill setup --apply
```

`--skill` picks `delegate` (the default) or `setup`.

The installer previews by default; `--apply` copies. It targets one explicit directory and never discovers or edits global host configuration. Run it once per host to avoid duplicated instructions from more than one copy.

If `delegate/` already exists at the destination with the exact canonical bytes, `--apply` is a no-op (`{"applied": false, "unchanged": true}`). If it exists with different content, the installer refuses (`destination_exists`, exit 2) unless `--force` is also given, which replaces it. It never silently overwrites someone else's edits.

## Archives

```sh
python3 tools/package.py --output dist
```

Produces four ZIP archives in `dist/`, each named `branchyard-<kind>-<version>.zip`:

| Archive | Contents |
|---|---|
| `branchyard-plugin-<version>.zip` | `plugins/branchyard/`, including both skills, the `/branchyard:setup` command, both manifests, `LICENSE`, and a `MANIFEST.sha256.json` of every file's digest |
| `branchyard-skill-<version>.zip` | The standalone delegate skill alone, `plugins/branchyard/skills/delegate/`, for a host with no plugin loader |
| `branchyard-setup-skill-<version>.zip` | The standalone setup skill alone, `plugins/branchyard/skills/setup/` |
| `branchyard-sdk-<version>.zip` | `sdk/python/`: the Python module that wraps `by --json` (`docs/sdk.md`) |

Building refuses to proceed if either plugin manifest's `name` or `version` does not match the workspace's own `Cargo.toml` version, so a stale manifest cannot ship silently.

**Reproducible.** Every archive is deterministic: inputs are read in sorted path order, every entry gets a fixed timestamp (`1980-01-01T00:00:00`, ZIP's minimum) and mode (`0644`), and compression is deflate at a fixed level. Two builds from the same source tree are byte-for-byte identical; `tests/test_distribution.py` checks this, along with every manifest entry's digest.

**Tested outside the checkout.** An editable or source install can hide a missing release input (a file that exists in the working tree but was never added to `tools/package.py`'s source roots). `tests/test_distribution.py` extracts the plugin, skill and SDK archives into a temporary directory outside the repository, then:

- Runs the shipped `install_skill.py` from the extracted plugin, checking the preview/apply/no-op/refuse/`--force` behavior above and that the installed skill's bytes match the canonical source.
- Imports the extracted SDK module and calls it against a stub `by`, checking it runs standalone (no `sys.path` trick back into the checkout) and its bytes match the canonical source.
- Compares the plugin archive's bundled skills against the standalone skill archives: each holds the same canonical bytes, `plugins/branchyard/skills/<name>/` inside one and the top level of the other; and checks the plugin ships `commands/setup.md`.
- Installs the setup skill with `--skill setup`.

Run it directly, or as part of `.github/workflows/check.yml`:

```sh
python3 tests/test_distribution.py
```
