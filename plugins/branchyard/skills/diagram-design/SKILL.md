---
name: diagram-design
description: "Create editable process, relationship and system diagrams using Mermaid or established drawing tools."
---
# Design a diagram

Choose notation by meaning: flowchart for decisions, sequence for interactions, timeline for milestones, dependency graph for prerequisites. Define what boxes, arrows and grouping mean.

Use Mermaid for text-authored diagrams when appropriate. Keep the editable .mmd source with rendered output. Read [the diagram recipe](references/diagrams.md) for a renderer command. Check whether that established renderer is installed; report an unavailable renderer instead of inventing an export pipeline or silently installing one.

For other layouts use existing drawing tools or editable slide shapes. Reuse a supplied visual style. Label proposed versus observed behavior. Review missing paths, arrow direction, ownership and label readability, then inspect the actual render and supply a textual description.
