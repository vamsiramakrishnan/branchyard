# The setup protocol

Schema: `schema/setup.protocol.json` (`branchyard.setup/v1`). Every
response is deterministic for the same answers and machine.

## Commands

```sh
by init --json                                          # topics
by init TOPIC --json --next [--answers FILE|-]          # next batch, or done + plan
by init TOPIC --answers FILE|- --dry-run --json         # the plan: files, diffs, validation
by init TOPIC --answers FILE|- --apply [--force] --json # write it
```

`--defaults` fills every unanswered question with its default (only when
the user asked for defaults). `-` reads the answers from stdin.

## A step's response

```json
{
  "protocol": "branchyard.setup/v1",
  "topic": "project",
  "done": false,
  "facts": [{"id": "harnesses", "label": "Harnesses", "value": "claude-code 2.1.283, codex 0.157.1"}],
  "answers": {"scope": "project"},
  "errors": [{"id": "budget_usd", "message": "must be at least 0.01"}],
  "questions": [
    {
      "id": "harness", "kind": "select", "header": "Harness",
      "prompt": "Which harness should new branches use?",
      "why": "by run and by fan use it when you give no --harness.",
      "choices": [{"value": "claude-code", "label": "Claude Code", "description": "installed: 2.1.283", "recommended": true}],
      "more_choices": [], "allow_other": true, "default": "claude-code", "optional": false
    }
  ],
  "remaining": 7
}
```

When `done` is true, `plan` holds `files` (`path`, `kind`, `mode`,
`action` = create|update|unchanged|keep, `overwrites`, `sensitive`,
`content`, `diff`, `validation`), `commands` and `notes`, and `valid`.

## Mapping a question to AskUserQuestion

| Field | AskUserQuestion |
|---|---|
| `header` (≤12 characters) | `header` |
| `prompt` | `question` |
| `choices[].label`, `choices[].description` | `options[].label`, `options[].description` |
| `kind == "multiselect"` | `multiSelect: true` |

- Use the choices as given; they are already at most four, recommended
  first. If a question has only one choice, add an option "Type a value"
  so there are two, or ask it in chat.
- `more_choices` and `allow_other` are reached through "Other": the
  answer may be any `more_choices` value or label, or free text the
  question's `rules` accept.
- Send back the option's **label** or its `value`; both are accepted.
  For `multiselect`, send a list or the labels joined by ", ".
- `kind`: `confirm` takes `true`/`false` or `"yes"`/`"no"`; `number` a
  number or numeric text; `secret_ref` a variable name or `@path`, never a
  secret; `"skip"` or `null` skips an `optional` question.
- `when` says which earlier answer made a question appear; you need not
  evaluate it, the engine already did.

## Errors

A refusal prints `{"error": {"kind", "message"}}` and exits non-zero:
`incomplete` (questions remain), `invalid_plan` (a file failed its
validator), `would_overwrite` (with `paths`; needs `--force` and the
user's consent), `invalid_answers`, `usage`, `io`.
