#!/usr/bin/env python3
"""Check first-party documentation: local Markdown file targets, and dates that run forwards.

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
MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August",
          "September", "October", "November", "December"]
DATE = re.compile(r"\b(\d{1,2}) (%s) (\d{4})\b" % "|".join(MONTHS))
FRONT = re.compile(r"(Written|Prepared|Updated|Last updated|Set) ")
SECTION = re.compile(r"(\*\*Status|Written |Set |\| [^|]*\| *(\*\*)?\d{1,2} (%s) \d{4})" % "|".join(MONTHS))


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
    for number, line in enumerate(lines[front + 1:], front + 2):
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


def main():
    files = sorted(ROOT.glob("*.md")) + sorted((ROOT / "docs").rglob("*.md"))
    errors = []
    for source in files:
        errors += date_problems(source, source.read_text())
        body = re.sub(r"(?ms)^(```|~~~).*?^\1[^\n]*$", "", source.read_text())
        for target in re.findall(r"\[[^\]]*\]\(([^)\s]+)\)", body):
            url = urlsplit(target)
            if url.scheme or url.netloc or not url.path:
                continue
            path = (source.parent / unquote(url.path)).resolve()
            if not path.is_relative_to(ROOT) or not path.exists():
                errors.append(f"{source.relative_to(ROOT)} -> {target}")
    if errors:
        raise SystemExit("Documentation problems (broken local links, or dates out of order):\n" + "\n".join(errors))
    print(f"Verified local file links and date order in {len(files)} documentation files (anchors and external URLs excluded).")


if __name__ == "__main__":
    main()
