# Distribution

Two canonical skills: [`plugins/branchyard/skills/setup`](../plugins/branchyard/skills/setup/SKILL.md) teaches a harness to set Branchyard up by interviewing the user through `by init --json` ([setup](setup.md)), and [`plugins/branchyard/skills/delegate`](../plugins/branchyard/skills/delegate/SKILL.md) teaches it to use `by`'s delegation commands (`docs/delegation.md`). The Claude plugin also has a `/branchyard:setup` command ([`commands/setup.md`](../plugins/branchyard/commands/setup.md)) that starts the setup skill. Two plugin manifests, [`.claude-plugin/plugin.json`](../plugins/branchyard/.claude-plugin/plugin.json) and [`.codex-plugin/plugin.json`](../plugins/branchyard/.codex-plugin/plugin.json), point at the same `./skills/` directory, so Claude Code and Codex load an identical skill; a host with neither plugin loader installs an exact standalone copy with the shipped installer. The skills call the `by` CLI by name; install it from a [prebuilt release](#prebuilt-binaries) or with `cargo install --locked --path crates/branchyard-cli`.

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

## Prebuilt binaries

`.github/workflows/release.yml` runs only when a `v*` tag is pushed. It builds `by` and `branchyard-server` (with the `postgres` feature, as the container image does) for four targets, packs each with `LICENSE`, `THIRD_PARTY.md`, `README.md` and the Orca and emdash license texts their ported code requires, and attaches them to a **draft** release for a person to publish:

| Target | Runner | Linking |
|---|---|---|
| `x86_64-unknown-linux-musl` | `ubuntu-24.04` | static (musl-gcc) |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` | static (musl-gcc) |
| `aarch64-apple-darwin` | `macos-14` | system libraries only |
| `x86_64-apple-darwin` | `macos-14`, cross-compiled | system libraries only |

**Why musl works with this dependency set.** The default and `postgres` builds link no OpenSSL and no aws-lc: TLS is rustls on `ring` everywhere (the server's `reqwest` is built `rustls-no-provider`), and PostgreSQL is `postgres` without TLS. Their C code is `ring`'s, SQLite (`rusqlite`'s `bundled` feature) and BLAKE3's, which musl-gcc compiles; `protoc-bin-vendored` runs on the build host only. aws-lc appears only with the `microsandbox` feature (its SDK pulls `aws-lc-rs`), which needs `libcap-ng` and KVM anyway and is not in the release. Each Linux build is checked with `file … | grep 'statically linked'`.

Each archive gets a `.sha256`, the release a `SHA256SUMS` of all four, and both are covered by [build provenance attestations](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations) (`actions/attest-build-provenance`, pinned by commit like every action). Check one with `gh attestation verify branchyard-0.1.0-x86_64-unknown-linux-musl.tar.gz --repo vamsiramakrishnan/branchyard`. The same workflow builds the [image with harnesses](deploy.md#image-with-harnesses).

### `install.sh`

```sh
curl -fsSLO https://github.com/vamsiramakrishnan/branchyard/releases/download/v0.1.0/install.sh
less install.sh                                  # read it
sh install.sh --version 0.1.0                    # ~/.local/bin/by and branchyard-server
sh install.sh --version 0.1.0 --prefix /opt/branchyard
```

POSIX `sh`. It picks the archive for this machine (`--target` overrides), downloads it and `SHA256SUMS` over `https` only (curl or wget; `file://` and a plain directory work too), compares the archive's SHA-256 with the listed one and installs nothing on a mismatch, then copies the binaries with mode 0755 (through a temporary name and a rename) and the license files into `PREFIX/share/branchyard`. It never calls `sudo` and is not meant to be piped into a shell: for a prefix you cannot write, run the file you read with the privileges that can.

### Homebrew

`packaging/homebrew/branchyard.rb.in` is a formula template for a tap of your own; `python3 tools/homebrew_formula.py --version 0.1.0 --sums SHA256SUMS > branchyard.rb` fills its URLs and checksums from a release's `SHA256SUMS` (and fails if one is missing). The formula installs both binaries, shell completions (`by completions`) and the man page (`by man`). Nothing publishes it.

### Verified here

On 1 October 2026, on x86_64 Linux: `cargo build --release -p branchyard-cli --locked --offline` built `by` for this host (glibc, not musl) in 5 minutes 40 seconds: 45,930,280 bytes, 36,830,896 stripped (the workflow strips), and `by --version` printed `by 0.0.1`; the same archive layout installed with `install.sh` from a `file://` directory ran. A musl build (`--target x86_64-unknown-linux-musl --features postgres`, the target added with rustup) compiled every Rust crate and `ring`, then failed only to link SQLite's C, which, with no musl C compiler here (`CC=gcc`, glibc headers), referenced glibc's `__memcpy_chk` and `stat64`: the reason the workflow installs `musl-tools` and sets `CC_<target>=musl-gcc`; `tests/test_distribution.py` runs `install.sh` against a `file://` release directory in a temporary prefix (installed, modes 0755, licenses copied, a reinstall replacing, a tampered `SHA256SUMS` installing nothing, plain `http` refused, `--dry-run`), checks `sh -n`, fills the Homebrew template, and checks the workflow's trigger (only a `v*` tag push), its commit-pinned actions, targets, checksums and attestations, parsing it as YAML where PyYAML is installed. **Not verified:** the workflow itself (it never ran), a complete musl build (no musl-gcc here) and macOS builds (no macOS), the attestations, and the formula under `brew`.
