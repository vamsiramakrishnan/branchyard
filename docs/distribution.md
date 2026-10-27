# Distribution

Two canonical skills: [`plugins/branchyard/skills/setup`](../plugins/branchyard/skills/setup/SKILL.md) teaches a harness to set Branchyard up by interviewing the user through `by init --json` ([setup](setup.md)), and [`plugins/branchyard/skills/delegate`](../plugins/branchyard/skills/delegate/SKILL.md) teaches it to use `by`'s delegation commands (`docs/delegation.md`). The Claude plugin also has three slash commands, `/branchyard:setup` ([`commands/setup.md`](../plugins/branchyard/commands/setup.md)), which starts the setup skill, `/branchyard:status` and `/branchyard:wait`, and two hooks ([`hooks/hooks.json`](../plugins/branchyard/hooks/hooks.json)) that tell a session when its branches finish ([harness integration](harness-integration.md#claude-code)). Two plugin manifests, [`.claude-plugin/plugin.json`](../plugins/branchyard/.claude-plugin/plugin.json) and [`.codex-plugin/plugin.json`](../plugins/branchyard/.codex-plugin/plugin.json), point at the same `./skills/` directory, so Claude Code and Codex load an identical skill; a host with neither plugin loader installs an exact standalone copy with the shipped installer. The skills call the `by` CLI by name; [build it from source](#install-by) (no prebuilt release exists yet).

## Install `by`

Build from source. It is the only path until the first release.

```sh
git clone https://github.com/vamsiramakrishnan/branchyard && cd branchyard
cargo install --locked --path crates/branchyard-cli    # installs `by` into ~/.cargo/bin
by --version
```

`rust-toolchain.toml` pins Rust 1.94.0. Prebuilt binaries, `install.sh`, the Python SDK's wheel and a Homebrew formula are [prepared for the first release and not published](#prebuilt-binaries); [First release](#first-release) is how a person with push rights makes it.

## Install into a harness

`by setup` does this for every harness it detects ([setup](setup.md#one-command)). By hand, one harness at a time, each row is what to run, what it writes and what only you can do next. Every `by init plugin ID` plans first: on a terminal it shows the diff and asks, and elsewhere it prints the plan and writes nothing until `--apply` (`--dry-run` never writes); [harness hooks](harness-hooks.md) has a section per harness, and [below](#install-per-harness) is how the manifests work. Where a row offers a plugin and `by init plugin`, install one, not both, or each session hears the news twice.

| Harness | Command | What lands | Next step |
|---|---|---|---|
| Claude Code | `by init plugin claude-code`, or `/plugin marketplace add vamsiramakrishnan/branchyard` then `/plugin install branchyard@branchyard` | The `setup` and `delegate` skills, three commands and the `SessionStart` and `UserPromptSubmit` hooks | Restart Claude Code; `/hooks` lists the two hooks. The status line is not written: add `"statusLine": {"type": "command", "command": "by statusline"}` yourself |
| Codex | `by init plugin codex`, or `codex plugin marketplace add vamsiramakrishnan/branchyard` then `codex plugin add branchyard@branchyard` | `by init plugin`: the same two hooks, merged into `~/.codex/hooks.json`, and no skills. The plugin: the `setup` and `delegate` skills and the hooks | **Trust the hooks.** Codex runs none until you do: start `codex` once and choose "Trust all and continue" at "Hooks need review", or run `/hooks` and press `t`. `codex exec` skips an untrusted hook silently; for an unattended run pass `--dangerously-bypass-hook-trust` |
| Gemini CLI | `by init plugin gemini-cli`, or `gemini extensions install <clone>/plugins/branchyard/harnesses/gemini-cli` | The `BeforeAgent` hook merged into `~/.gemini/settings.json`, and the commands `/branchyard:setup`, `/branchyard:status`, `/branchyard:wait` (the extension names them `/setup`, `/status`, `/wait`) | Hooks run only in a trusted folder; `gemini -p` needs `--skip-trust` or `GEMINI_CLI_TRUST_WORKSPACE=true`. `/hooks panel` lists `branchyard-before-agent` |
| Qwen Code | `by init plugin qwen-code` | The hooks and status line, merged into `~/.qwen/settings.json` | Nothing to enable. For the commands: `qwen extensions install <clone>/plugins/branchyard/harnesses/gemini-cli` |
| Antigravity | `by init plugin antigravity`, or `agy plugin install <clone>/plugins/branchyard/harnesses/antigravity` | Hooks in `~/.gemini/config/hooks.json` and the status line in `~/.gemini/antigravity-cli/settings.json` | None |
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

That gives the two skills, the `/branchyard:setup`, `/branchyard:status` and `/branchyard:wait` commands, and the `SessionStart` and `UserPromptSubmit` hooks, which run `by hook` when `by` is on `PATH` and do nothing otherwise; in a repository driven through a server (`[remote]` or `BRANCHYARD_REMOTE`) they ask that server, and a server's PostgreSQL store is read only by a `by` built with the `postgres` feature ([harness integration](harness-integration.md)). The status line is not part of a plugin (a plugin cannot set one); `by init plugin claude-code` adds it to your Claude Code settings when you have none ([below](#install-per-harness)), or add `"statusLine": {"type": "command", "command": "by statusline"}` yourself.

Or point Claude Code's plugin loader at a checkout or an extracted archive for one session:

```sh
claude --plugin-dir plugins/branchyard
```

## Install for Codex

`codex plugin marketplace add vamsiramakrishnan/branchyard` (or the path of a clone), then `codex plugin add branchyard@branchyard`. Codex reads [`.agents/plugins/marketplace.json`](../.agents/plugins/marketplace.json), which lists the packaged directory (`plugins/branchyard`, containing `.codex-plugin/plugin.json`). The plugin gives the skills and the two hooks; `by init plugin codex` gives the hooks alone, so use one or the other. Codex runs no hook until you trust it ([the table above](#install-into-a-harness), [harness hooks](harness-hooks.md#codex)). Both manifests are validated against real installed `.codex-plugin/plugin.json` and `.claude-plugin/plugin.json` files (this account's own plugins, and public plugins such as Superpowers and OpenRig Core) for their field shapes; there is no vendored schema to check them against mechanically, so a live install is the final confirmation, same as PR #1 left it.

## Install with `by`

`by init plugin` installs Branchyard into the harnesses you use: for each one, the files its install manifest lists ([below](#install-per-harness)), embedded in `by` byte for byte so no checkout is needed. On a terminal it lists the harnesses, asks which (the ones found on `PATH` are chosen), shows every diff and asks before writing; `by init plugin ID...` or `--all-detected` skips the question. For Claude Code that is the `setup` and `delegate` skills, the commands, the two hooks and a status line, into `~/.claude`. To put the skills in a project's `.claude/skills` or another host's skills directory, or to load the whole plugin for one session, see [below](#install-anywhere-else-with-a-skill-directory) and `claude --plugin-dir`.

## Install per harness

Every harness Branchyard has hooks for has an install manifest, `plugins/branchyard/install/<id>.toml`: the programs that show it is installed (`binaries`, looked for on `PATH`), what it gets (`provides`), each packaged file and where it goes (`[[file]]`: `from` under `plugins/branchyard`, `to` under `~` or `{project}`, the repository root, and `how`), and what only you can do afterwards (`[[note]]`). [Harness hooks](harness-hooks.md) has a section per harness: what it gets, its own marketplace or extension command, caveats and a live check.

```sh
by init plugin                          # every harness, detected ones first, and the files each would get
by init plugin --json                   # the same as JSON (HarnessList in schema/setup.protocol.json)
by init plugin codex hermes             # the plan: every file it would write or merge, as a diff; then asks
by init plugin --all-detected --dry-run # the plan for every harness on PATH; writes nothing
by init plugin cursor --apply           # writes without asking; --force also replaces differing written files
```

The plan is an ordinary `by init` plan (also `by init plugin --json --next` with `{"harnesses": [...]}`; a `host` answer from the retired skills-only interview is ignored), so a harness driving the setup skill gets the same diffs. Each harness gets a line of what it gets and where (`Claude Code: 2 skills, 3 commands, 2 hooks, status line in ~/.claude/settings.json`), and a diff names a file under your home relative to it (`+++ b/.claude/settings.json`). The plan ends with the command that writes it (`by init plugin --all-detected --apply`); after `--apply` it ends with `by setup`. With no harness named and no answers, `--dry-run` and `--apply` refuse (exit 2): ``name harness IDs or --all-detected; `by init plugin` lists them``. On a terminal it asks before writing, and asks separately before replacing a file that exists with different content; elsewhere it prints the plan and writes nothing until `--apply`. The notes are printed with the plan and again after writing.

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
| `branchyard-plugin-<version>.zip` | `plugins/branchyard/`, including both skills, the three slash commands, the hooks, both manifests, `LICENSE`, and a `MANIFEST.sha256.json` of every file's digest |
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
- Installs the setup skill with `--skill setup`.

Run it directly, or as part of `.github/workflows/check.yml`:

```sh
python3 tests/test_distribution.py
```

## Prebuilt binaries

> **Not published yet.** No tag or GitHub release exists, so there is nothing to download: [build from source](#install-by) until the first one. The workflow has never run on GitHub; the parts this machine can run, ran ([Verified here](#verified-here)). Its first full run is the dry run in [First release](#first-release).

`.github/workflows/release.yml` builds `by` and `branchyard-server` (with the `postgres` feature, as the container image does) for four targets, and packs each with `LICENSE`, `THIRD_PARTY.md`, `README.md` and the Orca and emdash license texts their ported code requires, as `branchyard-VERSION-TARGET.tar.gz` holding one directory of that name:

| Target | Runner | Linking |
|---|---|---|
| `x86_64-unknown-linux-musl` | `ubuntu-24.04` | static (musl-gcc) |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` | static (musl-gcc) |
| `aarch64-apple-darwin` | `macos-14` | system libraries only |
| `x86_64-apple-darwin` | `macos-14`, cross-compiled | system libraries only |

Beside the four archives a release carries the Python SDK's wheel and sdist (`python -m build`: `branchyard-0.1.0rc1-py3-none-any.whl` for `0.1.0-rc.1`), the plugin, skill and SDK archives of [Archives](#archives), `install.sh`, the [Homebrew formula](#homebrew) filled in for it, and `SHA256SUMS` over every one of them.

**When it runs, and what it may change.**

| Trigger | What the run does |
|---|---|
| a `v*` tag pushed | every job below, then attests every file and **drafts** a GitHub release for a person to publish: a pre-release when the version is `-alpha.N`, `-beta.N` or `-rc.N`, so publishing it does not make it the latest release |
| Run workflow (Actions, Release) on a branch | a dry run: the same builds, packaging and installs, kept as the run's artifacts; nothing is attested, no release is drafted, no image is pushed. On a `v*` tag with `draft` ticked it is the tag push again (to draft once more after deleting a draft) |
| a pull request that changes `release.yml`, `install.sh`, `packaging/`, `sdk/python/pyproject.toml`, `tools/package.py`, `tools/release_version.py` or `tools/homebrew_formula.py` | a dry run, without the image |

The jobs, in order:

| Job | What it does |
|---|---|
| `version` | Reads the version every file is named for from `Cargo.toml`, after [`tools/release_version.py`](#the-version) checks that every version field agrees and, on a tag, that the tag is `v` and that version; decides whether the run drafts |
| `build` (one per target) | Installs the pinned toolchain with the target, runs `cargo build --release --locked --offline --target …`, checks that a Linux binary is static (`file` says `static-pie linked`: Rust links musl executables static-pie) and that `by --version` prints `by VERSION`, packs the archive and its `.sha256`, then installs it with `install.sh` from a directory and runs the installed `by --version` and `by` outside a repository (the x86_64 macOS binary only where the Apple silicon runner can execute it) |
| `python` | Runs `tools/package.py` and `python -m build sdk/python`, installs the wheel into a fresh venv and imports both modules, checking the installed version |
| `assemble` | Gathers every file into one directory, checks each archive against its `.sha256`, writes `SHA256SUMS`, fills the Homebrew formula from it, runs `install.sh` against the directory as a person would, and uploads the lot as the run's `release` artifact; the release notes ([`packaging/release-notes.md.in`](../packaging/release-notes.md.in), filled in) and `SHA256SUMS` are the job's summary |
| `draft` | Only when the run drafts: attests `SHA256SUMS` and runs `gh release create TAG --draft --verify-tag` with the notes and every file. The only job with `contents: write` |
| `image` | The [image with harnesses](deploy.md#image-with-harnesses): built and smoke-tested (its `by --version` must be the release's) except on a pull request, whose paths leave `deploy/` out; pushed to GHCR only when the run drafts and the repository variable `PUBLISH_IMAGES` is `true` |

**Why musl works with this dependency set.** The default and `postgres` builds link no OpenSSL and no aws-lc: TLS is rustls on `ring` everywhere (the server's `reqwest` is built `rustls-no-provider`), and PostgreSQL is `postgres` without TLS. Their C code is `ring`'s, SQLite (`rusqlite`'s `bundled` feature) and BLAKE3's, which musl-gcc compiles; `protoc-bin-vendored` runs on the build host only. aws-lc appears only with the `microsandbox` feature (its SDK pulls `aws-lc-rs`), which needs `libcap-ng` and KVM anyway and is not in the release.

The archives, the wheel, the sdist and the zips each get a [build provenance attestation](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations) (`actions/attest-build-provenance`, pinned by commit like every action) in the job that built them, and so does `SHA256SUMS`, which also lists `install.sh` and `branchyard.rb`. Check one with `gh attestation verify branchyard-0.1.0-rc.1-x86_64-unknown-linux-musl.tar.gz --repo vamsiramakrishnan/branchyard`.

### The version

One version names everything: `[workspace.package] version` in `Cargo.toml`. Every other file that carries Branchyard's own version must carry the same one: the crates' path dependencies on one another and their `Cargo.lock` entries, each host's plugin manifest, both marketplaces, the Herdr plugin, and `sdk/python/pyproject.toml`, which spells it the PEP 440 way (`0.1.0-rc.1` is `0.1.0rc1`).

```sh
python3 tools/release_version.py                     # checks every field and prints 0.1.0-rc.1
python3 tools/release_version.py --tag v0.1.0-rc.1   # and that the tag names it, as the workflow does first
python3 tools/release_version.py --pep440            # 0.1.0rc1
python3 tools/release_version.py --set 0.1.0         # rewrites every field, Cargo.lock included
```

A release version is `X.Y.Z`, or `X.Y.Z-alpha.N`, `-beta.N` or `-rc.N`: the forms Cargo and Python can both say. A field that disagrees is named, with the command that fixes it. `tools/package.py` refuses to build archives while one disagrees, and `tests/test_distribution.py` runs the check on every `tools/check.py`, so a bump is one command and one commit.

### `install.sh`

```sh
curl -fsSLO https://github.com/vamsiramakrishnan/branchyard/releases/download/v0.1.0-rc.1/install.sh
less install.sh                                       # read it
sh install.sh --version 0.1.0-rc.1                    # ~/.local/bin/by and branchyard-server
sh install.sh --version 0.1.0-rc.1 --prefix /opt/branchyard
BRANCHYARD_INSTALL_BASE=./downloaded sh install.sh --version 0.1.0-rc.1   # from files you already have
```

POSIX `sh`. It picks the archive for this machine (`--target` overrides), fetches it and `SHA256SUMS` over `https` only (curl or wget) or from a local directory or `file://` URL (`--base-url`, or `BRANCHYARD_INSTALL_BASE`: how the workflow installs what it just built, and how a draft's files are checked before publishing), compares the archive's SHA-256 with the listed one and installs nothing on a mismatch, then copies the binaries with mode 0755 (through a temporary name and a rename) and the license files into `PREFIX/share/branchyard`. When `PREFIX/bin` is not on `PATH` it prints the `export PATH=…` line to add; when another `by` comes first on `PATH` (a `cargo install` in `~/.cargo/bin`, say) it names it. It never calls `sudo` and is not meant to be piped into a shell: for a prefix you cannot write, run the file you read with the privileges that can.

### Homebrew

`packaging/homebrew/branchyard.rb.in` is a formula template for a tap of your own; `python3 tools/homebrew_formula.py --version 0.1.0-rc.1 --sums SHA256SUMS > branchyard.rb` fills its URLs and checksums from a release's `SHA256SUMS` (and fails if one is missing), and the release workflow attaches the filled `branchyard.rb`. The formula installs both binaries, shell completions (`by completions`) and the man page (`by man`); its test runs `by --version`, and `by recipe list` in a new repository. Nothing publishes it to a tap.

## First release

The first tag is `v0.1.0-rc.1`: a release candidate, so its draft is a pre-release and the repository has no "latest" release until a plain `X.Y.Z` is published. The workspace already carries the version (`python3 tools/release_version.py` prints `0.1.0-rc.1`). Nothing below has run; the person with push rights to `vamsiramakrishnan/branchyard` runs it.

1. **Rehearse on `main`.** Once the change that set the version is merged: Actions, Release, Run workflow, branch `main`, `draft` unticked (or `gh workflow run release.yml --ref main`, then `gh run watch`). Every job but `draft` runs. When it is green, download the `release` artifact (`gh run download RUN_ID --name release`) and read the file list and `release-notes.md`; the `assemble` job's summary shows the notes and `SHA256SUMS`. Fix what is red before tagging.

2. **Tag the merged commit and push the tag.**

   ```sh
   git switch main && git pull --ff-only
   python3 tools/release_version.py --tag v0.1.0-rc.1    # prints 0.1.0-rc.1, or names what disagrees
   git tag -a v0.1.0-rc.1 -m "Branchyard 0.1.0-rc.1" && git push origin v0.1.0-rc.1
   ```

   The push starts Release on the tag. When every job passes, Releases shows a draft `v0.1.0-rc.1` marked pre-release; only people with write access see it, and nothing is public.

3. **Check the draft.** A draft's files cannot be fetched from the public `releases/download/` URL until it is published, so take them with `gh`:

   ```sh
   gh release download v0.1.0-rc.1 --repo vamsiramakrishnan/branchyard --dir rc1
   (cd rc1 && sha256sum -c SHA256SUMS)                  # shasum -a 256 -c SHA256SUMS on macOS
   BRANCHYARD_INSTALL_BASE=rc1 sh rc1/install.sh --version 0.1.0-rc.1 --prefix "$PWD/rc1-prefix"
   rc1-prefix/bin/by --version                           # by 0.1.0-rc.1
   gh attestation verify rc1/branchyard-0.1.0-rc.1-x86_64-unknown-linux-musl.tar.gz --repo vamsiramakrishnan/branchyard
   ```

4. **Publish.** Open the draft (Releases, the `v0.1.0-rc.1` draft, the pencil icon), read the notes, leave "Set as a pre-release" ticked and click **Publish release**. From then on the `install.sh` lines in the notes work for anyone. To withdraw it instead, delete the draft and the tag (`git push origin :refs/tags/v0.1.0-rc.1`, then `git tag -d v0.1.0-rc.1`), fix, and tag again.

5. **After publishing**, in one change: say on this page, in the README and in [getting started](getting-started.md) that a release exists, and set the next version (`python3 tools/release_version.py --set 0.1.0-rc.2`, or `0.1.0`), so builds from `main` no longer claim to be the published one.

## Verified here

**27 October 2026, on this x86_64 Linux machine; no model, nothing pushed or published.** `python3 tools/release_version.py --set 0.1.0-rc.1` rewrote 18 files (23 `Cargo.lock` entries) and `cargo metadata --locked --offline` accepted the lock. With musl-tools 1.2.4 from apt and the musl target added by the workflow's own `rustup toolchain install 1.94.0 --profile minimal --target x86_64-unknown-linux-musl`, the workflow's build command (`cargo build --release --locked --offline --target x86_64-unknown-linux-musl -p branchyard-cli -p branchyard-server --features "$FEATURES"`, `CC_x86_64_unknown_linux_musl=musl-gcc`, symbols stripped) compiled 335 crates in 6 minutes 22 seconds on four shared cores, SQLite's C included this time: `by` is 60,224,000 bytes and `branchyard-server` 40,688,768, and `file` 5.45 calls both `ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), static-pie linked, … stripped` (`ldd`: statically linked). The workflow's first version grepped that line for `statically linked` and would have failed every Linux build; it now accepts `static-pie linked`. `by --version` printed `by 0.1.0-rc.1`. Then the workflow's own step scripts, cut from `release.yml` and run with bash, ran on those binaries: Check the binaries; Package (`branchyard-0.1.0-rc.1-x86_64-unknown-linux-musl.tar.gz`, 39,469,762 bytes, one directory holding `by`, `branchyard-server`, `LICENSE`, `THIRD_PARTY.md`, `README.md` and `licenses/`); Install the archive with `install.sh` (`installed by 0.1.0-rc.1 into …/runner/by/bin (sha256 2501d275…)`, the `export PATH=…` advice, and the status card outside a repository, `Branchyard: …/runner is not inside a git repository.`); the python job's Build with `build` 1.2.2.post1 (`branchyard-0.1.0rc1.tar.gz`, `branchyard-0.1.0rc1-py3-none-any.whl` and the four archives) and its install of the wheel into a fresh venv, which imported both modules and printed `0.1.0rc1`; and the assemble job's checksums, formula, install and notes steps, with stand-ins for the three archives this machine cannot build: 12 files in `SHA256SUMS`, every one `OK` under `sha256sum -c`. The installed `by` in a new repository printed `No recipes: add [recipes.NAME] to …` for `by recipe list`, which the formula's test had matched as lower-case `no recipes` (it now ignores case), and `by man` titled its page `by 0.1.0-rc.1`. Building the SDK warned that a `project.license` table is deprecated and stops building after 18 February 2027; it is now the SPDX `license = "Apache-2.0"`, with `setuptools>=77`. `tests/test_distribution.py` (48 tests) runs the version check, `install.sh` from `BRANCHYARD_INSTALL_BASE` and its `PATH` advice, the workflow's triggers and what a dry run may not do, and the same step scripts against stand-in binaries. The aarch64 and macOS matrix lines were checked by reading: the musl and `file` steps are conditioned on `musl`, the rustup step installs rustup where an image lacks it, `shasum` and BSD `sed` are what macOS has, and the x86_64 macOS binary is run only where the Apple silicon runner can execute it. On 1 October 2026 a glibc build for this host was 45.9 MB, 36.8 MB stripped. **Not done:** the workflow on GitHub (no push from here; its first run is the rehearsal in [First release](#first-release)), the aarch64 Linux and macOS builds, the attestations and `gh attestation verify`, a draft and its publishing, `docker build` of the image (no daemon), the formula under `brew`, and `/plugin marketplace add` against a live Claude Code.
