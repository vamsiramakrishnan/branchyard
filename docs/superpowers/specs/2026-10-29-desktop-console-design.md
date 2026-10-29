# Desktop console: design

Written 29 October 2026. The web companion (`by serve --app`, [companion](../../companion.md)) becomes a desktop-first console. A person keeps it open beside an editor, sees every branch at a glance, reads what an agent did and merges it. Phones still work: the same page collapses to one column.

## Why

The companion was built phone-first. On a desktop it is one 1000-pixel column, and its branch page stacks six forms (send, steer, resume, cancel, merge, fork), disabled ones explained by "Not now:" captions, above the diff. Stream notices pile up as toasts that cover the page. Six top-level sections compete for attention, and every state looks alike.

## Principles

- **Progressive disclosure.** Three levels: Glance (does anything need me?), Focus (what happened and what is next?), Detail (prove it). Each level shows only what it needs; the next is one click away.
- **Colour means act.** The chrome is neutral. Colour marks a state you can act on: amber needs you, blue is running, green is ready.
- **One next step.** A branch shows one primary action for its state. Rare actions sit in a menu.
- **Quiet.** The list is the notice: a branch that needs you moves to the top and its dot turns amber. Toasts report the result of your own action only.

## Layout

At 1100 pixels and wider, three panes fill the window:

| Pane | Shows | Level |
|---|---|---|
| Sidebar (about 260 px) | Repository picker, live state, **New task**, the filter, then branches grouped into lanes; a **Needs you** link with its count; Triggers, Queue and Settings at the foot | Glance |
| Main | The selected branch: a status strip with its primary action and a ⋯ menu, the conversation, and one message box | Focus |
| Inspector (about 380 px) | Tabs: **Changes** (files with their counts, then the selected file's diff), **Details** (facts, checkpoints, operations), **Activity** (the raw event log) | Detail |

From 760 to 1100 pixels the inspector stacks under the conversation. Below 760 pixels the page is one column: the sidebar is the home view, and a branch opens full width with a back link.

With no branch selected, the main pane shows the repository at a glance: counts per lane, and the branches that need you.

## Domain structures

These replace scattered conditionals in `app.js`.

**Lanes.** One table maps a branch to its group, in display order:

| Lane | Branches | Shown |
|---|---|---|
| Needs you | `blocked`, `failed`, `interrupted`, `budget_exceeded`, `awaiting_plan_approval`; stalled; with a pending approval or question | expanded |
| Running | `running`, `waiting` (not stalled) | expanded |
| Ready | `ready` | expanded |
| Done | `merged`, `no_changes` | collapsed to "Show N done" |

**Primary action.** One table maps a status to the strip's main button: `ready` → Merge (confirmed); `interrupted` → Resume. Other states have no main button. Cancel turn and Fork live in the ⋯ menu, enabled under the existing `when` rules. An action that does not apply is not rendered. The "Not now:" captions go.

**Message box mode.** The box's verb follows the state, from the same `when` rules: a running branch steers (`…/steer`); `waiting` disables the box and says why in its placeholder; any other state sends a follow-up (`…/send`). Fork, chosen from the menu, puts the box in fork mode until sent or cancelled. "Allow every tool" is a toggle beside the button.

**Conversation.** A reducer over the event page turns events into turns. Each turn has the prompt (or steer text), the agent's text joined from message deltas, the tool calls of that turn folded into one "N tool calls" disclosure that opens to their names, and the turn's outcome (candidate, checkpoint, status). Statuses and session bookkeeping go to Activity only.

## Routes

`#/` home; `#/b/<repo>/<branch>` a branch; `#/new` the task form in the main pane; `#/attention` Needs you, which merges the Inbox (questions, escalations, permission requests seen) and Approvals (waiting approvals and the effect ledger); `#/triggers`, `#/queue`, `#/settings`. `#/inbox` and `#/approvals` stay as aliases of `#/attention`: push notifications open them (`companion/push.rs`).

## Visual system

System fonts. Two sizes for most text (13 and 15 pixels), a 20-pixel title, monospace for ids, paths and diff counts. Warm neutral greys, one green accent, the same tokens in light and dark. Text keeps at least 4.5:1 contrast against its background in both themes. Spacing on a 4-pixel grid. Radius 8 pixels for controls, 12 for panes and cards.

Delight, all small and within `prefers-reduced-motion`:

- A running branch's dot breathes slowly; done branches go still and grey.
- After a merge succeeds, the button reads **Merged** with a drawn check, and the branch moves to Done.
- The tab title carries the Needs you count, as `(2) Branchyard`.
- Relative times ("4m") tick without a reload.
- Keys: `/` focuses the filter, `j` and `k` move through branches, `Enter` opens one.

## Constraints

- No server or API change. The page's files stay self-contained: no inline script or style, no `innerHTML`, no external URL (`every_asset_is_self_contained` in `companion/mod.rs`, and the CSP). No framework and no build step.
- Server text is set as text only.
- `companion/mod.rs` and `companion/link.rs` are not edited; another branch is changing them.

## Tests

`tests/companion_browser.js` keeps every behaviour it checks today: a refused token, pairing with the code removed from the address, the empty, loading and error states with retry, the live state, filtering, the preserved selection during a live refresh, no horizontal overflow at 320 and 390 pixels, a follow-up sent from the branch page, the second turn, the diff, the expired-token recovery, and no policy violation or page error. It adds the three panes side by side at 1280 pixels, a ready branch's **Merge** in the strip with no "Not now:" text on the page, folded tool calls, and `#/inbox` and `#/approvals` opening Needs you.

## Not in scope

Plan approval from the page, syntax highlighting, resizable panes, starting several tasks at once, and the workspace console on `feat/console-foundation`.
