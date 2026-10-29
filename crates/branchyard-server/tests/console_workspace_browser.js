// Drives the console's knowledge workspaces in headless Chromium for
// tests/console_workspace_browser.rs, which runs the server and reads this
// script's stdout a line at a time: LOADED, PROJECT, WORKSPACE, SOURCES,
// SESSIONS, OUTPUTS, ACCEPTED and DONE, each once the page has shown it.
//
// PLAYWRIGHT names the playwright module; BY_URL the server; BY_CODE a
// pairing code; BY_KNOWLEDGE_TOKEN a token that reaches knowledge projects
// and no repository; BY_FIXTURES the directory tools/console_fixtures.py
// wrote; BY_SCREENSHOTS, when set, a directory for screenshots.
'use strict';

const fs = require('fs');
const path = require('path');
const { chromium } = require(process.env.PLAYWRIGHT);

const say = (text) => process.stdout.write(`${text}\n`);
const shots = process.env.BY_SCREENSHOTS;
const base = process.env.BY_URL;
const fixtures = process.env.BY_FIXTURES;
const scale = Number(process.env.BY_TEST_TIMEOUT_SCALE) || 1;
const TURN = { timeout: 60000 * scale };
const PREVIEW = '**/artifacts/*/preview*';
const CSV = 'ticket_backlog.csv';
const XLSX = 'backlog_metrics.xlsx';
const DOCX = 'backlog_report.docx';
const PPTX = 'backlog_briefing.pptx';
const files = JSON.parse(fs.readFileSync(path.join(fixtures, 'manifest.json'), 'utf8')).files;
const mediaType = (name) => files.find((f) => f.filename === name).media_type;
const format = (name) => path.extname(name).slice(1).toUpperCase();
const fixture = (name) => fs.readFileSync(path.join(fixtures, name));
const shq = (text) => `'${text.replace(/'/g, `'\\''`)}'`;
const fence = '```';
const capturePrompt = [
  'SH mkdir -p outputs',
  `cp ${files.map((f) => shq(path.join(fixtures, f.filename))).join(' ')} outputs/`,
  `printf %s ${shq(JSON.stringify({
    schema_version: 1,
    files: files.map((f) => ({ path: f.filename, media_type: f.media_type, sha256: f.sha256 })),
  }))} > outputs/artifacts.json`,
  `printf '\\n${fence}json\\n{"summary":"backlog evidence"}\\n${fence}\\n'`,
].join(' && ');
let shown = null;
let browser = null;

(async () => {
  browser = await chromium.launch();
  const context = await browser.newContext({ acceptDownloads: true });
  context.setDefaultTimeout(30000 * scale);
  const page = await context.newPage();
  shown = () => page.locator('main').innerText();
  const problems = [];
  page.on('console', (m) => { if (m.type() === 'error') problems.push(m.text()); });
  page.on('pageerror', (e) => problems.push(String(e)));
  const go = (hash) => page.evaluate((h) => { location.hash = h; }, hash);
  const hash = () => new URL(page.url()).hash;
  const toast = (text) => page.locator('.toast', { hasText: text }).first().waitFor(TURN);
  const noOverflow = async (what) => {
    if (await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)) throw new Error(`${what} overflows at ${page.viewportSize().width}px`);
  };
  const downloaded = async (click) => {
    const [download] = await Promise.all([page.waitForEvent('download', TURN), click()]);
    return fs.readFileSync(await download.path());
  };
  const sameBytes = (bytes, name, what) => {
    if (!bytes.equals(fixture(name))) throw new Error(`${what} is ${bytes.length} bytes that are not ${name}`);
  };
  const needsYou = page.getByRole('heading', { level: 1, name: 'Needs you' });
  const picker = page.locator('#sidebar').getByRole('combobox', { name: 'Source', exact: true });
  const sideLink = page.locator('#new-task');
  const strip = page.locator('header#strip');
  const titled = (text) => strip.getByRole('heading', { level: 1, name: text, exact: true });
  const composer = (mode) => page.locator(`#composer[data-mode="${mode}"]`).waitFor(TURN);
  const submit = page.locator('#composer-go');
  const submitSays = async (text) => {
    const said = (await submit.innerText()).trim();
    if (said !== text) throw new Error(`the composer button says ${JSON.stringify(said)}, not ${text}`);
  };
  const menu = strip.locator('summary[aria-label="More actions"]');
  const tabs = page.locator('nav#sessions a.session-tab');
  const selectedTab = (sid) => page.locator(`nav#sessions a.session-tab[data-session="${sid}"][aria-current="page"]`);
  const outputs = page.locator('ul#outputs > li[data-artifact]');
  const outputRow = (name) => outputs.filter({ has: page.locator('button.output-name', { hasText: name }) });
  const outputName = (name) => outputRow(name).locator('button.output-name');
  const choose = (name) => page.getByRole('checkbox', { name: `Select ${name}`, exact: true });
  const preview = page.locator('#preview-host .by-artifact-preview');
  const notice = page.locator('#preview-host .preview-missing')
    .filter({ hasText: 'This server cannot preview workspace outputs yet.' })
    .filter({ hasText: 'no such route' });

  await page.goto(`${base}/app/`);
  await page.getByRole('heading', { name: 'Connect to Branchyard' }).waitFor();
  await page.getByLabel('Server token', { exact: true }).fill(process.env.BY_KNOWLEDGE_TOKEN);
  await page.getByRole('button', { name: 'Connect to server', exact: true }).click();
  await page.getByRole('heading', { level: 1, name: 'Projects', exact: true }).waitFor();
  await page.locator('#sidebar').waitFor();
  if (await page.getByText('This token can reach no repository.').count()) throw new Error('a knowledge-only token was refused for reaching no repository');
  await picker.locator('option[value="new-project"]', { hasText: 'New project…' }).waitFor({ state: 'attached' });
  if (await picker.locator('optgroup[label="Repositories"]').count()) throw new Error('a knowledge-only token is offered repositories');
  await page.locator('#main').getByRole('link', { name: 'New project', exact: true }).waitFor();
  if (shots) await page.screenshot({ path: `${shots}/knowledge-only-home.png` });
  await go('#/settings');
  await page.getByRole('button', { name: 'Sign out', exact: true }).click();
  await page.getByRole('heading', { name: 'Connect to Branchyard' }).waitFor();

  await page.goto(`${base}/app/#pair=${process.env.BY_CODE}`);
  await page.getByRole('heading', { level: 1, name: 'app', exact: true }).waitFor();
  await picker.locator('optgroup[label="Repositories"] option[value="repo:app"]').waitFor({ state: 'attached' });
  await sideLink.filter({ hasText: 'New task' }).waitFor();
  await page.route('**/v1/projects', (route) => (route.request().method() === 'GET'
    ? route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: { code: 'unavailable', message: 'Fixture projects unavailable.' } }) })
    : route.continue()));
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.getByRole('heading', { level: 1, name: 'app', exact: true }).waitFor();
  await toast('Projects did not load: Fixture projects unavailable.');
  if (await picker.locator('option[value="new-project"]').count()) throw new Error('New project… is offered while the project list failed');
  await page.unroute('**/v1/projects');
  await page.reload({ waitUntil: 'domcontentloaded' });
  await picker.locator('option[value="new-project"]').waitFor({ state: 'attached' });
  say('LOADED');

  await picker.selectOption('new-project');
  await page.getByRole('heading', { level: 1, name: 'New project', exact: true }).waitFor();
  if (hash() !== '#/p/new') throw new Error(`New project… opened ${hash()}`);
  const form = page.locator('#main');
  const source = form.getByLabel('Source', { exact: true });
  await source.locator('option[value="repo:app"]', { hasText: 'Repository app' }).waitFor({ state: 'attached' });
  await form.getByLabel('Project name', { exact: true }).fill('Quarterly research');
  await source.selectOption({ label: 'Knowledge work (no repository)' });
  await form.getByLabel('Results', { exact: true }).selectOption('artifacts');
  if (shots) await page.screenshot({ path: `${shots}/new-project.png` });
  await form.getByRole('button', { name: 'Create project', exact: true }).click();
  await page.waitForURL((url) => /^#\/p\/[^/]+$/.test(url.hash) && url.hash !== '#/p/new', TURN);
  const pid = hash().slice('#/p/'.length);
  await page.getByRole('heading', { level: 1, name: 'Quarterly research', exact: true }).waitFor();
  await page.locator('#main .lede', { hasText: 'Knowledge project' }).waitFor();
  await picker.locator(`optgroup[label="Projects"] option[value="project:${pid}"]`).waitFor({ state: 'attached' });
  say('PROJECT');

  await sideLink.filter({ hasText: 'New workspace' }).waitFor();
  const newWorkspace = await sideLink.getAttribute('href');
  if (newWorkspace !== `#/p/${pid}/new`) throw new Error(`New workspace links to ${newWorkspace}`);
  await sideLink.click();
  await page.waitForURL((url) => url.hash === `#/p/${pid}/new`);
  const title = page.getByLabel('Workspace title', { exact: true });
  await page.waitForFunction(() => {
    const focused = document.activeElement;
    const labels = [...(focused.labels || [])].map((label) => label.textContent.trim());
    return labels.includes('Workspace title') || focused.getAttribute('aria-label') === 'Workspace title';
  });
  // Holding the request is what makes the in-progress label observable.
  let releaseCreate;
  const creating = new Promise((resolve) => { releaseCreate = resolve; });
  await page.route('**/v1/workspaces', async (route) => {
    if (route.request().method() === 'POST') await creating;
    await route.continue();
  });
  await title.fill('Backlog review');
  await page.getByRole('button', { name: 'Create workspace', exact: true }).click();
  await page.getByRole('button', { name: 'Creating…', exact: true }).waitFor();
  releaseCreate();
  await page.waitForURL((url) => /^#\/w\/[^/]+$/.test(url.hash), TURN);
  await page.unroute('**/v1/workspaces');
  const ws = hash().slice('#/w/'.length);
  await titled('Backlog review').waitFor(TURN);
  await strip.locator('p.state', { hasText: 'Ready' }).waitFor(TURN);
  const meta = await strip.locator('p.strip-meta').innerText();
  if (!meta.includes('Quarterly research') || !meta.includes('revision')) throw new Error(`the strip says ${JSON.stringify(meta)}`);
  // A desktop shows the sidebar instead; the link is for one column.
  await strip.locator('a.back', { hasText: '‹ Workspaces' }).waitFor({ state: 'attached' });
  const row = page.locator(`#branch-list a.branch-row[data-workspace="${ws}"]`);
  await row.locator('.row-name', { hasText: 'Backlog review' }).waitFor();
  if (await row.getAttribute('href') !== `#/w/${ws}`) throw new Error(`the workspace row links to ${await row.getAttribute('href')}`);
  await page.getByRole('searchbox', { name: 'Filter workspaces', exact: true }).waitFor();
  say('WORKSPACE');

  await page.locator('#tab-sources').click();
  await page.getByRole('button', { name: 'Upload sources', exact: true }).waitFor();
  await page.setInputFiles('#source-file', [CSV, DOCX].map((name) => path.join(fixtures, name)));
  await toast('Uploaded 2 sources.');
  const sources = page.locator('ul#sources > li[data-source]');
  await sources.nth(1).waitFor();
  if (await sources.count() !== 2) throw new Error(`${await sources.count()} sources are listed after uploading 2`);
  for (const name of [CSV, DOCX]) {
    const listed = sources.filter({ hasText: name });
    if (!(await listed.innerText()).includes(format(name))) throw new Error(`source ${name} shows ${JSON.stringify(await listed.innerText())}`);
    if (await listed.locator('.item-meta').getAttribute('title') !== mediaType(name)) throw new Error(`source ${name} does not name ${mediaType(name)}`);
  }
  sameBytes(await downloaded(() => page.getByRole('button', { name: `Download ${CSV}`, exact: true }).click()), CSV, 'the downloaded source');
  say('SOURCES');

  const startSession = async (name, prompt, collect) => {
    await page.locator('nav#sessions').getByRole('button', { name: '+ Session', exact: true }).click();
    const session = page.locator('#session-form');
    await session.getByLabel('Session name', { exact: true }).fill(name);
    const harness = session.getByLabel('Harness', { exact: true });
    if (await harness.evaluate((control) => control.tagName === 'SELECT')) await harness.selectOption('gemini-cli');
    else await harness.fill('gemini-cli');
    await session.getByLabel('Access', { exact: true }).selectOption('writer');
    await session.getByLabel('First prompt', { exact: true }).fill(prompt);
    for (const upload of [CSV, DOCX]) {
      if (!(await session.getByRole('checkbox', { name: upload, exact: true }).isChecked())) throw new Error(`source ${upload} is not attached by default`);
    }
    const capture = session.getByRole('checkbox', { name: 'Collect outputs (outputs/artifacts.json)', exact: true });
    if (!(await capture.isChecked())) throw new Error('an outputs project does not collect outputs by default');
    await capture.setChecked(collect);
    if (shots && collect) await page.screenshot({ path: `${shots}/session-form.png` });
    const before = hash();
    await session.getByRole('button', { name: 'Start session', exact: true }).click();
    await toast('Session started.');
    await page.waitForURL((url) => url.hash.startsWith(`#/w/${ws}/s/`) && url.hash !== before, TURN);
    const sid = hash().slice(`#/w/${ws}/s/`.length);
    await selectedTab(sid).filter({ hasText: name }).waitFor();
    return sid;
  };

  const analystSid = await startSession('Backlog analyst', capturePrompt, true);
  const shReply = page.locator('#convo .turn .agent', { hasText: /sh: \d+/ });
  await shReply.waitFor(TURN);
  if (!(await shReply.innerText()).includes('sh: 0')) throw new Error(`the capture command failed: ${await shReply.innerText()}`);
  await composer('send');

  const steeringSid = await startSession('Steering check', 'AWAIT_STEER', false);
  await page.locator('#convo .turn .agent', { hasText: 'waiting for steering' }).waitFor(TURN);
  await composer('steer');
  const steerBox = page.getByLabel('Steer the running turn', { exact: true });
  await steerBox.fill('focus on totals');
  for (let poll = 0; poll < 2; poll++) await page.waitForResponse((r) => new URL(r.url()).pathname === `/v1/workspaces/${ws}`, TURN);
  await composer('steer');
  const kept = await steerBox.evaluate((area) => [area === document.activeElement, area.value]);
  if (JSON.stringify(kept) !== JSON.stringify([true, 'focus on totals'])) throw new Error(`a live update changed the message box: ${JSON.stringify(kept)}`);
  await submitSays('Steer');
  await submit.click();
  await toast('Steering sent. The running turn reads it next.');
  await page.locator('#convo .turn .agent', { hasText: 'steered: focus on totals' }).waitFor(TURN);
  await composer('send');

  await page.getByLabel('Send a follow-up', { exact: true }).fill('HANG');
  await submitSays('Send');
  await submit.click();
  await toast('Sent. A new turn is starting.');
  await page.locator('#convo .turn .you', { hasText: 'HANG' }).waitFor(TURN);
  await composer('steer');
  await menu.click();
  await strip.getByRole('button', { name: 'Cancel turn', exact: true }).click();
  await page.locator('#confirm-ok').click();
  await toast('Cancelling the running turn.');
  await toast('Turn cancelled.');
  await composer('resume');
  await page.getByLabel('Resume the interrupted turn', { exact: true }).waitFor();
  await submitSays('Resume');
  const turnsBeforeResume = await page.locator('#convo .turn').count();
  await submit.click();
  await toast('Resuming. A new turn is starting.');
  await page.waitForFunction((n) => document.querySelectorAll('#convo .turn').length > n, turnsBeforeResume, TURN);
  await composer('send');

  if (await tabs.count() !== 2) throw new Error(`${await tabs.count()} session tabs, not 2`);
  await tabs.filter({ hasText: 'Backlog analyst' }).click();
  await selectedTab(analystSid).waitFor();
  await page.locator('#convo .turn .you', { hasText: 'mkdir -p outputs' }).waitFor();
  if (await page.locator('#convo .turn .you', { hasText: 'AWAIT_STEER' }).count()) throw new Error('the analyst session shows the steering session\'s turns');
  await tabs.filter({ hasText: 'Steering check' }).click();
  await selectedTab(steeringSid).waitFor();
  await page.locator('#convo .turn .you', { hasText: 'AWAIT_STEER' }).waitFor();
  if (await page.locator('#convo .turn .you', { hasText: 'mkdir -p outputs' }).count()) throw new Error('the steering session shows the analyst session\'s turns');
  say('SESSIONS');

  await page.locator('#tab-outputs').click();
  await outputs.nth(3).waitFor(TURN);
  if (await outputs.count() !== 4) throw new Error(`${await outputs.count()} outputs, not 4`);
  const artifact = {};
  for (const name of [CSV, XLSX, DOCX, PPTX]) {
    artifact[name] = await outputRow(name).getAttribute('data-artifact');
    const label = (await outputName(name).innerText()).trim();
    if (label !== name) throw new Error(`output ${name} is labelled ${JSON.stringify(label)}`);
    if (!(await outputRow(name).innerText()).includes(format(name))) throw new Error(`output ${name} does not show ${format(name)}`);
    if (await outputRow(name).locator('.item-meta').getAttribute('title') !== mediaType(name)) throw new Error(`output ${name} does not name ${mediaType(name)}`);
  }
  const previewPath = (name) => `/v1/workspaces/${ws}/artifacts/${artifact[name]}/preview`;
  const previewMissing = async (name) => {
    const [response] = await Promise.all([
      page.waitForResponse((r) => new URL(r.url()).pathname === previewPath(name), TURN),
      outputName(name).click(),
    ]);
    if (response.status() !== 404) throw new Error(`the preview of ${name} answered ${response.status()}`);
    await page.locator('#preview-title', { hasText: name }).waitFor();
    if (await outputName(name).getAttribute('aria-pressed') !== 'true') throw new Error(`${name} is not marked as previewed`);
    await notice.waitFor();
    await preview.waitFor({ state: 'detached' });
    await page.getByRole('button', { name: 'Download original', exact: true }).waitFor();
  };
  for (const name of [CSV, XLSX, DOCX, PPTX]) await previewMissing(name);

  // Never answered, so each preview stays in flight until the page abandons it.
  await page.route(PREVIEW, () => {});
  const abandon = async (name, leave, what) => {
    await page.locator('#tab-outputs').click();
    const failed = page.waitForEvent('requestfailed', { predicate: (r) => new URL(r.url()).pathname === previewPath(name), ...TURN });
    await outputName(name).click();
    await preview.waitFor();
    await leave();
    await failed;
    if (await preview.count()) throw new Error(`the preview of ${name} outlived ${what}`);
  };
  await abandon(DOCX, async () => { await go('#/inbox'); await needsYou.waitFor(); }, 'leaving the workspace');
  await go(`#/w/${ws}`);
  await titled('Backlog review').waitFor();
  await abandon(XLSX, () => page.locator('#tab-sources').click(), 'switching to Sources');
  await page.unroute(PREVIEW);
  say('OUTPUTS');

  await page.locator('#tab-outputs').click();
  await previewMissing(CSV);
  sameBytes(await downloaded(() => page.getByRole('button', { name: 'Download original', exact: true }).click()), CSV, 'the downloaded original');
  await choose(CSV).check();
  await strip.getByRole('button', { name: 'Accept 1 selected', exact: true }).waitFor();
  await choose(XLSX).check();
  await strip.getByRole('button', { name: 'Accept 2 selected', exact: true }).click();
  const ok = page.locator('#confirm-ok');
  await ok.waitFor();
  if ((await ok.innerText()).trim() !== 'Accept') throw new Error(`the confirmation says ${await ok.innerText()}`);
  await ok.click();
  await toast('Accepted 2 outputs.');
  for (const name of [CSV, XLSX]) {
    await outputRow(name).locator('.tag', { hasText: 'Accepted' }).waitFor(TURN);
    if (!(await choose(name).isDisabled())) throw new Error(`accepted ${name} can still be selected`);
  }
  const accepted = await page.evaluate(async (id) => {
    const response = await fetch(`/v1/workspaces/${id}`, { headers: { Authorization: `Bearer ${sessionStorage.getItem('branchyard.token')}` } });
    if (!response.ok) throw new Error(`GET /v1/workspaces/${id} answered ${response.status}`);
    return (await response.json()).accepted_deliverables.map((d) => d.artifact_id).sort();
  }, ws);
  const want = [artifact[CSV], artifact[XLSX]].sort();
  if (JSON.stringify(accepted) !== JSON.stringify(want)) throw new Error(`the server accepted ${JSON.stringify(accepted)}, not ${JSON.stringify(want)}`);
  say('ACCEPTED');

  await menu.click();
  const sleep = strip.getByRole('button', { name: /^Sleep/ });
  await sleep.waitFor();
  if (!(await sleep.isDisabled())) throw new Error('Sleep is offered without the provider capability');
  if (!(await sleep.innerText()).includes('provider_capability_required')) throw new Error(`Sleep says ${JSON.stringify(await sleep.innerText())}`);
  await strip.getByRole('button', { name: 'Archive', exact: true }).waitFor();
  if (shots) await page.screenshot({ path: `${shots}/workspace-menu.png` });
  if (await strip.getByRole('button', { name: 'Restore', exact: true }).count()) throw new Error('Restore is offered for a workspace that is not archived');
  if (await strip.getByRole('button', { name: 'Cancel turn', exact: true }).count()) throw new Error('Cancel turn is offered with no running turn');
  await menu.click();
  await sleep.waitFor({ state: 'hidden' });

  await go('#/inbox');
  await needsYou.waitFor();
  await row.click();
  await titled('Backlog review').waitFor();
  if (!hash().startsWith(`#/w/${ws}`)) throw new Error(`the sidebar row opened ${hash()}`);
  await go(`#/p/${pid}`);
  await page.getByRole('heading', { level: 2, name: 'Workspaces', exact: true }).waitFor();
  await page.locator('#main h1').focus();
  await page.keyboard.press('/');
  if (!(await page.getByRole('searchbox', { name: 'Filter workspaces', exact: true }).evaluate((input) => input === document.activeElement))) throw new Error('/ did not focus the workspace filter');
  await page.locator('#main h1').focus();
  await page.keyboard.press('j');
  if (await page.evaluate(() => document.activeElement.dataset.workspace || null) !== ws) throw new Error('j did not focus the workspace row');
  await page.locator(`#main a.card-link[href="#/w/${ws}"]`, { hasText: 'Backlog review' }).click();
  await titled('Backlog review').waitFor();
  for (const [sid, sent, replied] of [[analystSid, 'mkdir -p outputs', 'sh: 0'], [steeringSid, 'AWAIT_STEER', 'steered: focus on totals']]) {
    await go(`#/w/${ws}/s/${sid}`);
    await selectedTab(sid).waitFor();
    await page.reload();
    await titled('Backlog review').waitFor();
    await selectedTab(sid).waitFor();
    await page.locator('#convo .turn .you', { hasText: sent }).waitFor();
    await page.locator('#convo .turn .agent', { hasText: replied }).waitFor();
  }

  await page.locator('#tab-details').click();
  await page.locator('#ws-sessions li', { hasText: 'Steering check' }).waitFor();
  if (shots) await page.screenshot({ path: `${shots}/workspace-details.png` });
  for (const route of ['#/inbox', '#/approvals']) {
    await go(route);
    await needsYou.waitFor();
  }
  await go('#/b/app/first');
  await titled('first').waitFor();
  await page.getByRole('searchbox', { name: 'Filter branches', exact: true }).waitFor();
  await go(`#/w/${ws}`);
  await titled('Backlog review').waitFor();

  const showNotice = async (name) => {
    await page.locator('#tab-outputs').click();
    if (await outputName(name).getAttribute('aria-pressed') !== 'true') await outputName(name).click();
    await notice.waitFor();
    await notice.scrollIntoViewIfNeeded();
  };
  const reopen = async () => {
    await go('#/inbox');
    await needsYou.waitFor();
    await go(`#/w/${ws}`);
    await titled('Backlog review').waitFor();
  };
  await page.setViewportSize({ width: 390, height: 844 });
  await reopen();
  if (await page.locator('#sidebar').isVisible()) throw new Error('the sidebar shows beside a workspace at 390px');
  await strip.locator('a.back', { hasText: '‹ Workspaces' }).waitFor();
  await noOverflow('a workspace');
  await showNotice(CSV);
  await noOverflow('workspace outputs');

  if (shots) {
    const desktop = [
      ['project-glance', async () => {
        await go(`#/p/${pid}`);
        await page.locator(`#main a.card-link[href="#/w/${ws}"]`).waitFor();
      }],
      ['workspace-sessions', async () => {
        await reopen();
        await tabs.nth(1).waitFor();
      }],
      ['workspace-sources', async () => {
        await page.locator('#tab-sources').click();
        await page.locator('ul#sources > li[data-source]').nth(1).waitFor();
      }],
      ['workspace-outputs', () => showNotice(CSV)],
    ];
    const phone = [
      ['phone-workspace', reopen],
      ['phone-outputs', () => showNotice(CSV)],
    ];
    for (const [width, height, views] of [[1440, 900, desktop], [390, 844, phone]]) {
      await page.setViewportSize({ width, height });
      for (const scheme of ['light', 'dark']) {
        await page.emulateMedia({ colorScheme: scheme });
        for (const [name, ready] of views) {
          await ready();
          await page.screenshot({ path: `${shots}/${name}-${scheme}.png` });
        }
      }
    }
    await page.emulateMedia({ colorScheme: 'light' });
  }

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
