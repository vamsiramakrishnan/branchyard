---
name: google-slides
description: "Create, inspect and update native Google Slides through a granted connector while preserving presentation identity, layouts and permissions."
---
# Google Slides

Read the shared `artifact-tools` connector guidance and the granted connector index. Discover actual presentation, slide/page-element, revision and export operations. Use the authorized connector account; keep native OAuth scopes and presentation permissions effective.

Read the presentation and layout/object IDs before an update. Preserve stable IDs and unrelated slides, use supported revision guards, then reread affected content and render/export when available. Verify chart editability and linked-data behavior when requested; a screenshot is not an editable chart.

For parallel work, use a shared outline and independent section specifications from `parallel-deck`, then let one assembler write the native presentation through supported connector operations. Do not assume PPTX import or batch update exists. Return the observed presentation ID, URL, revision and verification evidence; report unavailable operations/consent plainly.
