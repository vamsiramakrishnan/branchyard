// The Branchyard companion: one page over the server's own HTTP API and
// event stream (docs/companion.md). Every action is an API call made with
// the bearer token this tab holds, under that token's scopes; the page
// adds no authority of its own. Text from the server is only ever set as
// text (textContent), never parsed as HTML.
'use strict';

(() => {
  const TOKEN_KEY = 'branchyard.token';
  const REPO_KEY = 'branchyard.repo';
  // What `R` in `by watch` sends to an interrupted branch.
  const RESUME_PROMPT = 'Your previous turn was interrupted before it finished. ' +
    'Continue the task from where you left off, then summarize what you did.';
  const DIFF_LIMIT = 400000;

  const state = {
    token: null,
    me: null,
    repos: [],
    repo: null,
    branches: new Map(),
    cursor: null,
    stream: null,
    backoff: 1000,
    loaded: false,
    seen: new Set(),
    statusSeen: new Map(),
    permissions: [],
    inbox: [],
    inboxStamp: 0,
    approvals: [],
    effects: [],
    route: { name: 'branches' },
    pushInfo: null,
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
  let uid = 0;
  const nextId = (prefix) => `${prefix}-${++uid}`;

  function main(...children) {
    const m = $('main');
    m.replaceChildren(...children.flat(Infinity).filter((c) => c != null && c !== false));
  }

  function focusHeading() {
    const heading = $('main').querySelector('h1');
    if (heading) {
      heading.setAttribute('tabindex', '-1');
      heading.focus({ preventScroll: false });
    }
  }

  function toast(text, href) {
    const box = h('div', { class: 'toast' }, text, href ? [' ', h('a', { href, text: 'Open' })] : null);
    const toasts = $('toasts');
    toasts.append(box);
    while (toasts.children.length > 3) toasts.firstChild.remove();
    setTimeout(() => box.remove(), 8000);
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
    ok.className = danger ? 'danger' : '';
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
    constructor(status, code, message) {
      super(message);
      this.status = status;
      this.code = code;
    }
  }

  function key() {
    if (window.crypto && crypto.randomUUID) return crypto.randomUUID();
    const bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  }

  async function api(method, path, body) {
    const headers = { Accept: 'application/json' };
    if (state.token) headers.Authorization = `Bearer ${state.token}`;
    const init = { method, headers, cache: 'no-store', credentials: 'omit' };
    if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
      if (method === 'POST') headers['Idempotency-Key'] = key();
    }
    let response;
    try {
      response = await fetch(path, init);
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
      throw new ApiError(response.status, err.code || 'error', err.message || `HTTP ${response.status}`);
    }
    return json;
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

  function badge(info) {
    const s = statusOf(info);
    return h('span', { class: `badge s-${s.key}`, text: s.text.length > 60 ? `${s.text.slice(0, 59)}…` : s.text });
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

  function money(v) {
    return v == null ? '—' : `$${Number(v).toFixed(v < 1 ? 4 : 2)}`;
  }

  function ago(ms) {
    const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
    if (s < 60) return `${s}s ago`;
    if (s < 3600) return `${Math.round(s / 60)}m ago`;
    if (s < 86400) return `${Math.round(s / 3600)}h ago`;
    return `${Math.round(s / 86400)}d ago`;
  }

  function clock(ms) {
    const d = new Date(ms);
    return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  }

  // ---------------------------------------------------------------
  // What deserves a notice: `by watch`'s rules, each once

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
    const href = kind === 'question' ? '#/inbox' : kind === 'approval' ? '#/approvals' : `#/b/${enc(state.repo)}/${enc(target)}`;
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
    $('nav').hidden = true;
    $('repo-pick').hidden = true;
    setLive('off', 'signed out');
    renderSignIn(message);
  }

  function renderSignIn(message) {
    const id = nextId('token');
    const error = h('p', { class: 'error', role: 'alert', text: message || '' });
    const input = h('input', { id, type: 'password', autocomplete: 'off', spellcheck: 'false', required: true });
    const submit = async () => {
      const token = input.value.trim();
      if (!token) return;
      state.token = token;
      try {
        await start();
        save(TOKEN_KEY, token);
      } catch (e) {
        state.token = null;
        error.textContent = e.status === 401 ? 'That token is not accepted.' : e.message;
      }
    };
    input.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') submit(); });
    main(
      h('h1', { text: 'Sign in' }),
      h('p', {}, 'Open a pairing link from ', h('code', { text: 'by serve token new --link' }),
        ' on this device, or paste a token. It is kept for this tab only and sent only to this server.'),
      h('div', { class: 'panel' },
        h('label', { class: 'block', for: id, text: 'Token' }), input,
        h('div', { class: 'row' }, h('button', { type: 'button', onclick: submit, text: 'Sign in' }))),
      error,
    );
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
    main(h('h1', { text: 'Pairing…' }));
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

  async function start() {
    state.me = await api('GET', '/v1/app/me');
    const { repos } = await api('GET', '/v1/repos');
    state.repos = repos.map((r) => r.name);
    if (!state.repos.length) throw new ApiError(403, 'no_repos', 'This token can reach no repository.');
    const remembered = load(REPO_KEY);
    state.repo = state.repos.includes(remembered) ? remembered : state.repos[0];
    const select = $('repo');
    select.replaceChildren(...state.repos.map((r) => h('option', { value: r, text: r, selected: r === state.repo })));
    $('repo-pick').hidden = state.repos.length < 2;
    $('nav').hidden = false;
    await switchRepo(state.repo);
    route();
  }

  async function switchRepo(repo) {
    state.repo = repo;
    save(REPO_KEY, repo);
    state.branches = new Map();
    state.cursor = null;
    state.loaded = false;
    state.permissions = [];
    state.inbox = [];
    state.approvals = [];
    state.effects = [];
    if (state.stream) state.stream.abort();
    openStream();
    await loadBranches();
  }

  async function loadBranches() {
    const { branches } = await api('GET', `${repoPath()}/branches`);
    state.branches = new Map(branches.map((b) => [b.name, b]));
    state.loaded = true;
    refreshInbox();
    refreshApprovals();
    if (['branches', 'queue'].includes(state.route.name)) render();
    if (state.route.name === 'branch') updateBranchHeader();
  }

  const pending = new Map();
  function refreshBranch(name) {
    if (pending.has(name)) return;
    pending.set(name, setTimeout(async () => {
      pending.delete(name);
      try {
        const info = await api('GET', branchPath(name));
        state.branches.set(name, info);
      } catch (e) {
        if (e.status === 404) state.branches.delete(name);
      }
      if (state.route.name === 'branches') render();
      if (state.route.name === 'branch' && state.route.branch === name) updateBranchHeader();
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
      if (n) {
        toast(n.text, n.href);
        if (document.hidden && 'Notification' in window && Notification.permission === 'granted' && !state.pushOn) {
          try { new Notification(`${state.repo}: ${n.text}`, { tag: n.href }); } catch (_) { /* page notifications unavailable */ }
        }
      }
    }
    refreshBranch(entry.branch);
    if (state.route.name === 'branch' && state.route.branch === entry.branch) appendEvent(entry);
    if (state.route.name === 'inbox') renderInbox();
    if (state.route.name === 'approvals' && a.effect) renderApprovals();
  }

  // ---------------------------------------------------------------
  // Routing

  function parseRoute() {
    const hash = location.hash || '#/';
    const parts = hash.replace(/^#\/?/, '').split('/').map((p) => { try { return decodeURIComponent(p); } catch (_) { return p; } });
    switch (parts[0]) {
      case '': return { name: 'branches' };
      case 'b': return { name: 'branch', repo: parts[1], branch: parts.slice(2).join('/') };
      case 'inbox': return { name: 'inbox' };
      case 'approvals': return { name: 'approvals' };
      case 'triggers': return { name: 'triggers' };
      case 'queue': return { name: 'queue' };
      case 'settings': return { name: 'settings' };
      case 'new': return { name: 'new' };
      default: return { name: 'branches' };
    }
  }

  async function route() {
    if (!state.token) return;
    state.route = parseRoute();
    if (state.route.name === 'branch' && state.route.repo && state.route.repo !== state.repo && state.repos.includes(state.route.repo)) {
      $('repo').value = state.route.repo;
      await switchRepo(state.route.repo);
    }
    const section = state.route.name === 'branch' || state.route.name === 'new' ? 'branches' : state.route.name;
    for (const link of document.querySelectorAll('#nav a')) {
      if (link.dataset.route === section) link.setAttribute('aria-current', 'page');
      else link.removeAttribute('aria-current');
    }
    await render();
    focusHeading();
  }

  async function render() {
    switch (state.route.name) {
      case 'branch': return renderBranch();
      case 'inbox': return renderInbox();
      case 'approvals': return renderApprovals();
      case 'triggers': return renderTriggers();
      case 'queue': return renderQueue();
      case 'settings': return renderSettings();
      case 'new': return renderNew();
      default: return renderBranches();
    }
  }

  // ---------------------------------------------------------------
  // Branches

  let filterText = '';
  function renderBranches() {
    const all = Array.from(state.branches.values());
    const rank = (b) => ({ running: 0, waiting: 1 }[b.status.state] ?? 2);
    all.sort((x, y) => rank(x) - rank(y) || y.created_at - x.created_at);
    const shown = all.filter((b) => !filterText || b.name.includes(filterText) || (b.prompt || '').toLowerCase().includes(filterText.toLowerCase()));
    const searchId = nextId('filter');
    const search = h('input', { id: searchId, type: 'search', value: filterText, placeholder: 'Filter by name or prompt' });
    search.addEventListener('input', () => {
      filterText = search.value;
      const list = $('branch-list');
      if (list) list.replaceChildren(...branchItems(all.filter((b) => !filterText || b.name.includes(filterText) || (b.prompt || '').toLowerCase().includes(filterText.toLowerCase()))));
    });
    const focused = document.activeElement && document.activeElement.type === 'search';
    main(
      h('h1', { text: `Branches in ${state.repo}` }),
      h('div', { class: 'toolbar' },
        h('label', { for: searchId, class: 'visually-hidden', text: 'Filter branches' }), search,
        can('run') ? h('a', { class: 'button', href: '#/new', text: 'New task' }) : null),
      state.loaded ? null : h('p', { class: 'muted', text: 'Loading…' }),
      state.loaded && !all.length ? h('p', { class: 'muted', text: 'No branches yet.' }) : null,
      h('ul', { class: 'list', id: 'branch-list', 'aria-label': 'Branches' }, branchItems(shown)),
    );
    if (focused) { search.focus(); search.setSelectionRange(search.value.length, search.value.length); }
  }

  function branchItems(list) {
    return list.map((b) => {
      const c = b.candidate;
      const meta = [
        b.harness,
        `${b.turns} turn${b.turns === 1 ? '' : 's'}`,
        b.cost_usd != null ? money(b.cost_usd) : null,
        c ? `${c.files_changed} file${c.files_changed === 1 ? '' : 's'} +${c.insertions} −${c.deletions}` : null,
        b.parent ? `from ${b.parent}` : null,
      ].filter(Boolean).join(' · ');
      return h('li', {},
        h('a', { href: `#/b/${enc(state.repo)}/${enc(b.name)}` },
          h('div', { class: 'item-head' }, h('span', { class: 'item-name', text: b.name }), badge(b)),
          h('div', { class: 'item-meta', text: meta }),
          b.prompt ? h('div', { class: 'item-meta', text: b.prompt.length > 140 ? `${b.prompt.slice(0, 139)}…` : b.prompt }) : null));
    });
  }

  // ---------------------------------------------------------------
  // One branch

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

  function appendEvent(entry) {
    const list = $('events');
    if (!list) return;
    if (entry.activity.harness && entry.activity.harness.type === 'message_delta') return;
    list.prepend(eventItem(entry));
    while (list.children.length > 200) list.lastChild.remove();
  }

  function updateBranchHeader() {
    const info = state.branches.get(state.route.branch);
    const slot = $('branch-head');
    if (!info || !slot) return;
    slot.replaceChildren(...branchHead(info));
    // Rebuild the forms only when what they allow changed, so text being
    // typed into one survives the branch's activity.
    const actions = $('actions');
    const signature = `${info.status.state}|${info.candidate ? info.candidate.commit : ''}`;
    if (actions && actions.dataset.signature !== signature) {
      actions.dataset.signature = signature;
      actions.replaceChildren(...actionForms(info));
    }
  }

  function branchHead(info) {
    const c = info.candidate;
    const facts = [
      ['Status', badge(info)],
      ['Harness', `${info.harness}${info.profile && info.profile !== info.harness ? ` (${info.profile})` : ''}`],
      ['Turns', String(info.turns)],
      ['Cost', money(info.cost_usd)],
      ['Started', new Date(info.created_at * 1000).toLocaleString()],
      ['Candidate', c ? h('span', {}, h('span', { class: 'mono', text: c.commit.slice(0, 12) }), ` · ${c.files_changed} files +${c.insertions} −${c.deletions}`) : 'none yet'],
      info.parent ? ['From', h('a', { href: `#/b/${enc(state.repo)}/${enc(info.parent)}`, text: info.parent })] : null,
      info.children && info.children.length ? ['Delegated to', info.children.map((ch, i) => [i ? ', ' : '', h('a', { href: `#/b/${enc(state.repo)}/${enc(ch)}`, text: ch })])] : null,
      info.superseded_by ? ['Superseded by', h('a', { href: `#/b/${enc(state.repo)}/${enc(info.superseded_by)}`, text: info.superseded_by })] : null,
    ].filter(Boolean);
    const readiness = when.merge(info);
    return [
      h('dl', { class: 'facts' }, facts.map(([k, v]) => [h('dt', { text: k }), h('dd', {}, v)])),
      h('p', { class: readiness ? 'hint' : '' },
        readiness ? `Not ready to merge: ${readiness}.` : 'Ready to merge: its check runs on the merge result first, and nothing changes unless it passes.'),
    ];
  }

  async function act(label, run, after) {
    try {
      const result = await run();
      if (result && result.id && result.state) {
        toast(`${label}: operation ${result.id} ${result.state}.`);
        followOperation(result.id, label);
      } else {
        toast(`${label}: done.`);
      }
      if (after) after(result);
      refreshBranch(state.route.branch);
      return true;
    } catch (e) {
      toast(`${label} refused: ${e.message}`);
      return false;
    }
  }

  async function followOperation(id, label) {
    for (let i = 0; i < 600; i++) {
      await new Promise((r) => setTimeout(r, i < 10 ? 1000 : 3000));
      let op;
      try { op = await api('GET', `/v1/operations/${enc(id)}`); } catch (_) { continue; }
      if (['succeeded', 'failed', 'interrupted'].includes(op.state)) {
        toast(op.error ? `${label} ${op.state}: ${op.error.message}` : `${label} ${op.state}.`);
        return;
      }
    }
  }

  function textAction({ name, title, help, rule, info, scope, button, run, placeholder, allowAll }) {
    const why = !can(scope) ? `this token lacks the ${scope} scope` : rule(info);
    const id = nextId(name);
    const hintId = `${id}-hint`;
    const area = h('textarea', { id, placeholder: placeholder || '', disabled: !!why, 'aria-describedby': hintId });
    const allowId = `${id}-allow`;
    const allow = allowAll ? h('input', { type: 'checkbox', id: allowId, disabled: !!why }) : null;
    const go = h('button', {
      type: 'button', disabled: !!why, text: button,
      onclick: async () => {
        const text = area.value.trim();
        if (!text) { area.focus(); return; }
        go.disabled = true;
        const ok = await act(button, () => run(text, allow && allow.checked));
        go.disabled = false;
        if (ok) area.value = '';
      },
    });
    return h('div', { class: 'action' },
      h('label', { class: 'block', for: id, text: title }),
      area,
      allow ? h('div', { class: 'check' }, allow, h('label', { for: allowId, text: 'Allow every tool (like --yes); otherwise tool requests are denied' })) : null,
      h('div', { class: 'row' }, go),
      h('p', { class: 'hint', id: hintId, text: why ? `Not now: ${why}.` : help }));
  }

  function buttonAction({ name, help, rule, info, scope, button, danger, confirm, run }) {
    const why = !can(scope) ? `this token lacks the ${scope} scope` : rule(info);
    const hintId = nextId(`${name}-hint`);
    const go = h('button', {
      type: 'button', class: danger ? 'danger' : 'secondary', disabled: !!why, text: button, 'aria-describedby': hintId,
      onclick: async () => {
        if (confirm && !(await confirmDialog(`${button}?`, confirm, button, danger))) return;
        go.disabled = true;
        await act(button, run);
        go.disabled = false;
      },
    });
    return h('div', { class: 'action' }, h('div', { class: 'row' }, go),
      h('p', { class: 'hint', id: hintId, text: why ? `Not now: ${why}.` : help }));
  }

  function policy(allowAll) {
    return allowAll ? { mode: 'allow' } : undefined;
  }

  function actionForms(info) {
    const b = info.name;
    return [
      textAction({
        name: 'send', title: 'Send a follow-up prompt', help: 'Starts a new turn in the branch\'s session (by send).',
        rule: when.send, info, scope: 'run', button: 'Send', allowAll: true,
        run: (text, all) => api('POST', `${branchPath(b)}/send`, { prompt: text, policy: policy(all) }),
      }),
      textAction({
        name: 'steer', title: 'Steer the running turn', help: 'Adds input to the running turn without stopping it (by send --steer).',
        rule: when.steer, info, scope: 'run', button: 'Steer',
        run: (text) => api('POST', `${branchPath(b)}/steer`, { text }),
      }),
      buttonAction({
        name: 'resume', help: 'Resumes an interrupted branch in its own session.', rule: when.resume, info, scope: 'run', button: 'Resume',
        run: () => api('POST', `${branchPath(b)}/send`, { prompt: RESUME_PROMPT }),
      }),
      buttonAction({
        name: 'cancel', help: 'Stops the running turn and those of every branch it delegated to (by cancel).',
        rule: when.cancel, info, scope: 'run', button: 'Cancel turn', danger: true,
        confirm: `Cancel ${b}'s running turn, and those of every branch it delegated to?`,
        run: () => api('POST', `${branchPath(b)}/cancel`, {}),
      }),
      buttonAction({
        name: 'merge', help: 'Validated merge into the served repository\'s checked-out branch (by merge).',
        rule: when.merge, info, scope: 'merge', button: 'Merge',
        confirm: `Merge ${b} into the checked-out branch? Its check runs on the merge result first, and nothing changes unless it passes.`,
        run: () => api('POST', `${branchPath(b)}/merge`, {}),
      }),
      textAction({
        name: 'fork', title: 'Fork with a prompt', help: 'A new branch from this one\'s candidate and conversation (by fork).',
        rule: when.fork, info, scope: 'run', button: 'Fork', allowAll: true,
        run: (text, all) => api('POST', `${branchPath(b)}/fork`, { prompt: text, policy: policy(all) }),
      }),
    ];
  }

  function renderDiff(text, slot) {
    const shown = text.length > DIFF_LIMIT ? text.slice(0, DIFF_LIMIT) : text;
    const lines = shown.split('\n');
    const pre = h('pre', { class: 'diff', tabindex: '0', 'aria-label': 'Diff' });
    for (const line of lines) {
      let cls = null;
      if (line.startsWith('diff --git') || line.startsWith('+++ ') || line.startsWith('--- ')) cls = 'file';
      else if (line.startsWith('@@')) cls = 'hunk';
      else if (line.startsWith('+')) cls = 'add';
      else if (line.startsWith('-')) cls = 'del';
      pre.append(h('span', { class: cls }, line));
    }
    slot.replaceChildren(
      text.length > DIFF_LIMIT ? h('p', { class: 'hint', text: `Showing the first ${DIFF_LIMIT} characters of ${text.length}.` }) : null,
      text ? pre : h('p', { class: 'muted', text: 'The diff is empty.' }));
  }

  async function renderBranch() {
    const name = state.route.branch;
    let info = state.branches.get(name);
    if (!info) {
      try { info = await api('GET', branchPath(name)); state.branches.set(name, info); } catch (e) {
        main(h('h1', { text: name }), h('p', { class: 'error', text: e.message }), h('a', { href: '#/', text: 'All branches' }));
        return;
      }
    }
    const eventsList = h('ul', { class: 'events', id: 'events', 'aria-label': 'Recent events' }, h('li', { class: 'muted', text: 'Loading…' }));
    const checkpoints = h('div', {}, h('p', { class: 'muted', text: 'Loading…' }));
    const opsSlot = h('div', {});
    const diffSlot = h('div', {});
    const diffWhy = when.diff(info);
    const diffButton = h('button', {
      type: 'button', class: 'secondary', disabled: !!diffWhy, text: 'Show the diff',
      onclick: async () => {
        diffButton.disabled = true;
        try { renderDiff((await api('GET', `${branchPath(name)}/diff`)).diff || '', diffSlot); } catch (e) { diffSlot.replaceChildren(h('p', { class: 'error', text: e.message })); }
        diffButton.disabled = false;
      },
    });
    main(
      h('p', {}, h('a', { href: '#/', text: '← Branches' })),
      h('h1', { text: name }),
      h('section', { class: 'panel', 'aria-label': 'Summary', id: 'branch-head' }, branchHead(info)),
      h('h2', { text: 'Prompt' }),
      h('div', { class: 'panel' }, h('p', { class: 'prompt', text: info.prompt || '' })),
      h('h2', { text: 'Actions' }),
      h('section', { class: 'panel', id: 'actions', 'aria-label': 'Actions', 'data-signature': `${info.status.state}|${info.candidate ? info.candidate.commit : ''}` }, actionForms(info)),
      h('h2', { text: 'Operations' }), opsSlot,
      h('h2', { text: 'Checkpoints' }), h('div', { class: 'panel' }, checkpoints),
      h('h2', { text: 'Diff' }),
      h('div', { class: 'row' }, diffButton, diffWhy ? h('span', { class: 'hint', text: diffWhy }) : null),
      diffSlot,
      h('h2', { text: 'Recent events' }), h('div', { class: 'panel' }, eventsList),
    );
    try {
      const page = await api('GET', `${branchPath(name)}/event-page?limit=200`);
      const events = coalesce(page.events);
      eventsList.replaceChildren(...events.slice().reverse().map(eventItem));
      if (!events.length) eventsList.replaceChildren(h('li', { class: 'muted', text: 'No events yet.' }));
      const cps = page.events.filter((e) => e.activity.checkpoint).map((e) => e.activity.checkpoint);
      checkpoints.replaceChildren(cps.length
        ? h('ul', { class: 'events' }, cps.reverse().map((cp) => h('li', {}, `turn ${cp.turn} · `, h('span', { class: 'mono', text: cp.commit.slice(0, 12) }), ` · ${cp.files_changed} files +${cp.insertions} −${cp.deletions}`)))
        : h('p', { class: 'muted', text: page.total > page.events.length ? 'None in the latest events.' : 'None yet: each finished turn leaves one.' }));
    } catch (e) {
      eventsList.replaceChildren(h('li', { class: 'error', text: e.message }));
      checkpoints.replaceChildren();
    }
    try {
      const { operations } = await api('GET', `${repoPath()}/operations?branch=${enc(name)}`);
      opsSlot.replaceChildren(operations.length
        ? h('ul', { class: 'list' }, operations.map(opItem))
        : h('p', { class: 'muted', text: 'Nothing queued or running for this branch.' }));
    } catch (e) {
      opsSlot.replaceChildren(h('p', { class: 'error', text: e.message }));
    }
  }

  function opItem(op) {
    return h('li', {}, h('div', { class: 'item' },
      h('div', { class: 'item-head' }, h('span', { class: 'item-name', text: `${op.kind} ${op.branches.join(', ')}` }), h('span', { class: `badge s-${op.state === 'running' ? 'running' : 'waiting'}`, text: op.state })),
      h('div', { class: 'item-meta', text: [`${op.id}`, `${ago(op.created_at_ms)}`, op.priority ? `priority ${op.priority}` : null, op.waiting].filter(Boolean).join(' · ') })));
  }

  // ---------------------------------------------------------------
  // A new task

  async function renderNew() {
    const promptId = nextId('prompt');
    const nameId = nextId('name');
    const harnessId = nextId('harness');
    const allowId = nextId('allow');
    const area = h('textarea', { id: promptId, required: true });
    const nameInput = h('input', { id: nameId, type: 'text', autocomplete: 'off', spellcheck: 'false' });
    const select = h('select', { id: harnessId }, h('option', { value: '', text: 'the server\'s default' }));
    const allow = h('input', { type: 'checkbox', id: allowId });
    const error = h('p', { class: 'error', role: 'alert' });
    const go = h('button', {
      type: 'button', text: 'Start', disabled: !can('run'),
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
          toast(`Task ${op.id} ${op.state}.`);
          followOperation(op.id, 'Task');
          location.hash = op.branches.length ? `#/b/${enc(state.repo)}/${enc(op.branches[0])}` : '#/';
        } catch (e) {
          error.textContent = e.message;
          go.disabled = false;
        }
      },
    });
    main(
      h('p', {}, h('a', { href: '#/', text: '← Branches' })),
      h('h1', { text: `New task in ${state.repo}` }),
      h('div', { class: 'panel' },
        h('label', { class: 'block', for: promptId, text: 'Prompt' }), area,
        h('label', { class: 'block', for: harnessId, text: 'Harness' }), select,
        h('label', { class: 'block', for: nameId, text: 'Branch name (optional)' }), nameInput,
        h('div', { class: 'check' }, allow, h('label', { for: allowId, text: 'Allow every tool (like --yes); otherwise tool requests are denied' })),
        h('div', { class: 'row' }, go),
        can('run') ? null : h('p', { class: 'hint', text: 'This token lacks the run scope.' })),
      error,
    );
    try {
      const { harnesses } = await api('GET', '/v1/harnesses');
      for (const hn of harnesses.filter((x) => x.available)) select.append(h('option', { value: hn.harness, text: hn.harness }));
    } catch (_) { /* the default harness still works */ }
  }

  // ---------------------------------------------------------------
  // Inbox: questions and escalations waiting for an answer, and the
  // permission requests seen while this page was open

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
    const count = $('inbox-count');
    count.hidden = !state.inbox.length;
    count.textContent = String(state.inbox.length);
    if (state.route.name === 'inbox') renderInbox();
  }

  function answerForm(m) {
    const id = nextId('answer');
    const area = h('textarea', { id, placeholder: m.kind === 'escalation' ? 'Optional note' : 'Your answer' });
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
        toast(`Answer refused: ${e.message}`);
      }
    };
    return h('div', { class: 'action' },
      h('label', { class: 'block', for: id, text: `Answer as ${m.to}` }), area,
      h('div', { class: 'row' },
        m.kind === 'escalation'
          ? [h('button', { type: 'button', disabled, text: 'Approve', onclick: () => send('Approved.') }),
            h('button', { type: 'button', class: 'danger', disabled, text: 'Deny', onclick: () => send('Denied.') })]
          : h('button', { type: 'button', disabled, text: 'Answer', onclick: () => send(null) })),
      disabled ? h('p', { class: 'hint', text: 'This token lacks the run scope.' }) : null);
  }

  function renderInbox() {
    const perms = state.permissions;
    main(
      h('h1', { text: 'Inbox' }),
      h('h2', { text: 'Questions and escalations' }),
      state.inbox.length
        ? h('ul', { class: 'list' }, state.inbox.map((m) => h('li', {}, h('div', { class: 'item' },
          h('div', { class: 'item-head' }, h('span', { class: 'item-name', text: `${m.from} → ${m.to}` }), h('span', { class: 'badge s-waiting', text: m.kind })),
          h('p', { class: 'prompt', text: m.text }),
          h('div', { class: 'item-meta', text: ago(m.at_ms) }),
          answerForm(m)))))
        : h('p', { class: 'muted', text: 'Nothing waiting for an answer.' }),
      h('h2', { text: 'Permission requests' }),
      h('p', { class: 'hint', text: 'A server decides each tool request at once by the policy its task or send was given; there is nothing to approve afterwards. Send again with "Allow every tool" to let a branch use what was denied.' }),
      perms.length
        ? h('ul', { class: 'events' }, perms.map((p) => h('li', {}, h('time', { text: clock(p.at_ms) }),
          h('a', { href: `#/b/${enc(state.repo)}/${enc(p.branch)}`, text: p.branch }), ` asked to use ${p.tool}${p.decision ? `: ${p.decision}` : ''}`)))
        : h('p', { class: 'muted', text: 'None seen since this page opened.' }),
    );
  }

  // ---------------------------------------------------------------
  // Approvals: tools and connector calls a turn waits on, and staged
  // effects held until approved (docs/effects.md); and the effect ledger

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
    const count = $('approvals-count');
    count.hidden = !state.approvals.length;
    count.textContent = String(state.approvals.length);
    if (state.route.name === 'approvals') renderApprovals();
  }

  function aboutText(about) {
    switch (about.kind) {
      case 'tool': return `use the tool ${about.tool}`;
      case 'operation': return `${about.connector} ${about.operation} (${about.class}${about.deletion ? ', deletes' : ''})`;
      case 'promote': return `perform the staged ${about.connector} ${about.operation} (${about.class})`;
      default: return about.kind;
    }
  }

  function approvalForm(a) {
    const id = nextId('approval');
    const area = h('textarea', { id, placeholder: 'Optional reason' });
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
        toast(`${allow ? 'Allowed' : 'Denied'}: ${aboutText(a.about)}.`);
        loadApprovals();
      } catch (e) {
        toast(`Refused: ${e.message}`);
      }
    };
    return h('div', { class: 'action' },
      h('label', { class: 'block', for: id, text: 'Reason' }), area,
      h('div', { class: 'row' },
        h('button', { type: 'button', disabled, text: 'Allow', onclick: () => send(true) }),
        h('button', { type: 'button', class: 'danger', disabled, text: 'Deny', onclick: () => send(false) })),
      disabled ? h('p', { class: 'hint', text: 'This token lacks the run scope.' }) : null);
  }

  function renderApprovals() {
    const waiting = state.approvals;
    const ledger = state.effects;
    main(
      h('h1', { text: 'Approvals' }),
      waiting.length
        ? h('ul', { class: 'list' }, waiting.map((a) => h('li', {}, h('div', { class: 'item' },
          h('div', { class: 'item-head' },
            h('a', { class: 'item-name', href: `#/b/${enc(state.repo)}/${enc(a.branch)}`, text: a.branch }),
            h('span', { class: 'badge s-waiting', text: a.resolved ? a.resolved.layer : 'ask' })),
          h('p', { class: 'prompt', text: `May it ${aboutText(a.about)}?` }),
          a.request ? h('pre', { class: 'diff', text: JSON.stringify(a.request, null, 2).slice(0, 2000) }) : null,
          h('div', { class: 'item-meta', text: [`turn ${a.turn}`, ago(a.created_ms), a.deadline_ms ? `waits until ${clock(a.deadline_ms)}` : null].filter(Boolean).join(' · ') }),
          approvalForm(a)))))
        : h('p', { class: 'muted', text: 'Nothing waiting for approval.' }),
      h('h2', { text: 'Effect ledger' }),
      h('p', { class: 'hint', text: 'What branches did outside the machine. by undo plans and performs inverses where the upstream allows.' }),
      ledger.length
        ? h('ul', { class: 'events' }, ledger.map((e) => h('li', {}, h('time', { text: clock(e.updated_ms) }),
          h('a', { href: `#/b/${enc(state.repo)}/${enc(e.branch)}`, text: e.branch }),
          ` ${e.connector} ${e.operation}: ${e.state} (${e.class})${e.undo ? ` · undo ${e.undo.operation}` : (e.undo_unavailable ? ` · no undo: ${e.undo_unavailable}` : '')}`)))
        : h('p', { class: 'muted', text: 'No effects recorded.' }),
    );
  }

  // ---------------------------------------------------------------
  // Triggers

  async function renderTriggers() {
    main(h('h1', { text: 'Triggers' }), h('p', { class: 'muted', text: 'Loading…' }));
    let triggers;
    try { ({ triggers } = await api('GET', '/v1/triggers')); } catch (e) {
      main(h('h1', { text: 'Triggers' }), h('p', { class: 'error', text: e.message }));
      return;
    }
    const whenText = (w) => w.kind === 'cron' ? `cron ${w.expr}${w.timezone && w.timezone !== 'UTC' ? ` (${w.timezone})` : ''}`
      : w.kind === 'interval' ? `every ${w.seconds}s` : `on ${w.source} events`;
    main(
      h('h1', { text: 'Triggers' }),
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
            toast(`Refused: ${e.message}`);
            box.disabled = false;
          }
        });
        return h('li', {}, h('div', { class: 'item' },
          h('div', { class: 'item-head' }, h('span', { class: 'item-name', text: t.name }), h('span', { class: `badge s-${t.enabled ? 'ready' : 'no_changes'}`, text: t.enabled ? 'enabled' : 'disabled' })),
          h('div', { class: 'item-meta', text: [t.repo, whenText(t.when), t.next_due_ms ? `next ${new Date(t.next_due_ms).toLocaleString()}` : null].filter(Boolean).join(' · ') }),
          t.paused_reason ? h('div', { class: 'item-meta', text: t.paused_reason }) : null,
          h('div', { class: 'check' }, box, h('label', { for: box.id, text: 'Enabled' }))));
      })) : h('p', { class: 'muted', text: 'No triggers. by trigger add makes one.' }),
    );
  }

  // ---------------------------------------------------------------
  // Queue and stats

  async function renderQueue() {
    const all = Array.from(state.branches.values());
    const count = (pred) => all.filter(pred).length;
    const cost = all.reduce((s, b) => s + (b.cost_usd || 0), 0);
    const turns = all.reduce((s, b) => s + (b.turns || 0), 0);
    const opsSlot = h('div', {}, h('p', { class: 'muted', text: 'Loading…' }));
    const stat = (label, value) => h('div', { class: 'stat' }, h('b', { text: value }), label);
    main(
      h('h1', { text: `Queue and stats for ${state.repo}` }),
      h('div', { class: 'stats' },
        stat('branches', String(all.length)),
        stat('running', String(count((b) => b.status.state === 'running'))),
        stat('ready to merge', String(count((b) => b.status.state === 'ready'))),
        stat('failed or blocked', String(count((b) => ['failed', 'blocked'].includes(b.status.state)))),
        stat('turns', String(turns)),
        stat('cost reported', money(cost))),
      h('h2', { text: 'Queued and running operations' }), opsSlot,
    );
    try {
      const { operations } = await api('GET', `${repoPath()}/operations`);
      opsSlot.replaceChildren(operations.length
        ? h('ul', { class: 'list' }, operations.map(opItem))
        : h('p', { class: 'muted', text: 'Nothing queued or running.' }));
    } catch (e) {
      opsSlot.replaceChildren(h('p', { class: 'error', text: e.message }));
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
    main(
      h('h1', { text: 'Settings' }),
      h('section', { class: 'panel', 'aria-label': 'This token' },
        h('dl', { class: 'facts' },
          h('dt', { text: 'Name' }), h('dd', { text: me.name }),
          h('dt', { text: 'Tenant' }), h('dd', { text: me.tenant }),
          h('dt', { text: 'Scopes' }), h('dd', { text: me.scopes.join(', ') || 'none' }),
          h('dt', { text: 'Repositories' }), h('dd', { text: (me.repos || state.repos).join(', ') }),
          h('dt', { text: 'Kind' }), h('dd', { text: me.kind === 'paired' ? 'paired from a link' : 'from the server\'s configuration' }),
          me.expires_at_ms ? [h('dt', { text: 'Expires' }), h('dd', { text: new Date(me.expires_at_ms).toLocaleString() })] : null)),
      h('h2', { text: 'Notifications' }), pushSlot, status,
      h('h2', { text: 'Sign out' }),
      h('p', { class: 'hint', text: 'Forgets the token in this tab. To stop the token working everywhere, revoke it on the server: by serve token revoke NAME.' }),
      h('div', { class: 'row' }, h('button', { type: 'button', class: 'danger', text: 'Sign out', onclick: () => signOut('Signed out.') })),
    );
    let info;
    try { info = await api('GET', '/v1/app/push'); } catch (e) {
      pushSlot.replaceChildren(h('p', { class: 'error', text: e.message }));
      return;
    }
    state.pushInfo = info;
    const supported = 'serviceWorker' in navigator && 'PushManager' in window && window.isSecureContext;
    const pageNote = h('p', { class: 'hint', text: 'While this page is open it shows a notice for each permission request, question, stall, failure and finished turn.' });
    if (!info.enabled || !supported) {
      pushSlot.replaceChildren(pageNote, h('p', { class: 'hint', text: !info.enabled
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
      type: 'button', class: on ? 'secondary' : '', text: on ? 'Turn off on this device' : 'Notify this device',
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
      type: 'button', class: 'secondary', text: 'Send a test', disabled: !on,
      onclick: async () => {
        try {
          const r = await api('POST', '/v1/app/push/test', {});
          status.textContent = r.delivered ? 'Sent: it should arrive in a moment.' : `Not sent: ${(r.failures || []).join('; ') || 'no subscription'}`;
        } catch (e) { status.textContent = e.message; }
      },
    });
    pushSlot.replaceChildren(pageNote,
      h('p', { text: on ? 'This device gets push notifications for the repositories this token can read.' : 'Get a push notification on this device even when the page is closed.' }),
      h('div', { class: 'row' }, toggle, test));
  }

  // ---------------------------------------------------------------
  // Start

  async function boot() {
    $('repo').addEventListener('change', async (ev) => {
      await switchRepo(ev.target.value);
      location.hash = '#/';
      route();
    });
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
      main(h('h1', { text: 'Cannot reach the server' }), h('p', { class: 'error', text: e.message }),
        h('div', { class: 'row' }, h('button', { type: 'button', text: 'Try again', onclick: () => location.reload() })));
      focusHeading();
    }
  }

  boot();
})();
