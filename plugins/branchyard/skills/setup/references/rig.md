# Topic: rig

Writes a rig spec (default `rigs/<name>.toml`): a lead seat and, by
shape, an implementer (two may run at once), a reviewer that may not
edit files, or two implementers on different harnesses.

- Budgets are split so the children's limits times their instances fit
  in the lead's, which `by rig check` requires.
- A seat on Antigravity, Pi or Amp cannot route tool permission requests;
  `by rig run` then needs `--allow-unapproved-tools`. Prefer other
  harnesses unless the user insists.
- After applying: `by rig check <path>`, then
  `by rig run <path> "<task>"`. See docs/rigs.md for fields the
  interview does not ask about (pods, startup files, `escalates_to`).
