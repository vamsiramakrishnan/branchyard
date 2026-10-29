# Mermaid diagram recipe

Author an editable source file, for example:

```mermaid
flowchart LR
  S[Source snapshots] --> A[Analysis]
  A --> D[Draft]
  D --> R[Evidence and layout review]
```

Save it as workflow.mmd. If Mermaid CLI is already installed, render with:

```sh
mmdc -i workflow.mmd -o workflow.svg
```

Use the installed renderer's help for supported export formats. If mmdc is missing, keep the editable source and report that rendering is unavailable. Install the official Mermaid CLI explicitly when the task calls for it; do not build another renderer.

Retain the .mmd file, inspect the actual SVG or PDF, and provide a short text description of the relationships.

Primary references: [Mermaid syntax](https://mermaid.js.org/intro/) and [Mermaid CLI](https://github.com/mermaid-js/mermaid-cli).
