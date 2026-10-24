# Harness hooks

## Other harnesses (Goose and the rest)

A harness with no hook that can add context to a session (Goose documents plugin hooks but no way for one to inject context), or any harness not listed above, gets Branchyard two other ways.

- **MCP tools, on a branch.** When Branchyard itself runs the harness on a branch, the engine gives it the delegation tools as an MCP server (`by mcp --root ROOT --branch NAME`, projected per driver; [delegation](delegation.md#what-a-delegated-harness-gets), [its tools](delegation.md#mcp-tools)): `inbox`, `children`, `wait` and the rest answer what the hooks would tell. `by mcp` serves a branch, so a session a person started outside Branchyard does not get it.
- **An instruction line, anywhere.** [`plugins/branchyard/harnesses/generic/AGENTS.md`](../plugins/branchyard/harnesses/generic/AGENTS.md) holds one section per harness to copy into the repository's AGENTS.md (or whatever instructions file the harness reads). Its `## Goose and the rest` section asks the agent to run `by hook --harness generic user-prompt-submit` with the working directory before a task and before reporting back, and to act on the `context` it prints. All such sessions share the `default` session cursor, so news one session was shown is not repeated to another.

There is no install manifest for these: `by init plugin` does not edit an AGENTS.md, which is yours. Try it: in a repository Branchyard uses, `printf '{"cwd": "%s"}' "$PWD" | by hook --harness generic user-prompt-submit` prints `{"context": "..."}` the first time and nothing the second.

Not done: the instruction line relies on the model following it, which no test checks beyond the `generic` dialect's own; it has not been run in Goose or any other harness.
