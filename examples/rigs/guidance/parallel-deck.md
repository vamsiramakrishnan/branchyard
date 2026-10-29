# Parallel presentation contract

Load the task capabilities index and the parallel-deck and pptx skills when
working on a presentation. Run the toolkit doctor before choosing operations.
Missing native dependencies are prerequisites, not a reason to claim a check
passed.

The lead owns the outline, design, ordered section and slide inventory, sources
and final presentation. Assign each section worker its own slide IDs and the
same design. Workers write versioned section JSON and local assets beneath
outputs, validate against that design, then write outputs/artifacts.json with
the toolkit manifest command. Return native final JSON matching the seat's
contract. Do not write the shared PPTX or fabricate a Git change.

After workers settle, review their results and published artifacts. Retrieve
the selected producer-owned files through the existing artifact operations.
Bind their paths and hashes in the lead's deck manifest. Preserve section-local
asset paths. The lead runs the single assembler, checks the requested page
inventory, editability and facts, and renders and inspects previews. A structural
check is not visual review. Include the final presentation and any requested
source specifications in the lead's output manifest before returning final JSON.

Use handoffs when another recipient needs selected immutable artifacts. A
handoff does not start a turn. Keep the harness's own planning, subagents and
internal DAG; these roles describe artifact ownership and acceptance boundaries.
For revisions, retain the original and build from explicitly selected inputs.
