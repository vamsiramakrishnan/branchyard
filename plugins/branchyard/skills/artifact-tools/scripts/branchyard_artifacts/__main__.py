"""JSON CLI for optional artifact operations, without implicit bootstrap or model calls."""

from __future__ import annotations

import argparse
import importlib
import json
from pathlib import Path
from typing import Any

from .common import ArtifactError, Unavailable


def parser() -> argparse.ArgumentParser:
    """Build the documented command tree without loading optional libraries."""
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="format", required=True)
    doctor = commands.add_parser("doctor", help="Report installed capabilities; never install dependencies")
    doctor.add_argument("--require", default="", help="Comma-separated required capabilities")
    manifest = commands.add_parser("manifest", help="Hash named outputs for after-transfer collection; never publish")
    manifest.add_argument("files", type=Path, nargs="+")
    manifest.add_argument("--out", type=Path, required=True)
    knowledge = commands.add_parser("knowledge", help="Capture evidence, transform tables and check deliverables")
    operations = knowledge.add_subparsers(dest="action", required=True)
    for action in ["source-capture", "table-extract", "data-clean", "calculate", "claim-verify", "inspect-layout"]:
        operation = operations.add_parser(action)
        operation.add_argument("file", type=Path)
        operation.add_argument("--out", type=Path, required=True)
    deck = commands.add_parser("deck", help="Validate independent sections and render one editable deck")
    actions = deck.add_subparsers(dest="action", required=True)
    section = actions.add_parser("section-check")
    section.add_argument("file", type=Path)
    section.add_argument("--deck", type=Path)
    assemble = actions.add_parser("assemble")
    assemble.add_argument("file", type=Path)
    assemble.add_argument("--out", type=Path, required=True)
    schema = actions.add_parser("schema")
    schema.add_argument("name", choices=["deck", "section"])
    for name in ["docx", "xlsx", "pdf", "pptx"]:
        format_parser = commands.add_parser(name)
        actions = format_parser.add_subparsers(dest="action", required=True)
        for action in ["create", "read", "update", "check", *(["pivot"] if name == "xlsx" else [])]:
            command = actions.add_parser(action)
            if action == "create":
                command.add_argument("spec", type=Path)
            else:
                command.add_argument("file", type=Path)
            if action in {"update", "pivot"}:
                command.add_argument("spec", type=Path)
            if action in {"create", "update", "pivot"}:
                command.add_argument("--out", type=Path, required=True)
            if action == "check":
                command.add_argument("--require-render", action="store_true")
                command.add_argument("--preview-dir", type=Path)
    return root


def execute(args: argparse.Namespace) -> dict[str, Any]:
    """Dispatch only known operations; JSON requests cannot select code or dependencies."""
    if args.format == "doctor":
        from .doctor import doctor

        return doctor([item for item in args.require.split(",") if item])
    if args.format == "manifest":
        from .manifest import create

        return create(args.files, args.out)
    if args.format == "knowledge":
        if args.action in {"source-capture", "claim-verify"}:
            from .evidence import capture, verify_claims

            return (capture if args.action == "source-capture" else verify_claims)(args.file, args.out)
        if args.action == "table-extract":
            from .tables import extract

            return extract(args.file, args.out)
        if args.action in {"data-clean", "calculate"}:
            from .analysis import calculate, clean

            return (clean if args.action == "data-clean" else calculate)(args.file, args.out)
        from .native import render

        return render(args.file, args.out)
    if args.format == "deck":
        from .deck import assemble, validate_section

        if args.action == "schema":
            return json.loads((Path(__file__).parent / "schemas" / (args.name + "-v1.json")).read_text())
        if args.action == "section-check":
            return validate_section(args.file, args.deck)
        return assemble(args.file, args.out)
    module = importlib.import_module("." + args.format, __package__)
    operation = getattr(module, args.action)
    if args.action == "create":
        return operation(args.spec, args.out)
    if args.action in {"update", "pivot"}:
        return operation(args.file, args.spec, args.out)
    result = operation(args.file)
    if args.action == "check" and args.require_render:
        from .native import render

        result.update(render(args.file, args.preview_dir or args.file.with_suffix(".preview")))
    return result


def main(argv: list[str] | None = None) -> int:
    """Print one JSON value, distinguishing unavailable requirements from failed checks."""
    args = parser().parse_args(argv)
    try:
        value = execute(args)
    except Exception as error:
        # This is the executable boundary, including library parse errors for malformed files.
        unavailable = isinstance(error, Unavailable)
        value = {
            "status": "unavailable" if unavailable else "failed",
            "error": {
                "kind": error.kind if isinstance(error, ArtifactError) else "operation_failed",
                "message": str(error),
                "next": "Run python -m branchyard_artifacts doctor; inspect the input spec",
            },
        }
    print(json.dumps(value, ensure_ascii=False, allow_nan=False))
    return {"passed": 0, "failed": 1, "unavailable": 3}.get(value.get("status", "passed"), 0)


if __name__ == "__main__":
    raise SystemExit(main())
