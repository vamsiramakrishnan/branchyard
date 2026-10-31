// Drives the console's guided start in headless Chromium for
// tests/console_guided_browser.rs, which runs the server and reads this
// script's stdout a line at a time: LOADED, SOURCES, ENVIRONMENTS, PARALLEL,
// FOLLOWUP, ACCEPTED, REMOTE, ARCHIVED, ANTIGRAVITY, ROLES, KEYBOARD, PHONE
// and DONE, each once the page has shown it and, where the server decides,
// its API agrees.
//
// PLAYWRIGHT names the playwright module; BY_URL the server; BY_CODE a
// pairing code for the owner; BY_VIEWER_TOKEN and BY_REVIEWER_TOKEN tokens
// whose principals become the workspace's viewer and reviewer; BY_FIXTURES
// the directory tools/console_fixtures.py wrote; BY_COLLECT the script a
// session runs to copy its inputs to outputs; BY_CONFIGURED, as JSON, the
// configured values no page or session may show; BY_SCREENSHOTS, when set,
// a directory for screenshots.
'use strict';

const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const { chromium } = require(process.env.PLAYWRIGHT);

const say = (text) => process.stdout.write(`${text}\n`);
const shots = process.env.BY_SCREENSHOTS;
const base = process.env.BY_URL;
const fixtures = process.env.BY_FIXTURES;
const configured = JSON.parse(process.env.BY_CONFIGURED);
const scale = Number(process.env.BY_TEST_TIMEOUT_SCALE) || 1;
const TURN = { timeout: 60000 * scale };
// A start on a Substrate cluster that refuses connections ends only once the
// Substrate client stops retrying, which took a minute in the server's own
// remote_presets_never_fall_back tests on a loaded machine.
const REMOTE = { timeout: 240000 * scale };
const CSV = 'ticket_backlog.csv';
const XLSX = 'backlog_metrics.xlsx';
const DOCX = 'backlog_report.docx';
const PROMPT = `SH python3 ${process.env.BY_COLLECT}`;
const PRESETS = ['local', 'substrate-closed', 'substrate-missing'];
const ACK = 'I accept tools without per-tool approval';
let browser = null;
let shown = null;

const problems = [];
const starts = [];
// Every page's errors and every session start it sends, by who sent it.
function watch(page, who) {
  page.on('console', (m) => { if (m.type() === 'error') problems.push(`${who}: ${m.text()}`); });
  page.on('pageerror', (e) => problems.push(`${who}: ${e}`));
  page.on('request', (r) => {
    if (r.method() === 'POST' && /^\/v1\/workspaces\/[^/]+\/sessions$/.test(new URL(r.url()).pathname)) starts.push({ who, body: r.postDataJSON() });
  });
}

const same = (got, want, what) => {
  if (JSON.stringify(got) !== JSON.stringify(want)) throw new Error(`${what}: ${JSON.stringify(got)}, not ${JSON.stringify(want)}`);
};

(async () => {
  browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  context.setDefaultTimeout(30000 * scale);
  const page = await context.newPage();
  shown = () => page.locator('main').innerText();
  watch(page, 'owner');
  const hash = (p = page) => new URL(p.url()).hash;
  const go = (p, to) => p.evaluate((h) => { location.hash = h; }, to);
  const toast = (text, p = page) => p.locator('.toast', { hasText: text }).first().waitFor(TURN);
  const callApi = (p, method, pathname, body) => p.evaluate(async ([method, pathname, body]) => {
    const headers = { Authorization: `Bearer ${sessionStorage.getItem('branchyard.token')}` };
    if (body) Object.assign(headers, { 'Content-Type': 'application/json', 'Idempotency-Key': crypto.randomUUID() });
    const response = await fetch(pathname, { method, headers, body: body ? JSON.stringify(body) : undefined });
    return { status: response.status, body: await response.json().catch(() => null) };
  }, [method, pathname, body]);
  // page.waitForFunction would take an async predicate's promise as truthy
  // and return at once, so an operation is awaited here.
  const awaitOperation = async (p, id) => {
    for (const deadline = Date.now() + TURN.timeout; ;) {
      const op = (await callApi(p, 'GET', `/v1/operations/${id}?wait=5`)).body;
      if (!['queued', 'running'].includes(op.state)) return op;
      if (Date.now() > deadline) throw new Error(`operation ${id} is still ${op.state}`);
    }
  };
  const noOverflow = async (p, what) => {
    if (await p.evaluate(() => document.documentElement.scrollWidth > innerWidth)) throw new Error(`${what} overflows at ${p.viewportSize().width}px`);
  };

  await page.goto(`${base}/app/#pair=${process.env.BY_CODE}`);
  await page.getByRole('heading', { level: 1, name: 'app', exact: true }).waitFor();
  const token = await page.evaluate(() => sessionStorage.getItem('branchyard.token'));
  // A write another client makes while one of the page's requests is held.
  const write = async (method, pathname, body) => {
    const response = await page.request.fetch(`${base}${pathname}`, {
      method,
      headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json', 'Idempotency-Key': crypto.randomUUID() },
      data: body ? JSON.stringify(body) : undefined,
    });
    return { status: response.status(), body: await response.json() };
  };
  const picker = page.locator('#sidebar').getByRole('combobox', { name: 'Source', exact: true });
  await picker.locator('option[value="new-project"]').waitFor({ state: 'attached' });
  say('LOADED');

  await picker.selectOption('new-project');
  await page.getByRole('heading', { level: 1, name: 'New project', exact: true }).waitFor();
  const projectForm = page.locator('#main');
  await projectForm.getByLabel('Project name', { exact: true }).fill('Backlog research');
  await projectForm.getByLabel('Source', { exact: true }).selectOption({ label: 'Knowledge work (no repository)' });
  await projectForm.getByLabel('Results', { exact: true }).selectOption('artifacts');
  await projectForm.getByRole('button', { name: 'Create project', exact: true }).click();
  await page.waitForURL((url) => /^#\/p\/[^/]+$/.test(url.hash) && url.hash !== '#/p/new', TURN);
  const pid = hash().slice('#/p/'.length);
  await page.locator('#new-task').filter({ hasText: 'New workspace' }).click();
  await page.waitForURL((url) => url.hash === `#/p/${pid}/new`);
  await page.waitForFunction(() => [...(document.activeElement.labels || [])].some((label) => label.textContent.trim() === 'Workspace title'));
  await page.getByLabel('Workspace title', { exact: true }).fill('Guided review');
  await page.getByRole('button', { name: 'Create workspace', exact: true }).click();
  await page.waitForURL((url) => /^#\/w\/[^/]+$/.test(url.hash), TURN);
  const ws = hash().slice('#/w/'.length);
  const workspacePath = `/v1/workspaces/${ws}`;
  const sessionsPath = `${workspacePath}/sessions`;
  const strip = page.locator('header#strip');
  const guide = page.locator('#guide');
  const plus = page.locator('nav#sessions').getByRole('button', { name: '+ Session', exact: true });
  await strip.getByRole('heading', { level: 1, name: 'Guided review', exact: true }).waitFor(TURN);
  await strip.locator('p.state', { hasText: 'Ready' }).waitFor(TURN);
  await guide.getByText('Upload the files its sessions should read, such as a spreadsheet, a document or a CSV.', { exact: true }).waitFor();
  await guide.getByRole('button', { name: 'Upload sources', exact: true }).waitFor();
  if (!(await page.locator('#session-form').count())) await plus.click();
  const form = page.locator('#session-form');
  const status = form.locator('#sf-status');
  const alert = form.getByRole('alert');
  const statusSays = (text, p = page) => p.locator('#sf-status', { hasText: text }).waitFor();
  await form.getByText('No sources yet. Upload the files it should read, such as a spreadsheet, a document or a CSV.', { exact: true }).waitFor();
  const name = form.getByLabel('Session name', { exact: true });
  const prompt = form.getByLabel('First prompt', { exact: true });
  await name.fill('Analyst A');

  // Another write lands first, then the page's own request goes on
  // unchanged; the server's answer is kept to check.
  const interleave = (role) => {
    const seen = { write: null, answer: null };
    return [seen, async (route) => {
      if (route.request().method() !== 'POST' || seen.write) { await route.continue(); return; }
      const { revision } = (await write('GET', workspacePath)).body;
      seen.write = await write('POST', `${workspacePath}/members/${role}`, { revision, role });
      const response = await route.fetch();
      seen.answer = { status: response.status(), body: await response.json() };
      await route.fulfill({ response });
    }];
  };
  const [interleaved, beforeUpload] = interleave('viewer');
  await page.route(`**${workspacePath}/sources`, beforeUpload);
  const upload = async (files) => {
    const [chooser] = await Promise.all([page.waitForEvent('filechooser'), form.getByRole('button', { name: 'Upload files', exact: true }).click()]);
    await chooser.setFiles(files.map((f) => path.join(fixtures, f)));
  };
  await upload([CSV, XLSX, DOCX]);
  await toast(`Could not upload ${CSV}: the workspace changed meanwhile. It is reloaded at revision`);
  const stale = interleaved.answer;
  if (stale.status !== 409 || stale.body.error.code !== 'revision_conflict') throw new Error(`an upload at a stale revision answered ${JSON.stringify(stale)}`);
  if (interleaved.write.status !== 200) throw new Error(`adding the viewer answered ${JSON.stringify(interleaved.write)}`);
  await toast(`Not sent: ${CSV}, ${XLSX}, ${DOCX}. Upload them again.`);
  if (await name.inputValue() !== 'Analyst A') throw new Error('a refused upload changed the draft');
  await page.unroute(`**${workspacePath}/sources`);
  await upload([CSV, XLSX, DOCX]);
  await toast('Uploaded 3 sources.');
  const sourceBox = (title) => form.getByRole('checkbox', { name: title, exact: true });
  for (const title of [CSV, XLSX, DOCX]) {
    await sourceBox(title).waitFor();
    if (!(await sourceBox(title).isChecked())) throw new Error(`a new source, ${title}, does not start ticked`);
  }
  const detail = (await callApi(page, 'GET', workspacePath)).body;
  const sourceId = Object.fromEntries(detail.sources.map((s) => [s.title, s.id]));
  if (!detail.members.some((m) => m.subject === 'viewer' && m.role === 'viewer')) throw new Error(`the viewer is not a member: ${JSON.stringify(detail.members)}`);
  say('SOURCES');

  const radios = form.getByRole('radio');
  await radios.nth(2).waitFor();
  same(await radios.evaluateAll((rs) => rs.map((r) => r.value)), PRESETS, 'the environments offered');
  const discovered = (await callApi(page, 'GET', `/v1/execution-presets?workspace=${ws}`)).body;
  if (!discovered.required || discovered.presets.some((p) => p.origin !== 'operator')) throw new Error(`discovery answered ${JSON.stringify(discovered)}`);
  same(discovered.presets.map((p) => p.id), PRESETS, 'the presets discovered');
  same(await radios.evaluateAll((rs) => rs.filter((r) => r.checked).length), 0, 'environments chosen for the person');
  await statusSays('Choose where it runs.');
  await form.getByRole('button', { name: 'Start session', exact: true }).click();
  if (await page.evaluate(() => document.activeElement.name) !== 'environment') throw new Error('Start did not focus the environment to choose');
  const card = (key) => form.locator(`.env-card[data-environment="${key}"]`);
  const row = (key, label) => card(key).locator('.readiness li', { hasText: `${label}:` });
  const rowSays = async (key, label, parts, state) => {
    const text = await row(key, label).innerText();
    for (const part of parts) if (!text.includes(part)) throw new Error(`${key}'s ${label} row says ${JSON.stringify(text)}, without ${JSON.stringify(part)}`);
    if (state && !(await row(key, label).locator(`.dot-${state}`).count())) throw new Error(`${key}'s ${label} row has no ${state} dot`);
  };
  await rowSays('substrate-missing', 'Environment', ['Not ready, checked on this server, checked ', 'This server cannot read the Substrate settings for this environment.',
    'Operator: An operator should check this environment in the server configuration and run `by serve --check`.'], 'needs');
  await rowSays('substrate-closed', 'Environment', ['Ready, checked on this server', 'This server can read the Substrate settings and credential files.'], 'ready');
  await rowSays('substrate-closed', 'Reachability', ['Unknown, in the environment itself. The cluster is contacted when a session starts, not before.'], 'done');
  const linked = await page.evaluate(() => {
    const radio = document.getElementById('sf-env-substrate-missing');
    return radio.getAttribute('aria-describedby').split(' ').map((id) => (document.getElementById(id) || { textContent: null }).textContent);
  });
  if (linked.some((t) => t == null) || !linked.some((t) => t.includes('Not ready, checked on this server'))) throw new Error(`the radio describes itself as ${JSON.stringify(linked)}`);
  await form.getByRole('radio', { name: 'Local workers', exact: true }).check();
  const harness = form.getByLabel('Harness', { exact: true });
  const model = form.getByLabel('Model', { exact: true });
  same(await harness.locator('option').evaluateAll((os) => os.map((o) => [o.value, o.disabled])), [['claude-code-acp', false], ['antigravity', false]], 'the local harnesses');
  same(await model.locator('option').evaluateAll((os) => os.map((o) => o.value)), ['fake-fast', 'fake-deep'], 'the models offered');
  if (await model.inputValue() !== 'fake-fast') throw new Error('the preset\'s default model is not chosen');
  const harnessRows = await form.locator('#sf-harness-ready').innerText();
  for (const part of ['Executable: Ready, checked on this server', 'Sign-in: Unknown, in the environment itself.',
    'Operator: An operator can add the harness\'s key to this preset\'s secrets.', 'Key: This environment supplies no key.']) {
    if (!harnessRows.includes(part)) throw new Error(`the harness readiness says ${JSON.stringify(harnessRows)}, without ${JSON.stringify(part)}`);
  }
  say('ENVIRONMENTS');

  const start = async (p = page) => {
    const [request] = await Promise.all([
      p.waitForRequest((r) => r.method() === 'POST' && new URL(r.url()).pathname === sessionsPath, TURN),
      p.locator('#session-form').getByRole('button', { name: 'Start session', exact: true }).click(),
    ]);
    const response = await request.response();
    return { body: request.postDataJSON(), status: response.status(), json: await response.json() };
  };
  const opened = async (result, p = page) => {
    if (result.status !== 200) throw new Error(`a start answered ${result.status} ${JSON.stringify(result.json)}`);
    await toast('Session started.', p);
    await p.waitForURL((url) => url.hash === `#/w/${ws}/s/${result.json.session_id}`, TURN);
    return result.json.session_id;
  };
  const tab = (sid, p = page) => p.locator(`nav#sessions a.session-tab[data-session="${sid}"]`);
  const openTab = async (sid) => {
    await tab(sid).click();
    await tab(sid).and(page.locator('[aria-current="page"]')).waitFor();
  };
  const phase = async (sid, pattern, p = page, wait = TURN) => {
    try {
      await p.waitForFunction(([sid, source]) => new RegExp(source)
        .test((document.querySelector(`nav#sessions a.session-tab[data-session="${sid}"]`) || {}).title || ''), [sid, pattern.source], wait);
    } catch (e) {
      throw new Error(`the session's tab says ${JSON.stringify(await tab(sid, p).getAttribute('title').catch(() => null))}, not ${pattern}: ${e.message}`);
    }
  };
  const sessionJson = async (sid) => (await callApi(page, 'GET', workspacePath)).body.sessions.find((s) => s.id === sid);

  await model.selectOption('fake-deep');
  await sourceBox(DOCX).uncheck();
  await prompt.fill(PROMPT);
  if (!(await form.getByRole('checkbox', { name: 'Collect outputs (outputs/artifacts.json)', exact: true }).isChecked())) throw new Error('an outputs project does not collect outputs by default');
  await statusSays('Ready to start: Local workers · claude-code-acp · reads 2 sources');
  const first = await start();
  same([first.body.preset, first.body.harness, first.body.model, first.body.access, first.body.sources, !!first.body.result_contract, 'unapproved_tools' in first.body],
    ['local', 'claude-code-acp', 'fake-deep', 'writer', [sourceId[CSV], sourceId[XLSX]], true, false], 'the first start');
  const analystA = await opened(first);

  await plus.click();
  await form.waitFor();
  if (!(await form.getByRole('radio', { name: 'Local workers', exact: true }).isChecked())) throw new Error('the next draft lost its environment');
  same([await harness.inputValue(), await model.inputValue(), await name.inputValue(), await prompt.inputValue()], ['claude-code-acp', 'fake-deep', '', ''], 'the next draft');
  for (const title of [CSV, XLSX, DOCX]) if (!(await sourceBox(title).isChecked())) throw new Error(`the next draft does not read ${title}`);
  await name.fill('Analyst B');
  await sourceBox(CSV).uncheck();
  await sourceBox(XLSX).uncheck();
  await model.selectOption('fake-fast');
  await prompt.fill(PROMPT);
  await openTab(analystA);
  await page.waitForFunction(() => document.querySelector('#composer').dataset.mode !== 'off', null, TURN);
  await page.locator('#composer-text').fill('draft for A');
  await plus.click();
  await form.waitFor();
  same([await name.inputValue(), await prompt.inputValue(), await model.inputValue()], ['Analyst B', PROMPT, 'fake-fast'], 'the new-session draft after switching sessions');
  same([await sourceBox(CSV).isChecked(), await sourceBox(XLSX).isChecked(), await sourceBox(DOCX).isChecked()], [false, false, true], 'the new-session draft\'s sources');
  await openTab(analystA);
  if (await page.locator('#composer-text').inputValue() !== 'draft for A') throw new Error('the composer draft did not survive opening the form');
  await plus.click();
  await form.waitFor();
  const second = await start();
  same([second.body.preset, second.body.model, second.body.sources], ['local', 'fake-fast', [sourceId[DOCX]]], 'the second start');
  const analystB = await opened(second);
  await phase(analystA, /: Completed$/);
  await phase(analystB, /: Completed$/);
  const copiedA = `copied ${XLSX}, ${CSV}`;
  const copiedB = `copied ${DOCX}`;
  for (const [sid, copied, other] of [[analystA, copiedA, copiedB], [analystB, copiedB, copiedA]]) {
    await openTab(sid);
    await page.locator('#convo .turn .agent', { hasText: 'sh: 0' }).filter({ hasText: copied }).waitFor(TURN);
    if (await page.locator('#convo .turn .agent', { hasText: other }).count()) throw new Error(`a session read other inputs than its own: ${await page.locator('#convo').innerText()}`);
  }
  await openTab(analystA);
  const facts = await page.locator('#session-facts').innerText();
  for (const part of ['Local workers (Local)', 'claude-code-acp', 'fake-deep', 'writer', 'reads 2 sources', 'collects outputs', 'Completed']) {
    if (!facts.includes(part)) throw new Error(`the facts line says ${JSON.stringify(facts)}, without ${part}`);
  }
  if (await page.locator('#composer-text').inputValue() !== 'draft for A') throw new Error('the composer draft did not survive switching sessions');
  const sessions = (await callApi(page, 'GET', workspacePath)).body.sessions;
  const byId = Object.fromEntries(sessions.map((s) => [s.id, s]));
  same([byId[analystA].preset, byId[analystA].sources, byId[analystB].preset, byId[analystB].sources],
    ['local', [sourceId[CSV], sourceId[XLSX]], 'local', [sourceId[DOCX]]], 'the sessions\' presets and sources');
  const outputsOf = async (sid) => {
    const { artifacts } = (await callApi(page, 'GET', `${workspacePath}/artifacts`)).body;
    const branch = (await sessionJson(sid)).branch;
    return artifacts.filter((a) => a.publisher_branch === branch);
  };
  same((await outputsOf(analystA)).map((a) => a.name).sort(), [XLSX, CSV].sort(), 'Analyst A\'s outputs');
  same((await outputsOf(analystB)).map((a) => a.name).sort(), [DOCX], 'Analyst B\'s outputs');
  await page.locator('#tab-outputs').click();
  const group = (sessionName) => page.locator('#outputs-list section.output-group').filter({ has: page.getByRole('heading', { level: 3, name: new RegExp(`^${sessionName}`) }) });
  await group('Analyst A').locator('li[data-artifact]').nth(1).waitFor(TURN);
  same((await group('Analyst A').locator('button.file-name').allInnerTexts()).sort(), [XLSX, CSV].sort(), 'the page\'s outputs under Analyst A');
  same(await group('Analyst B').locator('button.file-name').allInnerTexts(), [DOCX], 'the page\'s outputs under Analyst B');
  await guide.getByText('3 outputs waiting for review.', { exact: true }).waitFor();
  say('PARALLEL');

  await page.locator('#composer-text').fill(PROMPT);
  await page.locator('#composer-go').click();
  await toast('Sent. A new turn is starting.');
  await page.locator('#convo .turn .you').nth(1).waitFor(TURN);
  await page.waitForFunction(() => document.querySelectorAll('#outputs-list li[data-standing="current"]').length === 3
    && [...document.querySelectorAll('#outputs-list li[data-standing="current"] .item-meta')].filter((m) => m.textContent.endsWith('turn 2')).length === 2, null, TURN);
  same((await outputsOf(analystA)).map((a) => [a.name, a.turn]).sort(), [[CSV, 1], [CSV, 2], [XLSX, 1], [XLSX, 2]].sort(), 'Analyst A\'s outputs after its second turn');
  const typed = await page.locator('#convo .turn .you').allInnerTexts();
  same(typed, [PROMPT, PROMPT], 'what the conversation shows the person sent');
  say('FOLLOWUP');

  const csvNow = group('Analyst A').locator('li[data-standing="current"]').filter({ has: page.locator('button.file-name', { hasText: CSV }) });
  const csvId = await csvNow.getAttribute('data-artifact');
  await csvNow.getByRole('checkbox').check();
  await guide.getByText('1 output ticked: ready to accept.', { exact: true }).waitFor();
  const finalize = `${workspacePath}/finalize`;
  const [reviewerAdded, beforeAccept] = interleave('reviewer');
  await page.route(`**${finalize}`, beforeAccept);
  const accept = async () => {
    await strip.getByRole('button', { name: 'Accept 1 selected', exact: true }).click();
    await page.locator('#confirm-ok').click();
  };
  await accept();
  await toast('Could not accept: the workspace changed meanwhile. It is reloaded at revision');
  const conflict = reviewerAdded.answer;
  if (conflict.status !== 409 || conflict.body.error.code !== 'revision_conflict') throw new Error(`a stale acceptance answered ${JSON.stringify(conflict)}`);
  if (reviewerAdded.write.status !== 200) throw new Error(`adding the reviewer answered ${JSON.stringify(reviewerAdded.write)}`);
  await accept();
  await toast('Accepted 1 output.');
  await page.unroute(`**${finalize}`);
  same((await callApi(page, 'GET', workspacePath)).body.accepted_deliverables.map((d) => d.artifact_id), [csvId], 'the accepted outputs');

  if (shots) {
    await page.waitForFunction(() => !document.querySelector('#toasts .toast'), null, TURN);
    const capture = async (shot, ready) => {
      for (const [width, height] of [[1440, 900], [390, 844]]) {
        await page.setViewportSize({ width, height });
        for (const scheme of ['light', 'dark']) {
          await page.emulateMedia({ colorScheme: scheme });
          await ready(width);
          await page.screenshot({ path: path.join(shots, `${shot}-${width}-${scheme}.png`), fullPage: width < 760 });
          if (width >= 760 && await page.locator('#sf-start').count()) {
            await page.locator('#sf-start').scrollIntoViewIfNeeded();
            await page.evaluate(() => new Promise((painted) => requestAnimationFrame(() => requestAnimationFrame(painted))));
            await page.screenshot({ path: path.join(shots, `${shot}-end-${width}-${scheme}.png`) });
          }
        }
      }
      await page.setViewportSize({ width: 1440, height: 900 });
      await page.emulateMedia({ colorScheme: 'light' });
    };
    await capture('workspace-two-sessions', async () => {
      await openTab(analystA);
      await page.locator('#tab-outputs').click();
      await page.locator('#convo').evaluate((box) => { box.scrollTop = 0; });
    });
    await capture('guided-form', async () => {
      if (!(await form.count())) await plus.click();
      await form.locator('#sf-environments').waitFor();
      await page.locator('#convo').evaluate((box) => { box.scrollTop = 0; });
      await page.evaluate(() => window.scrollTo(0, 0));
    });
    await form.getByRole('button', { name: 'Cancel', exact: true }).click();
  }
  say('ACCEPTED');

  await plus.click();
  await form.getByRole('radio', { name: 'Substrate, cluster closed', exact: true }).check();
  same(await model.inputValue(), 'fake-fast', 'the Substrate preset\'s default model');
  await name.fill('Remote check');
  await prompt.fill(PROMPT);
  const remote = await start();
  same([remote.body.preset, remote.body.harness, remote.body.model], ['substrate-closed', 'claude-code-acp', 'fake-fast'], 'the Substrate start');
  const remoteSid = await opened(remote);
  await phase(remoteSid, /: (Failed|Did not start)/, page, REMOTE);
  const recovery = page.locator('#convo section.recovery');
  await recovery.getByRole('button', { name: 'Try again', exact: true }).waitFor();
  const recoveryText = await recovery.innerText();
  if (!recoveryText.includes('Try again, choosing another environment or harness if this one cannot run it.')) throw new Error(`the failed session says ${JSON.stringify(recoveryText)}`);
  const remoteJson = await sessionJson(remoteSid);
  if (remoteJson.preset !== 'substrate-closed') throw new Error(`the Substrate session's preset is ${remoteJson.preset}`);
  const remoteError = remoteJson.error ? remoteJson.error.message : (remoteJson.status_detail || {}).reason;
  if (!remoteError || !recoveryText.includes(remoteError)) throw new Error(`the page does not show the server's error ${JSON.stringify(remoteError)}: ${recoveryText}`);
  const remoteMessages = (await callApi(page, 'GET', `${sessionsPath}/${remoteSid}/messages`)).body;
  if ((remoteMessages.messages || []).some((m) => m.role === 'assistant')) throw new Error('a harness answered on an unreachable cluster');
  if ((await outputsOf(remoteSid)).length) throw new Error('the unreachable session published outputs');
  const leaked = async (what, text) => {
    const found = configured.filter((value) => text.includes(value));
    if (found.length) throw new Error(`${what} shows ${found.join(', ')}`);
  };
  await leaked('the Substrate session\'s JSON', JSON.stringify(remoteJson) + JSON.stringify(remoteMessages));
  await leaked('the page', await page.locator('body').innerText());
  await guide.getByText(/^Remote check: (failed|did not start)\. Try again/).waitFor();
  await recovery.getByRole('button', { name: 'Try again', exact: true }).click();
  await form.waitFor();
  same([await name.inputValue(), await prompt.inputValue(), await harness.inputValue(), await model.inputValue()],
    ['Remote check (again)', PROMPT, 'claude-code-acp', 'fake-fast'], 'the draft Try again restores');
  if (!(await form.getByRole('radio', { name: 'Substrate, cluster closed', exact: true }).isChecked())) throw new Error('Try again lost the environment');
  for (const title of [CSV, XLSX, DOCX]) if (!(await sourceBox(title).isChecked())) throw new Error(`Try again lost the source ${title}`);
  await form.getByRole('radio', { name: 'Substrate, key missing', exact: true }).check();
  const notReadyStart = await start();
  same(notReadyStart.body.preset, 'substrate-missing', 'the start on the environment that is not ready');
  const missingSid = await opened(notReadyStart);
  await phase(missingSid, /: (Failed|Did not start)/, page, REMOTE);
  await leaked('the not-ready session\'s JSON', JSON.stringify(await sessionJson(missingSid)));
  await leaked('the page', await page.locator('body').innerText());
  say('REMOTE');

  await plus.click();
  await form.getByRole('radio', { name: 'Local workers', exact: true }).check();
  await name.fill('After restore');
  await prompt.fill('Say hello after the restore.');
  await form.getByRole('checkbox', { name: 'Collect outputs (outputs/artifacts.json)', exact: true }).uncheck();
  const { revision } = (await callApi(page, 'GET', workspacePath)).body;
  // The page's own reads of the workspace wait until the start has been
  // answered, so the refusal comes from the server and not from a poll that
  // saw the archive first.
  let release = null;
  const held = new Promise((resolve) => { release = resolve; });
  await page.route(`**${workspacePath}`, async (route) => {
    if (route.request().method() === 'GET') await held;
    await route.continue();
  });
  const archiving = await callApi(page, 'POST', `${workspacePath}/actions/archive`, { revision });
  if (archiving.status !== 200) throw new Error(`archiving answered ${archiving.status} ${JSON.stringify(archiving.body)}`);
  const archived = await awaitOperation(page, archiving.body.id);
  if (archived.state !== 'succeeded') throw new Error(`archiving ${archived.state}`);
  const refused = await start();
  release();
  await page.unrouteAll({ behavior: 'wait' });
  if (refused.status !== 409 || refused.json.error.code !== 'workspace_not_ready') throw new Error(`a start in an archived workspace answered ${refused.status} ${JSON.stringify(refused.json)}`);
  await alert.filter({ hasText: 'The workspace is archived: restore it from ⋯, then start. Your draft is kept.' }).waitFor();
  same([await name.inputValue(), await prompt.inputValue()], ['After restore', 'Say hello after the restore.'], 'the draft after a refused start');
  await guide.getByText('The workspace is archived: restore it from ⋯, then start.', { exact: true }).waitFor();
  await strip.locator('summary[aria-label="More actions"]').click();
  await strip.getByRole('button', { name: 'Restore', exact: true }).click();
  await toast('Restored.');
  await strip.locator('p.state', { hasText: 'Ready' }).waitFor(TURN);
  await statusSays('Ready to start: Local workers · claude-code-acp · reads 3 sources');
  const restarted = await start();
  same([restarted.body.name, restarted.body.prompt, restarted.body.preset], ['After restore', 'Say hello after the restore.', 'local'], 'the start after Restore');
  const restoredSid = await opened(restarted);
  await phase(restoredSid, /: Completed/);
  say('ARCHIVED');

  await plus.click();
  await harness.selectOption('antigravity');
  await form.getByText('This environment chooses the model.', { exact: true }).waitFor();
  const ack = form.getByRole('checkbox', { name: ACK, exact: true });
  if (await ack.isChecked()) throw new Error('the tool acknowledgement starts ticked');
  if (!(await form.locator('#sf-agent').getByRole('checkbox', { name: ACK, exact: true }).count())) throw new Error('the acknowledgement is not in the Agent step');
  await name.fill('Evidence check');
  await prompt.fill('Check the evidence in the sources.');
  await form.getByRole('checkbox', { name: 'Collect outputs (outputs/artifacts.json)', exact: true }).uncheck();
  const sentBefore = starts.length;
  await form.getByRole('button', { name: 'Start session', exact: true }).click();
  await page.waitForFunction(() => document.activeElement && document.activeElement.id === 'sf-ack');
  await statusSays(`Tick “${ACK}” to start antigravity as a writer, or choose another harness.`);
  if (starts.length !== sentBefore) throw new Error('Start sent a request without the acknowledgement');
  await ack.check();
  const agy = await start();
  same([agy.body.harness, agy.body.preset, agy.body.unapproved_tools, 'model' in agy.body, agy.body.access], ['antigravity', 'local', true, false, 'writer'], 'the Antigravity start');
  const agySid = await opened(agy);
  await phase(agySid, /: Completed/);
  const agyJson = await sessionJson(agySid);
  same([agyJson.preset, agyJson.harness, agyJson.unapproved_tools, agyJson.status_detail.state], ['local', 'antigravity', true, 'no_changes'], 'the Antigravity session');
  say('ANTIGRAVITY');

  const signIn = async (secret, who) => {
    const other = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    other.setDefaultTimeout(30000 * scale);
    const p = await other.newPage();
    watch(p, who);
    await p.goto(`${base}/app/`);
    await p.getByRole('heading', { name: 'Connect to Branchyard' }).waitFor();
    await p.getByLabel('Server token', { exact: true }).fill(secret);
    await p.getByRole('button', { name: 'Connect to server', exact: true }).click();
    await p.locator('#sidebar').waitFor();
    await go(p, `#/w/${ws}`);
    await p.locator('header#strip').getByRole('heading', { level: 1, name: 'Guided review', exact: true }).waitFor(TURN);
    return p;
  };
  const viewer = await signIn(process.env.BY_VIEWER_TOKEN, 'viewer');
  await viewer.locator('#guide').getByText('You can read and download here; an owner can make you an editor.', { exact: true }).waitFor();
  if (await viewer.locator('#guide button').count()) throw new Error('the viewer\'s guide offers an action');
  if (await viewer.getByRole('button', { name: '+ Session', exact: true }).count()) throw new Error('a viewer is offered a new session');
  if (!(await viewer.locator('#inspector').getByRole('button', { name: 'Upload sources', exact: true }).isDisabled())) throw new Error('a viewer can upload sources');
  if (await viewer.locator('#source-drop').isVisible()) throw new Error('a viewer is offered a drop zone');
  const reviewer = await signIn(process.env.BY_REVIEWER_TOKEN, 'reviewer');
  await reviewer.locator('nav#sessions').getByRole('button', { name: '+ Session', exact: true }).click();
  const review = reviewer.locator('#session-form');
  await review.locator('#sf-advanced[open]').waitFor();
  const access = review.getByLabel('Access', { exact: true });
  if (await access.inputValue() !== 'reviewer' || !(await access.isDisabled())) throw new Error('a reviewer\'s access is not fixed to reviewer');
  await review.getByRole('radio', { name: 'Local workers', exact: true }).check();
  const reviewHarness = review.getByLabel('Harness', { exact: true });
  same(await reviewHarness.locator('option').evaluateAll((os) => os.map((o) => [o.value, o.disabled])), [['claude-code-acp', false], ['antigravity', true]], 'the reviewer\'s harnesses');
  await review.getByText('antigravity is not available: reviewers need a harness that routes each tool request for approval.', { exact: true }).waitFor();
  if (await review.getByRole('checkbox', { name: ACK, exact: true }).count()) throw new Error('a reviewer is asked for the tool acknowledgement');
  if (await review.getByRole('checkbox', { name: 'Collect outputs (outputs/artifacts.json)', exact: true }).count()) throw new Error('a reviewer is offered Collect outputs');
  await review.getByLabel('Session name', { exact: true }).fill('Review');
  await review.getByLabel('First prompt', { exact: true }).fill('Review the CSV and say what stands out.');
  await reviewer.locator('#sf-status', { hasText: 'Choose at least one output to review (under Advanced).' }).waitFor();
  await review.locator('#sf-outputs').getByRole('checkbox', { name: CSV, exact: true }).check();
  const reviewStart = await start(reviewer);
  same([reviewStart.body.access, reviewStart.body.preset, reviewStart.body.harness, reviewStart.body.artifacts, 'result_contract' in reviewStart.body, 'unapproved_tools' in reviewStart.body],
    ['reviewer', 'local', 'claude-code-acp', [csvId], false, false], 'the review start');
  const reviewSid = await opened(reviewStart, reviewer);
  await phase(reviewSid, /: Completed/, reviewer);
  await reviewer.locator('#composer[data-mode="send"]').waitFor(TURN);
  await reviewer.locator('#convo .turn .agent', { hasText: 'Review the CSV' }).waitFor(TURN);
  await reviewer.locator('#composer-text').fill('Anything about owners?');
  await reviewer.locator('#composer-go').click();
  await toast('Sent. A new turn is starting.', reviewer);
  await reviewer.locator('#convo .turn .agent', { hasText: 'Anything about owners?' }).waitFor(TURN);
  if (!(await reviewer.locator('#inspector').getByRole('button', { name: 'Upload sources', exact: true }).isDisabled())) throw new Error('a reviewer can upload sources');
  say('ROLES');

  await page.reload();
  await strip.getByRole('heading', { level: 1, name: 'Guided review', exact: true }).waitFor();
  const focused = () => page.evaluate(() => {
    const el = document.activeElement;
    return { id: el.id, tag: el.tagName, visible: el.matches(':focus-visible'), checked: !!el.checked, value: el.value };
  });
  const press = async (key) => {
    await page.keyboard.press(key);
    const now = await focused();
    if (now.tag === 'BODY' || !now.id) throw new Error(`${key} left the focus on ${JSON.stringify(now)}`);
    if (!now.visible) throw new Error(`${key} moved the focus to ${now.id} without showing it`);
    return now;
  };
  const tabTo = async (id, key = 'Tab') => {
    for (let i = 0; i < 40; i++) if ((await press(key)).id === id) return;
    throw new Error(`${key} never reached ${id}`);
  };
  await page.locator('#new-session').focus();
  await page.keyboard.press('Enter');
  await form.waitFor();
  await page.waitForFunction(() => document.activeElement.id === 'sf-env-local');
  await press('Space');
  if (!(await focused()).checked) throw new Error('Space did not choose Local workers');
  await harness.waitFor();
  const moved = await press('ArrowDown');
  if (moved.id !== 'sf-env-substrate-closed' || !moved.checked) throw new Error(`an arrow in the environments left ${JSON.stringify(moved)}`);
  const back = await press('ArrowUp');
  if (back.id !== 'sf-env-local' || !back.checked) throw new Error(`an arrow did not return to Local workers: ${JSON.stringify(back)}`);
  await tabTo('sf-harness');
  const changed = await press('ArrowDown');
  await form.locator('#sf-ack').waitFor();
  if (changed.value !== 'antigravity' || (await focused()).id !== 'sf-harness') throw new Error(`the redraw after choosing a harness left ${JSON.stringify(await focused())}`);
  await press('ArrowUp');
  await form.locator('#sf-ack').waitFor({ state: 'detached' });
  if ((await focused()).id !== 'sf-harness') throw new Error('the redraw after choosing a harness again lost the focus');
  await tabTo('sf-name');
  await page.keyboard.type('Keyboard run');
  await tabTo('sf-prompt');
  await page.keyboard.type('Summarise the backlog by owner.');
  await tabTo('sf-collect');
  await press('Space');
  await tabTo(`sf-source-${sourceId[DOCX]}`, 'Shift+Tab');
  await press('Space');
  if ((await focused()).checked) throw new Error('Space did not untick a source');
  await tabTo('sf-start');
  const [keyboardRequest] = await Promise.all([
    page.waitForRequest((r) => r.method() === 'POST' && new URL(r.url()).pathname === sessionsPath, TURN),
    page.keyboard.press('Enter'),
  ]);
  const keyboardBody = keyboardRequest.postDataJSON();
  same([keyboardBody.name, keyboardBody.prompt, keyboardBody.preset, keyboardBody.sources, 'result_contract' in keyboardBody],
    ['Keyboard run', 'Summarise the backlog by owner.', 'local', [sourceId[CSV], sourceId[XLSX]], false], 'the keyboard start');
  await toast('Session started.');
  say('KEYBOARD');

  await page.setViewportSize({ width: 390, height: 844 });
  await go(page, '#/inbox');
  await page.getByRole('heading', { level: 1, name: 'Needs you' }).waitFor();
  await go(page, `#/w/${ws}/s/${analystA}`);
  await strip.getByRole('heading', { level: 1, name: 'Guided review', exact: true }).waitFor();
  await tab(analystA).waitFor();
  await noOverflow(page, 'a workspace with sessions');
  await plus.click();
  await form.locator('.env-card').nth(2).waitFor();
  await noOverflow(page, 'the guided form');
  for (const part of await form.locator('.env-card, #sf-start, #sf-status').all()) {
    const box = await part.boundingBox();
    if (!box || box.x < 0 || box.x + box.width > 390) throw new Error(`part of the guided form does not fit in 390px: ${JSON.stringify(box)}`);
  }
  await form.getByRole('button', { name: 'Cancel', exact: true }).click();
  await noOverflow(page, 'the workspace after the form');
  say('PHONE');

  const offered = starts.filter((s) => !PRESETS.includes(s.body.preset));
  if (offered.length) throw new Error(`starts without an offered preset: ${JSON.stringify(offered)}`);
  if (!starts.some((s) => s.who === 'reviewer')) throw new Error('the reviewer\'s start was not seen');
  const csp = problems.filter((p) => /Content.Security.Policy|Refused to/i.test(p));
  if (csp.length) throw new Error(`policy violations: ${csp.join(' | ')}`);
  const errors = problems.filter((p) => !/Failed to load resource/.test(p));
  if (errors.length) throw new Error(`page errors: ${errors.join(' | ')}`);
  say('DONE');
  await browser.close();
  process.exit(0);
})().catch(async (e) => {
  // A pipe takes stderr asynchronously; exiting before it drains cuts the page's text.
  const report = `${e.stack || e}\n${shown ? `the page showed:\n${await shown().catch(() => '?')}\n` : ''}`;
  await new Promise((drained) => process.stderr.write(report, drained));
  if (browser) await browser.close().catch(() => {});
  process.exit(1);
});
