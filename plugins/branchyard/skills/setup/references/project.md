# Topic: project

Writes `branchyard.toml` at the repository root (scope `project`) or the
user's `~/.config/branchyard/config.toml` (scope `user`). The project file
overrides the user file key by key; `BRANCHYARD_REMOTE`,
`BRANCHYARD_TOKEN_FILE`, `BRANCHYARD_REPO` and `BRANCHYARD_CA_FILE`
override both; flags override everything.

- Re-running keeps keys the interview does not ask about (`[mcp]`,
  `auth`, `instructions`, `[serve]`) and uses the file's current values as
  defaults. Comments are rewritten; the diff shows it.
- Defaults apply to new branches (`by run`, `by fan`); `permissions` also
  to `send`, `fork`, `reincarnate` and `spawn`. Inside a harness running
  on a branch (`BRANCHYARD_BRANCH` set) the file is not read at all.
- Secrets apply only to isolated or sandboxed branches: they need a
  private home. Isolation scrubs the harness's login, so an isolated
  harness usually needs one secret for its credentials.
- Suggest `by init project --json --next` when a user repeats the same
  flags on every `by run`.

After applying: `by config validate`, then `by config show` to see each
value with where it came from.
