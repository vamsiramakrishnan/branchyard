# Console screenshots

The images in this directory show the browser console (`by serve --app --knowledge`) with demo data. A script produced every conversation, diff and table. The repository's fake agent ran behind a filter that plays a fixed story. No model ran. The repository, the customers and the numbers are invented. The branch views read `claude-code` because the server runs the scripted agent under Claude Code's ACP profile, `claude-code-acp`. Claude Code itself never ran.

| File | Shows |
|---|---|
| `console-branches.png`, `console-branches-dark.png` | A Rust library with branches in each lane: one interrupted branch under Needs you, two running, two ready, two merged under "Show 2 done". The ready branch `parser-fix` is open, with its prompt, the agent's reply, its five tool calls unfolded and Merge in the status strip. The Changes tab shows its diff. 1440 by 900 pixels. |
| `console-workspace.png`, `console-workspace-dark.png` | A knowledge project's workspace with two named sessions. The open session's conversation is scrolled to its latest turns, as the console opens it, and the Outputs tab previews a CSV table the session published. 1440 by 900 pixels. |
| `console-phone.png` | The same ready branch on a phone, in one column, with Merge at the top. 390 by 844 pixels at twice the density. |

## Regenerate them

Build `by` and the fake agent, then run the generator from the repository root:

```sh
cargo build --locked --offline -p branchyard-cli --bin by -p branchyard-runtime --bin fake-acp-agent
BRANCHYARD_PREVIEW_BWRAP=/usr/bin/bwrap \
BRANCHYARD_PREVIEW_PYTHON=/srv/branchyard/artifacts/bin/python \
PLAYWRIGHT="$(npm root -g)/playwright" \
python3 tools/readme_screenshots/generate.py --by target/debug/by --agent target/debug/fake-acp-agent
```

The two preview variables name the isolated converter that renders the table ([artifact previews](../../docs/artifact-previews.md)). Without them the generator stops before it starts a server. Playwright reads `PLAYWRIGHT_BROWSERS_PATH` from the environment as usual. `--out DIR` writes the images somewhere else, and `--work DIR` puts the temporary repository and server state somewhere other than the system's temporary directory.

The generator builds nothing and calls no model. It writes the story in [`story.py`](../../tools/readme_screenshots/story.py) to a new temporary directory, starts the server, plays the story through the API and signs a desktop and a phone browser in with the server token. [`shoot.js`](../../tools/readme_screenshots/shoot.js) only opens pages and clicks. It never changes the page, and it stops before a screenshot if the page text contains the temporary directory, your home directory, the machine's name or the token.

The generator replaces the images here only after it has taken all five. A failed run leaves them alone, keeps its temporary directory with the server's log, and prints where that directory is. `--keep` keeps it after a successful run too. The workspace images come out byte for byte the same on every run. The branch images change between runs in commit hashes and relative times.
