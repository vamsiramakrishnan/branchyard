// The Branchyard companion's service worker: keeps the page's own files
// for offline start-up, and shows Web Push notifications. It never caches
// or sees API responses, and it holds no token: a push message carries
// its own (encrypted) text, and a tap only opens the page.
'use strict';

const CACHE = 'branchyard-app-v1';
const SHELL = ['./', 'app.js', 'app.css', 'icon.svg', 'manifest.webmanifest'];

self.addEventListener('install', (event) => {
  event.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (event) => {
  event.waitUntil(caches.keys()
    .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
    .then(() => self.clients.claim()));
});

// The page's files: network first (so an upgraded server's page is used),
// the cache when offline. Everything else, the API included, is left alone.
self.addEventListener('fetch', (event) => {
  const url = new URL(event.request.url);
  if (event.request.method !== 'GET' || url.origin !== self.location.origin) return;
  const scope = new URL(self.registration.scope);
  if (!url.pathname.startsWith(scope.pathname) || url.pathname.startsWith('/v1/')) return;
  event.respondWith(fetch(event.request)
    .then((response) => {
      if (response.ok) {
        const copy = response.clone();
        caches.open(CACHE).then((c) => c.put(event.request, copy));
      }
      return response;
    })
    .catch(() => caches.match(event.request, { ignoreSearch: true })));
});

self.addEventListener('push', (event) => {
  let n = {};
  try { n = event.data ? event.data.json() : {}; } catch (_) { n = { body: event.data ? event.data.text() : '' }; }
  const title = n.title || 'Branchyard';
  event.waitUntil(self.registration.showNotification(title, {
    body: n.body || '',
    tag: n.tag || undefined,
    icon: 'icon.svg',
    data: { url: n.url || '#/' },
    requireInteraction: n.kind === 'permission' || n.kind === 'question',
  }));
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const hash = String((event.notification.data && event.notification.data.url) || '#/');
  // Only a fragment of this page: a payload cannot send the browser elsewhere.
  const target = new URL(self.registration.scope);
  target.hash = hash.startsWith('#') ? hash.slice(1) : '/';
  event.waitUntil(self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((windows) => {
    for (const w of windows) {
      if (w.url.startsWith(self.registration.scope) && 'focus' in w) {
        w.navigate(target.href).catch(() => {});
        return w.focus();
      }
    }
    return self.clients.openWindow(target.href);
  }));
});
