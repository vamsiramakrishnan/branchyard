"""Small reproducible CSV cleaning and unit-aware calculations through pandas and Pint."""

from __future__ import annotations

import csv
import hashlib
import importlib.metadata
import io
import json
import math
import re
import tokenize
from decimal import Decimal
from pathlib import Path
from typing import Any

from .common import ArtifactError, contained, digest, finite, library, load_json, output_directory, output_file, request

MAX_SPEC = 256 * 1024
MAX_INPUT = 64 * 1024 * 1024
MAX_ROWS = 100_000
MAX_COLUMNS = 256
MAX_CELLS = 1_000_000


def _spec(path: Path) -> dict[str, Any]:
    """Bound the small convenience specification before parsing it."""
    if path.stat().st_size > MAX_SPEC:
        raise ArtifactError("Analysis specification exceeds 256 KiB")
    return load_json(path)


def _columns(value: Any, name: str, available: list[str]) -> list[str]:
    """Validate explicit column choices without silently interpreting misspellings."""
    if not isinstance(value, list) or any(not isinstance(item, str) for item in value):
        raise ArtifactError(name + " must be a list of column names")
    if len(value) != len(set(value)) or set(value) - set(available):
        raise ArtifactError(name + " contains repeated or unknown columns")
    return value


def _integer(value: str) -> int:
    """Convert decimal integer input exactly; pandas' nullable float cast can truncate."""
    number = Decimal(value)
    if not number.is_finite() or number != number.to_integral_value() or not -(2**63) <= number < 2**63:
        raise ArtifactError("Integer conversion needs an exact signed 64-bit integer")
    return int(number)


def clean(spec: Path, out: Path) -> dict[str, Any]:
    """Publish a new CSV/receipt bundle with explicit trim, null, type and duplicate policies."""
    spec, out = Path(spec), Path(out)
    value = _spec(spec)
    request(value, {"input", "trim", "null_tokens", "types", "deduplicate"}, {"input"})
    ref = value["input"]
    if not isinstance(ref, dict) or set(ref) - {"path", "sha256"} or not isinstance(ref.get("path"), str):
        raise ArtifactError("input needs a relative path and optional SHA-256")
    source = contained(spec.parent, ref["path"])
    with source.open("rb") as stream:
        data = stream.read(MAX_INPUT + 1)
    if len(data) > MAX_INPUT:
        raise ArtifactError("CSV input exceeds 64 MiB")
    source_hash = hashlib.sha256(data).hexdigest()
    if "sha256" in ref and ref["sha256"] != source_hash:
        raise ArtifactError("CSV input SHA-256 mismatch")
    # Use the stdlib CSV parser to reject ambiguous headers pandas would rename.
    reader = csv.reader(io.StringIO(data.decode("utf-8-sig"), newline=""), strict=True)
    header = next(reader, None)
    if not header or len(header) > MAX_COLUMNS or any(not name for name in header) or len(set(header)) != len(header):
        raise ArtifactError("CSV needs unique nonempty column names, at most 256")
    for number, row in enumerate(reader, 1):
        if len(row) != len(header):
            raise ArtifactError("CSV record " + str(number) + " has a different width from its header")
        if number > MAX_ROWS or number * len(header) > MAX_CELLS:
            raise ArtifactError("CSV exceeds 100000 rows or one million cells")
    pandas = library("pandas")
    frame = pandas.read_csv(
        io.BytesIO(data),
        dtype="string",
        keep_default_na=False,
        na_filter=False,
        skip_blank_lines=False,
        encoding="utf-8-sig",
        nrows=MAX_ROWS + 1,
        on_bad_lines="error",
    )
    if list(frame.columns) != header or len(frame) > MAX_ROWS or len(frame) * len(header) > MAX_CELLS:
        raise ArtifactError("CSV exceeds 100000 rows or one million cells")
    trim = _columns(value.get("trim", []), "trim", header)
    tokens = value.get("null_tokens", [])
    if not isinstance(tokens, list) or len(tokens) > 256 or any(not isinstance(token, str) for token in tokens):
        raise ArtifactError("null_tokens must be at most 256 explicit strings")
    types = value.get("types", {})
    if (
        not isinstance(types, dict)
        or set(types) - set(header)
        or any(kind not in {"integer", "number", "string"} for kind in types.values())
    ):
        raise ArtifactError("types must map existing columns to integer, number or string")
    dedupe = value.get("deduplicate")
    if dedupe is not None:
        if not isinstance(dedupe, dict) or set(dedupe) != {"columns", "keep"}:
            raise ArtifactError("deduplicate needs columns and keep")
        if not _columns(dedupe["columns"], "deduplicate.columns", header) or dedupe["keep"] not in {
            "first",
            "last",
            "error",
        }:
            raise ArtifactError("deduplicate needs nonempty columns and keep first, last or error")
    trimmed = 0
    for name in trim:
        normalized = frame[name].str.strip()
        trimmed += int((frame[name] != normalized).sum())
        frame[name] = normalized
    nulls = 0
    for name in header:
        matches = frame[name].isin(tokens)
        nulls += int(matches.sum())
        frame.loc[matches, name] = pandas.NA
    for name, kind in types.items():
        if kind == "integer":
            frame[name] = pandas.Series(
                [pandas.NA if pandas.isna(item) else _integer(item) for item in frame[name]],
                index=frame.index,
                dtype="Int64",
            )
        elif kind == "number":
            converted = pandas.to_numeric(frame[name], errors="raise")
            if any(not math.isfinite(number) for number in converted.dropna()):
                raise ArtifactError("Non-finite numeric value in column " + name)
            frame[name] = converted.astype("Float64")
    original_rows = len(frame)
    if dedupe:
        if dedupe["keep"] == "error":
            if frame.duplicated(subset=dedupe["columns"], keep=False).any():
                raise ArtifactError("Duplicate rows violate the explicit error policy")
        else:
            frame = frame.drop_duplicates(subset=dedupe["columns"], keep=dedupe["keep"])
    receipt = {
        "schema_version": 1,
        "operation": "data-clean",
        "source": {"name": source.name, "sha256": source_hash, "bytes": len(data)},
        "spec_sha256": digest(spec),
        "tool": {"name": "pandas", "version": importlib.metadata.version("pandas")},
        "policy": {"trim": trim, "null_tokens": tokens, "types": types, "deduplicate": dedupe},
        "counts": {
            "input_rows": original_rows,
            "output_rows": len(frame),
            "trimmed_cells": trimmed,
            "nullified_cells": nulls,
            "duplicates_removed": original_rows - len(frame),
        },
        "columns": header,
        "null_counts": {name: int(frame[name].isna().sum()) for name in header},
        "null_serialization": "empty CSV field; consult null_counts and the explicit policy",
        "spreadsheet_formula_escape": "not_applied; CSV values are preserved; open as data, not executable formulas",
    }
    with output_directory(out) as staged:
        target = staged / "data.csv"
        frame.to_csv(target, index=False, lineterminator="\n", encoding="utf-8", na_rep="")
        if target.stat().st_size > MAX_INPUT:
            raise ArtifactError("Cleaned CSV output exceeds 64 MiB")
        receipt["output"] = {"path": "data.csv", "sha256": digest(target), "bytes": target.stat().st_size}
        (staged / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, allow_nan=False, indent=2) + "\n")
    return {"status": "passed", "path": str(out), "receipt": str(out / "receipt.json"), **receipt}


def _expression(expression: Any, variables: dict[str, Any]) -> str:
    """Bound Pint's native algebra input; token validation is not a second evaluator."""
    if not isinstance(expression, str) or not expression.strip() or len(expression) > 4096:
        raise ArtifactError("expression must be a nonempty string of at most 4096 characters")
    expression = expression.strip()
    tokens = list(tokenize.generate_tokens(io.StringIO(expression).readline))
    if len(tokens) > 256:
        raise ArtifactError("expression exceeds 256 tokens")
    if any(token.type == tokenize.OP and token.string not in {"+", "-", "*", "/", "(", ")"} for token in tokens):
        raise ArtifactError(
            "The calculation convenience supports +, -, *, / and parentheses; use native Pint for powers"
        )
    if any(token.type == tokenize.NAME and token.string not in variables for token in tokens):
        raise ArtifactError("expression names must refer to declared variables; functions are not supported")
    if any(
        token.type
        not in {tokenize.NAME, tokenize.NUMBER, tokenize.OP, tokenize.NEWLINE, tokenize.NL, tokenize.ENDMARKER}
        for token in tokens
    ):
        raise ArtifactError("expression supports numeric literals, declared variables and native arithmetic only")
    return expression


def calculate(spec: Path, out: Path) -> dict[str, Any]:
    """Evaluate bounded native Pint algebra with explicit variables, target units and currency basis."""
    spec, out = Path(spec), Path(out)
    value = _spec(spec)
    request(
        value, {"variables", "expression", "unit", "currency_basis", "assumptions"}, {"variables", "expression", "unit"}
    )
    target = value["unit"]
    if not isinstance(target, str) or not target or len(target) > 256:
        raise ArtifactError("unit must name explicit target units, or dimensionless")
    variables = value["variables"]
    if not isinstance(variables, dict) or len(variables) > 128:
        raise ArtifactError("variables must be an object of at most 128 named quantities")
    expression = _expression(value["expression"], variables)
    assumptions = value.get("assumptions", [])
    if (
        not isinstance(assumptions, list)
        or len(assumptions) > 128
        or any(not isinstance(item, str) or not item or len(item) > 2048 for item in assumptions)
    ):
        raise ArtifactError("assumptions must be at most 128 nonempty strings of at most 2048 characters")
    registry = library("pint").UnitRegistry()
    currency = value.get("currency_basis")
    if currency is not None:
        if (
            not isinstance(currency, dict)
            or set(currency) != {"code", "basis"}
            or not isinstance(currency["code"], str)
            or re.fullmatch("[A-Z]{3}", currency["code"]) is None
            or not isinstance(currency["basis"], str)
            or not 1 <= len(currency["basis"]) <= 2048
        ):
            raise ArtifactError("currency_basis needs an uppercase three-letter code and explicit basis text")
        registry.define(currency["code"] + " = [currency_" + currency["code"] + "]")
    quantities = {}
    for name, item in variables.items():
        if re.fullmatch("[a-z][a-z0-9_]{0,63}", name) is None or name in registry or name in {"nan", "inf", "infinity"}:
            raise ArtifactError("Variable names must be lowercase identifiers distinct from native unit names")
        if not isinstance(item, dict) or set(item) != {"value", "unit"}:
            raise ArtifactError("Each variable needs value and unit")
        if not isinstance(item["unit"], str) or not item["unit"] or len(item["unit"]) > 256:
            raise ArtifactError("Variable unit must be a nonempty string of at most 256 characters")
        quantities[name] = registry.Quantity(finite(item["value"], name), item["unit"])
    result = registry.parse_expression(expression, **quantities)
    result = registry.Quantity(result).to(target)
    magnitude = finite(float(result.magnitude), "Calculation result")
    report = {
        "schema_version": 1,
        "status": "passed",
        "operation": "calculate",
        "spec_sha256": digest(spec),
        "tool": {"name": "Pint", "version": importlib.metadata.version("Pint")},
        "expression": expression,
        "variables": variables,
        "result": {"value": magnitude, "unit": str(result.units)},
        "currency_basis": currency,
        "assumptions": assumptions,
        "exchange_rates": "not_used",
        "semantic_review": "not_performed",
    }
    encoded = json.dumps(report, ensure_ascii=False, allow_nan=False, indent=2).encode("utf-8") + b"\n"
    if out.resolve() == spec.resolve() or (out.exists() and out.samefile(spec)):
        raise ArtifactError("Calculation output must preserve its input specification")
    with output_file(out) as staged:
        staged.write_bytes(encoded)
    return {**report, "path": str(out), "sha256": hashlib.sha256(encoded).hexdigest()}
