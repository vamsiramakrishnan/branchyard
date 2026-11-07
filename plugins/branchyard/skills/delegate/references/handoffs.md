# Delegate reference: Relay a frozen result

Read when: you hand a settled result and chosen artifacts to another coworker with by handoff.

## Relay a frozen result

After the producer's current turn settles, hand its accepted result and only
the selected artifacts to an existing coworker in the same yard:

```sh
by handoff research --to synthesis --artifact art1 \
  --note "Use this finding and preserve its uncertainty." --json
```

Use a real published artifact ID, or omit `--artifact`. A handoff copies the
result, its source incarnation, turn, result event, candidate commit when any,
and immutable artifact references. It grants artifact access to the current
recipient incarnation. It never steers a running turn or starts the next one.
After the recipient's current turn finishes, resume it explicitly when ready.

A managed lead must be a common ancestor of both endpoints and be able to
read every selected artifact, as must the source. An operator can relay
between separate coworkers, with source artifact access still required.
Repeating an unchanged handoff returns its original ID. A changed producer
needs a new handoff. `--expected-version` accepts a reviewed version object
with `incarnation`, `turn`, and `result_event`, and refuses a stale one.
