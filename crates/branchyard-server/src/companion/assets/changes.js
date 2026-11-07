/**
 * changes.js
 * The console's Changes page: a branch's candidate diff against its base,
 * file by file, where a person selects lines, writes comments and sends them
 * to the branch as one review (docs/companion.md#changes-and-review).
 *
 * It reads `GET …/changes` (the diff as data) and `GET …/reviews` (what was
 * sent before, with the comments a later candidate addressed marked) once
 * each, and writes `POST …/reviews`. It polls nothing: the page is told when
 * the branch's candidate changes and offers to reload.
 *
 * Exposes window.BranchyardChanges with mount(container, options), and, for
 * the tests that run it under Node, the pure functions it draws from.
 */
(function (root) {
  'use strict';

  const SHORT = 7;
  const short = (sha) => (sha ? String(sha).slice(0, SHORT) : '');
  const plural = (n, one, many) => `${n} ${n === 1 ? one : many || `${one}s`}`;
  const clock = (ms) => new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  // Replace an element's children, skipping what is null or false, as h() does.
  function fill(el, ...children) {
    el.replaceChildren(...children.flat(Infinity).filter((c) => c != null && c !== false));
    return el;
  }
  const STATUS_WORDS = { added: 'added', deleted: 'deleted', renamed: 'renamed', modified: 'modified' };

  // ---------------------------------------------------------------
  // The model: a file's hunks as rows, a selection as lines on one side

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

  // The rows a file's diff is drawn as: a header row per hunk, then its
  // lines, each remembering its hunk and the side it can be commented on.
  function rowsOf(file) {
    const rows = [];
    (file.hunks || []).forEach((hunk, hunkIndex) => {
      rows.push({ type: 'hunk', hunk: hunkIndex, header: hunk.header });
      for (const line of hunk.lines) {
        if (line.kind === 'no_newline') { rows.push({ type: 'note', hunk: hunkIndex, text: line.text }); continue; }
        rows.push({
          type: 'line', hunk: hunkIndex, kind: line.kind, old: line.old == null ? null : line.old,
          new: line.new == null ? null : line.new, text: line.text,
        });
      }
    });
    return rows;
  }

  // A removed line is commented on the base's side; any other, the branch's.
  const sideOf = (row) => (row.kind === 'del' ? 'old' : 'new');
  const numberOf = (row, side) => (side === 'old' ? row.old : row.new);

  // The selection between two rows of one hunk, on the side of the first:
  // rows without a number on that side are skipped, the others make the range.
  function selectBetween(rows, from, to) {
    const a = rows[from];
    if (!a || a.type !== 'line') return null;
    const side = sideOf(a);
    const lo = Math.min(from, to);
    const hi = Math.max(from, to);
    const numbers = [];
    for (let i = lo; i <= hi; i++) {
      const r = rows[i];
      if (r.type !== 'line' || r.hunk !== a.hunk) continue;
      const n = numberOf(r, side);
      if (n != null) numbers.push(n);
    }
    if (!numbers.length) return null;
    return { side, start: Math.min(...numbers), end: Math.max(...numbers), hunk: a.hunk };
  }

  // The text of lines start..end on a side, in order.
  function quoteOf(rows, side, start, end) {
    const out = [];
    for (const r of rows) {
      if (r.type !== 'line') continue;
      const n = numberOf(r, side);
      if (n != null && n >= start && n <= end) out.push(r.text);
    }
    return out;
  }

  // Where a pending comment's quoted lines stand in a newly loaded file, or
  // null when they are not there any more.
  function reanchor(rows, comment) {
    const quote = comment.quote || [];
    if (!quote.length) return null;
    const lines = rows.filter((r) => r.type === 'line' && numberOf(r, comment.side) != null);
    for (let i = 0; i + quote.length <= lines.length; i++) {
      let same = true;
      for (let k = 0; k < quote.length && same; k++) {
        const here = lines[i + k];
        same = here.text === quote[k] && numberOf(here, comment.side) === numberOf(lines[i], comment.side) + k;
      }
      if (same) {
        const first = numberOf(lines[i], comment.side);
        return { start: first, end: first + quote.length - 1 };
      }
    }
    return null;
  }

  const rangeWord = (c) => (c.start_line != null && c.start_line !== c.line ? `lines ${c.start_line}–${c.line}` : c.line ? `line ${c.line}` : 'the file');

  // ---------------------------------------------------------------
  // The page

  function mount(container, opts) {
    const view = {
      changes: null, reviews: null, error: null, loading: true,
      selected: null, filter: '', viewed: new Set(), pending: [], nextId: 1,
      rows: [], cursor: null, sel: null, anchor: null, composer: null,
      stale: false, sending: false, note: '', delivery: 'next_turn', problem: null,
      destroyed: false, collapsed: new Set(),
    };
    const ui = {};
    const storeKey = `branchyard.review.${opts.repo}.${opts.branch}`;

    // ----- local state: what is viewed and the comments not yet sent -----
    function readStore() {
      try { return JSON.parse(localStorage.getItem(storeKey) || 'null'); } catch (_) { return null; }
    }
    function writeStore() {
      try {
        localStorage.setItem(storeKey, JSON.stringify({
          commit: view.changes && view.changes.commit, viewed: [...view.viewed], pending: view.pending, nextId: view.nextId, note: view.note,
        }));
      } catch (_) { /* private window or blocked storage: the page works without it */ }
    }

    // ----- loading -----
    function load() {
      container.dataset.ready = 'false';
      view.loading = true;
      view.error = null;
      renderAll();
      return Promise.all([
        opts.api('GET', `${opts.path}/changes`),
        opts.api('GET', `${opts.path}/reviews`),
      ]).then(([changes, reviews]) => {
        if (view.destroyed) return;
        const before = view.changes && view.changes.commit;
        view.changes = changes;
        view.reviews = reviews;
        view.loading = false;
        view.stale = false;
        const saved = readStore();
        if (!before && saved) {
          if (saved.commit === changes.commit) view.viewed = new Set(saved.viewed || []);
          view.pending = Array.isArray(saved.pending) ? saved.pending : [];
          view.nextId = saved.nextId || view.pending.length + 1;
          view.note = saved.note || '';
        } else if (before && before !== changes.commit) {
          view.viewed = new Set();
        }
        if (!view.selected || !changes.files.some((f) => f.path === view.selected)) {
          view.selected = changes.files.length ? changes.files[0].path : null;
        }
        view.composer = null;
        view.sel = null;
        view.cursor = null;
        placePending();
        writeStore();
        renderAll();
        container.dataset.ready = 'true';
      }).catch((e) => {
        if (view.destroyed) return;
        view.loading = false;
        view.error = e.message || 'The changes could not be read.';
        renderAll();
        container.dataset.ready = 'true';
      });
    }

    const fileOf = (path) => (view.changes ? view.changes.files.find((f) => f.path === path) : null);

    // Pending comments follow their lines to the candidate just loaded; one
    // whose lines are gone is kept, marked, and not sent.
    function placePending() {
      for (const c of view.pending) {
        const file = fileOf(c.path);
        const rows = file ? rowsOf(file) : [];
        const now = quoteOf(rows, c.side, c.start, c.end);
        if (c.commit === view.changes.commit && now.length === (c.quote || []).length) { c.lost = false; continue; }
        const moved = file && !file.binary ? reanchor(rows, c) : null;
        if (moved) { c.start = moved.start; c.end = moved.end; c.commit = view.changes.commit; c.lost = false; } else { c.lost = true; }
      }
    }

    // ----- the file list -----
    function visibleFiles() {
      if (!view.changes) return [];
      const q = view.filter.trim().toLowerCase();
      return view.changes.files.filter((f) => !q || f.path.toLowerCase().includes(q) || (f.old_path || '').toLowerCase().includes(q));
    }

    function selectFile(path, focusDiff) {
      view.selected = path;
      view.composer = null;
      view.sel = null;
      view.cursor = null;
      renderFiles();
      renderDiff();
      if (focusDiff && ui.table) ui.table.focus({ preventScroll: true });
    }

    function renderFiles() {
      const files = visibleFiles();
      const total = view.changes ? view.changes.files.length : 0;
      const viewed = view.changes ? view.changes.files.filter((f) => view.viewed.has(f.path)).length : 0;
      const pendingBy = new Map();
      for (const c of view.pending) pendingBy.set(c.path, (pendingBy.get(c.path) || 0) + 1);
      ui.filesTitle.textContent = `Files (${total})`;
      ui.progress.textContent = total ? `${viewed} of ${total} viewed` : '';
      const list = h('ul', { class: 'ch-filelist' }, files.map((f) => {
        const isViewed = view.viewed.has(f.path);
        const n = pendingBy.get(f.path);
        return h('li', { class: `ch-fileitem${f.path === view.selected ? ' current' : ''}${isViewed ? ' viewed' : ''}` },
          h('input', {
            type: 'checkbox', class: 'ch-viewed', 'aria-label': `Mark ${f.path} as viewed`, 'data-path': f.path,
            checked: isViewed ? 'checked' : null,
            onchange: (ev) => setViewed(f.path, ev.target.checked),
          }),
          h('button', {
            type: 'button', class: 'ch-file', 'data-path': f.path, 'aria-current': f.path === view.selected ? 'true' : null,
            onclick: () => selectFile(f.path),
          },
          h('span', { class: 'ch-filename' },
            h('span', { class: 'ch-dir', text: f.path.includes('/') ? f.path.slice(0, f.path.lastIndexOf('/') + 1) : '' }),
            h('span', { class: 'ch-base', text: f.path.slice(f.path.lastIndexOf('/') + 1) })),
          h('span', { class: 'ch-counts' },
            f.binary ? h('span', { class: 'ch-bin', text: 'binary' }) : [
              h('span', { class: 'add', text: `+${f.additions}` }), ' ', h('span', { class: 'del', text: `−${f.deletions}` })],
            n ? h('span', { class: 'ch-badge', title: `${plural(n, 'pending comment')}`, text: String(n) }) : null)));
      }));
      if (!files.length) list.append(h('li', { class: 'ch-empty muted', text: total ? 'No file matches.' : 'No changed files.' }));
      fill(ui.files, list);
    }

    function setViewed(path, on) {
      if (on) view.viewed.add(path); else view.viewed.delete(path);
      writeStore();
      renderFiles();
      if (path === view.selected) renderDiff();
    }

    // ----- the diff of the selected file -----
    function renderDiff() {
      const file = fileOf(view.selected);
      ui.table = null;
      if (!file) {
        view.rows = [];
        fill(ui.diff, h('p', { class: 'muted pad', text: view.changes && !view.changes.files.length ? (view.changes.commit ? 'The candidate has no changes against its base.' : 'This branch has no candidate yet, so there is nothing to review.') : 'Choose a file.' }));
        return;
      }
      const rows = rowsOf(file);
      view.rows = rows;
      const fileIndex = view.changes.files.indexOf(file);
      const head = h('div', { class: 'ch-diffhead' },
        h('div', { class: 'ch-diffpath' },
          h('span', { class: 'ch-path mono', text: file.old_path ? `${file.old_path} → ${file.path}` : file.path }),
          h('span', { class: `ch-status-word ch-s-${file.status}`, text: STATUS_WORDS[file.status] || file.status }),
          h('span', { class: 'ch-counts' }, h('span', { class: 'add', text: `+${file.additions}` }), ' ', h('span', { class: 'del', text: `−${file.deletions}` }))),
        h('label', { class: 'ch-check' },
          h('input', { type: 'checkbox', class: 'ch-viewed-head', checked: view.viewed.has(file.path) ? 'checked' : null, onchange: (ev) => setViewed(file.path, ev.target.checked) }),
          ' Viewed'));
      const body = h('div', { class: 'ch-scroll' });
      if (file.binary) {
        body.append(h('p', { class: 'muted pad ch-note', text: 'Binary file, not shown.' }));
      } else if (!rows.length) {
        body.append(h('p', { class: 'muted pad ch-note', text: file.truncated ? 'The diff of this file is too large to show.' : 'No line changes to show (a mode change or an empty file).' }));
      } else {
        body.append(buildTable(file, rows, fileIndex));
      }
      if (file.truncated) body.append(h('p', { class: 'hint pad ch-note', role: 'note', text: 'This file is large: only its first lines are shown. The counts above are for the whole file.' }));
      fill(ui.diff, head, body);
      if (view.composer) {
        const area = ui.diff.querySelector('.ch-composer textarea');
        if (area) area.focus({ preventScroll: false });
      }
    }

    function buildTable(file, rows, fileIndex) {
      const table = h('table', {
        class: 'ch-table', role: 'grid', tabindex: '0', 'aria-label': `Diff of ${file.path}`,
        'aria-describedby': 'ch-keys-hint',
        onkeydown: onTableKey,
      });
      ui.table = table;
      const tbody = h('tbody');
      table.append(tbody);
      const sentHere = (view.reviews ? view.reviews.reviews : []).filter((r) => r.commit && r.commit === view.changes.commit);
      const endRow = (side, line) => rows.findIndex((r) => r.type === 'line' && numberOf(r, side) === line);
      const extras = new Map();
      const addExtra = (index, el) => { if (index < 0) return; if (!extras.has(index)) extras.set(index, []); extras.get(index).push(el); };
      for (const review of sentHere) {
        for (const c of review.comments) {
          if (c.path !== file.path || !c.line) continue;
          addExtra(endRow(c.side || 'new', c.line), sentCard(review, c));
        }
      }
      for (const c of view.pending) {
        if (c.path === file.path && !c.lost && !(view.composer && view.composer.id === c.id)) addExtra(endRow(c.side, c.end), pendingCard(c));
      }
      if (view.composer && view.composer.path === file.path) {
        addExtra(endRow(view.composer.side, view.composer.end), composerBox());
      }
      rows.forEach((row, index) => {
        const id = `ch-r-${fileIndex}-${index}`;
        let tr;
        if (row.type === 'hunk') {
          tr = h('tr', { class: 'ch-hunk', id, 'data-hunk': row.hunk, role: 'row' },
            h('td', { colspan: 4, role: 'gridcell' }, h('span', { class: 'ch-hunk-text mono', text: row.header })));
        } else if (row.type === 'note') {
          tr = h('tr', { class: 'ch-nonl', id, role: 'row' }, h('td', { colspan: 4, role: 'gridcell' }, h('span', { class: 'mono', text: row.text })));
        } else {
          const side = sideOf(row);
          const n = numberOf(row, side);
          const word = row.kind === 'add' ? 'added' : row.kind === 'del' ? 'removed' : 'unchanged';
          tr = h('tr', {
            class: `ch-line ch-${row.kind}`, id, role: 'row', 'data-side': side, 'data-line': n, 'data-index': index,
            'aria-label': `${word} line ${n}`,
          },
          h('td', { class: 'ch-num ch-old', role: 'gridcell' }, row.old == null ? '' : String(row.old)),
          h('td', { class: 'ch-num ch-new', role: 'gridcell' },
            row.new == null ? '' : String(row.new)),
          h('td', { class: 'ch-gut', role: 'gridcell' },
            h('button', {
              type: 'button', class: 'ch-gutter', tabindex: '-1', 'data-side': side, 'data-line': n, 'data-index': index,
              'aria-label': `Comment on ${row.kind === 'del' ? 'removed ' : ''}line ${n}`,
              title: 'Comment on this line (drag or Shift-click for a range)',
              onpointerdown: (ev) => startDrag(ev, index),
              onclick: (ev) => { if (ev.detail === 0) { pick(index, index, ev.shiftKey); openComposer(); } },
            }, '+')),
          h('td', { class: 'ch-code', role: 'gridcell' }, h('span', { class: 'ch-mark', 'aria-hidden': 'true', text: row.kind === 'add' ? '+' : row.kind === 'del' ? '−' : ' ' }), h('code', { class: 'mono', text: row.text })));
        }
        row.el = tr;
        tbody.append(tr);
        for (const extra of extras.get(index) || []) tbody.append(h('tr', { class: 'ch-extra', role: 'row' }, h('td', { colspan: 4, role: 'gridcell' }, extra)));
      });
      paintSelection();
      return table;
    }

    // The selection and cursor are classes on rows, so changing them while
    // a drag is under way redraws nothing.
    function paintSelection() {
      if (!ui.table) return;
      for (const [index, row] of view.rows.entries()) {
        if (!row.el) continue;
        const side = view.sel && view.sel.side;
        const n = view.sel && row.type === 'line' ? numberOf(row, side) : null;
        const inSel = !!(view.sel && row.type === 'line' && row.hunk === view.sel.hunk && n != null && n >= view.sel.start && n <= view.sel.end);
        row.el.classList.toggle('ch-selected', inSel);
        const isCursor = view.cursor === index;
        row.el.classList.toggle('ch-cursor', isCursor);
        if (isCursor) ui.table.setAttribute('aria-activedescendant', row.el.id);
      }
      if (view.cursor == null) ui.table.removeAttribute('aria-activedescendant');
    }

    // ----- selecting lines: a click, a drag, Shift-click or the keyboard -----
    function pick(from, to, extend) {
      const rows = view.rows;
      const start = extend && view.anchor != null ? view.anchor : from;
      const sel = selectBetween(rows, start, to);
      if (!sel) return;
      view.anchor = start;
      view.sel = { ...sel, path: view.selected };
      view.cursor = to;
      paintSelection();
    }

    function startDrag(ev, index) {
      if (ev.button) return;
      ev.preventDefault();
      pick(index, index, ev.shiftKey);
      const origin = view.anchor;
      let last = index;
      const move = (m) => {
        const el = document.elementFromPoint(m.clientX, m.clientY);
        const row = el && el.closest ? el.closest('tr.ch-line') : null;
        if (!row || !ui.table || !ui.table.contains(row)) return;
        const i = Number(row.dataset.index);
        if (i !== last) { last = i; pick(origin, i, false); }
      };
      const up = () => {
        window.removeEventListener('pointermove', move);
        window.removeEventListener('pointerup', up);
        window.removeEventListener('pointercancel', up);
        openComposer();
      };
      window.addEventListener('pointermove', move);
      window.addEventListener('pointerup', up);
      window.addEventListener('pointercancel', up);
    }

    function lineRows() { return view.rows.map((r, i) => (r.type === 'line' ? i : -1)).filter((i) => i >= 0); }

    function moveCursor(step, extend) {
      const lines = lineRows();
      if (!lines.length) return;
      const at = view.cursor == null ? -1 : lines.indexOf(view.cursor);
      const next = lines[Math.max(0, Math.min(lines.length - 1, at + step))];
      if (extend && view.anchor != null) pick(view.anchor, next, false); else { view.anchor = next; view.sel = null; view.cursor = next; paintSelection(); }
      const row = view.rows[next];
      if (row && row.el && row.el.scrollIntoView) row.el.scrollIntoView({ block: 'nearest' });
    }

    function onTableKey(ev) {
      if (ev.target !== ui.table) return;
      if (ev.key === 'ArrowDown') { ev.preventDefault(); moveCursor(1, ev.shiftKey); }
      else if (ev.key === 'ArrowUp') { ev.preventDefault(); moveCursor(-1, ev.shiftKey); }
      else if (ev.key === 'Enter') { ev.preventDefault(); if (!view.sel && view.cursor != null) pick(view.cursor, view.cursor, false); openComposer(); }
    }

    // ----- the comment box -----
    function openComposer(existing) {
      if (!view.sel && existing == null) return;
      if (!opts.can('run')) { say('This token cannot send reviews (it lacks the run scope).'); return; }
      const sel = existing || view.sel;
      const quote = quoteOf(view.rows, sel.side, sel.start, sel.end);
      view.composer = { path: view.selected, side: sel.side, start: sel.start, end: sel.end, hunk: sel.hunk, quote, id: existing ? existing.id : null, body: existing ? existing.body : '' };
      renderDiff();
    }

    function composerBox() {
      const c = view.composer;
      const label = c.start === c.end ? `line ${c.end}` : `lines ${c.start}–${c.end}`;
      const field = h('textarea', {
        class: 'ch-input', rows: '3', 'aria-label': `Comment on ${label} of ${c.path}`, placeholder: 'Write a comment. Enter adds a line; Ctrl+Enter adds it to the review.',
        onkeydown: (ev) => {
          if (ev.key === 'Escape') { ev.preventDefault(); closeComposer(); }
          if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey)) { ev.preventDefault(); saveComposer(field.value); }
        },
        oninput: () => { c.body = field.value; },
      });
      field.value = c.body || '';
      return h('div', { class: 'ch-composer', role: 'group', 'aria-label': `New comment on ${label}` },
        h('p', { class: 'ch-composer-title', text: `${c.side === 'old' ? 'Removed ' : ''}${label}${c.side === 'old' ? ' (base)' : ''}` }),
        field,
        h('div', { class: 'ch-actions' },
          h('button', { type: 'button', class: 'primary small ch-add', text: c.id ? 'Update comment' : 'Add to review', onclick: () => saveComposer(field.value) }),
          h('button', { type: 'button', class: 'small ch-cancel', text: 'Cancel', onclick: closeComposer })));
    }

    function closeComposer() {
      view.composer = null;
      renderDiff();
      if (ui.table) ui.table.focus({ preventScroll: true });
    }

    function saveComposer(text) {
      const body = text.trim();
      if (!body) { say('Write something first.'); return; }
      const c = view.composer;
      if (c.id) {
        const mine = view.pending.find((p) => p.id === c.id);
        if (mine) mine.body = body;
      } else {
        view.pending.push({ id: view.nextId++, path: c.path, side: c.side, start: c.start, end: c.end, quote: c.quote, body, commit: view.changes.commit, lost: false });
      }
      view.composer = null;
      view.sel = null;
      writeStore();
      renderDiff();
      renderFiles();
      renderTray();
      say(`Comment added. ${plural(view.pending.length, 'comment')} in this review.`);
    }

    function removePending(id) {
      view.pending = view.pending.filter((p) => p.id !== id);
      writeStore();
      renderDiff();
      renderFiles();
      renderTray();
    }

    function pendingCard(c) {
      const label = c.start === c.end ? `line ${c.end}` : `lines ${c.start}–${c.end}`;
      return h('div', { class: 'ch-comment ch-pending', role: 'group', 'aria-label': `Pending comment on ${label}` },
        h('p', { class: 'ch-comment-head' }, h('span', { class: 'ch-tag', text: 'Pending' }), ` ${label}`),
        h('p', { class: 'ch-comment-body', text: c.body }),
        h('div', { class: 'ch-actions' },
          h('button', { type: 'button', class: 'small ch-edit', text: 'Edit', onclick: () => { view.sel = null; openComposer(c); } }),
          h('button', { type: 'button', class: 'small ghost ch-remove', text: 'Remove', onclick: () => removePending(c.id) })));
    }

    function sentCard(review, c) {
      const label = rangeWord(c);
      return h('div', { class: 'ch-comment ch-sent', role: 'group', 'aria-label': `Sent comment on ${label}` },
        h('p', { class: 'ch-comment-head' }, h('span', { class: 'ch-tag ch-tag-sent', text: 'Sent' }), ` ${label} · ${clock(review.at_ms)}`),
        h('p', { class: 'ch-comment-body', text: c.body }));
    }

    // ----- jumping around -----
    function jumpToHunk(step) {
      const hunks = view.rows.map((r, i) => (r.type === 'hunk' ? i : -1)).filter((i) => i >= 0);
      if (!hunks.length) return;
      const here = view.cursor == null ? -1 : hunks.filter((i) => i <= view.cursor).length - 1;
      const target = hunks[Math.max(0, Math.min(hunks.length - 1, here + step))];
      const first = view.rows.findIndex((r, i) => i > target && r.type === 'line');
      view.anchor = first;
      view.sel = null;
      view.cursor = first;
      paintSelection();
      const row = view.rows[target];
      if (row && row.el) row.el.scrollIntoView({ block: 'start' });
      if (ui.table) ui.table.focus({ preventScroll: true });
    }

    function stepFile(step) {
      const files = visibleFiles();
      if (!files.length) return;
      const at = files.findIndex((f) => f.path === view.selected);
      const next = files[Math.max(0, Math.min(files.length - 1, at + step))];
      if (next.path !== view.selected) selectFile(next.path);
      const item = ui.files.querySelector(`.ch-file[data-path="${CSS.escape(next.path)}"]`);
      if (item) item.scrollIntoView({ block: 'nearest' });
    }

    function commentHere() {
      if (!view.sel && view.cursor == null) {
        const first = view.rows.findIndex((r) => r.type === 'line' && r.kind !== 'context');
        const start = first >= 0 ? first : view.rows.findIndex((r) => r.type === 'line');
        if (start < 0) return;
        pick(start, start, false);
      } else if (!view.sel) {
        pick(view.cursor, view.cursor, false);
      }
      openComposer();
    }

    function onKey(ev) {
      if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
      const t = ev.target;
      if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable)) return;
      if (ev.key === 'j') { ev.preventDefault(); stepFile(1); }
      else if (ev.key === 'k') { ev.preventDefault(); stepFile(-1); }
      else if (ev.key === 'n') { ev.preventDefault(); jumpToHunk(1); }
      else if (ev.key === 'p') { ev.preventDefault(); jumpToHunk(-1); }
      else if (ev.key === 'c') { ev.preventDefault(); commentHere(); }
      else if (ev.key === 'Escape' && view.composer) closeComposer();
    }

    // ----- the review: pending comments, a note, and how to send -----
    function renderTray() {
      const usable = view.pending.filter((c) => !c.lost);
      const lost = view.pending.length - usable.length;
      const running = opts.info() && opts.info().status && opts.info().status.state === 'running';
      if (view.delivery === 'steer' && !running) view.delivery = 'next_turn';
      const list = h('ol', { class: 'ch-pendinglist' }, view.pending.map((c) => {
        const label = c.start === c.end ? `${c.path}:${c.end}` : `${c.path}:${c.start}–${c.end}`;
        return h('li', { class: `ch-pitem${c.lost ? ' lost' : ''}`, 'data-id': c.id },
          h('button', { type: 'button', class: 'ch-jump link', onclick: () => { selectFile(c.path); } , text: `${c.side === 'old' ? '(base) ' : ''}${label}` }),
          c.lost ? h('p', { class: 'ch-warn', role: 'note', text: 'The branch changed these lines. This comment cannot be placed and will not be sent; copy its text if you need it.' }) : null,
          h('p', { class: 'ch-pbody', text: c.body }),
          h('button', { type: 'button', class: 'small ghost', 'aria-label': `Remove the comment on ${label}`, text: 'Remove', onclick: () => removePending(c.id) }));
      }));
      const note = h('textarea', {
        class: 'ch-note-input', id: 'ch-note', rows: '2', placeholder: 'An overall note for the agent (optional)',
        oninput: () => { view.note = note.value; writeStore(); updateSend(); },
      });
      note.value = view.note;
      const radio = (value, label, disabled, why) => h('label', { class: `ch-radio${disabled ? ' off' : ''}` },
        h('input', { type: 'radio', name: 'ch-delivery', value, checked: view.delivery === value ? 'checked' : null, disabled: disabled ? 'disabled' : null, onchange: () => { view.delivery = value; } }),
        ` ${label}`, why ? h('span', { class: 'hint', text: ` ${why}` }) : null);
      ui.send = h('button', { type: 'button', class: 'primary ch-send', onclick: sendReview });
      fill(ui.tray, 
        h('h2', { id: 'ch-tray-title', text: 'Your review' }),
        view.pending.length ? list : h('p', { class: 'muted', text: 'Select lines in the diff (click the + beside a line, or drag across several) and write a comment. They collect here until you send them together.' }),
        lost ? h('p', { class: 'ch-warn', role: 'status', text: `${plural(lost, 'comment')} could not be placed on the new candidate.` }) : null,
        note,
        h('fieldset', { class: 'ch-delivery' },
          h('legend', { text: 'Send as' }),
          radio('next_turn', 'The next turn', false, running ? 'Waits for the running turn to end.' : ''),
          radio('steer', 'A steer into the running turn', !running, running ? 'Does not interrupt it.' : 'No turn is running.')),
        ui.send,
        view.problem ? h('p', { class: 'error ch-problem', role: 'alert', text: view.problem }) : null,
        sentSection());
      updateSend();
    }

    function updateSend() {
      if (!ui.send) return;
      const usable = view.pending.filter((c) => !c.lost).length;
      const hasNote = view.note.trim().length > 0;
      ui.send.textContent = view.sending ? 'Sending…' : usable ? `Send review (${usable})` : 'Send review';
      ui.send.disabled = view.sending || (!usable && !hasNote) || !opts.can('run');
    }

    function sentSection() {
      const reviews = view.reviews ? view.reviews.reviews : [];
      if (!reviews.length) return null;
      const now = view.changes && view.changes.commit;
      return h('details', { class: 'ch-sentlist' },
        h('summary', { text: `Sent reviews (${reviews.length})` }),
        h('ol', {}, reviews.slice().reverse().map((r) => h('li', { class: 'ch-sentreview', 'data-review': r.id },
          h('p', { class: 'ch-comment-head', text: `${clock(r.at_ms)} · ${plural(r.comments.length, 'comment')}${r.commit && r.commit !== now ? ` on ${short(r.commit)}` : ''}` }),
          r.note ? h('p', { class: 'ch-pbody', text: `Note: ${r.note}` }) : null,
          h('ul', {}, r.comments.map((c) => h('li', {},
            h('span', { class: 'mono', text: `${c.path}${c.line ? `:${c.start_line && c.start_line !== c.line ? `${c.start_line}–${c.line}` : c.line}` : ''}` }), ' ',
            c.addressed_in ? h('span', { class: 'ch-tag ch-tag-done', text: `Addressed in ${short(c.addressed_in)}` }) : h('span', { class: 'ch-tag ch-tag-sent', text: 'Sent' }),
            h('span', { class: 'ch-pbody', text: ` ${c.body}` }))))))));
    }

    async function sendReview() {
      const usable = view.pending.filter((c) => !c.lost);
      const note = view.note.trim();
      if (view.sending || (!usable.length && !note)) return;
      view.sending = true;
      view.problem = null;
      updateSend();
      const body = {
        comments: usable.map((c) => {
          const out = { path: c.path, line: c.end, side: c.side, body: c.body };
          if (c.start !== c.end) out.start_line = c.start;
          return out;
        }),
        delivery: view.delivery,
        commit: view.changes.commit,
      };
      if (note) body.note = note;
      try {
        const sent = await opts.api('POST', `${opts.path}/reviews`, body);
        view.pending = view.pending.filter((c) => c.lost);
        view.note = '';
        view.composer = null;
        view.sel = null;
        writeStore();
        view.reviews = await opts.api('GET', `${opts.path}/reviews`);
        opts.toast(`Review sent: ${plural(sent.comments, 'comment')}, ${sent.delivery === 'steer' ? 'steered into the running turn' : 'as the next turn'}.`);
        say(`Review sent to ${opts.branch}.`);
      } catch (e) {
        view.problem = e.code === 'stale_candidate'
          ? 'The branch has a new candidate since you loaded this diff, so these line numbers may point at other code. Reload the changes: your comments move with their lines where they can.'
          : e.code === 'not_running' ? 'No turn is running, so there is nothing to steer. Send it as the next turn.' : (e.message || 'The review could not be sent.');
        if (e.code === 'stale_candidate') view.stale = true;
      }
      view.sending = false;
      renderBanner();
      renderTray();
      renderDiff();
      renderFiles();
    }

    // ----- the page's frame -----
    function say(text) { if (ui.live) { ui.live.textContent = ''; setTimeout(() => { if (ui.live) ui.live.textContent = text; }, 20); } }

    function renderBanner() {
      const info = opts.info();
      const now = info && info.candidate ? info.candidate.commit : null;
      const newer = !!view.changes && (view.changes.commit || null) !== (now || null);
      const show = view.stale || newer;
      ui.banner.replaceChildren(...(show ? [
        h('p', { class: 'ch-banner-text', role: 'status', text: 'This branch has a new candidate. The diff below is the one you opened.' }),
        h('button', { type: 'button', class: 'small primary ch-reload', text: 'Reload changes', onclick: () => load() }),
      ] : []));
      ui.banner.hidden = !show;
    }

    function renderSummary() {
      const c = view.changes;
      if (!c) { fill(ui.summary, ); return; }
      fill(ui.summary, 
        h('span', { class: 'ch-total' }, plural(c.files.length + (c.omitted_files || 0), 'file'), ' changed, ',
          h('span', { class: 'add', text: `+${c.additions}` }), ' ', h('span', { class: 'del', text: `−${c.deletions}` })),
        c.commit ? h('span', { class: 'muted mono', title: `against ${c.base}`, text: ` · ${short(c.commit)}` }) : null,
        c.omitted_files ? h('p', { class: 'hint', role: 'note', text: `${plural(c.omitted_files, 'more file')} changed but not listed (the list is capped); the totals count them.` }) : null);
    }

    function renderAll() {
      if (view.destroyed) return;
      renderBanner();
      if (view.loading) {
        fill(ui.summary, );
        fill(ui.files, h('p', { class: 'muted pad', role: 'status', text: 'Reading the changes…' }));
        fill(ui.diff, );
        fill(ui.tray, );
        return;
      }
      if (view.error) {
        fill(ui.files, );
        fill(ui.diff, h('p', { class: 'error pad', role: 'alert', text: view.error }), h('button', { type: 'button', class: 'small', text: 'Try again', onclick: () => load() }));
        return;
      }
      renderSummary();
      renderFiles();
      renderDiff();
      renderTray();
    }

    function build() {
      ui.live = h('div', { class: 'sr-only', role: 'status', 'aria-live': 'polite' });
      ui.banner = h('div', { class: 'ch-banner', hidden: true });
      ui.summary = h('div', { class: 'ch-summary' });
      ui.filesTitle = h('span', { class: 'ch-filestitle' });
      ui.progress = h('span', { class: 'ch-progress muted' });
      ui.filter = h('input', {
        type: 'search', class: 'ch-filter', 'aria-label': 'Filter files by path', placeholder: 'Filter files', autocomplete: 'off',
        oninput: () => { view.filter = ui.filter.value; renderFiles(); },
      });
      ui.files = h('div', { class: 'ch-files-body' });
      ui.diff = h('div', { class: 'ch-diff' });
      ui.tray = h('aside', { class: 'ch-tray', 'aria-labelledby': 'ch-tray-title' });
      const sidebar = h('details', { class: 'ch-sidebar', open: true },
        h('summary', {}, ui.filesTitle, ' ', ui.progress),
        ui.filter, ui.files);
      // A phone starts with the file list closed: the diff is what it came for.
      if (root.matchMedia && root.matchMedia('(max-width: 720px)').matches) sidebar.open = false;
      ui.sidebar = sidebar;
      container.replaceChildren(h('div', { class: 'ch' },
        ui.banner, ui.summary,
        h('p', { class: 'hint ch-keys', id: 'ch-keys-hint', text: 'Keys: j and k change file, n and p change hunk, c comments on the selected line, arrows move within a diff.' }),
        h('div', { class: 'ch-layout' }, sidebar, ui.diff, ui.tray), ui.live));
    }

    build();
    document.addEventListener('keydown', onKey);
    load();
    return {
      branchChanged() { if (!view.destroyed && view.changes) renderBanner(); if (!view.destroyed && !view.loading) renderTray(); },
      reload: load,
      destroy() { view.destroyed = true; document.removeEventListener('keydown', onKey); container.replaceChildren(); },
      state: view,
    };
  }

  const api = { mount, model: { rowsOf, selectBetween, quoteOf, reanchor, sideOf, rangeWord } };
  root.BranchyardChanges = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})(typeof window !== 'undefined' ? window : globalThis);
