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
const shots = process.env.BY_SCREENSHOTS;

(async () => {
  browser = await chromium.launch();
  const context = await browser.newContext();
  const page = await context.newPage();
  shown = () => page.locator('main').innerText();
  const problems = [];
  page.on('console', (m) => { if (m.type() === 'error') problems.push(m.text()); });
  page.on('pageerror', (e) => problems.push(String(e)));
  const go = (hash) => page.evaluate((h) => { location.hash = h; }, hash);
  const noOverflow = async (what) => {
    if (await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)) throw new Error(`${what} overflows at ${page.viewportSize().width}px`);
  };

  await page.goto(`${process.env.BY_URL}/app/`);
  await page.getByRole('heading', { name: 'Connect to Branchyard' }).waitFor();
  await page.getByLabel('Server token', { exact: true }).fill('invalid-fixture-only');
  await page.getByRole('button', { name: 'Connect to server', exact: true }).click();
  await page.getByRole('alert').filter({ hasText: 'That token is not accepted.' }).waitFor();
  if (await page.getByRole('button', { name: 'Connect to server', exact: true }).isDisabled()) throw new Error('failed sign-in left the connect button disabled');

  await page.goto(`${process.env.BY_URL}/app/#pair=${process.env.BY_CODE}`);
  await page.getByRole('heading', { level: 1, name: 'app', exact: true }).waitFor({ timeout: 30000 });
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
  if (shots) await page.screenshot({ path: `${shots}/error-desktop.png` });
  await page.getByRole('button', { name: 'Retry loading branches' }).click();
  await page.getByText('No branches yet.').waitFor();
  await page.unroute('**/v1/repos/app/branches');

  // Keep the response pending to observe an actual loading state, without a sleep.
  let releaseList;
  const heldList = new Promise((resolve) => { releaseList = resolve; });
  await page.route('**/v1/repos/app/branches', async (route) => { await heldList; await route.continue(); });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.getByRole('heading', { name: 'Loading branches…' }).waitFor();
  if (shots) await page.screenshot({ path: `${shots}/loading-desktop.png` });
  releaseList();
  await page.getByText('No branches yet.').waitFor();
  await page.unroute('**/v1/repos/app/branches');
  if (shots) await page.screenshot({ path: `${shots}/empty-desktop.png` });
  await page.locator('#live[data-state="live"]').waitFor({ timeout: 30000 });
  say('LOADED');

  await nextLine();
  const item = page.locator('#lane-ready a', { hasText: 'first' });
  await item.waitFor({ timeout: 60000 });
  if (await page.locator('.toast').count()) throw new Error(`a stream event raised a toast: ${await page.locator('.toast').allInnerTexts()}`);
  if (await page.title() !== 'Branchyard') throw new Error(`the title counts work that does not need you: ${await page.title()}`);
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
  await page.locator('#branch-count', { hasText: '2 branches' }).waitFor();
  say('FILTER PRESERVED');

  await page.locator('#main h1').focus();
  await page.keyboard.press('/');
  if (!(await search.evaluate((input) => input === document.activeElement))) throw new Error('/ did not focus the filter');
  await page.locator('#main h1').focus();
  const rows = page.locator('#branch-list a');
  const focusedRow = () => page.evaluate(() => document.activeElement.dataset.branch || null);
  await page.keyboard.press('j');
  await page.keyboard.press('j');
  const second = await rows.nth(1).getAttribute('data-branch');
  if (await focusedRow() !== second) throw new Error(`j j focused ${await focusedRow()}, not ${second}`);
  await page.keyboard.press('k');
  const top = await rows.nth(0).getAttribute('data-branch');
  if (await focusedRow() !== top) throw new Error(`k focused ${await focusedRow()}, not ${top}`);
  await page.keyboard.press('Enter');
  await page.locator('#strip h1', { hasText: top }).waitFor();

  await go('#/');
  for (const width of [320, 390]) {
    await page.setViewportSize({ width, height: 844 });
    await item.waitFor();
    await noOverflow('the branch list');
    for (const link of await page.locator('#sidebar nav a').all()) {
      const box = await link.boundingBox();
      if (!box || box.x < 0 || box.x + box.width > width + 1) throw new Error(`navigation is clipped at ${width}px`);
    }
    await item.click();
    await page.getByRole('link', { name: '‹ Branches' }).waitFor();
    await page.locator('#strip').getByRole('button', { name: 'Merge', exact: true }).waitFor();
    await noOverflow('a branch');
    await page.getByRole('link', { name: '‹ Branches' }).click();
    await item.waitFor();
  }

  await page.setViewportSize({ width: 1280, height: 900 });
  if (shots) await page.screenshot({ path: `${shots}/branches-desktop.png` });
  await item.click();
  await page.getByRole('heading', { name: 'first', exact: true }).waitFor();
  const [side, focus, inspector] = await Promise.all(['#sidebar', '.focus', '#inspector'].map((s) => page.locator(s).boundingBox()));
  if (!(side.x + side.width <= focus.x + 1 && focus.x + focus.width <= inspector.x + 1 && Math.abs(focus.y - inspector.y) < 2 && Math.abs(side.y - focus.y) < 2)) {
    throw new Error(`the three panes are not side by side at 1280px: ${JSON.stringify([side, focus, inspector])}`);
  }
  await page.locator('#strip').getByRole('button', { name: 'Merge', exact: true }).waitFor();
  if (await page.getByText('Not now:').count()) throw new Error('an action that does not apply is still explained');
  await page.getByRole('button', { name: 'Resume', exact: true }).waitFor({ state: 'detached' });
  await page.locator('#strip summary[aria-label="More actions"]').click();
  await page.getByRole('button', { name: 'Fork…', exact: true }).waitFor();
  if (await page.getByRole('button', { name: 'Cancel turn' }).count()) throw new Error('Cancel turn is offered for a ready branch');
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: 'Fork…', exact: true }).waitFor({ state: 'hidden' });

  await page.locator('#convo .you', { hasText: 'WRITE a.txt=1' }).waitFor();
  await page.locator('#convo .agent', { hasText: 'wrote a.txt' }).waitFor();
  if ((await page.locator('#convo').innerText()).includes('session')) throw new Error('session bookkeeping is in the conversation');
  await page.locator('#tab-changes[aria-selected="true"]').waitFor();
  await page.locator('pre.diff span.add', { hasText: '+1' }).waitFor({ timeout: 30000 });

  await page.getByLabel('Send a follow-up prompt').fill('PERMISSION_BY_ID WRITE b.txt=2');
  await page.getByLabel('Allow every tool').check();
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await page.locator('.toast', { hasText: 'Sent. A new turn is starting.' }).first().waitFor({ timeout: 30000 });
  say('SENT');
  await page.waitForFunction(() => {
    const dt = Array.from(document.querySelectorAll('dl.facts dt')).find((d) => d.textContent === 'Turns');
    return dt && dt.nextElementSibling.textContent === '2';
  }, null, { timeout: 60000 });
  await page.locator('#convo .you', { hasText: 'PERMISSION_BY_ID WRITE b.txt=2' }).waitFor({ timeout: 60000 });

  const fold = page.locator('#convo .turn details.tools').last();
  await fold.locator('summary', { hasText: '1 tool call' }).waitFor({ timeout: 60000 });
  if (await fold.evaluate((d) => d.open)) throw new Error('tool calls are not folded');
  await fold.locator('summary').click();
  await fold.locator('li', { hasText: 'bash' }).waitFor();

  await page.locator('#files button', { hasText: 'b.txt' }).click({ timeout: 30000 });
  await page.locator('pre.diff span.add', { hasText: '+2' }).waitFor({ timeout: 30000 });
  await page.getByRole('tab', { name: 'Activity' }).click();
  await page.locator('#events', { hasText: 'prompt: PERMISSION_BY_ID WRITE b.txt=2' }).waitFor({ timeout: 60000 });
  await page.locator('#events', { hasText: 'session started' }).waitFor();

  for (const hash of ['#/inbox', '#/approvals', '#/attention']) {
    await go(hash);
    await page.getByRole('heading', { level: 1, name: 'Needs you' }).waitFor();
  }

  // BY_SCREENSHOTS=DIR saves desktop and phone views in both themes, for a look.
  if (shots) {
    const views = [
      ['home', '#/', () => page.getByRole('heading', { level: 1, name: 'app', exact: true })],
      ['attention', '#/attention', () => page.getByRole('heading', { level: 1, name: 'Needs you' })],
      ['branch', '#/b/app/first', () => page.locator('pre.diff')],
    ];
    await page.setViewportSize({ width: 1440, height: 900 });
    for (const scheme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: scheme });
      for (const [name, hash, ready] of views) {
        await go(hash);
        await ready().waitFor();
        await page.screenshot({ path: `${shots}/desktop-${name}-${scheme}.png` });
      }
    }
    const phone = [
      ['branch', '#/b/app/first', () => page.locator('pre.diff')],
      ['branches', '#/', () => item],
      ['inbox', '#/inbox', () => page.getByRole('heading', { level: 1, name: 'Needs you' })],
      ['queue', '#/queue', () => page.getByRole('heading', { level: 1, name: 'Queue and stats' })],
      ['settings', '#/settings', () => page.getByRole('heading', { level: 1, name: 'Settings' })],
    ];
    await page.setViewportSize({ width: 390, height: 844 });
    for (const scheme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: scheme });
      for (const [name, hash, ready] of phone) {
        await go(hash);
        await ready().waitFor();
        await page.screenshot({ path: `${shots}/${name}-${scheme}.png`, fullPage: name === 'branch' });
      }
    }
    await page.emulateMedia({ colorScheme: 'light' });
  }

  await page.setViewportSize({ width: 1280, height: 900 });
  await go('#/b/app/first');
  await page.locator('#strip').getByRole('button', { name: 'Merge', exact: true }).click();
  await page.locator('#confirm-ok').click();
  await page.locator('.toast', { hasText: 'Merged into' }).waitFor({ timeout: 60000 });
  await page.locator('#strip .state-merged svg.check').waitFor();
  await page.locator('#lane-done').waitFor({ state: 'attached' });
  if (await page.locator('#lane-ready a', { hasText: /^first/ }).count()) throw new Error('a merged branch stayed in Ready');

  // A token can expire between a failed list request and its retry.
  let listAttempts = 0;
  await page.route('**/v1/repos/app/branches', async (route) => {
    listAttempts += 1;
    const status = listAttempts === 1 ? 503 : 401;
    await route.fulfill({ status, contentType: 'application/json', body: JSON.stringify({ error: {
      code: status === 503 ? 'unavailable' : 'unauthorized',
      message: status === 503 ? 'Fixture server temporarily unavailable.' : 'Fixture session expired.',
    } }) });
  });
  await page.goto(`${process.env.BY_URL}/app/`, { waitUntil: 'domcontentloaded' });
  await page.getByRole('heading', { name: 'Could not load branches' }).waitFor();
  await page.getByRole('button', { name: 'Retry loading branches' }).click();
  await page.getByRole('heading', { name: 'Connect to Branchyard' }).waitFor();
  await page.getByLabel('Server token', { exact: true }).fill('new-token-fixture-only');
  if (await page.getByRole('button', { name: 'Connect to server', exact: true }).isDisabled()) throw new Error('expired-token recovery left connection disabled');
  const signedOut = await page.evaluate(() => sessionStorage.getItem('branchyard.token') === null && document.getElementById('sidebar').hidden);
  if (!signedOut || listAttempts !== 2) throw new Error('retry did not clear the expired session');

  const csp = problems.filter((p) => /Content.Security.Policy|Refused to/i.test(p));
  if (csp.length) throw new Error(`policy violations: ${csp.join(' | ')}`);
  const errors = problems.filter((p) => !/Failed to load resource/.test(p));
  if (errors.length) throw new Error(`page errors: ${errors.join(' | ')}`);
  say('DONE');
  await browser.close();
  process.exit(0);
})().catch(async (e) => {
  process.stderr.write(`${e.stack || e}\n`);
  if (shown) process.stderr.write(`the page showed:\n${await shown().catch(() => '?')}\n`);
  if (browser) await browser.close().catch(() => {});
  process.exit(1);
});
