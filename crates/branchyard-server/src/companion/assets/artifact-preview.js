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

  function formatBytes(bytes) {
    if (typeof bytes !== 'number' || isNaN(bytes)) return '';
    if (bytes < 1024) return bytes + ' B';
    var kb = bytes / 1024;
    if (kb < 1024) return kb.toFixed(1) + ' KB';
    var mb = kb / 1024;
    return mb.toFixed(1) + ' MB';
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
    var pendingRevokeTimerIds = new Set();
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
      // Clear pending delayed revocation timers
      pendingRevokeTimerIds.forEach(function (tid) {
        clearTimeout(tid);
      });
      pendingRevokeTimerIds.clear();

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

      // Download Original Button (always available)
      var downloadBtn = document.createElement('button');
      downloadBtn.type = 'button';
      downloadBtn.className = 'by-preview-btn by-preview-btn-primary';
      downloadBtn.textContent = 'Download Original';
      downloadBtn.setAttribute('aria-label', 'Download original file');
      downloadBtn.addEventListener('click', function () {
        handleDownloadOriginal();
      });
      rightGroup.appendChild(downloadBtn);

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
      text.textContent = msg || 'Preparing preview...';
      container.appendChild(text);

      contentEl.appendChild(container);
      announce(text.textContent);
    }

    function renderUnavailableOrError(titleMsg, detailNext, showSetup, showRetry) {
      contentEl.textContent = '';
      toolbarEl.textContent = '';
      renderWarnings([]);

      var container = document.createElement('div');
      container.className = 'by-preview-state-container';

      var titleEl = document.createElement('div');
      titleEl.className = 'by-preview-state-message';
      titleEl.textContent = titleMsg || 'Preview unavailable';
      container.appendChild(titleEl);

      if (detailNext) {
        var subtext = document.createElement('div');
        subtext.className = 'by-preview-state-subtext';
        subtext.textContent = detailNext;
        container.appendChild(subtext);
      }

      var actions = document.createElement('div');
      actions.className = 'by-preview-state-actions';

      if (showRetry) {
        var retryBtn = document.createElement('button');
        retryBtn.type = 'button';
        retryBtn.className = 'by-preview-btn';
        retryBtn.textContent = 'Retry Preview';
        retryBtn.addEventListener('click', function () {
          loadPreview();
        });
        actions.appendChild(retryBtn);
      }

      if (showSetup) {
        var setupBtn = document.createElement('button');
        setupBtn.type = 'button';
        setupBtn.className = 'by-preview-btn';
        setupBtn.textContent = 'Setup Instructions';
        setupBtn.addEventListener('click', function () {
          alert('Setup action: ' + (detailNext || 'Please configure the required preview service.'));
        });
        actions.appendChild(setupBtn);
      }

      var dlBtn = document.createElement('button');
      dlBtn.type = 'button';
      dlBtn.className = 'by-preview-btn by-preview-btn-primary';
      dlBtn.textContent = 'Download Original';
      dlBtn.addEventListener('click', function () {
        handleDownloadOriginal();
      });
      actions.appendChild(dlBtn);

      container.appendChild(actions);
      contentEl.appendChild(container);
      announce(titleEl.textContent);
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

      announce('Loaded table preview, ' + rows.length + ' rows displayed.');
    }

    function renderPages(data, currentGen, previewQuery) {
      renderToolbar(data);
      renderWarnings(data.warnings);
      contentEl.textContent = '';

      var imageFilename = data.image;
      if (!imageFilename || !isSafeBasename(imageFilename)) {
        renderUnavailableOrError('Security Error: Invalid or unsafe page asset reference.', null, false, false);
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
          throw new Error('Failed to load page image: ' + res.status);
        }
        var ctype = res.headers && res.headers.get ? (res.headers.get('content-type') || '') : '';
        if (ctype.toLowerCase().indexOf('image/png') === -1) {
          throw new Error('Unsafe content type for page preview: ' + ctype);
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
        announce('Loaded page ' + (data.page || currentPage) + ' of ' + (data.pages || 1));
      }).catch(function (err) {
        activeAbortControllers.delete(assetAbort);
        if (currentGen !== generation || isDisposed) return;
        if (err.name === 'AbortError') return;
        pagesContainer.textContent = '';
        var errBox = document.createElement('div');
        errBox.className = 'by-preview-state-message';
        errBox.textContent = 'Error loading page visual: ' + err.message;
        pagesContainer.appendChild(errBox);
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
      announce('Loaded text preview.');
    }

    function handleDownloadOriginal() {
      if (isDisposed) return;
      var contentPath = baseUrl + '/content';
      var dlAbort = new AbortController();
      activeAbortControllers.add(dlAbort);

      fetchAuthenticated(contentPath, {
        method: 'GET',
        signal: dlAbort.signal
      }).then(function (res) {
        activeAbortControllers.delete(dlAbort);
        if (isDisposed) return;
        if (!res.ok) {
          throw new Error('Download failed: ' + res.status);
        }
        return res.blob();
      }).then(function (blob) {
        if (isDisposed || !blob) return;
        var blobUrl = URL.createObjectURL(blob);
        activeBlobUrls.push(blobUrl);

        var a = document.createElement('a');
        a.style.display = 'none';
        a.href = blobUrl;
        a.download = displayName || 'download';
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);

        var timerId = setTimeout(function () {
          pendingRevokeTimerIds.delete(timerId);
          try {
            URL.revokeObjectURL(blobUrl);
            var idx = activeBlobUrls.indexOf(blobUrl);
            if (idx !== -1) {
              activeBlobUrls.splice(idx, 1);
            }
          } catch (e) {
            // ignore
          }
        }, 1000);
        pendingRevokeTimerIds.add(timerId);
      }).catch(function (err) {
        activeAbortControllers.delete(dlAbort);
        if (isDisposed || err.name === 'AbortError') return;
        alert('Could not download original document: ' + err.message);
      });
    }

    function loadPreview() {
      if (isDisposed) return;

      cancelPendingRequests();
      revokeAllBlobs();

      var thisGen = ++generation;
      var fetchAbort = new AbortController();
      activeAbortControllers.add(fetchAbort);

      renderLoading('Loading document preview...');

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

      var retryCount = 0;
      var MAX_RETRIES = 5;

      function executeFetch() {
        if (thisGen !== generation || isDisposed) return;

        var reqAbort = new AbortController();
        activeAbortControllers.add(reqAbort);

        fetchAuthenticated(previewUrl, {
          method: 'GET',
          signal: reqAbort.signal
        }).then(function (res) {
          activeAbortControllers.delete(reqAbort);
          if (thisGen !== generation || isDisposed) return;

          // 202 Accepted: Still preparing preview with Retry-After
          if (res.status === 202) {
            retryCount++;
            if (retryCount > MAX_RETRIES) {
              renderUnavailableOrError('Preview generation timed out.', 'The preview took too long to prepare.', false, true);
              return;
            }
            var retrySec = 1;
            var headerVal = res.headers && res.headers.get ? res.headers.get('Retry-After') : null;
            if (headerVal) {
              var parsed = parseInt(headerVal, 10);
              if (!isNaN(parsed) && parsed > 0 && parsed <= 10) {
                retrySec = parsed;
              }
            }
            renderLoading('Preparing preview (attempt ' + retryCount + ')...');
            retryTimerId = setTimeout(function () {
              retryTimerId = null;
              if (thisGen === generation && !isDisposed) {
                executeFetch();
              }
            }, retrySec * 1000);
            return;
          }

          return res.json().then(function (body) {
            return { ok: res.ok, status: res.status, headers: res.headers, body: body };
          }).catch(function (parseErr) {
            return { ok: res.ok, status: res.status, headers: res.headers, body: null };
          });
        }).then(function (result) {
          if (!result || thisGen !== generation || isDisposed) return;

          // Handle 429 Too Many Requests (e.g. preview_busy)
          if (result.status === 429) {
            var body = result.body || {};
            var errObj = body.error || body;
            var isBusy = errObj.code === 'preview_busy' || (errObj.detail && errObj.detail.retry_after != null);
            var retryAfterSec = null;

            if (errObj.detail && typeof errObj.detail.retry_after === 'number') {
              retryAfterSec = errObj.detail.retry_after;
            } else if (result.headers && result.headers.get && result.headers.get('Retry-After')) {
              var hSec = parseInt(result.headers.get('Retry-After'), 10);
              if (!isNaN(hSec) && hSec > 0) {
                retryAfterSec = hSec;
              }
            }

            if (isBusy && retryAfterSec !== null && retryAfterSec > 0 && retryAfterSec <= 10 && retryCount < MAX_RETRIES) {
              retryCount++;
              renderLoading('Preview service busy; retrying in ' + retryAfterSec + 's (attempt ' + retryCount + ')...');
              retryTimerId = setTimeout(function () {
                retryTimerId = null;
                if (thisGen === generation && !isDisposed) {
                  executeFetch();
                }
              }, retryAfterSec * 1000);
              return;
            }

            var busyMsg = errObj.message || 'Preview service is busy.';
            var busyNext = (errObj.detail && errObj.detail.next) || 'Please retry in a moment.';
            renderUnavailableOrError(busyMsg, busyNext, false, true);
            return;
          }

          if (!result.ok) {
            var errMsg = 'Preview request failed (' + result.status + ')';
            var nextAction = null;
            if (result.body && result.body.error) {
              errMsg = result.body.error.message || errMsg;
              if (result.body.error.detail && result.body.error.detail.next) {
                nextAction = result.body.error.detail.next;
              }
            }
            renderUnavailableOrError(errMsg, nextAction, Boolean(nextAction), result.status >= 500);
            return;
          }

          var payload = result.body;
          if (!payload) {
            renderUnavailableOrError('Empty preview response received.', null, false, false);
            return;
          }

          if (payload.status === 'failed' || payload.status === 'unavailable') {
            var msg = payload.message || 'Preview unavailable for this artifact.';
            var detailNext = (payload.detail && payload.detail.next) || (payload.error && payload.error.detail && payload.error.detail.next) || null;
            var showSetup = Boolean(payload.setup || detailNext);
            renderUnavailableOrError(msg, detailNext, showSetup, false);
            return;
          }

          if (payload.status === 'passed') {
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
              renderUnavailableOrError('Unsupported preview kind: ' + payload.kind, null, false, false);
            }
          } else {
            renderUnavailableOrError('Unexpected preview status: ' + payload.status, null, false, false);
          }
        }).catch(function (err) {
          activeAbortControllers.delete(reqAbort);
          if (thisGen !== generation || isDisposed) return;
          if (err.name === 'AbortError') return;
          renderUnavailableOrError('Preview error: ' + err.message, null, false, true);
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
