---
name: google-docs
description: "Create, read and update native Google Docs through an authorized connector, checking actual document identity and revisions."
---
# Google Docs

Read the shared `artifact-tools` connector guidance and the granted connector index. Discover the actual Docs create, read, update and export operations and their argument schemas. Work through the granted SDK/account so Google's OAuth scopes and document permissions remain authoritative.

For updates, read the target document, identify the requested content, and use revision guards when supported. Apply only the authorized change, then reread to verify it. Preserve unrelated content and sharing. Return the observed document ID, URL and revision when provided; report exports separately from the native document.

If the connector lacks a needed operation, account consent or permission, name that prerequisite and preserve prepared content. A DOCX file is an alternative deliverable only when the user accepts it; it is not a native Google Doc.
