// Takes the README screenshots for tools/readme_screenshots/generate.py, which runs the server
// and plays the demo first. It only opens pages and clicks what a person would; it never changes
// the page.
//
// PLAYWRIGHT names the playwright module; BY_URL the server; BY_TOKEN the server token the demo
// ran as, which the connect form takes, so the browser is a member of the workspace the demo
// made; BY_PLAN what to open and the reply text each view must show; BY_FORBIDDEN text no page
// may contain anywhere, its paths and token among them; BY_OUT the directory for the PNGs.
'use strict';

const path = require('path');
const { chromium } = require(process.env.PLAYWRIGHT);

const plan = JSON.parse(process.env.BY_PLAN);
const forbidden = JSON.parse(process.env.BY_FORBIDDEN);
const app = `${process.env.BY_URL}/app/`;
const TURN = { timeout: 60000 };

async function signIn(browser, options) {
  const context = await browser.newContext({ colorScheme: 'light', reducedMotion: 'reduce', ...options });
  const page = await context.newPage();
  const problems = [];
  page.on('pageerror', (e) => problems.push(String(e)));
  page.on('console', (m) => { if (m.type() === 'error') problems.push(m.text()); });
  await page.goto(app);
  await page.getByLabel('Server token', { exact: true }).fill(process.env.BY_TOKEN);
  await page.getByRole('button', { name: 'Connect to server', exact: true }).click();
  await page.locator('#branch-list a.branch-row').first().waitFor(TURN);
  return { page, problems };
}

async function shoot(page, problems, file, schemes) {
  await page.mouse.move(0, 0);
  await page.waitForFunction(() => !document.querySelector('.toast'), null, { timeout: 15000 });
  if (problems.length) throw new Error(`the page reported: ${problems.join('; ')}`);
  const text = await page.evaluate(() => document.body.innerText);
  const leaked = forbidden.filter((secret) => text.includes(secret));
  if (leaked.length) throw new Error(`${file} would show ${leaked.length} forbidden string(s)`);
  for (const scheme of schemes) {
    await page.emulateMedia({ colorScheme: scheme });
    const name = scheme === 'dark' ? `${file}-dark.png` : `${file}.png`;
    await page.screenshot({ path: path.join(process.env.BY_OUT, name), animations: 'disabled' });
    process.stdout.write(`${name}\n`);
  }
  await page.emulateMedia({ colorScheme: 'light' });
}

async function openBranch(page) {
  await page.locator('header#strip').getByRole('button', { name: 'Merge', exact: true }).waitFor(TURN);
  await page.locator('#convo .turn .agent', { hasText: plan.branchSays }).waitFor(TURN);
  await page.locator('#changes pre.diff').waitFor(TURN);
}

(async () => {
  const browser = await chromium.launch();
  try {
    const desktop = await signIn(browser, { viewport: { width: 1440, height: 900 } });
    let page = desktop.page;
    await page.goto(`${app}#/b/${plan.repo}/${plan.branch}`);
    await openBranch(page);
    await page.locator('#convo details.tools summary').click();
    await page.locator('#convo details.tools[open] li').first().waitFor();
    await shoot(page, desktop.problems, 'console-branches', ['light', 'dark']);

    await page.goto(`${app}#/w/${plan.workspace}/s/${plan.session}`);
    await page.locator(`nav#sessions a.session-tab[data-session="${plan.session}"][aria-current="page"]`).waitFor(TURN);
    await page.locator('#convo .turn .agent', { hasText: plan.sessionSays }).waitFor(TURN);
    await page.locator('#tab-outputs').click();
    await page.locator(`#outputs li[data-artifact="${plan.output}"] button.output-name`).click();
    await page.locator('#preview-host .by-artifact-preview table td').first().waitFor(TURN);
    await shoot(page, desktop.problems, 'console-workspace', ['light', 'dark']);

    const phone = await signIn(browser, { viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true });
    page = phone.page;
    await page.locator('#branch-list a.branch-row', { hasText: plan.branch }).click();
    await openBranch(page);
    await shoot(page, phone.problems, 'console-phone', ['light']);
  } finally {
    await browser.close();
  }
})().catch((e) => {
  process.stderr.write(`${e.stack || e}\n`);
  process.exit(1);
});
