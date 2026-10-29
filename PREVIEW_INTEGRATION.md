# Branchyard Artifact Preview Component Integration

`BranchyardArtifactPreview` is a framework-free drop-in DOM component for the Branchyard review pane that renders rich document previews (`csv`, `xlsx`, `docx`, `pptx`, `pdf`, images, and text).

## Files

- `artifact-preview.js`: Defines global `window.BranchyardArtifactPreview`.
- `artifact-preview.css`: Styles for the preview component adhering to Branchyard design system CSS variables and dark/light modes.
- `test-preview-component.js`: Standalone Playwright hermetic test suite verifying inert rendering, pagination, multi-sheet selection, page navigation, asset fetching, and URL revocation.

## Usage Example

```javascript
// Container element in the review pane
const container = document.getElementById('review-pane-preview-container');

// Mount preview component
const preview = window.BranchyardArtifactPreview.mount(container, {
  // Base URL for artifact API route (no trailing slash)
  baseUrl: '/api/v1/workspaces/ws_123/artifacts/art_456',
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
- **Derived Endpoints Only**: All calls derive from `baseUrl`:
  - `GET {baseUrl}/preview?view=auto|table|pages&page=1&sheet=NAME&offset=0&limit=100`
  - `GET {baseUrl}/preview/assets/{encodeURIComponent(image)}{previewQuery}`
  - `GET {baseUrl}/content` (for original document download)
- **Safe Rendering**:
  - Spreadsheet / CSV cells are rendered with strict `textContent` preventing formula or HTML injection.
  - Page visual previews require safe basenames and `image/png` content types; rendered via temporary Blob URLs.
  - All Object URLs are tracked and revoked on refresh or dispose.
- **Race Condition & Timer Safety**:
  - `AbortController` aborts pending requests on view changes, navigation, or disposal.
  - Bounded 202 `Retry-After` retry timers are cleared on dispose or when newer requests are triggered.
