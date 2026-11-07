#!/usr/bin/env python3
"""Find a skill, one of its references, a connector operation or a helper in this turn's tool tree.

    tools search WORDS... [--plain]   rank everything in the tree against the words (BM25)
    tools show ID                     print one skill, reference, connector, operation or helper
    tools schema CONNECTOR.OP         print one operation's input schema

The tree is the directory this script's `bin/` sits in: `INDEX.md`, `skills/`,
`connectors/` and, when helpers were chosen, `helpers/INDEX.md`. Pass `--root DIR`
or set BRANCHYARD_TOOLS_DIR (or BRANCHYARD_TOOLS, which a sandbox sets to /tools) to read
another. Standard library only.

Every document is built from the tree when a command runs; nothing is written beside a skill.
A skill is one document (its id, frontmatter `keywords`, its description and its headings,
weighted as FIELD_WEIGHTS says) and each of its `references/*.md` is another (the skill's and the
file's name, its first heading, the content words of its body), so a search can land on one
reference without the skill. A reference adds nothing to the corpus statistics, so the references
a tree carries never reorder the tools among themselves. Ids are
`[kind:][source:]name[/references/FILE.md]`: `pptx`, `repository:tone`,
`pptx/references/layouts.md`, `skill:pptx`. A tool `INDEX.md` lists (chosen for the turn) scores
CHOSEN times one it does not, and a reference is scored on its words alone; `--plain` ranks by
BM25 alone.

A connector package with a `search.json` (Anvil's operation search index, which `anvil
package harness` writes; docs/anvil-contract-v2.md item 3) is read from it, and strictly: a malformed one is refused
with a message that names the file and the field, none of that connector's operations are
searched, and the command exits 3 after answering with everything else. Only a package
without `search.json` is read tolerantly, from `harness.json`, `compose.json`, `schemas/`
and `reference/`.

Exit status: 0 found, 1 nothing found or nothing recorded, 2 bad usage, 3 a `search.json`
was refused.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import sys
from pathlib import Path
from typing import Any, NamedTuple

K1 = 1.5
B = 0.75
SNIPPET = 110
#: How many times each field of a skill or reference document counts (docs/skill-registry.md, section 3;
#: docs/skill-eval.md records the measurements behind the numbers). A connector, operation or helper
#: document has its id (counted as `id`) and one `text`.
FIELD_WEIGHTS = {"id": 4, "keywords": 3, "description": 1, "headings": 1, "title": 1, "body": 1}
#: The score of a chosen tool (a skill or connector `INDEX.md` lists, the connector's operations, or a
#: helper) is multiplied by this unless `--plain`; a reference is scored on its words alone.
CHOSEN = 1.5
#: The most characters of a document's first paragraph that are shown as its summary.
SUMMARY_CAP = 600
#: English function words left out of a reference's indexed body, so a paragraph is found by what it
#: says. A skill's own document (its id, description and headings) is indexed whole, as before.
STOPWORDS = frozenset(
    [
        "a",
        "an",
        "and",
        "are",
        "as",
        "at",
        "be",
        "but",
        "by",
        "can",
        "do",
        "for",
        "from",
        "has",
        "have",
        "how",
        "i",
        "if",
        "in",
        "into",
        "is",
        "it",
        "its",
        "me",
        "my",
        "no",
        "not",
        "of",
        "on",
        "or",
        "our",
        "so",
        "than",
        "that",
        "the",
        "their",
        "them",
        "then",
        "there",
        "these",
        "they",
        "this",
        "to",
        "up",
        "us",
        "we",
        "what",
        "when",
        "where",
        "which",
        "who",
        "will",
        "with",
        "would",
        "you",
        "your",
    ]
)
REFERENCES = "references"
KINDS = ("skill", "reference", "connector", "operation", "helper")
OP_ID = re.compile(r"[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+")
HEADING_OP = re.compile(r"^#{2,5}\s+`?(" + OP_ID.pattern + r")`?\s*(?:[-:—]\s*(.*))?$")
TABLE_OP = re.compile(r"^\|\s*`?(" + OP_ID.pattern + r")`?\s*\|\s*([^|]*)\|")
LIST_ITEM = re.compile(r"^[-*]\s+`?([^`:]+?)`?\s*(?:\([^)]*\))?\s*:\s*(.+)$")
SCHEMA_KEYS = ("input_schema", "inputSchema", "schema", "parameters")

# The operation search index of docs/anvil-contract-v2.md, item 3 ("anvil.search/v1"), as Anvil writes it.
SEARCH_FILE = "search.json"
SEARCH_SCHEMA = "anvil.search/v1"
INDEX_SCHEMA = "anvil-search-v1.json"
EFFECTS = ("read", "reversible", "compensable", "irreversible")
IDEMPOTENCY = ("safe", "keyed", "none")
SUMMARY_MAX = 160
SLUG = re.compile(r"^[a-z0-9]+(-[a-z0-9]+)*$")
BUNDLE_HASH = re.compile(r"^sha256:[0-9a-f]{64}$")
SEGMENT = r"[A-Za-z0-9_-]+"
OPERATION_FIELDS: dict[str, Any] = {
    "id": str,
    "title": str,
    "summary": str,
    "effect": str,
    "risk": (str, type(None)),
    "confirm": bool,
    "idempotency": str,
    "resource": str,
    "tags": list,
    "terms": list,
    "schema": str,
    "cli": str,
}


class SearchIndexError(Exception):
    """A package's `search.json` is not the contract's: the message says which file and field."""


class Doc(NamedTuple):
    """One thing that can be found: its kind, id, one-line description and where it lives.

    `fields` are the weighted fields of a skill or reference document, `(name, text)` with
    the names of FIELD_WEIGHTS; a document without them is ranked on its id and `text`.
    `skill` is the skill a skill or reference document belongs to.
    """

    kind: str
    id: str
    description: str
    path: Path | None
    text: str
    connector: str = ""
    op: str = ""
    fields: tuple[tuple[str, str], ...] = ()
    skill: str = ""


def read(path: Path) -> str:
    """The file's text, or empty when it is missing or unreadable."""
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""


def frontmatter(text: str) -> dict[str, str]:
    """`name`, `description` and `keywords` from a SKILL.md's leading YAML block.

    Values may be plain, quoted or folded; `keywords` may be a flow list (`[a, b]`), a block
    list (`- a` lines) or words, and comes back as one space-separated string.
    """
    lines = text.lstrip("﻿").split("\n")
    if not lines or lines[0].strip() != "---":
        return {}
    found: dict[str, str] = {}
    key = ""
    for line in lines[1:]:
        if line.strip() in ("---", "..."):
            break
        match = re.match(r"^([A-Za-z_-]+):\s*(.*)$", line)
        if match and not line.startswith(" "):
            key, value = match.group(1), match.group(2).strip()
            if value[:1] in ('"', "'"):
                try:
                    value = json.loads(value) if value[0] == '"' else value.strip("'")
                except ValueError:
                    value = value.strip("\"'")
            elif value in (">", "|", ">-", "|-"):
                value = ""
            found[key] = str(value)
        elif key and (line.startswith(" ") or line.startswith("- ")) and line.strip():
            found[key] = (found.get(key, "") + " " + line.strip().removeprefix("- ")).strip()
    out = {k: " ".join(v.split()) for k, v in found.items() if k in ("name", "description", "keywords")}
    if "keywords" in out:
        out["keywords"] = " ".join(re.findall(r"[A-Za-z0-9_-]+", out["keywords"]))
    return out


def body_of(text: str) -> str:
    """The markdown after a leading frontmatter block, or the whole text without one."""
    text = text.lstrip("﻿")
    if text.startswith("---\n") and "\n---\n" in text:
        return text.split("\n---\n", 1)[1]
    return text


def headings(text: str) -> str:
    """The markdown headings of a body, which name what a skill covers."""
    return " ".join(line.lstrip("# ").strip() for line in body_of(text).split("\n") if line.startswith("#"))


def title_of(text: str, fallback: str) -> str:
    """The first heading of a document, or `fallback` (a file stem, read as words) without one."""
    for line in body_of(text).split("\n"):
        if line.startswith("#"):
            return line.lstrip("# ").strip()
    return fallback.replace("-", " ").replace("_", " ")


def summary_of(text: str) -> str:
    """The first paragraph after the title: prose, not a heading, list, table, rule or code fence."""
    paragraph: list[str] = []
    for line in [*body_of(text).split("\n"), ""]:
        stripped = line.strip()
        if stripped and not paragraph and (stripped[0] in "#-*|>`" or re.match(r"\d+\.\s", stripped)):
            continue
        if stripped:
            paragraph.append(stripped)
        elif paragraph:
            break
    summary = " ".join(paragraph)
    return summary if len(summary) <= SUMMARY_CAP else summary[: SUMMARY_CAP - 3].rstrip() + "..."


def content_words(text: str) -> str:
    """The words of a reference's body worth indexing: what it says, without code fences, link targets,
    the lines that only cite where the text came from (`Source:`, `License:`) and STOPWORDS."""
    body = re.sub(r"```.*?```", " ", body_of(text), flags=re.S)
    body = "\n".join(line for line in body.split("\n") if not line.startswith(("Source:", "License:")))
    body = re.sub(r"\(https?://[^)\s]*\)|https?://\S+", " ", body)
    return " ".join(word for word in tokens(body) if word not in STOPWORDS)


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def directory(root: Path, name: str, fallback: str) -> Path:
    """`root/name`, or the sibling the engine staged it in when the link is missing."""
    first = root / name
    return first if first.is_dir() else root.parent / fallback


def skill_docs(root: Path) -> list[Doc]:
    base = directory(root, "skills", "capabilities")
    docs: list[Doc] = []
    if not base.is_dir():
        return docs
    for child in sorted(base.iterdir()):
        if child.name == "topics" or not child.is_dir():
            continue
        found = [(child, child.name)] if (child / "SKILL.md").is_file() else []
        if not found:
            found = [(c, f"{child.name}:{c.name}") for c in sorted(child.iterdir()) if (c / "SKILL.md").is_file()]
        for directory_, label in found:
            text = read(directory_ / "SKILL.md")
            meta = frontmatter(text)
            description = meta.get("description", "")
            name = meta.get("name") or directory_.name
            fields = (
                ("id", label),
                ("keywords", meta.get("keywords", "")),
                ("description", description),
                ("headings", headings(text)),
            )
            docs.append(
                Doc(
                    "skill",
                    label,
                    description,
                    directory_ / "SKILL.md",
                    f"{label} {description} {headings(text)}",
                    fields=fields,
                    skill=label,
                )
            )
            docs.extend(reference_docs(directory_, label, name))
    return docs


def reference_docs(skill_dir: Path, label: str, name: str) -> list[Doc]:
    """One document per `references/*.md` of a skill: `LABEL/references/FILE.md`, indexed by the skill's name
    and the file's, its first heading and the content words of its body; shown with its first paragraph."""
    base = skill_dir / REFERENCES
    if not base.is_dir():
        return []
    docs: list[Doc] = []
    for file in sorted(base.glob("*.md")):
        if not file.is_file():
            continue
        text = read(file)
        title = title_of(text, file.stem)
        # A reference that carries frontmatter (a vendored skill) says what it is for there.
        summary = frontmatter(text).get("description") or summary_of(text)
        ident = f"{label}/{REFERENCES}/{file.name}"
        fields = (("id", f"{name} {file.stem}"), ("title", title), ("body", content_words(text)))
        description = f"{title}. {summary}" if summary else title
        docs.append(Doc("reference", ident, description, file, f"{title} {summary}", fields=fields, skill=label))
    return docs


def operations_from_json(data: Any) -> list[tuple[str, str, Any]]:
    """(operation id, description, schema) from a harness.json or compose.json of unknown shape."""
    out: list[tuple[str, str, Any]] = []
    if isinstance(data, dict):
        for key in ("operations", "ops", "tools", "catalog"):
            if key in data:
                return operations_from_json(data[key])
        return out
    if isinstance(data, dict) or not isinstance(data, list):
        return out
    for item in data:
        if isinstance(item, dict):
            ident = item.get("id") or item.get("operation") or item.get("name")
            if isinstance(ident, str):
                description = item.get("description") or item.get("summary") or ""
                schema = next((item[k] for k in SCHEMA_KEYS if k in item), None)
                out.append((ident, str(description), schema))
        elif isinstance(item, str):
            out.append((item, "", None))
    return out


def operations_from_mapping(data: Any) -> list[tuple[str, str, Any]]:
    """The same, for `{"operations": {"issues.list": "List issues" | {...}}}`."""
    if not isinstance(data, dict):
        return []
    for key in ("operations", "ops", "tools", "catalog"):
        inner = data.get(key)
        if isinstance(inner, dict):
            out = []
            for ident, value in inner.items():
                if isinstance(value, dict):
                    description = value.get("description") or value.get("summary") or ""
                    schema = next((value[k] for k in SCHEMA_KEYS if k in value), None)
                    out.append((ident, str(description), schema))
                else:
                    out.append((ident, str(value or ""), None))
            return out
    return operations_from_json(data)


def validate_search(data: Any, package: Path, connector: str) -> list[dict[str, Any]]:
    """The operations of a `search.json`, or SearchIndexError naming the first field that is wrong.

    Unknown fields are ignored (a later Anvil may add some); every field the contract lists
    must be there, of its type and in its range. Operations are unique and sorted by id.
    """
    label = f"{package.name}/{SEARCH_FILE}"

    def bad(where: str, why: str) -> SearchIndexError:
        return SearchIndexError(f"{label}: {where}: {why}")

    if not isinstance(data, dict):
        raise bad("the file", "must be a JSON object")
    if data.get("schema") != SEARCH_SCHEMA:
        raise bad("schema", f"is {data.get('schema')!r}; this reader understands {SEARCH_SCHEMA!r}")
    if data.get("connector") != connector:
        raise bad("connector", f"is {data.get('connector')!r} but the package directory is {connector!r}")
    if not isinstance(data.get("bundle_hash"), str) or not BUNDLE_HASH.match(data["bundle_hash"]):
        raise bad("bundle_hash", "must be 'sha256:' and 64 lower-case hex digits")
    if "icon" not in data:
        raise bad("icon", "is missing (use null for none)")
    icon = data["icon"]
    if icon is not None and (not isinstance(icon, str) or len(icon) > 64 or not SLUG.match(icon)):
        raise bad("icon", "must be null or a lower-case slug of at most 64 characters")
    listed = data.get("operations")
    if not isinstance(listed, list):
        raise bad("operations", "must be a list")
    inside = package.resolve()
    id_pattern = re.compile(rf"^{re.escape(connector)}(\.{SEGMENT})+$")
    previous = ""
    out: list[dict[str, Any]] = []
    for index, op in enumerate(listed):
        at = f"operations[{index}]"
        if not isinstance(op, dict):
            raise bad(at, "must be an object")
        for field, kind in OPERATION_FIELDS.items():
            if field not in op:
                raise bad(f"{at}.{field}", "is missing")
            value = op[field]
            if not isinstance(value, kind) or (kind is str and not value.strip()):
                what = "a non-empty string" if kind is str else "of the type the contract gives it"
                raise bad(f"{at}.{field}", f"must be {what}")
        ident = op["id"]
        if not id_pattern.match(ident):
            raise bad(f"{at}.id", f"{ident!r} must be '{connector}.' and dotted names of letters, digits, _ and -")
        if ident <= previous:
            raise bad(f"{at}.id", f"{ident!r} is repeated or out of order; operations are unique and sorted by id")
        previous = ident
        if "\n" in op["summary"] or len(op["summary"]) > SUMMARY_MAX:
            raise bad(f"{at}.summary", f"must be one line of at most {SUMMARY_MAX} characters")
        if op["effect"] not in EFFECTS:
            raise bad(f"{at}.effect", f"{op['effect']!r} is not one of {', '.join(EFFECTS)}")
        if op["idempotency"] not in IDEMPOTENCY:
            raise bad(f"{at}.idempotency", f"{op['idempotency']!r} is not one of {', '.join(IDEMPOTENCY)}")
        if op["risk"] is not None and not op["risk"].strip():
            raise bad(f"{at}.risk", "must be null or a non-empty string")
        for field in ("tags", "terms"):
            if not all(isinstance(item, str) and item.strip() for item in op[field]):
                raise bad(f"{at}.{field}", "must be a list of non-empty strings")
        if not all(re.fullmatch(r"[a-z0-9]+", item) for item in op["terms"]):
            raise bad(f"{at}.terms", "must be lower-case words of letters and digits")
        relative = Path(op["schema"])
        target = (inside / relative).resolve()
        if relative.is_absolute() or ".." in relative.parts or inside not in target.parents:
            raise bad(f"{at}.schema", f"{op['schema']!r} must be a path inside the package")
        if not target.is_file():
            raise bad(f"{at}.schema", f"{op['schema']!r} does not exist in the package")
        out.append(op)
    return out


def search_index(package: Path, connector: str) -> list[dict[str, Any]] | None:
    """The package's validated `search.json` operations, or None when it has no such file."""
    path = package / SEARCH_FILE
    if not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise SearchIndexError(f"{package.name}/{SEARCH_FILE}: cannot be read as JSON: {error}") from error
    return validate_search(data, package, connector)


def operations(package: Path, connector: str) -> list[tuple[str, str, Any, str]]:
    """Every operation a connector package declares: (id, description, schema, where found)."""
    found: dict[str, tuple[str, Any, str]] = {}

    def add(ident: str, description: str, schema: Any, source: str) -> None:
        ident = ident.removeprefix(connector + ".") if connector else ident
        old = found.get(ident)
        if old is None or (not old[0] and description) or (old[1] is None and schema is not None):
            found[ident] = (
                description or (old[0] if old else ""),
                schema if schema is not None else (old[1] if old else None),
                source,
            )

    for name in ("harness.json", "compose.json"):
        for ident, description, schema in operations_from_mapping(load_json(package / name)):
            add(ident, description, schema, name)
    schemas = package / "schemas"
    if schemas.is_dir():
        for file in sorted(schemas.glob("*.json")):
            if file.name == INDEX_SCHEMA or file.name.endswith(".schema.json"):
                continue  # the index's own schema; Anvil's per-tool schemas, which are named by tool
            data = load_json(file)
            description = ""
            if isinstance(data, dict):
                description = str(data.get("description") or data.get("title") or "")
            add(file.stem.removesuffix(".input"), description, data, f"schemas/{file.name}")
    reference = package / "reference"
    if reference.is_dir():
        for file in sorted(reference.rglob("*.md")):
            for line in read(file).split("\n"):
                match = HEADING_OP.match(line.strip()) or TABLE_OP.match(line.strip())
                if match:
                    add(
                        match.group(1), (match.group(2) or "").strip(), None, f"reference/{file.relative_to(reference)}"
                    )
    return [(ident, d, s, src) for ident, (d, s, src) in sorted(found.items())]


def index_description(index: str, name: str) -> str:
    """What `INDEX.md` says of a connector: its `## id - title` block's lines, else its one-line entry."""
    lines = index.split("\n")
    for number, text in enumerate(lines):
        heading = re.match(rf"^##\s+{re.escape(name)}\s+[—-]\s+(.*)$", text.strip())
        if heading:
            block = []
            for follow in lines[number + 1 :]:
                if follow.startswith("#"):
                    break
                if follow.startswith("- **For:**") or follow.startswith("- **Use when:**"):
                    block.append(follow.split(":**", 1)[1].strip())
            return " ".join([heading.group(1).strip(), *block])
    line = next((ln for ln in lines if re.search(rf"\b{re.escape(name)}\b", ln)), "")
    return line.lstrip("-* ").strip()


def connector_docs(root: Path, refused: list[str] | None = None) -> list[Doc]:
    base = directory(root, "connectors", "connectors")
    docs: list[Doc] = []
    if not base.is_dir():
        return docs
    index = read(base / "INDEX.md")
    for package in sorted(base.iterdir()):
        if not package.is_dir() or package.name.startswith("."):
            continue
        name = package.name
        text = read(package / "SKILL.md")
        description = frontmatter(text).get("description", "")
        if not description:
            description = index_description(index, name)
        docs.append(
            Doc(
                "connector",
                name,
                description,
                package / "SKILL.md",
                f"{name} {description} {headings(text)}",
                connector=name,
            )
        )
        try:
            indexed = search_index(package, name)
        except SearchIndexError as error:
            if refused is not None:
                refused.append(str(error))
            continue
        if indexed is not None:
            for op in indexed:
                ident = op["id"].removeprefix(name + ".")
                text = " ".join(
                    [op["id"], ident.replace(".", " "), op["title"], op["summary"], op["resource"], op["effect"]]
                    + op["tags"]
                    + op["terms"]
                )
                docs.append(Doc("operation", op["id"], op["summary"], package, text, connector=name, op=ident))
            continue
        for ident, op_description, _, _ in operations(package, name):
            full = f"{name}.{ident}"
            docs.append(
                Doc(
                    "operation",
                    full,
                    op_description,
                    package,
                    f"{full} {ident.replace('.', ' ')} {op_description}",
                    connector=name,
                    op=ident,
                )
            )
    return docs


def helper_docs(root: Path) -> list[Doc]:
    path = root / "helpers" / "INDEX.md"
    docs = []
    for line in read(path).split("\n"):
        match = LIST_ITEM.match(line.strip())
        if match:
            docs.append(
                Doc(
                    "helper", match.group(1).strip(), match.group(2).strip(), path, f"{match.group(1)} {match.group(2)}"
                )
            )
    return docs


def documents(root: Path, refused: list[str] | None = None) -> list[Doc]:
    """Everything findable in the tree; `refused` collects the messages of refused `search.json` files."""
    return skill_docs(root) + connector_docs(root, refused) + helper_docs(root)


def tokens(text: str) -> list[str]:
    return re.findall(r"[a-z0-9]+", text.lower())


def bag(doc: Doc) -> list[str]:
    """A document's terms, each field repeated FIELD_WEIGHTS times; without fields, the id (less once, since
    the text opens with it) and the text."""
    if not doc.fields:
        return tokens(doc.id) * (FIELD_WEIGHTS["id"] - 1) + tokens(doc.text)
    out: list[str] = []
    for name, text in doc.fields:
        out += tokens(text) * FIELD_WEIGHTS.get(name, 1)
    return out


def chosen_tools(root: Path) -> frozenset[str]:
    """The skills and connectors `INDEX.md` lists: what was chosen for the turn. Behind an overflow line
    (`- ...N more`) the list is cut, not the choice, so every tool in the tree counts as chosen (`*`)."""
    listed: set[str] = set()
    inside = False
    for line in read(root / "INDEX.md").split("\n"):
        if line.startswith("## "):
            inside = line.startswith("## Skills") or line.startswith("## Connectors")
            continue
        if inside and line.startswith("- "):
            name = line[2:].split(":", 1)[0].strip()
            if name.startswith("..."):
                return frozenset({"*"})
            listed.add(name)
    return frozenset(listed)


def is_chosen(doc: Doc, chosen: frozenset[str]) -> bool:
    """A skill, a connector and its operations, when `INDEX.md` lists the tool; a helper always; never a
    reference, which is scored on its words alone so a tool of equal match is read first."""
    if doc.kind == "reference":
        return False
    if doc.kind == "helper" or "*" in chosen:
        return True
    return (doc.connector or doc.id) in chosen


def rank(docs: list[Doc], words: list[str], chosen: frozenset[str] = frozenset()) -> list[tuple[float, Doc]]:
    """BM25 of the words against each document's weighted fields; a chosen tool's score times CHOSEN.

    The corpus statistics (term rarity, average length) are those of the tools: skills, connectors,
    operations and helpers. A reference is scored against them and adds nothing to them, so the
    references a tree carries never reorder the tools among themselves.
    """
    corpus = [bag(d) for d in docs]
    tools = [terms for doc, terms in zip(docs, corpus, strict=True) if doc.kind != "reference"] or corpus
    if not corpus:
        return []
    average = sum(len(c) for c in tools) / len(tools) or 1.0
    query = list(dict.fromkeys(tokens(" ".join(words))))
    scored = []
    for doc, terms in zip(docs, corpus, strict=True):
        score = 0.0
        for word in query:
            frequency = terms.count(word)
            if not frequency:
                continue
            holding = sum(1 for c in tools if word in c)
            idf = math.log(1 + (len(tools) - holding + 0.5) / (holding + 0.5))
            score += idf * frequency * (K1 + 1) / (frequency + K1 * (1 - B + B * len(terms) / average))
        if score > 0:
            if chosen and is_chosen(doc, chosen):
                score *= CHOSEN
            scored.append((score, doc))
    scored.sort(key=lambda pair: (-pair[0], pair[1].kind, pair[1].id))
    return scored


def line(doc: Doc) -> str:
    description = (
        doc.description if len(doc.description) <= SNIPPET else doc.description[: SNIPPET - 3].rstrip() + "..."
    )
    return f"{doc.kind:<10} {doc.id}" + (f": {description}" if description else "")


def report(refused: list[str]) -> int:
    """Say each refused `search.json` on stderr; the exit status when there were any, else 0."""
    for message in refused:
        print(f"tools: refused {message}; none of that connector's operations are searched", file=sys.stderr)
    return 3 if refused else 0


def search(root: Path, words: list[str], limit: int, kind: str | None, plain: bool = False) -> int:
    refused: list[str] = []
    docs = [d for d in documents(root, refused) if kind in (None, d.kind)]
    if not docs:
        print(f"nothing to search: {root} holds no skills, connectors or helpers", file=sys.stderr)
        return report(refused) or 1
    hits = rank(docs, words, frozenset() if plain else chosen_tools(root))[:limit]
    if not hits:
        print(f"no tool matches {' '.join(words)!r}; read {root / 'INDEX.md'} for the whole list", file=sys.stderr)
        return report(refused) or 1
    for _, doc in hits:
        print(line(doc))
    return report(refused)


def find(docs: list[Doc], ident: str) -> Doc | None:
    """The document an id names: `[kind:][source:]name[/references/FILE.md]`, or CONNECTOR.OP, or OP alone."""
    kind, _, rest = ident.partition(":")
    if rest and kind in KINDS:
        if kind == "skill" and f"/{REFERENCES}/" in rest:
            kind = "reference"  # `skill:pptx/references/layouts.md` is the skill's reference
        return next((d for d in docs if d.kind == kind and d.id == rest), None)
    for wanted in KINDS:
        for doc in docs:
            if doc.kind == wanted and doc.id == ident:
                return doc
    # An operation without its connector, when exactly one connector has it.
    named = [d for d in docs if d.kind == "operation" and d.op == ident]
    return named[0] if len(named) == 1 else None


def show(root: Path, ident: str) -> int:
    refused: list[str] = []
    docs = documents(root, refused)
    doc = find(docs, ident)
    if doc is None:
        near = [d.id for _, d in rank(docs, [ident])[:5]]
        hint = f"; closest: {', '.join(near)}" if near else ""
        print(f"no tool {ident!r} here{hint}", file=sys.stderr)
        return report(refused) or 1
    if doc.kind in ("skill", "reference") and doc.path is not None:
        print(f"# {doc.path}\n")
        print(read(doc.path).rstrip())
    elif doc.kind == "connector":
        print(f"connector {doc.id}: {doc.description}".rstrip())
        print(
            f"read {doc.path} for how to call it; `{doc.path.parent / 'bin' / doc.id}` is its CLI" if doc.path else ""
        )
        listed = [d for d in docs if d.kind == "operation" and d.connector == doc.id]
        for op in listed[:40]:
            print(f"  {op.op}" + (f": {op.description}" if op.description else ""))
        if len(listed) > 40:
            print(f"  ... {len(listed) - 40} more: tools search {doc.id} WORDS")
        if not listed:
            print("  (no operations are listed in this package; its SKILL.md and `--help` are the reference)")
    elif doc.kind == "operation":
        print(f"{doc.id}: {doc.description}".rstrip())
        print(f"schema: tools schema {doc.id}")
    else:
        print(f"helper {doc.id}: {doc.description}")
    return report(refused)


def schema(root: Path, spec: str) -> int:
    connector, _, op = spec.partition(".")
    base = directory(root, "connectors", "connectors")
    package = base / connector
    if not op or not package.is_dir():
        known = (
            sorted(p.name for p in base.iterdir() if p.is_dir() and not p.name.startswith(".")) if base.is_dir() else []
        )
        print(
            f"usage: tools schema CONNECTOR.OPERATION; connectors here: {', '.join(known) or 'none'}", file=sys.stderr
        )
        return 1
    try:
        indexed = search_index(package, connector)
    except SearchIndexError as error:
        return report([str(error)])
    if indexed is not None:
        entry = next((o for o in indexed if o["id"] == f"{connector}.{op}"), None)
        if entry is None:
            print(
                f"no such operation {connector}.{op} in {package.name}/{SEARCH_FILE}; "
                f"`tools search {connector} WORDS` finds one",
                file=sys.stderr,
            )
            return 1
        data = load_json(package / entry["schema"])
        if data is None:
            return report([f"{package.name}/{entry['schema']}: is not JSON"])
        print(json.dumps(data, indent=2))
        return 0
    for ident, _, value, _source in operations(package, connector):
        if ident == op and value is not None:
            print(json.dumps(value, indent=2))
            return 0
    candidates = [
        package / "schemas" / f"{name}.json"
        for name in (op, f"{connector}.{op}", op.replace(".", "_"), op.replace(".", "-"))
    ]
    for file in candidates:
        data = load_json(file)
        if data is not None:
            print(json.dumps(data, indent=2))
            return 0
    declared = any(ident == op for ident, _, _, _ in operations(package, connector))
    cli = package / "bin" / connector
    print(
        f"no schema recorded for {connector}.{op} in this package"
        + ("" if declared else " (and no such operation is listed)")
        + f"; ask the connector: `{cli} --schema` (add the operation name if it takes one)",
        file=sys.stderr,
    )
    return 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="tools", description=(__doc__ or "").split("\n")[0])
    parser.add_argument("--root", help="the tool tree (default: the directory above this script's bin/)")
    sub = parser.add_subparsers(dest="command", required=True)
    finder = sub.add_parser("search", help="rank skills, their references, connector operations and helpers")
    finder.add_argument("words", nargs="+")
    finder.add_argument("--limit", type=int, default=8)
    finder.add_argument("--kind", choices=list(KINDS))
    finder.add_argument("--plain", action="store_true", help="BM25 alone: no extra weight for a chosen tool")
    shower = sub.add_parser("show", help="print one tool: a skill, SKILL/references/FILE.md, a connector, CONNECTOR.OP")
    shower.add_argument("id")
    schemer = sub.add_parser("schema", help="print one operation's input schema")
    schemer.add_argument("operation", metavar="CONNECTOR.OP")
    args = parser.parse_args(argv)
    chosen = args.root or os.environ.get("BRANCHYARD_TOOLS_DIR") or os.environ.get("BRANCHYARD_TOOLS")
    root = Path(chosen) if chosen else Path(__file__).resolve().parent.parent
    if args.command == "search":
        return search(root, args.words, max(1, args.limit), args.kind, args.plain)
    if args.command == "show":
        return show(root, args.id)
    return schema(root, args.operation)


if __name__ == "__main__":
    sys.exit(main())
