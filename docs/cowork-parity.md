# Cowork parity

Branchyard's direction, set 1 October 2026: the capabilities of Claude Cowork, for any harness, locally or on a server. Cowork is Anthropic's agent for knowledge work in the Claude desktop app. It works in folders the user grants, runs code in a VM, uses connectors, skills and plugins, runs sub-agents and scheduled tasks, takes tasks from a phone, and produces documents, spreadsheets and slides. This page compares the two and says which wave closes each gap. Sources are at the end.

| Cowork | Branchyard today | Gap | Wave |
|---|---|---|---|
| Works in folders the user grants, on any files | Every task is a git repository: in a repository you have, or `by task new --folder PATH` with the folder's git directory kept outside it, attempts in their own worktrees and the folder written only on accept (refused over your own changes), large files chunked, or `--no-files`; the conversation committed beside each checkpoint ([task repositories](task-repos.md)) | Sync to cloud storage | 6 |
| Code runs in an isolated VM | Local process, Microsandbox microVMs, Substrate actors, recipe machines | None in kind; egress restriction arrives with Wave 4 | 4 |
| Projects: files, instructions and context kept across sessions; `CLAUDE.md` at global and folder level | Repositories, `[workspace]` setup, repository knowledge adopted after review | Instructions per folder and per person, not only per repository | 6 |
| Sub-agents work in parallel and the results are combined | `by fan`, delegation, graphs; `by map` in Wave 4 | None once `by map` lands | 4 |
| Scheduled tasks, run in the cloud while the computer sleeps | Triggers on cron, interval and webhooks, run by `by serve` or a worker | Email triggers in Wave 4; a hosted scheduler needs a server, which Branchyard leaves to you | 4 |
| Tasks sent from a phone, run with local files and connectors | The companion page and Web Push | Starting a new task from the phone, not only following one | 6 |
| Connectors to Slack, Gmail, Drive, Jira and others, reading and writing | Connectors compiled by Anvil, through one gateway, per-turn tokens, audited | The catalog refreshes from the official MCP registry on request, pinned and checksummed, over the static baseline, and the gateway is found by capability rather than configured ([registry](registry.md)); writes are in an [effect ledger](effects.md) (Wave 5) | 5 |
| Skills, loaded on demand, with built-in skills for PDF, Word, Excel and PowerPoint | Connector skills and an index; repository knowledge | Document skills for any harness, and skills that are not tied to a connector | 6 |
| Plugins: skills, connectors, slash commands and sub-agents in one installable package, from a marketplace | Harness profiles, connector packages, a skill and plugin archives for Claude Code and Codex | A Branchyard plugin format and a registry to install from | 6 |
| Permission modes (ask first, or act); deletion always asks; each tool Allow, Ask or Block; admin locks | Policies with rules, delegated narrowing, plan approval, presets; and, built in Wave 5, [approvals](effects.md#approvals): each tool and connector operation Allow, Ask, Block or Stage from an administrator's locked policy, the seat's, the person's and the preset's, deletion always asked, asks answered from `by approvals`, `by watch`, the companion page, the API or a delegating parent; every connector write in an effect ledger, staged as a draft, reconciled after a crash, and undone where the upstream allows (`by undo`) | Anvil's effect declarations, which the ledger reads (until then every write is irreversible) | 5 |
| Classifiers look for prompt injection in untrusted content | None | Screening connector results and fetched pages before a harness sees them | 6 |
| Browser use (Claude in Chrome) | None | A browser a harness can drive inside the sandbox, under egress policy | 6 |
| Live artifacts: dashboards that refresh when reopened | Diffs, logs and the companion page | Results a branch publishes as a page that reads live data | 6 |
| OpenTelemetry to a SIEM for audit | Prometheus metrics, OTLP traces, connector audit | None in kind | — |
| One model, chosen by the app | Any harness and model, routed per task kind, with failover and a judge; on the [model gateway](model-gateway.md), model calls go through Branchyard on the turn's token, with weighted backends, fallbacks, rate limits, budgets and exact cost, and keys stay out of sandboxes | Branchyard is ahead. Built in Wave 5 and tested against mocks only; subscription logins still call their provider directly | 5 |

## What Branchyard adds that Cowork does not

- Any harness, not one: 15 driven, 47 known, routed per task kind with outcomes recorded.
- Durable operations on SQLite or PostgreSQL that survive restarts, with several servers on one database.
- Validated merges, judged candidates, plans approved before execution, goals a judge verifies.
- A server you run yourself, on Linux, with workers carrying labels.

## Sources

- [Get started with Claude Cowork](https://support.claude.com/en/articles/13345190-get-started-with-claude-cowork)
- [Schedule recurring tasks in Claude Cowork](https://support.claude.com/en/articles/13854387-schedule-recurring-tasks-in-claude-cowork)
- [Customize Claude Cowork](https://academy.claude.com/tutorials/customize-claude-cowork)
- [Claude Cowork features overview (fast.io)](https://fast.io/resources/claude-cowork-features-overview/)
- [Claude Cowork guide for power users](https://karozieminski.substack.com/p/claude-cowork-guide-plugins-memory-sub-agents-tips)
