// Drives the console's knowledge workspaces in headless Chromium for
// tests/console_workspace_browser.rs, which runs the server and reads this
// script's stdout a line at a time: LOADED, PROJECT, WORKSPACE, SOURCES,
// SESSIONS, OUTPUTS, ACCEPTED, STALE and DONE, each once the page has shown
// it. Previews come from the server's own renderer: the script observes the
// page's preview requests and checks the page shows what each answer means,
// and writes which formats rendered to stderr.
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
const SOURCE_PREVIEW = '**/sources/*/preview*';
const CSV = 'ticket_backlog.csv';
const XLSX = 'backlog_metrics.xlsx';
const DOCX = 'backlog_report.docx';
const PPTX = 'backlog_briefing.pptx';
const NOTES = 'meeting-notes.rtf';
// CSV's bytes under a name with accents, an apostrophe and parentheses
// (which encodeURIComponent leaves bare), CJK and an emoji.
const UNICODE = 'Étude d\'été (T3) — 季度报告 📊.csv';
const UNICODE_ENCODED = 'UTF-8\'\'%C3%89tude%20d%27%C3%A9t%C3%A9%20%28T3%29%20%E2%80%94%20%E5%AD%A3%E5%BA%A6%E6%8A%A5%E5%91%8A%20%F0%9F%93%8A.csv';
// The answers a working server may give for each format, whether or not this
// machine has the document renderer; anything else is a failure of the page.
const ALLOWED = {
  [CSV]: ['table', 'renderer unavailable'],
  [UNICODE]: ['table', 'renderer unavailable'],
  [XLSX]: ['table', 'renderer unavailable'],
  [DOCX]: ['pages', 'conversion failed', 'renderer unavailable'],
  [PPTX]: ['pages', 'conversion failed', 'renderer unavailable'],
  [NOTES]: ['unsupported'],
};
const answerOf = (status, body) => {
  const error = (body && body.error) || {};
  if (status === 200 && body.status === 'passed') return body.kind;
  if (status === 200 && body.status === 'failed') return 'conversion failed';
  if ((status === 200 && body.status === 'unavailable') || (status === 503 && error.code === 'preview_unavailable')) return 'renderer unavailable';
  if (status === 422 && error.code === 'preview_unsupported') return 'unsupported';
  return `HTTP ${status} ${error.code || ''}`.trim();
};
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
  const outputs = page.locator('#outputs-list li[data-artifact]');
  const outputRow = (name) => outputs.filter({ has: page.locator('button.file-name', { hasText: name }) });
  const outputName = (name) => outputRow(name).locator('button.file-name');
  const choose = (name) => page.getByRole('checkbox', { name: `Select ${name}`, exact: true });
  const byId = (id) => page.locator(`#outputs-list li[data-artifact="${id}"]`);
  const preview = page.locator('#preview-host .by-artifact-preview');
  const settled = preview.locator('.by-preview-table, .by-preview-page-img, .by-preview-failure').first();
  const failure = preview.locator('.by-preview-failure');
  const focusedPreview = () => page.evaluate(() => document.activeElement.dataset.preview || null);
  const said = (pattern) => page.waitForFunction((source) => new RegExp(source)
    .test((document.querySelector('#preview-host .by-preview-sr-only[aria-live="polite"]') || {}).textContent || ''), pattern.source);
  const callApi = (method, pathname, body) => page.evaluate(async ([method, pathname, body]) => {
    const headers = { Authorization: `Bearer ${sessionStorage.getItem('branchyard.token')}` };
    if (body) Object.assign(headers, { 'Content-Type': 'application/json', 'Idempotency-Key': crypto.randomUUID() });
    const response = await fetch(pathname, { method, headers, body: body ? JSON.stringify(body) : undefined });
    return { status: response.status, body: await response.json().catch(() => null) };
  }, [method, pathname, body]);
  // page.waitForFunction would take an async predicate's promise as truthy
  // and return at once, so an operation is awaited here.
  const awaitOperation = async (id) => {
    for (const deadline = Date.now() + TURN.timeout; ;) {
      const op = (await callApi('GET', `/v1/operations/${id}?wait=5`)).body;
      if (!['queued', 'running'].includes(op.state)) return op;
      if (Date.now() > deadline) throw new Error(`operation ${id} is still ${op.state}`);
    }
  };
  const previewed = async (click, pathname) => {
    const [response] = await Promise.all([page.waitForResponse((r) => new URL(r.url()).pathname === pathname, TURN), click()]);
    if (!/^Bearer .+/.test(response.request().headers().authorization || '')) throw new Error(`the preview at ${pathname} omitted its bearer token`);
    const body = await response.json().catch(() => null);
    return { status: response.status(), body, answer: answerOf(response.status(), body) };
  };
  const SHOWN = {
    table: async (name) => {
      await preview.getByRole('columnheader', { name: 'ticket_id', exact: true }).waitFor();
      await preview.getByRole('cell', { name: '00451', exact: true }).waitFor();
      if (name === CSV) await preview.getByRole('cell', { name: "<script>alert('reported')</script> seen in the log copy", exact: true }).waitFor();
      if (name === XLSX) {
        const sheets = await preview.getByRole('combobox', { name: 'Select sheet' }).locator('option').allInnerTexts();
        if (JSON.stringify(sheets) !== JSON.stringify(['Backlog', 'Summary'])) throw new Error(`the XLSX preview offers sheets ${JSON.stringify(sheets)}`);
        const warned = (await preview.locator('.by-preview-warning-banner').textContent()) || '';
        if (/Truncated columns/.test(warned)) throw new Error(`the XLSX preview of a narrow workbook warns ${JSON.stringify(warned)}`);
      }
      await said(/^Preview ready: \d+ rows?\.$/);
    },
    pages: async () => {
      const image = preview.locator('img.by-preview-page-img');
      await image.waitFor();
      await image.evaluate((img) => (img.complete && img.naturalWidth > 0 ? Promise.resolve() : new Promise((resolve, reject) => {
        img.addEventListener('load', resolve, { once: true });
        img.addEventListener('error', () => reject(new Error('the rendered page did not load')), { once: true });
      })));
      await said(/^Preview ready: page 1 of \d+\.$/);
    },
    'conversion failed': async (name, got) => {
      await failure.locator('.by-preview-state-message', { hasText: 'This file could not be converted for preview' }).waitFor();
      await failure.getByText(got.body.error.message, { exact: true }).waitFor();
      await said(/^This file could not be converted for preview$/);
    },
    'renderer unavailable': async (name, got) => {
      await failure.locator('.by-preview-state-message', { hasText: 'This server cannot make previews right now' }).waitFor();
      await failure.getByText('Download the original to open it on this device.', { exact: true }).waitFor();
      const operator = failure.locator('details.by-preview-operator');
      await operator.locator('summary', { hasText: 'Details for the server\'s operator' }).click();
      await operator.getByText(got.body.error.message, { exact: true }).waitFor();
      await said(/^This server cannot make previews right now$/);
    },
    unsupported: async () => {
      await failure.locator('.by-preview-state-message', { hasText: 'No preview for this format' }).waitFor();
      await failure.getByText('Download the original to open it on this device.', { exact: true }).waitFor();
      await said(/^No preview for this format$/);
    },
  };
  const expectShown = async (name, got) => {
    if (!ALLOWED[name].includes(got.answer)) throw new Error(`the preview of ${name} answered ${got.answer}`);
    await page.locator('#preview-title', { hasText: name }).waitFor();
    await page.locator('#preview .preview-head').getByRole('button', { name: 'Download original', exact: true }).waitFor();
    await SHOWN[got.answer](name, got);
  };
  const rendered = [];
  const told = (what, got) => rendered.push(`${what}: ${got.answer}${got.body && got.body.error ? ` (${got.body.error.message})` : ''}`);

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
  const uploadSources = page.locator('#inspector').getByRole('button', { name: 'Upload sources', exact: true });
  await uploadSources.waitFor();
  await page.locator('#guide').getByRole('button', { name: 'Upload sources', exact: true }).waitFor();
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
  fs.writeFileSync(path.join(fixtures, NOTES), '{\\rtf1\\ansi Minutes of the backlog review.}');
  await page.setInputFiles('#source-file', path.join(fixtures, NOTES));
  await toast('Uploaded 1 source.');
  await sources.nth(2).waitFor();
  const sourceId = {};
  for (const name of [CSV, DOCX, NOTES]) sourceId[name] = await sources.filter({ hasText: name }).getAttribute('data-source');
  const notesMeta = await sources.filter({ hasText: NOTES }).locator('.item-meta').innerText();
  if (!notesMeta.startsWith('RTF · ')) throw new Error(`the RTF source shows ${JSON.stringify(notesMeta)}, not its format`);
  const sourcePath = (name) => `/v1/workspaces/${ws}/sources/${sourceId[name]}/preview`;
  const sourceName = (name) => sources.filter({ hasText: name }).locator('button.file-name');
  await uploadSources.focus();
  await page.keyboard.press('Tab');
  if (await focusedPreview() !== `source:${sourceId[CSV]}`) throw new Error(`Tab from Upload sources reached ${await focusedPreview()}, not the first source`);
  const csvSource = await previewed(() => page.keyboard.press('Enter'), sourcePath(CSV));
  await expectShown(CSV, csvSource);
  if (await sourceName(CSV).getAttribute('aria-pressed') !== 'true') throw new Error('the previewed source is not marked');
  if (!(await page.locator('#panel-sources #preview-host').count())) throw new Error('the source preview is not in the Sources panel');
  told('CSV source', csvSource);
  const notes = await previewed(() => sourceName(NOTES).click(), sourcePath(NOTES));
  await expectShown(NOTES, notes);
  if (await sourceName(CSV).getAttribute('aria-pressed') !== 'false') throw new Error('the previous source is still marked previewed');
  sameBytes(await downloaded(() => page.getByRole('button', { name: 'Download original', exact: true }).click()), NOTES, 'the unsupported source\'s original');
  const [unicodeUpload] = await Promise.all([
    page.waitForRequest((r) => r.method() === 'POST' && new URL(r.url()).pathname === `/v1/workspaces/${ws}/sources`, TURN),
    page.setInputFiles('#source-file', { name: UNICODE, mimeType: 'text/csv', buffer: fixture(CSV) }),
  ]);
  const titleHeaders = unicodeUpload.headers();
  if (titleHeaders['x-source-title-encoded'] !== UNICODE_ENCODED) throw new Error(`the upload sent X-Source-Title-Encoded ${titleHeaders['x-source-title-encoded']}`);
  if (titleHeaders['x-source-title'] !== '_tude d\'_t_ (T3) _ ____ _.csv') throw new Error(`the upload sent X-Source-Title ${titleHeaders['x-source-title']}`);
  await sources.nth(3).waitFor();
  const unicodeName = sourceName(UNICODE);
  if (await unicodeName.textContent() !== UNICODE) throw new Error(`the source is listed as ${JSON.stringify(await unicodeName.textContent())}`);
  sourceId[UNICODE] = await sources.filter({ hasText: UNICODE }).getAttribute('data-source');
  const unicodeSource = await previewed(() => unicodeName.click(), sourcePath(UNICODE));
  await expectShown(UNICODE, unicodeSource);
  const previewTitle = await page.locator('#preview-title').textContent();
  if (previewTitle !== UNICODE) throw new Error(`the preview is titled ${JSON.stringify(previewTitle)}`);
  told('Unicode-titled CSV source', unicodeSource);
  if (shots) await page.screenshot({ path: `${shots}/unicode-source.png` });
  sameBytes(await downloaded(() => page.getByRole('button', { name: `Download ${UNICODE}`, exact: true }).click()), CSV, 'the Unicode-titled source');
  // Held, never answered, so each preview is still loading when the page moves on.
  await page.route(SOURCE_PREVIEW, () => {});
  const dropped = (name) => page.waitForEvent('requestfailed', { predicate: (r) => new URL(r.url()).pathname === sourcePath(name), ...TURN });
  const docxDropped = dropped(DOCX);
  await sourceName(DOCX).click();
  await preview.waitFor();
  await sourceName(CSV).click();
  await docxDropped;
  if (await preview.count() !== 1) throw new Error(`${await preview.count()} previews after choosing another source`);
  await page.locator('#preview-title', { hasText: CSV }).waitFor();
  const csvDropped = dropped(CSV);
  await page.locator('#tab-outputs').click();
  await csvDropped;
  if (await preview.count()) throw new Error('the source preview outlived switching to Outputs');
  await page.unroute(SOURCE_PREVIEW);
  await page.locator('#tab-sources').click();
  say('SOURCES');

  const startSession = async (name, prompt, collect) => {
    await page.locator('nav#sessions').getByRole('button', { name: '+ Session', exact: true }).click();
    const session = page.locator('#session-form');
    const environment = session.getByRole('radio', { name: 'This server', exact: true });
    if (!(await environment.isChecked())) throw new Error('the only environment, this server, is not chosen');
    await session.getByLabel('Session name', { exact: true }).fill(name);
    const harness = session.getByLabel('Harness', { exact: true });
    if (await harness.evaluate((control) => control.tagName === 'SELECT')) await harness.selectOption('gemini-cli');
    else await harness.fill('gemini-cli');
    await session.locator('#sf-advanced > summary').click();
    await session.getByLabel('Access', { exact: true }).selectOption('writer');
    await session.locator('#sf-advanced > summary').click();
    await session.getByLabel('First prompt', { exact: true }).fill(prompt);
    for (const upload of [CSV, DOCX]) {
      if (!(await session.getByRole('checkbox', { name: upload, exact: true }).isChecked())) throw new Error(`source ${upload} is not attached by default`);
    }
    const capture = session.getByRole('checkbox', { name: 'Collect outputs (outputs/artifacts.json)', exact: true });
    if (!(await capture.isChecked())) throw new Error('an outputs project does not collect outputs by default');
    await capture.setChecked(collect);
    if (collect) {
      if (await session.getByRole('checkbox', { name: 'I accept tools without per-tool approval', exact: true }).count()) throw new Error('a harness with tool approvals asks for the acknowledgement');
      const model = session.getByLabel('Model (optional)', { exact: true });
      await model.fill('knowledge-model');
      const sessionRoute = `**/v1/workspaces/${ws}/sessions`;
      let refusedBody = null;
      await page.route(sessionRoute, async (route) => {
        refusedBody = route.request().postDataJSON();
        await route.fulfill({ status: 403, contentType: 'application/json', body: JSON.stringify({ error: { code: 'provider_not_allowed', message: 'Fixture operator refused this request.' } }) });
      });
      await session.getByRole('button', { name: 'Start session', exact: true }).click();
      await session.getByRole('alert').filter({ hasText: 'Fixture operator refused this request. Choose another environment or harness, or ask the operator.' }).waitFor();
      await page.unroute(sessionRoute);
      if (refusedBody.model !== 'knowledge-model') throw new Error(`the free-text model was lost: ${JSON.stringify(refusedBody)}`);
      if ('preset' in refusedBody || 'unapproved_tools' in refusedBody) throw new Error(`the default environment sent ${JSON.stringify(refusedBody)}`);
      const kept = [await session.getByLabel('Session name', { exact: true }).inputValue(), await session.getByLabel('First prompt', { exact: true }).inputValue()];
      if (JSON.stringify(kept) !== JSON.stringify([name, prompt])) throw new Error(`a refused start changed the draft: ${JSON.stringify(kept)}`);
      await model.fill('');
    }
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
  const groups = page.locator('#outputs-list section.output-group');
  if (await groups.count() !== 1) throw new Error(`the outputs of one session show in ${await groups.count()} groups`);
  await groups.getByRole('heading', { level: 3, name: /^Backlog analyst/ }).waitFor();
  const artifact = {};
  for (const name of [CSV, XLSX, DOCX, PPTX]) {
    artifact[name] = await outputRow(name).getAttribute('data-artifact');
    const label = (await outputName(name).innerText()).trim();
    if (label !== name) throw new Error(`output ${name} is labelled ${JSON.stringify(label)}`);
    if (!(await outputRow(name).innerText()).includes(format(name))) throw new Error(`output ${name} does not show ${format(name)}`);
    if (await outputRow(name).locator('.item-meta').getAttribute('title') !== mediaType(name)) throw new Error(`output ${name} does not name ${mediaType(name)}`);
    if (await outputRow(name).getAttribute('data-standing') !== 'current') throw new Error(`output ${name} of the finished turn is not current`);
    await outputRow(name).locator('.tag', { hasText: 'Current' }).waitFor();
    if (!(await outputRow(name).locator('.item-meta').innerText()).endsWith('turn 1')) throw new Error(`output ${name} does not name its turn`);
  }
  const previewPath = (id) => `/v1/workspaces/${ws}/artifacts/${id}/preview`;
  for (const name of [CSV, XLSX, DOCX, PPTX]) {
    const got = await previewed(() => outputName(name).click(), previewPath(artifact[name]));
    if (await outputName(name).getAttribute('aria-pressed') !== 'true') throw new Error(`${name} is not marked as previewed`);
    if (!(await page.locator('#panel-outputs #preview-host').count())) throw new Error('the output preview is not in the Outputs panel');
    await expectShown(name, got);
    told(`${format(name)} output`, got);
  }
  process.stderr.write(`previews from this server: ${rendered.join('; ')}\n`);

  // Held, never answered, so each preview stays in flight until the page abandons it.
  await page.route(PREVIEW, () => {});
  const abandon = async (name, leave, what) => {
    await page.locator('#tab-outputs').click();
    const failed = page.waitForEvent('requestfailed', { predicate: (r) => new URL(r.url()).pathname === previewPath(artifact[name]), ...TURN });
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

  await page.locator('#tab-outputs').click();
  await choose(CSV).focus();
  await page.keyboard.press('Space');
  await strip.getByRole('button', { name: 'Accept 1 selected', exact: true }).waitFor();
  await page.keyboard.press('Space');
  await strip.locator('#accept').waitFor({ state: 'detached' });
  await page.keyboard.press('Tab');
  if (await focusedPreview() !== `output:${artifact[CSV]}`) throw new Error(`Tab from an output's checkbox reached ${await focusedPreview()}, not its name`);
  await expectShown(CSV, await previewed(() => page.keyboard.press('Enter'), previewPath(artifact[CSV])));
  say('OUTPUTS');

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

  const workspaceRoute = `**/v1/workspaces/${ws}`;
  let presentedRole = 'viewer';
  const presentRole = async (route) => {
    const response = await route.fetch();
    const body = await response.json();
    body.current_role = presentedRole;
    body.capabilities.can_run = false;
    body.capabilities.can_comment = presentedRole === 'reviewer';
    await route.fulfill({ response, json: body });
  };
  // A reload abandons a poll this handler still holds, and that request
  // needs no answer.
  await page.route(workspaceRoute, (route) => presentRole(route).catch((e) => {
    if (!/Route is already handled/.test(e.message)) throw e;
  }));
  await page.reload();
  await titled('Backlog review').waitFor();
  if (!(await page.getByRole('button', { name: 'Upload sources', exact: true }).isDisabled())) throw new Error('a viewer can upload sources');
  if (await page.getByRole('button', { name: '+ Session', exact: true }).count()) throw new Error('a viewer is offered a new session');
  if (!(await page.locator('#composer-go').isDisabled())) throw new Error('a viewer can continue a session');
  await page.locator('#tab-outputs').click();
  await choose(DOCX).waitFor();
  if (!(await choose(DOCX).isDisabled())) throw new Error('a viewer can select an output for acceptance');
  presentedRole = 'reviewer';
  await page.reload();
  await titled('Backlog review').waitFor();
  await page.locator('nav#sessions').getByRole('button', { name: '+ Session', exact: true }).click();
  const reviewForm = page.locator('#session-form');
  await reviewForm.locator('#sf-advanced[open]').waitFor();
  const reviewAccess = reviewForm.getByLabel('Access', { exact: true });
  if (await reviewAccess.inputValue() !== 'reviewer' || !(await reviewAccess.isDisabled())) throw new Error('a reviewer can select writer access');
  for (const consent of ['I accept tools without per-tool approval', 'Allow every tool']) {
    if (await reviewForm.getByRole('checkbox', { name: consent, exact: true }).count()) throw new Error(`a reviewer is offered ${consent}`);
  }
  await reviewForm.getByLabel('Session name', { exact: true }).fill('Read-only review');
  await reviewForm.getByLabel('Harness', { exact: true }).selectOption('gemini-cli');
  await reviewForm.getByLabel('First prompt', { exact: true }).fill('Review the frozen CSV without modifying files.');
  await reviewForm.locator('fieldset').filter({ hasText: 'Outputs to review' }).getByRole('checkbox', { name: CSV, exact: true }).check();
  await reviewForm.getByRole('button', { name: 'Start session', exact: true }).click();
  await toast('Session started.');
  await composer('send');
  if (await page.locator('#composer-go').isDisabled()) throw new Error('a reviewer cannot continue its own read-only review');
  if (!(await page.getByRole('button', { name: 'Upload sources', exact: true }).isDisabled())) throw new Error('a reviewer can upload sources');
  await page.unroute(workspaceRoute);
  await page.reload();
  await titled('Backlog review').waitFor();

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
  await page.locator('#tab-outputs').click();
  await outputs.nth(3).waitFor();
  const continued = await callApi('POST', `/v1/workspaces/${ws}/sessions/${analystSid}/messages`, { prompt: capturePrompt, action: 'send' });
  if (continued.status >= 300) throw new Error(`continuing the analyst answered ${continued.status}: ${JSON.stringify(continued.body)}`);
  const second = await awaitOperation(continued.body.id);
  if (second.state !== 'succeeded') throw new Error(`the analyst's second turn ${second.state}`);
  const listed = (await callApi('GET', `/v1/workspaces/${ws}/artifacts`)).body.artifacts.length;
  if (listed !== 8) throw new Error(`the workspace lists ${listed} outputs after the second turn, not 8`);
  if (await byId(artifact[DOCX]).getAttribute('data-standing') !== 'current') throw new Error('the page saw the second turn by itself, so it is not a stale page');
  await choose(DOCX).check();
  const { revision } = (await callApi('GET', `/v1/workspaces/${ws}`)).body;
  // A page's reads see a newer turn together with the revision it moved, so
  // a stale page meets revision_conflict first. A current revision stands in
  // for a page that read the workspace after the turn was admitted and
  // before it ran; the refusal is the server's own.
  const finalize = `/v1/workspaces/${ws}/finalize`;
  await page.route(`**${finalize}`, (route) => route.continue({ postData: JSON.stringify({ ...route.request().postDataJSON(), revision }) }));
  await strip.getByRole('button', { name: 'Accept 1 selected', exact: true }).click();
  const [refusal] = await Promise.all([
    page.waitForResponse((r) => new URL(r.url()).pathname === finalize, TURN),
    page.locator('#confirm-ok').click(),
  ]);
  await page.unroute(`**${finalize}`);
  const refused = await refusal.json();
  if (refusal.status() !== 409 || refused.error.code !== 'deliverable_not_current') throw new Error(`accepting a stale output answered ${refusal.status()} ${JSON.stringify(refused)}`);
  await toast('Not accepted: an output you chose is no longer its session\'s current result. The outputs are reloaded; accept the ones marked Current.');
  await outputs.nth(7).waitFor(TURN);
  const earlier = byId(artifact[DOCX]);
  await page.waitForFunction((id) => document.querySelector(`#outputs-list li[data-artifact="${id}"]`).dataset.standing === 'earlier', artifact[DOCX]);
  if (!(await earlier.locator('.item-meta').innerText()).endsWith('Earlier turn 1 — not the current result')) throw new Error(`the earlier DOCX says ${await earlier.locator('.item-meta').innerText()}`);
  const earlierBox = earlier.getByRole('checkbox');
  if (!(await earlierBox.isDisabled()) || await earlierBox.isChecked()) throw new Error('an earlier turn\'s output can still be selected');
  if (await strip.locator('#accept').count()) throw new Error('Accept is still offered for an earlier turn\'s output');
  const standingOf = (id) => byId(id).getAttribute('data-standing');
  if (await standingOf(artifact[PPTX]) !== 'earlier') throw new Error('the first turn\'s PPTX is not marked earlier');
  for (const name of [CSV, XLSX]) if (await standingOf(artifact[name]) !== 'accepted') throw new Error(`accepted ${name} lost its mark`);
  const latest = {};
  for (const name of [CSV, XLSX, DOCX, PPTX]) {
    const fresh = outputRow(name).and(page.locator('[data-standing="current"]'));
    latest[name] = await fresh.getAttribute('data-artifact');
    if (!(await fresh.locator('.item-meta').innerText()).endsWith('turn 2')) throw new Error(`the second turn's ${name} does not say turn 2`);
  }
  await expectShown(DOCX, await previewed(() => earlier.locator('button.file-name').click(), previewPath(artifact[DOCX])));
  const direct = await callApi('POST', finalize, { revision, artifacts: [artifact[DOCX]] });
  if (direct.status !== 409 || direct.body.error.code !== 'deliverable_not_current') throw new Error(`the API accepted an earlier turn's output: ${direct.status} ${JSON.stringify(direct.body)}`);
  await byId(latest[DOCX]).getByRole('checkbox').focus();
  await page.keyboard.press('Space');
  await strip.getByRole('button', { name: 'Accept 1 selected', exact: true }).waitFor();
  await page.keyboard.press('Space');
  await strip.locator('#accept').waitFor({ state: 'detached' });
  const stillAccepted = (await callApi('GET', `/v1/workspaces/${ws}`)).body.accepted_deliverables.map((d) => d.artifact_id).sort();
  if (JSON.stringify(stillAccepted) !== JSON.stringify(want)) throw new Error(`the server holds ${JSON.stringify(stillAccepted)} as accepted, not ${JSON.stringify(want)}`);
  say('STALE');

  const openOutput = async (which = CSV) => {
    await page.locator('#tab-outputs').click();
    const name = byId(latest[which]).locator('button.file-name');
    if (await name.getAttribute('aria-pressed') !== 'true') await name.click();
    await settled.waitFor(TURN);
    await page.locator('#preview').scrollIntoViewIfNeeded();
  };
  const openSource = async () => {
    await page.locator('#tab-sources').click();
    if (await sourceName(CSV).getAttribute('aria-pressed') !== 'true') await sourceName(CSV).click();
    await settled.waitFor(TURN);
    await page.locator('#preview').scrollIntoViewIfNeeded();
  };
  const fits = async (what) => {
    await noOverflow(what);
    const width = page.viewportSize().width;
    for (const part of ['#preview', '#preview .preview-head button']) {
      const box = await page.locator(part).boundingBox();
      if (!box || box.x < 0 || box.x + box.width > width) throw new Error(`${part} of ${what} does not fit in ${width}px`);
    }
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
  await openOutput();
  await fits('an output preview');
  await openSource();
  await fits('a source preview');

  if (shots) {
    await page.waitForFunction(() => !document.querySelector('#toasts .toast'), null, TURN);
    const desktop = [
      ['project-glance', async () => {
        await go(`#/p/${pid}`);
        await page.locator(`#main a.card-link[href="#/w/${ws}"]`).waitFor();
      }],
      ['workspace-sessions', async () => {
        await reopen();
        await tabs.nth(1).waitFor();
      }],
      ['workspace-sources', openSource],
      ['workspace-outputs', () => openOutput(CSV)],
      ['workspace-document', () => openOutput(DOCX)],
    ];
    const phone = [
      ['phone-workspace', reopen],
      ['phone-sources', openSource],
      ['phone-outputs', () => openOutput(CSV)],
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
