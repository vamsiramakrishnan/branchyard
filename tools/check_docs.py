#!/usr/bin/env python3
"""Check first-party documentation: local Markdown file targets, the `#anchor` of each link to a
Markdown file, and dates that run forwards.

An anchor is a heading's GitHub-style slug (lower case, punctuation dropped, spaces to hyphens,
`-1`, `-2` for a repeated heading) or an HTML `id`/`name` written in the page. A link such as
`testing-live.md#6-agent-substrate-cluster` fails when no heading on that page has the slug;
rename the heading or the link, never both blindly.

A page that opens with a date line ("Written 30 September 2026 ...", "Prepared
... and updated ...") must not carry a dated section newer than that line: a
`**Status, 3 October 2026.**` paragraph, a "Written ..." or "Set ..." line, or
a table row whose result cell starts with a date. Update the opening line when
you add a newer one. No network checks.
"""

import re
from datetime import date
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
MONTHS = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
]
MONTH_NAMES = "|".join(MONTHS)
DATE = re.compile(rf"\b(\d{{1,2}}) ({MONTH_NAMES}) (\d{{4}})\b")
FRONT = re.compile(r"(Written|Prepared|Updated|Last updated|Set|Research baseline:?) ")
SECTION = re.compile(rf"(\*\*Status|Written |Set |\| [^|]*\| *(\*\*)?\d{{1,2}} ({MONTH_NAMES}) \d{{4}})")


FENCE = re.compile(r"(?ms)^(```|~~~).*?^\1[^\n]*$")
HEADING = re.compile(r"^ {0,3}(#{1,6})[ \t]+(.*?)(?:[ \t]+#+)?[ \t]*$", re.M)
HTML_ID = re.compile(r"""<[a-zA-Z][^>]*?\b(?:id|name)\s*=\s*["']([^"']+)["']""")


def slug(heading):
    """GitHub's anchor for a heading: its text without inline Markdown, lower case, only letters,
    digits, `_` and `-`, each space a hyphen."""
    text = re.sub(r"!?\[([^\]]*)\]\([^)]*\)", r"\1", heading)
    text = re.sub(r"<[^>]*>", "", text).replace("`", "").replace("*", "")
    text = re.sub(r"(?<!\w)_|_(?!\w)", "", text).lower()
    return re.sub(r"[^\w\- ]", "", text).replace(" ", "-")


def anchors(text):
    """Every anchor a page offers: its heading slugs (a repeat gets -1, -2, ...) and HTML ids."""
    body = FENCE.sub("", text)
    found, seen = set(HTML_ID.findall(body)), {}
    for _, heading in HEADING.findall(body):
        base = slug(heading)
        count = seen.get(base, 0)
        seen[base] = count + 1
        found.add(base if count == 0 else f"{base}-{count}")
    return found


def dates(text):
    return [date(int(y), MONTHS.index(m) + 1, int(d)) for d, m, y in DATE.findall(text)]


def date_problems(source, text):
    """The opening date line is no older than any dated section below it."""
    lines = text.splitlines()
    front = next((i for i, line in enumerate(lines) if line.strip() and not line.startswith("#")), None)
    if front is None or not FRONT.match(lines[front]):
        return []
    opening = dates(lines[front])
    if not opening:
        return []
    problems = []
    for number, line in enumerate(lines[front + 1 :], front + 2):
        if not SECTION.match(line):
            continue
        # A table row's date opens its second cell; otherwise look near the start.
        cell = line.split("|")[2] if line.startswith("|") and line.count("|") > 2 else line
        found = dates(cell[:100])
        if found and found[0] > max(opening):
            problems.append(
                f"{source.relative_to(ROOT)}:{number}: dated {found[0]:%d %B %Y}, "
                f"after the opening line's {max(opening):%d %B %Y}; update the opening line"
            )
    return problems


def link_problems(source, text, offered):
    """Each local link in `text` names a file that exists and, for a Markdown file, an anchor it offers.

    `offered` caches the anchors of each page read, by path."""
    problems = []
    body = FENCE.sub("", text)
    for target in re.findall(r"\[[^\]]*\]\(([^)\s]+)\)", body):
        url = urlsplit(target)
        if url.scheme or url.netloc:
            continue
        # A link of "#anchor" alone names this page.
        path = (source.parent / unquote(url.path)).resolve() if url.path else source
        if not path.is_relative_to(ROOT) or not path.exists():
            problems.append(f"{source.relative_to(ROOT)} -> {target}")
        elif url.fragment and path.suffix == ".md" and path.is_file():
            if path not in offered:
                offered[path] = anchors(path.read_text())
            if unquote(url.fragment).lower() not in offered[path]:
                problems.append(f"{source.relative_to(ROOT)} -> {target} (no heading with that anchor)")
    return problems


def main():
    files = sorted(ROOT.glob("*.md")) + sorted((ROOT / "docs").rglob("*.md"))
    errors, offered = [], {}
    for source in files:
        text = source.read_text()
        errors += date_problems(source, text)
        errors += link_problems(source, text, offered)
    if errors:
        raise SystemExit(
            "Documentation problems (broken local links or anchors, or dates out of order):\n" + "\n".join(errors)
        )
    print(
        f"Verified local file links, heading anchors and date order in {len(files)} documentation files "
        "(external URLs excluded)."
    )


if __name__ == "__main__":
    main()
