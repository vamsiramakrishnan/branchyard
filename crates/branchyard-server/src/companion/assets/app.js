// The Branchyard companion: one page over the server's own HTTP API and
// event stream (docs/companion.md). Every action is an API call made with
// the bearer token this tab holds, under that token's scopes; the page
// adds no authority of its own. Text from the server is only ever set as
// text (textContent), never parsed as HTML.
'use strict';

(() => {
  const TOKEN_KEY = 'branchyard.token';
  const REPO_KEY = 'branchyard.repo';
  const SOURCE_KEY = 'branchyard.source';
  // What `R` in `by watch` sends to an interrupted branch.
  const RESUME_PROMPT = 'Your previous turn was interrupted before it finished. ' +
    'Continue the task from where you left off, then summarize what you did.';
  // What "Collect outputs" asks of a new session: a required deliverable
  // whose declared files the server publishes as the workspace's outputs
  // after the turn (docs/artifact-capture.md).
  const OUTPUT_MANIFEST = 'outputs/artifacts.json';
  const OUTPUT_CONTRACT = {
    schema: { type: 'object', required: ['summary'], properties: { summary: { type: 'string' } } },
    required: true, purpose: 'deliverable', capture: { manifest: OUTPUT_MANIFEST },
  };
  const DIFF_LIMIT = 400000;
  // The server's largest event page; an open branch keeps at most
  // EVENT_KEEP of its latest events.
  const EVENT_WINDOW = 200;
  const EVENT_KEEP = 2000;
  const SVG = 'http://www.w3.org/2000/svg';

  const state = {
    token: null,
    me: null,
    repos: [],
    repo: null,
    // What the sidebar lists: a repository's branches ({ kind: 'repo' },
    // the one in `repo`), a project's workspaces ({ kind: 'project', id }),
    // or nothing yet ({ kind: 'none' }).
    source: { kind: 'none' },
    // null when this server has no projects; otherwise every project.
    projects: null,
    // The listed project's workspaces.
    workspaces: { project: null, list: [], loaded: false, error: null },
    branches: new Map(),
    lastActive: new Map(),
    cursor: null,
    stream: null,
    backoff: 1000,
    loaded: false,
    branchError: null,
    filter: '',
    doneOpen: false,
    seen: new Set(),
    statusSeen: new Map(),
    permissions: [],
    inbox: [],
    approvals: [],
    effects: [],
    pending: new Map(),
    merged: new Set(),
    route: { name: 'home' },
    pushInfo: null,
    pushOn: false,
  };

  // ---------------------------------------------------------------
  // Storage: the token lives in this tab's sessionStorage only.

  function load(key) {
    try { return sessionStorage.getItem(key); } catch (_) { return null; }
  }
  function save(key, value) {
    try {
      if (value == null) sessionStorage.removeItem(key); else sessionStorage.setItem(key, value);
    } catch (_) { /* private mode: kept in memory for this page only */ }
  }

  // ---------------------------------------------------------------
  // Elements

  function h(tag, props, ...children) {
    const el = document.createElement(tag);
    for (const [key, value] of Object.entries(props || {})) {
      if (value == null || value === false) continue;
      if (key === 'class') el.className = value;
      else if (key === 'text') el.textContent = value;
      else if (key.startsWith('on')) el.addEventListener(key.slice(2), value);
      else if (value === true) el.setAttribute(key, '');
      else el.setAttribute(key, String(value));
    }
    for (const child of children.flat(Infinity)) {
      if (child == null || child === false) continue;
      el.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return el;
  }
  const $ = (id) => document.getElementById(id);
  const enc = encodeURIComponent;
  const cap = (text) => text.charAt(0).toUpperCase() + text.slice(1);
  // A `when` reason as a sentence; one that opens with a branch's name keeps its case.
  const sentence = (why, name) => `${name && why.startsWith(name) ? why : cap(why)}.`;
  const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;
  const branchHref = (name) => `#/b/${enc(state.repo)}/${enc(name)}`;
  let uid = 0;
  const nextId = (prefix) => `${prefix}-${++uid}`;

  // Replace an element's children, skipping what is null or false, as h() does.
  function fill(el, ...children) {
    el.replaceChildren(...children.flat(Infinity).filter((c) => c != null && c !== false));
    return el;
  }

  // A page's heading takes focus on navigation, and keeps it when a live
  // update redraws the page.
  function main(...children) {
    const m = $('main');
    const kept = document.activeElement && document.activeElement.tagName === 'H1' && m.contains(document.activeElement);
    m.removeAttribute('aria-busy');
    fill(m, ...children);
    const heading = m.querySelector('h1');
    if (heading) heading.tabIndex = -1;
    if (heading && kept) heading.focus({ preventScroll: true });
  }

  function focusHeading() {
    const heading = $('main').querySelector('h1');
    if (heading) heading.focus({ preventScroll: false });
  }

  const dot = (lane) => h('span', { class: `dot dot-${lane}`, 'aria-hidden': 'true' });

  function counts(insertions, deletions) {
    return [insertions || !deletions ? h('span', { class: 'add', text: `+${insertions}` }) : null,
      insertions && deletions ? ' ' : null,
      deletions ? h('span', { class: 'del', text: `−${deletions}` }) : null];
  }

  function checkMark(drawn) {
    const svg = document.createElementNS(SVG, 'svg');
    svg.setAttribute('viewBox', '0 0 16 16');
    svg.setAttribute('class', drawn ? 'check drawn' : 'check');
    svg.setAttribute('aria-hidden', 'true');
    const path = document.createElementNS(SVG, 'path');
    path.setAttribute('d', 'M3 8.5l3.2 3L13 4.5');
    svg.append(path);
    return svg;
  }

  // Only what the person did here gets a toast: the branch list is the
  // notice for everything else.
  function toast(text, href) {
    const box = h('div', { class: 'toast' }, text, href ? [' ', h('a', { href, text: 'Open' })] : null);
    const toasts = $('toasts');
    toasts.append(box);
    while (toasts.children.length > 2) toasts.firstChild.remove();
    setTimeout(() => box.remove(), 6000);
  }

  function setLive(which, text) {
    const live = $('live');
    live.dataset.state = which;
    live.textContent = text || which;
  }

  function confirmDialog(title, text, okText, danger) {
    const dialog = $('confirm');
    $('confirm-title').textContent = title;
    $('confirm-text').textContent = text;
    const ok = $('confirm-ok');
    ok.textContent = okText || 'Confirm';
    ok.className = danger ? 'danger' : 'primary';
    if (typeof dialog.showModal !== 'function') return Promise.resolve(window.confirm(`${title}\n\n${text}`));
    return new Promise((resolve) => {
      const done = (answer) => {
        ok.removeEventListener('click', yes);
        $('confirm-cancel').removeEventListener('click', no);
        dialog.removeEventListener('cancel', no);
        if (dialog.open) dialog.close();
        resolve(answer);
      };
      const yes = () => done(true);
      const no = () => done(false);
      ok.addEventListener('click', yes);
      $('confirm-cancel').addEventListener('click', no);
      dialog.addEventListener('cancel', no);
      dialog.showModal();
      $('confirm-cancel').focus();
    });
  }

  // ---------------------------------------------------------------
  // The API

  class ApiError extends Error {
    constructor(status, code, message, detail) {
      super(message);
      this.status = status;
      this.code = code;
      this.detail = detail || null;
    }
  }

  function key() {
    if (window.crypto && crypto.randomUUID) return crypto.randomUUID();
    const bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  }

  // Every request carries this tab's token, and nothing else of the
  // browser's (no cookies, no cache).
  function authFetch(path, init = {}) {
    const headers = new Headers(init.headers || {});
    if (state.token) headers.set('Authorization', `Bearer ${state.token}`);
    return fetch(path, { ...init, headers, cache: 'no-store', credentials: 'omit' });
  }

  // A response as JSON, or its error envelope as an ApiError.
  async function call(path, init) {
    let response;
    try {
      response = await authFetch(path, init);
    } catch (e) {
      throw new ApiError(0, 'unavailable', 'The server cannot be reached.');
    }
    const text = await response.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch (_) { json = null; }
    if (!response.ok) {
      const err = (json && json.error) || {};
      if (response.status === 401 && state.me) {
        signOut('This token is no longer accepted (expired or revoked). Pair again.');
      }
      throw new ApiError(response.status, err.code || 'error', err.message || `HTTP ${response.status}`, err.detail);
    }
    return json;
  }

  function api(method, path, body) {
    const headers = { Accept: 'application/json' };
    const init = { method, headers };
    if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
      if (method === 'POST') headers['Idempotency-Key'] = key();
    }
    return call(path, init);
  }

  // An operation polled until it settles, at `by`'s cadence: every second
  // at first, then every three. null when it is still going after half an
  // hour, or this tab signed out.
  async function awaitOperation(id) {
    for (let i = 0; i < 600 && state.token; i++) {
      await new Promise((r) => setTimeout(r, i < 10 ? 1000 : 3000));
      let op;
      try { op = await api('GET', `/v1/operations/${enc(id)}`); } catch (_) { continue; }
      if (['succeeded', 'failed', 'interrupted'].includes(op.state)) return op;
    }
    return null;
  }

  // Save a response's body under its name, through a Blob URL that is
  // revoked once the download has had time to start.
  async function download(path, name) {
    let blob;
    try {
      const response = await authFetch(path);
      if (!response.ok) {
        let message = `HTTP ${response.status}`;
        try { message = (await response.json()).error.message || message; } catch (_) { /* not JSON */ }
        throw new Error(message);
      }
      blob = await response.blob();
    } catch (e) {
      toast(`Could not download ${name}: ${e instanceof TypeError ? 'the server cannot be reached' : e.message}.`);
      return;
    }
    const url = URL.createObjectURL(blob);
    const link = h('a', { href: url, download: name, hidden: true });
    document.body.append(link);
    link.click();
    link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 60000);
  }

  const can = (scope) => !!(state.me && state.me.scopes.includes(scope));
  const repoPath = (repo) => `/v1/repos/${enc(repo || state.repo)}`;
  const branchPath = (branch, repo) => `${repoPath(repo)}/branches/${enc(branch)}`;

  // ---------------------------------------------------------------
  // Statuses, the same words `by ls` uses

  function statusOf(info) {
    const s = info.status || {};
    if (info.stalled && s.state === 'running') return { key: 'stalled', text: 'running, stalled' };
    switch (s.state) {
      case 'running': return { key: 'running', text: 'running' };
      case 'waiting': return { key: 'waiting', text: 'waiting' };
      case 'blocked': return { key: 'blocked', text: `blocked: ${s.reason}` };
      case 'ready': return { key: 'ready', text: 'ready' };
      case 'no_changes': return { key: 'no_changes', text: 'no changes' };
      case 'interrupted': return { key: 'interrupted', text: 'interrupted' };
      case 'budget_exceeded': return { key: 'budget_exceeded', text: `over budget (${s.limit})` };
      case 'failed': return { key: 'failed', text: `failed: ${s.reason}` };
      case 'merged': return { key: 'merged', text: `merged into ${s.target}` };
      case 'awaiting_plan_approval': return { key: 'awaiting_plan_approval', text: 'awaiting plan approval' };
      default: return { key: 'unknown', text: s.state || 'unknown' };
    }
  }

  // When each action applies: the rules of `by watch`'s action registry.
  const when = {
    send(info) {
      const st = info.status.state;
      if (st === 'running') return `${info.name} has a turn running; steer it or cancel it`;
      if (st === 'waiting') return `${info.name} is waiting for its prerequisites and has no session yet`;
      return null;
    },
    steer(info) {
      return info.status.state === 'running' ? null : `${info.name} has no turn running; send a new prompt instead`;
    },
    resume(info) {
      return info.status.state === 'interrupted' ? null : 'only an interrupted branch resumes';
    },
    cancel(info) {
      return ['running', 'waiting'].includes(info.status.state) ? null : `nothing to cancel: ${info.name} is ${statusOf(info).text}`;
    },
    merge(info) {
      return info.status.state === 'ready' && info.candidate ? null : `only a ready branch merges; ${info.name} is ${statusOf(info).text}`;
    },
    fork(info) {
      return ['waiting', 'blocked'].includes(info.status.state) ? `${info.name} has not started, so there is nothing to fork` : null;
    },
    diff(info) {
      return info.candidate ? null : `${info.name} has no candidate commit yet`;
    },
  };

  // Branches grouped by what they ask of you, in display order: the first
  // lane whose test passes takes the branch. `attention` maps a branch to
  // the approval or question it waits on a person for.
  const LANES = [
    {
      key: 'needs', label: 'Needs you', collapsed: false,
      test: (b, attention) => attention.has(b.name) || b.stalled ||
        ['blocked', 'failed', 'interrupted', 'budget_exceeded', 'awaiting_plan_approval'].includes(b.status.state),
      meta: (b, attention) => attention.get(b.name) || (b.stalled ? 'stalled' : statusOf(b).text.split(/[:(]/)[0].trim()),
    },
    {
      key: 'running', label: 'Running', collapsed: false,
      test: (b) => ['running', 'waiting'].includes(b.status.state),
      meta: (b) => b.status.state,
    },
    {
      key: 'ready', label: 'Ready', collapsed: false,
      test: (b) => b.status.state === 'ready',
      meta: (b) => (b.candidate ? counts(b.candidate.insertions, b.candidate.deletions) : 'ready'),
    },
    {
      key: 'done', label: 'Done', collapsed: true,
      test: () => true,
      meta: (b) => (b.status.state === 'merged' ? 'merged' : statusOf(b).text),
    },
  ];

  function attention() {
    const waiting = new Map();
    for (const m of state.inbox) waiting.set(m.from, m.kind);
    for (const a of state.approvals) waiting.set(a.branch, 'approval');
    return waiting;
  }

  const laneOf = (b, waiting) => LANES.find((lane) => lane.test(b, waiting));
  const activeAt = (b) => state.lastActive.get(b.name) || b.created_at * 1000;

  function lanes(branches) {
    const waiting = attention();
    const sorted = branches.slice().sort((x, y) => activeAt(y) - activeAt(x));
    return LANES.map((lane) => ({ lane, branches: sorted.filter((b) => laneOf(b, waiting) === lane) }));
  }

  // Workspaces grouped by state in the same order and colours: one that
  // failed to start needs you; one starting, running, archiving or
  // restoring runs; an idle one is ready for work; archived ones fold away.
  const WORKSPACE_LANES = [
    { key: 'needs', label: 'Needs you', collapsed: false, states: ['failed', 'interrupted'] },
    { key: 'running', label: 'Running', collapsed: false, states: ['running', 'pending', 'restoring', 'archiving'] },
    { key: 'ready', label: 'Ready', collapsed: false, states: ['ready'] },
    { key: 'done', label: 'Archived', collapsed: true, states: null },
  ];
  const workspaceLane = (w) => WORKSPACE_LANES.find((lane) => !lane.states || lane.states.includes(w.state));
  const workspaceStateText = (w) => ({ pending: 'starting' }[w.state] || w.state);

  function money(v) {
    return v == null ? '—' : `$${Number(v).toFixed(v < 1 ? 4 : 2)}`;
  }

  function since(ms) {
    const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
    if (s < 60) return `${s}s`;
    if (s < 3600) return `${Math.round(s / 60)}m`;
    if (s < 86400) return `${Math.round(s / 3600)}h`;
    return `${Math.round(s / 86400)}d`;
  }

  // A relative time that ticks (see boot) without redrawing its page.
  function ago(ms, suffix = ' ago') {
    return h('time', { datetime: new Date(ms).toISOString(), 'data-at': ms, 'data-suffix': suffix, text: since(ms) + suffix });
  }

  function clock(ms) {
    const d = new Date(ms);
    return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  }

  // ---------------------------------------------------------------
  // What deserves a system notification: `by watch`'s rules, each once

  function notice(entry) {
    const a = entry.activity || {};
    const b = entry.branch;
    let kind = null;
    let text = null;
    let target = b;
    if (a.harness && a.harness.type === 'permission_requested') {
      const k = `perm:${b}:${JSON.stringify(a.harness.request.key)}`;
      if (state.seen.has(k)) return null;
      state.seen.add(k);
      kind = 'permission';
      text = `${b} asks to use ${a.harness.request.tool}`;
    } else if (a.message && ['question', 'escalation'].includes(a.message.kind)) {
      const k = `msg:${a.message.id}`;
      if (state.seen.has(k)) return null;
      state.seen.add(k);
      kind = 'question';
      target = a.message.to;
      text = `${a.message.from} ${a.message.kind === 'question' ? 'asks' : 'escalates to'} ${a.message.to}: ${a.message.text}`;
    } else if (a.effect && a.effect.kind === 'asked') {
      const k = `approval:${a.effect.ask}`;
      if (state.seen.has(k)) return null;
      state.seen.add(k);
      kind = 'approval';
      text = `${b} waits for approval: ${aboutText(a.effect.about)}`;
    } else if (a.stalled) {
      const k = `stall:${b}:${a.stalled.since_ms}`;
      if (state.seen.has(k)) return null;
      state.seen.add(k);
      kind = 'stalled';
      text = `${b} has stalled`;
    } else if (a.status) {
      const st = a.status.state;
      if (st === 'running' || st === 'waiting') { state.statusSeen.delete(b); return null; }
      if (st === 'merged') return null;
      const map = {
        ready: ['finished', `${b} is ready to merge`],
        no_changes: ['finished', `${b} finished with no changes`],
        budget_exceeded: ['finished', `${b} stopped at its budget`],
        interrupted: ['interrupted', `${b} was interrupted`],
        failed: ['failed', `${b} failed: ${a.status.reason}`],
        blocked: ['failed', `${b} is blocked: ${a.status.reason}`],
        awaiting_plan_approval: ['question', `${b} has a plan waiting for your approval`],
      };
      if (!map[st]) return null;
      [kind, text] = map[st];
      if (state.statusSeen.get(b) === kind) return null;
      state.statusSeen.set(b, kind);
    } else {
      return null;
    }
    const href = kind === 'question' || kind === 'approval' ? '#/attention' : branchHref(target);
    return { kind, text, href };
  }

  // ---------------------------------------------------------------
  // Signing in and pairing

  function signOut(message) {
    state.token = null;
    state.me = null;
    save(TOKEN_KEY, null);
    if (state.stream) state.stream.abort();
    state.stream = null;
    closePreview();
    closeWorkspace();
    clearTimeout(workspacesTimer);
    Object.assign(state, {
      repos: [], repo: null, source: { kind: 'none' }, projects: null,
      workspaces: { project: null, list: [], loaded: false, error: null },
      branches: new Map(), lastActive: new Map(), loaded: false, branchError: null,
      permissions: [], inbox: [], approvals: [], effects: [],
    });
    $('sidebar').hidden = true;
    document.title = 'Branchyard';
    setLive('off', 'signed out');
    renderSignIn(message);
  }

  function renderSignIn(message) {
    document.body.dataset.view = 'signin';
    const id = nextId('token');
    const error = h('p', { class: 'error', role: 'alert', text: message || '' });
    const input = h('input', { id, type: 'password', autocomplete: 'off', spellcheck: 'false', required: true });
    const connect = h('button', { type: 'button', class: 'primary', text: 'Connect to server', onclick: () => submit() });
    const submit = async () => {
      const token = input.value.trim();
      if (!token) { input.focus(); input.reportValidity(); return; }
      if (connect.disabled) return;
      connect.disabled = true;
      connect.textContent = 'Connecting…';
      error.textContent = '';
      state.token = token;
      try {
        await start();
        save(TOKEN_KEY, token);
      } catch (e) {
        state.token = null;
        error.textContent = e.status === 401 ? 'That token is not accepted.' : e.message;
        connect.disabled = false;
        connect.textContent = 'Connect to server';
      }
    };
    input.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') submit(); });
    main(h('div', { class: 'signin' },
      h('img', { class: 'signin-mark', src: 'icon.svg', alt: '', width: 32, height: 32 }),
      h('h1', { text: 'Connect to Branchyard' }),
      h('p', { class: 'lede', text: 'Follow your tasks and review their branches from this browser.' }),
      h('section', { class: 'card connect', 'aria-label': 'Connect to your server' },
        h('h2', { text: 'Open a pairing link' }),
        h('p', {}, 'On the machine running your server, run ', h('code', { text: 'by serve token new --link' }),
          '. Open the printed link on this device.'),
        h('h2', { text: 'Or use a server token' }),
        h('label', { class: 'block', for: id, text: 'Server token' }), input,
        h('p', { class: 'hint', text: 'Kept only in this browser tab and sent only to this server.' }),
        h('div', { class: 'row form-actions' }, connect)),
      error));
    focusHeading();
  }

  function deviceLabel() {
    const ua = navigator.userAgent || '';
    const browser = /Firefox\//.test(ua) ? 'Firefox' : /Edg\//.test(ua) ? 'Edge'
      : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : 'Browser';
    const os = /Android/.test(ua) ? 'Android' : /iPhone|iPad/.test(ua) ? 'iOS' : /Mac OS X/.test(ua) ? 'macOS'
      : /Windows/.test(ua) ? 'Windows' : /Linux/.test(ua) ? 'Linux' : '';
    return os ? `${browser} on ${os}` : browser;
  }

  async function pair(code) {
    // Take the code out of the address bar and history first.
    history.replaceState(null, '', location.pathname + location.search);
    document.body.dataset.view = 'signin';
    main(h('div', { class: 'signin' }, h('h1', { text: 'Pairing…' })));
    try {
      const paired = await api('POST', '/app/pair', { code, device: deviceLabel() });
      state.token = paired.token;
      save(TOKEN_KEY, paired.token);
      toast(`Paired as ${paired.me.name}.`);
      return true;
    } catch (e) {
      renderSignIn(e.code === 'rate_limited' ? e.message : `Pairing failed: ${e.message}`);
      return false;
    }
  }

  // ---------------------------------------------------------------
  // Loading and the live stream

  // A token that reaches no repository still has a console when this
  // server has projects: knowledge work needs none.
  async function start() {
    state.me = await api('GET', '/v1/app/me');
    const [{ repos }, projects] = await Promise.all([api('GET', '/v1/repos'), loadProjects()]);
    state.repos = repos.map((r) => r.name);
    state.projects = projects instanceof ApiError ? null : projects;
    if (!state.repos.length && !state.projects) {
      if (projects instanceof ApiError) throw projects;
      throw new ApiError(403, 'no_repos', 'This token can reach no repository.');
    }
    if (projects instanceof ApiError) toast(`Projects did not load: ${projects.message}`);
    const asked = parseRoute();
    const remembered = load(REPO_KEY);
    state.repo = state.repos.includes(asked.repo) ? asked.repo : state.repos.includes(remembered) ? remembered : state.repos[0] || null;
    state.source = initialSource(asked);
    renderPicker();
    $('sidebar').hidden = false;
    $('live').hidden = !state.repo;
    const loading = state.repo ? switchRepo(state.repo) : Promise.resolve();
    if (state.source.kind === 'project') loadWorkspaces(state.source.id);
    route();
    await loading;
  }

  // ---------------------------------------------------------------
  // Sources: repositories and projects, in the sidebar's picker

  // Every project; null on a server without them; the error when the list
  // failed, so a token with repositories still gets its branches.
  async function loadProjects() {
    try {
      return (await api('GET', '/v1/projects')).projects;
    } catch (e) {
      if (e.status === 404) return null;
      if (e.status === 401) throw e;
      return e;
    }
  }

  const projectOf = (id) => (state.projects || []).find((p) => p.id === id) || null;

  // A workspace's route names no project: opening it shows its own.
  function initialSource(asked) {
    if (asked.name === 'branch' && state.repo) return { kind: 'repo' };
    if (asked.name === 'project') return { kind: 'project', id: asked.project };
    const remembered = /^project:(.+)$/.exec(load(SOURCE_KEY) || '');
    if (remembered && projectOf(remembered[1])) return { kind: 'project', id: remembered[1] };
    if (state.repo) return { kind: 'repo' };
    if (state.projects && state.projects.length) return { kind: 'project', id: state.projects[0].id };
    return { kind: 'none' };
  }

  function useSource(source) {
    state.source = source;
    save(SOURCE_KEY, source.kind === 'project' ? `project:${source.id}` : source.kind);
    renderPicker();
  }

  // One select for both: Repositories, then Projects, then "New project…".
  function renderPicker() {
    const select = $('repo');
    const current = state.source.kind === 'project' ? `project:${state.source.id}` : state.repo ? `repo:${state.repo}` : '';
    const groups = [];
    if (state.repos.length) {
      groups.push(h('optgroup', { label: 'Repositories' }, state.repos.map((r) => h('option', { value: `repo:${r}`, text: r }))));
    }
    if (state.projects && state.projects.length) {
      groups.push(h('optgroup', { label: 'Projects' }, state.projects.map((p) => h('option', { value: `project:${p.id}`, text: p.name }))));
    }
    const creating = state.projects && can('run');
    fill(select, current ? null : h('option', { value: '', text: 'Choose a project', disabled: true }), groups,
      creating ? h('option', { value: 'new-project', text: 'New project…' }) : null);
    select.value = current;
    const options = select.options.length;
    $('repo-pick').hidden = options < 2;
    $('repo-name').hidden = options > 1;
    $('repo-name').textContent = current.startsWith('repo:') ? state.repo : (projectOf(state.source.id) || {}).name || '';
    updateSideLinks();
  }

  async function pickSource(value) {
    if (value === 'new-project') { renderPicker(); go('#/p/new'); return; }
    const at = value.indexOf(':');
    const kind = value.slice(0, at);
    const name = value.slice(at + 1);
    if (kind === 'project') { go(`#/p/${enc(name)}`); return; }
    closePreview();
    closeWorkspace();
    useSource({ kind: 'repo' });
    if (name !== state.repo) await switchRepo(name);
    renderPicker();
    go('#/');
  }

  // Navigate, or route again when the address already says so.
  function go(hash) {
    if (location.hash === hash) route(); else location.hash = hash;
  }

  // The sidebar's link to start work follows the source.
  function updateSideLinks() {
    const link = $('new-task');
    const project = state.source.kind === 'project' ? state.source.id : null;
    link.hidden = !can('run') || (!project && !state.repo);
    link.href = project ? `#/p/${enc(project)}/new` : '#/new';
    link.dataset.route = project ? 'newWorkspace' : 'new';
    link.lastChild.textContent = project ? 'New workspace' : 'New task';
    const what = project ? 'workspaces' : 'branches';
    $('branch-filter').placeholder = `Filter ${what}`;
    $('branch-filter-label').textContent = `Filter ${what}`;
    $('sidebar').setAttribute('aria-label', cap(what));
  }

  // The listed project's workspaces, refreshed every few seconds while
  // one of them is starting, running, archiving or restoring.
  let workspacesTimer = null;
  async function loadWorkspaces(project) {
    clearTimeout(workspacesTimer);
    const list = state.workspaces;
    if (list.project !== project) state.workspaces = { project, list: [], loaded: false, error: null };
    renderLanes();
    let workspaces;
    try {
      ({ workspaces } = await api('GET', `/v1/workspaces?project=${enc(project)}`));
    } catch (e) {
      if (state.workspaces.project !== project) return;
      state.workspaces.error = e.message;
      workspacesChanged();
      return;
    }
    if (state.workspaces.project !== project) return;
    state.workspaces = { project, list: workspaces, loaded: true, error: null };
    workspacesChanged();
    if (workspaces.some((w) => workspaceLane(w).key === 'running')) {
      workspacesTimer = setTimeout(() => {
        if (state.source.kind === 'project' && state.source.id === project) loadWorkspaces(project);
      }, 3000);
    }
  }

  function workspacesChanged() {
    renderLanes();
    if (state.route.name === 'project' || (state.route.name === 'home' && state.source.kind === 'project')) updateProjectGlance();
  }

  // Show a project in the sidebar, loading its workspaces when it was not.
  function useProject(id) {
    if (state.source.kind !== 'project' || state.source.id !== id) useSource({ kind: 'project', id });
    if (state.workspaces.project !== id) loadWorkspaces(id);
  }

  async function switchRepo(repo) {
    state.repo = repo;
    save(REPO_KEY, repo);
    state.branches = new Map();
    state.lastActive = new Map();
    state.cursor = null;
    state.loaded = false;
    state.branchError = null;
    state.permissions = [];
    state.inbox = [];
    state.approvals = [];
    state.effects = [];
    if (state.stream) state.stream.abort();
    openStream();
    await loadBranches();
  }

  async function loadBranches() {
    const repo = state.repo;
    state.branchError = null;
    renderLanes();
    let branches;
    try {
      ({ branches } = await api('GET', `${repoPath()}/branches`));
    } catch (e) {
      if (e.status === 401) throw e;
      if (repo !== state.repo) return;
      state.branchError = e.message;
      branchesChanged();
      return;
    }
    if (repo !== state.repo) return;
    state.branches = new Map(branches.map((b) => [b.name, b]));
    state.loaded = true;
    refreshInbox();
    refreshApprovals();
    branchesChanged();
    if (state.route.name === 'queue') renderQueue();
  }

  const refreshing = new Map();
  function refreshBranch(name) {
    if (refreshing.has(name)) return;
    refreshing.set(name, setTimeout(async () => {
      refreshing.delete(name);
      try {
        const info = await api('GET', branchPath(name));
        state.branches.set(name, info);
      } catch (e) {
        if (e.status === 404) state.branches.delete(name);
      }
      branchesChanged();
    }, 250));
  }

  function openStream() {
    const controller = new AbortController();
    state.stream = controller;
    const repo = state.repo;
    const url = `${repoPath(repo)}/events/stream${state.cursor != null ? `?cursor=${state.cursor}` : ''}`;
    setLive('reconnecting', 'connecting');
    (async () => {
      let response;
      try {
        response = await fetch(url, {
          headers: { Authorization: `Bearer ${state.token}`, Accept: 'text/event-stream' },
          signal: controller.signal, cache: 'no-store', credentials: 'omit',
        });
      } catch (_) {
        return retry(controller);
      }
      if (response.status === 401) { signOut('This token is no longer accepted (expired or revoked). Pair again.'); return; }
      if (response.status === 400) {
        // A cursor past the feed's end (a reset store): start again from now.
        state.cursor = null;
        loadBranches().catch(() => {});
        return retry(controller, 0);
      }
      if (!response.ok || !response.body) return retry(controller);
      state.backoff = 1000;
      setLive('live', 'live');
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      try {
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true }).replace(/\r\n?/g, '\n');
          let at;
          while ((at = buffer.indexOf('\n\n')) >= 0) {
            const block = buffer.slice(0, at);
            buffer = buffer.slice(at + 2);
            if (controller === state.stream) onSse(block);
          }
        }
      } catch (_) { /* dropped: reconnect from the cursor */ }
      retry(controller);
    })();
  }

  function retry(controller, delay) {
    if (controller !== state.stream || controller.signal.aborted || !state.token) return;
    setLive('reconnecting', 'reconnecting');
    const wait = delay != null ? delay : state.backoff;
    state.backoff = Math.min(state.backoff * 2, 30000);
    setTimeout(() => { if (controller === state.stream) openStream(); }, wait);
  }

  function onSse(block) {
    let event = 'message';
    const data = [];
    for (const line of block.split('\n')) {
      if (!line || line.startsWith(':')) continue;
      const colon = line.indexOf(':');
      const field = colon < 0 ? line : line.slice(0, colon);
      const value = colon < 0 ? '' : line.slice(colon + 1).replace(/^ /, '');
      if (field === 'event') event = value;
      else if (field === 'data') data.push(value);
    }
    let payload;
    try { payload = JSON.parse(data.join('\n')); } catch (_) { return; }
    if (event === 'open') {
      const first = state.cursor == null;
      state.cursor = payload.cursor;
      // Anything recorded between the branch list and the stream's start.
      if (first && state.loaded) loadBranches().catch(() => {});
      return;
    }
    if (event !== 'activity') return;
    state.cursor = payload.seq;
    onActivity(payload);
  }

  function onActivity(entry) {
    const a = entry.activity || {};
    state.lastActive.set(entry.branch, entry.at_ms);
    if (a.harness && a.harness.type === 'permission_requested') {
      state.permissions.unshift({ branch: entry.branch, tool: a.harness.request.tool, at_ms: entry.at_ms, decision: null });
      state.permissions.length = Math.min(state.permissions.length, 50);
    } else if (a.decision) {
      const p = state.permissions.find((x) => x.branch === entry.branch && x.decision == null);
      if (p) p.decision = a.decision.allowed ? 'allowed' : 'denied';
    }
    if (a.message) refreshInbox();
    if (a.effect) refreshApprovals();
    if (state.loaded && Date.now() - entry.at_ms < 60000) {
      const n = notice(entry);
      // On screen the branch list is the notice; a hidden tab gets the system's.
      if (n && document.hidden && 'Notification' in window && Notification.permission === 'granted' && !state.pushOn) {
        try { new Notification(`${state.repo}: ${n.text}`, { tag: n.href }); } catch (_) { /* page notifications unavailable */ }
      }
    }
    refreshBranch(entry.branch);
    if (view && view.name === entry.branch) refreshTimeline(view);
  }

  // ---------------------------------------------------------------
  // Routing

  // Inbox and Approvals became Needs you; push notifications still open them.
  const ALIASES = { '': 'home', inbox: 'attention', approvals: 'attention' };
  const VIEWS = {
    home: renderHome,
    branch: renderBranch,
    new: renderNew,
    attention: renderAttention,
    triggers: renderTriggers,
    queue: renderQueue,
    settings: renderSettings,
    newProject: renderNewProject,
    project: renderProject,
    workspace: renderWorkspace,
  };

  function parseRoute() {
    const hash = location.hash || '#/';
    const parts = hash.replace(/^#\/?/, '').split('/').map((p) => { try { return decodeURIComponent(p); } catch (_) { return p; } });
    if (parts[0] === 'b') return { name: 'branch', repo: parts[1], branch: parts.slice(2).join('/') };
    if (parts[0] === 'p' && parts[1] === 'new') return { name: 'newProject' };
    if (parts[0] === 'p' && parts[1]) return { name: 'project', project: parts[1], creating: parts[2] === 'new' };
    if (parts[0] === 'w' && parts[1]) return { name: 'workspace', workspace: parts[1], session: parts[2] === 's' && parts[3] ? parts[3] : null };
    const name = ALIASES[parts[0]] || parts[0];
    return { name: VIEWS[name] && !['branch', 'newProject', 'project', 'workspace'].includes(name) ? name : 'home' };
  }

  async function route() {
    if (!state.token) return;
    closePreview();
    state.route = parseRoute();
    const r = state.route;
    if (r.name === 'branch' && r.repo && state.repos.includes(r.repo)) {
      if (state.source.kind !== 'repo') useSource({ kind: 'repo' });
      if (r.repo !== state.repo) { await switchRepo(r.repo); renderPicker(); }
    }
    if (r.name === 'project') useProject(r.project);
    if (r.name !== 'workspace' || (wsView && wsView.id !== r.workspace)) closeWorkspace();
    // On a phone a project's glance is its workspace list, the sidebar.
    document.body.dataset.view = r.name === 'project' && !r.creating ? 'home' : r.name;
    view = null;
    drawn = null;
    const current = r.name === 'project' && r.creating ? 'newWorkspace' : r.name;
    for (const link of document.querySelectorAll('#sidebar a[data-route]')) {
      if (link.dataset.route === current) link.setAttribute('aria-current', 'page');
      else link.removeAttribute('aria-current');
    }
    renderLanes();
    await VIEWS[r.name]();
    const field = r.name === 'project' && r.creating ? $('workspace-title') : null;
    if (field) field.focus(); else focusHeading();
  }

  // A live update redraws a page only when what it shows changed, so focus
  // and half-typed text survive the stream.
  let drawn = null;
  function redraw(signature, draw) {
    if (signature === drawn) return;
    drawn = signature;
    draw();
  }

  function branchesChanged() {
    renderLanes();
    if (state.route.name === 'home' && state.source.kind === 'repo') redraw(homeSignature(), renderHome);
    if (state.route.name === 'attention') redraw(attentionSignature(), renderAttention);
    if (state.route.name === 'branch') updateBranch();
  }

  // ---------------------------------------------------------------
  // The sidebar: a repository's branches, or a project's workspaces, by lane

  function renderLanes() {
    const list = $('branch-list');
    const needs = lanes(Array.from(state.branches.values()))[0].branches.length;
    document.title = needs ? `(${needs}) Branchyard` : 'Branchyard';
    $('attention-count').hidden = !needs;
    $('attention-count').textContent = String(needs);
    const focused = list.contains(document.activeElement) ? document.activeElement.dataset.key : null;
    const needle = state.filter.trim().toLowerCase();
    if (state.source.kind === 'none') { $('filter').hidden = true; fill(list); return; }
    fill(list, ...(state.source.kind === 'project' ? workspaceLanes(needle) : branchLanes(needle)));
    if (focused) {
      const again = Array.from(list.querySelectorAll('a.branch-row')).find((a) => a.dataset.key === focused);
      if (again) again.focus({ preventScroll: true });
    }
  }

  // A lane of rows: a heading and its count, or, when collapsed, a
  // disclosure that a search opens.
  function laneGroup(lane, rows, needle, prefix) {
    if (!rows.length) return null;
    const id = `${prefix}${lane.key}`;
    const items = h('ul', { class: 'lane-list' }, rows);
    if (!lane.collapsed) {
      return h('section', { class: 'lane', id, 'aria-labelledby': `${id}-h` },
        h('h2', { class: 'lane-head', id: `${id}-h` }, h('span', { text: lane.label }), h('span', { class: 'lane-count', text: String(rows.length) })),
        items);
    }
    const open = state.doneOpen || !!needle;
    const summary = h('summary', { class: 'lane-head' }, h('span', { text: `${open ? 'Hide' : 'Show'} ${rows.length} ${lane.label.toLowerCase()}` }));
    const box = h('details', { class: 'lane', id, open }, summary, items);
    box.addEventListener('toggle', () => {
      if (!needle) state.doneOpen = box.open;
      summary.firstChild.textContent = `${box.open ? 'Hide' : 'Show'} ${rows.length} ${lane.label.toLowerCase()}`;
    });
    return box;
  }

  function noMatches(hint) {
    return h('section', { class: 'list-state' },
      h('h2', { text: `No matching ${state.source.kind === 'project' ? 'workspaces' : 'branches'}` }),
      h('p', { class: 'muted', text: hint }),
      h('button', { type: 'button', text: 'Clear search', onclick: () => {
        const search = $('branch-filter');
        state.filter = '';
        search.value = '';
        renderLanes();
        search.focus();
      } }));
  }

  function branchLanes(needle) {
    const all = Array.from(state.branches.values());
    const matches = (b) => !needle || b.name.toLowerCase().includes(needle) || (b.prompt || '').toLowerCase().includes(needle);
    const shown = all.filter(matches);
    const waiting = attention();
    $('filter').hidden = !all.length;
    $('branch-count').textContent = needle
      ? `Showing ${shown.length} of ${all.length} branches`
      : plural(all.length, 'branch', 'branches');
    const current = state.route.name === 'branch' ? state.route.branch : null;
    const row = (b, lane) => h('li', {}, h('a', {
      class: 'branch-row', href: branchHref(b.name), 'data-branch': b.name, 'data-key': `b:${b.name}`, title: b.name,
      'aria-current': b.name === current ? 'page' : null,
    }, dot(lane.key), h('span', { class: 'row-name', text: b.name }),
    h('span', { class: 'row-meta' }, lane.meta(b, waiting), lane.key === 'needs' || lane.key === 'running' ? [' · ', ago(activeAt(b), '')] : null)));
    const status = [];
    if (state.branchError) {
      status.push(h('section', { class: 'list-state error-state', role: 'alert' },
        h('h2', { text: state.loaded ? 'Could not refresh branches' : 'Could not load branches' }),
        h('p', { text: state.branchError }),
        h('p', { class: 'hint', text: 'Check that the server is reachable and this token can read the repository, then retry. Your token stays in this tab.' }),
        h('button', { type: 'button', text: 'Retry loading branches', onclick: async () => {
          try { await loadBranches(); } catch (e) {
            if (e.status !== 401) toast(`Could not load branches: ${e.message}`);
          }
        } })));
    } else if (!all.length) {
      status.push(h('section', { class: 'list-state', role: state.loaded ? null : 'status' },
        h('h2', { text: state.loaded ? 'No branches yet.' : 'Loading branches…' }),
        h('p', { class: 'muted', text: state.loaded
          ? can('run') ? 'Start a task to create its own branch. Its progress and results will appear here.'
            : 'Branches started from the CLI or another browser will appear here. This token has read access.'
          : 'Getting the latest work from your server.' })));
    }
    if (all.length && !shown.length) status.push(noMatches('Try another name or a word from the prompt.'));
    return [...status, ...lanes(shown).map(({ lane, branches }) => laneGroup(lane, branches.map((b) => row(b, lane)), needle, 'lane-'))];
  }

  function workspaceLanes(needle) {
    const { list: all, loaded, error } = state.workspaces;
    const shown = all.filter((w) => !needle || w.title.toLowerCase().includes(needle));
    $('filter').hidden = !all.length;
    $('branch-count').textContent = needle
      ? `Showing ${shown.length} of ${all.length} workspaces`
      : plural(all.length, 'workspace');
    const current = state.route.name === 'workspace' ? state.route.workspace : null;
    const row = (w, lane) => h('li', {}, h('a', {
      class: 'branch-row', href: `#/w/${enc(w.id)}`, 'data-workspace': w.id, 'data-key': `w:${w.id}`, title: w.title,
      'aria-current': w.id === current ? 'page' : null,
    }, dot(lane.key), h('span', { class: 'row-name', text: w.title }), h('span', { class: 'row-meta', text: workspaceStateText(w) })));
    const status = [];
    if (error) {
      status.push(h('section', { class: 'list-state error-state', role: 'alert' },
        h('h2', { text: loaded ? 'Could not refresh workspaces' : 'Could not load workspaces' }),
        h('p', { text: error }),
        h('button', { type: 'button', text: 'Retry loading workspaces', onclick: () => loadWorkspaces(state.source.id) })));
    } else if (!all.length) {
      status.push(h('section', { class: 'list-state', role: loaded ? null : 'status' },
        h('h2', { text: loaded ? 'No workspaces yet.' : 'Loading workspaces…' }),
        h('p', { class: 'muted', text: loaded
          ? can('run') ? 'A workspace holds sources, sessions and their outputs. Start one with New workspace.' : 'Workspaces you are a member of appear here.'
          : 'Getting this project\'s workspaces.' })));
    }
    if (all.length && !shown.length) status.push(noMatches('Try another word from a workspace title.'));
    const sorted = shown.slice().sort((x, y) => y.created_at_ms - x.created_at_ms);
    return [...status, ...WORKSPACE_LANES.map((lane) => laneGroup(lane, sorted.filter((w) => workspaceLane(w) === lane).map((w) => row(w, lane)), needle, 'ws-lane-'))];
  }

  // `j` and `k` walk the visible branches; Enter follows the focused link.
  function stepBranch(by) {
    const rows = Array.from(document.querySelectorAll('#branch-list a.branch-row')).filter((a) => a.offsetParent);
    if (!rows.length) return;
    let at = rows.indexOf(document.activeElement);
    if (at < 0) at = rows.findIndex((a) => a.getAttribute('aria-current') === 'page');
    const next = at < 0 ? (by > 0 ? 0 : rows.length - 1) : Math.min(rows.length - 1, Math.max(0, at + by));
    rows[next].focus();
  }

  // ---------------------------------------------------------------
  // Home: the repository at a glance

  function homeSignature() {
    const groups = lanes(Array.from(state.branches.values()));
    return JSON.stringify([state.loaded, state.branchError, groups.map((g) => g.branches.map((b) => [b.name, b.status, b.stalled])), Array.from(attention())]);
  }

  function renderHome() {
    if (state.source.kind === 'project') return renderProject();
    if (state.source.kind === 'none') return renderNoSource();
    const all = Array.from(state.branches.values());
    const groups = lanes(all);
    const needs = groups[0].branches;
    const waiting = attention();
    const lede = !state.loaded
      ? state.branchError ? 'The branch list did not load.' : 'Loading…'
      : !all.length ? 'Nothing has run here yet.'
        : needs.length ? `${plural(needs.length, 'branch', 'branches')} ${needs.length === 1 ? 'needs' : 'need'} you.` : 'Nothing needs you right now.';
    const card = (b) => {
      const s = statusOf(b);
      const why = waiting.get(b.name);
      return h('li', {}, h('a', { class: 'card card-link', href: why === 'approval' || why === 'question' || why === 'escalation' ? '#/attention' : branchHref(b.name) },
        h('span', { class: 'card-head' }, dot('needs'), h('span', { class: 'card-name', text: b.name }), ago(activeAt(b))),
        h('span', { class: 'card-text', text: why ? `${cap(why)} waiting for you` : cap(s.text) }),
        b.prompt ? h('span', { class: 'card-prompt', text: b.prompt.length > 160 ? `${b.prompt.slice(0, 159)}…` : b.prompt }) : null));
    };
    main(h('div', { class: 'page' },
      h('header', { class: 'page-head' },
        h('div', {}, h('h1', { text: state.repo }), h('p', { class: 'lede', text: lede }))),
      state.loaded && all.length ? h('ul', { class: 'glance', 'aria-label': 'Branches by lane' }, groups.map(({ lane, branches }) =>
        h('li', { class: 'glance-item' }, h('span', { class: 'glance-label' }, dot(branches.length ? lane.key : 'done'), lane.label), h('b', { text: String(branches.length) })))) : null,
      needs.length ? [h('h2', { text: 'Needs you' }), h('ul', { class: 'cards' }, needs.map(card))] : null,
      state.loaded && !all.length && can('run') ? h('div', { class: 'welcome' },
        h('p', { text: 'Start a task and it gets a branch of its own, with its conversation, its changes and a merge button.' }),
        h('a', { class: 'button primary', href: '#/new', text: 'Start a task' })) : null));
  }

  // A token that reaches no repository, before its first project.
  function renderNoSource() {
    main(h('div', { class: 'page' },
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'Projects' }),
        h('p', { class: 'lede', text: 'This token reaches no repository. A knowledge project needs none: it holds the sources you upload, the sessions that work on them, and the outputs you accept.' }))),
      can('run') ? h('div', { class: 'welcome' },
        h('p', { text: 'Start a project, then a workspace in it, and give its sessions your documents.' }),
        h('a', { class: 'button primary', href: '#/p/new', text: 'New project' }))
        : h('p', { class: 'muted', text: 'This token has read access only; projects started elsewhere appear in the picker.' })));
  }

  // ---------------------------------------------------------------
  // Projects: a new one, and one at a glance with its workspaces

  const RESULT_MODES = [
    ['artifacts', 'Outputs to accept'],
    ['mixed', 'Code and outputs'],
    ['code', 'Code changes'],
  ];
  const resultText = (mode) => (RESULT_MODES.find(([m]) => m === mode) || [mode, mode])[1].toLowerCase();
  const sourceText = (p) => (p.source.kind === 'knowledge' ? 'Knowledge project' : `Repository project on ${p.source.repo}`);

  function renderNewProject() {
    const nameId = nextId('project');
    const sourceId = nextId('source');
    const modeId = nextId('mode');
    const name = h('input', { id: nameId, type: 'text', autocomplete: 'off', required: true });
    const source = h('select', { id: sourceId },
      h('option', { value: 'knowledge', text: 'Knowledge work (no repository)' }),
      state.repos.map((r) => h('option', { value: `repo:${r}`, text: `Repository ${r}` })));
    const mode = h('select', { id: modeId }, RESULT_MODES.map(([value, text]) => h('option', { value, text })));
    const error = h('p', { class: 'error', role: 'alert' });
    const back = state.source.kind === 'project' ? `#/p/${enc(state.source.id)}` : '#/';
    const go = h('button', {
      type: 'button', class: 'primary', text: 'Create project', disabled: !can('run'),
      onclick: async () => {
        const title = name.value.trim();
        if (!title) { name.focus(); return; }
        const body = {
          name: title,
          source: source.value === 'knowledge' ? { kind: 'knowledge' } : { kind: 'repository', repo: source.value.slice(5) },
          result_mode: mode.value,
        };
        go.disabled = true;
        error.textContent = '';
        let project;
        try {
          project = await api('POST', '/v1/projects', body);
        } catch (e) {
          error.textContent = e.code === 'knowledge_disabled' ? `${e.message}: this server keeps no knowledge projects until its operator turns them on.` : e.message;
          go.disabled = false;
          return;
        }
        state.projects = (state.projects || []).filter((p) => p.id !== project.id).concat(project);
        toast(`Project ${project.name} created.`);
        location.hash = `#/p/${enc(project.id)}`;
      },
    });
    name.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') go.click(); });
    main(h('div', { class: 'page narrow' },
      h('a', { class: 'back', href: back, text: '‹ Back' }),
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'New project' }),
        h('p', { class: 'lede', text: 'A project names where work comes from and what counts as its result. Its workspaces hold the sessions and their outputs.' }))),
      h('div', { class: 'card form' },
        h('label', { class: 'block', for: nameId, text: 'Project name' }), name,
        h('div', { class: 'form-grid' },
          h('div', {}, h('label', { class: 'block', for: sourceId, text: 'Source' }), source),
          h('div', {}, h('label', { class: 'block', for: modeId, text: 'Results' }), mode)),
        h('p', { class: 'hint', text: 'Knowledge work keeps its files on the server; a repository project works on branches of a repository this token reaches.' }),
        h('div', { class: 'row form-actions' }, go),
        can('run') ? null : h('p', { class: 'hint', text: 'This token lacks the run scope.' })),
      error));
  }

  // A project at a glance, as home is a repository's: counts per lane, a
  // field to start a workspace, and its workspaces.
  async function renderProject() {
    const route = state.route;
    const id = route.name === 'project' ? route.project : state.source.id;
    let project = projectOf(id);
    if (!project) {
      let error = null;
      try {
        project = await api('GET', `/v1/projects/${enc(id)}`);
        state.projects = (state.projects || []).filter((p) => p.id !== id).concat(project);
        renderPicker();
      } catch (e) {
        error = e;
      }
      if (state.route !== route) return;
      if (error) {
        main(h('div', { class: 'page' }, h('a', { class: 'back', href: '#/', text: '‹ Back' }),
          h('h1', { text: 'Project not found' }), h('p', { class: 'error', text: error.message })));
        return;
      }
    }
    const titleField = h('input', { id: 'workspace-title', type: 'text', autocomplete: 'off', placeholder: 'What is this workspace for?' });
    const status = h('p', { class: 'hint', role: 'status' });
    const create = h('button', {
      type: 'button', class: 'primary', text: 'Create workspace', disabled: !can('run'),
      onclick: () => createWorkspace(project, titleField, create, status),
    });
    titleField.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') create.click(); });
    main(h('div', { class: 'page' },
      h('a', { class: 'back', href: `#/p/${enc(project.id)}`, text: '‹ Workspaces' }),
      h('header', { class: 'page-head' }, h('div', {},
        h('h1', { text: project.name }),
        h('p', { class: 'lede', id: 'project-lede' }))),
      h('ul', { class: 'glance', id: 'project-glance', 'aria-label': 'Workspaces by state' }),
      can('run') ? h('section', { class: 'card new-workspace', 'aria-label': 'New workspace' },
        h('label', { class: 'block', for: 'workspace-title', text: 'Workspace title' }),
        h('div', { class: 'row inline-form' }, titleField, create),
        status) : null,
      h('h2', { text: 'Workspaces' }),
      h('div', { id: 'project-cards' })));
    updateProjectGlance();
  }

  // What the glance shows from the workspace list; a poll redraws only this,
  // so a title being typed stays.
  function updateProjectGlance() {
    const lede = $('project-lede');
    if (!lede) return;
    const project = projectOf(state.workspaces.project);
    const { list, loaded, error } = state.workspaces;
    if (!project) return;
    lede.textContent = `${sourceText(project)} · ${resultText(project.result_mode)} · ${loaded ? plural(list.length, 'workspace') : 'loading workspaces…'}`;
    fill($('project-glance'), loaded && list.length ? WORKSPACE_LANES.map((lane) => {
      const n = list.filter((w) => workspaceLane(w) === lane).length;
      return h('li', { class: 'glance-item' }, h('span', { class: 'glance-label' }, dot(n ? lane.key : 'done'), lane.label), h('b', { text: String(n) }));
    }) : null);
    const sorted = list.slice().sort((x, y) => y.created_at_ms - x.created_at_ms);
    fill($('project-cards'), error ? h('p', { class: 'error', text: error })
      : !loaded ? h('p', { class: 'muted', text: 'Loading…' })
        : !list.length ? h('p', { class: 'muted', text: 'No workspaces yet.' })
          : h('ul', { class: 'cards' }, sorted.map((w) => {
            const lane = workspaceLane(w);
            return h('li', {}, h('a', { class: 'card card-link', href: `#/w/${enc(w.id)}` },
              h('span', { class: 'card-head' }, dot(lane.key), h('span', { class: 'card-name', text: w.title }), ago(w.created_at_ms)),
              h('span', { class: 'card-text', text: cap(workspaceStateText(w)) }),
              w.error ? h('span', { class: 'card-prompt', text: w.error.message }) : null));
          })));
  }

  // A workspace is reserved at once and set up by a queued operation; it
  // opens when that operation has succeeded.
  async function createWorkspace(project, field, button, status) {
    const title = field.value.trim();
    if (!title) { field.focus(); return; }
    button.disabled = true;
    button.textContent = 'Creating…';
    status.textContent = 'Setting up the workspace…';
    const reset = (text) => {
      button.disabled = false;
      button.textContent = 'Create workspace';
      status.textContent = text;
    };
    let op;
    try {
      op = await api('POST', '/v1/workspaces', { project_id: project.id, title });
    } catch (e) {
      reset(`Could not create the workspace: ${e.message}`);
      return;
    }
    const done = await awaitOperation(op.id);
    if (!done || done.state !== 'succeeded') {
      reset(done ? `The workspace did not start: ${done.error ? done.error.message : done.state}.` : 'The workspace is still being set up; it appears in the list when it is ready.');
      if (state.workspaces.project === project.id) loadWorkspaces(project.id);
      return;
    }
    const id = done.workspace_id || op.workspace_id;
    field.value = '';
    toast(`Workspace ${title} is ready.`);
    if (state.workspaces.project === project.id) loadWorkspaces(project.id);
    if (status.isConnected) location.hash = `#/w/${enc(id)}`;
  }

  // ---------------------------------------------------------------
  // One branch: the status strip, the conversation, the message box, and
  // the inspector

  function describe(activity) {
    const [kind] = Object.keys(activity);
    const v = activity[kind];
    switch (kind) {
      case 'harness':
        switch (v.type) {
          case 'tool_started': return `tool ${v.name}`;
          case 'message_delta': return v.text;
          case 'permission_requested': return `asks to use ${v.request.tool}`;
          case 'turn_ended': return `turn ${v.turn} ended`;
          case 'turn_accepted': return `turn ${v.turn} started`;
          case 'warning': return `warning: ${v.message}`;
          case 'steer_accepted': return 'steering delivered';
          case 'steer_rejected': return `steering refused: ${v.reason}`;
          default: return v.type.replace(/_/g, ' ');
        }
      case 'prompt': return `prompt: ${v}`;
      case 'decision': return `${v.allowed ? 'allowed' : 'denied'} ${v.tool}${v.message ? `: ${v.message}` : ''}`;
      case 'snapshot': return `candidate ${v.commit.slice(0, 10)} (${v.files_changed} files +${v.insertions} −${v.deletions})`;
      case 'status': return `status: ${statusOf({ status: v }).text}`;
      case 'warning': return `warning: ${v}`;
      case 'checkpoint': return `checkpoint ${v.turn} at ${v.commit.slice(0, 10)}`;
      case 'steered': return `steered by ${v.by}: ${v.text}`;
      case 'stalled': return 'stalled: no harness activity';
      case 'resumed': return 'active again';
      case 'message': return `${v.kind} from ${v.from} to ${v.to}: ${v.text}`;
      case 'recovered': return `recovered: ${v.reason}`;
      default: return kind.replace(/_/g, ' ');
    }
  }

  function eventItem(e) {
    return h('li', {}, h('time', { datetime: new Date(e.at_ms).toISOString(), text: clock(e.at_ms) }), describe(e.activity));
  }

  // Message deltas arrive a few words at a time: join each run of them.
  function coalesce(events) {
    const out = [];
    for (const e of events) {
      const prev = out[out.length - 1];
      const delta = e.activity.harness && e.activity.harness.type === 'message_delta';
      const prevDelta = prev && prev.activity.harness && prev.activity.harness.type === 'message_delta';
      if (delta && prevDelta) {
        prev.activity = { harness: { type: 'message_delta', text: prev.activity.harness.text + e.activity.harness.text } };
      } else {
        out.push({ at_ms: e.at_ms, activity: e.activity });
      }
    }
    return out.map((e) => {
      if (e.activity.harness && e.activity.harness.type === 'message_delta') {
        const text = e.activity.harness.text.trim();
        return { at_ms: e.at_ms, activity: { harness: { type: 'message_delta', text: `said: ${text.length > 400 ? `${text.slice(0, 399)}…` : text}` } } };
      }
      return e;
    });
  }

  // The event page as a conversation. A prompt or a steer opens a turn,
  // which gathers what the agent said (one paragraph per run of deltas
  // between tool calls), the tools it called, and how it ended. Events
  // before the window's first prompt open a turn with no prompt; statuses
  // and session bookkeeping belong to Activity alone.
  function conversation(events) {
    const turns = [];
    let turn = null;
    let cut = true;
    const open = (text, steer) => {
      turn = { text, steer, said: [], tools: [], status: null, checkpoint: null, candidate: null };
      turns.push(turn);
      cut = true;
      return turn;
    };
    for (const { activity: a } of events) {
      const ev = a.harness || {};
      if (a.prompt != null) open(a.prompt, false);
      else if (a.steered) open(a.steered.text, true);
      else if (ev.type === 'message_delta') {
        const t = turn || open(null, false);
        if (cut) t.said.push(ev.text); else t.said[t.said.length - 1] += ev.text;
        cut = false;
      } else if (ev.type === 'tool_started') {
        (turn || open(null, false)).tools.push(ev.name);
        cut = true;
      } else if (a.snapshot) (turn || open(null, false)).candidate = a.snapshot;
      else if (a.checkpoint) (turn || open(null, false)).checkpoint = a.checkpoint;
      else if (a.status && turn && !['running', 'waiting'].includes(a.status.state)) turn.status = a.status;
    }
    for (const t of turns) t.said = t.said.map((s) => s.trim()).filter(Boolean);
    return turns;
  }

  // A unified diff split at each file: its path, its counts, and its lines
  // marked as they render. The file list already names the path, so git's
  // own header lines are dropped; a mode, rename or binary note stays.
  const GIT_HEADER = /^(diff --git |index |--- |\+\+\+ )/;
  function parseDiff(text) {
    const files = [];
    let file = null;
    let inHunk = false;
    for (const line of text.split('\n')) {
      if (line.startsWith('diff --git ')) {
        const m = /^diff --git a\/(.*) b\/(.*)$/.exec(line);
        file = { path: m ? m[2] : line.slice(11), add: 0, del: 0, lines: [] };
        files.push(file);
        inHunk = false;
      }
      if (!file) continue;
      let kind = null;
      if (line.startsWith('@@')) { kind = 'hunk'; inHunk = true; }
      else if (!inHunk) {
        kind = 'file';
        if (line.startsWith('+++ b/')) file.path = line.slice(6);
        if (GIT_HEADER.test(line)) continue;
      } else if (line.startsWith('+')) { kind = 'add'; file.add += 1; }
      else if (line.startsWith('-')) { kind = 'del'; file.del += 1; }
      file.lines.push({ kind, text: line });
    }
    return files;
  }

  function policy(allowAll) {
    return allowAll ? { mode: 'allow' } : undefined;
  }

  // What a branch's buttons do. `started` is said once the server took the
  // request; `finished`, when its operation succeeded.
  const ACTIONS = {
    merge: {
      label: 'Merge', busy: 'Merging…', scope: 'merge', rule: when.merge,
      confirm: (b) => [`Merge ${b}?`, `Merge ${b} into the checked-out branch? Its check runs on the merge result first, and nothing changes unless it passes.`],
      run: (b) => api('POST', `${branchPath(b)}/merge`, {}),
      started: 'Merging. Its check runs on the merge result first.',
      finished: (info) => (info && info.status.state === 'merged' ? `Merged into ${info.status.target}.` : 'Merged.'),
    },
    resume: {
      label: 'Resume', busy: 'Resuming…', scope: 'run', rule: when.resume,
      run: (b) => api('POST', `${branchPath(b)}/send`, { prompt: RESUME_PROMPT }),
      started: 'Resuming. A new turn is starting.',
    },
    cancel: {
      label: 'Cancel turn', busy: 'Cancelling…', scope: 'run', rule: when.cancel, danger: true,
      confirm: (b) => ['Cancel turn?', `Cancel ${b}'s running turn, and those of every branch it delegated to?`],
      run: (b) => api('POST', `${branchPath(b)}/cancel`, {}),
      started: 'Cancelling the running turn.',
      finished: () => 'Turn cancelled.',
    },
  };

  // The strip's one main action for a state; a state without one has none.
  const PRIMARY = {
    ready: ACTIONS.merge,
    interrupted: ACTIONS.resume,
  };

  // Rare actions, in the ⋯ menu. Fork puts the message box in fork mode.
  const MENU = [
    ACTIONS.cancel,
    { label: 'Fork…', scope: 'run', rule: when.fork, pick: () => { view.forking = true; updateBranch(); $('composer-text').focus(); } },
  ];

  // The message box's verb, by the same rules: a running branch steers,
  // any other sends a follow-up (refused, with the reason in the box, while
  // it waits), and Fork from the menu holds until sent or cancelled.
  const COMPOSER = {
    send: {
      label: 'Send', title: 'Send a follow-up prompt', scope: 'run', rule: when.send, allowAll: true,
      placeholder: (b) => `Follow up on ${b}…`,
      run: (b, text, all) => api('POST', `${branchPath(b)}/send`, { prompt: text, policy: policy(all) }),
      started: 'Sent. A new turn is starting.',
    },
    steer: {
      label: 'Steer', title: 'Steer the running turn', scope: 'run', rule: when.steer, allowAll: false,
      placeholder: (b) => `Steer ${b}'s running turn…`,
      run: (b, text) => api('POST', `${branchPath(b)}/steer`, { text }),
      started: 'Steering sent. The running turn reads it next.',
    },
    fork: {
      label: 'Fork', title: 'Fork with a prompt', scope: 'run', rule: when.fork, allowAll: true, opens: true,
      placeholder: (b) => `What should a fork of ${b} do?`,
      run: (b, text, all) => api('POST', `${branchPath(b)}/fork`, { prompt: text, policy: policy(all) }),
      started: 'Forked. The new branch is starting.',
    },
  };

  const lacks = (action) => (can(action.scope) ? null : `this token lacks the ${action.scope} scope`);
  const applies = (action, info) => !lacks(action) && !action.rule(info);

  function composerMode(info, forking) {
    const mode = forking ? COMPOSER.fork : when.steer(info) ? COMPOSER.send : COMPOSER.steer;
    return { mode, why: lacks(mode) || mode.rule(info) };
  }

  // Run one of a branch's actions and say what became of it, in words.
  async function act(action, name, ...args) {
    let result;
    try {
      result = await action.run(name, ...args);
    } catch (e) {
      toast(`Could not ${action.label.toLowerCase()}: ${e.message}`);
      return null;
    }
    const opened = action.opens && result && result.branches && result.branches[0];
    toast(action.started, opened ? branchHref(opened) : null);
    if (result && result.id && result.state) followOperation(result.id, action, name);
    refreshBranch(name);
    if (view && view.name === name) loadOperations(view);
    return result || {};
  }

  async function followOperation(id, action, name) {
    const repo = state.repo;
    const op = await awaitOperation(id);
    if (!op) return;
    let info = null;
    if (name && repo === state.repo) {
      try { info = await api('GET', branchPath(name)); state.branches.set(name, info); } catch (_) { /* the list refresh catches up */ }
    }
    if (state.pending.get(name) === action.busy) state.pending.delete(name);
    if (op.state !== 'succeeded') toast(`${action.label} ${op.state}${op.error ? `: ${op.error.message}` : '.'}`);
    else if (action.finished) {
      if (action === ACTIONS.merge) state.merged.add(name);
      toast(action.finished(info));
    }
    branchesChanged();
  }

  async function runAction(action, name) {
    if (action.confirm) {
      const [title, text] = action.confirm(name);
      if (!(await confirmDialog(title, text, action.label, action.danger))) return;
    }
    state.pending.set(name, action.busy);
    updateBranch();
    const result = await act(action, name);
    if (!result || !result.id) state.pending.delete(name);
    updateBranch();
  }

  // The open branch: what its panes show, kept across live updates.
  let view = null;

  async function renderBranch() {
    const name = state.route.branch;
    let info = state.branches.get(name);
    if (!info) {
      try { info = await api('GET', branchPath(name)); state.branches.set(name, info); } catch (e) {
        main(h('div', { class: 'page' }, h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
          h('h1', { text: name }), h('p', { class: 'error', text: e.message })));
        return;
      }
    }
    const v = {
      name, events: [], first: 0, next: null, chain: null, queued: false,
      forking: false, tab: 'changes', file: null, files: null, diffFor: undefined, truncated: 0,
      open: new Set(), signature: null, operations: null,
    };
    view = v;
    main(h('div', { class: 'branch' },
      h('section', { class: 'focus', 'aria-label': 'Conversation' },
        h('header', { class: 'strip', id: 'strip' }),
        h('div', { class: 'convo', id: 'convo' }, h('p', { class: 'muted', text: 'Loading the conversation…' })),
        composer(() => submitComposer())),
      h('aside', { class: 'inspector', id: 'inspector', 'aria-label': 'Inspector' },
        tablist(BRANCH_TABS, () => v.tab, (k) => selectTab(v, k)),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-changes', 'aria-labelledby': 'tab-changes' }, h('div', { id: 'changes' })),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-details', 'aria-labelledby': 'tab-details', hidden: true },
          h('dl', { class: 'facts', id: 'facts' }),
          h('h2', { text: 'Checkpoints' }), h('div', { id: 'checkpoints' }, h('p', { class: 'muted', text: 'Loading…' })),
          h('h2', { text: 'Operations' }), h('div', { id: 'operations' }, h('p', { class: 'muted', text: 'Loading…' }))),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-activity', 'aria-labelledby': 'tab-activity', hidden: true },
          h('ul', { class: 'events', id: 'events', 'aria-label': 'Recent events' }, h('li', { class: 'muted', text: 'Loading…' }))))));
    updateBranch();
    v.chain = pull(v).then(() => drawTimeline(v), (e) => {
      if (view !== v) return;
      fill($('convo'), h('p', { class: 'error', text: e.message }));
      fill($('events'), h('li', { class: 'error', text: e.message }));
    });
    loadOperations(v);
  }

  const BRANCH_TABS = [['changes', 'Changes', 'changes-count'], ['details', 'Details'], ['activity', 'Activity']];

  // The inspector's tabs: [key, label, id of a count beside it]. Arrow keys
  // move between them.
  function tablist(tabs, current, pick) {
    return h('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Inspector', onkeydown: (ev) => {
      const at = tabs.findIndex(([k]) => k === current());
      const by = { ArrowRight: 1, ArrowLeft: -1 }[ev.key];
      if (!by) return;
      const [k] = tabs[(at + by + tabs.length) % tabs.length];
      pick(k);
      $(`tab-${k}`).focus();
    } }, tabs.map(([k, label, count]) => h('button', {
      type: 'button', role: 'tab', id: `tab-${k}`, 'aria-controls': `panel-${k}`,
      'aria-selected': String(k === current()), tabindex: k === current() ? '0' : '-1',
      onclick: () => pick(k),
    }, label, count ? [' ', h('span', { class: 'tab-count', id: count })] : null)));
  }

  function showTab(tabs, tab) {
    for (const [k] of tabs) {
      const on = k === tab;
      $(`tab-${k}`).setAttribute('aria-selected', String(on));
      $(`tab-${k}`).tabIndex = on ? 0 : -1;
      $(`panel-${k}`).hidden = !on;
    }
  }

  function selectTab(v, tab) {
    v.tab = tab;
    showTab(BRANCH_TABS, tab);
    drawTimeline(v);
  }

  // The latest window of events first, then everything after it, page by
  // page; a burst of stream events for this branch asks for one more pull.
  async function pull(v) {
    for (;;) {
      const query = v.next == null ? '' : `cursor=${v.next}&`;
      const page = await api('GET', `${branchPath(v.name)}/event-page?${query}limit=${EVENT_WINDOW}`);
      if (v.next == null) v.first = page.next_cursor - page.events.length;
      v.events.push(...page.events);
      v.next = page.next_cursor;
      if (v.events.length > EVENT_KEEP) {
        v.first += v.events.length - EVENT_KEEP;
        v.events.splice(0, v.events.length - EVENT_KEEP);
      }
      if (page.events.length < EVENT_WINDOW || v.next >= page.total) return;
    }
  }

  function refreshTimeline(v) {
    if (v.queued || !v.chain) return;
    v.queued = true;
    v.chain = v.chain
      .then(() => new Promise((r) => setTimeout(r, 150)))
      .then(() => { v.queued = false; return pull(v); })
      .then(() => drawTimeline(v))
      .catch(() => { v.queued = false; });
  }

  function drawTimeline(v) {
    if (view !== v || v.next == null) return;
    const box = $('convo');
    const atEnd = box.scrollHeight - box.scrollTop - box.clientHeight < 48;
    const turns = conversation(v.events);
    const info = state.branches.get(v.name);
    const working = info && ['running', 'waiting'].includes(info.status.state);
    fill(box,
      v.first > 0 ? h('p', { class: 'convo-note', text: `Earlier turns are not shown here: this is the latest of ${plural(v.first + v.events.length, 'event')}.` }) : null,
      turns.length ? turns.map((t, i) => turnItem(t, i, v, working && i === turns.length - 1)) : h('p', { class: 'muted', text: 'No turns yet.' }),
      working && !turns.length ? workingLine(info) : null);
    if (atEnd || !v.scrolled) box.scrollTop = box.scrollHeight;
    v.scrolled = true;
    if (v.tab === 'activity') {
      const events = coalesce(v.events);
      fill($('events'), ...(events.length ? events.slice().reverse().map(eventItem) : [h('li', { class: 'muted', text: 'No events yet.' })]));
    }
    if (v.tab === 'details') {
      const cps = v.events.filter((e) => e.activity.checkpoint).map((e) => e.activity.checkpoint);
      fill($('checkpoints'), cps.length
        ? h('ul', { class: 'events' }, cps.reverse().map((cp) => h('li', {}, `turn ${cp.turn} · `, h('span', { class: 'mono', text: cp.commit.slice(0, 12) }), ` · ${plural(cp.files_changed, 'file')} `, counts(cp.insertions, cp.deletions))))
        : h('p', { class: 'muted', text: v.first > 0 ? 'None in the latest events.' : 'None yet: each finished turn leaves one.' }));
    }
  }

  function workingLine(info) {
    return h('p', { class: 'outcome' }, dot('running'), info.status.state === 'waiting' ? 'Waiting for its prerequisites…' : 'Working…');
  }

  function turnItem(t, i, v, last) {
    let tools = null;
    if (t.tools.length) {
      tools = h('details', { class: 'tools', open: v.open.has(i) },
        h('summary', { text: plural(t.tools.length, 'tool call') }),
        h('ol', {}, t.tools.map((name) => h('li', { class: 'mono', text: name }))));
      tools.addEventListener('toggle', () => { if (tools.open) v.open.add(i); else v.open.delete(i); });
    }
    const c = t.checkpoint || t.candidate;
    const lane = t.status ? laneOf({ name: '', status: t.status }, new Map()).key : null;
    const outcome = t.status || c ? h('p', { class: `outcome outcome-${lane || 'done'}` },
      lane === 'ready' ? h('span', { class: 'tick', 'aria-hidden': 'true', text: '✓' }) : lane ? dot(lane) : null,
      t.status ? cap(statusOf({ status: t.status }).text) : null,
      c ? [t.status ? ' · ' : '', h('span', { class: 'mono', text: c.commit.slice(0, 7) }), ` · ${plural(c.files_changed, 'file')} `, counts(c.insertions, c.deletions)] : null) : null;
    const info = state.branches.get(v.name);
    return h('article', { class: 'turn' },
      t.text != null ? h('div', { class: t.steer ? 'you steer' : 'you' },
        t.steer ? h('span', { class: 'you-label', text: 'Steered' }) : null, h('p', { text: t.text })) : null,
      t.said.length ? h('div', { class: 'agent' }, t.said.map((s) => h('p', { text: s }))) : null,
      tools,
      outcome,
      last && !outcome && info ? workingLine(info) : null);
  }

  function composer(submit) {
    const area = h('textarea', { id: 'composer-text', rows: '2' });
    area.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter' && (ev.metaKey || ev.ctrlKey)) { ev.preventDefault(); submit(); }
    });
    return h('div', { class: 'composer', id: 'composer' },
      h('label', { class: 'visually-hidden', for: 'composer-text', id: 'composer-label' }),
      area,
      h('div', { class: 'composer-tools' },
        h('label', { class: 'toggle', id: 'composer-allow-box', title: 'Like --yes. Otherwise every tool request is denied.' },
          h('input', { type: 'checkbox', id: 'composer-allow' }), h('span', { text: 'Allow every tool' })),
        h('button', { type: 'button', class: 'ghost', id: 'composer-cancel', text: 'Cancel fork', onclick: () => {
          view.forking = false;
          updateBranch();
          $('composer-text').focus();
        } }),
        h('span', { class: 'spacer' }),
        h('button', { type: 'button', class: 'primary', id: 'composer-go', onclick: () => submit() })));
  }

  function applyComposer(info) {
    const { mode, why } = composerMode(info, view.forking);
    const area = $('composer-text');
    $('composer').dataset.mode = Object.keys(COMPOSER).find((k) => COMPOSER[k] === mode);
    $('composer-label').textContent = mode.title;
    area.placeholder = why ? sentence(why, info.name) : mode.placeholder(info.name);
    area.disabled = !!why;
    $('composer-go').disabled = !!why;
    $('composer-go').textContent = mode.label;
    $('composer-allow-box').hidden = !mode.allowAll;
    $('composer-cancel').hidden = mode !== COMPOSER.fork;
  }

  async function submitComposer() {
    const v = view;
    const info = state.branches.get(v.name);
    if (!info) return;
    const { mode, why } = composerMode(info, v.forking);
    const area = $('composer-text');
    const allow = $('composer-allow');
    const go = $('composer-go');
    if (why || go.disabled) return;
    const text = area.value.trim();
    if (!text) { area.focus(); return; }
    go.disabled = true;
    const result = await act(mode, v.name, text, allow.checked);
    if (view !== v) return;
    if (result) {
      area.value = '';
      allow.checked = false;
      if (mode === COMPOSER.fork) v.forking = false;
    }
    applyComposer(state.branches.get(v.name) || info);
  }

  function strip(info) {
    const s = statusOf(info);
    const lane = laneOf(info, attention()).key;
    const primary = PRIMARY[info.status.state];
    const busy = state.pending.get(info.name);
    const menu = MENU.filter((a) => applies(a, info));
    const meta = [info.harness, plural(info.turns, 'turn'), info.cost_usd != null ? money(info.cost_usd) : null].filter(Boolean).join(' · ');
    let action = null;
    if (busy) action = h('button', { type: 'button', class: 'primary', disabled: true, text: busy });
    else if (primary && applies(primary, info)) {
      action = h('button', { type: 'button', class: 'primary', text: primary.label, onclick: () => runAction(primary, info.name) });
    }
    return [
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      h('div', { class: 'strip-title' }, h('h1', { tabindex: '-1', text: info.name }), h('p', { class: 'strip-meta', text: meta })),
      s.key === 'merged'
        ? h('p', { class: 'state state-merged' }, checkMark(state.merged.has(info.name)), cap(s.text))
        : h('p', { class: `state state-${lane}` }, dot(lane), cap(s.text)),
      action,
      menu.length ? menuOf(menu, info) : null,
    ];
  }

  function menuOf(items, info) {
    const menu = h('details', { class: 'menu' },
      h('summary', { 'aria-label': 'More actions', title: 'More actions' }, h('span', { 'aria-hidden': 'true', text: '⋯' })),
      h('div', { class: 'menu-list' }, items.map((a) => h('button', {
        type: 'button', class: a.danger ? 'menu-item danger-text' : 'menu-item', text: a.label,
        onclick: () => { menu.open = false; if (a.pick) a.pick(); else runAction(a, info.name); },
      }))));
    return menu;
  }

  function facts(info) {
    const c = info.candidate;
    const lane = laneOf(info, attention()).key;
    const link = (name) => h('a', { href: branchHref(name), text: name });
    const rows = [
      ['Status', h('span', { class: 'fact-state' }, dot(lane), cap(statusOf(info).text))],
      ['Harness', `${info.harness}${info.profile && info.profile !== info.harness ? ` (${info.profile})` : ''}`],
      info.model ? ['Model', info.model] : null,
      ['Turns', String(info.turns)],
      ['Cost', money(info.cost_usd)],
      ['Started', new Date(info.created_at * 1000).toLocaleString()],
      ['Candidate', c ? h('span', {}, h('span', { class: 'mono', text: c.commit.slice(0, 12) }), ` · ${plural(c.files_changed, 'file')} `, counts(c.insertions, c.deletions)) : 'none yet'],
      info.parent ? ['From', link(info.parent)] : null,
      info.children && info.children.length ? ['Delegated to', info.children.map((ch, i) => [i ? ', ' : '', link(ch)])] : null,
      info.superseded_by ? ['Superseded by', link(info.superseded_by)] : null,
      ['Prompt', h('span', { class: 'prompt', text: info.prompt || '' })],
    ].filter(Boolean);
    return rows.map(([k, v]) => [h('dt', { text: k }), h('dd', {}, v)]);
  }

  function updateBranch() {
    const v = view;
    const info = v && state.branches.get(v.name);
    if (!info || !$('strip')) return;
    const signature = JSON.stringify([info.status, info.stalled, info.turns, info.cost_usd, info.candidate, state.pending.get(info.name) || '',
      state.merged.has(info.name), laneOf(info, attention()).key]);
    if (signature !== v.signature) {
      const first = v.signature == null;
      v.signature = signature;
      const kept = document.activeElement === $('strip').querySelector('h1');
      fill($('strip'), ...strip(info));
      if (kept) $('strip').querySelector('h1').focus({ preventScroll: true });
      fill($('facts'), ...facts(info).flat());
      if (!first) drawTimeline(v);
    }
    applyComposer(info);
    $('changes-count').textContent = info.candidate ? String(info.candidate.files_changed) : '';
    const commit = info.candidate ? info.candidate.commit : null;
    if (commit !== v.diffFor) loadDiff(v, info);
  }

  async function loadDiff(v, info) {
    v.diffFor = info.candidate ? info.candidate.commit : null;
    const slot = $('changes');
    const why = when.diff(info);
    if (why) { fill(slot, h('p', { class: 'muted pad', text: sentence(why, v.name) })); return; }
    if (!v.files) fill(slot, h('p', { class: 'muted pad', text: 'Loading the changes…' }));
    let text;
    try {
      text = (await api('GET', `${branchPath(v.name)}/diff`)).diff || '';
    } catch (e) {
      if (view === v) fill(slot, h('p', { class: 'error pad', text: e.message }));
      return;
    }
    if (view !== v || v.diffFor !== info.candidate.commit) return;
    v.truncated = text.length > DIFF_LIMIT ? text.length : 0;
    v.files = parseDiff(text.slice(0, DIFF_LIMIT));
    if (!v.files.some((f) => f.path === v.file)) v.file = v.files.length ? v.files[0].path : null;
    renderChanges(v, false);
  }

  function renderChanges(v, focus) {
    const slot = $('changes');
    if (!v.files.length) { fill(slot, h('p', { class: 'muted pad', text: 'The diff is empty.' })); return; }
    const file = v.files.find((f) => f.path === v.file);
    const list = h('ul', { class: 'files', id: 'files', 'aria-label': 'Changed files' }, v.files.map((f) => {
      const at = f.path.lastIndexOf('/');
      return h('li', {}, h('button', {
        type: 'button', class: 'file-row', 'aria-pressed': String(f === file), 'data-path': f.path, title: f.path,
        onclick: () => { v.file = f.path; renderChanges(v, true); },
      }, h('span', { class: 'path' }, h('span', { class: 'dir', text: f.path.slice(0, at + 1) }), h('b', { text: f.path.slice(at + 1) })),
      h('span', { class: 'file-counts' }, counts(f.add, f.del))));
    }));
    const pre = h('pre', { class: 'diff', tabindex: '0', 'aria-label': `Diff of ${file.path}` },
      file.lines.map((l) => h('span', { class: l.kind }, l.text)));
    fill(slot,
      v.truncated ? h('p', { class: 'hint pad', text: `Showing the first ${DIFF_LIMIT} characters of ${v.truncated}.` }) : null,
      list, pre);
    if (focus) list.querySelector('[aria-pressed="true"]').focus();
  }

  async function loadOperations(v) {
    let operations;
    try {
      ({ operations } = await api('GET', `${repoPath()}/operations?branch=${enc(v.name)}`));
    } catch (e) {
      if (view === v) fill($('operations'), h('p', { class: 'error', text: e.message }));
      return;
    }
    if (view !== v) return;
    fill($('operations'), operations.length
      ? h('ul', { class: 'list' }, operations.map(opItem))
      : h('p', { class: 'muted', text: 'Nothing queued or running for this branch.' }));
  }

  function opItem(op) {
    return h('li', { class: 'item' },
      h('div', { class: 'item-head' }, h('span', { class: 'item-name', text: `${op.kind} ${op.branches.join(', ')}` }),
        h('span', { class: 'tag', text: op.state })),
      h('div', { class: 'item-meta' }, h('span', { class: 'mono', text: op.id }), ' · ', ago(op.created_at_ms),
        [op.priority ? `priority ${op.priority}` : null, op.waiting].filter(Boolean).map((t) => ` · ${t}`)));
  }

  // ---------------------------------------------------------------
  // One workspace: the strip with its acceptance and lifecycle actions,
  // its sessions as tabs over one conversation and message box, and an
  // inspector of its sources, outputs and details

  const workspacePath = (id) => `/v1/workspaces/${enc(id)}`;
  const workspaceHref = (id, session) => `#/w/${enc(id)}${session ? `/s/${enc(session)}` : ''}`;

  function size(n) {
    if (n == null) return '';
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / 1024 / 1024).toFixed(1)} MB`;
  }

  // Short names for formats people know; any other shows its file name's
  // extension, and a row's tooltip always gives the media type in full.
  const FORMATS = {
    'text/csv': 'CSV',
    'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet': 'XLSX',
    'application/vnd.openxmlformats-officedocument.wordprocessingml.document': 'DOCX',
    'application/vnd.openxmlformats-officedocument.presentationml.presentation': 'PPTX',
    'application/pdf': 'PDF',
    'application/json': 'JSON',
    'text/plain': 'Text',
    'text/markdown': 'Markdown',
    'image/png': 'PNG',
    'image/jpeg': 'JPEG',
  };
  function formatOf(type, name) {
    const known = FORMATS[(type || '').split(';')[0].trim().toLowerCase()];
    if (known) return known;
    const extension = /\.([a-z0-9]{1,8})$/i.exec(name || '');
    return extension ? extension[1].toUpperCase() : 'File';
  }

  // A source's title travels as an RFC 8187 extended value, so any name
  // arrives as written. encodeURIComponent would leave ' ( ) * bare, which
  // attr-char excludes, so each UTF-8 byte outside attr-char is escaped
  // here. X-Source-Title keeps an ASCII copy for a server that reads only it.
  const ATTR_CHAR = /^[A-Za-z0-9!#$&+\-.^_`|~]$/;
  const encodedTitle = (name) => `UTF-8''${Array.from(new TextEncoder().encode(name), (byte) => {
    const c = String.fromCharCode(byte);
    return ATTR_CHAR.test(c) ? c : `%${byte.toString(16).toUpperCase().padStart(2, '0')}`;
  }).join('')}`;
  const asciiTitle = (name) => name.replace(/[^\x20-\x7e]/gu, '_');
  // The server keeps a title to 512 UTF-8 bytes, so a longer name keeps its
  // extension and loses whole characters from the end of the rest.
  const utf8Length = (text) => new TextEncoder().encode(text).length;
  function sourceTitle(name) {
    if (utf8Length(name) <= 512) return name;
    const extension = (/\.[a-z0-9]{1,8}$/i.exec(name) || [''])[0];
    const stem = Array.from(name.slice(0, name.length - extension.length));
    while (stem.length && utf8Length(stem.join('') + extension) > 512) stem.pop();
    return stem.join('') + extension;
  }

  // A session's native status once its turn exists. Before that it has no
  // `status_detail`, and `status` is its admission operation's state.
  const sessionStatus = (s) => (s.status_detail ? { name: s.name, status: s.status_detail } : null);
  const unstarted = (s) => !s.status_detail && ['failed', 'interrupted'].includes(s.status);
  function sessionActive(s) {
    const info = sessionStatus(s);
    return info ? ['running', 'waiting', 'waiting_on_children'].includes(info.status.state) : !unstarted(s);
  }
  function sessionLane(s) {
    const info = sessionStatus(s);
    if (!info) return unstarted(s) ? 'needs' : 'running';
    return info.status.state === 'waiting_on_children' ? 'running' : laneOf(info, new Map()).key;
  }
  function sessionText(s) {
    if (unstarted(s)) return `did not start${s.error ? `: ${s.error.message}` : ''}`;
    const info = sessionStatus(s);
    return info ? statusOf(info).text : 'starting';
  }
  const sessionsOf = (v) => (v.workspace.sessions || []).slice().sort((x, y) => x.created_at_ms - y.created_at_ms);
  const selectedSession = (v) => sessionsOf(v).find((s) => s.id === v.session) || null;

  // The message box's verb for the selected session, by the branch rules:
  // a running turn steers, an interrupted one resumes, any other sends.
  const SESSION_COMPOSER = {
    send: {
      action: 'send', label: 'Send', title: 'Send a follow-up', rule: when.send,
      placeholder: (s) => `Follow up in ${s.name}…`, started: 'Sent. A new turn is starting.',
    },
    steer: {
      action: 'steer', label: 'Steer', title: 'Steer the running turn', rule: when.steer,
      placeholder: (s) => `Steer ${s.name}'s running turn…`, started: 'Steering sent. The running turn reads it next.',
    },
    resume: {
      action: 'resume', label: 'Resume', title: 'Resume the interrupted turn', rule: when.resume, empty: RESUME_PROMPT,
      placeholder: () => 'Resume as it was, or say what to do next…', started: 'Resuming. A new turn is starting.',
    },
  };

  function workspaceCanRun(v) {
    const ws = v.workspace;
    const caps = ws.capabilities || {};
    return can('run') && !v.gone && caps.can_run !== false &&
      (!ws.current_role || ['owner', 'editor'].includes(ws.current_role));
  }

  function workspaceCanComment(v) {
    const ws = v.workspace;
    const caps = ws.capabilities || {};
    return can('run') && !v.gone && caps.can_comment !== false &&
      (!ws.current_role || ['owner', 'editor', 'reviewer'].includes(ws.current_role));
  }

  function sessionCanContinue(v, s) {
    return s && s.access === 'reviewer' ? workspaceCanComment(v) : workspaceCanRun(v);
  }

  function sessionComposer(v, s) {
    const info = s && sessionStatus(s);
    const st = info && info.status.state;
    const mode = st === 'running' ? SESSION_COMPOSER.steer : st === 'interrupted' ? SESSION_COMPOSER.resume : SESSION_COMPOSER.send;
    let why = null;
    if (!sessionCanContinue(v, s)) why = 'this workspace does not permit continuing this session';
    else if (!s) why = 'start a session first';
    else if (!['ready', 'running'].includes(v.workspace.state)) why = `the workspace is ${workspaceStateText(v.workspace)}`;
    else if (unstarted(s)) why = `${s.name} did not start${s.error ? `: ${s.error.message}` : ''}`;
    else if (!info) why = `${s.name} is starting; its first turn has not begun`;
    else if ((v.workspace.current_subject || state.me.subject) && s.subject && s.subject !== (v.workspace.current_subject || state.me.subject)) why = `only ${s.subject} can continue ${s.name}`;
    else why = mode.rule(info);
    return { mode, why };
  }

  // The workspace's rare actions, in the ⋯ menu. `when` is false when an
  // action does not apply now (not shown), the server's reason when it
  // reports the capability missing (shown disabled, with that reason), or
  // null when it can be asked for.
  const WORKSPACE_MENU = [
    {
      label: 'Cancel turn', danger: true, run: (v, s) => cancelSession(v, s),
      when: (v, s) => (s && sessionActive(s) && sessionCanContinue(v, s) ? null : false),
    },
    {
      label: 'Archive', run: (v) => lifecycle(v, 'archive'),
      when: (v) => (v.workspace.state === 'ready' && workspaceCanRun(v) ? null : false),
    },
    {
      label: 'Restore', run: (v) => lifecycle(v, 'restore'),
      when: (v) => (['archived', 'failed', 'interrupted'].includes(v.workspace.state) && workspaceCanRun(v) ? null : false),
    },
    { label: 'Sleep', run: (v) => lifecycle(v, 'sleep'), when: (v) => capability(v, 'sleep') },
  ];

  function capability(v, name) {
    const c = (v.workspace.capabilities || {})[name];
    if (!c || v.workspace.state !== 'ready' || !workspaceCanRun(v)) return false;
    return c.available ? null : c.reason || 'not available';
  }

  // The open workspace, kept across polls and session switches.
  let wsView = null;

  function closeWorkspace() {
    if (wsView) clearTimeout(wsView.timer);
    wsView = null;
  }

  // The one live preview, of a source or an output, disposed on every route,
  // tab, item and source change and on signing out, so no hidden preview
  // keeps fetching.
  let livePreview = null;
  function closePreview() {
    if (livePreview) livePreview.dispose();
    livePreview = null;
    if (!wsView) return;
    wsView.previewing = null;
    const slot = $('preview');
    if (slot) fill(slot);
    for (const b of document.querySelectorAll('#inspector button[data-preview]')) b.setAttribute('aria-pressed', 'false');
  }

  // Every write the server checks against the workspace's revision reads it
  // here. A stale revision (or a write that lost a race for the record)
  // reloads the workspace and asks the person to try again: no write is
  // retried behind their back.
  async function revised(v, what, write) {
    try {
      return await write(v.workspace.revision);
    } catch (e) {
      if (e.status !== 409 || e.code !== 'revision_conflict') throw e;
      let reloaded = true;
      try { await reloadDetail(v); } catch (_) { reloaded = false; }
      if (wsView === v) drawWorkspace(v);
      toast(reloaded
        ? `${what}: the workspace changed meanwhile. It is reloaded at revision ${v.workspace.revision}; try again.`
        : `${what}: the workspace changed meanwhile, and reloading it failed. Reload the page, then try again.`);
      return null;
    }
  }

  async function renderWorkspace() {
    const { workspace: id, session } = state.route;
    if (wsView && wsView.id === id && $('workspace')) {
      selectSession(wsView, session);
      return;
    }
    const v = {
      id, workspace: null, session: null, messages: new Map(),
      artifacts: null, artifactsError: null, artifactsFor: null,
      selected: new Set(), previewing: null, tab: 'sources',
      composing: false, busy: null, sending: false, following: 0, drafts: new Map(),
      timer: null, refreshing: false, again: false, failures: 0, failure: null,
      asked: 0, applied: 0, drawn: {},
    };
    wsView = v;
    main(h('div', { class: 'page' }, h('p', { class: 'muted', role: 'status', text: 'Opening the workspace…' })));
    try {
      await reloadDetail(v);
    } catch (e) {
      if (wsView !== v) return;
      main(h('div', { class: 'page' }, h('a', { class: 'back', href: '#/', text: '‹ Back' }),
        h('h1', { text: 'Workspace not available' }), h('p', { class: 'error', text: e.message })));
      return;
    }
    if (wsView !== v) return;
    workspaceLayout(v);
    selectSession(v, session);
    loadArtifacts(v).then(() => { if (wsView === v) drawOutputs(v); });
    scheduleWorkspace(v);
  }

  // Reads overlap (a poll, an upload's reload): one asked earlier never
  // replaces one asked later. Outputs are read again whenever a session's
  // turn or status moved since they were last read.
  async function reloadDetail(v) {
    const asked = (v.asked += 1);
    const ws = await api('GET', workspacePath(v.id));
    if (wsView !== v || asked < v.applied) return;
    v.applied = asked;
    v.workspace = ws;
    prune(v);
    if (!projectOf(ws.project_id)) {
      const project = await api('GET', `/v1/projects/${enc(ws.project_id)}`);
      state.projects = (state.projects || []).filter((p) => p.id !== project.id).concat(project);
      if (wsView !== v) return;
    }
    useProject(ws.project_id);
    const row = state.workspaces.list.find((w) => w.id === ws.id);
    if (row && (row.state !== ws.state || row.title !== ws.title)) {
      Object.assign(row, { state: ws.state, title: ws.title, revision: ws.revision });
      workspacesChanged();
    } else if (!row && state.workspaces.loaded && state.workspaces.project === ws.project_id) {
      loadWorkspaces(ws.project_id);
    }
    if (v.artifacts !== null && turnsOf(v) !== v.artifactsFor) await loadArtifacts(v);
  }

  const INSPECTOR = [['sources', 'Sources', 'sources-count'], ['outputs', 'Outputs', 'outputs-count'], ['details', 'Details']];

  function workspaceLayout(v) {
    const files = h('input', { type: 'file', id: 'source-file', multiple: true, hidden: true, tabindex: '-1' });
    files.addEventListener('change', () => {
      const picked = Array.from(files.files);
      files.value = '';
      if (picked.length) uploadSources(v, picked);
    });
    const drop = h('div', { class: 'drop', id: 'source-drop', text: 'Or drop files here' });
    drop.addEventListener('dragover', (ev) => { ev.preventDefault(); drop.classList.add('over'); });
    drop.addEventListener('dragleave', () => drop.classList.remove('over'));
    drop.addEventListener('drop', (ev) => {
      ev.preventDefault();
      drop.classList.remove('over');
      const picked = Array.from((ev.dataTransfer && ev.dataTransfer.files) || []);
      if (picked.length && workspaceCanRun(v)) uploadSources(v, picked);
    });
    main(h('div', { class: 'branch workspace', id: 'workspace' },
      h('section', { class: 'focus', 'aria-label': 'Conversation' },
        h('header', { class: 'strip', id: 'strip' }),
        h('nav', { class: 'sessions', id: 'sessions', 'aria-label': 'Sessions' }),
        h('div', { class: 'convo', id: 'convo' }),
        composer(() => submitSession(v))),
      h('aside', { class: 'inspector', id: 'inspector', 'aria-label': 'Inspector' },
        tablist(INSPECTOR, () => v.tab, (tab) => selectWorkspaceTab(v, tab)),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-sources', 'aria-labelledby': 'tab-sources' },
          h('div', { class: 'panel-tools' },
            h('button', { type: 'button', id: 'upload-sources', text: 'Upload sources', disabled: !workspaceCanRun(v), onclick: () => files.click() }),
            files, workspaceCanRun(v) ? drop : null),
          h('ul', { class: 'files file-list', id: 'sources', 'aria-label': 'Sources' }),
          h('div', { class: 'preview', id: 'preview', 'aria-label': 'Preview' })),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-outputs', 'aria-labelledby': 'tab-outputs', hidden: true },
          h('div', { id: 'outputs-list' })),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-details', 'aria-labelledby': 'tab-details', hidden: true },
          h('dl', { class: 'facts', id: 'ws-facts' }),
          h('h2', { text: 'Sessions' }), h('ul', { class: 'events', id: 'ws-sessions' }),
          h('h2', { text: 'Members' }), h('ul', { class: 'events', id: 'ws-members' })))));
  }

  function selectWorkspaceTab(v, tab) {
    closePreview();
    v.tab = tab;
    showTab(INSPECTOR, tab);
    if (tab === 'outputs' && v.artifacts === null) loadArtifacts(v).then(() => { if (wsView === v) drawOutputs(v); });
    drawWorkspace(v);
  }

  // The session the route names, else the most recent; a workspace with no
  // session opens the form for its first.
  function selectSession(v, id) {
    const sessions = sessionsOf(v);
    const chosen = sessions.find((s) => s.id === id) || sessions[sessions.length - 1] || null;
    const area = $('composer-text');
    if (v.session !== (chosen && chosen.id)) {
      if (v.session) v.drafts.set(v.session, area.value);
      area.value = (chosen && v.drafts.get(chosen.id)) || '';
    }
    v.session = chosen ? chosen.id : null;
    if (v.composing || !chosen) {
      v.composing = !chosen && (workspaceCanRun(v) || workspaceCanComment(v)) && v.workspace.state === 'ready';
      v.drawn.convo = null;
    }
    drawWorkspace(v);
    if (v.composing) openSessionForm(v);
    if (chosen && sessionStatus(chosen)) loadMessages(v, chosen.id).then(() => { if (wsView === v) drawWorkspace(v); });
  }

  // A session's messages page by page from its cursor. The server reads
  // the native log a page at a time, so a page may hold no message while
  // the cursor still moves.
  function loadMessages(v, id) {
    let m = v.messages.get(id);
    if (!m) {
      m = { list: [], cursor: '0', loaded: false, error: null, loading: null };
      v.messages.set(id, m);
    }
    if (m.loading) return m.loading;
    m.loading = (async () => {
      try {
        for (;;) {
          const page = await api('GET', `${workspacePath(v.id)}/sessions/${enc(id)}/messages?after=${enc(m.cursor)}`);
          m.list.push(...page.messages);
          const moved = page.next_cursor !== m.cursor;
          m.cursor = page.next_cursor;
          if (!moved) break;
        }
        m.loaded = true;
        m.error = null;
      } catch (e) {
        m.error = e.message;
      } finally {
        m.loading = null;
      }
    })();
    return m.loading;
  }

  const turnsOf = (v) => JSON.stringify((v.workspace.sessions || []).map((s) => [s.id, s.status, s.turns]));
  async function loadArtifacts(v) {
    const sessions = v.workspace.sessions || [];
    v.artifactsFor = turnsOf(v);
    if (!sessions.length) { v.artifacts = []; return; }
    try {
      v.artifacts = (await api('GET', `${workspacePath(v.id)}/artifacts`)).artifacts;
      v.artifactsError = null;
    } catch (e) {
      v.artifactsError = e.message;
      if (v.artifacts === null) v.artifacts = [];
    }
    prune(v);
  }

  // While anything here is starting, running or settling, the page reads
  // the workspace again every second and a half; managed workspaces have
  // no event stream to listen to.
  function scheduleWorkspace(v, delay = 1500) {
    clearTimeout(v.timer);
    if (wsView !== v || !v.workspace || v.gone) return;
    const busy = v.following > 0 || workspaceLane(v.workspace).key === 'running' || (v.workspace.sessions || []).some(sessionActive);
    if (busy) v.timer = setTimeout(() => refreshWorkspace(v), Math.min(30000, delay * 2 ** v.failures));
  }

  async function refreshWorkspace(v) {
    if (wsView !== v) return;
    if (v.refreshing) { v.again = true; return; }
    v.refreshing = true;
    try {
      await reloadDetail(v);
      const s = wsView === v ? selectedSession(v) : null;
      if (s && sessionStatus(s)) await loadMessages(v, s.id);
      v.failures = 0;
      v.failure = null;
    } catch (e) {
      v.failures += 1;
      v.gone = e.status === 403 || e.status === 404;
      v.failure = v.gone ? `This workspace is no longer open to this token: ${e.message}` : `Could not refresh: ${e.message}`;
    } finally {
      v.refreshing = false;
    }
    if (wsView !== v || !state.token) return;
    drawWorkspace(v);
    if (v.again) { v.again = false; refreshWorkspace(v); return; }
    scheduleWorkspace(v);
  }

  // Watch an operation this page started; the workspace is read again as
  // it settles, and a failure (or `finished`) is said.
  async function follow(v, op, label, finished) {
    v.following += 1;
    scheduleWorkspace(v, 300);
    const done = await awaitOperation(op.id);
    v.following -= 1;
    if (done && done.state !== 'succeeded') toast(`${label} ${done.state}${done.error ? `: ${done.error.message}` : '.'}`);
    else if (done && finished) toast(finished);
    if (wsView === v) refreshWorkspace(v);
  }

  function drawWorkspace(v) {
    if (wsView !== v || !$('strip')) return;
    drawStrip(v);
    drawSessions(v);
    if (v.composing && v.syncForm) v.syncForm();
    $('upload-sources').disabled = !workspaceCanRun(v);
    const drop = $('source-drop');
    if (drop) drop.hidden = !workspaceCanRun(v);
    drawConversation(v);
    applySessionComposer(v);
    drawSources(v);
    drawOutputs(v);
    drawDetails(v);
  }

  function drawStrip(v) {
    const ws = v.workspace;
    const s = selectedSession(v);
    const project = projectOf(ws.project_id);
    const lane = workspaceLane(ws);
    const items = WORKSPACE_MENU.map((item) => ({ item, why: item.when(v, s) })).filter(({ why }) => why !== false);
    const n = v.selected.size;
    const signature = JSON.stringify([ws.title, ws.state, ws.revision, project && project.name, n, v.busy, v.failure, s && s.id, items.map(({ item, why }) => [item.label, why])]);
    if (signature === v.drawn.strip) return;
    const strip = $('strip');
    if (strip.querySelector('details.menu[open]')) return;
    v.drawn.strip = signature;
    const kept = document.activeElement === strip.querySelector('h1');
    const focused = strip.contains(document.activeElement) && document.activeElement.id;
    let action = null;
    if (v.busy) action = h('button', { type: 'button', class: 'primary', disabled: true, text: v.busy });
    else if (n) action = h('button', { type: 'button', class: 'primary', id: 'accept', text: `Accept ${n} selected`, disabled: !workspaceCanRun(v), onclick: () => acceptSelected(v) });
    fill(strip,
      h('a', { class: 'back', href: `#/p/${enc(ws.project_id)}`, text: '‹ Workspaces' }),
      h('div', { class: 'strip-title' }, h('h1', { tabindex: '-1', text: ws.title }),
        h('p', { class: 'strip-meta', text: `${project ? project.name : 'Project'} · revision ${ws.revision}` })),
      h('p', { class: `state state-${lane.key}` }, dot(lane.key), cap(workspaceStateText(ws))),
      action,
      items.length ? workspaceMenu(v, s, items) : null,
      v.failure ? h('p', { class: 'error strip-error', role: 'status', text: v.failure }) : null);
    if (kept) strip.querySelector('h1').focus({ preventScroll: true });
    else if (focused && $(focused)) $(focused).focus({ preventScroll: true });
  }

  function workspaceMenu(v, s, items) {
    const menu = h('details', { class: 'menu' },
      h('summary', { 'aria-label': 'More actions', title: 'More actions' }, h('span', { 'aria-hidden': 'true', text: '⋯' })),
      h('div', { class: 'menu-list' }, items.map(({ item, why }) => h('button', {
        type: 'button', class: item.danger ? 'menu-item danger-text' : 'menu-item', disabled: why != null,
        onclick: () => { menu.open = false; item.run(v, s); },
      }, h('span', { text: item.label }), why != null ? h('span', { class: 'menu-why', text: `Not available here: ${why}` }) : null))));
    menu.addEventListener('toggle', () => { if (!menu.open && wsView === v) drawStrip(v); });
    return menu;
  }

  function drawSessions(v) {
    const sessions = sessionsOf(v);
    const signature = JSON.stringify([sessions.map((s) => [s.id, s.name, sessionLane(s), sessionText(s)]), v.session, v.composing, workspaceCanRun(v), workspaceCanComment(v)]);
    if (signature === v.drawn.sessions) return;
    v.drawn.sessions = signature;
    fill($('sessions'), sessions.map((s) => h('a', {
      class: 'session-tab', href: workspaceHref(v.id, s.id), 'data-session': s.id, title: `${s.name}: ${sessionText(s)}`,
      'aria-current': s.id === v.session && !v.composing ? 'page' : null,
      onclick: () => { if (v.composing) { v.composing = false; v.drawn.convo = null; drawWorkspace(v); } },
    }, dot(sessionLane(s)), h('span', { class: 'session-name', text: s.name }))),
    workspaceCanRun(v) || workspaceCanComment(v) ? h('button', {
      type: 'button', class: 'ghost new-session', id: 'new-session', 'aria-pressed': String(v.composing), text: '+ Session',
      onclick: () => { v.composing = true; v.drawn.convo = null; drawWorkspace(v); openSessionForm(v); },
    }) : null);
  }

  // Messages are turned into turns by the branch conversation's reducer:
  // what the person sent opens a turn and the agent's text joins it.
  function drawConversation(v) {
    if (v.composing) return;
    const box = $('convo');
    const s = selectedSession(v);
    const m = s ? v.messages.get(s.id) : null;
    const signature = JSON.stringify([s && s.id, s && sessionText(s), m && [m.list.length, m.error, m.loaded]]);
    if (signature === v.drawn.convo) return;
    const fresh = !v.drawn.convo || JSON.parse(v.drawn.convo)[0] !== (s && s.id);
    v.drawn.convo = signature;
    const atEnd = box.scrollHeight - box.scrollTop - box.clientHeight < 48;
    if (!s) {
      fill(box, h('p', { class: 'muted', text: workspaceCanRun(v) || workspaceCanComment(v) ? 'No sessions yet. Start one with + Session.' : 'No sessions yet.' }));
      return;
    }
    const working = sessionActive(s);
    const turns = conversation((m ? m.list : []).map((x) => ({
      activity: x.role === 'user' ? { prompt: x.text } : { harness: { type: 'message_delta', text: x.text } },
    })));
    const workingLine = () => h('p', { class: 'outcome' }, dot('running'), sessionStatus(s) ? 'Working…' : 'Starting…');
    fill(box,
      m && m.error ? h('p', { class: 'error', text: m.error }) : null,
      turns.map((t, i) => h('article', { class: 'turn' },
        t.text != null ? h('div', { class: 'you' }, h('p', { text: t.text })) : null,
        t.said.length ? h('div', { class: 'agent' }, t.said.map((x) => h('p', { text: x }))) : null,
        working && i === turns.length - 1 ? workingLine() : null)),
      working && !turns.length ? workingLine() : null,
      unstarted(s) ? h('p', { class: 'outcome outcome-needs' }, dot('needs'), cap(sessionText(s))) : null,
      !working && !unstarted(s) && !turns.length && m && m.loaded ? h('p', { class: 'muted', text: 'No messages yet.' }) : null);
    if (atEnd || fresh) box.scrollTop = box.scrollHeight;
  }

  function applySessionComposer(v) {
    const s = selectedSession(v);
    const { mode, why } = sessionComposer(v, s);
    const area = $('composer-text');
    $('composer').hidden = v.composing;
    $('composer').dataset.mode = why ? 'off' : mode.action;
    $('composer-label').textContent = mode.title;
    area.placeholder = why ? sentence(why, s && s.name) : mode.placeholder(s);
    area.disabled = !!why;
    $('composer-go').disabled = !!why || v.sending;
    $('composer-go').textContent = mode.label;
    $('composer-allow-box').hidden = true;
    $('composer-cancel').hidden = true;
  }

  async function submitSession(v) {
    if (wsView !== v) return;
    const s = selectedSession(v);
    const { mode, why } = sessionComposer(v, s);
    const area = $('composer-text');
    if (why || v.sending) return;
    const text = area.value.trim() || mode.empty;
    if (!text) { area.focus(); return; }
    v.sending = true;
    applySessionComposer(v);
    const verb = mode.label.toLowerCase();
    let op = null;
    try {
      op = await revised(v, `Could not ${verb}`, () => api('POST', `${workspacePath(v.id)}/sessions/${enc(s.id)}/messages`, { prompt: text, action: mode.action }));
    } catch (e) {
      toast(`Could not ${verb}: ${e.message}`);
    }
    v.sending = false;
    if (wsView !== v) return;
    if (op) {
      area.value = '';
      toast(mode.started);
      follow(v, op, mode.label);
    }
    applySessionComposer(v);
  }

  async function cancelSession(v, s) {
    if (!(await confirmDialog('Cancel turn?', `Cancel ${s.name}'s running turn?`, 'Cancel turn', true)) || wsView !== v) return;
    let op = null;
    try {
      op = await revised(v, 'Could not cancel the turn', () => api('POST', `${workspacePath(v.id)}/sessions/${enc(s.id)}/cancel`, {}));
    } catch (e) {
      toast(`Could not cancel the turn: ${e.message}`);
    }
    if (!op || wsView !== v) return;
    toast('Cancelling the running turn.');
    follow(v, op, 'Cancel', 'Turn cancelled.');
  }

  const LIFECYCLE = {
    archive: { busy: 'Archiving…', finished: 'Archived. Its sources, transcripts and accepted outputs are kept.' },
    restore: { busy: 'Restoring…', finished: 'Restored.' },
    sleep: { busy: 'Going to sleep…', finished: 'Asleep.' },
  };

  async function lifecycle(v, action) {
    const ws = v.workspace;
    if (action === 'archive' && !(await confirmDialog(`Archive ${ws.title}?`,
      'Its sources, transcripts and accepted outputs are kept, and Restore brings it back. No session runs while it is archived.', 'Archive'))) return;
    if (wsView !== v) return;
    v.busy = LIFECYCLE[action].busy;
    drawStrip(v);
    let op = null;
    try {
      op = await revised(v, `Could not ${action} ${ws.title}`, (revision) => api('POST', `${workspacePath(v.id)}/actions/${action}`, { revision }));
    } catch (e) {
      const reason = e.detail && e.detail.reason ? ` (${e.detail.reason})` : '';
      toast(`Could not ${action} ${ws.title}: ${e.message}${reason}.`);
    }
    v.busy = null;
    if (wsView !== v) return;
    if (op) follow(v, op, cap(action), LIFECYCLE[action].finished);
    drawWorkspace(v);
  }

  // Sources upload one at a time: each upload moves the revision the next
  // one must name. A file the server refuses is named and the rest still
  // go; a stale revision or a failed reload stops, naming what was not sent.
  async function uploadSources(v, files) {
    const sent = [];
    const refused = [];
    let unsent = [];
    for (const [i, file] of files.entries()) {
      let source = null;
      const title = sourceTitle(file.name);
      try {
        source = await revised(v, `Could not upload ${file.name}`, (revision) => call(`${workspacePath(v.id)}/sources`, {
          method: 'POST',
          headers: {
            Accept: 'application/json',
            'Content-Type': file.type || 'application/octet-stream',
            'X-Source-Title-Encoded': encodedTitle(title),
            'X-Source-Title': asciiTitle(title),
            'X-Workspace-Revision': String(revision),
            'Idempotency-Key': key(),
          },
          body: file,
        }));
      } catch (e) {
        if (wsView !== v) return;
        refused.push(`${file.name} (${e.message})`);
        continue;
      }
      if (wsView !== v) return;
      if (!source) { unsent = files.slice(i); break; }
      sent.push(file.name);
      try { await reloadDetail(v); } catch (_) { unsent = files.slice(i + 1); break; }
    }
    if (wsView !== v) return;
    if (sent.length) toast(`Uploaded ${plural(sent.length, 'source')}.`);
    if (refused.length) toast(`Not uploaded: ${refused.join('; ')}.`);
    if (unsent.length) toast(`Not sent: ${unsent.map((f) => f.name).join(', ')}. Upload ${unsent.length === 1 ? 'it' : 'them'} again.`);
    drawWorkspace(v);
  }

  // Refusals that mean the page's list was out of date: what happened, and
  // what to do once the outputs are read again.
  const ACCEPT_REFUSED = {
    deliverable_not_current: ['an output you chose is no longer its session\'s current result', 'The outputs are reloaded; accept the ones marked Current.'],
    producer_busy: ['the session that made these outputs is working on a newer turn', 'The outputs are reloaded; accept the ones marked Current once that turn finishes.'],
  };

  async function acceptSelected(v) {
    const ids = Array.from(v.selected);
    const names = ids.map((id) => ((v.artifacts || []).find((a) => a.id === id) || { name: id }).name);
    if (!(await confirmDialog(`Accept ${plural(ids.length, 'output')}?`,
      `${names.join(', ')} become accepted deliverables of ${v.workspace.title}. The server checks each came from its session's latest finished turn.`, 'Accept'))) return;
    if (wsView !== v) return;
    v.busy = 'Accepting…';
    drawStrip(v);
    let result = null;
    try {
      result = await revised(v, 'Could not accept', (revision) => api('POST', `${workspacePath(v.id)}/finalize`, { revision, artifacts: ids }));
    } catch (e) {
      const refused = e.status === 409 && ACCEPT_REFUSED[e.code];
      if (refused) {
        let reloaded = true;
        try { await reloadDetail(v); } catch (_) { reloaded = false; }
        if (wsView === v) scheduleWorkspace(v);
        toast(`Not accepted: ${refused[0]}. ${reloaded ? refused[1] : 'Reload the page, then try again.'}`);
      } else {
        toast(`Could not accept: ${e.message}`);
      }
    }
    v.busy = null;
    if (wsView !== v) return;
    if (result) {
      v.selected.clear();
      toast(`Accepted ${plural(result.artifacts.length, 'output')}.`);
      try { await reloadDetail(v); } catch (_) { /* the poll catches up */ }
    }
    drawWorkspace(v);
  }

  // A source's or output's name: it opens that file in the preview.
  function previewButton(v, item) {
    return h('button', {
      type: 'button', class: 'file-name', 'data-preview': item.key, 'aria-pressed': String(v.previewing === item.key),
      title: `Preview ${item.name}`, text: item.name, onclick: () => openPreview(v, item),
    });
  }

  function drawSources(v) {
    const sources = (v.workspace.sources || []).slice().sort((x, y) => x.created_at_ms - y.created_at_ms);
    $('sources-count').textContent = sources.length ? String(sources.length) : '';
    const signature = JSON.stringify(sources.map((src) => src.id));
    if (signature === v.drawn.sources) return;
    v.drawn.sources = signature;
    fill($('sources'), sources.length ? sources.map((src) => {
      const base = `${workspacePath(v.id)}/sources/${enc(src.id)}`;
      return h('li', { class: 'file-item', 'data-source': src.id },
        h('div', { class: 'file-main' },
          previewButton(v, { key: `source:${src.id}`, name: src.title, mediaType: src.media_type, base }),
          h('span', { class: 'item-meta', title: src.media_type }, `${formatOf(src.media_type, src.title)} · ${size(src.size_bytes)} · `, ago(src.created_at_ms))),
        h('button', { type: 'button', class: 'ghost', text: 'Download', 'aria-label': `Download ${src.title}`, onclick: () => download(`${base}/content`, src.title) }));
    })
      : h('li', { class: 'muted pad', text: 'No sources yet. Upload the documents this workspace\'s sessions should read; each new session gets the ones you tick.' }));
  }

  // Where an output stands toward acceptance. The server accepts an output
  // only from its session's current turn, once that turn finished with its
  // result and nothing more is queued for the session (`finalize` in
  // console/sources.rs); `current` is exactly that.
  function standing(a, s, accepted) {
    if (accepted.has(a.id)) return 'accepted';
    if (s && a.turn < s.turns) return 'earlier';
    if (s && sessionActive(s)) return 'working';
    const info = s && sessionStatus(s);
    const finished = info && ['ready', 'no_changes', 'merged'].includes(info.status.state) && s.result_event != null && !s.result_error;
    return finished ? 'current' : 'unsettled';
  }

  const STANDING = {
    accepted: { pick: false, tag: 'Accepted', note: (a) => `turn ${a.turn}` },
    current: { pick: true, tag: 'Current', note: (a) => `turn ${a.turn}` },
    working: { pick: false, note: (a) => `turn ${a.turn} · a newer turn is running` },
    earlier: { pick: false, note: (a) => `Earlier turn ${a.turn} — not the current result` },
    unsettled: { pick: false, note: (a) => `turn ${a.turn} · this turn ended without its result` },
  };

  // Outputs under the session that made them, in session order; within a
  // session, the newest turn first.
  function outputGroups(v) {
    const accepted = new Set((v.workspace.accepted_deliverables || []).map((d) => d.artifact_id));
    const sessions = sessionsOf(v);
    const byBranch = new Map(sessions.map((s) => [s.branch, s]));
    const groups = new Map();
    for (const a of v.artifacts || []) {
      const s = byBranch.get(a.publisher_branch) || null;
      if (!groups.has(a.publisher_branch)) groups.set(a.publisher_branch, { s, name: s ? s.name : a.publisher_branch, rows: [] });
      groups.get(a.publisher_branch).rows.push({ a, standing: standing(a, s, accepted) });
    }
    const order = (g) => (g.s ? sessions.indexOf(g.s) : sessions.length);
    return Array.from(groups.values()).sort((x, y) => order(x) - order(y))
      .map((g) => ({ ...g, rows: g.rows.sort((x, y) => y.a.turn - x.a.turn) }));
  }

  // Ticked outputs that can no longer be accepted are unticked.
  function prune(v) {
    if (!v.artifacts) return;
    const pickable = new Set(outputGroups(v).flatMap((g) => g.rows).filter((r) => STANDING[r.standing].pick).map((r) => r.a.id));
    for (const id of v.selected) if (!pickable.has(id)) v.selected.delete(id);
  }

  function drawOutputs(v) {
    const list = $('outputs-list');
    if (!list || wsView !== v) return;
    const all = v.artifacts || [];
    const groups = outputGroups(v);
    $('outputs-count').textContent = all.length ? String(all.length) : '';
    const signature = JSON.stringify([groups.map((g) => [g.name, g.s && sessionText(g.s), g.rows.map((r) => [r.a.id, r.standing])]),
      v.artifactsError, v.artifacts === null, workspaceCanRun(v)]);
    if (signature === v.drawn.outputs) return;
    v.drawn.outputs = signature;
    const row = ({ a, standing: st }) => {
      const meaning = STANDING[st];
      const note = nextId('output-note');
      const box = h('input', {
        type: 'checkbox', 'aria-label': `Select ${a.name}`, 'aria-describedby': note,
        checked: st === 'accepted' || v.selected.has(a.id), disabled: !meaning.pick || !workspaceCanRun(v),
      });
      box.addEventListener('change', () => {
        if (box.checked) v.selected.add(a.id); else v.selected.delete(a.id);
        drawStrip(v);
      });
      return h('li', { class: 'output-row', 'data-artifact': a.id, 'data-standing': st },
        box,
        h('div', { class: 'file-main' },
          previewButton(v, { key: `output:${a.id}`, name: a.name, mediaType: a.media_type, base: `${workspacePath(v.id)}/artifacts/${enc(a.id)}` }),
          h('span', { class: 'item-meta', id: note, title: a.media_type, text: `${formatOf(a.media_type, a.name)} · ${size(a.size)} · ${meaning.note(a)}` })),
        meaning.tag ? h('span', { class: `tag tag-${st}` }, st === 'accepted' ? checkMark(false) : null, meaning.tag) : null);
    };
    const group = (g) => {
      const heading = nextId('output-group');
      return h('section', { class: 'output-group', 'aria-labelledby': heading },
        h('h3', { id: heading }, g.s ? dot(sessionLane(g.s)) : null, h('span', { class: 'group-name', text: g.name }),
          g.s ? h('span', { class: 'group-meta', text: cap(sessionText(g.s)) }) : null),
        h('ul', { class: 'file-list', 'aria-labelledby': heading }, g.rows.map(row)));
    };
    fill(list,
      v.artifactsError ? h('p', { class: 'error pad', text: `Could not list outputs: ${v.artifactsError}` }) : null,
      v.artifacts === null ? h('p', { class: 'muted pad', text: 'Loading outputs…' })
        : !all.length ? h('p', { class: 'muted pad', text: `No outputs yet. A session started with Collect outputs publishes the files it lists in ${OUTPUT_MANIFEST} when its turn ends.` })
          : groups.map(group));
  }

  // Mount the preview component for one source or output in the open
  // panel. A table, a page, or why the server made neither is the
  // component's to show; the original stays one click away.
  function openPreview(v, item) {
    closePreview();
    v.previewing = item.key;
    for (const b of document.querySelectorAll('#inspector button[data-preview]')) b.setAttribute('aria-pressed', String(b.dataset.preview === item.key));
    const slot = $('preview');
    $(`panel-${v.tab}`).append(slot);
    const host = h('div', { class: 'preview-host', id: 'preview-host' });
    fill(slot,
      h('div', { class: 'preview-head' },
        h('h3', { id: 'preview-title', text: item.name, title: item.name }),
        h('button', { type: 'button', text: 'Download original', onclick: () => download(`${item.base}/content`, item.name) })),
      host);
    livePreview = window.BranchyardArtifactPreview.mount(host, { baseUrl: item.base, mediaType: item.mediaType, fetchAuthenticated: authFetch });
  }

  function drawDetails(v) {
    const ws = v.workspace;
    const project = projectOf(ws.project_id);
    const sessions = sessionsOf(v);
    const caps = ws.capabilities || {};
    const signature = JSON.stringify([ws.state, ws.revision, project && project.name, sessions.map((s) => [s.id, sessionText(s)]), ws.members, caps]);
    if (signature === v.drawn.details) return;
    v.drawn.details = signature;
    const lane = workspaceLane(ws);
    const unavailable = Object.entries(caps).filter(([, c]) => c && c.available === false);
    const rows = [
      ['Project', project ? h('a', { href: `#/p/${enc(project.id)}`, text: project.name }) : ws.project_id],
      ['State', h('span', { class: 'fact-state' }, dot(lane.key), cap(workspaceStateText(ws)))],
      ['Revision', String(ws.revision)],
      ['Results', cap(resultText(ws.result_mode))],
      ['Task', h('span', { class: 'mono', text: ws.task_id })],
      ['Created', new Date(ws.created_at_ms).toLocaleString()],
      ...unavailable.map(([name, c]) => [cap(name), `not available: ${c.reason}`]),
    ];
    fill($('ws-facts'), ...rows.map(([k, val]) => [h('dt', { text: k }), h('dd', {}, val)]).flat());
    fill($('ws-sessions'), sessions.length ? sessions.map((s) => h('li', {},
      h('a', { href: workspaceHref(v.id, s.id), text: s.name }),
      ` · ${[s.harness, s.access, sessionText(s)].filter(Boolean).join(' · ')}`))
      : h('li', { class: 'muted', text: 'None yet.' }));
    const members = ws.members || [];
    fill($('ws-members'), members.length ? members.map((m) => h('li', {}, h('span', { class: 'mono', text: m.subject }), ` · ${m.role}`))
      : h('li', { class: 'muted', text: 'This server lists no members.' }));
  }

  // The form for a new session, in the conversation pane until started or
  // cancelled. A poll does not redraw it.
  async function openSessionForm(v) {
    const box = $('convo');
    if (!box || box.querySelector('#session-form')) return;
    const ids = { name: nextId('session'), harness: nextId('harness'), access: nextId('access'), prompt: nextId('first'), model: nextId('model') };
    const name = h('input', { id: ids.name, type: 'text', autocomplete: 'off', placeholder: 'For example: Analyst' });
    let harness = h('select', { id: ids.harness, disabled: true }, h('option', { value: '', text: 'Loading…' }));
    const harnessSlot = h('div', {}, h('label', { class: 'block', for: ids.harness, text: 'Harness' }), harness);
    const access = h('select', { id: ids.access },
      h('option', { value: 'writer', text: 'Writer: works in its own copy' }),
      h('option', { value: 'reviewer', text: 'Reviewer: reads outputs you choose' }));
    if (!workspaceCanRun(v)) { access.value = 'reviewer'; access.disabled = true; }
    const canStart = () => access.value === 'reviewer' ? workspaceCanComment(v) : workspaceCanRun(v);
    const prompt = h('textarea', { id: ids.prompt, rows: '5', placeholder: 'What should this session do?' });
    const model = h('input', { id: ids.model, type: 'text', autocomplete: 'off', placeholder: 'Harness default' });
    const checkbox = (label, checked, value) => {
      const input = h('input', { type: 'checkbox', checked, value });
      return h('label', { class: 'toggle' }, input, h('span', { text: label }));
    };
    // The sources and outputs to choose from follow the workspace while the
    // form is open, keeping what was ticked; a new source starts ticked.
    const sourcesBox = h('fieldset', { class: 'choices' });
    const outputsBox = h('fieldset', { class: 'choices', hidden: true });
    const choices = (box, legend, items, ticked, empty) => {
      const was = new Map(Array.from(box.querySelectorAll('input'), (i) => [i.value, i.checked]));
      fill(box, h('legend', { text: legend }), items.length
        ? items.map(([value, label]) => checkbox(label, was.has(value) ? was.get(value) : ticked, value))
        : h('p', { class: 'hint', text: empty }));
    };
    let synced = null;
    let start = null;
    let submitting = false;
    v.syncForm = () => {
      if (start) start.disabled = submitting || v.workspace.state !== 'ready' || !canStart();
      const sources = (v.workspace.sources || []).map((src) => [src.id, src.title]);
      const outputs = (v.artifacts || []).map((a) => [a.id, a.name]);
      const signature = JSON.stringify([sources, outputs]);
      if (signature === synced) return;
      synced = signature;
      choices(sourcesBox, 'Sources to give it', sources, true, 'No sources yet: the Sources tab uploads them.');
      choices(outputsBox, 'Outputs to review', outputs, false, 'No outputs yet: a reviewer needs at least one.');
    };
    v.syncForm();
    const collect = checkbox(`Collect outputs (${OUTPUT_MANIFEST})`, v.workspace.result_mode !== 'code');
    const allow = checkbox('Allow every tool', false);
    const acknowledgement = checkbox('I accept tools without per-tool approval', false);
    const error = h('p', { class: 'error', role: 'alert' });
    const ready = v.workspace.state === 'ready';
    start = h('button', { type: 'button', class: 'primary', text: 'Start session', disabled: !ready || !canStart() });
    const cancel = h('button', {
      type: 'button', text: 'Cancel',
      onclick: () => { v.composing = false; v.drawn.convo = null; drawWorkspace(v); $('composer-text').focus(); },
    });
    access.addEventListener('change', () => {
      outputsBox.hidden = access.value !== 'reviewer';
      const reviewing = access.value === 'reviewer';
      collect.hidden = reviewing;
      for (const control of [allow, acknowledgement]) {
        const input = control.querySelector('input');
        input.disabled = reviewing;
        if (reviewing) input.checked = false;
      }
      v.syncForm();
    });
    start.addEventListener('click', async () => {
      const body = { name: name.value.trim(), prompt: prompt.value.trim(), harness: harness.value.trim(), access: access.value };
      const missing = !body.name ? [name, 'Name the session.'] : !body.harness ? [harness, 'Choose a harness.'] : !body.prompt ? [prompt, 'Give it a first prompt.'] : null;
      if (missing) { error.textContent = missing[1]; missing[0].focus(); return; }
      const ticked = (box) => Array.from(box.querySelectorAll('input:checked'), (i) => i.value);
      if (ticked(sourcesBox).length) body.sources = ticked(sourcesBox);
      if (body.access === 'reviewer') {
        body.artifacts = ticked(outputsBox);
        if (!body.artifacts.length) { error.textContent = 'Choose at least one output to review.'; return; }
      } else if (collect.querySelector('input').checked) body.result_contract = OUTPUT_CONTRACT;
      if (allow.querySelector('input').checked) body.policy = { mode: 'allow' };
      if (acknowledgement.querySelector('input').checked) body.unapproved_tools = true;
      if (model.value.trim()) body.model = model.value.trim();
      submitting = true;
      v.syncForm();
      error.textContent = '';
      let op = null;
      try {
        op = await revised(v, 'Could not start the session', () => api('POST', `${workspacePath(v.id)}/sessions`, body));
      } catch (e) {
        error.textContent = e.message;
      }
      if (wsView !== v) return;
      if (!op) { submitting = false; v.syncForm(); return; }
      toast('Session started.');
      v.composing = false;
      v.drawn.convo = null;
      try { await reloadDetail(v); } catch (_) { /* the poll catches up */ }
      follow(v, op, 'Session');
      if (wsView === v) go(workspaceHref(v.id, op.session_id));
    });
    fill(box, h('section', { class: 'card form session-form', id: 'session-form', 'aria-labelledby': 'session-form-h' },
      h('h2', { id: 'session-form-h', text: 'New session' }),
      h('label', { class: 'block', for: ids.name, text: 'Session name' }), name,
      h('div', { class: 'form-grid' }, harnessSlot,
        h('div', {}, h('label', { class: 'block', for: ids.access, text: 'Access' }), access)),
      h('label', { class: 'block', for: ids.model, text: 'Model (optional)' }), model,
      h('label', { class: 'block', for: ids.prompt, text: 'First prompt' }), prompt,
      sourcesBox,
      outputsBox,
      h('div', { class: 'row choices-row' }, collect, allow),
      h('details', {}, h('summary', { text: 'Tool approval compatibility' }), acknowledgement,
        h('p', { class: 'hint', text: 'AGY and some other harnesses cannot enforce per-tool approvals. Acknowledge this only when you accept that behavior. The server must also permit it; reviewers remain read-only.' })),
      h('p', { class: 'hint', text: `Collect outputs asks the session to list its result files in ${OUTPUT_MANIFEST} and end with a JSON summary; the server publishes those files as outputs you can preview and accept.` }),
      ready ? null : h('p', { class: 'hint', text: `The workspace is ${workspaceStateText(v.workspace)}; sessions start once it is ready.` }),
      h('div', { class: 'row form-actions' }, start, cancel),
      error));
    access.dispatchEvent(new Event('change'));
    name.focus();
    let listed = null;
    let why = null;
    try {
      listed = (await api('GET', '/v1/harnesses')).harnesses;
    } catch (e) {
      why = e.message;
    }
    if (!harness.isConnected) return;
    const usable = (listed || []).filter((x) => x.available);
    if (usable.length) {
      fill(harness, usable.map((x) => h('option', { value: x.default ? x.harness : x.profile, text: x.default ? x.harness : `${x.harness} (${x.profile})` })));
      harness.disabled = false;
    } else {
      const typed = h('input', { id: ids.harness, type: 'text', autocomplete: 'off', spellcheck: 'false', placeholder: 'A harness this server runs' });
      harness.replaceWith(typed);
      harness = typed;
      harnessSlot.append(h('p', { class: 'hint', text: why ? `The server lists no harnesses for this token (${why}); type one it runs.` : 'The server lists no available harness; type one it runs.' }));
    }
  }

  // ---------------------------------------------------------------
  // A new task

  async function renderNew() {
    if (!state.repo) {
      main(h('div', { class: 'page narrow' }, h('a', { class: 'back', href: '#/', text: '‹ Back' }),
        h('h1', { text: 'New task' }),
        h('p', { class: 'lede', text: 'A task runs in a repository, and this token reaches none. Start a knowledge project instead.' }),
        can('run') ? h('div', { class: 'row form-actions' }, h('a', { class: 'button primary', href: '#/p/new', text: 'New project' })) : null));
      return;
    }
    const promptId = nextId('prompt');
    const nameId = nextId('name');
    const harnessId = nextId('harness');
    const allowId = nextId('allow');
    const area = h('textarea', { id: promptId, required: true, rows: '6' });
    const nameInput = h('input', { id: nameId, type: 'text', autocomplete: 'off', spellcheck: 'false' });
    const select = h('select', { id: harnessId }, h('option', { value: '', text: 'the server\'s default' }));
    const allow = h('input', { type: 'checkbox', id: allowId });
    const error = h('p', { class: 'error', role: 'alert' });
    const go = h('button', {
      type: 'button', class: 'primary', text: 'Start task', disabled: !can('run'),
      onclick: async () => {
        const prompt = area.value.trim();
        if (!prompt) { area.focus(); return; }
        const body = { prompt };
        if (select.value) body.harness = select.value;
        if (nameInput.value.trim()) body.name = nameInput.value.trim();
        if (allow.checked) body.policy = { mode: 'allow' };
        go.disabled = true;
        try {
          const op = await api('POST', `${repoPath()}/tasks`, body);
          toast('Task started. Its branch is on the list.');
          followOperation(op.id, { label: 'Task' }, op.branches[0] || '');
          location.hash = op.branches.length ? branchHref(op.branches[0]) : '#/';
        } catch (e) {
          error.textContent = e.message;
          go.disabled = false;
        }
      },
    });
    main(h('div', { class: 'page narrow' },
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'New task' }),
        h('p', { class: 'lede', text: `It runs in ${state.repo}, on a branch of its own.` }))),
      h('div', { class: 'card form' },
        h('label', { class: 'block', for: promptId, text: 'Prompt' }), area,
        h('div', { class: 'form-grid' },
          h('div', {}, h('label', { class: 'block', for: harnessId, text: 'Harness' }), select),
          h('div', {}, h('label', { class: 'block', for: nameId, text: 'Branch name (optional)' }), nameInput)),
        h('label', { class: 'toggle', for: allowId }, allow, h('span', { text: 'Allow every tool (like --yes); otherwise tool requests are denied' })),
        h('div', { class: 'row form-actions' }, go),
        can('run') ? null : h('p', { class: 'hint', text: 'This token lacks the run scope.' })),
      error));
    try {
      const { harnesses } = await api('GET', '/v1/harnesses');
      for (const hn of harnesses.filter((x) => x.available)) select.append(h('option', { value: hn.harness, text: hn.harness }));
    } catch (_) { /* the default harness still works */ }
  }

  // ---------------------------------------------------------------
  // Needs you: approvals a turn waits on, questions and escalations
  // waiting for an answer, branches that stopped for a person, the
  // permission requests seen while this page was open, and the effect
  // ledger

  let inboxTimer = null;
  function refreshInbox() {
    clearTimeout(inboxTimer);
    inboxTimer = setTimeout(loadInbox, 300);
  }

  async function loadInbox() {
    const parents = Array.from(state.branches.values()).filter((b) => b.children && b.children.length);
    const asked = [];
    const answered = new Set();
    await Promise.all(parents.map(async (p) => {
      try {
        const inbox = await api('GET', `${branchPath(p.name)}/inbox`);
        for (const m of inbox.messages) {
          if (m.kind === 'question' || m.kind === 'escalation') asked.push(m);
        }
      } catch (_) { /* skip that branch */ }
    }));
    const askers = new Set(asked.map((m) => m.from));
    await Promise.all(Array.from(askers).map(async (name) => {
      try {
        const inbox = await api('GET', `${branchPath(name)}/inbox`);
        for (const m of inbox.messages) if (m.in_reply_to != null) answered.add(m.in_reply_to);
      } catch (_) { /* skip */ }
    }));
    state.inbox = asked.filter((m) => !answered.has(m.id)).sort((a, b) => b.at_ms - a.at_ms);
    branchesChanged();
  }

  let approvalsTimer = null;
  function refreshApprovals() {
    clearTimeout(approvalsTimer);
    approvalsTimer = setTimeout(loadApprovals, 300);
  }

  async function loadApprovals() {
    try {
      const [{ approvals }, { effects }] = await Promise.all([
        api('GET', `${repoPath()}/approvals`),
        api('GET', `${repoPath()}/effects`),
      ]);
      state.approvals = approvals;
      state.effects = effects.slice(-50).reverse();
    } catch (_) { return; }
    branchesChanged();
  }

  function aboutText(about) {
    switch (about.kind) {
      case 'tool': return `use the tool ${about.tool}`;
      case 'operation': return `${about.connector} ${about.operation} (${about.class}${about.deletion ? ', deletes' : ''})`;
      case 'promote': return `perform the staged ${about.connector} ${about.operation} (${about.class})`;
      default: return about.kind;
    }
  }

  function answerForm(m) {
    const id = nextId('answer');
    const area = h('textarea', { id, rows: '2', 'data-draft': `answer:${m.id}`, placeholder: m.kind === 'escalation' ? 'Optional note' : 'Your answer' });
    const disabled = !can('run');
    const send = async (prefix) => {
      const note = area.value.trim();
      const text = prefix ? (note ? `${prefix} ${note}` : prefix) : note;
      if (!text) { area.focus(); return; }
      try {
        await api('POST', `${branchPath(m.to)}/answer`, { message_id: m.id, text });
        toast(`Answered ${m.from}.`);
        loadInbox();
      } catch (e) {
        toast(`Could not answer: ${e.message}`);
      }
    };
    return h('div', { class: 'reply' },
      h('label', { class: 'block', for: id, text: `Answer as ${m.to}` }), area,
      h('div', { class: 'row' },
        m.kind === 'escalation'
          ? [h('button', { type: 'button', class: 'primary', disabled, text: 'Approve', onclick: () => send('Approved.') }),
            h('button', { type: 'button', class: 'danger', disabled, text: 'Deny', onclick: () => send('Denied.') })]
          : h('button', { type: 'button', class: 'primary', disabled, text: 'Answer', onclick: () => send(null) })),
      disabled ? h('p', { class: 'hint', text: 'This token lacks the run scope.' }) : null);
  }

  function approvalForm(a) {
    const id = nextId('approval');
    const area = h('textarea', { id, rows: '2', 'data-draft': `approval:${a.id}`, placeholder: 'Optional reason' });
    const disabled = !can('run');
    const send = async (allow) => {
      if (!allow || a.about.kind === 'promote') {
        const ok = await confirmDialog(allow ? 'Perform it?' : 'Deny it?',
          allow ? `This performs ${aboutText(a.about)} for real.` : `${a.branch} is refused ${aboutText(a.about)}.`,
          allow ? 'Perform' : 'Deny', !allow);
        if (!ok) return;
      }
      const reason = area.value.trim();
      try {
        await api('POST', `${repoPath()}/approvals/${enc(a.id)}/${allow ? 'allow' : 'deny'}`,
          { surface: 'companion', ...(reason ? { reason } : {}) });
        toast(`${allow ? 'Allowed' : 'Denied'}: ${a.branch} may${allow ? '' : ' not'} ${aboutText(a.about)}.`);
        loadApprovals();
      } catch (e) {
        toast(`Could not answer the approval: ${e.message}`);
      }
    };
    return h('div', { class: 'reply' },
      h('label', { class: 'block', for: id, text: 'Reason' }), area,
      h('div', { class: 'row' },
        h('button', { type: 'button', class: 'primary', disabled, text: 'Allow', onclick: () => send(true) }),
        h('button', { type: 'button', class: 'danger', disabled, text: 'Deny', onclick: () => send(false) })),
      disabled ? h('p', { class: 'hint', text: 'This token lacks the run scope.' }) : null);
  }

  function attentionSignature() {
    const needs = lanes(Array.from(state.branches.values()))[0].branches;
    return JSON.stringify([state.approvals.map((a) => a.id), state.inbox.map((m) => m.id), state.permissions,
      state.effects.map((e) => [e.updated_ms, e.state]), needs.map((b) => [b.name, b.status, b.stalled])]);
  }

  function renderAttention() {
    // Keep what was being typed, and where, across a redraw.
    const drafts = new Map(Array.from(document.querySelectorAll('#main [data-draft]'), (el) => [el.dataset.draft, el.value]));
    const active = document.activeElement && document.activeElement.dataset ? document.activeElement : null;
    const focused = active && active.dataset.draft ? [active.dataset.draft, active.selectionStart, active.selectionEnd] : null;
    const waiting = attention();
    const stopped = lanes(Array.from(state.branches.values()))[0].branches.filter((b) => !waiting.has(b.name));
    const perms = state.permissions;
    const total = state.approvals.length + state.inbox.length + stopped.length;
    const section = (title, items) => h('section', { class: 'attention-group' }, h('h2', { text: title }), h('ul', { class: 'cards' }, items));
    main(h('div', { class: 'page' },
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'Needs you' }),
        h('p', { class: 'lede', text: !state.repo ? 'This token reaches no repository. A workspace shows what it waits for in its own project.'
          : total ? `${plural(total, 'thing')} ${total === 1 ? 'waits' : 'wait'} for you in ${state.repo}.` : `Nothing in ${state.repo} waits for you.` }))),
      state.approvals.length ? section('Waiting for approval', state.approvals.map((a) => h('li', { class: 'card' },
        h('div', { class: 'card-head' }, dot('needs'), h('a', { class: 'card-name', href: branchHref(a.branch), text: a.branch }),
          h('span', { class: 'tag', text: a.resolved ? a.resolved.layer : 'ask' })),
        h('p', { class: 'card-text', text: `May it ${aboutText(a.about)}?` }),
        a.request ? h('details', { class: 'request' }, h('summary', { text: 'The call\'s arguments' }),
          h('pre', { class: 'code', text: JSON.stringify(a.request, null, 2).slice(0, 2000) })) : null,
        h('p', { class: 'item-meta' }, `turn ${a.turn} · `, ago(a.created_ms), a.deadline_ms ? ` · waits until ${clock(a.deadline_ms)}` : null),
        approvalForm(a)))) : null,
      state.inbox.length ? section('Questions and escalations', state.inbox.map((m) => h('li', { class: 'card' },
        h('div', { class: 'card-head' }, dot('needs'), h('span', { class: 'card-name', text: `${m.from} → ${m.to}` }), h('span', { class: 'tag', text: m.kind })),
        h('p', { class: 'card-text prompt', text: m.text }),
        h('p', { class: 'item-meta' }, ago(m.at_ms)),
        answerForm(m)))) : null,
      stopped.length ? section('Stopped branches', stopped.map((b) => h('li', {}, h('a', { class: 'card card-link', href: branchHref(b.name) },
        h('span', { class: 'card-head' }, dot('needs'), h('span', { class: 'card-name', text: b.name }), ago(activeAt(b))),
        h('span', { class: 'card-text', text: b.stalled ? 'Running, but stalled: no harness activity' : cap(statusOf(b).text) }))))) : null,
      total ? null : h('p', { class: 'calm' }, checkMark(false), 'Approvals, questions and branches that stop for you appear here.'),
      h('details', { class: 'attention-group ledger' },
        h('summary', {}, h('h2', { text: `Permission requests (${perms.length})` })),
        h('p', { class: 'hint', text: 'A server decides each tool request at once by the policy its task or send was given; there is nothing to approve afterwards. Send again with "Allow every tool" to let a branch use what was denied.' }),
        perms.length
          ? h('ul', { class: 'events' }, perms.map((p) => h('li', {}, h('time', { text: clock(p.at_ms) }),
            h('a', { href: branchHref(p.branch), text: p.branch }), ` asked to use ${p.tool}${p.decision ? `: ${p.decision}` : ''}`)))
          : h('p', { class: 'muted', text: 'None seen since this page opened.' })),
      h('details', { class: 'attention-group ledger' },
        h('summary', {}, h('h2', { text: `Effect ledger (${state.effects.length})` })),
        h('p', { class: 'hint', text: 'What branches did outside the machine. by undo plans and performs inverses where the upstream allows.' }),
        state.effects.length
          ? h('ul', { class: 'events' }, state.effects.map((e) => h('li', {}, h('time', { text: clock(e.updated_ms) }),
            h('a', { href: branchHref(e.branch), text: e.branch }),
            ` ${e.connector} ${e.operation}: ${e.state} (${e.class})${e.undo ? ` · undo ${e.undo.operation}` : (e.undo_unavailable ? ` · no undo: ${e.undo_unavailable}` : '')}`)))
          : h('p', { class: 'muted', text: 'No effects recorded.' }))));
    for (const el of document.querySelectorAll('#main [data-draft]')) {
      if (drafts.has(el.dataset.draft)) el.value = drafts.get(el.dataset.draft);
      if (focused && el.dataset.draft === focused[0]) { el.focus({ preventScroll: true }); el.setSelectionRange(focused[1], focused[2]); }
    }
  }

  // ---------------------------------------------------------------
  // Triggers

  async function renderTriggers() {
    const head = h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'Triggers' }),
      h('p', { class: 'lede', text: 'Tasks that start on a schedule or on an event.' })));
    main(h('div', { class: 'page' }, h('a', { class: 'back', href: '#/', text: '‹ Branches' }), head, h('p', { class: 'muted', text: 'Loading…' })));
    let triggers;
    try { ({ triggers } = await api('GET', '/v1/triggers')); } catch (e) {
      main(h('div', { class: 'page' }, h('a', { class: 'back', href: '#/', text: '‹ Branches' }), head, h('p', { class: 'error', text: e.message })));
      return;
    }
    const whenText = (w) => w.kind === 'cron' ? `cron ${w.expr}${w.timezone && w.timezone !== 'UTC' ? ` (${w.timezone})` : ''}`
      : w.kind === 'interval' ? `every ${w.seconds}s` : `on ${w.source} events`;
    main(h('div', { class: 'page' },
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      head,
      triggers.length ? h('ul', { class: 'list' }, triggers.map((t) => {
        const box = h('input', { type: 'checkbox', id: nextId('trigger'), checked: t.enabled, disabled: !can('run') });
        box.addEventListener('change', async () => {
          box.disabled = true;
          try {
            await api('POST', `/v1/triggers/${enc(t.id)}/${box.checked ? 'enable' : 'disable'}`, {});
            toast(`${t.name} ${box.checked ? 'enabled' : 'disabled'}.`);
            renderTriggers();
          } catch (e) {
            box.checked = !box.checked;
            toast(`Could not change ${t.name}: ${e.message}`);
            box.disabled = false;
          }
        });
        return h('li', { class: 'item' },
          h('div', { class: 'item-head' }, dot(t.enabled ? 'ready' : 'done'), h('span', { class: 'item-name', text: t.name }), h('span', { class: 'tag', text: t.enabled ? 'enabled' : 'disabled' })),
          h('div', { class: 'item-meta', text: [t.repo, whenText(t.when), t.next_due_ms ? `next ${new Date(t.next_due_ms).toLocaleString()}` : null].filter(Boolean).join(' · ') }),
          t.paused_reason ? h('div', { class: 'item-meta', text: t.paused_reason }) : null,
          h('label', { class: 'toggle', for: box.id }, box, h('span', { text: 'Enabled' })));
      })) : h('p', { class: 'muted', text: 'No triggers. by trigger add makes one.' })));
  }

  // ---------------------------------------------------------------
  // Queue and stats

  async function renderQueue() {
    const all = Array.from(state.branches.values());
    const cost = all.reduce((s, b) => s + (b.cost_usd || 0), 0);
    const turns = all.reduce((s, b) => s + (b.turns || 0), 0);
    const opsSlot = h('div', {}, h('p', { class: 'muted', text: 'Loading…' }));
    const stat = (label, value, lane) => h('div', { class: 'stat' }, h('span', { class: 'glance-label' }, lane ? dot(lane) : null, label), h('b', { text: value }));
    main(h('div', { class: 'page' },
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'Queue and stats' }),
        h('p', { class: 'lede', text: state.repo ? `Work queued and done in ${state.repo}.` : 'Work queued and done in your repositories.' }))),
      h('div', { class: 'stats' },
        stat('branches', String(all.length)),
        lanes(all).map(({ lane, branches }) => stat(lane.label.toLowerCase(), String(branches.length), branches.length ? lane.key : 'done')),
        stat('turns', String(turns)),
        stat('cost reported', money(cost))),
      h('h2', { text: 'Queued and running operations' }), opsSlot));
    if (!state.repo) { fill(opsSlot, h('p', { class: 'muted', text: 'This token reaches no repository.' })); return; }
    try {
      const { operations } = await api('GET', `${repoPath()}/operations`);
      fill(opsSlot, operations.length
        ? h('ul', { class: 'list' }, operations.map(opItem))
        : h('p', { class: 'muted', text: 'Nothing queued or running.' }));
    } catch (e) {
      fill(opsSlot, h('p', { class: 'error', text: e.message }));
    }
  }

  // ---------------------------------------------------------------
  // Settings: who this is, notifications, signing out

  function b64ToBytes(text) {
    const pad = '='.repeat((4 - (text.length % 4)) % 4);
    const raw = atob((text + pad).replace(/-/g, '+').replace(/_/g, '/'));
    return Uint8Array.from(raw, (c) => c.charCodeAt(0));
  }

  async function pushRegistration() {
    if (!('serviceWorker' in navigator)) return null;
    return navigator.serviceWorker.register('sw.js', { scope: './' });
  }

  async function renderSettings() {
    const me = state.me;
    const status = h('p', { role: 'status' });
    const pushSlot = h('div', {}, h('p', { class: 'muted', text: 'Checking…' }));
    main(h('div', { class: 'page narrow' },
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'Settings' }))),
      h('section', { class: 'card', 'aria-label': 'This token' },
        h('dl', { class: 'facts' },
          h('dt', { text: 'Name' }), h('dd', { text: me.name }),
          h('dt', { text: 'Tenant' }), h('dd', { text: me.tenant }),
          h('dt', { text: 'Scopes' }), h('dd', { text: me.scopes.join(', ') || 'none' }),
          h('dt', { text: 'Repositories' }), h('dd', { text: (me.repos || state.repos).join(', ') }),
          h('dt', { text: 'Kind' }), h('dd', { text: me.kind === 'paired' ? 'paired from a link' : 'from the server\'s configuration' }),
          me.expires_at_ms ? [h('dt', { text: 'Expires' }), h('dd', { text: new Date(me.expires_at_ms).toLocaleString() })] : null)),
      h('h2', { text: 'Notifications' }), h('div', { class: 'card' }, pushSlot, status),
      h('h2', { text: 'Sign out' }),
      h('div', { class: 'card' },
        h('p', { class: 'hint', text: 'Forgets the token in this tab. To stop the token working everywhere, revoke it on the server: by serve token revoke NAME.' }),
        h('div', { class: 'row form-actions' }, h('button', { type: 'button', class: 'danger', text: 'Sign out', onclick: () => signOut('Signed out.') })))));
    let info;
    try { info = await api('GET', '/v1/app/push'); } catch (e) {
      fill(pushSlot, h('p', { class: 'error', text: e.message }));
      return;
    }
    state.pushInfo = info;
    const supported = 'serviceWorker' in navigator && 'PushManager' in window && window.isSecureContext;
    const pageNote = h('p', { class: 'hint', text: 'While this page is open, a branch that needs you moves to the top of the list and the tab\'s title counts them. When the tab is hidden, and the browser allows it, a system notification says why.' });
    if (!info.enabled || !supported) {
      fill(pushSlot, pageNote, h('p', { class: 'hint', text: !info.enabled
        ? 'This server does not send push notifications.'
        : 'This browser cannot receive push notifications here (it needs HTTPS, or localhost, and push support).' }));
      return;
    }
    let subscription = null;
    try {
      const reg = await pushRegistration();
      await navigator.serviceWorker.ready;
      subscription = reg ? await reg.pushManager.getSubscription() : null;
    } catch (_) { subscription = null; }
    const on = !!subscription && info.subscriptions.includes(subscription.endpoint);
    state.pushOn = on;
    const toggle = h('button', {
      type: 'button', class: on ? '' : 'primary', text: on ? 'Turn off on this device' : 'Notify this device',
      onclick: async () => {
        toggle.disabled = true;
        try {
          const reg = await pushRegistration();
          await navigator.serviceWorker.ready;
          if (on) {
            const sub = await reg.pushManager.getSubscription();
            if (sub) {
              await api('DELETE', '/v1/app/push/subscriptions', { endpoint: sub.endpoint });
              await sub.unsubscribe();
            }
            status.textContent = 'Push notifications are off on this device.';
          } else {
            const permission = await Notification.requestPermission();
            if (permission !== 'granted') throw new Error('notifications were not allowed');
            let sub = await reg.pushManager.getSubscription();
            if (!sub) sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: b64ToBytes(info.public_key) });
            const json = sub.toJSON();
            await api('POST', '/v1/app/push/subscriptions', { endpoint: json.endpoint, keys: json.keys });
            status.textContent = 'This device will be notified.';
          }
        } catch (e) {
          status.textContent = `Could not change notifications: ${e.message}`;
        }
        renderSettings();
      },
    });
    const test = h('button', {
      type: 'button', text: 'Send a test', disabled: !on,
      onclick: async () => {
        try {
          const r = await api('POST', '/v1/app/push/test', {});
          status.textContent = r.delivered ? 'Sent: it should arrive in a moment.' : `Not sent: ${(r.failures || []).join('; ') || 'no subscription'}`;
        } catch (e) { status.textContent = e.message; }
      },
    });
    fill(pushSlot, pageNote,
      h('p', { text: on ? 'This device gets push notifications for the repositories this token can read.' : 'Get a push notification on this device even when the page is closed.' }),
      h('div', { class: 'row' }, toggle, test));
  }

  // ---------------------------------------------------------------
  // Start

  function onKey(ev) {
    if (ev.defaultPrevented || ev.metaKey || ev.ctrlKey || ev.altKey || !state.me || $('confirm').open) return;
    if (ev.target.closest('input, textarea, select, [contenteditable]')) return;
    const search = $('branch-filter');
    if (ev.key === '/' && search.offsetParent) { ev.preventDefault(); search.focus(); search.select(); }
    else if (ev.key === 'j' || ev.key === 'k') { ev.preventDefault(); stepBranch(ev.key === 'j' ? 1 : -1); }
  }

  async function boot() {
    $('repo').addEventListener('change', (ev) => pickSource(ev.target.value));
    $('branch-filter').addEventListener('input', (ev) => {
      state.filter = ev.target.value;
      renderLanes();
    });
    document.addEventListener('keydown', onKey);
    document.addEventListener('click', (ev) => {
      for (const menu of document.querySelectorAll('details.menu[open]')) if (!menu.contains(ev.target)) menu.open = false;
    });
    document.addEventListener('keydown', (ev) => {
      if (ev.key !== 'Escape') return;
      for (const menu of document.querySelectorAll('details.menu[open]')) { menu.open = false; menu.querySelector('summary').focus(); }
    });
    setInterval(() => {
      for (const t of document.querySelectorAll('time[data-at]')) t.textContent = since(Number(t.dataset.at)) + t.dataset.suffix;
    }, 15000);
    window.addEventListener('hashchange', () => {
      const m = /^#pair=([0-9a-zA-Z]+)$/.exec(location.hash);
      if (m) { pair(m[1]).then((ok) => ok && start().catch((e) => renderSignIn(e.message))); return; }
      route();
    });
    if ('serviceWorker' in navigator && window.isSecureContext) {
      navigator.serviceWorker.register('sw.js', { scope: './' }).catch(() => {});
    }
    const paired = /^#pair=([0-9a-zA-Z]+)$/.exec(location.hash);
    if (paired && !(await pair(paired[1]))) return;
    state.token = state.token || load(TOKEN_KEY);
    if (!state.token) { setLive('off', 'signed out'); renderSignIn(); return; }
    try {
      await start();
    } catch (e) {
      if (e.status === 401) { signOut('This token is no longer accepted (expired or revoked). Pair again.'); return; }
      setLive('off', 'offline');
      document.body.dataset.view = 'signin';
      main(h('div', { class: 'signin' }, h('h1', { text: 'Cannot reach the server' }), h('p', { class: 'error', text: e.message }),
        h('div', { class: 'row' }, h('button', { type: 'button', class: 'primary', text: 'Try again', onclick: () => location.reload() }))));
      focusHeading();
    }
  }

  boot();
})();
