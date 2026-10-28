# Adding a harness

A harness appears on eight surfaces. A contributor who adds one touches the files below, in this order, and `python3 tests/test_harness_identity.py` says which are still missing. This page is the map; [writing a driver](writing-a-driver.md) is the depth on the drive side, and [harness hooks](harness-hooks.md) on the inform side.

Decide first what the harness gets. **Drive** means `by run --harness ID` starts it (surfaces 1 to 4 and 8). **Inform** means a session a person starts in it hears what Branchyard's branches did, through its hooks (surfaces 1, 2 and 5 to 7). Most harnesses get both. Droid and Kiro get only inform; Goose and Kimi CLI get only drive.

| # | Surface | The files | What fails when it is missing |
|---|---|---|---|
| 1 | [Identity](#1-identity) | `crates/branchyard-controls/src/harness.rs` | the controls tests; every other surface names an ID that is not registered |
| 2 | [Catalog entry](#2-catalog-entry) | `catalog/harnesses.toml` (generated), `curated` in `crates/branchyard-controls/src/catalog.rs`; how its login is read: `catalog/logins.toml` and `crates/branchyard/tests/golden/logins/ID.toml` | `by harnesses` does not see the CLI; the controls catalog test; without a login table, the LOGIN cell knows only its key variables, and a table without a golden file fails `cargo test -p branchyard --test logins` |
| 3 | [Driver profile and fixtures](#3-driver-profile-and-fixtures) | `crates/branchyard-harness/src/profiles.rs`, `crates/branchyard-harness/tests/` | `by run --harness ID` says the harness is not drivable |
| 4 | [The integration matrix](#4-the-integration-matrix) | `HARNESSES` `.target()`, `docs/harness-integration.md`, the count in `every_integration_target_is_registered_once` | the controls and profile tests |
| 5 | [Install manifest and plugin files](#5-install-manifest-and-plugin-files) | `plugins/branchyard/install/ID.toml`, `plugins/branchyard/harnesses/ID/` | `by init plugin` does not offer it; the cross-check test |
| 6 | [Hook dialect and fixtures](#6-hook-dialect-and-fixtures) | `DIALECTS` in `crates/branchyard-cli/src/hook_cmd.rs`, `crates/branchyard-cli/tests/fixtures/hooks/ID/` | `by hook --harness ID` prints nothing and a note; the cross-check test |
| 7 | [The hooks document](#7-the-hooks-document) | `docs/harness-hooks.md` | the cross-check test |
| 8 | [Compatibility and qualification](#8-compatibility-and-qualification) | `docs/compatibility.md` (generated), `docs/qualification/PROFILE.json`, `REPORTS` in `crates/branchyard/src/harness.rs` | the matrix says "not qualified" (true until you run it) |

## 1. Identity

Every other list names a harness by one ID, registered once in `HARNESSES` in `crates/branchyard-controls/src/harness.rs`. Add the entry with the names the vendored sources use for it (`emdash`, `orca`, `agentapi`, `coder_module`, `scion`, `herdr_manifest`), if they have it. Those names are how the catalog is generated. `.target("Row name")` is for an integration target, and surface 4 covers it. The controls tests fail when a vendored source names something the registry does not map.

## 2. Catalog entry

`catalog/harnesses.toml` is generated from pinned upstream sources (emdash, Orca, agentapi, Coder) and begins "do not edit". Do not edit it. Register the harness (surface 1), then regenerate:

```sh
BRANCHYARD_BLESS=1 cargo test -p branchyard-controls catalog
```

and review the diff. Where the upstream sources are wrong or silent (a different executable, no login command, a key variable that is not listed), correct the entry in `curated` in `crates/branchyard-controls/src/catalog.rs`, which replaces the field after generation. That is the only hand edit to the generated file. `binaries` matter most: `by harnesses` finds the CLI by them, and the driver profile's executable must be one of them. Cursor's `agent` is there because its profile runs `agent`, and the vendored sources knew only `cursor-agent`.

### How its login is read

`by harnesses`, `by doctor`, `by setup` and the check before a run read a harness's login from a table in [`catalog/logins.toml`](../catalog/logins.toml), written by hand and keyed by the catalog ID. Without one, the catalog's `auth_env` alone decides: a key variable set is `key set (NAME)`, and nothing is `unknown`. Add what the CLI itself shows you, by name only, never a value:

```toml
[my-harness]
env = ["MY_TOKEN"]                       # key variables besides auth_env
files = [{ path = "$HOME/.my/auth.json", shown = "~/.my/auth.json" }]

[my-harness.status]                      # its own status command, if it has one
args = ["auth", "status"]
logged_out = ["not logged in"]           # phrases, checked first, ignoring case
logged_in = ["logged in"]

[[my-harness.probe]]                     # a look inside its configuration
kind = "grep"
file = "$HOME/.my/settings.json"
pattern = '"provider"[[:space:]]*:'
fact = "provider-set"
login = "~/.my/settings.json selects a provider (its key is not checked)"
```

The file's header lists every field and probe kind (`grep`, `flag`, `provider`, `providers`, `env-file`, `sqlite`), and [the login rules](harness-lifecycle.md#where-the-rules-live) give the order they are consulted in. Then add `crates/branchyard/tests/golden/logins/ID.toml`, one `[[case]]` per state you checked against the CLI: the fake home's files, the variables (made-up values), the status command's output, and the exact lines and LOGIN cell detection must give. Copy a neighbour (`pi.toml` is the shortest) and run:

```sh
cargo test -p branchyard --test logins
```

It runs the real detection script against each case and prints every line that differs. Write in the table's comment which version of the CLI you checked, as the other tables do. A login the probe kinds cannot express is a new kind in `crates/branchyard/src/inventory/logins.rs`, with cases of its own, not a branch for one harness.

## 3. Driver profile and fixtures

Add the profile to `PROFILES` in `crates/branchyard-harness/src/profiles.rs`, record a fixture of the real exchange, and pass the conformance kit. [Writing a driver](writing-a-driver.md) covers all of it: the sans-IO model, the fixture format and redaction, `tests/conformance.rs`, and `acp(id, harness, command)` for a harness that speaks ACP, which is most of them. Set `checked_against` only to a version a fixture or schema came from.

A profile's first element of `command` must be in the catalog's `binaries` for the harness (surface 2). The one exception is an ACP adapter that is its own package, such as `claude-agent-acp`, which `by harnesses` cannot see; it is recorded in `tests/test_harness_identity.py`.

## 4. The integration matrix

A harness Branchyard drives is an integration target. Add `.target("Row name")` to its `HARNESSES` entry, and a row with the same name, in the same order, to the table under "Sixteen initial integration targets" in [harness integration](harness-integration.md). `every_integration_target_is_registered_once` in `crates/branchyard-controls/src/harness.rs` compares the two and asserts the count (16 today): raise it. A target with a profile is implemented; one without is listed in `NOT_IMPLEMENTED` in `profiles.rs` with the reason, never both. For an ACP profile, its command appears in backticks in the matrix (`acp_commands_match_the_integration_matrix`).

## 5. Install manifest and plugin files

For a harness that can run hooks, `plugins/branchyard/install/ID.toml` says how `by init plugin` detects and equips it: `binaries` (every one in the catalog entry, and the profile's), `provides`, each packaged `[[file]]` (`from` under `plugins/branchyard`, `to` under `~` or `{project}`, `how` one of `write`, `merge-json`, `merge-toml`, `merge-yaml`), and a `[[note]]` for each step only the person can do (trust the hooks, restart, approve). The format and the merge rules are in [distribution](distribution.md#install-per-harness).

The files it installs live in `plugins/branchyard/harnesses/ID/` (Oh My Pi installs Pi's extension, so it has none of its own). The whole of `plugins/branchyard` is embedded in `by` when it is built. Write each hook as `command -v by >/dev/null 2>&1 && by hook --harness ID EVENT || true`, so a harness on a machine without `by` hears nothing and fails nothing. If the files are JSON or YAML configuration that a harness parses, add them to `tests/test_plugin_config.py`, which checks each parses, has the fields the harness needs, and is silent with an empty `PATH`.

A drivable harness with no manifest is recorded, with the reason, in `NO_MANIFEST` in `tests/test_harness_identity.py` (Goose and Kimi CLI).

## 6. Hook dialect and fixtures

`by hook --harness ID` and `by statusline --harness ID` read the harness's stdin JSON and answer in its shape. Add a row to `DIALECTS` in `crates/branchyard-cli/src/hook_cmd.rs`: where the session id and the working directory are, the status line's directory, the reply shape, and the events the harness drops. Then record one payload and its exact expected output per event, in `crates/branchyard-cli/tests/fixtures/hooks/ID/` (`session-start.json` and `.out`, `user-prompt-submit.json` and `.out`, `statusline.json` and `.out`). `crates/branchyard-cli/tests/harness_hooks.rs` runs every directory. Record the payloads from the real CLI where you can, and say in the document (surface 7) where you could not.

A plugin written in code (Pi, Oh My Pi, Amp, OpenCode) calls `by hook --harness generic` and needs no row.

## 7. The hooks document

[Harness hooks](harness-hooks.md) gets a row in its `--harness` table and a `## Name` section, named for the harness (the catalog's name, except Cursor and Kiro, which drop its suffix; `SECTION` in the cross-check test records them), in the pattern of its neighbours: **What you get**, **Install**, **Caveats**, **Try it**. A **Try it** step is a command a person can run, and the section ends with the version it was run on, or an honest "Not done:" for what was not. Add the harness to the list of implemented drivers in [harness integration](harness-integration.md#implemented-drivers) if it is driven.

## 8. Compatibility and qualification

Regenerate the matrix, which is derived from the registry and the profiles:

```sh
cargo run -p branchyard-harness --example compat_matrix > docs/compatibility.md
```

A profile is "not qualified" until `branchyard-qualify` has run its nine scenarios against the real binary. When you have, keep the report as `docs/qualification/PROFILE.json`, named for the profile, add `"date"` by hand, add the profile to `REPORTS` in `crates/branchyard/src/harness.rs` (a test compares that list with the directory), describe the findings on the [qualification page](qualification/README.md), and regenerate the matrix. The steps and the flags are in [writing a driver](writing-a-driver.md#qualify-live). A failing report is evidence too.

## The cross-check test

```sh
python3 tests/test_harness_identity.py
```

reads the catalog, the profiles, the install manifests, the dialects, their fixtures and `docs/harness-hooks.md`, and fails with the harness and the surface that is missing. It checks that:

- every profile names a catalog harness and launches one of its `binaries`;
- every install manifest is named for a catalog harness, detects all of its `binaries` and the profile's, and installs files that exist;
- every drivable harness has a manifest, or a recorded reason in `NO_MANIFEST`, and every manifest without a driver is a recorded one (`INFORM_ONLY`);
- every manifest has a section in the hooks document, a dialect with recorded payloads, or is a plugin written in code that uses the generic one.

The places the lists legitimately differ are written at the top of that file with their reasons, and a record that has stopped being true fails the test.

Not done: nothing checks that a hook installed by `by init plugin` is trusted and running in the harness; that stays a manual step in each section of the hooks document. The lists are kept in step by tests, not by construction: a harness's identity, catalog entry, profile, install manifest and dialect are still separate records, not one.

Before sending the change, run what [writing a driver](writing-a-driver.md#before-sending-the-change) lists, and `python3 tools/check.py --fast`.
