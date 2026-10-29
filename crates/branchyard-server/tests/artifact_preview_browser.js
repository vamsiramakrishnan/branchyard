/**
 * artifact_preview_browser.js
 * Standalone Playwright hermetic test suite for BranchyardArtifactPreview component.
 * Uses intercepted route handlers to simulate backend API responses without model or server calls.
 * Deterministic testing: uses Playwright clock and explicit deferred promises instead of fixed sleeps.
 */

const fs = require('fs');
const path = require('path');
const assert = require('assert');

// Try finding playwright either via BY_TEST_PLAYWRIGHT / PLAYWRIGHT environment variables,
// standard require('playwright'), or fallback to the provided test environment path.
let playwright;
const playwrightCandidates = [
  process.env.BY_TEST_PLAYWRIGHT,
  process.env.PLAYWRIGHT,
  'playwright'
].filter(Boolean);

for (const p of playwrightCandidates) {
  try {
    playwright = require(p);
    break;
  } catch (e) {
    // continue search
  }
}

if (!playwright) {
  console.error('Playwright could not be resolved from environment or standard paths.');
  process.exit(1);
}

// Relative paths to component assets within the repository
const assetsDir = path.join(__dirname, '..', 'src', 'companion', 'assets');
const componentJs = fs.readFileSync(path.join(assetsDir, 'artifact-preview.js'), 'utf8');
const componentCss = fs.readFileSync(path.join(assetsDir, 'artifact-preview.css'), 'utf8');

// Minimal 1x1 transparent PNG bytes for mock page preview image
const samplePngBase64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==';
const samplePngBuffer = Buffer.from(samplePngBase64, 'base64');

// Helper to create a controlled deferred promise
function createDeferred() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

async function runTests() {
  console.log('Launching Chromium headless browser for hermetic preview tests...');
  const browser = await playwright.chromium.launch({
    headless: true,
    args: ['--no-sandbox', '--disable-setuid-sandbox']
  });

  const context = await browser.newContext();
  // Install Playwright clock to allow deterministic control over timers
  await context.clock.install();

  const page = await context.newPage();

  // Load hermetic test fixture HTML page
  await page.setContent(`
    <!DOCTYPE html>
    <html lang="en">
    <head>
      <meta charset="utf-8">
      <title>Hermetic Preview Component Test Fixture</title>
      <style>${componentCss}</style>
    </head>
    <body style="margin: 0; padding: 0; width: 800px; height: 600px;">
      <div id="preview-container" style="width: 100%; height: 100%;"></div>
      <script>${componentJs}</script>
    </body>
    </html>
  `);

  console.log('Fixture loaded. Running test cases...');

  // Track created Blob URLs to verify revocation
  await page.evaluate(() => {
    window.createdBlobUrls = [];
    window.revokedBlobUrls = [];
    const origCreate = URL.createObjectURL;
    const origRevoke = URL.revokeObjectURL;
    URL.createObjectURL = function (obj) {
      const url = origCreate.call(URL, obj);
      window.createdBlobUrls.push(url);
      return url;
    };
    URL.revokeObjectURL = function (url) {
      window.revokedBlobUrls.push(url);
      return origRevoke.call(URL, url);
    };
  });

  // Track authenticated fetch calls
  await page.evaluate(() => {
    window.fetchLogs = [];
    window.mockFetchAuthenticated = function (url, init) {
      window.fetchLogs.push({ url, init });
      return fetch(url, init);
    };
  });

  // ==========================================
  // Test 1: Malicious CSV cell rendering & inertness
  // ==========================================
  console.log('Test 1: Inert malicious CSV cell rendering...');
  await page.route('https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-1/preview*', (route) => {
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        status: 'passed',
        kind: 'table',
        format: 'csv',
        columns: ['ID', 'Payload', 'Formula'],
        rows: [
          [1, '<script>window.pwned=true</script><img src=x onerror="window.pwned=true">', '=1+1']
        ],
        warnings: ['Formula calculation disabled for security']
      })
    });
  });

  await page.evaluate(() => {
    const container = document.getElementById('preview-container');
    container.innerHTML = '';
    window.activePreview = window.BranchyardArtifactPreview.mount(container, {
      baseUrl: 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-1',
      displayName: 'data.csv',
      mediaType: 'text/csv',
      fetchAuthenticated: window.mockFetchAuthenticated
    });
  });

  // Verify cell content is textContent and script did NOT execute
  await page.waitForSelector('.by-preview-table');
  const pwned = await page.evaluate(() => window.pwned);
  assert.strictEqual(pwned, undefined, 'Malicious script executed! Vulnerable to XSS!');

  const cellText = await page.locator('.by-preview-table tbody td').nth(1).textContent();
  assert(cellText.includes('<script>window.pwned=true</script>'), 'Cell text should be inert string');

  const warningText = await page.locator('.by-preview-warning-banner').textContent();
  assert(warningText.includes('Formula calculation disabled'), 'Warning banner should display formula warning');
  console.log('  ✓ CSV inert rendering & formula warning passed.');

  // ==========================================
  // Test 2: Two-sheet XLSX selection and table pagination
  // ==========================================
  console.log('Test 2: Two-sheet XLSX selection and pagination...');
  await page.route('https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-2/preview*', (route) => {
    const url = new URL(route.request().url());
    const sheet = url.searchParams.get('sheet') || 'Summary';
    const offset = parseInt(url.searchParams.get('offset') || '0', 10);

    if (sheet === 'Summary') {
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          status: 'passed',
          kind: 'table',
          format: 'xlsx',
          sheets: ['Summary', 'Details'],
          sheet: 'Summary',
          columns: ['Metric', 'Value'],
          rows: [['Total Users', 100]],
          offset: 0,
          next_offset: null
        })
      });
    } else {
      // Details sheet with pagination
      const nextOff = offset === 0 ? 100 : null;
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          status: 'passed',
          kind: 'table',
          format: 'xlsx',
          sheets: ['Summary', 'Details'],
          sheet: 'Details',
          columns: ['Item', 'Amount'],
          rows: offset === 0 ? [['Item A', 10]] : [['Item B', 20]],
          offset: offset,
          next_offset: nextOff
        })
      });
    }
  });

  await page.evaluate(() => {
    const container = document.getElementById('preview-container');
    container.innerHTML = '';
    window.activePreview = window.BranchyardArtifactPreview.mount(container, {
      baseUrl: 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-2',
      displayName: 'financials.xlsx',
      mediaType: 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet',
      fetchAuthenticated: window.mockFetchAuthenticated
    });
  });

  await page.waitForSelector('.by-preview-select');
  const sheetOptions = await page.locator('.by-preview-select option').allTextContents();
  assert.deepStrictEqual(sheetOptions, ['Summary', 'Details'], 'Sheets selector options must match API');

  // Select 'Details' sheet
  await page.selectOption('.by-preview-select', 'Details');
  await page.waitForSelector('text=Item A');
  assert(await page.locator('.by-preview-table tbody').textContent().then(t => t.includes('Item A')));

  // Test pagination button
  const nextBtn = page.locator('button:has-text("Next 100 ▶")');
  assert(await nextBtn.isEnabled(), 'Next button should be enabled');
  await nextBtn.click();
  await page.waitForSelector('text=Item B');
  assert(await page.locator('.by-preview-table tbody').textContent().then(t => t.includes('Item B')));
  console.log('  ✓ XLSX sheet selection & table pagination passed.');

  // ==========================================
  // Test 3: Word/PPTX page navigation and safe PNG preview asset fetch with same query
  // ==========================================
  console.log('Test 3: DOCX/PPTX page navigation and same-query asset fetch...');
  let assetRequestedUrl = null;
  await page.route('https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-3/preview*', (route) => {
    const url = new URL(route.request().url());
    const pageNum = parseInt(url.searchParams.get('page') || '1', 10);
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        status: 'passed',
        kind: 'pages',
        format: 'pptx',
        page: pageNum,
        pages: 3,
        image: `slide_${pageNum}.png`,
        width: 800,
        height: 450
      })
    });
  });

  await page.route('https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-3/preview/assets/*', (route) => {
    assetRequestedUrl = route.request().url();
    route.fulfill({
      status: 200,
      contentType: 'image/png',
      body: samplePngBuffer
    });
  });

  await page.evaluate(() => {
    const container = document.getElementById('preview-container');
    container.innerHTML = '';
    window.activePreview = window.BranchyardArtifactPreview.mount(container, {
      baseUrl: 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-3',
      displayName: 'pitch.pptx',
      mediaType: 'application/vnd.openxmlformats-officedocument.presentationml.presentation',
      fetchAuthenticated: window.mockFetchAuthenticated
    });
  });

  await page.waitForSelector('.by-preview-page-img');
  assert(assetRequestedUrl.includes('/preview/assets/slide_1.png'), 'Asset path must be fetched');
  assert(assetRequestedUrl.includes('view=auto'), 'Asset fetch must retain same preview query params');

  const pageIndicator = await page.locator('.by-preview-page-indicator').textContent();
  assert.strictEqual(pageIndicator.trim(), '1 / 3');

  // Navigate to slide 2
  await page.locator('button:has-text("Next ▶")').click();
  await page.waitForSelector('img[alt="Page 2 of 3"]');
  assert(assetRequestedUrl.includes('/preview/assets/slide_2.png'), 'Slide 2 asset must be fetched');
  console.log('  ✓ Page navigation and same-query asset fetch passed.');

  // ==========================================
  // Test 4: The server's failure answers, each as a plain state
  // ==========================================
  console.log('Test 4: Server failure envelopes as plain states...');
  const envelope = (code, message, detail) => ({ error: Object.assign({ code, message }, detail ? { detail } : {}) });
  const failures = [
    {
      name: 'unsupported', status: 422,
      body: envelope('preview_unsupported', 'preview supports CSV, XLSX, DOCX, PPTX, PDF and raster images'),
      title: 'No preview for this format', shows: ['Download the original to open it on this device.'], retry: false
    },
    {
      name: 'renderer missing', status: 503,
      body: envelope('preview_unavailable', 'A preview executable is unavailable.', { status: 'unavailable', next: 'Configure the documented isolated preview backend.' }),
      title: 'This server cannot make previews right now', operator: ['A preview executable is unavailable.', 'Configure the documented isolated preview backend.'], retry: false
    },
    {
      name: 'renderer lacks a component', status: 200,
      body: { status: 'unavailable', error: { kind: 'unavailable', message: 'Missing pypdf', next: 'Run the doctor' } },
      title: 'This server cannot make previews right now', operator: ['Missing pypdf', 'Run the doctor'], retry: false
    },
    {
      name: 'conversion failed', status: 200,
      body: { status: 'failed', error: { kind: 'invalid_artifact', message: 'LibreOffice completed without producing the requested PDF', next: 'Inspect the input' } },
      title: 'This file could not be converted for preview', shows: ['LibreOffice completed without producing the requested PDF', 'Next: Inspect the input'], retry: false
    },
    {
      name: 'timeout', status: 504,
      body: envelope('preview_timeout', 'document conversion exceeded its time limit'),
      title: 'The preview took too long to make', shows: ['Try again, or download the original.'], retry: true
    },
    {
      name: 'too large', status: 413,
      body: envelope('preview_too_large', 'document previews accept at most 64 MiB'),
      title: 'This file is too large to preview', shows: ['document previews accept at most 64 MiB'], retry: false
    },
    {
      name: 'output limit', status: 422,
      body: envelope('preview_output_limit', 'document conversion exceeded its output bound or lost a pipe'),
      title: 'This preview is too large to show', retry: false
    },
    {
      name: 'gone', status: 404,
      body: envelope('not_found', 'no artifact art-4 in this workspace'),
      title: 'This file is no longer available', shows: ['no artifact art-4 in this workspace'], retry: false
    }
  ];
  const failureRoute = 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-4/preview*';
  for (const failure of failures) {
    await page.route(failureRoute, (route) => route.fulfill({ status: failure.status, contentType: 'application/json', body: JSON.stringify(failure.body) }));
    await page.evaluate(() => {
      const container = document.getElementById('preview-container');
      container.innerHTML = '';
      window.activePreview = window.BranchyardArtifactPreview.mount(container, {
        baseUrl: 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-4',
        mediaType: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
        fetchAuthenticated: window.mockFetchAuthenticated
      });
    });
    const state = page.locator('.by-preview-failure');
    await state.locator('.by-preview-state-message', { hasText: failure.title }).waitFor();
    for (const text of failure.shows || []) {
      assert(await state.getByText(text, { exact: true }).isVisible(), `${failure.name}: shows ${text}`);
    }
    const operator = state.locator('details.by-preview-operator');
    assert.strictEqual(await operator.count(), failure.operator ? 1 : 0, `${failure.name}: operator details`);
    if (failure.operator) {
      assert(!(await operator.getByText(failure.operator[0], { exact: true }).isVisible()), `${failure.name}: operator details start closed`);
      await operator.locator('summary').click();
      for (const text of failure.operator) {
        assert(await operator.getByText(text, { exact: true }).isVisible(), `${failure.name}: operator details show ${text}`);
      }
    }
    assert.strictEqual(await state.getByRole('button', { name: 'Try again' }).count(), failure.retry ? 1 : 0, `${failure.name}: Try again`);
    assert.strictEqual(await page.locator('.by-artifact-preview button', { hasText: /download/i }).count(), 0, `${failure.name}: the host owns the download`);
    await page.waitForFunction((title) => document.querySelector('.by-preview-sr-only[aria-live="polite"]').textContent === title, failure.title);
    await page.unroute(failureRoute);
  }

  await page.route(failureRoute, (route) => route.abort('failed'));
  await page.evaluate(() => window.activePreview.refresh());
  await page.locator('.by-preview-failure .by-preview-state-message', { hasText: 'The server cannot be reached' }).waitFor();
  await page.unroute(failureRoute);
  await page.route(failureRoute, (route) => route.fulfill({
    status: 200,
    contentType: 'application/json',
    body: JSON.stringify({ status: 'passed', kind: 'text', text: 'BACK AFTER TRY AGAIN' })
  }));
  await page.getByRole('button', { name: 'Try again' }).click();
  await page.waitForSelector('text=BACK AFTER TRY AGAIN');
  await page.waitForFunction(() => document.querySelector('.by-preview-sr-only[aria-live="polite"]').textContent === 'Preview ready.');
  await page.unroute(failureRoute);
  console.log('  ✓ Each failure envelope shows its own plain state, server words and Try again where it helps.');

  // ==========================================
  // Test 5: Rapid document switching & deterministic late-response rejection
  // ==========================================
  console.log('Test 5: Rapid document switching and deterministic stale-response rejection...');
  const slowDeferred = createDeferred();

  await page.route('https://api.branchyard.local/v1/workspaces/ws-1/artifacts/slow-doc/preview*', async (route) => {
    await slowDeferred.promise;
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        status: 'passed',
        kind: 'text',
        text: 'OLD SLOW CONTENT THAT SHOULD BE DROPPED'
      })
    });
  });

  await page.route('https://api.branchyard.local/v1/workspaces/ws-1/artifacts/fast-doc/preview*', (route) => {
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        status: 'passed',
        kind: 'text',
        text: 'NEW FAST CONTENT'
      })
    });
  });

  await page.evaluate(() => {
    const container = document.getElementById('preview-container');
    container.innerHTML = '';
    // Rapidly switch from slow-doc to fast-doc
    window.slowPreview = window.BranchyardArtifactPreview.mount(container, {
      baseUrl: 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/slow-doc',
      fetchAuthenticated: window.mockFetchAuthenticated
    });
    // Immediately mount fast-doc into the container, disposing the previous
    window.slowPreview.dispose();
    window.fastPreview = window.BranchyardArtifactPreview.mount(container, {
      baseUrl: 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/fast-doc',
      fetchAuthenticated: window.mockFetchAuthenticated
    });
  });

  // Fast content renders first
  await page.waitForSelector('.by-preview-text-content');
  const preText = await page.locator('.by-preview-text-content').textContent();
  assert.strictEqual(preText, 'NEW FAST CONTENT');

  // Now resolve the delayed slow request deterministically AFTER disposal
  slowDeferred.resolve();
  // Tick microtasks to allow any late promise chains to run
  await page.evaluate(() => Promise.resolve());

  const finalText = await page.locator('.by-preview-text-content').textContent();
  assert.strictEqual(finalText, 'NEW FAST CONTENT', 'Late resolved response must not overwrite current content');
  console.log('  ✓ Rapid document switching & deterministic stale response rejection passed.');

  // ==========================================
  // Test 6: preview_busy retry_after polling and cancellation on dispose via Playwright Clock
  // ==========================================
  console.log('Test 6: preview_busy retry polling and cancellation on dispose via Playwright Clock...');
  const busy = { error: { code: 'preview_busy', message: 'both document renderers are busy; retry shortly', detail: { retry_after: 2 } } };
  let pollAttempts = 0;
  let busyAnswers = 1;
  await page.route('https://api.branchyard.local/v1/workspaces/ws-1/artifacts/polling-doc/preview*', (route) => {
    pollAttempts++;
    if (pollAttempts <= busyAnswers) {
      route.fulfill({ status: 429, contentType: 'application/json', body: JSON.stringify(busy) });
    } else {
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          status: 'passed',
          kind: 'text',
          text: 'READY AFTER RETRY SEQUENCE'
        })
      });
    }
  });
  const mountPolling = () => page.evaluate(() => {
    const container = document.getElementById('preview-container');
    container.innerHTML = '';
    window.pollingPreview = window.BranchyardArtifactPreview.mount(container, {
      baseUrl: 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/polling-doc',
      fetchAuthenticated: window.mockFetchAuthenticated
    });
  });

  // Paused, so only runFor moves the page's time and retry_after is measured exactly.
  await context.clock.pauseAt(Date.now() + 60000);
  await mountPolling();
  await page.waitForSelector('text=The server is busy making other previews. Trying again in 2 seconds…');
  assert.strictEqual(pollAttempts, 1);
  await context.clock.runFor(1999);
  assert.strictEqual(pollAttempts, 1, 'The retry waits the whole retry_after');
  await context.clock.runFor(1);
  await page.waitForSelector('text=READY AFTER RETRY SEQUENCE');
  assert.strictEqual(pollAttempts, 2);

  pollAttempts = 0;
  busyAnswers = Infinity;
  await mountPolling();
  for (let attempt = 1; attempt <= 5; attempt++) {
    await page.waitForFunction((n) => window.fetchLogs.filter((l) => l.url.includes('polling-doc')).length >= n, attempt);
    await context.clock.runFor(2000);
  }
  await page.locator('.by-preview-failure .by-preview-state-message', { hasText: 'The server is busy making other previews' }).waitFor();
  assert.strictEqual(pollAttempts, 6, 'Five retries, then the busy state');
  assert(await page.getByRole('button', { name: 'Try again' }).isVisible(), 'A busy server offers Try again');

  pollAttempts = 0;
  await mountPolling();
  await page.waitForSelector('text=The server is busy making other previews. Trying again in 2 seconds…');
  assert.strictEqual(pollAttempts, 1);
  await page.evaluate(() => {
    window.pollingPreview.dispose();
  });
  const attemptsAtDispose = pollAttempts;
  await context.clock.runFor(10000);
  assert.strictEqual(pollAttempts, attemptsAtDispose, 'Dispose must cancel pending retry timers');
  await context.clock.resume();
  console.log('  ✓ preview_busy retry_after polling, retry limit & timer cancellation on dispose passed.');

  // ==========================================
  // Test 7: Blob URL creation and revocation verification
  // ==========================================
  console.log('Test 7: Blob URL revocation on dispose & refresh...');
  const initialCreatedBlobs = await page.evaluate(() => window.createdBlobUrls.length);
  const initialRevokedBlobs = await page.evaluate(() => window.revokedBlobUrls.length);

  // Mount an image preview (from Test 3 pptx)
  await page.evaluate(() => {
    const container = document.getElementById('preview-container');
    container.innerHTML = '';
    window.blobTestPreview = window.BranchyardArtifactPreview.mount(container, {
      baseUrl: 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-3',
      fetchAuthenticated: window.mockFetchAuthenticated
    });
  });

  await page.waitForSelector('.by-preview-page-img');
  const blobsAfterMount = await page.evaluate(() => window.createdBlobUrls.length);
  assert(blobsAfterMount > initialCreatedBlobs, 'A Blob URL should have been created');

  // Dispose and check revocation
  await page.evaluate(() => {
    window.blobTestPreview.dispose();
  });

  const blobsRevokedAfterDispose = await page.evaluate(() => window.revokedBlobUrls.length);
  assert(blobsRevokedAfterDispose > initialRevokedBlobs, 'Blob URL must be revoked on dispose');
  console.log('  ✓ Blob URL revocation passed.');

  await browser.close();
  console.log('\nAll 7 hermetic Playwright component regression tests PASSED successfully!\n');
}

runTests().catch((err) => {
  console.error('\nPlaywright Test Suite Failed:', err);
  process.exit(1);
});
