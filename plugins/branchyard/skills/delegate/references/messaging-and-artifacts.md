# Delegate reference: Sharing results and asking your parent

Read when: you publish an artifact, ask or answer a question, report or set a status, read the inbox or message a sibling.

## Sharing results and asking your parent

Every child, even one that cannot delegate further, can publish files and
message you, and you can do the same with your own parent (the person, if
you have none):

```sh
by artifact publish results.json --name results --media-type application/json
by artifact list
by artifact get 3 --out data/results.json
by ask "Should the parser accept tabs?" --wait 300
by report "Tokenizer done; formatter next"
by report "Found two files" --result '{"files": ["a.rs", "b.rs"]}'   # when your parent set a result_contract
by report-status working "Porting the tokenizer's tests"
by inbox --unread
by answer 12 "Yes, accept tabs"
by message formatter "Lexer done; token kinds are in src/token.rs"   # a sibling reads it at its next turn
```

`by report-status <state> "<one line>" [--link URL]` states your own status
beside the one Branchyard derives: `working`, `idle`, `blocked`, `complete`
or `failure`, in at most 280 characters. Your parent sees the latest in
`by inspect` and the person watching in `by watch`; `blocked` and `failure`
are also escalated to your parent's inbox. It is a claim, not a message
and not a result: `by report` is how you tell your parent something.

Artifacts you publish are readable by your ancestors and descendants; a
sibling needs `by artifact share ID --to BRANCH`. Whoever can already
read the artifact may run that share, not only its publisher: the
publishing sibling itself, or a common ancestor of both. The receiving
sibling cannot, since it cannot read the artifact yet. `BRANCH`
must already exist: spawn the consumer first, or have it `by artifact
get` a copy a common ancestor republished to it. An artifact's `digest`
is the blake3 hash of its bytes.
