// Minimal service worker: shell cache only. SOS and incident calls are NEVER
// served from cache (contract: offline diary != active protection).
const SHELL = ['/worker/', '/worker/index.html', '/style.css', '/worker/manifest.webmanifest'];
self.addEventListener('install', e => e.waitUntil(caches.open('yw-shell-v1').then(c => c.addAll(SHELL))));
self.addEventListener('fetch', e => {
  const u = new URL(e.request.url);
  if (u.pathname.startsWith('/api/')) return; // always network, never cached
  e.respondWith(caches.match(e.request).then(r => r || fetch(e.request)));
});
