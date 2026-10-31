/**
 * artifact-preview.js
 * Framework-free drop-in document-preview integration component for Branchyard.
 * Exposes window.BranchyardArtifactPreview with mount(container, options).
 */

(function () {
  'use strict';

  function isSafeBasename(filename) {
    if (typeof filename !== 'string' || filename.length === 0) return false;
    // Disallow path traversal, slashes, backslashes, control characters
    if (filename.includes('/') || filename.includes('\\') || filename.includes('..')) {
      return false;
    }
    // Allow standard alphanumeric + . - _
    return /^[a-zA-Z0-9_\-\.]+$/.test(filename);
  }

  var MAX_RETRIES = 5;

  // What a refused or failed preview means to the person reading it, by the
  // server's error code. `server` shows the server's own message and next
  // step; `operator` keeps them under a disclosure, since they are setup
  // instructions for whoever runs the server; `retry` offers Try again.
  var FAILURES = {
    preview_unsupported: { title: 'No preview for this format', hint: 'Download the original to open it on this device.' },
    preview_unavailable: { title: 'This server cannot make previews right now', hint: 'Download the original to open it on this device.', operator: true },
    conversion_failed: { title: 'This file could not be converted for preview', server: true },
    preview_timeout: { title: 'The preview took too long to make', hint: 'Try again, or download the original.', retry: true },
    preview_too_large: { title: 'This file is too large to preview', server: true },
    preview_output_limit: { title: 'This preview is too large to show', hint: 'Download the original to open it on this device.' },
    preview_busy: { title: 'The server is busy making other previews', hint: 'Try again in a moment.', retry: true },
    not_found: { title: 'This file is no longer available', server: true },
    unknown_preview_asset: { title: 'This page of the preview is no longer available', retry: true },
    unreachable: { title: 'The server cannot be reached', hint: 'Check the connection, then try again.', retry: true },
    error: { title: 'The preview could not be shown', server: true, retry: true }
  };

  // The server's answer as preview data, or as a failure with the server's
  // words. A refusal is a JSON error envelope; a conversion that failed, or
  // whose renderer lacks a component, is a 200 whose status says so.
  function outcome(status, body) {
    var error = (body && body.error) || {};
    if (status === 200 && body && body.status === 'passed') return { data: body };
    if (status === 200 && body && (body.status === 'failed' || body.status === 'unavailable')) {
      return { code: body.status === 'failed' ? 'conversion_failed' : 'preview_unavailable', message: error.message, next: error.next };
    }
    var detail = error.detail || {};
    return { code: error.code || 'error', message: error.message || 'HTTP ' + status, next: detail.next || error.next, retryAfter: detail.retry_after };
  }

  function mount(container, options) {
    if (!container) {
      throw new Error('mount requires a valid DOM container element');
    }
    options = options || {};
    var baseUrl = options.baseUrl;
    var fetchAuthenticated = options.fetchAuthenticated;
    var displayName = options.displayName || '';
    var mediaType = options.mediaType || '';

    if (typeof baseUrl !== 'string' || typeof fetchAuthenticated !== 'function') {
      throw new Error('mount options require baseUrl (string) and fetchAuthenticated (function)');
    }

    // Instance state
    var isDisposed = false;
    var activeAbortControllers = new Set();
    var retryTimerId = null;
    var activeBlobUrls = [];
    var generation = 0;

    // View state
    var currentView = 'auto'; // 'auto' | 'table' | 'pages'
    var currentPage = 1;
    var currentSheet = '';
    var currentOffset = 0;
    var limit = 100;

    // Component DOM structure
    var rootEl = document.createElement('div');
    rootEl.className = 'by-artifact-preview';

    var liveRegion = document.createElement('div');
    liveRegion.className = 'by-preview-sr-only';
    liveRegion.setAttribute('aria-live', 'polite');
    liveRegion.setAttribute('aria-atomic', 'true');
    rootEl.appendChild(liveRegion);

    var toolbarEl = document.createElement('div');
    toolbarEl.className = 'by-preview-toolbar';
    toolbarEl.setAttribute('role', 'toolbar');
    toolbarEl.setAttribute('aria-label', 'Document preview controls');
    rootEl.appendChild(toolbarEl);

    var warningsEl = document.createElement('div');
    warningsEl.className = 'by-preview-warning-banner';
    warningsEl.style.display = 'none';
    rootEl.appendChild(warningsEl);

    var contentEl = document.createElement('div');
    contentEl.className = 'by-preview-content';
    contentEl.setAttribute('tabindex', '0');
    contentEl.setAttribute('aria-label', 'Preview content');
    rootEl.appendChild(contentEl);

    container.appendChild(rootEl);

    function announce(msg) {
      if (!isDisposed && liveRegion) {
        liveRegion.textContent = '';
        if (typeof queueMicrotask === 'function') {
          queueMicrotask(function () {
            if (!isDisposed && liveRegion) {
              liveRegion.textContent = msg;
            }
          });
        } else {
          liveRegion.textContent = msg;
        }
      }
    }

    function revokeAllBlobs() {
      for (var i = 0; i < activeBlobUrls.length; i++) {
        try {
          URL.revokeObjectURL(activeBlobUrls[i]);
        } catch (e) {
          // ignore
        }
      }
      activeBlobUrls = [];
    }

    function cancelPendingRequests() {
      activeAbortControllers.forEach(function (ctrl) {
        try {
          ctrl.abort();
        } catch (e) {
          // ignore
        }
      });
      activeAbortControllers.clear();

      if (retryTimerId !== null) {
        clearTimeout(retryTimerId);
        retryTimerId = null;
      }
    }

    function renderToolbar(data) {
      toolbarEl.textContent = '';

      var leftGroup = document.createElement('div');
      leftGroup.className = 'by-preview-toolbar-left';

      var rightGroup = document.createElement('div');
      rightGroup.className = 'by-preview-toolbar-right';

      // Title & Format Badge
      if (displayName) {
        var titleEl = document.createElement('span');
        titleEl.className = 'by-preview-title';
        titleEl.textContent = displayName;
        titleEl.title = displayName;
        leftGroup.appendChild(titleEl);
      }

      var effectiveFormat = (data && data.format) || mediaType;
      if (effectiveFormat) {
        var badgeEl = document.createElement('span');
        badgeEl.className = 'by-preview-badge';
        badgeEl.textContent = effectiveFormat;
        leftGroup.appendChild(badgeEl);
      }

      // If data is XLSX or doc supports view switching
      var isXlsx = (data && data.format === 'xlsx') || (mediaType.indexOf('spreadsheet') !== -1 || mediaType.indexOf('excel') !== -1);
      if (isXlsx && data && (data.kind === 'table' || data.kind === 'pages')) {
        var switchGroup = document.createElement('div');
        switchGroup.className = 'by-preview-switch';
        switchGroup.setAttribute('role', 'group');
        switchGroup.setAttribute('aria-label', 'View mode');

        var tableBtn = document.createElement('button');
        tableBtn.type = 'button';
        tableBtn.className = 'by-preview-switch-btn' + (data.kind === 'table' ? ' active' : '');
        tableBtn.textContent = 'Table';
        tableBtn.setAttribute('aria-pressed', data.kind === 'table' ? 'true' : 'false');
        tableBtn.addEventListener('click', function () {
          if (currentView !== 'table') {
            currentView = 'table';
            currentOffset = 0;
            currentPage = 1;
            loadPreview();
          }
        });

        var printBtn = document.createElement('button');
        printBtn.type = 'button';
        printBtn.className = 'by-preview-switch-btn' + (data.kind === 'pages' ? ' active' : '');
        printBtn.textContent = 'Print View';
        printBtn.setAttribute('aria-pressed', data.kind === 'pages' ? 'true' : 'false');
        printBtn.addEventListener('click', function () {
          if (currentView !== 'pages') {
            currentView = 'pages';
            currentPage = 1;
            currentOffset = 0;
            loadPreview();
          }
        });

        switchGroup.appendChild(tableBtn);
        switchGroup.appendChild(printBtn);
        leftGroup.appendChild(switchGroup);
      }

      // Sheet selector if sheets exist
      if (data && data.kind === 'table' && Array.isArray(data.sheets) && data.sheets.length > 1) {
        var sheetLabel = document.createElement('label');
        sheetLabel.className = 'by-preview-sr-only';
        sheetLabel.textContent = 'Select Sheet';
        var sheetSelect = document.createElement('select');
        sheetSelect.className = 'by-preview-select';
        sheetSelect.setAttribute('aria-label', 'Select sheet');
        for (var i = 0; i < data.sheets.length; i++) {
          var opt = document.createElement('option');
          opt.value = data.sheets[i];
          opt.textContent = data.sheets[i];
          if (data.sheets[i] === (data.sheet || currentSheet)) {
            opt.selected = true;
          }
          sheetSelect.appendChild(opt);
        }
        sheetSelect.addEventListener('change', function () {
          currentSheet = sheetSelect.value;
          currentOffset = 0;
          loadPreview();
        });
        leftGroup.appendChild(sheetSelect);
      }

      // Page navigation for pages kind
      if (data && data.kind === 'pages') {
        var pageCount = typeof data.pages === 'number' ? data.pages : 1;
        var activePage = typeof data.page === 'number' ? data.page : currentPage;

        var prevBtn = document.createElement('button');
        prevBtn.type = 'button';
        prevBtn.className = 'by-preview-btn';
        prevBtn.textContent = '◀ Prev';
        prevBtn.setAttribute('aria-label', 'Previous page');
        prevBtn.disabled = activePage <= 1;
        prevBtn.addEventListener('click', function () {
          if (activePage > 1) {
            currentPage = activePage - 1;
            loadPreview();
          }
        });

        var pageLabel = document.createElement('span');
        pageLabel.className = 'by-preview-page-indicator';
        pageLabel.textContent = activePage + ' / ' + pageCount;

        var nextBtn = document.createElement('button');
        nextBtn.type = 'button';
        nextBtn.className = 'by-preview-btn';
        nextBtn.textContent = 'Next ▶';
        nextBtn.setAttribute('aria-label', 'Next page');
        nextBtn.disabled = activePage >= pageCount;
        nextBtn.addEventListener('click', function () {
          if (activePage < pageCount) {
            currentPage = activePage + 1;
            loadPreview();
          }
        });

        rightGroup.appendChild(prevBtn);
        rightGroup.appendChild(pageLabel);
        rightGroup.appendChild(nextBtn);
      }

      toolbarEl.appendChild(leftGroup);
      toolbarEl.appendChild(rightGroup);
    }

    function renderWarnings(warnings) {
      if (Array.isArray(warnings) && warnings.length > 0) {
        warningsEl.textContent = '';
        var warnText = warnings.join('; ');
        var iconSpan = document.createElement('span');
        iconSpan.setAttribute('aria-hidden', 'true');
        iconSpan.textContent = '⚠️ ';
        warningsEl.appendChild(iconSpan);

        var msgSpan = document.createElement('span');
        msgSpan.textContent = warnText;
        warningsEl.appendChild(msgSpan);

        warningsEl.style.display = 'flex';
      } else {
        warningsEl.style.display = 'none';
        warningsEl.textContent = '';
      }
    }

    function renderLoading(msg) {
      contentEl.textContent = '';
      renderWarnings([]);

      var container = document.createElement('div');
      container.className = 'by-preview-state-container';

      var spinner = document.createElement('div');
      spinner.className = 'by-preview-spinner';
      spinner.setAttribute('aria-hidden', 'true');
      container.appendChild(spinner);

      var text = document.createElement('div');
      text.className = 'by-preview-state-message';
      text.textContent = msg || 'Making a preview…';
      container.appendChild(text);

      contentEl.appendChild(container);
      announce(text.textContent);
    }

    function textBlock(tag, className, text) {
      var el = document.createElement(tag);
      el.className = className;
      el.textContent = text;
      return el;
    }

    // One failure as `outcome` reads it: a plain title, then the server's
    // words where they help the reader, or under a disclosure for the
    // server's operator.
    function renderFailure(failure) {
      var meaning = FAILURES[failure.code] || FAILURES.error;
      contentEl.textContent = '';
      toolbarEl.textContent = '';
      renderWarnings([]);

      var container = document.createElement('div');
      container.className = 'by-preview-state-container by-preview-failure';
      container.appendChild(textBlock('p', 'by-preview-state-message', meaning.title));
      if (meaning.server && failure.message) container.appendChild(textBlock('p', 'by-preview-state-subtext', failure.message));
      if (meaning.server && failure.next) container.appendChild(textBlock('p', 'by-preview-state-subtext', 'Next: ' + failure.next));
      if (meaning.hint) container.appendChild(textBlock('p', 'by-preview-state-subtext', meaning.hint));
      if (meaning.operator && (failure.message || failure.next)) {
        var details = document.createElement('details');
        details.className = 'by-preview-operator';
        details.appendChild(textBlock('summary', '', 'Details for the server\'s operator'));
        if (failure.message) details.appendChild(textBlock('p', '', failure.message));
        if (failure.next) details.appendChild(textBlock('p', '', failure.next));
        container.appendChild(details);
      }
      if (meaning.retry) {
        var retryBtn = document.createElement('button');
        retryBtn.type = 'button';
        retryBtn.className = 'by-preview-btn';
        retryBtn.textContent = 'Try again';
        retryBtn.addEventListener('click', function () {
          loadPreview();
        });
        container.appendChild(retryBtn);
      }
      contentEl.appendChild(container);
      announce(meaning.title);
    }

    function renderTable(data) {
      contentEl.textContent = '';
      renderToolbar(data);
      renderWarnings(data.warnings);

      var tableContainer = document.createElement('div');
      tableContainer.className = 'by-preview-table-container';

      var table = document.createElement('table');
      table.className = 'by-preview-table';
      table.setAttribute('role', 'table');

      var thead = document.createElement('thead');
      var headerRow = document.createElement('tr');
      var columns = Array.isArray(data.columns) ? data.columns : [];

      for (var c = 0; c < columns.length; c++) {
        var th = document.createElement('th');
        th.setAttribute('scope', 'col');
        th.textContent = columns[c] != null ? String(columns[c]) : '';
        headerRow.appendChild(th);
      }
      thead.appendChild(headerRow);
      table.appendChild(thead);

      var tbody = document.createElement('tbody');
      var rows = Array.isArray(data.rows) ? data.rows : [];

      for (var r = 0; r < rows.length; r++) {
        var tr = document.createElement('tr');
        var rowData = rows[r];
        if (Array.isArray(rowData)) {
          for (var i = 0; i < rowData.length; i++) {
            var td = document.createElement('td');
            // Strict inert textContent - prevent CSV injection formula / script execution
            td.textContent = rowData[i] != null ? String(rowData[i]) : '';
            tr.appendChild(td);
          }
        }
        tbody.appendChild(tr);
      }
      table.appendChild(tbody);
      tableContainer.appendChild(table);
      contentEl.appendChild(tableContainer);

      // Pagination footer if offset or next_offset
      var hasPagination = (typeof data.offset === 'number' && data.offset > 0) || (typeof data.next_offset === 'number');
      if (hasPagination) {
        var footer = document.createElement('div');
        footer.className = 'by-preview-table-footer';

        var rowsInfo = document.createElement('span');
        var startRow = (data.offset || 0) + 1;
        var endRow = (data.offset || 0) + rows.length;
        rowsInfo.textContent = rows.length > 0 ? ('Showing rows ' + startRow + '–' + endRow) : 'No rows';
        footer.appendChild(rowsInfo);

        var pageNav = document.createElement('div');
        pageNav.className = 'by-preview-toolbar-right';

        var prevPageBtn = document.createElement('button');
        prevPageBtn.type = 'button';
        prevPageBtn.className = 'by-preview-btn';
        prevPageBtn.textContent = '◀ Previous ' + limit;
        prevPageBtn.disabled = !data.offset || data.offset <= 0;
        prevPageBtn.addEventListener('click', function () {
          currentOffset = Math.max(0, (data.offset || 0) - limit);
          loadPreview();
        });
        pageNav.appendChild(prevPageBtn);

        var nextPageBtn = document.createElement('button');
        nextPageBtn.type = 'button';
        nextPageBtn.className = 'by-preview-btn';
        nextPageBtn.textContent = 'Next ' + limit + ' ▶';
        nextPageBtn.disabled = typeof data.next_offset !== 'number';
        nextPageBtn.addEventListener('click', function () {
          if (typeof data.next_offset === 'number') {
            currentOffset = data.next_offset;
            loadPreview();
          }
        });
        pageNav.appendChild(nextPageBtn);

        footer.appendChild(pageNav);
        contentEl.appendChild(footer);
      }

      announce('Preview ready: ' + rows.length + (rows.length === 1 ? ' row.' : ' rows.'));
    }

    function renderPages(data, currentGen, previewQuery) {
      renderToolbar(data);
      renderWarnings(data.warnings);
      contentEl.textContent = '';

      var imageFilename = data.image;
      if (!imageFilename || !isSafeBasename(imageFilename)) {
        renderFailure({ code: 'error', message: 'The page image has an unexpected name.' });
        return;
      }

      var pagesContainer = document.createElement('div');
      pagesContainer.className = 'by-preview-pages-container';

      var spinner = document.createElement('div');
      spinner.className = 'by-preview-spinner';
      pagesContainer.appendChild(spinner);
      contentEl.appendChild(pagesContainer);

      // Fetch the asset with the SAME preview query and authenticated fetch
      var assetPath = baseUrl + '/preview/assets/' + encodeURIComponent(imageFilename) + previewQuery;
      var assetAbort = new AbortController();
      activeAbortControllers.add(assetAbort);

      fetchAuthenticated(assetPath, {
        method: 'GET',
        signal: assetAbort.signal
      }).then(function (res) {
        activeAbortControllers.delete(assetAbort);
        if (currentGen !== generation || isDisposed) return;
        if (!res.ok) {
          throw new Error('the server answered ' + res.status + '.');
        }
        var ctype = res.headers && res.headers.get ? (res.headers.get('content-type') || '') : '';
        if (ctype.toLowerCase().indexOf('image/png') === -1) {
          throw new Error('it is not a PNG image.');
        }
        return res.blob();
      }).then(function (blob) {
        if (currentGen !== generation || isDisposed || !blob) return;
        var blobUrl = URL.createObjectURL(blob);
        activeBlobUrls.push(blobUrl);

        pagesContainer.textContent = '';
        var img = document.createElement('img');
        img.className = 'by-preview-page-img';
        img.src = blobUrl;
        img.alt = 'Page ' + (data.page || currentPage) + ' of ' + (data.pages || 1);
        if (typeof data.width === 'number' && data.width > 0) {
          img.style.maxWidth = data.width + 'px';
        }
        pagesContainer.appendChild(img);
        announce('Preview ready: page ' + (data.page || currentPage) + ' of ' + (data.pages || 1) + '.');
      }).catch(function (err) {
        activeAbortControllers.delete(assetAbort);
        if (currentGen !== generation || isDisposed) return;
        if (err.name === 'AbortError') return;
        pagesContainer.textContent = '';
        var errBox = document.createElement('div');
        errBox.className = 'by-preview-state-message';
        errBox.textContent = 'The page image did not load: ' + err.message;
        pagesContainer.appendChild(errBox);
        announce(errBox.textContent);
      });
    }

    function renderText(data) {
      contentEl.textContent = '';
      renderToolbar(data);
      renderWarnings(data.warnings);

      var textContainer = document.createElement('div');
      textContainer.className = 'by-preview-text-container';

      var pre = document.createElement('pre');
      pre.className = 'by-preview-text-content';
      pre.textContent = typeof data.text === 'string' ? data.text : '';
      textContainer.appendChild(pre);

      contentEl.appendChild(textContainer);
      announce('Preview ready.');
    }

    function loadPreview() {
      if (isDisposed) return;

      cancelPendingRequests();
      revokeAllBlobs();

      var thisGen = ++generation;
      renderLoading();

      var queryParts = ['view=' + encodeURIComponent(currentView)];
      if (currentPage > 1) {
        queryParts.push('page=' + encodeURIComponent(currentPage));
      }
      if (currentSheet) {
        queryParts.push('sheet=' + encodeURIComponent(currentSheet));
      }
      if (currentOffset > 0) {
        queryParts.push('offset=' + encodeURIComponent(currentOffset));
      }
      if (limit !== 100) {
        queryParts.push('limit=' + encodeURIComponent(limit));
      }
      var previewQuery = '?' + queryParts.join('&');
      var previewUrl = baseUrl + '/preview' + previewQuery;
      var retries = 0;

      function executeFetch() {
        if (thisGen !== generation || isDisposed) return;

        var reqAbort = new AbortController();
        activeAbortControllers.add(reqAbort);

        fetchAuthenticated(previewUrl, {
          method: 'GET',
          signal: reqAbort.signal
        }).then(function (res) {
          return res.json().catch(function () { return null; }).then(function (body) {
            return outcome(res.status, body);
          });
        }, function (err) {
          if (err.name === 'AbortError') throw err;
          return { code: 'unreachable' };
        }).then(function (result) {
          activeAbortControllers.delete(reqAbort);
          if (thisGen !== generation || isDisposed) return;

          // Both of the server's conversion slots are taken; it says when to ask again.
          var wait = result.retryAfter;
          if (result.code === 'preview_busy' && typeof wait === 'number' && wait > 0 && wait <= 10 && retries < MAX_RETRIES) {
            retries++;
            renderLoading('The server is busy making other previews. Trying again in ' + wait + (wait === 1 ? ' second…' : ' seconds…'));
            retryTimerId = setTimeout(function () {
              retryTimerId = null;
              executeFetch();
            }, wait * 1000);
            return;
          }
          if (!result.data) {
            renderFailure(result);
            return;
          }

          var payload = result.data;
          if (payload.kind === 'table') {
            if (payload.sheet) {
              currentSheet = payload.sheet;
            }
            renderTable(payload);
          } else if (payload.kind === 'pages') {
            renderPages(payload, thisGen, previewQuery);
          } else if (payload.kind === 'text') {
            renderText(payload);
          } else {
            renderFailure({ code: 'error', message: 'The server sent a kind of preview this page does not show: ' + payload.kind + '.' });
          }
        }).catch(function (err) {
          activeAbortControllers.delete(reqAbort);
          if (thisGen !== generation || isDisposed || err.name === 'AbortError') return;
          renderFailure({ code: 'error', message: err.message });
        });
      }

      executeFetch();
    }

    // Initial load
    loadPreview();

    return {
      dispose: function () {
        if (isDisposed) return;
        isDisposed = true;
        generation++;
        cancelPendingRequests();
        revokeAllBlobs();
        if (rootEl && rootEl.parentNode) {
          rootEl.parentNode.removeChild(rootEl);
        }
      },
      refresh: function () {
        if (isDisposed) return;
        // Preserves current page / sheet / view on refresh
        loadPreview();
      }
    };
  }

  window.BranchyardArtifactPreview = {
    mount: mount
  };
})();
