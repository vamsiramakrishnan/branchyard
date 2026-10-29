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
  // Test 4: Unavailable/Error state with setup action & Original Download
  // ==========================================
  console.log('Test 4: Error/Unavailable state with setup action & download...');
  await page.route('https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-4/preview*', (route) => {
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        status: 'unavailable',
        message: 'LibreOffice preview service not running',
        detail: {
          next: 'Run branchyard rig setup preview-services to enable document conversion.'
        },
        setup: true
      })
    });
  });

  const downloadRequestPromise = page.waitForRequest('https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-4/content');

  await page.route('https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-4/content', (route) => {
    route.fulfill({
      status: 200,
      contentType: 'application/octet-stream',
      body: Buffer.from('Original file bytes')
    });
  });

  await page.evaluate(() => {
    const container = document.getElementById('preview-container');
    container.innerHTML = '';
    window.activePreview = window.BranchyardArtifactPreview.mount(container, {
      baseUrl: 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/art-4',
      displayName: 'spec.docx',
      mediaType: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
      fetchAuthenticated: window.mockFetchAuthenticated
    });
  });

  await page.waitForSelector('text=LibreOffice preview service not running');
  await page.waitForSelector('button:has-text("Setup Instructions")');
  const dlButton = page.locator('button:has-text("Download Original")');
  assert(await dlButton.isVisible(), 'Original download button must be visible even when unavailable');

  await dlButton.click();
  const downloadReq = await downloadRequestPromise;
  assert(downloadReq, 'Original download content endpoint must be requested');
  console.log('  ✓ Unavailable state, setup action, and original download passed.');

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
  // Test 6: 202 / 429 Retry-After polling and cancellation on dispose via Playwright Clock
  // ==========================================
  console.log('Test 6: 202/429 retry polling and cancellation on dispose via Playwright Clock...');
  let pollAttempts = 0;
  await page.route('https://api.branchyard.local/v1/workspaces/ws-1/artifacts/polling-doc/preview*', (route) => {
    pollAttempts++;
    if (pollAttempts === 1) {
      // Return 202 Accepted with Retry-After: 1
      route.fulfill({
        status: 202,
        headers: { 'Retry-After': '1' },
        body: JSON.stringify({ status: 'preparing' })
      });
    } else if (pollAttempts === 2) {
      // Return 429 preview_busy with detail.retry_after: 2
      route.fulfill({
        status: 429,
        contentType: 'application/json',
        body: JSON.stringify({
          error: {
            code: 'preview_busy',
            message: 'Preview renderer busy',
            detail: { retry_after: 2 }
          }
        })
      });
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

  await page.evaluate(() => {
    const container = document.getElementById('preview-container');
    container.innerHTML = '';
    window.pollingPreview = window.BranchyardArtifactPreview.mount(container, {
      baseUrl: 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/polling-doc',
      fetchAuthenticated: window.mockFetchAuthenticated
    });
  });

  // Verify attempt 1 occurred
  await page.waitForSelector('text=Preparing preview (attempt 1)...');
  assert.strictEqual(pollAttempts, 1);

  // Advance clock by 1000ms for 202 Retry-After: 1
  await context.clock.runFor(1000);
  await page.waitForSelector('text=Preview service busy; retrying in 2s (attempt 2)...');
  assert.strictEqual(pollAttempts, 2);

  // Advance clock by 2000ms for 429 retry_after: 2
  await context.clock.runFor(2000);
  await page.waitForSelector('text=READY AFTER RETRY SEQUENCE');
  assert.strictEqual(pollAttempts, 3);

  // Now test deterministic cancellation on dispose
  pollAttempts = 0;
  await page.evaluate(() => {
    const container = document.getElementById('preview-container');
    container.innerHTML = '';
    window.cancelledPreview = window.BranchyardArtifactPreview.mount(container, {
      baseUrl: 'https://api.branchyard.local/v1/workspaces/ws-1/artifacts/polling-doc',
      fetchAuthenticated: window.mockFetchAuthenticated
    });
  });

  // Wait for 1st attempt to settle into retry wait
  await page.waitForSelector('text=Preparing preview (attempt 1)...');
  assert.strictEqual(pollAttempts, 1);

  // Dispose while retry timer is pending
  await page.evaluate(() => {
    window.cancelledPreview.dispose();
  });
  const attemptsAtDispose = pollAttempts;

  // Advance clock far past Retry-After
  await context.clock.runFor(10000);
  assert.strictEqual(pollAttempts, attemptsAtDispose, 'Dispose must cancel pending retry timers');
  console.log('  ✓ 202/429 retry polling & timer cancellation on dispose passed.');

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
