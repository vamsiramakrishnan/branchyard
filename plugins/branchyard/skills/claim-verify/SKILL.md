---
name: claim-verify
description: "Check source hashes, reproduced extraction anchors and exact quotations while keeping factual support and semantic review explicit."
---
# Verify claim evidence

Run `knowledge claim-verify claims.json --out outputs/claim-checks.json` through artifact-tools. The request names captured source indexes with their expected SHA-256 values, and claims with IDs, text and evidence references. Each reference identifies a source, locator and exact quotation.

The tool checks captured bytes, extraction hashes and reproduced source segments before quotations at their locators. A changed source, missing anchor or missing quote fails the evidence check.

Then assess whether the quotation supports the claim, contrary evidence exists, and conclusions stay within the source's scope. Record reasons and locations. Semantic review remains `not_performed` in the mechanical report; a quotation match does not establish truth or causality. Read [evidence appraisal](references/evidence-review.md) when judging support.
