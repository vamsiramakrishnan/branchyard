// Drives the companion page in headless Chromium for
// tests/companion_browser.rs, which runs the server and talks to this
// script over stdin and stdout, a line at a time:
//
//   script: LOADED            the page paired and shows no branches
//   test:   (submits a task named "first" through the API)
//   script: SEEN first        the branch appeared over the event stream
//   script: FILTERED          a search is focused with selected text
//   test:   (starts another branch), then "refresh"
//   script: FILTER PRESERVED  the live update kept the query and selection
//   script: SENT              a follow-up was sent from the branch page
//   script: DONE              and the page shows its second turn
//
// PLAYWRIGHT names the playwright module; BY_URL the server; BY_CODE a
// pairing code.
'use strict';

const { chromium } = require(process.env.PLAYWRIGHT);
const readline = require('readline');

const lines = readline.createInterface({ input: process.stdin });
const waiting = [];
lines.on('line', (line) => { const w = waiting.shift(); if (w) w.resolve(line); });
lines.on('close', () => { for (const w of waiting.splice(0)) w.reject(new Error('the test controller closed stdin')); });
const nextLine = () => new Promise((resolve, reject) => waiting.push({ resolve, reject }));
let shown = null;
let browser = null;
const say = (text) => process.stdout.write(`${text}\n`);

(async () => {
  browser = await chromium.launch();
  const context = await browser.newContext();
  const page = await context.newPage();
  shown = () => page.locator('main').innerText();
  const problems = [];
  page.on('console', (m) => { if (m.type() === 'error') problems.push(m.text()); });
  page.on('pageerror', (e) => problems.push(String(e)));

  await page.goto(`${process.env.BY_URL}/app/`);
  await page.getByRole('heading', { name: 'Connect to Branchyard' }).waitFor();
  await page.getByLabel('Server token', { exact: true }).fill('invalid-fixture-only');
  await page.getByRole('button', { name: 'Connect to server', exact: true }).click();
  await page.getByRole('alert').filter({ hasText: 'That token is not accepted.' }).waitFor();
  if (await page.getByRole('button', { name: 'Connect to server', exact: true }).isDisabled()) throw new Error('failed sign-in left the connect button disabled');

  await page.goto(`${process.env.BY_URL}/app/#pair=${process.env.BY_CODE}`);
  await page.getByRole('heading', { name: 'Branches in app' }).waitFor({ timeout: 30000 });
  // The code is gone from the address bar once redeemed.
  if (page.url().includes('pair=')) throw new Error(`the code stayed in the URL: ${page.url()}`);
  await page.getByText('No branches yet.').waitFor();
  await page.getByRole('link', { name: 'New task', exact: true }).waitFor();

  // A failed list request has its own retry, and never looks like an empty yard.
  let failList = true;
  await page.route('**/v1/repos/app/branches', async (route) => {
    if (failList) {
      failList = false;
      await route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: { code: 'unavailable', message: 'Fixture server temporarily unavailable.' } }) });
    } else await route.continue();
  });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.getByRole('heading', { name: 'Could not load branches' }).waitFor();
  if (await page.getByText('No branches yet.').count()) throw new Error('a failed request looked empty');
  if (process.env.BY_SCREENSHOTS) await page.screenshot({ path: `${process.env.BY_SCREENSHOTS}/error-desktop.png` });
  await page.getByRole('button', { name: 'Retry loading branches' }).click();
  await page.getByText('No branches yet.').waitFor();
  await page.unroute('**/v1/repos/app/branches');

  // Keep the response pending to observe an actual loading state, without a sleep.
  let releaseList;
  const heldList = new Promise((resolve) => { releaseList = resolve; });
  await page.route('**/v1/repos/app/branches', async (route) => { await heldList; await route.continue(); });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.getByRole('heading', { name: 'Loading branches…' }).waitFor();
  if (process.env.BY_SCREENSHOTS) await page.screenshot({ path: `${process.env.BY_SCREENSHOTS}/loading-desktop.png` });
  releaseList();
  await page.getByText('No branches yet.').waitFor();
  await page.unroute('**/v1/repos/app/branches');
  if (process.env.BY_SCREENSHOTS) await page.screenshot({ path: `${process.env.BY_SCREENSHOTS}/empty-desktop.png` });
  await page.locator('#live[data-state="live"]').waitFor({ timeout: 30000 });
  say('LOADED');

  await nextLine();
  const item = page.locator('#branch-list a', { hasText: 'first' });
  await item.waitFor({ timeout: 60000 });
  await item.locator('.badge', { hasText: 'ready' }).waitFor({ timeout: 60000 });
  say('SEEN first');

  const search = page.getByRole('searchbox', { name: 'Filter branches' });
  await search.fill('FIRST');
  await page.locator('#branch-count', { hasText: 'Showing 1 of 1 branches' }).waitFor();
  await search.fill('unmatched-query');
  await page.getByRole('heading', { name: 'No matching branches' }).waitFor();
  await page.getByRole('button', { name: 'Clear search' }).click();
  await item.waitFor();
  if (!(await search.evaluate((input) => input === document.activeElement))) throw new Error('clearing search lost keyboard focus');
  await search.fill('first');
  await search.evaluate((input) => input.setSelectionRange(1, 3));
  say('FILTERED');
  await nextLine();
  await page.locator('#branch-count', { hasText: 'Showing 1 of 2 branches' }).waitFor();
  const selection = await search.evaluate((input) => [input === document.activeElement, input.selectionStart, input.selectionEnd, input.value]);
  if (JSON.stringify(selection) !== JSON.stringify([true, 1, 3, 'first'])) throw new Error(`live refresh changed the search selection: ${JSON.stringify(selection)}`);
  await search.fill('');
  say('FILTER PRESERVED');

  // All six navigation links stay visible and long branch names do not widen a phone.
  for (const width of [320, 390]) {
    await page.setViewportSize({ width, height: 844 });
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth);
    if (overflow) throw new Error(`branch list overflows at ${width}px`);
    for (const link of await page.locator('#nav a').all()) {
      const box = await link.boundingBox();
      if (!box || box.x < 0 || box.x + box.width > width + 1) throw new Error(`navigation is clipped at ${width}px`);
    }
  }
  await page.setViewportSize({ width: 1280, height: 900 });
  if (process.env.BY_SCREENSHOTS) await page.screenshot({ path: `${process.env.BY_SCREENSHOTS}/branches-desktop.png` });

  await item.click();
  await page.getByRole('heading', { name: 'first' }).waitFor();
  await page.getByLabel('Send a follow-up prompt').fill('WRITE b.txt=2');
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await page.locator('.toast', { hasText: 'Send: operation' }).first().waitFor({ timeout: 30000 });
  say('SENT');
  await page.waitForFunction(() => {
    const dt = Array.from(document.querySelectorAll('dl.facts dt')).find((d) => d.textContent === 'Turns');
    return dt && dt.nextElementSibling.textContent === '2';
  }, null, { timeout: 60000 });
  await page.locator('#events', { hasText: 'prompt: WRITE b.txt=2' }).waitFor({ timeout: 60000 });

  // The diff renders as monospace lines.
  await page.getByRole('button', { name: 'Show the diff' }).click();
  await page.locator('pre.diff span.add', { hasText: '+2' }).waitFor({ timeout: 30000 });

  const csp = problems.filter((p) => /Content.Security.Policy|Refused to/i.test(p));
  if (csp.length) throw new Error(`policy violations: ${csp.join(' | ')}`);
  const errors = problems.filter((p) => !/Failed to load resource/.test(p));
  if (errors.length) throw new Error(`page errors: ${errors.join(' | ')}`);
  // BY_SCREENSHOTS=DIR saves phone-sized views in both themes, for a look.
  if (process.env.BY_SCREENSHOTS) {
    await page.setViewportSize({ width: 390, height: 844 });
    for (const scheme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: scheme });
      for (const [name, hash] of [['branch', null], ['branches', '#/'], ['inbox', '#/inbox'], ['queue', '#/queue'], ['settings', '#/settings']]) {
        if (hash) {
          await page.evaluate((h) => { location.hash = h; }, hash);
          await page.getByRole('heading', { level: 1, name: new RegExp(name === 'branches' ? '^Branches in ' : name === 'queue' ? '^Queue and stats' : name, 'i') }).waitFor();
        }
        await page.screenshot({ path: `${process.env.BY_SCREENSHOTS}/${name}-${scheme}.png`, fullPage: name === 'branch' });
      }
      await page.evaluate(() => { location.hash = '#/b/app/first'; });
      await page.getByRole('heading', { name: 'first', exact: true }).waitFor();
    }
  }
  say('DONE');
  await browser.close();
  process.exit(0);
})().catch(async (e) => {
  process.stderr.write(`${e.stack || e}\n`);
  if (shown) process.stderr.write(`the page showed:\n${await shown().catch(() => '?')}\n`);
  if (browser) await browser.close().catch(() => {});
  process.exit(1);
});
