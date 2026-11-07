// Drives the console's Changes page in headless Chromium for
// tests/console_changes_browser.rs, which runs the server and talks to this
// script over stdin and stdout, a line at a time. argv[2] is the path of
// changes.js, whose pure functions are checked under Node first.
//
//   script: MODEL       selections and re-anchoring, under Node
//   script: LOADED      the board's row opens the page; it read the diff and
//                       the reviews once each and nothing else of the branch
//   script: FILES       the file list, the path filter, j and k, viewed boxes
//   script: COMMENTED   a dragged range and a single line commented, n and c,
//                       Escape, the tray; nothing sent yet
//   script: SENT        one note, one write, one read; the comments inline
//   test:   (the prompt is checked) "go"
//   test:   (a later turn rewrote the lines) "changed"
//   script: BANNER      the new-candidate banner, without a reload
//   script: ADDRESSED   reloading marks the comment on the rewritten lines
//   script: VIEWPORTS   1440 and 390 pixels: no page overflow; light and dark
//   script: DONE
//
// PLAYWRIGHT names the playwright module; BY_URL the server; BY_CODE a
// pairing code; BY_SCREENSHOTS a directory for screenshots.
'use strict';

const { chromium } = require(process.env.PLAYWRIGHT);
const readline = require('readline');
const assert = require('assert');

const lines = readline.createInterface({ input: process.stdin });
const waiting = [];
const arrived = [];
let closed = false;
lines.on('line', (line) => { const w = waiting.shift(); if (w) w.resolve(line); else arrived.push(line); });
lines.on('close', () => { closed = true; for (const w of waiting.splice(0)) w.reject(new Error('the test controller closed stdin')); });
const nextLine = () => (arrived.length ? Promise.resolve(arrived.shift())
  : closed ? Promise.reject(new Error('the test controller closed stdin'))
    : new Promise((resolve, reject) => waiting.push({ resolve, reject })));
const say = (text) => { process.stderr.write(`[page] ${text}\n`); process.stdout.write(`${text}\n`); };
let where = 'starting';
setTimeout(() => { process.stderr.write(`[page] still at ${where} after four minutes\n`); process.exit(3); }, 240000).unref();
const shots = process.env.BY_SCREENSHOTS;
const eq = (actual, expected, what) => assert.deepStrictEqual(actual, expected, `${what}: ${JSON.stringify(actual)} is not ${JSON.stringify(expected)}`);
const ok = (value, what) => assert.ok(value, what);

function checkModel(path) {
  const { model } = require(path);
  const file = {
    hunks: [{
      header: '@@ -1,3 +1,4 @@',
      lines: [
        { kind: 'context', old: 1, new: 1, text: 'a' },
        { kind: 'del', old: 2, text: 'b' },
        { kind: 'add', new: 2, text: 'B' },
        { kind: 'add', new: 3, text: 'B2' },
        { kind: 'context', old: 3, new: 4, text: 'c' },
      ],
    }, { header: '@@ -9 +10 @@', lines: [{ kind: 'del', old: 9, text: 'x' }, { kind: 'add', new: 10, text: 'y' }] }],
  };
  const rows = model.rowsOf(file);
  eq(rows.map((r) => r.type), ['hunk', 'line', 'line', 'line', 'line', 'line', 'hunk', 'line', 'line'], 'a hunk row, then its lines');
  // A selection is on the side of the row it began on, and stays in its hunk.
  eq(model.selectBetween(rows, 3, 4), { side: 'new', start: 2, end: 3, hunk: 0 }, 'two added lines');
  eq(model.selectBetween(rows, 1, 5), { side: 'new', start: 1, end: 4, hunk: 0 }, 'the removed line between is skipped');
  eq(model.selectBetween(rows, 2, 2), { side: 'old', start: 2, end: 2, hunk: 0 }, 'a removed line is on the base side');
  eq(model.selectBetween(rows, 3, 8), { side: 'new', start: 2, end: 4, hunk: 0 }, 'a range does not cross a hunk');
  eq(model.selectBetween(rows, 0, 0), null, 'a hunk header is not a line');
  eq(model.quoteOf(rows, 'new', 2, 3), ['B', 'B2'], 'the quoted text');
  // A comment follows its lines to a new candidate, or is lost.
  const moved = model.rowsOf({ hunks: [{ header: '@@', lines: [{ kind: 'add', new: 7, text: 'B' }, { kind: 'add', new: 8, text: 'B2' }] }] });
  eq(model.reanchor(moved, { side: 'new', quote: ['B', 'B2'] }), { start: 7, end: 8 }, 'moved with its lines');
  eq(model.reanchor(moved, { side: 'new', quote: ['B', 'gone'] }), null, 'lost with its lines');
  say('MODEL');
}

(async () => {
  checkModel(process.argv[2]);
  const browser = await chromium.launch({ args: ['--no-sandbox'] });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'light' });
  const page = await context.newPage();
  const problems = [];
  page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
  page.on('console', (m) => { if (m.type() === 'error') problems.push(`console: ${m.text()}`); });
  const requests = [];
  page.on('request', (r) => requests.push(`${r.method()} ${r.url()}`));
  const count = (re) => requests.filter((u) => re.test(u)).length;
  const url = process.env.BY_URL;
  const go = (hash) => page.evaluate((h) => { location.hash = h; }, hash);
  const noOverflow = async (what) => {
    const [scroll, inner] = await page.evaluate(() => [document.documentElement.scrollWidth, innerWidth]);
    if (scroll > inner) throw new Error(`${what} overflows at ${inner}px (${scroll})`);
  };
  const line = (n, side = 'new') => page.locator(`tr.ch-line[data-side="${side}"][data-line="${n}"]`);
  const gutter = (n) => line(n).locator('button.ch-gutter');
  const current = () => page.locator('.ch-fileitem.current .ch-file').getAttribute('data-path');

  where = 'loading the board';
  await page.goto(`${url}/app/#pair=${process.env.BY_CODE}`);
  await page.locator('#main h1').first().waitFor({ timeout: 60000 });
  await page.locator('.board-row[data-branch="polish"]').waitFor({ state: 'attached', timeout: 60000 });
  if (!(await page.locator('.board-row[data-branch="polish"]').isVisible())) await page.locator('.board-done-head').click();
  const review = page.locator('.board-row[data-branch="polish"] a.board-changes');
  await review.waitFor();
  eq(await review.getAttribute('aria-label'), 'Review the changes of polish', 'the board row links to the changes');
  requests.length = 0;
  await review.click();
  await page.getByRole('heading', { level: 1, name: 'Changes: polish' }).waitFor();
  await page.locator('.ch-host[data-ready="true"]').waitFor({ timeout: 60000 });
  await page.locator('.ch-fileitem').first().waitFor();
  eq(count(/GET .*\/branches\/polish\/changes$/), 1, 'the diff was read once');
  eq(count(/GET .*\/branches\/polish\/reviews$/), 1, 'the reviews were read once');
  eq(count(/\/branches\/polish\/diff/), 0, 'the plain diff was not read');
  eq(count(/\/branches\/polish\/(timeline|event-page|events)/), 0, 'no timeline or events were read');
  // The branch page and the Trajectory page link here too.
  await go('#/b/app/polish');
  await page.locator('a[data-changes="polish"]').waitFor();
  await go('#/trajectory/app/polish');
  await page.locator('a[data-changes="polish"]').waitFor();
  await go('#/changes/app/polish');
  await page.locator('.ch-host[data-ready="true"]').waitFor();
  await page.locator('.ch-fileitem').first().waitFor();
  say('LOADED');

  where = 'files';
  eq(await page.locator('.ch-file').evaluateAll((els) => els.map((e) => e.dataset.path)), ['a.txt', 'notes.md', 'src/main.rs'], 'the changed files');
  ok((await page.locator('.ch-total').innerText()).includes('3 files changed, +'), 'the total counts files and lines');
  eq(await page.locator('.ch-total .add').innerText(), '+10', 'the total added lines (1 + 3 + 6)');
  eq(await page.locator('.ch-total .del').innerText(), '−0', 'the total removed lines');
  eq(await current(), 'a.txt', 'the first file is open');
  eq(await page.locator('.ch-table').getAttribute('aria-label'), 'Diff of a.txt', 'the diff is labelled');
  // The filter narrows the list by path.
  await page.locator('.ch-filter').fill('main');
  eq(await page.locator('.ch-file').count(), 1, 'the filter keeps one file');
  await page.locator('.ch-filter').fill('zzz');
  eq(await page.locator('.ch-empty').innerText(), 'No file matches.', 'and says when none match');
  await page.locator('.ch-filter').fill('');
  eq(await page.locator('.ch-file').count(), 3, 'clearing it lists all');
  // j and k move between files.
  await page.locator('#main h1').focus();
  await page.keyboard.press('j');
  eq(await current(), 'notes.md', 'j goes to the next file');
  await page.keyboard.press('j');
  await page.keyboard.press('j');
  eq(await current(), 'src/main.rs', 'and stops at the last');
  await page.keyboard.press('k');
  eq(await current(), 'notes.md', 'k goes back');
  await page.keyboard.press('k');
  eq(await current(), 'a.txt', 'to the first');
  // Viewed boxes are local and counted.
  await page.locator('.ch-viewed[data-path="a.txt"]').check();
  eq(await page.locator('.ch-progress').innerText(), '1 of 3 viewed', 'one file viewed');
  ok(await page.locator('.ch-viewed-head').isChecked(), 'the header box agrees');
  eq(count(/POST /), 0, 'nothing was written');
  await page.locator('.ch-file[data-path="src/main.rs"]').click();
  eq(await page.locator('.ch-table').getAttribute('aria-label'), 'Diff of src/main.rs', 'a click opens a file');
  eq(await page.locator('tr.ch-line').count(), 6, 'six lines of the new file');
  eq(await line(2).locator('.ch-code').innerText(), '+    let first = 1;', 'a line shows its marker and text');
  say('FILES');

  where = 'commenting';
  // Drag across lines 2 and 3.
  const from = await gutter(2).boundingBox();
  const to = await gutter(3).boundingBox();
  await page.mouse.move(from.x + from.width / 2, from.y + from.height / 2);
  await page.mouse.down();
  await page.mouse.move(to.x + to.width / 2, to.y + to.height / 2, { steps: 4 });
  await page.mouse.up();
  await page.locator('.ch-composer').waitFor();
  eq(await page.locator('.ch-composer-title').innerText(), 'lines 2–3', 'the composer names the range');
  eq(await page.locator('tr.ch-selected').count(), 2, 'two lines are selected');
  eq(await page.evaluate(() => document.activeElement.tagName), 'TEXTAREA', 'the comment box has focus');
  await page.keyboard.type('Name these better.');
  await page.keyboard.press('Enter');
  await page.keyboard.type('Keep them short.');
  eq(await page.locator('.ch-input').inputValue(), 'Name these better.\nKeep them short.', 'a comment has several lines');
  await page.keyboard.press('Control+Enter');
  await page.locator('.ch-pending').waitFor();
  eq(await page.locator('.ch-composer').count(), 0, 'the composer closed');
  ok((await page.locator('.ch-pending .ch-comment-body').innerText()).includes('Keep them short.'), 'the pending comment is under its lines');
  // A click on one line.
  await gutter(5).click();
  await page.locator('.ch-composer').waitFor();
  eq(await page.locator('.ch-composer-title').innerText(), 'line 5', 'a click comments on one line');
  await page.locator('.ch-input').fill('Pad the total?');
  await page.getByRole('button', { name: 'Add to review' }).click();
  eq(await page.locator('.ch-pending').count(), 2, 'two pending comments');
  eq(await page.locator('.ch-pitem').evaluateAll((els) => els.map((e) => e.querySelector('.ch-jump').textContent)), ['src/main.rs:2–3', 'src/main.rs:5'], 'the tray lists both');
  eq(await page.locator('.ch-badge').innerText(), '2', 'the file carries a count');
  eq(await page.locator('.ch-send').innerText(), 'Send review (2)', 'the button counts them');
  // The keyboard: n goes to a hunk, c comments there, Escape cancels.
  await page.locator('.ch-file[data-path="notes.md"]').click();
  await page.locator('#main h1').focus();
  await page.keyboard.press('n');
  await page.keyboard.press('c');
  await page.locator('.ch-composer').waitFor();
  eq(await page.locator('.ch-composer-title').innerText(), 'line 1', 'c comments on the first line of the hunk');
  await page.keyboard.press('Escape');
  eq(await page.locator('.ch-composer').count(), 0, 'Escape cancels it');
  eq(await page.locator('.ch-pending').count(), 0, 'and adds nothing');
  eq(await page.locator('.ch-pitem').count(), 2, 'the tray still holds two');
  // An empty comment is not added.
  await page.keyboard.press('c');
  await page.locator('.ch-composer').waitFor();
  await page.getByRole('button', { name: 'Add to review' }).click();
  eq(await page.locator('.ch-pitem').count(), 2, 'an empty comment is refused');
  await page.getByRole('button', { name: 'Cancel' }).click();
  await page.locator('.ch-file[data-path="src/main.rs"]').click();
  eq(count(/POST /), 0, 'nothing has been sent');
  say('COMMENTED');

  where = 'sending';
  // Nothing runs, so a steer is not offered.
  ok(await page.locator('input[name="ch-delivery"][value="steer"]').isDisabled(), 'steering needs a running turn');
  ok(await page.locator('input[name="ch-delivery"][value="next_turn"]').isChecked(), 'the next turn is the default');
  await page.locator('#ch-note').fill('Please keep the diff small.');
  const before = requests.length;
  await page.locator('.ch-send').click();
  await page.locator('.ch-sent').first().waitFor({ timeout: 60000 });
  eq(await page.locator('.ch-pitem').count(), 0, 'the tray is empty once sent');
  eq(await page.locator('.ch-pending').count(), 0, 'and no pending comment remains');
  eq(await page.locator('.ch-sent').count(), 2, 'both comments are shown as sent, inline');
  ok((await page.locator('.ch-sent').first().innerText()).includes('Keep them short.'), 'with their text');
  const sentRequests = requests.slice(before);
  eq(sentRequests.filter((r) => /^POST .*\/branches\/polish\/reviews$/.test(r)).length, 1, 'one write');
  eq(sentRequests.filter((r) => /^GET .*\/branches\/polish\/reviews$/.test(r)).length, 1, 'and one read of the reviews');
  eq(sentRequests.filter((r) => /\/branches\/polish\/changes$/.test(r)).length, 0, 'and no second read of the diff');
  ok((await page.locator('.ch-sentlist summary').innerText()).includes('Sent reviews (1)'), 'the review is listed');
  ok(!/\bnull\b|undefined/.test(await page.locator('.ch').innerText()), 'the page prints no stray null or undefined');
  if (shots) await page.screenshot({ path: `${shots}/changes-desktop.png`, fullPage: true });
  say('SENT');

  await nextLine(); // go: the controller has checked the prompt
  where = 'banner';
  const changed = await nextLine();
  eq(changed, 'changed', 'the controller rewrote the lines');
  await page.locator('.ch-banner:not([hidden])').waitFor({ timeout: 60000 });
  ok((await page.locator('.ch-banner').innerText()).includes('new candidate'), 'the banner says so');
  // The page did not reload on its own: the lines it shows are the old ones.
  eq(await line(2).locator('code').innerText(), '    let first = 1;', 'the old diff stays until asked');
  say('BANNER');

  where = 'addressed';
  await page.locator('.ch-reload').click();
  await page.locator('.ch-host[data-ready="true"]').waitFor();
  await page.locator('.ch-fileitem').first().waitFor();
  eq(await line(2).locator('code').innerText(), '    let a = 1;', 'the reload shows the new diff');
  await page.locator('.ch-sentlist summary').click();
  const states = await page.locator('.ch-sentreview li').evaluateAll((els) => els.map((e) => e.textContent));
  eq(states.length, 2, 'both comments are listed');
  ok(/src\/main\.rs:2–3.*Addressed in [0-9a-f]{7}/.test(states[0]), `the comment on the rewritten lines is addressed: ${states[0]}`);
  ok(/src\/main\.rs:5.*Sent/.test(states[1]) && !/Addressed/.test(states[1]), `the other stands: ${states[1]}`);
  eq(await page.locator('.ch-banner:not([hidden])').count(), 0, 'the banner is gone');
  say('ADDRESSED');

  where = 'viewports';
  await noOverflow('the changes page');
  await page.emulateMedia({ colorScheme: 'dark' });
  eq(await page.evaluate(() => getComputedStyle(document.body).backgroundColor !== 'rgb(244, 243, 239)'), true, 'dark follows the device');
  await noOverflow('the dark changes page');
  if (shots) await page.screenshot({ path: `${shots}/changes-desktop-dark.png`, fullPage: true });
  await page.emulateMedia({ colorScheme: 'light' });
  await page.setViewportSize({ width: 390, height: 844 });
  await go('#/b/app/polish');
  await go('#/changes/app/polish');
  await page.locator('.ch-host[data-ready="true"]').waitFor();
  await page.locator('.ch-table').waitFor();
  await noOverflow('the changes page on a phone');
  ok(!(await page.locator('.ch-sidebar').evaluate((d) => d.open)), 'the file list starts closed on a phone');
  const box = await page.locator('.ch-diff').boundingBox();
  ok(box.x >= 0 && box.x + box.width <= 390, `the diff fits the phone: ${JSON.stringify(box)}`);
  const inner = await page.locator('.ch-scroll').evaluate((e) => [e.scrollWidth, e.clientWidth]);
  ok(inner[0] >= inner[1], 'and scrolls inside itself');
  await page.locator('.ch-sidebar summary').click();
  await noOverflow('the file list on a phone');
  await page.locator('.ch-file[data-path="src/main.rs"]').click();
  await page.locator('.ch-sidebar summary').click();
  await gutter(4).click();
  await page.locator('.ch-composer').waitFor();
  await page.locator('.ch-input').fill('A comment typed on a phone, long enough to wrap onto a second line of the narrow box.');
  await noOverflow('the composer on a phone');
  if (shots) await page.screenshot({ path: `${shots}/changes-phone.png`, fullPage: true });
  await page.emulateMedia({ colorScheme: 'dark' });
  if (shots) await page.screenshot({ path: `${shots}/changes-phone-dark.png`, fullPage: true });
  await page.keyboard.press('Escape');
  eq(problems, [], 'the page logged no error');
  say('VIEWPORTS');
  say('DONE');
  await browser.close();
  process.exit(0);
})().catch(async (e) => {
  console.error(e && e.stack ? e.stack : e);
  process.exit(1);
});
