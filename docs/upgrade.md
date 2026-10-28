# Upgrading

What to do when a newer Branchyard arrives, and the one step you cannot take back: a repository's store is upgraded in place, and an older `by` cannot read it afterwards. Back it up first if you may need to go back.

## Before you upgrade

1. Let running branches finish or stop them (`by ls`, `by cancel NAME`). A turn belongs to the engine process that started it; stopping first leaves none to be recovered.
2. Back up the store if you might return to the older `by`. It is one SQLite file in WAL mode, `.branchyard/state.db`. With no `by` command running, copy it with any `-wal` and `-shm` files beside it, or use `sqlite3 .branchyard/state.db ".backup state.db.bak"`. The work itself is in git (the `by/NAME` branches and the `refs/branchyard/` checkpoints), so a clone or a git backup of the repository covers that. The user-level files are in `~/.config/branchyard/` (your `config.toml`, workspace trust records, the harness inventory).
3. Note how you built `by`. A rebuild without the same `--features` (`postgres`, `microsandbox`) drops them.

## The binary

For a binary installation, rerun the installer with `--no-setup` to retain project configuration:

```sh
curl -fsSL https://github.com/vamsiramakrishnan/branchyard/releases/latest/download/install.sh | sh -s -- --no-setup
```

For a source installation, rebuild from the updated checkout:

```sh
cd branchyard
git pull
cargo install --locked --force --path crates/branchyard-cli    # add --features postgres,microsandbox if you had them
by --version
```

`--force` replaces a source installation even when its version number is unchanged. For a binary installation, rerun the single-command installer with `--no-setup` to update the binaries and retain project configuration. Add `--version X.Y.Z` to pin a release and `--runtime` to retain Microsandbox build support. See [distribution](distribution.md#install-by) for the installer and platform requirements.

Between source releases, `by --version` keeps the workspace version, so `git log -1` in the clone identifies the exact source revision.

## The store

Frozen handoffs add durable message metadata. Stores from before handoffs retain ordinary message behavior after migration.

The first `by` command that opens a repository after an upgrade migrates `.branchyard/state.db` to the schema the new binary reads, in one step. Nothing asks. `by doctor` warns before that happens, in the `repository.schema` row:

```text
schema 1; this binary reads 3 and upgrades the store in place the first time any `by` command opens it, after which an older `by` cannot read it
```

and the `To do:` list under the report adds `back up .branchyard/state.db if you may go back to an older by, then run any by command`. Once it has happened the row reads `schema 3, the version this binary reads`. The other direction fails: a store written by a newer `by` gives `schema N was written by a newer by; this binary reads M`, and the fix is the newer `by` (or the backup).

A server (`by serve`, `by worker`) migrates its own store the same way when the new binary starts, on SQLite or PostgreSQL. Back the database up first (`pg_dump` for PostgreSQL), stop every old server and worker on it, and start the new ones. Servers and workers sharing one database should run the same build.

## Plugins and hooks

The skills, commands and hook lines `by init plugin` writes are built into `by`, so they change when `by` does. Files already written into a harness's configuration do not change on their own. After upgrading `by`, run the install again:

```sh
by init plugin --all-detected --dry-run    # the plan for every harness on PATH; "unchanged" lines need nothing
by init plugin --all-detected --apply      # write it; --force also replaces written files that differ
```

`by setup` does the same for the harnesses it finds and, run twice, changes nothing the second time. Each file is shown as a diff first. A merge into a harness's own settings (JSON, TOML or YAML) only adds what is missing and removes nothing, so if a newer `by` words a hook line differently, the old line stays beside the new one: delete the old line by hand, or the hook runs twice. Harnesses that install the package themselves update it their own way: `gemini extensions update branchyard`, Claude Code's `/plugin` commands, Codex's `plugin` commands; see [harness hooks](harness-hooks.md). Codex asks again to trust a hook that changed.

## Harness CLIs

Branchyard does not pin the harnesses you run. `by harnesses` lists each one's version and login, and `by harnesses update ID [--version X] --yes` installs it again at a newer or a pinned version, when `[harnesses] install` in your user file allows it, and checks the result. A harness newer than the version recorded in [compatibility](compatibility.md) still runs; `by doctor` shows `qualified 9/9 on 2.1.283` beside the version you have, so you can see that the newer one has not been qualified. `by harnesses log` lists the installs and updates Branchyard made.

## Check it worked

```sh
by --version
by doctor                # repository.schema reads "the version this binary reads"; harnesses.installed lists versions
by harnesses
```

The schema warning above is an illustrative template. Hermetic backend conformance tests cover older-store migration and ordinary-message defaults for SQLite and PostgreSQL. Not done: a live upgrade with mixed old and new server binaries. The hook-duplication warning follows from the merge rules in [distribution](distribution.md) and was not seen across a real version change. `cargo install` was run with `--debug --offline` into a scratch root, not in release mode. The advice for servers and workers (stop the old ones, run one build, back up PostgreSQL) was not run.
