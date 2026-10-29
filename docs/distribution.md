# Distribution

The canonical [`plugins/branchyard/skills/setup`](../plugins/branchyard/skills/setup/SKILL.md) teaches a harness to set Branchyard up by interviewing the user through `by init --json` ([setup](setup.md)), and [`plugins/branchyard/skills/delegate`](../plugins/branchyard/skills/delegate/SKILL.md) teaches it to use `by`'s delegation commands (`docs/delegation.md`). Atomic document skills and their dependency graph live beside them in [`capabilities.json`](../plugins/branchyard/capabilities.json). Their shared `artifact-tools` payload includes the executable Python toolkit, examples and schemas; installation does not require this source checkout afterward. The Claude plugin also has three slash commands, `/branchyard:setup` ([`commands/setup.md`](../plugins/branchyard/commands/setup.md)), which starts the setup skill, `/branchyard:status` and `/branchyard:wait`, and two hooks ([`hooks/hooks.json`](../plugins/branchyard/hooks/hooks.json)) that tell a session when its branches finish ([harness integration](harness-integration.md#claude-code)). Two plugin manifests, [`.claude-plugin/plugin.json`](../plugins/branchyard/.claude-plugin/plugin.json) and [`.codex-plugin/plugin.json`](../plugins/branchyard/.codex-plugin/plugin.json), point at the same `./skills/` directory, so Claude Code and Codex load identical skills; a host with neither plugin loader installs an exact standalone copy with the shipped installer. The skills call the `by` CLI by name; [install it](#install-by) before installing a plugin.

## Install `by`

Install and enter the guided setup with one command:

```sh
curl -fsSL https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh | sh -s -- --setup
```

The installer downloads prebuilt `by`, `branchyard-server` and `branchyard-bridge`
from the latest stable GitHub release. It verifies archive checksums through the
pinned dist-generated installers, installs into `~/.local/bin` and starts
`by setup --guided` using the controlling terminal. It needs Git, curl or wget,
and a SHA-256 tool; it does not need Rust, Python or a source checkout to install
and use local or existing remote backends. Guided cloud provisioning reports its
additional tools before creating resources.

To install a particular version or prepare a headless machine:

```sh
sh install.sh --version 0.3.0 --prefix "$HOME/.local" --no-setup
sh install.sh --version 0.3.0 --setup --yes
```

`--yes` follows the existing noninteractive setup defaults and readiness checks;
it does not create cloud resources. Without a terminal, an explicit `--setup`
requires `--yes`; installation remains available with `--no-setup`. A default
install without either option starts guided setup when a terminal is available
and prints the next command otherwise. Existing shell profiles are managed by
the dist installer; open a new shell to pick up its PATH change.

For a source build, clone the repository and use the pinned toolchain:

```sh
git clone https://github.com/vamsiramakrishnan/branchyard && cd branchyard
cargo install --locked --path crates/branchyard-cli
```

`rust-toolchain.toml` pins Rust 1.94.0. Source builds need that toolchain; binary
installation does not. [Prebuilt packaging](#prebuilt-binaries) describes the
release artifacts and their verification.

## Install into a harness

`by setup` does this for every harness it detects ([setup](setup.md#one-command)). By hand, one harness at a time, each row is what to run, what it writes and what only you can do next. Every `by init plugin ID` plans first: on a terminal it shows the diff and asks, and elsewhere it prints the plan and writes nothing until `--apply` (`--dry-run` never writes); [harness hooks](harness-hooks.md) has a section per harness, and [below](#install-per-harness) is how the manifests work. Where a row offers a plugin and `by init plugin`, install one, not both, or each session hears the news twice.

| Harness | Command | What lands | Next step |
|---|---|---|---|
| Claude Code | `by init plugin claude-code`, or `/plugin marketplace add vamsiramakrishnan/branchyard` then `/plugin install branchyard@branchyard` | All canonical skills and the shared artifact toolkit, three commands and the `SessionStart` and `UserPromptSubmit` hooks | Restart Claude Code; `/hooks` lists the two hooks. Direct installation adds a status line if absent; a marketplace plugin cannot set one |
| Codex | `by init plugin codex`, or `codex plugin marketplace add vamsiramakrishnan/branchyard` then `codex plugin add branchyard@branchyard` | `by init plugin`: all canonical skills in `~/.agents/skills` and the hooks merged into `~/.codex/hooks.json`. The plugin carries the same skills, toolkit and hooks | **Trust the hooks.** Codex runs none until you do: start `codex` once and choose "Trust all and continue" at "Hooks need review", or run `/hooks` and press `t`. `codex exec` skips an untrusted hook silently; for an unattended run pass `--dangerously-bypass-hook-trust` |
| Gemini CLI | `by init plugin gemini-cli`, or `gemini extensions install <clone>/plugins/branchyard/harnesses/gemini-cli` | The `BeforeAgent` hook merged into `~/.gemini/settings.json`, and the commands `/branchyard:setup`, `/branchyard:status`, `/branchyard:wait` (the extension names them `/setup`, `/status`, `/wait`) | Hooks run only in a trusted folder; `gemini -p` needs `--skip-trust` or `GEMINI_CLI_TRUST_WORKSPACE=true`. `/hooks panel` lists `branchyard-before-agent` |
| Qwen Code | `by init plugin qwen-code` | The hooks and status line, merged into `~/.qwen/settings.json` | Nothing to enable. For the commands: `qwen extensions install <clone>/plugins/branchyard/harnesses/gemini-cli` |
| Antigravity | `by init plugin antigravity`, or `agy plugin install <clone>/plugins/branchyard/harnesses/antigravity` | Direct installation adds all canonical skills in `~/.gemini/antigravity-cli/skills`, hooks in `~/.gemini/config/hooks.json` and the status line in `~/.gemini/antigravity-cli/settings.json`. The CLI plugin supplies the hooks | None |
| GitHub Copilot CLI | `by init plugin github-copilot` | Hooks in `~/.copilot/hooks/branchyard.json`, the status line in `~/.copilot/settings.json` | Copilot drops what a prompt-submit hook prints from a file, so a session hears at its start; run `by ls` for news after |
| Cursor | `by init plugin cursor` | Hooks in `~/.cursor/hooks.json`, the status line in `~/.cursor/cli-config.json` | Skills and commands come with the Cursor plugin (`.cursor-plugin/marketplace.json`). Cursor hears at session start only |
| Droid | `by init plugin droid` | Hooks in `~/.factory/hooks.json`, the status line in `~/.factory/settings.json` | Move any `hooks` you keep in `settings.json` to `hooks.json`; review them with `/hooks` |
| Kiro | `by init plugin kiro` | `~/.kiro/hooks/branchyard.json` | Kiro CLI V3 reads global hooks; for the IDE copy the file to `.kiro/hooks/branchyard.json` in the repository |
| DeepSeek Harness | `by init plugin deepseek-harness` | `~/.dsh/branchyard-hooks.json` and an insert in `~/.dsh/cordis.patch.yml` | Add the hook bridge to each profile: `dsh plugin --profile NAME add @deepseek-ai/dsh-hooks-claude-code@VERSION @deepseek-ai/dsh-hook-protocol@VERSION`, at your `dsh --version` |
| Hermes | `by init plugin hermes` | A `hooks:` block in `~/.hermes/config.yaml` | Answer `y` once for the `pre_llm_call` hook in `hermes chat` at a terminal; without one Hermes skips the hook |
| Pi | `by init plugin pi` | `~/.pi/agent/extensions/branchyard.ts` | `/reload` in Pi, or start a new session |
| Oh My Pi | `by init plugin oh-my-pi` | `~/.omp/agent/extensions/branchyard.ts` | Restart Oh My Pi |
| OpenCode | `by init plugin opencode` | `~/.config/opencode/plugins/branchyard.js` | Restart OpenCode (written for `@opencode-ai/plugin` 1.19) |
| Amp | `by init plugin amp` | `~/.config/amp/plugins/branchyard.ts` | "plugins: reload" in Amp's command palette, or restart it |

## Install for Claude Code

The repository is a Claude Code plugin marketplace: [`.claude-plugin/marketplace.json`](../.claude-plugin/marketplace.json) at its root lists the one plugin, `branchyard`, at `./plugins/branchyard`. In Claude Code:

```text
/plugin marketplace add vamsiramakrishnan/branchyard
/plugin install branchyard@branchyard
```

That gives all the canonical skills, the `/branchyard:setup`, `/branchyard:status` and `/branchyard:wait` commands, and the `SessionStart` and `UserPromptSubmit` hooks, which run `by hook` when `by` is on `PATH` and do nothing otherwise; in a repository driven through a server (`[remote]` or `BRANCHYARD_REMOTE`) they ask that server, and a server's PostgreSQL store is read only by a `by` built with the `postgres` feature ([harness integration](harness-integration.md)). The status line is not part of a plugin (a plugin cannot set one); `by init plugin claude-code` adds it to your Claude Code settings when you have none ([below](#install-per-harness)), or add `"statusLine": {"type": "command", "command": "by statusline"}` yourself.

Or point Claude Code's plugin loader at a checkout or an extracted archive for one session:

```sh
claude --plugin-dir plugins/branchyard
```

## Install for Codex

`codex plugin marketplace add vamsiramakrishnan/branchyard` (or the path of a clone), then `codex plugin add branchyard@branchyard`. Codex reads [`.agents/plugins/marketplace.json`](../.agents/plugins/marketplace.json), which lists the packaged directory (`plugins/branchyard`, containing `.codex-plugin/plugin.json`). The plugin gives the skills and the two hooks; `by init plugin codex` installs the same skills and hooks directly, so use one or the other. Codex runs no hook until you trust it ([the table above](#install-into-a-harness), [harness hooks](harness-hooks.md#codex)). Both manifests are validated against real installed `.codex-plugin/plugin.json` and `.claude-plugin/plugin.json` files (this account's own plugins, and public plugins such as Superpowers and OpenRig Core) for their field shapes; there is no vendored schema to check them against mechanically, so a live install is the final confirmation, same as PR #1 left it.

## Install with `by`

`by init plugin` installs Branchyard into the harnesses you use: for each one, the files its install manifest lists ([below](#install-per-harness)), embedded in `by` byte for byte so no checkout is needed. On a terminal it lists the harnesses, asks which (the ones found on `PATH` are chosen), shows every diff and asks before writing; `by init plugin ID...` or `--all-detected` skips the question. For Claude Code that is all the canonical skills and toolkit files, the commands, the two hooks and a status line, into `~/.claude`. To put the skills in a project's `.claude/skills` or another host's skills directory, or to load the whole plugin for one session, see [below](#install-anywhere-else-with-a-skill-directory) and `claude --plugin-dir`.

## Install per harness

Every harness Branchyard has hooks for has an install manifest, `plugins/branchyard/install/<id>.toml`: the programs that show it is installed (`binaries`, looked for on `PATH`), what it gets (`provides`), each packaged file and where it goes (`[[file]]`: `from` under `plugins/branchyard`, `to` under `~` or `{project}`, the repository root, and `how`), and what only you can do afterwards (`[[note]]`). [Harness hooks](harness-hooks.md) has a section per harness: what it gets, its own marketplace or extension command, caveats and a live check.

```sh
by init plugin                          # every harness, detected ones first, and the files each would get
by init plugin --json                   # the same as JSON (HarnessList in schema/setup.protocol.json)
by init plugin codex hermes             # the plan: every file it would write or merge, as a diff; then asks
by init plugin --all-detected --dry-run # the plan for every harness on PATH; writes nothing
by init plugin cursor --apply           # writes without asking; --force also replaces differing written files
```

The plan is an ordinary `by init` plan (also `by init plugin --json --next` with `{"harnesses": [...]}`; a `host` answer from the retired skills-only interview is ignored), so a harness driving the setup skill gets the same diffs. Each harness gets a line counting its skills, commands and hooks, naming the status line when installed and the settings files that receive them, and a diff names a file under your home relative to it (`+++ b/.claude/settings.json`). The plan ends with the command that writes it (`by init plugin --all-detected --apply`); after `--apply` it ends with `by setup`. With no harness named and no answers, `--dry-run` and `--apply` refuse (exit 2): ``name harness IDs or --all-detected; `by init plugin` lists them``. On a terminal it asks before writing, and asks separately before replacing a file that exists with different content; elsewhere it prints the plan and writes nothing until `--apply`. The notes are printed with the plan and again after writing.

- **`write`** writes the packaged file, or every file under a packaged directory, as it is (0755 when it starts with `#!`). A file you have that differs is shown as a diff and replaced only with your consent (`--force`, or yes to the question).
- **`unless_set = "KEY"`** (`merge-json` at the top level) leaves a fragment out when your file already sets that key, instead of refusing the install for the clash, and the plan's notes say what to set to take Branchyard's instead. Claude Code's `statusLine` is merged this way: added when you have none, kept when you do (``kept the "statusLine" already in ~/.claude/settings.json. To use Branchyard's instead, set "statusLine": {...}``).
- **`merge-json`, `merge-toml`, `merge-yaml`** merge a packaged fragment, in the target's own format, into your file (under the dotted key `at`, or the top level): objects merge key by key, deeply; a list gains the entries it does not already hold, equal by value; nothing is removed or replaced. A key that holds another kind of value, or another scalar, is refused with its name, and then nothing at all is written. Run twice, the second run changes nothing. A merge keeps the file's mode, and writes through a symbolic link rather than over it.
- JSON is re-printed with the file's own indentation and key order; JSON with comments (JSONC) is refused. TOML is merged with `toml_edit`, so comments and formatting stay. YAML has no library in the build: `merge-yaml` reads block mappings and sequences by indentation and only inserts lines, so every existing line and comment stays; sequence items are compared by their text. It refuses tabs, several documents, flow collections (`{}`, `[]`) or a value with an anchor, alias or tag where it would have to enter them; a tag inside an item it copies (DeepSeek Harness's `!!js`) is kept as it is.

The plugin directory reaches an installed `by` without a checkout: `crates/branchyard-setup/build.rs` embeds every text file under `plugins/branchyard` with `include_str!` when `by` is built (a generated list, re-run when anything under the directory changes), so `by init plugin` writes exactly the bytes the plugin archive carries. The Docker images copy all of `plugins/branchyard` for that reason ([deploy](deploy.md)).

Tested hermetically (`crates/branchyard-setup/tests/install.rs` and the `install` module's unit tests): a fixture harness that uses every `how`, against existing files with comments and their own entries; a second run changing nothing; a clash refusing the plan; a differing written file needing consent; every shipped manifest planning validly into an empty home and again idempotently. Not done: no harness has been installed into by a real run of `by init plugin` on a machine that has it; detection is only the program on `PATH` (a harness installed elsewhere, or an app without a command, is not detected; name it); `merge-yaml` reads only the block YAML described above; there is no uninstall.

## Install anywhere else with a skill directory

```sh
python3 plugins/branchyard/scripts/install_skill.py --destination /path/to/host/skills
python3 plugins/branchyard/scripts/install_skill.py --destination /path/to/host/skills --apply
python3 plugins/branchyard/scripts/install_skill.py --destination /path/to/host/skills --skill setup --apply
```

`--skill` picks `delegate` by default, `setup`, or an atomic skill in the packaged capability catalog. Atomic skills install their dependency closure. For example:

```sh
python3 plugins/branchyard/scripts/install_skill.py --destination /path/to/host/skills --skill parallel-deck --apply
```

That installs `parallel-deck`, `pptx` and `artifact-tools` as sibling directories, with the shared launcher and toolkit under `artifact-tools/scripts/`. The installer checks the complete graph, required payloads and every destination before writing any skill. A missing dependency, a cycle, a symlink or a differing dependency without `--force` refuses the whole preflight. Every source file is read before installation; Python bytecode and `__pycache__` directories are ignored in both source and installed parity checks.

The direct host manifests use their native user skill directories: [Claude Code](https://code.claude.com/docs/en/skills) uses `~/.claude/skills`; [Codex](https://learn.chatgpt.com/docs/build-skills) uses `~/.agents/skills`; [Antigravity CLI](https://antigravity.google/docs/skills) documents `~/.gemini/antigravity-cli/skills`. Antigravity App/IDE documents a different global path, `~/.gemini/config/skills`; this manifest targets the CLI. Path selection is documented evidence. The archive tests exercise hermetic installation and the installed toolkit outside the checkout. Not done: live model discovery in these native host paths.

The installer previews by default; `--apply` copies. It targets one explicit host skill directory and never discovers or edits global host configuration. Run it once per host to avoid duplicated instructions from more than one copy.

If the entire selected closure already exists at the destination with the exact canonical bytes, `--apply` is a no-op (`{"applied": false, "unchanged": true}`). If any skill in the closure has different content, the installer refuses (`destination_exists`, exit 2) unless `--force` is also given, which replaces the differing directories. Payloads are staged before replacement; an existing forced directory remains available until its staged replacement can be renamed into place. It never silently overwrites someone else's edits.

## Archives

```sh
python3 tools/package.py --output dist
```

Produces four ZIP archives in `dist/`, each named `branchyard-<kind>-<version>.zip`:

| Archive | Contents |
|---|---|
| `branchyard-plugin-<version>.zip` | `plugins/branchyard/`, including every canonical skill, the shared artifact toolkit, the three slash commands, the hooks, both manifests, `LICENSE`, and a `MANIFEST.sha256.json` of every file's digest |
| `branchyard-skill-<version>.zip` | The standalone delegate skill alone, `plugins/branchyard/skills/delegate/`, for a host with no plugin loader |
| `branchyard-setup-skill-<version>.zip` | The standalone setup skill alone, `plugins/branchyard/skills/setup/` |
| `branchyard-sdk-<version>.zip` | `sdk/python/`: the Python modules, `branchyard.py` wrapping `by --json` for code inside a harness and `branchyard_client.py` speaking to `by serve` for a service outside, and `pyproject.toml`, so `pip install ./branchyard-sdk` installs both from the extracted archive ([SDKs](sdk.md#install)) |

Building refuses to proceed if a plugin manifest's `name` or `version`, or `sdk/python/pyproject.toml`'s `version` (in PEP 440's spelling), does not match the workspace's own `Cargo.toml` version, or any other field [`tools/release_version.py`](#the-version) checks disagrees, or if `.claude-plugin/marketplace.json` does not list the plugin once, at `./plugins/branchyard`, at that version, so a stale manifest cannot ship silently. Build leftovers in a source root (`__pycache__`, `*.pyc`, and the `build/` and `*.egg-info` that `pip install ./sdk/python` writes there) are never packaged.

**Reproducible.** Every archive is deterministic: inputs are read in sorted path order, every entry gets a fixed timestamp (`1980-01-01T00:00:00`, ZIP's minimum) and mode (`0644`), and compression is deflate at a fixed level. Two builds from the same source tree are byte-for-byte identical; `tests/test_distribution.py` checks this, along with every manifest entry's digest.

**Tested outside the checkout.** An editable or source install can hide a missing release input (a file that exists in the working tree but was never added to `tools/package.py`'s source roots). `tests/test_distribution.py` extracts the plugin, skill and SDK archives into a temporary directory outside the repository, then:

- Runs the shipped `install_skill.py` from the extracted plugin, checking the preview/apply/no-op/refuse/`--force` behavior above and that the installed skill's bytes match the canonical source.
- Imports the extracted SDK module and calls it against a stub `by`, checking it runs standalone (no `sys.path` trick back into the checkout) and its bytes match the canonical source.
- Builds the extracted SDK's source distribution with setuptools, as `pip install` does first, and imports both modules from it; checks `pyproject.toml` names every module at the workspace version with no dependencies. The build is skipped where setuptools 77 or later (the first to read an SPDX `license`) is not installed; the release workflow builds the sdist and the wheel on every run.
- Compares the plugin archive's bundled skills against the standalone skill archives: each holds the same canonical bytes, `plugins/branchyard/skills/<name>/` inside one and the top level of the other; and checks the plugin ships its three commands and `hooks/hooks.json`.
- Checks the marketplace lists the plugin directory, every command has a `description`, the manifest names no hooks file, and each hook runs `by hook` only when `by` is installed (run with an empty `PATH`, it prints nothing and exits 0).
- Installs the setup skill with `--skill setup` and the atomic dependency closure from the extracted archive; runs the installed toolkit away from the checkout, checks cache-tolerant reinstall, and proves a dependency conflict refuses before other skills are written.

Run it directly, or as part of `.github/workflows/check.yml`:

```sh
python3 tests/test_distribution.py
```

## Prebuilt binaries

The release pipeline uses [dist](https://github.com/axodotdev/cargo-dist), pinned
in [`dist-workspace.toml`](../dist-workspace.toml). It builds the portable CLI,
server and bridge as separate archives and generates their platform-aware shell
installers. The supported portable targets are declared in that file. Each
archive includes the applicable license files and third-party notices; the CLI
archive also carries the cloud helpers and bridge image resources. Those helpers
are embedded in the executable for setup outside a checkout.

A separate [`branchyard-runtime` package](../packaging/runtime/dist.toml) builds
all three binaries with Microsandbox and PostgreSQL support on its declared
Linux GNU targets. Choose it with:

```sh
curl -fsSL https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh | sh -s -- --runtime --setup
```

The native runtime is built on Ubuntu 24.04 and needs compatible glibc and
`libcap-ng0`, plus the pinned `msb` runtime and working KVM access for actual VM
execution. The guided checks distinguish a feature-enabled build from a host
that can run a VM. The portable build supports local processes, recipes and
Substrate without the Microsandbox compile-time feature. Installing a shared
library cannot add that feature to a portable executable.

The agy worker image is published separately by the release's image job. Its
immutable digest is recorded in `runtime-images.json`; cloud setup uses that
record instead of asking a developer to build an image. A model task remains a
separate action from installation and smoke checks.

### Installer behavior

The Branchyard bootstrap selects the release, downloads its generated
installers, checks that they all name the same release, and delegates platform
selection and archive installation to dist. The first generated installer pins
an unversioned install to one release before the remaining downloads begin.
`--prefix` accepts an absolute writable directory, including spaces;
`--base-url` supports an HTTPS mirror, a `file://` directory or a local directory.
`--dry-run` downloads and writes nothing. Plain HTTP is refused. Setup failures
return their exit status, and repeating the same command can finish a partial
installation.

The release publishes dist's `sha256.sum` and an additional `SHA256SUMS` for the
entrypoint assets and built archives. Build provenance is attached to the
platform artifacts. For a manual check, use `gh attestation verify` with the
archive and `--repo vamsiramakrishnan/branchyard`.

### Release maintenance

Use the pinned dist tool, then run:

```sh
python3 tools/release_workflow.py
python3 tools/release_workflow.py --check
```

The first command regenerates dist's CI and adds the required package-smoke
publication gate. `allow-dirty = ["ci"]` records this explicit CI customization;
installer and binary packaging remain owned by dist. Native build caches are
retained after a failed external attestation. A failed attestation gets one
required retry; a second failure prevents publication. `release-smoke.yml` installs
actual artifacts on matching native runners, checks reinstall, setup,
readiness and checksum refusal, and calls no model. Publication depends on its
successful result. The image job runs executable probes, also without a model.

macOS ARM builds and installation checks use `macos-15`; Intel checks use
`macos-15-intel`. Keep the build mapping in `dist-workspace.toml` and the native
smoke matrix aligned when a runner image retires. GitHub's
[macOS 14 retirement schedule](https://github.com/actions/runner-images/issues/13518)
includes outages before removal, so a queued or cancelled job is not packaging
validation.

### Homebrew

`tools/homebrew_formula.py` renders the formula template from actual dist archive
checksums. The release attaches `branchyard.rb`; a tap can install that formula.
A tap is not created or published automatically by the binary release.

### Verified here

Hermetic verification on 28 October 2026 runs `tests/test_distribution.py` against
local release fixtures: binary installation, latest-release pinning, a prefix
with spaces, reinstall, tampered-archive refusal, explicit headless setup,
setup-error propagation, dry-run and generated release dependencies. It also
checks packaged license freshness and Homebrew rendering. Not done: these
fixture checks alone do not establish successful native release builds or public
installation; the release workflow and its post-publication record provide that
evidence.

## The version

`tools/release_version.py` checks workspace crate versions, plugin manifests, the
Python SDK's PEP 440 version, and the generic native runtime package together.
`python3 tools/release_version.py --set VERSION` updates these fields as one
operation. `--tag vVERSION` verifies that a tag names the exact workspace version.
The release plan runs this check before building any artifact.

## First release

Use the Release workflow's manual run on a branch to rehearse the complete
build, installation and SDK packaging before tagging. A rehearsal creates a
`release` run artifact containing the files a tag would draft. It creates no
GitHub release, image push or provenance attestation. Pull requests changing
release inputs run the same rehearsal.

After a successful rehearsal, push a matching `vVERSION` tag. The workflow
builds and checks the artifacts, publishes the tested worker image, and creates
a draft GitHub release. A manual run on a tag drafts only when its `draft` input
is enabled. Publish the reviewed draft after checking its records. A prerelease
such as `0.1.0-rc.1` does not become GitHub's latest stable release; install that
release with the explicit `--version` option until a stable release is published.
