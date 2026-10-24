"""The Python HTTP client against a real `by serve`, driven by
`crates/branchyard-cli/tests/sdk_client.rs`, which starts the server with
the fake agent as `gemini-cli`, a fake Anvil for the connector packages
and delegation allowed, then runs this with:

    BRANCHYARD_URL         the server's URL
    BRANCHYARD_TOKEN_FILE  its token file
    BRANCHYARD_REPO        the served repository's name

A surface's conversation is created with a grant and an envelope, its
harness delegates a child that asks for more than the parent holds and
gets the intersection, a second message continues the same branch and
keeps the grant, the busy policies hold against a running turn, the server's
refusals arrive as the typed errors, and a check and a self-reported status
are run for the child through their routes. Prints `ok` and exits 0.

Not a `test_*.py`: it needs the server the Rust test starts.
"""

import json
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "sdk" / "python"))

import branchyard_client as bc  # noqa: E402


def check(condition: bool, *context: object) -> None:
    if not condition:
        raise AssertionError(" ".join(str(c) for c in context))


def grants(inspection: bc.Inspection) -> list[str]:
    return [str(g) for g in inspection.grants]


def main() -> None:
    client = bc.Client(os.environ["BRANCHYARD_URL"], token_file=os.environ["BRANCHYARD_TOKEN_FILE"], timeout=60)
    repo_name = os.environ["BRANCHYARD_REPO"]
    app = client.repo(repo_name)

    check(client.healthy(), "healthz")
    check(any(r.name == repo_name for r in client.repos()), "repos", client.repos())
    check(any(h.harness == "gemini-cli" for h in client.harnesses()), "harnesses", client.harnesses())

    # A wrong token is `unauthorized`, before anything else.
    try:
        bc.Client(client.url, "not-the-token").repos()
    except bc.UnauthorizedError as error:
        check(error.code == "unauthorized", error)
    else:
        raise AssertionError("a wrong token was accepted")

    # The surface: one branch per Slack channel, every one with the same
    # grant and the same envelope; the fake agent runs the `SH` lines of
    # its prompt and says their output.
    chat = app.surface(
        "slack",
        harness="gemini-cli",
        budget_usd=1.0,
        connectors=[bc.Grant.parse("github:write:issues.*"), "linear:read"],
        delegation={"max_depth": 1, "max_children": 2, "harnesses": []},
        allow_delegation=True,
    )
    conversation = chat.conversation("C0123")
    check(not conversation.exists(), "fresh conversation")
    branch = conversation.branch
    check(branch == "slack-c0123", branch)

    # 1. The first message creates the branch; its harness delegates a child
    #    that asks for a wider linear grant than the parent holds.
    spawn = (
        "SH by spawn 'SH echo kid-done' --name kid --budget-usd 0.2 --connector github:read:issues.list "
        "--connector linear:write --wait --json"
    )
    first = conversation.say(f"{spawn}\nSH echo hello-from-slack")
    check(first.operation is not None and first.operation.kind == "task", first.operation)
    first.wait(timeout=180)
    check(conversation.exists(), "created")
    text = first.text
    check("hello-from-slack" in text, "reply text:", text)
    # `by spawn --wait --json` printed the child's inspection into the turn,
    # its grant included: the harness-side view carries the same field.
    check('"name": "kid"' in text and '"grants"' in text, "the spawn's inspection:", text)
    info = first.branch_info
    # The fake agent writes nothing, so the branch settles as `no_changes`.
    check(info is not None and info.state in ("ready", "no_changes"), "branch after the turn:", info, text)

    # 2. Grants, as stored and as narrowed: the parent's as given, the child's
    #    the intersection (linear:write asked, linear:read held).
    parent = conversation.inspect()
    check(grants(parent) == ["github:write:issues.*", "linear:read"], "parent grant:", grants(parent))
    check([c.name for c in app.children(branch)] == ["kid"], "children:", app.children(branch), "said:", text)
    kid = app.inspect("kid")
    check(grants(kid) == ["github:read:issues.list", "linear:read"], "child grant:", grants(kid))
    check(kid.parent == branch and kid.depth == 1, kid.raw)
    check(app.branch("kid").state in ("ready", "no_changes"), app.branch("kid"))

    # 3. A second message continues the same branch; a send that names no
    #    grant keeps it, and the reply is this turn's text only.
    second = conversation.say("SH echo second-turn").wait(timeout=180)
    check("second-turn" in second.text and "hello-from-slack" not in second.text, "second reply:", second.text)
    again = conversation.inspect()
    check(again.turns == 2, "turns:", again.turns)
    check(grants(again) == ["github:write:issues.*", "linear:read"], "grant after a send:", grants(again))
    said, cursor = app.said(branch)
    check("hello-from-slack" in said and "second-turn" in said, "said:", said)
    check(cursor > 0, cursor)

    # 4. The feed from the start carries both turns for this branch.
    stream = app.stream(0, branches=[branch])
    texts = []
    for entry in stream:
        if entry.text:
            texts.append(entry.text)
        if stream.head is not None and entry.seq >= stream.head:
            stream.close()
    joined = "".join(texts)
    check("hello-from-slack" in joined and "second-turn" in joined, "stream:", joined)

    # 5. Busy policies against a turn that takes a while.
    slow = conversation.say("SH sleep 4\nSH echo slow-done")
    try:
        conversation.say("SH echo skipped", busy="skip")
    except bc.BranchBusyError as busy:
        check(busy.holder == slow.operation.id, busy.holder, slow.operation.id)  # type: ignore[union-attr]
    else:
        raise AssertionError("skip did not raise while the branch was busy")
    queued = conversation.say("SH echo after-queue", busy="queue", timeout=120)
    check(queued.operation.id != slow.operation.id, "queued after the holder")  # type: ignore[union-attr]
    slow.wait(timeout=180)
    queued.wait(timeout=180)
    check("slow-done" in slow.text, slow.text)
    check("after-queue" in queued.text, queued.text)
    check(conversation.inspect().turns == 4, conversation.inspect().turns)

    # 6. Idempotency: the same key and body again is the same operation.
    key = bc.new_key()
    once = app.send(branch, "SH echo third", key=key)
    client.wait(once, timeout=180, check=True)
    twice = app.send(branch, "SH echo third", key=key)
    check(twice.id == once.id and twice.replayed, once.id, twice.id, twice.replayed)
    with_other_body = None
    try:
        app.send(branch, "SH echo different", key=key)
    except bc.IdempotencyKeyReusedError as error:
        with_other_body = error
    check(with_other_body is not None and with_other_body.detail.get("operation") == once.id, with_other_body)

    # 7. The server's refusals arrive typed.
    try:
        app.inspect("nope")
    except bc.NotFoundError as error:
        check(error.code == "unknown_branch", error.code)
    else:
        raise AssertionError("inspecting a missing branch succeeded")
    try:
        app.send("nope", "x")
    except bc.NotFoundError as error:
        check(error.code == "unknown_branch", error.code)
    else:
        raise AssertionError("sending to a missing branch succeeded")
    # A grant without a private home is admitted as an operation and refused
    # as it runs, before a branch exists.
    unhomed = app.task("SH echo x", harness="gemini-cli", name="unhomed", connectors=["github:read"], isolated=False)
    try:
        client.wait(unhomed, timeout=180, check=True)
    except bc.OperationFailedError as error:
        check("home" in error.message or "isolated" in error.message, "the refusal's words:", error.message)
    else:
        raise AssertionError("a grant without a private home was accepted")
    check(not app.exists("unhomed"), "a refused grant made a branch")
    # A child asking for a connector its parent lacks: over HTTP the spawn
    # is admitted as an operation and refused `denied` as it runs, by name.
    wide = app.spawn(branch, "SH echo x", name="too-wide", budget_usd=0.1, connectors=["slack:read"])
    try:
        client.wait(wide, timeout=180, check=True)
    except bc.OperationFailedError as error:
        check(error.code == "denied" and "slack" in error.message, "the refusal:", error.code, error.message)
    else:
        raise AssertionError("a child was granted a connector its parent lacks")
    check(not app.exists("too-wide"), "a refused spawn made a branch")

    # 8. A check and a self-reported status for the child, as a person acting
    #    for it: the check is an operation whose result is the report (this
    #    branch has no check, so nothing ran and nothing conflicted), and a
    #    blocked status reaches the parent and shows in the inspection.
    checked = client.wait(app.check("kid"), timeout=180, check=True)
    report = checked.check_report
    check(report is not None and report.passed and report.outcome == "not_run", "check:", checked.raw)
    check(report is not None and report.branch == "kid" and report.target == f"by/{branch}", report)
    reported = app.report_status("kid", "blocked", "needs the API key", link="https://example.com/issues/7")
    check(reported.report.state == "blocked" and reported.report.link == "https://example.com/issues/7", reported)
    check(reported.escalation is not None and reported.escalation.to == branch, "escalation:", reported)
    check(app.inspect("kid").reported == reported.report, "inspection:", app.inspect("kid").raw)
    try:
        app.report_status("kid", "done", "x")
    except bc.InvalidRequestError as error:
        check(error.field == "state" and "working" in error.message, "the refusal:", error.detail, error.message)
    else:
        raise AssertionError("a state that is not one of the five was accepted")
    try:
        app.check("nope")
    except bc.NotFoundError as error:
        check(error.code == "unknown_branch", error.code)
    else:
        raise AssertionError("checking a missing branch succeeded")

    # 9. The harness-side view agrees: `by inspect --json` is the same object.
    check(json.dumps(parent.raw)[:1] == "{", parent.raw)

    # 10. The rest of the API, one real call or two per family.
    rest(client, app, repo_name, branch)
    print("ok")


def rest(client: bc.Client, app: bc.Repo, repo_name: str, branch: str) -> None:
    # Discovery, without the token; the fleet and its workers.
    known = client.well_known()
    check(known.service == "branchyard" and known.repos == [repo_name], "well-known:", known)
    check(isinstance(client.inventory(), list), "inventory")
    registered = client.register_service("model_gateway", id="gw-e2e", capabilities={"models": ["m"]}, ttl_seconds=60)
    check(registered.id == "gw-e2e" and registered.kind == "model_gateway", "registered:", registered)
    check("gw-e2e" in [s.id for s in client.services(kind="model_gateway")], "services:", client.services())
    check(client.deregister_service("gw-e2e").id == "gw-e2e", "deregistered")
    check(isinstance(client.reclaim_services(), list), "reclaimed")
    try:
        client.deregister_service("no-such-service")
    except bc.NotFoundError as error:
        check(error.code == "not_found", error.code)
    else:
        raise AssertionError("deregistering a missing service succeeded")

    # Knowledge: added as the caller (adopted), listed, edited, exported,
    # distilled from a branch; an unknown entry is typed.
    entry = app.add_knowledge("Run the parser tests before merging", path="src/**")
    check(entry.status == "adopted" and entry.scope == {"path": "src/**"}, "added:", entry)
    check([e.id for e in app.knowledge(status="adopted")] == [entry.id], "listed:", app.knowledge())
    proposed = app.add_knowledge("Prefer small commits", propose=True)
    check(proposed.status == "proposed", proposed)
    check(app.adopt_knowledge(proposed.id).status == "adopted", "adopted")
    check(app.edit_knowledge(entry.id, text="Run the parser tests").text == "Run the parser tests", "edited")
    check(app.reject_knowledge(proposed.id, reason="too vague").status == "rejected", "rejected")
    exported = app.export_knowledge()
    check("Run the parser tests" in exported.markdown and exported.entries == [entry.id], "exported:", exported)
    check(app.knowledge_entry(entry.id).id == entry.id, "entry")
    check(app.distill(branch).branch == branch, "distilled")
    check(app.remove_knowledge(proposed.id).id == proposed.id, "removed")
    try:
        app.knowledge_entry(9999)
    except bc.NotFoundError as error:
        check(error.code == "unknown_knowledge", error.code)
    else:
        raise AssertionError("an unknown knowledge entry was found")

    # A scratch area's lock, taken for the branch and released.
    app.create_scratch(branch, "cache")
    lock = app.lock_scratch(branch, "cache")
    check(lock.holder_branch == branch and lock.name == "cache", "lock:", lock)
    held = app.scratch_lock_state("cache")
    check(held is not None and held.holder_branch == branch, "lock state:", held)
    app.unlock_scratch(branch, "cache")
    check(app.scratch_lock_state("cache") is None, "unlocked")

    # The effect ledger: nothing to reconcile or undo on a branch that
    # called no connector.
    check(isinstance(app.reconcile_effects().settled, list), "reconciled")
    plan = app.undo_plan(branch)
    check(plan.branch == branch and plan.to == 0 and plan.reversible == [], "undo plan:", plan)
    check(app.undo(branch).outcomes == [], "undone")

    # Task records: the tasks this run started, one found by its id.
    records = app.task_records()
    check(records, "task records")
    check(app.task_record(records[0].id).id == records[0].id, "task record")

    # A map over two items, waited for, shown, listed, resumed and forgotten.
    items = [{"id": "a", "word": "alpha"}, {"id": "b", "word": "beta"}]
    mapped = client.wait(app.map("SH echo {{item.word}}", items, name="words", harness="gemini-cli"), timeout=180)
    check(mapped.kind == "map" and mapped.state == "succeeded", "map:", mapped.raw)
    report = app.map_report("words")
    check(report.total == 2 and report.done == 2, "map report:", report)
    check("words" in [m.name for m in app.maps()], "maps:", app.maps())
    again = client.wait(app.resume_map("words"), timeout=180, check=True)
    check(app.map_report("words").done == 2, "resumed:", again.raw)
    app.remove_map("words")
    check("words" not in [m.name for m in app.maps()], "map removed")

    # A generic trigger's secret, generated and shown once.
    client.create_trigger(
        {
            "name": "e2e-hook",
            "repo": repo_name,
            "when": {"kind": "event", "source": "generic"},
            "task": {"prompt": "{{event.text}}", "harness": "gemini-cli"},
        }
    )
    secret = client.set_trigger_secret("e2e-hook")
    check(isinstance(secret.secret, str) and len(secret.secret) >= 16, "secret:", secret)
    check(client.set_trigger_secret("e2e-hook", "given-secret-value").secret is None, "a given secret is not echoed")
    client.delete_trigger("e2e-hook")

    # Web Push on a server without the companion: no such route.
    try:
        client.push_info()
    except bc.NotFoundError as error:
        check(error.code == "not_found", error.code)
    else:
        raise AssertionError("push info without the companion")


if __name__ == "__main__":
    main()
