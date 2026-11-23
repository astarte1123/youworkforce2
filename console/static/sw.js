// Service worker YouWorkforce Console — strategia NETWORK-FIRST.
// Scopo: rendere la console installabile come PWA (icona su telefono/tablet, a schermo intero,
// camera posteriore per lo scanner) SENZA il problema della cache "vecchia": quando c'è rete la
// risposta viene SEMPRE dalla rete (niente app stantia); la cache serve solo da fallback offline.
// Le chiamate /api non vengono MAI messe in cache.
const RT = 'yw-rt-v1';

self.addEventListener('install', (e) => { self.skipWaiting(); });

self.addEventListener('activate', (e) => {
  e.waitUntil((async () => {
    const keys = await caches.keys();
    await Promise.all(keys.filter(k => k !== RT).map(k => caches.delete(k)));
    await self.clients.claim();
  })());
});

self.addEventListener('fetch', (e) => {
  const req = e.request;
  if (req.method !== 'GET') return;                 // POST/PUT/… passano dritti
  const url = new URL(req.url);
  if (url.pathname.startsWith('/api/')) return;      // API mai in cache (dati live)
  e.respondWith((async () => {
    try {
      const net = await fetch(req);
      if (net && net.ok && url.origin === location.origin) {
        const c = await caches.open(RT); c.put(req, net.clone());   // copia per offline
      }
      return net;
    } catch (err) {
      const c = await caches.open(RT);
      const hit = await c.match(req);
      if (hit) return hit;
      if (req.mode === 'navigate') { const app = await c.match('/app.html'); if (app) return app; }
      throw err;
    }
  })());
});
