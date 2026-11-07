# Delegate reference: Dependencies and graph, GitHub issues and rigs

Read when: one child needs another's work (--depends-on, by graph apply), the work is a GitHub issue, or you fill a seat in a rig.

## From a GitHub issue

If the work is a GitHub issue and `gh` works in your shell, `by spawn --issue 42`
(or an issue URL) makes the issue the child's prompt, under a header naming it,
and names the child `issue-42-<slug>`; a prompt you add is appended as extra
instructions. Pull requests are not yours to open: `by pr` pushes with the
user's credentials and refuses to run inside a harness. Integrate the child;
whoever started your branch opens the pull request.

## When one child needs another's work

Give it `--depends-on`: it is created waiting and starts by itself once the
other has finished (`--after integrated`: once you integrated it, so it
starts from that work). If the other fails, it is `blocked` and never runs.

```sh
by spawn "Add the schema migration" --name schema
by spawn "Use the new column in the API" --name api --depends-on schema --after integrated
by graph show           # your children, what each waits for, and the revision
```

`by graph apply --edits '[...]' --expected-revision N` creates several
children and dependencies at once, all or nothing; if it says
`stale_revision`, run `by graph show` and propose again. Each edit is an
object tagged by `kind`:

```sh
by graph apply --expected-revision 3 --edits '[
  {"kind": "spawn", "name": "schema", "prompt": "Add the migration"},
  {"kind": "spawn", "name": "api", "prompt": "Use the column", "depends_on": ["schema"]},
  {"kind": "add_dependency", "dependent": "docs", "prerequisite": "api"}]'
```

`by graph apply --help` lists every field.

## In a rig

If your instructions say you fill a seat in a rig, or `by inspect` shows a
`seat`, you spawn only by seat, and only the seats listed in its `seats`:

```sh
by spawn --seat implementer "Port the tokenizer to the new API; run its tests"
by spawn --seat reviewer "Review the tokenizer candidate on by/parser-implementer" --wait
```

The seat sets the child's harness, budget, check, instructions, owned paths,
and result contract. You may narrow its ownership or budget, never widen them.
A result override must equal the seat's contract. In Python, pass `seat=`;
the MCP `spawn` tool takes `seat`.
