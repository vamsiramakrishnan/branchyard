// Drives the companion page in headless Chromium for
// tests/companion_browser.rs, which runs the server and talks to this
// script over stdin and stdout, a line at a time:
//
//   script: LOADED            the page paired and shows no branches
//   test:   (submits a task named "first" through the API)
//   script: SEEN first        the branch appeared over the event stream
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
lines.on('line', (line) => { const w = waiting.shift(); if (w) w(line); });
const nextLine = () => new Promise((resolve) => waiting.push(resolve));
let shown = null;
const say = (text) => process.stdout.write(`${text}\n`);

(async () => {
  const browser = await chromium.launch();
  const context = await browser.newContext();
  const page = await context.newPage();
  shown = () => page.locator('main').innerText();
  const problems = [];
  page.on('console', (m) => { if (m.type() === 'error') problems.push(m.text()); });
  page.on('pageerror', (e) => problems.push(String(e)));

  await page.goto(`${process.env.BY_URL}/app/#pair=${process.env.BY_CODE}`);
  await page.getByRole('heading', { name: 'Branches in app' }).waitFor({ timeout: 30000 });
  // The code is gone from the address bar once redeemed.
  if (page.url().includes('pair=')) throw new Error(`the code stayed in the URL: ${page.url()}`);
  await page.getByText('No branches yet.').waitFor();
  await page.locator('#live[data-state="live"]').waitFor({ timeout: 30000 });
  say('LOADED');

  await nextLine();
  const item = page.locator('#branch-list a', { hasText: 'first' });
  await item.waitFor({ timeout: 60000 });
  await item.locator('.badge', { hasText: 'ready' }).waitFor({ timeout: 60000 });
  say('SEEN first');

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
        if (hash) { await page.evaluate((h) => { location.hash = h; }, hash); await page.waitForTimeout(500); }
        await page.screenshot({ path: `${process.env.BY_SCREENSHOTS}/${name}-${scheme}.png`, fullPage: name === 'branch' });
      }
      await page.evaluate(() => { location.hash = '#/b/app/first'; });
      await page.waitForTimeout(500);
    }
  }
  say('DONE');
  await browser.close();
  process.exit(0);
})().catch(async (e) => {
  process.stderr.write(`${e.stack || e}\n`);
  if (shown) process.stderr.write(`the page showed:\n${await shown().catch(() => '?')}\n`);
  process.exit(1);
});
