// sheprd service worker. Bump CACHE on every shell change.
const CACHE = 'sheprd-shell-v1';
const SHELL = ['/', 'index.html', 'app.js', 'md.js', 'ansi.js', 'sheets.js', 'search.js', 'files.js', 'v3.js', 'mb.js', 'res.js', 'res.css', 'fleet.js', 'fleet.css', 'git.js', 'git.css', 'voice.js', 'voice.css', 'art.js', 'art.css', 'talk.js', 'talk.css', 'queue.js', 'home.js', 'home.css', 'style.css', 'manifest.webmanifest',
  'icon-180.png', 'icon-192.png', 'icon-512.png', 'icon-maskable-512.png'];

self.addEventListener('install', e => {
  e.waitUntil(caches.open(CACHE).then(c => c.addAll(SHELL)).then(() => self.skipWaiting()));
});
self.addEventListener('activate', e => {
  e.waitUntil(caches.keys().then(ks => Promise.all(ks.filter(k => k !== CACHE).map(k => caches.delete(k))))
    .then(() => self.clients.claim()));
});
self.addEventListener('fetch', e => {
  const u = new URL(e.request.url);
  if (e.request.method !== 'GET' || u.origin !== location.origin || u.pathname.startsWith('/api/') || u.pathname.startsWith('/mm-approve')) return; // network only
  const key = e.request.mode === 'navigate' ? '/' : e.request;
  e.respondWith(caches.match(key, { ignoreSearch: true }).then(r => r || fetch(e.request).then(res => {
    if (res.ok) { const cp = res.clone(); caches.open(CACHE).then(c => c.put(e.request, cp)); }
    return res;
  })));
});

self.addEventListener('push', e => {
  let d = {};
  try { d = e.data ? e.data.json() : {}; } catch { d = { title: 'sheprd', body: e.data?.text() }; }
  e.waitUntil(self.registration.showNotification(d.title || 'sheprd', {
    body: d.body || '', tag: d.tag || undefined, renotify: !!d.tag,
    data: { url: d.url || '/' }, icon: 'icon-192.png', badge: 'icon-192.png',
  }));
});
self.addEventListener('notificationclick', e => {
  e.notification.close();
  // always the app (the sending agent's chat, at its item), never an external link
  let url = new URL(e.notification.data?.url || '/', self.location.origin);
  if (url.origin !== self.location.origin) url = new URL('/#/', self.location.origin);
  e.waitUntil((async () => {
    const cs = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    for (const c of cs) {
      if (new URL(c.url).origin === url.origin) {
        await c.focus();
        c.postMessage({ nav: url.hash || '#/' });
        return;
      }
    }
    await self.clients.openWindow(url.href);
  })());
});
