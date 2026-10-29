# Branchyard Artifact Preview Component Integration

`BranchyardArtifactPreview` is a framework-free drop-in DOM component for the Branchyard review pane that renders rich document previews (`csv`, `xlsx`, `docx`, `pptx`, `pdf`, images, and text).

## Files

- `crates/branchyard-server/src/companion/assets/artifact-preview.js`: Defines global `window.BranchyardArtifactPreview`.
- `crates/branchyard-server/src/companion/assets/artifact-preview.css`: Styles for the preview component adhering to Branchyard design system CSS variables and dark/light modes.
- `crates/branchyard-server/tests/artifact_preview_browser.js`: Deterministic Playwright hermetic test suite verifying inert rendering, pagination, multi-sheet selection, page navigation, asset fetching, 202/429 retry polling, and URL revocation.

## Usage Example

```javascript
// Container element in the review pane
const container = document.getElementById('review-pane-preview-container');

// Mount preview component
const preview = window.BranchyardArtifactPreview.mount(container, {
  // Base URL for artifact API route (real Branchyard prefix /v1, no trailing slash)
  baseUrl: '/v1/workspaces/ws_123/artifacts/art_456',
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

## Security & Architecture Contract

- **No Tokens Stored**: Component never accepts or stores credentials. All authenticated network requests are delegated through `options.fetchAuthenticated`.
- **Derived Endpoints Only**: All calls derive from `baseUrl` (e.g. `/v1/workspaces/{workspace_id}/artifacts/{artifact_id}`):
  - `GET {baseUrl}/preview?view=auto|table|pages&page=1&sheet=NAME&offset=0&limit=100`
  - `GET {baseUrl}/preview/assets/{encodeURIComponent(image)}{previewQuery}`
  - `GET {baseUrl}/content` (for original document download)
- **Safe Rendering**:
  - Spreadsheet / CSV cells are rendered with strict `textContent` preventing formula or HTML injection.
  - Page visual previews require safe basenames and `image/png` content types; rendered via temporary Blob URLs.
  - All Object URLs are tracked and revoked on refresh or dispose.
- **Race Condition & Timer Safety**:
  - `AbortController` aborts pending requests on view changes, navigation, or disposal.
  - Announcements use `queueMicrotask` instead of timers to avoid deferred execution races.
  - Pending 202 / 429 (`preview_busy`) `Retry-After` retry timers and delayed download URL revocation timers are tracked and cancelled on disposal or superseded requests.

## Test Invocation

Run the deterministic Playwright test suite using the browser and playwright module environment variables:

```bash
PLAYWRIGHT_BROWSERS_PATH=/tmp/branchyard-product-polish-20261029/ux/browsers \
BY_TEST_PLAYWRIGHT=/tmp/branchyard-product-polish-20261029/ux/browser/node_modules/playwright \
node crates/branchyard-server/tests/artifact_preview_browser.js
```

## Status

Evidence: hermetic (isolated Playwright headless browser test suite running against intercepted route handlers; deterministic clock and promise control, zero fixed sleeps or live model/server calls).

Not done: real-server/UI integration wiring (static asset endpoint registration in `crates/branchyard-server` and mounting inside companion web console `app.js`/`index.html` are owned by the parent UI branch).
