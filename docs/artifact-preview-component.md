# Branchyard Artifact Preview Component Integration

`BranchyardArtifactPreview` is a framework-free drop-in DOM component for the Branchyard review pane that renders rich document previews (`csv`, `xlsx`, `docx`, `pptx`, `pdf`, images, and text).

## Files

- `crates/branchyard-server/src/companion/assets/artifact-preview.js`: Defines global `window.BranchyardArtifactPreview`.
- `crates/branchyard-server/src/companion/assets/artifact-preview.css`: Styles for the preview component adhering to Branchyard design system CSS variables and dark/light modes.
- `crates/branchyard-server/tests/artifact_preview_browser.js`: Deterministic Playwright hermetic test suite verifying inert rendering, pagination, multi-sheet selection, page navigation, asset fetching, each failure envelope's state, `preview_busy` retry polling, and URL revocation.

## Usage Example

```javascript
// Container element in the review pane
const container = document.getElementById('review-pane-preview-container');

// Mount preview component
const preview = window.BranchyardArtifactPreview.mount(container, {
  // Base URL of the original's route (real Branchyard prefix /v1, no trailing slash):
  // a workspace artifact, or a source at /v1/workspaces/ws_123/sources/src_789
  baseUrl: '/v1/workspaces/ws_123/artifacts/art_456',
  // Optional: shown in the toolbar; the console names the file in its own heading
  displayName: 'financial-analysis.xlsx',
  mediaType: 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet',
  // Parent UI provides existing credentialed fetch
  fetchAuthenticated: (path, init) => window.fetchWithAuth(path, init)
});

// To refresh document content preserving worksheet/page state:
// preview.refresh();

// When switching to a new artifact or closing pane:
// preview.dispose();
```

## In the console

The companion serves both files from `/app/` (the `ASSETS` table in `crates/branchyard-server/src/companion/mod.rs`), loads them from `index.html`, and keeps them in the service worker's offline shell. The page's policy allows `img-src 'self' blob:` for the component's page images; scripts and requests stay same-origin. A workspace's **Sources** and **Outputs** tabs mount the component under their list, with `baseUrl` `/v1/workspaces/{workspace}/sources/{source}` or `/v1/workspaces/{workspace}/artifacts/{artifact}`, and `fetchAuthenticated` adds the tab's bearer token to each request. The console names the file and offers **Download original** above the component, in every state. It keeps one live handle for sources and outputs together and disposes it when the route, the inspector tab, the file previewed or the source changes, and on sign-out; see [projects and workspaces](companion.md#projects-and-workspaces).

## Security & Architecture Contract

- **No Tokens Stored**: Component never accepts or stores credentials. All authenticated network requests are delegated through `options.fetchAuthenticated`.
- **Derived Endpoints Only**: All calls derive from `baseUrl` (e.g. `/v1/workspaces/{workspace_id}/artifacts/{artifact_id}`):
  - `GET {baseUrl}/preview?view=auto|table|pages&page=1&sheet=NAME&offset=0&limit=100`
  - `GET {baseUrl}/preview/assets/{encodeURIComponent(image)}{previewQuery}`
- **The host downloads the original**: the component draws previews only; the console's **Download original** reads `{baseUrl}/content`.
- **Safe Rendering**:
  - Spreadsheet / CSV cells are rendered with strict `textContent` preventing formula or HTML injection.
  - Page visual previews require safe basenames and `image/png` content types; rendered via temporary Blob URLs.
  - All Object URLs are tracked and revoked on refresh or dispose.
- **Race Condition & Timer Safety**:
  - `AbortController` aborts pending requests on view changes, navigation, or disposal.
  - Announcements use `queueMicrotask` instead of timers to avoid deferred execution races.
  - The `preview_busy` retry timer is tracked and cancelled on disposal or a superseding request.

## Answers and states

The component reads the server's answers as `crates/branchyard-server/src/preview/` gives them. A refusal is the JSON error envelope `{"error": {"code", "message", "detail"}}`; a conversion that ran but failed, or whose renderer lacks a component, is a `200` whose `status` is `failed` or `unavailable`, with `error.message` and `error.next`. Each has its own state, set as text only:

| Answer | State |
|---|---|
| `200`, `status: "passed"` | The table, page image or text, announced as "Preview ready" |
| `200`, `status: "failed"` | This file could not be converted for preview, with the server's message and next step |
| `200`, `status: "unavailable"`, or `503 preview_unavailable` | This server cannot make previews right now; the server's message and next step sit under **Details for the server's operator** |
| `422 preview_unsupported` | No preview for this format |
| `504 preview_timeout` | The preview took too long to make, with **Try again** |
| `413 preview_too_large` | This file is too large to preview, with the server's message |
| `422 preview_output_limit` | This preview is too large to show |
| `429 preview_busy` | Asks again after `error.detail.retry_after` seconds (1 to 10), up to five times, then says the server is busy making other previews, with **Try again** |
| `404` | This file, or this page of its preview, is no longer available |
| No answer | The server cannot be reached, with **Try again** |

Any other refusal says the preview could not be shown, with the server's message and **Try again**. The server never answers `202` and sends no `Retry-After` header for previews.

## Test Invocation

Run the deterministic Playwright test suite using the browser and playwright module environment variables:

```bash
PLAYWRIGHT_BROWSERS_PATH=/tmp/branchyard-product-polish-20261029/ux/browsers \
BY_TEST_PLAYWRIGHT=/tmp/branchyard-product-polish-20261029/ux/browser/node_modules/playwright \
node crates/branchyard-server/tests/artifact_preview_browser.js
```

## Status

Evidence: hermetic (isolated Playwright headless browser test suite running against intercepted route handlers; deterministic clock and promise control, zero fixed sleeps or live model/server calls).

Console integration, 29 October 2026: hermetic. `tests/console_workspace_browser.rs` mounts the component from the console against a real server on a combined tree (the foundation's commit `f51fc785` with the console branch on top), checks that it requests the workspace preview path for CSV, XLSX, DOCX and PPTX outputs, and that leaving the workspace or the tab aborts a preview still loading.

Not done then: a rendered preview inside the console. The server had no workspace preview route yet, so every preview request from the console was answered 404.

Failure envelopes, 29 October 2026: hermetic. `tests/artifact_preview_browser.js` answers with each envelope above, as `preview/mod.rs`, `preview/sandbox.rs` and the artifact helper write them, and checks each state, its live-region announcement, **Try again**, and the `preview_busy` retry limit under Playwright's clock. Not done: a real server answering `429`, `504` or `413` in a browser.

Console with the server's renderer, 29 October 2026: fake agents, and the server's own renderer on the test machine. `tests/console_workspace_browser.rs` previews sources and outputs without intercepting any preview answer. The guided previews status in [the companion](companion.md) says which formats rendered, under which renderer settings. Not done: a rendered DOCX, PPTX or PDF page in the console.

