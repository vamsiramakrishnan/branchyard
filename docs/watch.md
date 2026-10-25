# Watching branches

`by watch` shows every branch as a tree, forks under their parents, with status, harness, current activity (the tool running, a pending permission request, the last line of the message), turns, cost and age. `by watch --once` prints it once:

```text
by watch · /src/app

BRANCH        HARNESS      STATUS      TURNS   COST  AGE  ACTIVITY
parser        claude-code  running         2  $0.41   3m  ▸ Bash · Running the parser tests
└ parser-alt  codex        ready           1  $0.12   1m  Rewrote the tokenizer loop
docs          gemini-cli   no changes      1      -   9m  The docs already cover this

3 branches, 1 running, $0.53 reported
```

On a terminal it is a live dashboard (ratatui): the tree with each status in its own color (an interrupted branch in black on yellow, since its work is stopped mid-turn), and beside or below it the selected branch's detail: its latest events, cost and tokens, unread inbox messages, children, prompt and the commands to run next. `j`/`k` or the arrows move, Enter focuses the selected branch's subtree, `/` filters by name, harness, status or prompt, `?` lists every key, and `q`, Esc or Ctrl-C exits and restores the terminal (as does a panic). Piped, it prints one line per change instead. It reads event logs incrementally, and works the same with `--remote`, where it follows the server's event stream.

It is also a cockpit: keys act on the selected branch by running the `by` command they name, with the same `--remote` and other global flags, and the footer lists the ones that apply to it.

| Key | Does | Runs |
|---|---|---|
| `s` | Send a follow-up prompt, typed in a box | `by send` in the background |
| `S` | Steer the running turn | `by send --steer` |
| `R` | Resume an interrupted branch in its own session, with one key | `by send` with a "continue where you left off" prompt |
| `x` | Cancel the running turn and its descendants', after a yes | `by cancel` |
| `m` | Merge, after a yes; a pane then shows the result of the check | `by merge` |
| `f` | Fork, with a prompt | `by fork` in the background |
| `d` / `l` | The candidate's diff, or the whole log (following new events), in a scrollable pane | `by diff`, `by log` |
| `y` / `Y` | Copy the branch's name, or its worktree's path, through the terminal (OSC 52) | |
| `p` / `P` | Check, push and open or update the pull request, after a yes; `P` then follows it, feeding CI failures and review comments back as turns | `by pr` (waited for, its output in a pane), `by pr --watch` in the background |
| `o` | Open the worktree in `$VISUAL` or `$EDITOR`; a terminal editor gets the screen until it exits, then the dashboard comes back | `by open` |
| `r` | Rewind to a checkpoint: type its number under the branch's checkpoint list, then confirm | `by rewind --to N --yes` |
| `c` | Compare the branch with its siblings (the rest of its `by fan`, or its parent's other children) in a pane | `by compare` |
| `t` | Try the branch's changes in this checkout, after a yes; `t` on the tried branch restores it | `by try`, `by try --off` |
| `v` | Review: the diff in `$VISUAL` or `$EDITOR`, which gets the terminal; comments written on `>>` lines go to the branch as one prompt when the editor closes | `by review --detach` |
| `b` / `K` | Open a port the branch listens on in a browser; stop its listening processes, after a yes (the detail pane lists them) | `by workspace browse`, `by workspace kill --yes` |

Commands that run a turn start detached (their output goes to `.branchyard/watch/` in the repository), so they carry on if the dashboard quits; a result or a refusal (such as `R` on a branch that is not interrupted) appears in the status line. With no terminal to ask on, those turns get the permissions `branchyard.toml` sets, and requests are otherwise denied. Remotely, an action that needs this machine (copying a worktree's path, and `p`, `P`, `o`, `r` and `t`, which act on this machine's repository, worktree or checkout) is refused with the reason. The detail pane shows the branch's checkpoint (`at 2 of 3`), its merge readiness once it has pull-request steps, and whether it is being tried. The keys come from one table, `crates/branchyard-cli/src/watch/actions.rs`, which also generates the `?` sheet.

`by watch` also tells you when a branch needs you or ends, and so does a waiting `by run`, `by fan`, `by send` or `by fork`: when a tool asks for permission, a branch asks or escalates a question, a turn stalls, or a branch fails, is interrupted, or finishes. Each event is said once, with a terminal bell and a desktop-notification escape, OSC 9 (iTerm2, WezTerm, kitty, Ghostty, Windows Terminal) or OSC 777 (foot, urxvt, VTE terminals such as GNOME Terminal), passed through tmux; it needs no D-Bus. History `by watch` reads when it starts is not news. `[notify] desktop = true` in `branchyard.toml` also runs `notify-send` (or `osascript` on macOS), `[notify] terminal` picks the escape (`auto`, `osc9`, `osc777`, `bell`, `none`), and `--no-notify` or `[notify] enabled = false` turns it off. Escapes go only to a terminal: `by watch`'s own, or a waiting command's stderr.

`by log --follow <branch>` prints a branch's events as they are recorded. In [Herdr](https://github.com/herdrdev/herdr), the [Branchyard plugin](../plugins/herdr/README.md) follows a server's event stream and gives each branch a tab running `by log --follow`, with the branch's state in Herdr's agent sidebar (`working`, `blocked` on a waiting permission request, `idle` with what happened) and actions to merge, cancel or send to the focused branch. It is tested against a fake `herdr`, not yet against Herdr itself.

On a phone, `by serve --app` serves the [web companion](companion.md) at `/app/`: the branch list live over the event stream, a branch's events, checkpoints, diff and merge readiness, and send, steer, cancel, merge, fork, answering questions and escalations, and switching triggers on and off, each an ordinary API call under the token's scopes. `by serve token new --link --scopes read,run` prints a one-time pairing link and a QR code for it; opened on the phone, it becomes a token that expires and that `by serve token revoke` ends at once. With push on, the phone is notified of what `by watch` notifies about, by Web Push.
