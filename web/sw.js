// sheprd service worker. The server rewrites CACHE to a hash of the web app, so each upgrade refreshes the shell.
const CACHE = 'sheprd-shell-v1';
const SHELL = ['/', 'index.html', 'app.js', 'md.js', 'ansi.js', 'sheets.js', 'search.js', 'files.js', 'v3.js', 'mb.js', 'res.js', 'res.css', 'fleet.js', 'fleet.css', 'git.js', 'git.css', 'voice.js', 'voice.css', 'art.js', 'art.css', 'talk.js', 'talk.css', 'queue.js', 'home.js', 'home.css', 'repos.js', 'repos.css', 'style.css', 'manifest.webmanifest',
  'icon-180.png', 'icon-192.png', 'icon-512.png', 'icon-maskable-512.png'];

self.addEventListener('install', e => {
  self.skipWaiting(); // activate at once: the shell is network-first, so a new worker never mixes builds
  e.waitUntil(caches.open(CACHE).then(c => c.addAll(SHELL.map(u => new Request(u, { cache: 'no-cache' })))).catch(() => { }));
});
self.addEventListener('activate', e => {
  e.waitUntil(caches.keys().then(ks => Promise.all(ks.filter(k => k !== CACHE).map(k => caches.delete(k))))
    .then(() => self.clients.claim()));
});
self.addEventListener('fetch', e => {
  const u = new URL(e.request.url);
  if (e.request.method !== 'GET' || u.origin !== location.origin || u.pathname.startsWith('/api/') || u.pathname.startsWith('/mm-approve')) return; // network only
  // Network first (revalidated: the server sends no-cache + ETag, so this is a 304 when nothing changed); the
  // cache is only the offline fallback. Cache-first served the old shell after a deploy until the new worker
  // took over, so a reload could boot the old build again (web/app.js self-update).
  const nav = e.request.mode === 'navigate';
  // (a navigation that hangs on a dead network falls back after 5 s; the page then sees an old build and reloads)
  const net = nav ? Promise.race([fetch(e.request), new Promise((_, rej) => setTimeout(() => rej(new Error('timeout')), 5000))])
    : fetch(e.request, { cache: 'no-cache' });
  e.respondWith(net.then(res => {
    if (res.ok && res.type === 'basic') { const cp = res.clone(); caches.open(CACHE).then(c => c.put(nav ? '/' : e.request, cp)); }
    return res;
  }).catch(err => caches.match(nav ? '/' : e.request, { ignoreSearch: true }).then(r => r || Promise.reject(err))));
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
