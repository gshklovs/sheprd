// Lazy: artifacts and outputs. Claude Artifact publishes and viewable files an agent wrote (html, md, pdf,
// svg, images, csv) as rich cards in chats, the newest one as a chip on agent cards / the workers tray /
// the switcher. A published artifact (it has a claude.ai URL) opens on claude.ai — the Claude app via universal
// link on iOS — because the in-app preview can't run its runtime capabilities (db, …) and breaks its layout; the
// fullscreen preview is for local outputs (and "Preview here" in the long-press menu). Previews are the local source in a sandboxed iframe
// (sandbox="allow-scripts", no same-origin; the server adds a CSP that blocks every request back to it).
// Only one live preview at a time, loaded when visible. API.md "Artifacts".
import { G, I, $, h, esc, api, toast, haptic, copyText, itemHref, nav, popMenu, mediaViewer, dropEmpty } from './app.js';
import { fileUrl, docUrl, mediaEl } from './md.js';

await new Promise(r => {
  if (document.querySelector('link[data-art]')) return r();
  const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'art.css'; l.dataset.art = 1;
  l.onload = l.onerror = r; document.head.append(l); setTimeout(r, 3000);
});

const enc = encodeURIComponent;
const ICON = {
  chart: '📊', graph: '📈', dashboard: '📊', list: '📋', table: '▦', calendar: '📅', recipe: '🍳', code: '⌨️', map: '🗺️',
  doc: '📄', document: '📄', report: '📄', notes: '📝', slides: '🖼️', deck: '🖼️', game: '🎮', music: '🎵', money: '💵',
  cart: '🛒', shopping: '🛒', travel: '✈️', health: '🩺', book: '📖', idea: '💡', check: '✅', form: '🧾', timer: '⏱️',
  printer: '🖨️', print: '🖨️', compare: '⚖️', video: '🎬', link: '🔗', docs: '📄', page: '📄',
  design: '🎨', palette: '🎨', photo: '🖼️', image: '🖼️', home: '🏠', star: '⭐', tool: '🛠️', settings: '⚙️', search: '🔎',
};
const EXT = /\.([a-z0-9]+)$/i;
const extOf = p => (String(p || '').match(EXT)?.[1] || '').toLowerCase();
const TYPE = { html: 'html', htm: 'html', md: 'md', markdown: 'md', pdf: 'pdf', svg: 'img', png: 'img', jpg: 'img', jpeg: 'img', webp: 'img', gif: 'img', csv: 'csv', mp4: 'video', m4v: 'video', mov: 'video', webm: 'video' };
export const typeOf = a => a.kind === 'artifact' && !a.path ? 'html' : TYPE[extOf(a.path)] || 'other';
const icoOf = a => {
  if (a.icon) return ICON[String(a.icon).toLowerCase()] || '✦';
  return { html: '🌐', md: '📄', pdf: '📕', img: '🖼️', csv: '▦', video: '🎬' }[typeOf(a)] || '📄';
};
const fname = p => String(p || '').split('/').pop();

// ---------- open / act ----------
export const openClaude = a => window.open(a.url, '_blank', 'noopener');
export function open(a) {
  if (!a) return;
  if (a.url) openClaude(a); else openLocal(a);
}
function openLocal(a) {
  const t = typeOf(a);
  if (t === 'img') { showImg(fileUrl(a.path)); return; }
  if (t === 'video') { mediaViewer(fileUrl(a.path)); return; }
  if (t === 'md' || t === 'csv') { nav(docUrl(a.path)); return; }
  if (t === 'pdf') { window.open(fileUrl(a.path), '_blank', 'noopener'); return; }
  preview(a);
}
// the app's photo viewer: a one-image "gallery" box the global img.thumb handler opens
function showImg(src) {
  const box = h(`<div class="sheet" hidden><img class="thumb" alt="" src="${esc(src)}"></div>`);
  document.body.append(box); $('img', box).click(); box.remove();
}
const jump = a => nav(itemHref(a.tid, a.i));
function menu(anchor, a) {
  const items = a.url
    ? [['Open in Claude', () => openClaude(a)], ...a.path ? [['Preview here', () => preview(a)]] : [], ['Copy link', () => copyText(a.url)]]
    : [['Preview', () => openLocal(a)], ...a.path ? [['Copy path', () => copyText(a.path)]] : []];
  if (a.tid && a.i != null) items.push(['Jump to message', () => jump(a)]);
  popMenu(anchor, items);
}

// Fullscreen preview: the live page, close by ✕, Escape, or swiping the bar down.
let fsEl = null;
export function preview(a) {
  fsEl?.close();
  const src = a.path ? fileUrl(a.path) : null;
  const ov = h(`<div class="artfs">
    <div class="af-bar">
      <button class="ib af-x" aria-label="Close">${I.x}</button>
      <span class="af-ico">${icoOf(a)}</span>
      <div class="af-t"><b>${esc(a.title || fname(a.path) || 'Artifact')}</b><small>${a.version ? 'Version ' + a.version + ' · ' : ''}${esc(a.kind === 'artifact' ? 'Artifact' : fname(a.path))}</small></div>
      ${a.url ? `<a class="btn sm af-open" href="${esc(a.url)}" target="_blank" rel="noopener">Open in Claude</a><button class="ib af-copy" aria-label="Copy link">${I.copy}</button>` : ''}
      ${src ? `<a class="ib" href="${esc(src)}" target="_blank" rel="noopener" aria-label="Open raw">${I.ext}</a>` : ''}
    </div>
    <div class="af-body">${src ? `<iframe sandbox="allow-scripts" referrerpolicy="no-referrer" title="${esc(a.title || 'Artifact')}"></iframe>` : `<div class="af-none">${noSource(a)}</div>`}</div>
  </div>`);
  const close = () => { if (fsEl === ov) fsEl = null; ov.classList.add('out'); setTimeout(() => ov.remove(), 180); removeEventListener('keydown', onKey); };
  ov.close = close;
  const onKey = e => { if (e.key === 'Escape') close(); };
  addEventListener('keydown', onKey);
  $('.af-x', ov).onclick = close;
  const cp = $('.af-copy', ov); if (cp) cp.onclick = () => copyText(a.url);
  if (src) exists(a.path).then(ok => {
    if (!ok) { $('.af-body', ov).innerHTML = `<div class="af-none">${noSource(a)}</div>`; return; }
    $('iframe', ov).src = src;
  });
  const bar = $('.af-bar', ov);
  let y0 = null, dy = 0;
  bar.addEventListener('touchstart', e => { y0 = e.touches.length === 1 ? e.touches[0].clientY : null; dy = 0; }, { passive: true });
  bar.addEventListener('touchmove', e => {
    if (y0 == null) return; dy = e.touches[0].clientY - y0;
    if (dy > 0) { ov.style.transition = 'none'; ov.style.transform = `translateY(${dy}px)`; }
  }, { passive: true });
  bar.addEventListener('touchend', () => { if (y0 == null) return; ov.style.transition = ''; ov.style.transform = ''; if (dy > 90) close(); y0 = null; });
  document.body.append(ov); fsEl = ov;
  requestAnimationFrame(() => requestAnimationFrame(() => ov.classList.add('in')));
}
const noSource = a => `<p>The source file isn’t on this machine any more.</p>${a.url ? `<a class="btn" href="${esc(a.url)}" target="_blank" rel="noopener">Open in Claude</a>` : ''}`;

const EX = new Map(); // path → Promise<bool>
function exists(p) {
  if (!EX.has(p)) EX.set(p, fetch(fileUrl(p), { method: 'HEAD' }).then(r => r.ok, () => false));
  return EX.get(p);
}

// ---------- chips: agent cards, tray, switcher ----------
const artOf = pane => G.st?.agents?.find(a => a.pane === pane)?.artifact;
export function chip(a, mode) {
  const x = a.artifact; if (!x) return '';
  const img = typeOf(x) === 'img' && x.path;
  // agent rows: 'thumb' = image thumbnail on the right (images only), 'inline' = compact chip leading the outcome line
  const at = `role="button" data-pane="${esc(a.pane)}" data-k="${esc(x.tid + ':' + x.i)}" aria-label="${esc(x.title)}"`;
  if (mode === 'thumb') return img ? `<span class="artchip athumb" ${at}><img src="${esc(fileUrl(x.path))}" alt="" loading="lazy"></span>` : '';
  if (mode === 'inline') return `<span class="artchip ainl" ${at}><i>${icoOf(x)}</i><b>${esc(x.title)}</b></span>`;
  return `<span class="artchip" role="button" data-pane="${esc(a.pane)}" data-k="${esc(x.tid + ':' + x.i)}">
    ${img ? `<img src="${esc(fileUrl(x.path))}" alt="" loading="lazy">` : `<i>${icoOf(x)}</i>`}
    <span class="sb"><b>${esc(x.title)}</b><small>${x.kind === 'artifact' ? `Artifact${x.version ? ' · v' + x.version : ''}` : esc(fname(x.path))}</small></span><span class="go">${x.url ? '↗' : '›'}</span></span>`;
}
export const mini = a => a.artifact ? `<span class="artmini" role="button" data-pane="${esc(a.pane)}" aria-label="${esc(a.artifact.title)}">${icoOf(a.artifact)}</span>` : '';

let lpT = 0, lpDone = false;
const CHIP = '.artchip, .artmini, .artc .ac-pv, .artc .ac-link'; // agent-card chip, tray/switcher mini, chat-card thumbnail / link card
const artFor = c => c.dataset.pane ? artOf(c.dataset.pane) : c.closest('.artc')?.art;
document.addEventListener('pointerdown', e => {
  const c = e.target.closest(CHIP); if (!c) return;
  e.stopPropagation(); // not the card's own long-press
  clearTimeout(lpT); lpDone = false;
  const x0 = e.clientX, y0 = e.clientY;
  const mv = ev => { if (Math.hypot(ev.clientX - x0, ev.clientY - y0) > 8) clearTimeout(lpT); };
  addEventListener('pointermove', mv);
  addEventListener('pointerup', () => { clearTimeout(lpT); removeEventListener('pointermove', mv); }, { once: true });
  lpT = setTimeout(() => { lpDone = true; haptic(); const a = artFor(c); if (a) menu(c, a); }, 450);
}, true);
document.addEventListener('contextmenu', e => { if (e.target.closest(CHIP)) e.preventDefault(); }, true);
document.addEventListener('click', e => {
  const c = e.target.closest(CHIP); if (!c) return;
  e.preventDefault(); e.stopPropagation();
  if (lpDone) { lpDone = false; return; } // the click ending a long-press
  const a = artFor(c);
  if (a) open(a); else toast('No artifact');
}, true);

// ---------- chat cards ----------
// Server list per transcript (titles from the source file, which outputs count), refetched on demand.
const LISTS = new Map(); // tid → { at, p }
function list(tid, fresh) {
  const c = LISTS.get(tid);
  if (c && (!fresh || Date.now() - c.at < 1500)) return c.p;
  const p = api(`t/${enc(tid)}/artifacts`).then(r => r.artifacts || [], () => []);
  LISTS.set(tid, { at: Date.now(), p });
  return p;
}
const PUB = /^\s*Published (.+?) at (https:\/\/claude\.ai\/\S+?)[.,)]?(?:\s|$)/;
const ART_URL = /https:\/\/claude\.ai\/(?:code\/)?artifact\/[\w-]+/;
// What a successful publish result says: the "Published <path> at <url> (Version N…) Icon: "x"" line (claude.ai/artifact/<id>
// or claude.ai/code/artifact/<uuid>), or a JSON result {url, file_path|path, version, icon}. Else null.
export function parsePub(text) {
  text = String(text || '');
  const m = text.match(PUB);
  if (m) return { url: m[2], path: m[1].startsWith('/') ? m[1] : null, version: +(text.match(/\(Version (\d+)/)?.[1]) || null, icon: text.match(/Icon: "([^"]+)"/)?.[1] || null };
  try {
    const j = JSON.parse(text);
    if (j && typeof j.url === 'string' && ART_URL.test(j.url) && j.url.startsWith('https://claude.ai/'))
      return { url: j.url, path: String(j.file_path || j.path || '').startsWith('/') ? j.file_path || j.path : null, version: +j.version || null, icon: j.icon || null };
  } catch { }
  return null;
}
// title: the tool input's title, else the source's <title> (server list, painted in after), else the file name
export function fromItem(it, r) {
  const x = it.input || {}, p = r && !r.err ? parsePub(r.text) : null;
  return {
    kind: 'artifact', title: x.title || fname(x.file_path || p?.path) || 'Artifact', description: x.description || '',
    icon: x.icon || p?.icon || null, path: x.file_path || p?.path || null,
    url: p?.url || x.url || null, version: p?.version || null, i: it.i, tid: null,
  };
}
const host = u => { try { return new URL(u).host; } catch { return 'claude.ai'; } };
// A claude.ai publish: one compact link row (icon, title, host, Open). Tap / Open → claude.ai (the Claude app via
// universal link on iOS); long-press → Open / Preview here / Copy link. claude.ai is never framed.
export function linkHTML(a, st) {
  const sub = st === 'run' ? '<span class="spin"></span> Publishing…' : st === 'err' ? '<span class="ac-err">Publish failed</span>'
    : `${esc(host(a.url))}${a.version > 1 ? ' · v' + a.version : ''}`;
  return `<div class="ac-link"${a.url ? ' role="button" aria-label="Open in Claude"' : ''}><span class="ac-ico">${icoOf(a)}</span><div class="ac-tx"><b>${esc(a.title)}</b><small>${sub}</small></div>${a.url ? `<a class="btn sm ac-open" href="${esc(a.url)}" target="_blank" rel="noopener">Open</a>` : ''}</div>`;
}
function cardHTML(a, st) {
  if (a.kind === 'artifact' && (a.url || st !== 'ok')) return linkHTML(a, st);
  if (!a.title && !a.path && !a.url) return '';
  const t = typeOf(a);
  const meta = `<div class="ac-meta"><span class="ac-ico">${icoOf(a)}</span><div class="ac-tx"><b>${esc(a.title)}</b>${a.description && a.description !== a.title ? `<small>${esc(a.description)}</small>` : ''}</div>${a.version ? `<span class="ac-v">v${a.version}</span>` : ''}</div>`;
  if (t === 'md' || t === 'csv') return `<a class="ac-doc" href="${esc(docUrl(a.path))}">${meta}<span class="ac-f">${esc(fname(a.path))}</span><span class="go">›</span></a>`;
  if (t === 'pdf') return `<a class="ac-doc" href="${esc(fileUrl(a.path))}" target="_blank" rel="noopener">${meta}<span class="ac-f">${esc(fname(a.path))}</span><span class="go">›</span></a>`;
  if (t === 'video') return `${meta}<div class="thumbs">${mediaEl(fileUrl(a.path))}</div>`;
  if (t === 'img') return `${meta}<div class="thumbs"><img class="thumb" loading="lazy" src="${esc(fileUrl(a.path))}" alt=""></div>`;
  const pv = a.path ? `<div class="ac-pv" role="button" aria-label="${a.url ? 'Open in Claude' : 'Preview'}"><div class="ac-ph"><span>${icoOf(a)}</span><b>${esc(a.title)}</b><small>${a.url ? 'Tap to open in Claude' : 'Tap to preview'}</small></div></div>` : '';
  const acts = st === 'run' ? `<div class="ac-st"><span class="spin"></span> Publishing…</div>`
    : st === 'err' ? `<div class="ac-st err">Publish failed</div>`
    : `<div class="ac-act">${a.url ? `<a class="btn sm" href="${esc(a.url)}" target="_blank" rel="noopener">Open in Claude</a><button class="btn sm ac-copy">${I.copy}Copy link</button>` : a.path ? `<button class="btn sm ac-full">Preview</button>` : ''}</div>`;
  return `${pv}${meta}${acts}`;
}
function bind(el, a) {
  el.art = a;
  const pv = $('.ac-pv', el); if (pv) PV.watch(pv, el); // tap / long-press: the delegated CHIP handlers
  const cp = $('.ac-copy', el); if (cp) cp.onclick = () => copyText(el.art.url);
  const fb = $('.ac-full', el); if (fb) fb.onclick = () => preview(el.art);
}
// `el` is the placeholder app.js put in place of an Artifact tool row; `el.res` arrives via `el.onres`.
export function card(el, view) {
  const draw = () => {
    const r = el.res, a = fromItem(el.item, r);
    a.tid = view.tid;
    const st = !r ? 'run' : r.err || !a.url ? 'err' : 'ok';
    const paint = b => { const html = cardHTML(b, st); if (html !== el._h) { el._h = html; PV.drop(el); el.innerHTML = html; bind(el, b); dropEmpty(el); } };
    paint(a);
    if (st === 'ok' && view.tid) list(view.tid, !el._fresh).then(ls => {
      el._fresh = 1;
      const s = ls.find(x => x.url === a.url && x.tid === view.tid && x.i === a.i) || ls.find(x => (a.url && x.url === a.url) || (a.path && x.path === a.path));
      if (s) paint({ ...a, title: el.item.input?.title || s.title || a.title, icon: a.icon || s.icon, description: a.description || s.description || '' });
    });
  };
  el.classList.add('artc');
  el.onres = draw; draw();
}
// A Write row: when the server counts the file as an output (and this is its newest write), add a card after the step.
export function output(row, view) {
  const check = fresh => {
    if (!row.res || row.res.err || row._art || !view.tid) return;
    list(view.tid, fresh).then(ls => {
      const p = row.item.input?.file_path;
      const a = ls.find(x => x.kind === 'file' && x.tid === view.tid && x.i === row.item.i && x.path === p);
      if (!a || row._art || !row.isConnected) return;
      const el = h(`<div class="artc out" data-i="${row.item.i}"></div>`);
      el.innerHTML = cardHTML(a, 'ok'); bind(el, a); if (!el.innerHTML.trim()) return; row._art = el;
      (row.closest('.grp') || row).after(el);
    });
  };
  row.onres = () => check(true);
  check(false);
}

// ---------- one live preview at a time, only while visible ----------
const PV = (() => {
  const vis = new Map(); // pv → ratio
  let live = null, t = 0;
  const VW = 640, H = 210;
  const io = new IntersectionObserver(es => {
    for (const e of es) if (e.isIntersecting && e.intersectionRatio > 0.15) vis.set(e.target, e.intersectionRatio); else vis.delete(e.target);
    clearTimeout(t); t = setTimeout(pick, 160);
  }, { threshold: [0, 0.15, 0.5, 0.9] });
  function pick() {
    let best = null, br = 0;
    for (const [el, r] of vis) if (el.isConnected && r >= br) { best = el; br = r; }
    if (best === live) return;
    unload(); if (best) load(best);
  }
  function unload() {
    if (!live) return;
    live.querySelector('iframe')?.remove(); live.classList.remove('live'); live = null;
  }
  function load(pv) {
    const a = pv._card?.art; if (!a?.path) return;
    live = pv;
    exists(a.path).then(ok => {
      if (live !== pv) return;
      if (!ok) { pv.classList.add('gone'); if (!a.url) $('.ac-ph small', pv).textContent = 'Source file no longer on this machine'; live = null; return; }
      const w = pv.clientWidth || 320, k = w / VW;
      const f = h(`<iframe sandbox="allow-scripts" referrerpolicy="no-referrer" tabindex="-1" aria-hidden="true" loading="lazy"></iframe>`);
      f.style.width = VW + 'px'; f.style.height = Math.ceil(H / k) + 'px'; f.style.transform = `scale(${k})`;
      f.onload = () => pv.classList.add('live');
      f.src = fileUrl(a.path);
      pv.append(f);
    });
  }
  return {
    watch(pv, cardEl) { pv._card = cardEl; io.observe(pv); },
    drop(cardEl) { const pv = $('.ac-pv', cardEl); if (!pv) return; io.unobserve(pv); vis.delete(pv); if (live === pv) unload(); },
  };
})();

G.art = { chip, mini, open, openClaude, preview };
