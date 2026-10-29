---
name: calculate
description: "Evaluate reproducible arithmetic with named quantities, explicit units and assumptions using Pint, recording inputs and results."
---
# Calculate

Run `knowledge calculate calculation.json --out outputs/calculation.json` through artifact-tools. Read the request example linked from this folder's index. Named variables carry numeric values and units; the expression and target unit are explicit.

The convenience command supports arithmetic and parentheses. Pint owns parsing and dimensional conversion. For functions or complex models use native Pint/Python, retaining the script. Never use Python eval for an expression.

Separate observed inputs from estimates, percentages from fractions, and released capacity from cash savings. Currency basis is an explicit assumption; unit labels supply no exchange rates or inflation adjustments.

Check dimensions, signs, denominators and magnitude against an independently worked example. Record uncertainty or scenarios when needed. [Unit guidance](references/units.md) covers the relevant upstream principles.
