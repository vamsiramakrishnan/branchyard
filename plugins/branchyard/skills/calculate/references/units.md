# Units and assumptions

Adapted from K-Dense's uncertainty-and-units skill. Changes: bounded arithmetic and units guidance for Branchyard; metrology-specific CLIs and Python 3.12 dependency snapshots omitted.

Attach units to inputs and retain them through the calculation. Convert at an explicit boundary. State the model and corrections before computing the result.

A unit-consistent result can still be implausible. Check its order of magnitude, reference period and denominator against the source. State the meaning of each percentage and rate.

If inputs have uncertainty, record how it was estimated and which inputs are correlated. Report the meaning of any interval or plus/minus value. Do not imply that this convenience calculation propagates uncertainty or supplies current exchange rates.

Pint performs parsing and conversion. Use native Python/Pint for a larger model and retain the script, assumptions and scenarios.

Source: https://github.com/K-Dense-AI/scientific-agent-skills/blob/5f2bc0341354fd5590aef76a533ada1e1e5b4e62/skills/uncertainty-and-units/SKILL.md
License: [MIT](LICENSE.k-dense.txt).
