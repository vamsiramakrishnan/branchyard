# Delegate reference: Writing a child's prompt

Read when: you write the prompt for by spawn or by delegate and want a child that can finish without you.

## Writing a child's prompt

A child sees your committed work, the shared brief and its prompt, not this
conversation. Write the prompt so that someone who knows nothing else could
do the task and show it is done. Say, in this order:

1. The goal, in one sentence.
2. The files it owns, and the ones it may read but not change.
3. The constraints: interfaces already settled (those belong in the brief),
   crates not to add, the style to match.
4. How it checks its work: the exact command, and what passing means.
5. What to report: what changed, what it could not do, what you must do next.

Good:

```sh
by spawn "Write the lexer for the calculator. You own src/lexer/ and may read src/token.rs, which you must not change. Match the error style in src/error.rs. Check with cargo test -p lexer; it must pass. Then report the token kinds you handle and anything you left out." \
  --name lexer --owns src/lexer --budget-usd 0.50
```

Bad: `"Do the lexer."` or `"Implement the plan we discussed."` The first names
no files, no interface and no check, so the child guesses all three; the second
points at a conversation the child never saw. Either one costs you a turn at
integration.
