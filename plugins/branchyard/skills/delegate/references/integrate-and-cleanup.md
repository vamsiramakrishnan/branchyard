# Delegate reference: Integrate, resolve sibling conflicts, cancel and discard

Read when: children are ready and you merge them: by integrate, a failed check, a conflict between siblings, cancel, discard, rm.

Steps 4 and 5 of "A managed lead coordinates code by hand".

4. Integrate. When children are `ready`, `by integrate <child>` merges one
   into your branch after its check passes; `by integrate a b c` merges
   several together, in order, runs the check once on the result and moves
   your branch once, all or none. Children that share your check integrate
   together: a child inherits your check unless you give it one (the spawn
   and `by inspect <child>` say which, and which siblings share it), and a
   whole-suite check passes only with all of them, so integrate them
   together rather than merging them into each other. Give a child its own
   check with `by spawn --check` when it should land alone. If you integrate one that shares
   its check and the check fails, the error names the siblings and the
   command (`by integrate a b c`). Never `git merge` children yourself: that
   bypasses the check, and Branchyard records them as merged only after the
   fact. A child your branch already contains is recorded as merged, not
   refused. A failed check shows its output and names every check that
   ran, the children it belongs to and whether it passed, so you know whose
   to fix: send the child a fix with `by send <child> "<prompt>"`, or do it
   yourself. `by check <child>` runs a child's check on its current work,
   as integrating it would, without integrating it.
   - **A conflict between siblings** names the child, the siblings merged
     before it and the files (`b conflicts with by/me plus a in calc.py;
     nothing was integrated`), and nothing moves. Resolve it in the
     conflicting child's branch, then integrate again. Its worktree shares
     the repository's branches, so it can merge the sibling's branch
     (`by/<sibling>`):

     ```sh
     by send b "Run git merge by/a in your worktree, resolve the conflicts in calc.py keeping both changes, commit the merge, run by check, then end your turn."
     by wait b
     by integrate a b
     ```

     b's candidate now contains a, so its merge onto a is clean, and
     siblings that share your check still integrate together. A child
     with a check of its own can land alone instead: `by integrate a`,
     then have b merge your branch (`git merge by/<you>`) and `by
     integrate b`. `by check b` says whether it still conflicts. Do not
     merge them yourself in your own worktree.
5. Clean up. `by cancel <child>` stops a running child and everything
   below it. A child that already finished is not running, so cancel
   changes nothing; set a finished child you will not use aside with
   `by discard <child> --reason "<why>"`. It keeps its record and cost,
   is never integrated or sent more work, and so never holds a slot or
   your budget again, and never keeps you waiting. It keeps its name, its
   worktree and its git branch until `by rm <child>` removes them; what it
   spent still counts in your budget.
