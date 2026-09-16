#!/usr/bin/env python3
"""Check local Markdown file targets in first-party documentation. No network checks."""
import re
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]


def main():
    files = sorted(ROOT.glob("*.md")) + sorted((ROOT / "docs").rglob("*.md"))
    errors = []
    for source in files:
        body = re.sub(r"(?ms)^(```|~~~).*?^\1[^\n]*$", "", source.read_text())
        for target in re.findall(r"\[[^\]]*\]\(([^)\s]+)\)", body):
            url = urlsplit(target)
            if url.scheme or url.netloc or not url.path:
                continue
            path = (source.parent / unquote(url.path)).resolve()
            if not path.is_relative_to(ROOT) or not path.exists():
                errors.append(f"{source.relative_to(ROOT)} -> {target}")
    if errors:
        raise SystemExit("Broken local documentation links:\n" + "\n".join(errors))
    print(f"Verified local file links in {len(files)} documentation files (anchors and external URLs excluded).")


if __name__ == "__main__":
    main()
