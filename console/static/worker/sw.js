// Minimal service worker: shell cache only. /api/* is ALWAYS network, never cached
// (contract: offline diary != active protection; SOS/incidents must be live).
const SHELL = ['/worker/', '/worker/index.html', '/app.css', '/app.js', '/worker/manifest.webmanifest'];
self.addEventListener('install', e => { self.skipWaiting(); e.waitUntil(caches.open('yw-shell-v4').then(c => c.addAll(SHELL))); });
self.addEventListener('activate', e => e.waitUntil(caches.keys().then(ks => Promise.all(ks.filter(k=>k!=='yw-shell-v4').map(k=>caches.delete(k))))));
self.addEventListener('fetch', e => {
  const u = new URL(e.request.url);
  if (u.pathname.startsWith('/api/')) return;              // live only
  e.respondWith(caches.match(e.request).then(r => r || fetch(e.request)));
});
