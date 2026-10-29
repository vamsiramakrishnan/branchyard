"""Local source snapshots and reproducible citation-integrity checks.

``capture`` accepts a versioned object with ``file`` relative to its spec,
``origin`` containing supplied ``uri``, ``retrieved_at`` and ``title``, and an
optional ``source_id``. It atomically publishes a new directory containing the
original bytes, text segments and an index. Retrieval metadata is unverified.

``verify_claims`` accepts ``sources`` with relative index ``path`` and recorded
``sha256`` plus ``claims`` with ``id``, ``text`` and ``evidence``. Each evidence
entry names ``source_id``, an exact ``locator`` and a nonempty literal ``quote``.
The verifier checks hashes and re-extracts the copied bytes before checking
quotes. Neither matching quotations nor supplied URLs establish semantic truth.
Both operations remain offline and never evaluate workbook formulas or run OCR.
"""

from __future__ import annotations

import csv
import hashlib
import importlib.metadata
import io
import json
import os
import re
import stat
import zipfile
from datetime import date, datetime, time
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from .common import ArtifactError, contained, library, output_directory, output_file, report, request

MAX_SOURCE_BYTES = 32 * 1024 * 1024
MAX_METADATA_BYTES = 16 * 1024 * 1024
MAX_TEXT_CHARS = 8 * 1024 * 1024
MAX_SEGMENTS = 100_000
MAX_PAGES = 1_000
MAX_SOURCES = 32
MAX_CLAIMS = 1_000
TEXT_SUFFIXES = {".txt", ".md", ".json"}
MEDIA = {
    ".txt": "text/plain",
    ".md": "text/markdown",
    ".json": "application/json",
    ".csv": "text/csv",
    ".pdf": "application/pdf",
    ".docx": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ".xlsx": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
}


def _object(value: Any, allowed: set[str], required: set[str], name: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) - allowed or required - set(value):
        raise ArtifactError(name + " has missing or unknown fields")
    return value


def _string(value: Any, name: str, limit: int = 4096) -> str:
    if not isinstance(value, str) or not value.strip() or len(value) > limit or "\x00" in value:
        raise ArtifactError(name + " must be a nonempty bounded string")
    return value


def _sha(value: Any) -> str:
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None:
        raise ArtifactError("sha256 must contain 64 lowercase hexadecimal characters")
    return value


def _identifier(value: Any, name: str) -> str:
    if not isinstance(value, str) or re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9._:-]{0,127}", value) is None:
        raise ArtifactError(
            name + " must be a bounded identifier using letters, digits, dot, colon, underscore or dash"
        )
    return value


def _read_regular(path: Path, limit: int) -> bytes:
    """Bound regular-file reads and refuse a final symlink before reading bytes."""
    descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as stream:
        metadata = os.fstat(stream.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > limit:
            raise ArtifactError("Input must be a regular file within the byte limit")
        payload = stream.read(limit + 1)
    if len(payload) > limit:
        raise ArtifactError("Input exceeds the byte limit")
    return payload


def _invalid_constant(value: str) -> None:
    raise ArtifactError("JSON contains a non-finite number: " + value)


def _read_json(path: Path) -> dict[str, Any]:
    value = json.loads(_read_regular(path, MAX_METADATA_BYTES), parse_constant=_invalid_constant)
    if not isinstance(value, dict):
        raise ArtifactError("Expected a JSON object")
    return value


def _json_bytes(value: Any) -> bytes:
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2, allow_nan=False) + "\n").encode("utf-8")


def _origin(value: Any) -> dict[str, str]:
    value = _object(value, {"uri", "retrieved_at", "title"}, {"uri", "retrieved_at", "title"}, "origin")
    result = {key: _string(item, "origin." + key, 8192) for key, item in value.items()}
    uri = urlsplit(result["uri"])
    if not uri.scheme or uri.username is not None or uri.password is not None:
        raise ArtifactError("origin.uri needs a scheme and must not embed credentials")
    retrieved = datetime.fromisoformat(result["retrieved_at"].replace("Z", "+00:00"))
    if retrieved.tzinfo is None or retrieved.utcoffset() is None:
        raise ArtifactError("origin.retrieved_at needs an ISO timestamp with a timezone")
    return result


def _archive_bounds(payload: bytes) -> None:
    with zipfile.ZipFile(io.BytesIO(payload)) as archive:
        entries = archive.infolist()
        if len(entries) > 10_000 or sum(entry.file_size for entry in entries) > 128 * 1024 * 1024:
            raise ArtifactError("Office package exceeds the expanded byte or part limit")


def _extract(payload: bytes, suffix: str, source_id: str) -> dict[str, Any]:
    """Derive stable locators using existing format libraries and explicit scopes."""
    segments: list[dict[str, str]] = []
    warnings: list[dict[str, str]] = []
    text_size = 0

    def add(locator: str, text: str) -> None:
        nonlocal text_size
        text_size += len(text)
        if len(segments) >= MAX_SEGMENTS or text_size > MAX_TEXT_CHARS:
            raise ArtifactError("Source extraction exceeds the segment or text limit")
        segments.append({"id": source_id + "/" + locator, "locator": locator, "text": text})

    if suffix in TEXT_SUFFIXES:
        for number, line in enumerate(payload.decode("utf-8-sig").splitlines(), 1):
            add("line:" + str(number), line)
        extractor = {"name": "utf8-lines", "version": "1", "options": {"encoding": "utf-8-sig"}}
    elif suffix == ".csv":
        rows = csv.reader(io.StringIO(payload.decode("utf-8-sig"), newline=""), strict=True)
        for row_number, row in enumerate(rows, 1):
            for column, text in enumerate(row, 1):
                add("row:" + str(row_number) + "/column:" + str(column), text)
        extractor = {"name": "csv", "version": "1", "options": {"dialect": "excel", "encoding": "utf-8-sig"}}
    elif suffix == ".pdf":
        reader = library("pypdf").PdfReader(io.BytesIO(payload), strict=True)
        if reader.is_encrypted:
            raise ArtifactError("Encrypted source PDFs require an explicit decryption workflow")
        if len(reader.pages) > MAX_PAGES:
            raise ArtifactError("PDF exceeds the page limit")
        for number, page in enumerate(reader.pages, 1):
            locator = "page:" + str(number)
            text = page.extract_text() or ""
            add(locator, text)
            if not text.strip():
                warnings.append(
                    {"locator": locator, "kind": "no_extractable_text", "next": "Inspect for scans; OCR did not run"}
                )
        extractor = {"name": "pypdf", "version": importlib.metadata.version("pypdf"), "options": {"strict": True}}
    elif suffix == ".docx":
        _archive_bounds(payload)
        document = library("docx").Document(io.BytesIO(payload))
        for number, paragraph in enumerate(document.paragraphs, 1):
            add("paragraph:" + str(number), paragraph.text)
        for table_number, table in enumerate(document.tables, 1):
            for row_number, row in enumerate(table.rows, 1):
                for cell_number, cell in enumerate(row.cells, 1):
                    add(f"table:{table_number}/row:{row_number}/cell:{cell_number}", cell.text)
        extractor = {
            "name": "python-docx",
            "version": importlib.metadata.version("python-docx"),
            "options": {"scope": "body_paragraphs_and_tables"},
        }
    elif suffix == ".xlsx":
        _archive_bounds(payload)
        workbook = library("openpyxl").load_workbook(
            io.BytesIO(payload), read_only=True, data_only=False, keep_links=False
        )
        try:
            visited = 0
            for sheet in workbook:
                if (sheet.max_row or 0) * (sheet.max_column or 0) > MAX_SEGMENTS:
                    raise ArtifactError("Worksheet dimensions exceed the cell limit")
                for row in sheet.iter_rows():
                    for cell in row:
                        visited += 1
                        if visited > MAX_SEGMENTS:
                            raise ArtifactError("Workbook exceeds the cell limit")
                        if cell.value is not None:
                            text = (
                                cell.value.isoformat()
                                if isinstance(cell.value, (date, datetime, time))
                                else str(cell.value)
                            )
                            add("sheet:" + sheet.title + "!" + cell.coordinate, text)
        finally:
            workbook.close()
        extractor = {
            "name": "openpyxl",
            "version": importlib.metadata.version("openpyxl"),
            "options": {"data_only": False, "scope": "nonempty_cells", "formulas": "stored_text_not_evaluated"},
        }
    else:
        raise ArtifactError("Supported source suffixes are " + ", ".join(sorted(MEDIA)))
    if not segments:
        warnings.append(
            {"locator": "source", "kind": "no_extractable_text", "next": "Inspect the source before citing it"}
        )
    return {
        "schema_version": 1,
        "kind": "source_extraction",
        "source_sha256": hashlib.sha256(payload).hexdigest(),
        "extractor": extractor,
        "segments": segments,
        "warnings": warnings,
    }


def capture(spec: Path, out: Path) -> dict[str, Any]:
    """Copy one bounded local source and its reproducible extraction to a new directory."""
    spec, out = Path(spec), Path(out)
    value = _read_json(spec)
    request(value, {"file", "origin", "source_id"}, {"file", "origin"})
    source = contained(spec.parent, _string(value["file"], "file"))
    suffix = source.suffix.lower()
    if suffix not in MEDIA:
        raise ArtifactError("Unsupported source suffix: " + suffix)
    payload = _read_regular(source, MAX_SOURCE_BYTES)
    sha = hashlib.sha256(payload).hexdigest()
    source_id = _identifier(value.get("source_id", "sha256:" + sha), "source_id")
    origin = _origin(value["origin"])
    extraction = _extract(payload, suffix, source_id)
    extraction_bytes = _json_bytes(extraction)
    if len(extraction_bytes) > MAX_METADATA_BYTES:
        raise ArtifactError("Serialized extraction exceeds the metadata byte limit")
    index = {
        "schema_version": 1,
        "kind": "source_capture",
        "source": {
            "id": source_id,
            "file": "source" + suffix,
            "sha256": sha,
            "size": len(payload),
            "media_type": MEDIA[suffix],
            "origin": origin,
            "retrieval_verified": False,
        },
        "extraction": {"file": "extraction.json", "sha256": hashlib.sha256(extraction_bytes).hexdigest()},
    }
    with output_directory(out) as staged:
        (staged / ("source" + suffix)).write_bytes(payload)
        (staged / "extraction.json").write_bytes(extraction_bytes)
        (staged / "index.json").write_bytes(_json_bytes(index))
    return report(
        out / "index.json",
        "application/json",
        source_id=source_id,
        segments=len(extraction["segments"]),
        retrieval_verified=False,
        extraction_review="not_performed",
        warnings=extraction["warnings"],
    )


def _load_capture(root: Path, reference: Any, out: Path) -> tuple[str, dict[str, str], dict[str, Any]]:
    reference = _object(reference, {"path", "sha256"}, {"path", "sha256"}, "source reference")
    index_path = contained(root, _string(reference["path"], "source.path"))
    if out.resolve().is_relative_to(index_path.parent.resolve()):
        raise ArtifactError("Claim output must be outside captured source directories")
    index_bytes = _read_regular(index_path, MAX_METADATA_BYTES)
    if hashlib.sha256(index_bytes).hexdigest() != _sha(reference["sha256"]):
        raise ArtifactError("Source index SHA-256 mismatch")
    index = json.loads(index_bytes, parse_constant=_invalid_constant)
    index = _object(
        index,
        {"schema_version", "kind", "source", "extraction"},
        {"schema_version", "kind", "source", "extraction"},
        "capture index",
    )
    request(index, {"kind", "source", "extraction"}, {"kind", "source", "extraction"})
    if index["kind"] != "source_capture":
        raise ArtifactError("Expected a source_capture index")
    source = _object(
        index["source"],
        {"id", "file", "sha256", "size", "media_type", "origin", "retrieval_verified"},
        {"id", "file", "sha256", "size", "media_type", "origin", "retrieval_verified"},
        "captured source",
    )
    source_id = _identifier(source["id"], "source.id")
    _origin(source["origin"])
    if source["retrieval_verified"] is not False:
        raise ArtifactError("Captured retrieval metadata is unverified")
    path = contained(index_path.parent, _string(source["file"], "source.file"))
    if source["file"] != "source" + path.suffix.lower() or path.suffix.lower() not in MEDIA:
        raise ArtifactError("Captured source path or suffix is invalid")
    payload = _read_regular(path, MAX_SOURCE_BYTES)
    if hashlib.sha256(payload).hexdigest() != _sha(source["sha256"]):
        raise ArtifactError("Captured source SHA-256 mismatch")
    if (
        type(source["size"]) is not int
        or source["size"] != len(payload)
        or source["media_type"] != MEDIA[path.suffix.lower()]
    ):
        raise ArtifactError("Captured source metadata differs from its bytes")
    extraction_ref = _object(index["extraction"], {"file", "sha256"}, {"file", "sha256"}, "extraction reference")
    if extraction_ref["file"] != "extraction.json":
        raise ArtifactError("Captured extraction must be extraction.json")
    extraction_path = contained(index_path.parent, extraction_ref["file"])
    extraction_bytes = _read_regular(extraction_path, MAX_METADATA_BYTES)
    if hashlib.sha256(extraction_bytes).hexdigest() != _sha(extraction_ref["sha256"]):
        raise ArtifactError("Extraction SHA-256 mismatch")
    extraction = json.loads(extraction_bytes, parse_constant=_invalid_constant)
    reproduced = _extract(payload, path.suffix.lower(), source_id)
    if extraction != reproduced:
        raise ArtifactError("Extraction does not reproduce from the captured source with the installed extractor")
    segments = {segment["locator"]: segment["text"] for segment in reproduced["segments"]}
    return (
        source_id,
        segments,
        {
            "source_id": source_id,
            "index_path": reference["path"],
            "index_sha256": reference["sha256"],
            "source_sha256": source["sha256"],
            "extraction_sha256": extraction_ref["sha256"],
            "origin": source["origin"],
            "retrieval_verified": False,
            "extractor": reproduced["extractor"],
            "warnings": reproduced["warnings"],
        },
    )


def verify_claims(spec: Path, out: Path) -> dict[str, Any]:
    """Check digest-bound quotations at exact locators, without assessing entailment or truth."""
    spec, out = Path(spec), Path(out)
    if out.resolve() == spec.resolve():
        raise ArtifactError("Claim output must preserve its input spec")
    value = _read_json(spec)
    request(value, {"sources", "claims"}, {"sources", "claims"})
    if not isinstance(value["sources"], list) or not 1 <= len(value["sources"]) <= MAX_SOURCES:
        raise ArtifactError("sources needs between 1 and " + str(MAX_SOURCES) + " capture references")
    if not isinstance(value["claims"], list) or not 1 <= len(value["claims"]) <= MAX_CLAIMS:
        raise ArtifactError("claims needs between 1 and " + str(MAX_CLAIMS) + " entries")
    sources: dict[str, dict[str, str]] = {}
    source_records = []
    for source_ref in value["sources"]:
        source_id, segments, record = _load_capture(spec.parent, source_ref, out)
        if source_id in sources:
            raise ArtifactError("Duplicate source id: " + source_id)
        sources[source_id] = segments
        source_records.append(record)
    claims, seen = [], set()
    for claim in value["claims"]:
        claim = _object(claim, {"id", "text", "evidence"}, {"id", "text", "evidence"}, "claim")
        claim_id = _identifier(claim["id"], "claim.id")
        if claim_id in seen:
            raise ArtifactError("Duplicate claim id: " + claim_id)
        seen.add(claim_id)
        text = _string(claim["text"], "claim.text", 100_000)
        if not isinstance(claim["evidence"], list) or not 1 <= len(claim["evidence"]) <= 32:
            raise ArtifactError("Each claim needs between 1 and 32 evidence entries")
        checks = []
        for citation in claim["evidence"]:
            citation = _object(
                citation, {"source_id", "locator", "quote"}, {"source_id", "locator", "quote"}, "evidence"
            )
            source_id = _identifier(citation["source_id"], "evidence.source_id")
            locator = _string(citation["locator"], "evidence.locator")
            quote = _string(citation["quote"], "evidence.quote", 100_000)
            source_segments = sources.get(source_id, {})
            reason = (
                "unknown_source"
                if source_id not in sources
                else "unknown_locator"
                if locator not in source_segments
                else "quote_missing"
                if quote not in source_segments[locator]
                else None
            )
            checks.append(
                {
                    "source_id": source_id,
                    "locator": locator,
                    "quote": quote,
                    "status": "failed" if reason else "passed",
                    "reason": reason,
                }
            )
        claims.append(
            {
                "id": claim_id,
                "text": text,
                "citation_integrity": "passed" if all(check["status"] == "passed" for check in checks) else "failed",
                "semantic_review": "not_performed",
                "evidence": checks,
            }
        )
    passed = all(claim["citation_integrity"] == "passed" for claim in claims)
    result = {
        "schema_version": 1,
        "kind": "claim_integrity",
        "status": "passed" if passed else "failed",
        "citation_integrity": "passed" if passed else "failed",
        "semantic_review": "not_performed",
        "retrieval_verified": False,
        "sources": source_records,
        "claims": claims,
    }
    with output_file(out) as staged:
        staged.write_bytes(_json_bytes(result))
    return {
        **report(out, "application/json"),
        "status": result["status"],
        "citation_integrity": result["citation_integrity"],
        "semantic_review": "not_performed",
        "retrieval_verified": False,
        "claims": len(claims),
    }
