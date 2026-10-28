#!/usr/bin/env python3
"""Regenerate dist's workflow and require package smoke tests before publication."""

import argparse
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def require_smoke(text: str) -> str:
    """Gate dist's host job on the custom smoke job, including its result."""
    start = text.index("\n  host:\n")
    end = text.index("\n  custom-release-smoke:\n", start)
    host = text[start:end]
    dependency = "      - custom-release-extras\n"
    if "      - custom-release-smoke\n" not in host:
        host = host.replace(dependency, dependency + "      - custom-release-smoke\n", 1)
    condition = "needs.plan.result == 'success'"
    guarded = condition + " && needs.custom-release-smoke.result == 'success'"
    if guarded not in host:
        host = host.replace(condition, guarded, 1)
    if "      - custom-release-smoke\n" not in host or guarded not in host:
        raise ValueError("dist host job changed; review the release smoke dependency")
    return text[:start] + host + text[end:]


def rehearsals(text: str) -> str:
    """Preserve tag drafts and complete branch/PR rehearsals without external writes."""
    text = text.replace('  "contents": "write"', '  "contents": "read"', 1)
    text = text.replace("      - '**[0-9]+.[0-9]+.[0-9]+*'", "      - 'v[0-9]+.[0-9]+.[0-9]+*'")
    trigger = "\n  workflow_dispatch:\n"
    if trigger not in text:
        where = text.index("\njobs:\n")
        text = (
            text[:where]
            + """
  workflow_dispatch:
    inputs:
      draft:
        description: Draft a release only when running on a version tag
        type: boolean
        default: false
"""
            + text[where:]
        )
    if "  pull_request:\n    paths:" not in text:
        text = text.replace(
            "  pull_request:\n",
            """  pull_request:
    paths:
      - .github/workflows/release*.yml
      - .github/workflows/agy-image.yml
      - .github/dist-build-setup.yml
      - dist-workspace.toml
      - install.sh
      - packaging/**
      - sdk/python/pyproject.toml
      - tools/package.py
      - tools/release*.py
      - tools/homebrew_formula.py
""",
            1,
        )
    text = text.replace(
        "${{ !github.event.pull_request && github.ref_name || '' }}",
        "${{ github.ref_type == 'tag' && github.ref_name || '' }}",
    )
    text = text.replace(
        "${{ !github.event.pull_request && format('--tag={0}', github.ref_name) || '' }}",
        "${{ github.ref_type == 'tag' && format('--tag={0}', github.ref_name) || '' }}",
    )
    text = text.replace(
        "publishing: ${{ !github.event.pull_request }}",
        "publishing: ${{ github.ref_type == 'tag' && (github.event_name == 'push' || inputs.draft == true) }}",
    )
    lines = text.splitlines(keepends=True)
    for index, line in enumerate(lines):
        if line.startswith("          dist ${{") and "> plan-dist-manifest.json" in line:
            lines[index] = """          if [ "$GITHUB_REF_TYPE" = tag ]; then
            python3 tools/release_version.py --tag "$GITHUB_REF_NAME"
            dist plan --tag "$GITHUB_REF_NAME" --output-format=json > plan-dist-manifest.json
          else
            python3 tools/release_version.py
            dist plan --output-format=json > plan-dist-manifest.json
          fi
"""
    text = "".join(lines)
    text = text.replace("  host:\n    needs:", "  host:\n    permissions:\n      contents: write\n    needs:", 1)
    attest = "      - name: Attest\n"
    if attest + "        if:" not in text and "        id: attest\n" not in text:
        text = text.replace(attest, attest + "        if: needs.plan.outputs.publishing == 'true'\n", 1)
    if "      - name: Retry attestation\n" not in text:
        start = text.index("      - name: Attest\n")
        end = text.index("      - id: cargo-dist\n", start)
        original = text[start:end]
        first = original.replace(
            "      - name: Attest\n",
            "      - name: Attest\n        id: attest\n        continue-on-error: true\n",
            1,
        )
        retry = original.replace("name: Attest", "name: Retry attestation", 1).replace(
            "if: needs.plan.outputs.publishing == 'true'",
            "if: needs.plan.outputs.publishing == 'true' && steps.attest.outcome == 'failure'",
            1,
        )
        text = text[:start] + first + retry + text[end:]
    if "--steps=create --steps=upload --steps=release" not in text:
        text = text.replace("--steps=upload --steps=release", "--steps=create --steps=upload --steps=release")
    image_start = text.index("\n  custom-agy-image:\n")
    image_end = text.index("\n  # Build and package", image_start)
    image = text[image_start:image_end]
    if "      publish:" not in image:
        image = image.replace(
            "    with:\n", "    with:\n      publish: ${{ needs.plan.outputs.publishing == 'true' }}\n", 1
        )
    text = text[:image_start] + image + text[image_end:]
    text = text.replace(
        'gh release create "${{ needs.plan.outputs.tag }}" --target',
        'gh release create "${{ needs.plan.outputs.tag }}" --draft --verify-tag --target',
    )
    if "\n  rehearsal:\n" not in text:
        text += """

  rehearsal:
    needs:
      - plan
      - build-local-artifacts
      - custom-agy-image
      - build-global-artifacts
      - custom-release-extras
      - custom-release-smoke
    if: needs.plan.outputs.publishing != 'true'
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333
        with:
          pattern: artifacts-*
          path: release
          merge-multiple: true
      - name: Remove granular build manifests
        run: rm -f release/*-dist-manifest.json
      - uses: actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9
        with:
          name: release
          path: release/*
          if-no-files-found: error
"""
    return text


def main() -> None:
    """Generate pinned dist configuration and apply the publication guard."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", default="dist", help="pinned dist executable")
    parser.add_argument("--check", action="store_true", help="check the publication guard without generating")
    args = parser.parse_args()
    path = ROOT / ".github/workflows/release.yml"
    if not args.check:
        config = ROOT / "dist-workspace.toml"
        original = config.read_text()
        try:
            config.write_text(original.replace('allow-dirty = ["ci"]', "allow-dirty = []"))
            subprocess.run([args.dist, "generate", "--mode", "ci"], cwd=ROOT, check=True)
        finally:
            config.write_text(original)
    text = path.read_text()
    guarded = rehearsals(require_smoke(text))
    if args.check:
        if guarded != text:
            raise SystemExit("release workflow must require smoke success before publication")
    else:
        path.write_text(guarded)


if __name__ == "__main__":
    main()
