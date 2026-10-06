// sheprd — iPhone client for herdr-managed Claude agents.
import { codeHTML, md, mdx, esc, textx, linkify, thumbs, L, fixUrl, fileUrl, IMG_EXT, pathLink } from './md.js';
import { ansi, stripAnsi } from './ansi.js';

const VERSION = 'v2';
const $ = (s, r = document) => r.querySelector(s);
const enc = encodeURIComponent;
const tpl = document.createElement('template');
const h = html => { tpl.innerHTML = html.trim(); return tpl.content.firstElementChild; };
// Patch `old` in place to match `nu`: only changed text/attributes are touched, so
// unchanged <img>/<iframe> elements are never re-created (no reload, no flicker).
// The root keeps its inline style (live swipe transforms).
function morph(old, nu, root = true) {
  if (old.nodeType !== nu.nodeType || old.nodeName !== nu.nodeName) { old.replaceWith(nu); return nu; }
  if (old.nodeType === 3 || old.nodeType === 8) { if (old.data !== nu.data) old.data = nu.data; return old; }
  if (old.dataset.gone && old.dataset.gone === nu.getAttribute('src')) return old; // still the "Image expired" tile
  for (const { name } of [...old.attributes]) if (!nu.hasAttribute(name) && !(root && name === 'style')) old.removeAttribute(name);
  for (const { name, value } of nu.attributes) if (old.getAttribute(name) !== value && !(root && name === 'style')) old.setAttribute(name, value);
  const a = [...old.childNodes], b = [...nu.childNodes];
  for (let i = 0; i < b.length; i++) a[i] ? morph(a[i], b[i], false) : old.appendChild(b[i]);
  for (let i = b.length; i < a.length; i++) a[i].remove();
  return old;
}
// Set a container's HTML only when it changed, patching in place.
function patchHTML(el, html) {
  if (el._html === html) return false;
  el._html = html;
  const t = document.createElement(el.tagName); t.innerHTML = html;
  // children only: the container keeps its own class/attributes
  const a = [...el.childNodes], b = [...t.childNodes];
  for (let i = 0; i < b.length; i++) a[i] ? morph(a[i], b[i], false) : el.appendChild(b[i]);
  for (let i = b.length; i < a.length; i++) a[i].remove();
  return true;
}
const ls = {
  get(k, d) { try { const v = localStorage.getItem('mm.' + k); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { v == null ? localStorage.removeItem('mm.' + k) : localStorage.setItem('mm.' + k, JSON.stringify(v)); } catch { } },
};
const debounce = (fn, ms) => { let t; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); }; };

async function api(path, o = {}) {
  if (o.json !== undefined) { o = { method: 'POST', body: JSON.stringify(o.json), headers: { 'content-type': 'application/json' }, ...o }; delete o.json; }
  const r = await fetch('/api/' + path, o);
  let j = null; try { j = await r.json(); } catch { }
  if (!r.ok) { const e = new Error(j?.error || 'HTTP ' + r.status); e.status = r.status; throw e; }
  return j;
}
const post = (p, json = {}) => api(p, { json });
const sendKeys = (pane, ks) => post(`a/${enc(pane)}/keys`, { keys: ks }).catch(e => toast(e.message, 'err'));

// ---------- icons ----------
const I = {
  back: '<svg viewBox="0 0 24 24"><path d="M15 4l-8 8 8 8"/></svg>',
  compose: '<svg viewBox="0 0 24 24"><path d="M12 20h8M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"/></svg>',
  plus: '<svg viewBox="0 0 24 24"><path d="M12 5v14M5 12h14"/></svg>',
  gear: '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/></svg>',
  term: '<svg viewBox="0 0 24 24"><rect x="3" y="4" width="18" height="16" rx="2.5"/><path d="M7 9l3 3-3 3M12.5 15H17"/></svg>',
  chat: '<svg viewBox="0 0 24 24"><path d="M21 12a8 8 0 0 1-11.6 7.1L4 20l1-4.6A8 8 0 1 1 21 12z"/></svg>',
  fork: '<svg viewBox="0 0 24 24"><circle cx="6" cy="5" r="2"/><circle cx="18" cy="5" r="2"/><circle cx="12" cy="19" r="2"/><path d="M6 7v2a3 3 0 0 0 3 3h6a3 3 0 0 0 3-3V7M12 12v5"/></svg>',
  up: '<svg viewBox="0 0 24 24"><path d="M12 19V5M5 12l7-7 7 7"/></svg>',
  img: '<svg viewBox="0 0 24 24"><rect x="3" y="5" width="18" height="14" rx="2.5"/><circle cx="8.5" cy="10" r="1.5"/><path d="M21 16l-5-5-8 8"/></svg>',
  x: '<svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6L6 18"/></svg>',
  clip: '<svg viewBox="0 0 24 24"><path d="M21 11.5l-8.6 8.6a5.5 5.5 0 0 1-7.8-7.8l8.9-8.9a3.7 3.7 0 0 1 5.2 5.2l-8.9 8.9a1.8 1.8 0 0 1-2.6-2.6l8.2-8.2"/></svg>',
  grid: '<svg viewBox="0 0 24 24"><rect x="4" y="4" width="7" height="7" rx="1.5"/><rect x="13" y="4" width="7" height="7" rx="1.5"/><rect x="4" y="13" width="7" height="7" rx="1.5"/><rect x="13" y="13" width="7" height="7" rx="1.5"/></svg>',
  down: '<svg viewBox="0 0 24 24"><path d="M7 10l5 5 5-5"/></svg>',
  ext: '<svg viewBox="0 0 24 24"><path d="M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"/></svg>',
  stop: '<svg viewBox="0 0 24 24"><rect x="6.5" y="6.5" width="11" height="11" rx="2" fill="currentColor" stroke="none"/></svg>',
  queue: '<svg viewBox="0 0 24 24"><path d="M5 7h14M5 12h14M5 17h8M17 15v6M14 18h6"/></svg>',
  search: '<svg viewBox="0 0 24 24"><circle cx="11" cy="11" r="6.5"/><path d="M20 20l-4.2-4.2"/></svg>',
  files: '<svg viewBox="0 0 24 24"><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5M9 13h6M9 17h4"/></svg>',
  bolt: '<svg viewBox="0 0 24 24"><path d="M13 3L5 13.5h6L10 21l8-10.5h-6z"/></svg>',
  bellx: '<svg viewBox="0 0 24 24"><path d="M6 8a6 6 0 0 1 9.3-5M18 8c0 7 3 9 3 9H9M13.7 21a2 2 0 0 1-3.4 0M3 3l18 18M6 8c0 2.3-.3 4-.8 5.3"/></svg>',
  pin: '<svg viewBox="0 0 24 24"><path d="M9 4h6l-1 5 4 4H6l4-4zM12 13v8"/></svg>',
  copy: '<svg viewBox="0 0 24 24"><rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/></svg>',
};

// ---------- global state ----------
const G = {
  st: ls.get('state', null), conn: 'off', seen: ls.get('seen', null), view: null, route: null,
  tc: new Map(), starting: new Map(), skew: 0, dismissed: new Set(Object.keys(ls.get('sfDismiss', {}))),
};
const app = $('#app');
const now = () => Date.now() + G.skew;
// the agent's last paragraph asks the user something (triage deck "Done · asks you", composer question chips)
const asksYou = t => /\?/.test(String(t || '').trim().split(/\n\s*\n/).pop().replace(/```[\s\S]*?```|`[^`]*`|https?:\S+/g, ''));

const LABEL = { blocked: 'Needs you', done: 'Done', working: 'Working', idle: 'Idle', unknown: '—' };
const RANK = { blocked: 0, done: 1, working: 2, idle: 3, unknown: 4 };
function dispStatus(a) {
  if (a.status === 'blocked') return 'blocked';
  if (a.status === 'working') return 'working';
  if (a.status === 'done' || a.status === 'idle') return !a.service && a.completionSeq > (G.seen?.[a.pane] ?? 0) ? 'done' : 'idle'; // a service is never Done
  return 'unknown';
}
function markSeen(a) {
  if (!a || !G.seen || (G.seen[a.pane] ?? 0) === a.completionSeq) return;
  G.seen[a.pane] = a.completionSeq; ls.set('seen', G.seen);
}
L.isPane = p => /^w\d+:p\d+$/.test(p) || !!G.st?.agents.some(a => a.pane === p);
L.paneLabel = p => { const a = G.st?.agents.find(a => a.pane === p); return a ? (a.home ? 'Master' : a.title || a.name || p) : p; };
const goBack = fb => { if (G.navs > 1) history.back(); else location.hash = fb; };
const agentHref = a => (a.home && a.pane === G.st?.home ? '#/' : '#/a/' + enc(a.pane));
const sortAgents = list => list.map(a => [a, dispStatus(a)])
  .sort((x, y) => (y[0].home ? 1 : 0) - (x[0].home ? 1 : 0) || (y[0].pinned ? 1 : 0) - (x[0].pinned ? 1 : 0) || RANK[x[1]] - RANK[y[1]] || (x[0].muted ? 1 : 0) - (y[0].muted ? 1 : 0) || (y[0].since || 0) - (x[0].since || 0)).map(x => x[0]);

// status strip: counts of non-home agents by display status; segments jump to agents
function strip(st) {
  const c = { blocked: [], working: [], done: [] };
  let n = 0;
  for (const a of st?.agents || []) { if (a.home) continue; n++; const s = dispStatus(a); if (c[s]) c[s].push(a); }
  const seg = k => c[k].length ? `<button class="seg s-${k}" data-s="${k}"><i></i>${c[k].length}${k === 'blocked' ? '<span class="lbl"> need' + (c[k].length > 1 ? '' : 's') + ' you</span>' : ''}</button>` : '';
  return `<div class="strip${c.blocked.length ? ' alert' : ''}">${seg('blocked')}${seg('working')}${seg('done')}${st?.jobsRunning > 0 ? `<button class="seg s-jobs" data-s="jobs"><span class="dspin"></span>${st.jobsRunning} job${st.jobsRunning > 1 ? 's' : ''}</button>` : ''}${G.res?.gpuSeg() || ''}<button class="seg ws" data-s="ws">${I.grid}<span>${n}</span></button><button class="seg srch" data-s="search" aria-label="Search">${I.search}</button></div>`;
}
// the strip scrolls sideways when it does not fit the bar; a fade marks hidden segments
function stripFit(box, html) {
  if (html != null && box.dataset.html !== html) {
    box.dataset.html = html; box.innerHTML = html;
    box.classList.remove('tight'); // labels go when the segments do not fit
  }
  if (!box.classList.contains('tight') && box.clientWidth && box.scrollWidth > box.clientWidth + 1) box.classList.add('tight');
  if (!box.dataset.fit) { box.dataset.fit = 1; box.addEventListener('scroll', () => stripFit(box), { passive: true }); }
  box.classList.toggle('ovf', box.scrollLeft + box.clientWidth < box.scrollWidth - 2);
}
function stripLong(box) { longPress(box, '.seg.s-working, .seg.s-blocked, .seg.s-done', b => switcher(b.dataset.s)); }
function stripClick(e) {
  const b = e.target.closest('[data-s]'); if (!b) return;
  const s = b.dataset.s;
  if (s === 'ws' || s === 'jobs') { location.hash = '#/agents'; return; }
  if (s === 'gpu') { location.hash = '#/r?f=gpu'; return; }
  if (s === 'search') { openSearch(); return; }
  if (s === 'blocked') { lazy('queue').then(q => { const it = q.items(G.st)[0]; if (it) location.hash = q.hrefOf(it); else switcher('blocked'); }); return; } // queue.js: the top item's chat
  const list = (G.st?.agents || []).filter(a => !a.home && dispStatus(a) === s);
  if (list.length === 1) location.hash = agentHref(list[0]); else switcher(s);
}
function switcher(filter) {
  const list = sortAgents((G.st?.agents || []).filter(a => !filter || dispStatus(a) === filter));
  const sh = sheet(`<h2>${filter ? LABEL[filter] : 'Switch agent'}</h2><div class="swl">${list.map(a => {
    const s = dispStatus(a);
    return `<a class="swrow${colCls(a.color)}" href="${agentHref(a)}"><span class="pill s-${s}"><i></i>${LABEL[s]}</span><span class="sb"><b>${cdot(a.color)}${a.home ? 'Master' : esc(a.title || a.name || a.pane)}</b>${doingOf(a) ? nowSmall(a) : `<small>${esc(shortPath(a.cwd))} · ${esc(a.wsLabel || a.ws)}${a.tabLabel ? ' / ' + esc(a.tabLabel) : ''}</small>`}</span>${G.art?.mini(a) || ''}<span class="since" data-since="${a.since}"></span></a>`;
  }).join('') || '<p class="dim">No agents.</p>'}</div><a class="btn swall" href="#/agents">${I.grid} Workspace</a>`);
  tick(sh);
  sh.addEventListener('click', e => { if (e.target.closest('a')) sh.close(); });
}
// "Next up": the agent that wants you most, other than `pane` — the Needs-you queue's order (queue.js), then working
const NEXT_RANK = { blocked: 0, done: 1, working: 2 };
function nextUp(pane) {
  const q = MODS.queue?.items(G.st).find(x => x.pane !== pane && (!x.a.muted || x.rank === 0))?.a;
  if (q) return q;
  return (G.st?.agents || []).map(a => [a, dispStatus(a)])
    .filter(([a, s]) => a.pane !== pane && s in NEXT_RANK && (!a.muted || s === 'blocked'))
    .sort((x, y) => NEXT_RANK[x[1]] - NEXT_RANK[y[1]] || (x[0].since || 0) - (y[0].since || 0))[0]?.[0] || null;
}
const nextName = a => a.home ? 'Master' : a.title || a.name || a.pane;
// pane chips in chats: live status dot, color, doing label (F6)
function updPchips(root) {
  const chips = root.querySelectorAll('.pchip'); if (!chips.length) return;
  const by = new Map((G.st?.agents || []).map(a => [a.pane, a]));
  for (const c of chips) {
    const a = by.get(c.dataset.pane), st = a ? dispStatus(a) : 'gone', d = a && doingOf(a)?.label || '';
    const sig = `${st}|${d}|${a?.color || ''}|${a?.title || ''}`;
    if (c.dataset.sig === sig) continue;
    c.dataset.sig = sig;
    c.className = 'pchip ps-' + st + colCls(a?.color);
    c.innerHTML = `<span class="pcl">${esc(L.paneLabel(c.dataset.pane))}</span>${d ? `<small class="pcd">${esc(d)}</small>` : ''}`;
  }
}
// live "now doing" (F5)
const doingOf = a => a.status === 'working' && a.doing?.label ? a.doing : null;
const doingHTML = d => `<span class="dspin"></span>${esc(d.label)}${d.ts ? ` · <span data-since="${d.ts}"></span>` : ''}`;
// rows say it in words (state activity, server/src/activity.rs) with a category icon; the raw tool label stays in the
// title tooltip, on long-press and in the chat header
const nowHTML = a => a.activity?.phrase
  ? `<span class="aico" aria-hidden="true">${esc(a.activity.icon || '•')}</span>${esc(a.activity.phrase[0].toUpperCase() + a.activity.phrase.slice(1))}${a.doing?.ts ? ` · <span data-since="${a.doing.ts}"></span>` : ''}`
  : doingHTML(a.doing);
const nowSmall = a => `<small class="doing${a.activity?.phrase ? ' aphr' : ''}" title="${esc(a.doing?.label || '')}">${nowHTML(a)}</small>`;
const shortPath = p => String(p ?? '').replace(/^\/home\/[^/]+/, '~');

// ---------- agent colors (Claude /color names → 7 pastels, style.css "agent colors") ----------
// COLORS = every name Claude's /color takes (class col-<name>); red is not offered and shows as rose.
const COLORS = ['red', 'orange', 'yellow', 'green', 'cyan', 'blue', 'purple', 'pink'];
const PASTELS = [['orange', 'Peach'], ['yellow', 'Butter'], ['green', 'Mint'], ['cyan', 'Aqua'], ['blue', 'Periwinkle'], ['purple', 'Lavender'], ['pink', 'Rose']];
const colCls = c => COLORS.includes(c) ? ' col-' + c : '';
// Agent color is never drawn as a dot (dots mean status); it tints text,
// card washes and your own bubbles instead. Kept as a no-op for callers.
const cdot = () => '';
// context gauge from the mod (modbridge; else estimated from the transcript, ctx.est): ring with the window fill %, amber ≥70, red ≥85; a spinner while compacting
function ctxHTML(a) {
  const c = a.ctx;
  if (!c) return '';
  if (c.compacting) return '<span class="ctxg busy" title="Compacting"><span class="spin"></span></span>';
  if (c.pct == null) return '';
  const p = Math.round(c.pct);
  return `<span class="ctxg${p >= 85 ? ' hi' : p >= 70 ? ' mid' : ''}" style="--p:${p}" title="Context ${p}%">${p}%</span>`;
}
function setCol(el, c) {
  if (el.dataset.col === (c || '')) return;
  if (el.dataset.col) el.classList.remove('col-' + el.dataset.col);
  el.dataset.col = COLORS.includes(c) ? c : '';
  if (el.dataset.col) el.classList.add('col-' + c);
}
// optimistic meta edits (title/color/pinned/muted) until the server reflects them
G.ov = new Map();
function applyOv(st) {
  for (const a of st.agents) {
    const o = G.ov.get(a.pane); if (!o) continue;
    if (Date.now() - o.at > 120000 || Object.keys(o.f).every(k => a[k] === o.f[k])) { G.ov.delete(a.pane); continue; }
    Object.assign(a, o.f);
  }
}
async function setMeta(a, f) {
  const prev = {}; for (const k in f) prev[k] = a[k];
  G.ov.set(a.pane, { at: Date.now(), f: { ...(G.ov.get(a.pane)?.f || {}), ...f } });
  Object.assign(a, f); G.view?.onState?.(G.st);
  try { await post(`a/${enc(a.pane)}/meta`, f); return true; }
  catch (e) { G.ov.delete(a.pane); Object.assign(a, prev); G.view?.onState?.(G.st); toast('Failed: ' + esc(e.message), 'err'); return false; }
}
// tap the chat title: rename (focused + preselected, Return saves) and recolor (instant)
function renameSheet(a) {
  const sh = sheet(`<form class="rn"><input class="rn-in" value="${esc(a.title || a.name || '')}" enterkeyhint="done" autocomplete="off" aria-label="Title"><button class="btn primary sm">Save</button></form>
    ${a.home ? '' : `<div class="cdots">${[['', 'No color'], ...PASTELS].map(([c, n]) => `<button class="cd${c ? ' col-' + c : ' none'}${(a.color === 'red' ? 'pink' : a.color || '') === c ? ' on' : ''}" data-c="${c}" aria-label="${n}"></button>`).join('')}</div>`}
    ${a.status === 'working' || a.status === 'blocked' ? '<p class="rn-hint">Busy now: Claude gets the new name and color when this turn ends.</p>' : ''}`, 'compact');
  const inp = $('.rn-in', sh);
  inp.focus(); inp.select();
  $('.rn', sh).onsubmit = e => {
    e.preventDefault(); inp.blur(); sh.close();
    const t = inp.value.trim();
    if (t && t !== a.title) setMeta(a, { title: t }).then(ok => ok && toast('Renamed'));
  };
  if (!a.home) $('.cdots', sh).onclick = e => {
    const b = e.target.closest('[data-c]'); if (!b) return;
    haptic(); sh.querySelectorAll('.cd').forEach(x => x.classList.toggle('on', x === b));
    setMeta(a, { color: b.dataset.c || null });
  };
}

// ---------- card actions (F9) ----------
function cardAct(a, act) {
  if (act === 'pin') setMeta(a, { pinned: !a.pinned }).then(ok => ok && toast(a.pinned ? 'Pinned' : 'Unpinned', '', null, 1400));
  else if (act === 'mute') setMeta(a, { muted: !a.muted }).then(ok => ok && toast(a.muted ? 'Muted — no done pushes' : 'Unmuted', '', null, 1600));
  else if (act === 'stop') sendKeys(a.pane, ['esc']).then(() => toast('Sent Esc to ' + esc(a.title || a.pane), '', null, 1600));
  else if (act === 'close') confirmClose(a);
  else if (act === 'term') location.hash = '#/s/' + enc(a.pane);
  else if (act === 'copy') copyText(a.pane);
  else if (act === 'rename') renameSheet(a);
  else if (act === 'desk') lazy('v3').then(m => m.focusDesk(a));
  else if (act === 'mode') lazy('v3').then(m => m.cycleMode(a));
  else if (act === 'compact') lazy('mb').then(m => m.compactNow(a));
  else if (act === 'ctx') lazy('mb').then(m => m.ctxSheet(a));
}
function confirmClose(a) {
  const sh = sheet(`<h2>Close this tab?</h2><p class="dim">${esc(a.title || a.pane)} — the session can be resumed.</p><div class="btns col"><button class="btn danger go">Close tab</button><button class="btn no">Cancel</button></div>`, 'compact');
  $('.no', sh).onclick = () => sh.close();
  $('.go', sh).onclick = async () => {
    sh.close(); haptic();
    try { await post(`a/${enc(a.pane)}/close`); toast('Closed'); } catch (e) { toast('Close failed: ' + esc(e.message), 'err'); }
  };
}

// ---------- prefs: snippets (F13) ----------
const DEF_SNIPS = ['continue', 'hows that going?', 'done', 'yes please'].map(t => ({ id: t, label: t, text: t }));
function getPrefs() {
  return G.prefsP ||= api('prefs').then(p => { G.prefs = p; return p; }, () => { G.prefsP = null; G.prefs = { snippets: ls.get('snips', DEF_SNIPS) }; return G.prefs; });
}
function insertAt(ta, text) {
  const a = ta.selectionStart ?? ta.value.length, b = ta.selectionEnd ?? a, v = ta.value;
  const pre = v.slice(0, a), sep = pre && !/\s$/.test(pre) ? ' ' : '';
  ta.value = pre + sep + text + v.slice(b);
  const k = (pre + sep + text).length; ta.focus(); ta.setSelectionRange(k, k);
  ta.dispatchEvent(new Event('input'));
}
const snippetSheet = (ta, done) => lazy('sheets').then(m => m.snippetSheet(ta, done));
const homeOf = st => (st?.agents?.concat(st.shells || []).map(a => String(a.cwd ?? '').match(/^\/home\/[^/]+/)?.[0]).find(Boolean)) || '/home';

function rel(ms) {
  if (!ms) return '';
  const s = Math.max(0, ((now() - ms) / 1000) | 0);
  if (s < 5) return 'now';
  if (s < 60) return s + 's';
  if (s < 3600) return ((s / 60) | 0) + 'm';
  if (s < 86400) { const hh = (s / 3600) | 0, mm = ((s % 3600) / 60) | 0; return hh + 'h' + (hh < 10 && mm ? ' ' + mm + 'm' : ''); }
  return ((s / 86400) | 0) + 'd';
}
const tickEl = el => { let t = rel(+el.dataset.since); if ('ago' in el.dataset && t) t = t === 'now' ? 'just now' : t + ' ago'; if (el.textContent !== t) el.textContent = t; };
const tick = root => root.querySelectorAll('[data-since]').forEach(tickEl);
setInterval(() => { if (!document.hidden) { tick(app); staleBanner(); } }, 1000);
// SSE down > 10 s: thin banner under the header (F14)
let staleEl;
function staleBanner() {
  const on = G.conn !== 'on' && G.downAt && Date.now() - G.downAt > 10000;
  if (!on) { if (staleEl) staleEl.hidden = true; return; }
  staleEl ||= document.body.appendChild(h('<div class="stalebar" hidden></div>'));
  const t = `Offline · ${G.lastUpd ? 'updated ' + rel(G.lastUpd + G.skew) + ' ago' : 'connecting…'}`;
  if (staleEl.textContent !== t) staleEl.textContent = t;
  staleEl.hidden = false;
}

const saveState = debounce(() => ls.set('state', G.st), 400);
function setState(st) {
  if (!st || !Array.isArray(st.agents)) return;
  G.skew = st.now ? st.now - Date.now() : 0; G.lastUpd = Date.now();
  if (G.ov.size) applyOv(st);
  G.st = st; L.ts = st.tsName; L.home = homeOf(st);
  if (st.home && st.home === G.homePending) G.homePending = null;
  if (!G.seen) { G.seen = {}; for (const a of st.agents) G.seen[a.pane] = a.completionSeq; ls.set('seen', G.seen); }
  for (const a of st.agents) {
    if ((G.seen[a.pane] ?? 0) > a.completionSeq) G.seen[a.pane] = 0; // pane id reused
    G.starting.delete(a.pane);
  }
  autoNameFx(st);
  saveState(); badge();
  if (!MODS.art && st.agents.some(a => a.artifact)) lazy('art').then(() => G.view?.onState?.(G.st), () => { }); // artifacts (art.js)
  G.view?.onState?.(st);
  resFollow();
}
function badge() {
  const n = (G.st?.agents || []).filter(a => { const s = dispStatus(a); return s === 'blocked' || s === 'done'; }).length;
  try { n ? navigator.setAppBadge?.(n) : navigator.clearAppBadge?.(); } catch { }
  document.title = (n ? `(${n}) ` : '') + 'sheprd';
}

// ---------- SSE (global) ----------
let es = null, esTry = 0, esTimer = 0;
function setConn(c) {
  if (c !== 'on' && (G.conn === 'on' || !G.downAt)) G.downAt ||= Date.now();
  if (c === 'on') G.downAt = 0;
  G.conn = c; staleBanner(); document.querySelectorAll('[data-conn]').forEach(d => d.dataset.conn = c); }
function connect() {
  clearTimeout(esTimer); es?.close();
  setConn('retry');
  const s = es = new EventSource('/api/events');
  s.addEventListener('state', e => { esTry = 0; setConn('on'); try { setState(JSON.parse(e.data)); } catch (x) { console.warn(x); } });
  s.addEventListener('surface', e => { try { surfaceToast(JSON.parse(e.data)); } catch { } });
  s.onopen = () => { esTry = 0; setConn('on'); obFlush(); flushDismiss(); };
  s.onerror = () => {
    if (es !== s) return;
    s.close(); setConn('retry');
    esTimer = setTimeout(connect, Math.min(6000, 400 * 2 ** esTry++));
  };
}

// ---------- viewing ----------
function viewing() {
  const body = JSON.stringify({ pane: G.view?.pane ?? null, visible: !document.hidden });
  if (document.hidden && navigator.sendBeacon) navigator.sendBeacon('/api/viewing', new Blob([body], { type: 'application/json' }));
  else fetch('/api/viewing', { method: 'POST', body, headers: { 'content-type': 'application/json' }, keepalive: true }).catch(() => { });
}
setInterval(() => { if (!document.hidden) viewing(); }, 30000);

// ---------- outbox: never lose a send, never send twice (F14) ----------
const rid = () => Math.random().toString(36).slice(2, 10) + Date.now().toString(36);
const OB_TTL = 600000, OBW = new Map(); // nonce → bubble updater
let OB = ls.get('outbox', []), obBusy = false;
OB.forEach(e => { e.queued = true; });
const obSave = () => ls.set('outbox', OB.length ? OB.map(({ nonce, dkey, raw, ts, via, steer, path, body, hold }) => ({ nonce, dkey, raw, ts, via, steer, path, body, hold })) : null);
const obEmit = (e, st, err, r) => OBW.get(e.nonce)?.(st, err, r);
async function obSend(e) {
  try { obEmit(e, 'ok', null, await post(e.path, e.body)); }
  catch (err) {
    if (err.status) { obEmit(e, 'err', err); return; } // HTTP errors are never retried
    e.queued = true; OB.push(e); obSave(); obEmit(e, 'wait');
  }
}
async function obFlush() {
  if (obBusy || !OB.length) return;
  obBusy = true;
  try {
    for (const e of [...OB]) {
      if (e.hold > Date.now()) continue; // undo window still open (held sends)
      if (Date.now() - e.ts > OB_TTL) { OB = OB.filter(x => x !== e); obSave(); obEmit(e, 'expired'); continue; }
      try { const r = await post(e.path, e.body); OB = OB.filter(x => x !== e); obSave(); obEmit(e, 'ok', null, r); }
      catch (err) { if (!err.status) break; OB = OB.filter(x => x !== e); obSave(); obEmit(e, 'err', err); }
    }
  } finally { obBusy = false; }
}
setInterval(() => { if (OB.length && !document.hidden) obFlush(); }, 15000);
// undo-send: the entry sits in the outbox (so a reload still sends it) until the window ends
const UNDO_MS = 3000;
function obHold(e) {
  e.hold ||= Date.now() + UNDO_MS; OB.push(e); obSave();
  e.holdT = setTimeout(() => {
    if (!OB.includes(e)) return;
    OB = OB.filter(x => x !== e); obSave(); obEmit(e, 'go'); obSend(e);
  }, UNDO_MS);
}
function obUnhold(e) { // → true when the send was still held and is now cancelled
  clearTimeout(e.holdT);
  if (!OB.includes(e) || !(e.hold > Date.now() - 200)) return false;
  OB = OB.filter(x => x !== e); obSave(); return true;
}

// ---------- haptics (iOS 18 switch-checkbox trick) ----------
let hapEl;
function haptic() {
  if (!ls.get('haptics', true)) return;
  try {
    if (navigator.vibrate) { if (navigator.userActivation?.hasBeenActive) navigator.vibrate(8); return; }
    hapEl ||= document.body.appendChild(h('<label class="hap" aria-hidden="true"><input type="checkbox" switch></label>'));
    hapEl.click();
  } catch { }
}

// A plain shell (not Claude) in `cwd` with a one-line command typed in, unsent: reuses a shell already sitting
// in that folder, else opens a new herdr tab there. Multi-line commands are copied instead.
async function openShell(cwd, cmd = '') {
  cwd ||= G.st?.agents.find(a => a.home)?.cwd || '';
  const lines = cmd.trim().split('\n').filter(Boolean);
  if (lines.length > 1) copyText(cmd).then(() => toast('Copied, paste it into the terminal', 'ok', null, 2200));
  let pane = (G.st?.shells || []).find(s => s.cwd === cwd)?.pane;
  if (!pane) {
    if (!cwd) return toast('No folder for this chat', 'err');
    haptic();
    try { pane = (await post('new', { cwd, label: 'term', shell: true })).pane; }
    catch (x) { return toast('Could not open a terminal: ' + esc(x.message), 'err'); }
  }
  if (lines.length === 1) G.termPrefill = lines[0];
  location.hash = '#/s/' + enc(pane);
}

// ---------- long-press + popover menu ----------
// Code blocks: Copy (whole block), Terminal (opens this agent's terminal with a
// one-line command filled in, unsent), tap a line to copy just that line.
document.addEventListener('click', e => {
  const cb = e.target.closest('.cb'); if (!cb) return;
  const raw = $('pre', cb)?.dataset.raw ?? '';
  if (e.target.closest('.cb-copy')) { e.preventDefault(); e.stopPropagation(); copyText(raw).then(() => toast('Copied', 'ok', null, 1200)); return; }
  if (e.target.closest('.cb-term')) {
    e.preventDefault(); e.stopPropagation();
    const pane = G.view?.target || G.view?.pane || G.st?.home;
    const cwd = G.st?.agents.find(a => a.pane === pane)?.cwd;
    openShell(cwd, raw); return;
  }
  const ln = e.target.closest('.ln');
  if (ln && !e.target.closest('a') && !String(getSelection?.() || '')) {
    e.stopPropagation();
    copyText(ln.textContent).then(() => { ln.classList.add('ln-hit'); setTimeout(() => ln.classList.remove('ln-hit'), 700); toast('Line copied', 'ok', null, 1000); });
  }
}, true);
function longPress(root, sel, fn, ms = 450) {
  let t = 0, x0 = 0, y0 = 0;
  const cancel = () => { clearTimeout(t); t = 0; };
  root.addEventListener('pointerdown', e => {
    const tg = sel ? e.target.closest(sel) : root;
    if (!tg || !root.contains(tg) || e.button > 0) return;
    x0 = e.clientX; y0 = e.clientY; cancel();
    t = setTimeout(() => { t = 0; if (fn(tg, e) !== false) { tg.dataset.lp = Date.now(); haptic(); } }, ms);
  });
  root.addEventListener('pointermove', e => { if (t && Math.hypot(e.clientX - x0, e.clientY - y0) > 8) cancel(); });
  root.addEventListener('pointerup', cancel); root.addEventListener('pointercancel', cancel);
  root.addEventListener('scroll', cancel, { passive: true, capture: true });
  root.addEventListener('click', e => {
    const tg = e.target.closest('[data-lp]');
    if (tg && root.contains(tg)) { const lp = +tg.dataset.lp; delete tg.dataset.lp; if (Date.now() - lp < 1500) { e.preventDefault(); e.stopPropagation(); } }
  }, true);
  root.addEventListener('contextmenu', e => { if (!sel || e.target.closest(sel)) e.preventDefault(); });
}
// prompt modifiers: toggled in the long-press Send tray (stackable, pinnable per agent)
// and typed as trailing shorthands (//sub //par //commit //surface //ro //plan //short),
// matched case-insensitively and after iOS's auto-period. Appended to the prompt on send.
const PMODS = [
  { k: 'sub', l: 'subagent', t: 'use a subagent' },
  { k: 'par', l: 'parallel', t: 'use parallel subagents' },
  { k: 'commit', l: 'commit', t: 'commit when done' },
  { k: 'surface', l: 'surface', t: 'surface the results to my phone' },
  { k: 'ro', l: 'read-only', t: 'read-only: do not modify files' },
  { k: 'plan', l: 'plan first', t: 'plan first, then build' },
  { k: 'short', l: 'short', t: 'keep it short, I read on a phone' },
];
const PM_ALIAS = { subagent: 'sub', parallel: 'par', readonly: 'ro', phone: 'surface' };
const PM_RE = /(?:\s*\/\/(sub|subagent|par|parallel|commit|surface|phone|ro|readonly|plan|short)\b\.?)+\s*$/i;
function takeMods(t) { // → [text without trailing shorthands, keys]
  const m = t.match(PM_RE); if (!m) return [t, []];
  const keys = [...m[0].matchAll(/\/\/(\w+)/g)].map(x => { const k = x[1].toLowerCase(); return PM_ALIAS[k] || k; });
  return [t.slice(0, m.index), keys];
}
const modSuffix = keys => { const ts = PMODS.filter(p => keys.includes(p.k)).map(p => p.t); return ts.length ? ` (${ts.join('; ')})` : ''; };
function popMenu(anchor, items) {
  const ov = h(`<div class="pop-ov"><div class="popm">${items.map(([l], k) => `<button data-k="${k}">${esc(l)}</button>`).join('')}</div></div>`);
  ov.onclick = e => { const b = e.target.closest('[data-k]'); ov.remove(); if (b) items[+b.dataset.k][1](); };
  document.body.append(ov);
  placeAbove($('.popm', ov), anchor);
}
// Pin a popover just above `anchor` (right-aligned), inside the visible viewport. Positions by `top` from the
// anchor's own rect: measuring from the bottom broke with the iOS keyboard up (the layout viewport extends
// behind the keyboard), which threw menus up under the status bar.
function placeAbove(m, anchor) {
  const place = () => {
    if (!m.isConnected) { vv?.removeEventListener('resize', place); vv?.removeEventListener('scroll', place); removeEventListener('resize', place); return; }
    const r = anchor.getBoundingClientRect(), vTop = (vv?.offsetTop ?? 0) + 8;
    m.style.right = Math.max(8, innerWidth - r.right) + 'px';
    m.style.bottom = 'auto';
    m.style.maxHeight = Math.max(120, r.top - 8 - vTop) + 'px';
    m.style.overflowY = 'auto';
    m.style.top = Math.max(vTop, r.top - 8 - m.offsetHeight) + 'px';
  };
  place();
  // the iOS keyboard often closes as the long-press lands, moving the anchor after we placed: follow it
  vv?.addEventListener('resize', place); vv?.addEventListener('scroll', place); addEventListener('resize', place);
  requestAnimationFrame(place); setTimeout(place, 350);
}

// ---------- toast / sheet / lightbox ----------
let toastBox;
function toast(html, kind = '', onClick = null, ms = 3200) {
  toastBox ||= document.body.appendChild(h('<div class="toasts"></div>'));
  const t = h(`<div class="toast ${kind}">${html}</div>`);
  t.onclick = () => { onClick?.(); t.remove(); };
  toastBox.append(t);
  setTimeout(() => { t.classList.add('out'); setTimeout(() => t.remove(), 300); }, ms);
}
function surfaceToast(s) {
  if (G.dismissed.has(String(s.id))) return;
  if (s.links?.some(l => l.url && l.url === location.hash)) return; // already looking at it
  if (G.st && !G.st.surfaces?.some(x => x.id === s.id)) { G.st.surfaces = [s, ...(G.st.surfaces || [])]; G.view?.onState?.(G.st); }
  const k = surfKey(s);
  toast(`<b>${esc(s.title)}</b>${s.agentTitle ? `<span>${esc(s.agentTitle)}</span>` : ''}${s.replies?.length && !REPLIED[k] ? `<div class="sf-rep" data-sfkey="${esc(k)}" data-pane="${esc(s.pane || '')}">${s.replies.map(r => `<button class="btn sm${DONE_REP.test(r.trim()) ? ' did' : ''}" data-reply="${esc(r)}">${esc(repLabel(r))}</button>`).join('')}</div>` : ''}`,
    'l-' + s.level, () => openSurface(s), s.replies?.length ? 9000 : 5000);
}

// ---------- messages other agents sent with /steer {from} (transcript kind "steer") ----------
// the sender: its live pane, or (escalations sent before `from` existed) the live agent with that name
const steerAgent = it => G.st?.agents.find(x => x.pane === it.agentId)
  || (!it.agentId && it.from ? G.st?.agents.find(x => [x.title, x.name].some(n => n && n.toLowerCase() === it.from.toLowerCase())) : null);
// ---------- surface cards: workspace, inline in chats, toasts (F7) ----------
// Dismissing sticks: ids are kept on this phone for a week (hidden at once, across reloads and while the
// cached state still has them) and sent until the server confirms; the server keeps the dismissal and the
// content's fingerprint, so other devices, the triage deck and re-posts of the same note stay clear.
const SFD = ls.get('sfDismiss', {}); // id → {t, ok}
let sfdBusy = false, sfdAgain = false;
function dismissSurfs(ids) {
  ids = ids.map(String).filter(Boolean); if (!ids.length) return;
  const t = Date.now();
  for (const id of ids) { G.dismissed.add(id); SFD[id] = { t, ok: false }; }
  ls.set('sfDismiss', SFD);
  navigator.serviceWorker?.ready.then(r => r.getNotifications()).then(ns => ns.forEach(n => ids.includes(n.tag) && n.close()), () => { });
  if (G.st) G.view?.onState?.(G.st);
  flushDismiss();
}
async function flushDismiss() {
  if (sfdBusy) { sfdAgain = true; return; } sfdBusy = true;
  try {
    const t = Date.now();
    for (const [id, d] of Object.entries(SFD)) if (!(t - d.t < 7 * 864e5)) delete SFD[id];
    const ids = Object.keys(SFD).filter(id => !SFD[id].ok);
    if (ids.length) { await post('surfaces/dismiss', { ids }); for (const id of ids) if (SFD[id]) SFD[id].ok = true; }
  } catch { sfdAgain = false; } finally { ls.set('sfDismiss', SFD); sfdBusy = false; }
  if (sfdAgain) { sfdAgain = false; flushDismiss(); }
}
const REPLIED = ls.get('replied', {});
const surfKey = s => s.tid && s.i != null ? `t:${s.tid}:${s.i}` : 's:' + s.id;
const asImg = p => /^(\/api\/|https?:)/.test(p) ? p : fileUrl(p);
// Surface bodies are clipped to a few lines with a fade; "Show more" expands in place.
const SF_OPEN = new Set();
document.addEventListener('click', e => {
  const b = e.target.closest('.sf-more'); if (!b) return;
  e.preventDefault(); e.stopPropagation();
  const k = b.dataset.sfmore, open = !SF_OPEN.has(k);
  open ? SF_OPEN.add(k) : SF_OPEN.delete(k);
  b.previousElementSibling?.classList.toggle('open', open); b.textContent = open ? 'Show less' : 'Show more';
}, true);
// "done"-style replies confirm the user did something themselves: label them so (and never as the main action)
const DONE_REP = /^(done|did it|i did it|finished|ok,? done)[.!]?$/i;
const repLabel = r => DONE_REP.test(r.trim()) ? '✓ I did it' : r;
const repBtns = (rs, cls = 'btn') => {
  const todo = rs.some(r => DONE_REP.test(r.trim()));
  return (todo ? '<div class="sf-todo">Your turn — tap after you’ve done it</div>' : '')
    + `<div class="sf-rep">${rs.map(r => `<button class="${cls}${DONE_REP.test(r.trim()) ? ' did' : ''}" data-reply="${esc(r)}">${esc(repLabel(r))}</button>`).join('')}</div>`;
};
function surfHTML(s, { inline, pane } = {}) {
  const k = surfKey(s), done = REPLIED[k];
  const links = (s.links || []).map(l => typeof l === 'string' ? { url: l } : l).filter(l => l?.url);
  return `<div class="surf l-${esc(s.level || 'info')}${inline ? ' inl' : ''}"${s.id != null ? ` data-id="${esc(s.id)}"` : ''}${inline ? ` data-i="${s.i}"` : ''} data-sfkey="${esc(k)}" data-pane="${esc(pane || s.pane || '')}">
      ${inline ? '' : `<button class="x" data-dismiss="${esc(s.id)}" aria-label="Dismiss">${I.x}</button>`}
      <div class="sf-t">${esc(s.title)}</div>
      ${inline ? '' : `<div class="sf-m">${esc(s.agentTitle || '')}${s.agentTitle ? ' · ' : ''}<span data-since="${s.ts}"></span></div>`}
      ${s.body ? `<div class="sf-b md${s.body.length > 220 || s.body.split('\n').length > 4 ? ' long' : ''}${SF_OPEN.has(k) ? ' open' : ''}">${md(s.body, null)}</div>${s.body.length > 220 || s.body.split('\n').length > 4 ? `<button class="sf-more" data-sfmore="${esc(k)}">${SF_OPEN.has(k) ? 'Show less' : 'Show more'}</button>` : ''}` : ''}
      ${thumbs((s.images || []).map(asImg))}
      ${links.length ? `<div class="chips">${links.map(l => `<a class="chip" href="${esc(fixUrl(l.url))}" target="_blank" rel="noopener">${esc(l.label || l.url)}</a>`).join('')}</div>` : ''}
      ${s.command ? `<div class="sf-cmd"><pre>${esc(s.command)}</pre><button class="btn sm sf-copy">${I.copy}Copy</button>${pane || s.pane ? '<button class="btn sm sf-term">Terminal</button>' : ''}</div>` : ''}
      ${s.replies?.length ? (done ? `<div class="sf-sent">✓ Sent: ${esc(repLabel(done))}</div>` : repBtns(s.replies)) : ''}
    </div>`;
}
async function copyText(t) {
  try { await navigator.clipboard.writeText(t); }
  catch {
    const ta = document.body.appendChild(h('<textarea readonly style="position:fixed;opacity:0;top:0"></textarea>'));
    ta.value = t; ta.select(); try { document.execCommand('copy'); } catch { } ta.remove();
  }
  haptic(); toast('Copied', 'ok', null, 1400);
}
document.addEventListener('click', e => {
  const cp = e.target.closest('.sf-copy');
  if (cp) { e.preventDefault(); e.stopPropagation(); copyText(cp.parentElement.querySelector('pre').textContent); return; }
  // Terminal: open the agent's pane with "! <command>" filled in (Claude's bash mode, so the agent sees the output)
  const tb = e.target.closest('.sf-term');
  if (tb) {
    e.preventDefault(); e.stopPropagation();
    const cmd = tb.parentElement.querySelector('pre').textContent.trim(), pane = tb.closest('[data-pane]')?.dataset.pane;
    if (!pane) return toast('Agent is gone', 'err');
    const inAgent = () => {
      if (cmd.includes('\n')) copyText(cmd).then(() => toast('Copied, paste it after ! in the terminal', 'ok', null, 2200));
      else G.termPrefill = '! ' + cmd;
      location.hash = '#/s/' + enc(pane);
    };
    popMenu(tb, [['New terminal tab', () => openShell(G.st?.agents.find(a => a.pane === pane)?.cwd, cmd)], ['In the agent (! — it sees the output)', inAgent]]);
    return;
  }
  const rb = e.target.closest('[data-reply]'); if (!rb) return;
  e.preventDefault(); e.stopPropagation();
  const box = rb.closest('[data-sfkey]'), k = box.dataset.sfkey, pane = box.dataset.pane, text = rb.dataset.reply;
  if (!pane) { toast('Agent is gone', 'err'); return; }
  if (G.view instanceof ChatView && G.view.target === pane && G.view.target === G.view.pane) G.view.send(text);
  else { haptic(); post(`a/${enc(pane)}/prompt`, { text, nonce: rid() }).then(() => toast('Sent: ' + esc(text), 'ok', null, 1600), x => toast('Not sent: ' + esc(x.message), 'err')); }
  REPLIED[k] = text; ls.set('replied', REPLIED);
  rb.closest('.toast')?.remove();
  document.querySelectorAll(`[data-sfkey="${CSS.escape(k)}"] .sf-rep`).forEach(r => r.replaceWith(h(`<div class="sf-sent">✓ Sent: ${esc(text)}</div>`)));
}, true);
function openSurface(s) {
  s = G.st?.surfaces?.find(x => x.id === s.id) || s; // `i` is resolved a moment after the surface arrives
  const a = G.st?.agents.find(x => x.pane === s.pane || (s.session && x.session === s.session));
  const qi = s.i != null ? '?i=' + s.i : '';
  if (a) nav(agentHref(a) + qi); else if (s.tid) nav('#/t/' + enc(s.tid) + qi);
}
// link to an item of a transcript: live pane if the session is running, else the read-only transcript
function itemHref(tid, i, pane) {
  const a = G.st?.agents.find(x => (pane && x.pane === pane) || (tid && x.tid === tid));
  return (a ? agentHref(a) : '#/t/' + enc(tid)) + (i != null ? '?i=' + i : '');
}

function sheet(html, cls = '') {
  // Opening a sheet dismisses the keyboard, so the sheet sizes to the full screen.
  if (document.activeElement?.blur && document.activeElement !== document.body) document.activeElement.blur();
  const ov = h(`<div class="sheet-ov"><div class="sheet ${cls}"><div class="grab"></div><button class="ib close" aria-label="Close">${I.x}</button><div class="sh-body">${html}</div></div></div>`);
  document.body.append(ov);
  requestAnimationFrame(() => requestAnimationFrame(() => ov.classList.add('open')));
  ov.close = () => { ov.classList.remove('open'); setTimeout(() => ov.remove(), 260); };
  ov.addEventListener('click', e => { if (e.target === ov || e.target.closest('.close')) ov.close(); });
  // Swipe down to dismiss: from the grab area anywhere, or from the body when it's scrolled to the top.
  const sh = $('.sheet', ov), body = $('.sh-body', ov);
  let y0 = null, dy = 0;
  sh.addEventListener('touchstart', e => {
    const fromTop = e.target.closest('.grab, .close') || e.touches[0].clientY - sh.getBoundingClientRect().top < 48;
    y0 = e.touches.length === 1 && (fromTop || body.scrollTop <= 0) ? e.touches[0].clientY : null; dy = 0;
  }, { passive: true });
  sh.addEventListener('touchmove', e => {
    if (y0 == null) return;
    dy = e.touches[0].clientY - y0;
    if (dy > 0 && (body.scrollTop <= 0 || e.target.closest('.grab'))) { sh.style.transition = 'none'; sh.style.transform = `translateY(${dy}px)`; }
    else if (dy < 0) y0 = null;
  }, { passive: true });
  sh.addEventListener('touchend', () => {
    if (y0 == null) return;
    sh.style.transition = ''; sh.style.transform = '';
    if (dy > 90) ov.close();
    y0 = null;
  });
  return ov;
}
addEventListener('keydown', e => { if (e.key === 'Escape') document.querySelector('.sheet-ov.open:last-of-type')?.close(); });
const closeSheets = () => document.querySelectorAll('.sheet-ov').forEach(o => o.close());

// Photo viewer: swipe left/right through every image in the same message or
// card, pinch or double-tap to zoom, tap anywhere off the image to close.
function lightbox(src, list = [src]) {
  let idx = Math.max(0, list.indexOf(src)); if (!list.length) list = [src];
  const many = list.length > 1;
  const ov = h(`<div class="lb"><div class="lb-sc"><img alt=""></div><button class="ib close" aria-label="Close">${I.x}</button><a class="ib open" target="_blank" rel="noopener" aria-label="Open">${I.ext}</a>${many ? '<button class="lb-nav prev" aria-label="Previous">‹</button><button class="lb-nav next" aria-label="Next">›</button><div class="lb-n"></div>' : ''}</div>`);
  const sc = $('.lb-sc', ov), img = $('img', ov);
  let z = 1, z0 = 1, d0 = 0, lastTap = 0, sx = null, sy = 0, swiping = false;
  const fit = () => { const r = sc.getBoundingClientRect(); const k = Math.min(r.width / img.naturalWidth, r.height / img.naturalHeight, 1); return img.naturalWidth * k || r.width; };
  const zoom = (nz, cx, cy) => {
    const r = sc.getBoundingClientRect(); cx ??= r.width / 2; cy ??= r.height / 2;
    const fx = (sc.scrollLeft + cx) / sc.scrollWidth, fy = (sc.scrollTop + cy) / sc.scrollHeight;
    z = Math.max(1, Math.min(8, nz));
    ov.classList.toggle('zoomed', z > 1);
    img.style.width = z > 1 ? fit() * z + 'px' : '';
    sc.scrollLeft = fx * sc.scrollWidth - cx; sc.scrollTop = fy * sc.scrollHeight - cy;
  };
  const show = i => {
    idx = (i + list.length) % list.length;
    zoom(1); img.src = list[idx]; $('.open', ov).href = list[idx];
    if (many) $('.lb-n', ov).textContent = `${idx + 1} / ${list.length}`;
    if (many) { const nx = new Image(); nx.src = list[(idx + 1) % list.length]; }
  };
  const close = () => { ov.classList.add('out'); setTimeout(() => ov.remove(), 160); removeEventListener('keydown', onKey); };
  const onKey = e => { if (e.key === 'Escape') close(); else if (many && e.key === 'ArrowRight') show(idx + 1); else if (many && e.key === 'ArrowLeft') show(idx - 1); };
  addEventListener('keydown', onKey);
  const dist = t => Math.hypot(t[0].clientX - t[1].clientX, t[0].clientY - t[1].clientY);
  sc.addEventListener('touchstart', e => {
    if (e.touches.length === 2) { d0 = dist(e.touches); z0 = z; sx = null; return; }
    sx = z === 1 && e.touches.length === 1 ? e.touches[0].clientX : null; sy = e.touches[0].clientY; swiping = false;
  }, { passive: true });
  sc.addEventListener('touchmove', e => {
    if (e.touches.length === 2 && d0) {
      e.preventDefault();
      const r = sc.getBoundingClientRect();
      zoom(z0 * dist(e.touches) / d0, (e.touches[0].clientX + e.touches[1].clientX) / 2 - r.left, (e.touches[0].clientY + e.touches[1].clientY) / 2 - r.top);
      return;
    }
    if (sx == null || !many) return;
    const dx = e.touches[0].clientX - sx, dy = e.touches[0].clientY - sy;
    if (!swiping && Math.abs(dx) > 10 && Math.abs(dx) > Math.abs(dy)) swiping = true;
    if (swiping) { e.preventDefault(); img.style.transform = `translateX(${dx}px)`; img.style.transition = 'none'; }
  }, { passive: false });
  sc.addEventListener('touchend', e => {
    if (e.touches.length < 2) d0 = 0;
    if (!swiping) return;
    const dx = (e.changedTouches[0]?.clientX ?? sx) - sx;
    img.style.transition = 'transform .18s ease-out';
    if (Math.abs(dx) > 50) { img.style.transform = `translateX(${dx > 0 ? 100 : -100}%)`; setTimeout(() => { img.style.transition = 'none'; img.style.transform = ''; show(idx + (dx < 0 ? 1 : -1)); }, 150); }
    else img.style.transform = '';
    swiping = false; sx = null; lastTap = 0;
  });
  sc.addEventListener('wheel', e => { if (e.ctrlKey) { e.preventDefault(); zoom(z * (1 - e.deltaY / 100)); } }, { passive: false });
  sc.addEventListener('click', e => {
    if (e.target !== img) { if (z === 1) close(); return; }   // anywhere off the image closes
    const t = Date.now();
    if (t - lastTap < 320) { const r = sc.getBoundingClientRect(); zoom(z > 1 ? 1 : 2.5, e.clientX - r.left, e.clientY - r.top); lastTap = 0; return; }
    lastTap = t;
  });
  $('.close', ov).onclick = close;
  if (many) { $('.prev', ov).onclick = () => show(idx - 1); $('.next', ov).onclick = () => show(idx + 1); }
  show(idx);
  document.body.append(ov);
}

document.addEventListener('click', e => {
  const im = e.target.closest('img.thumb');
  const pa = !im && e.target.closest('a.pimg');
  if (!im && !pa) return;
  e.preventDefault();
  // Gallery = every image in the same message / card / tool row, in order.
  const box = (im || pa).closest('.msg, .row, .card, .scard, .sfc, .note, article, .sheet, .view') || document;
  const list = [...new Set([...box.querySelectorAll('img.thumb, a.pimg')].map(x => x.tagName === 'A' ? x.href : (x.currentSrc || x.src)))];
  const src = im ? (im.currentSrc || im.src) : pa.href;
  lightbox(src, list.includes(src) ? list : [src]);
});
// a picture whose file is gone (and wasn't kept, keep.rs) shows a small "Image expired" tile instead of vanishing
const GONE_IMG = 'data:image/svg+xml,' + encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" width="168" height="96" viewBox="0 0 168 96"><rect width="168" height="96" rx="10" fill="#8e8e93" fill-opacity=".18"/><g fill="none" stroke="#8e8e93" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="70" y="18" width="28" height="22" rx="4"/><circle cx="78" cy="26" r="2.5"/><path d="M98 36l-8-8-14 12M66 14l36 30"/></g><text x="84" y="70" text-anchor="middle" font-family="-apple-system,system-ui,sans-serif" font-size="14" fill="#8e8e93">Image expired</text></svg>');
document.addEventListener('error', e => {
  const t = e.target;
  if (!t.matches?.('img.thumb') || t.dataset.gone) return;
  t.dataset.gone = t.getAttribute('src') || '1'; t.src = GONE_IMG; t.classList.add('gone');
}, true);
document.addEventListener('gesturestart', e => e.preventDefault());

// visual viewport → app height (keyboard aware)
const vv = window.visualViewport;
// Only follow the visual viewport while the keyboard is up. iOS (standalone
// especially) can leave visualViewport reporting a keyboard-sized height after
// the keyboard closes, which left the app short with blank space under the
// composer; with no keyboard the app fills its fixed box (100%).
// iOS Home Screen apps can report a viewport shorter than the screen by the
// status-bar height while still drawing from the top, leaving a blank band at
// the bottom; stretch to the real screen height in exactly that case.
function standaloneGap() {
  if (!navigator.standalone) return 0;
  const portrait = innerHeight > innerWidth;
  const sh = portrait ? Math.max(screen.width, screen.height) : Math.min(screen.width, screen.height);
  const gap = sh - innerHeight;
  return gap > 20 && gap < 120 && portrait ? sh : 0;
}
function fitVV() {
  if (!vv) return;
  const r = document.documentElement.style;
  const ae = document.activeElement;
  const typing = !!ae && (ae.tagName === 'TEXTAREA' || (ae.tagName === 'INPUT' && !/^(button|checkbox|radio|file)$/.test(ae.type)));
  const kb = typing && window.innerHeight - vv.height > 120;
  if (kb) {
    r.setProperty('--app-h', vv.height + 'px');
    r.setProperty('--vv-top', vv.offsetTop + 'px');
  } else {
    // opt-in (Settings → Fill to screen bottom) until it's proven on device: the earlier always-on stretch hid the composer
    const full = ls.get('fillgap', false) ? standaloneGap() : 0;
    if (full) r.setProperty('--app-h', full + 'px'); else r.removeProperty('--app-h');
    r.removeProperty('--vv-top');
  }
  document.body.classList.toggle('kb', kb);
  if (G.view?.stick) G.view.toBottom?.();
}
vv?.addEventListener('resize', fitVV); vv?.addEventListener('scroll', fitVV);
// The keyboard closing doesn't always fire a resize; re-measure on blur and return.
document.addEventListener('focusout', () => setTimeout(fitVV, 60));
document.addEventListener('focusin', () => setTimeout(fitVV, 300));
document.addEventListener('visibilitychange', () => { if (!document.hidden) fitVV(); });
addEventListener('pageshow', fitVV); addEventListener('orientationchange', () => setTimeout(fitVV, 300));
fitVV();

// ---------- Agents view ----------
// Auto-quiet (ux-ideas #10): test agents (cwd under ~/Work/tries, title or tab starting test/qa) and agents idle 30 min+
// with nothing unseen fold into the Quiet folder; blocked, working, pinned and Master never do. Nothing closes by itself.
const isTest = a => !a.home && (/\/Work\/tries(\/|$)/.test(String(a.cwd || '')) || /^(test|qa)\b/i.test(String(a.title || a.name || '')) || /^(test|qa)\b/i.test(String(a.tabLabel || '')));
const isQuiet = a => {
  if (a.home || a.pinned || svcNeeds(a)) return false;
  const s = dispStatus(a);
  if (s === 'blocked' || s === 'working') return false;
  return isTest(a) || (s === 'idle' && now() - (a.since || 0) >= 30 * 60e3);
};
// Background services (API.md "Background services") live in the Scheduled strip, not as cards, unless they need the user.
const svcNeeds = a => !!a.service && (dispStatus(a) === 'blocked' || !!a.service.error || !!a.service.stale);
const svcIssue = a => a.service?.error || (a.service?.stale ? `Not reporting since ${new Date(a.service.ts).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}` : '');
const humanName = n => { const t = String(n || '').replace(/[-_]+/g, ' ').trim(); return t.charAt(0).toUpperCase() + t.slice(1); };
const atTime = ms => { // "Thu 10:42 PM", "today 6:00 PM"
  const d = new Date(ms), t = d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
  const days = Math.round((new Date(ms).setHours(0, 0, 0, 0) - new Date().setHours(0, 0, 0, 0)) / 864e5);
  return (days === 0 ? 'today' : days === 1 ? 'tmrw' : d.toLocaleDateString([], { weekday: 'short' })) + ' ' + t;
};
const inTime = ms => { const m = Math.round((ms - now()) / 60e3); return m <= 0 ? 'now' : m < 60 ? `in ${m}m` : m < 1440 ? `in ${Math.floor(m / 60)}h${m % 60 ? ' ' + (m % 60) + 'm' : ''}` : `in ${Math.round(m / 1440)}d`; };
let agentsView;
function getAgentsView() {
  if (agentsView) return agentsView;
  const el = h(`<div class="view agents">
    <header class="bar"><div class="bar-in">
      <button class="ib back" aria-label="Home">${I.back}</button>
      <button class="brand" aria-label="Settings"><span class="conn" data-conn="${G.conn}"></span><span class="host"></span></button>
      <div class="sp"></div>
      <button class="ib srch" aria-label="Search">${I.search}</button>
      <button class="ib plus big" aria-label="New agent (hold for options)">${I.plus}</button>
    </div></header>
    <div class="scroll">
      <div class="warn" hidden></div>
      <div class="brief"></div>
      <div class="hero"></div>
      <section class="tsec" hidden><div class="gh"><span class="gt">In flight</span></div><div class="tiles"></div></section>
      <div class="surfs"></div>
      <div class="cbox">
        <div class="cards"></div>
        <div class="bgs" hidden></div>
        <div class="jobs" hidden></div>
        <div class="empty" hidden><p>No agents running.</p><button class="btn">Start an agent</button></div>
        <details class="quiet" hidden><summary><b>Quiet</b><em></em><span class="sp"></span><button class="qclose" hidden></button></summary><div class="qcards"></div><div class="qsh"></div></details>
      </div>
    </div></div>`);
  const sc = $('.scroll', el), cardsEl = $('.cards', el), cbox = $('.cbox', el), quietEl = $('.quiet', el), cards = new Map(), groups = new Map();
  let surfOpen = false, scrollY = 0;
  wirePlus($('.plus', el));
  $('.back', el).onclick = () => { location.hash = '#/'; };
  wirePlus($('.empty .btn', el));
  $('.brand', el).onclick = settingsSheet; // Settings: tap the host name (the gear lives on Machine too)
  $('.srch', el).onclick = openSearch;
  quietEl.open = !!ls.get('quietOpen', false);
  quietEl.addEventListener('toggle', () => ls.set('quietOpen', quietEl.open));
  $('.qclose', el).onclick = e => { // two taps: "Close tests" → "Close N?" for 3 s; never automatic
    e.preventDefault(); e.stopPropagation();
    const b = e.currentTarget, tests = (G.st?.agents || []).filter(isTest);
    if (!tests.length) return;
    if (!b.classList.contains('arm')) { b.classList.add('arm'); b.textContent = `Close ${tests.length}?`; setTimeout(() => { b.classList.remove('arm'); b.textContent = 'Close tests'; }, 3000); return; }
    b.classList.remove('arm'); b.textContent = 'Close tests'; haptic();
    for (const a of tests) cardAct(a, 'close');
  };
  lazy('fleet').then(() => render(G.st), () => { }); // fleet: agent order
  Promise.all([lazy('queue'), lazy('home')]).then(() => render(G.st), () => { }); // the briefing (home.js) and its queue
  const surfsEl = $('.surfs', el);
  surfsEl.addEventListener('click', e => {
    const x = e.target.closest('[data-dismiss]');
    if (x) {
      dismissSurfs([x.dataset.dismiss]); haptic(); return;
    }
    const clr = e.target.closest('.sf-clr');
    if (clr) { // two taps: "Clear all" → "Clear N?" for 3 s
      if (!clr.classList.contains('arm')) { clr.classList.add('arm'); clr.textContent = `Clear ${clr.dataset.n}?`; setTimeout(() => { clr.classList.remove('arm'); clr.textContent = 'Clear all'; }, 3000); return; }
      dismissSurfs(clr.dataset.ids.split(' ')); haptic(); surfOpen = false; return;
    }
    if (e.target.closest('.more')) { surfOpen = !surfOpen; render(G.st); return; }
    if (e.target.closest('a,img')) return;
    const c = e.target.closest('.surf');
    if (c) { const s = G.st.surfaces.find(s => String(s.id) === c.dataset.id); s && openSurface(s); }
  });

  // "↳ started by Master" on worker cards; tapping it opens the parent.
  const FROM = { spawn: 'started by', review: 'reviewing', fork: 'forked from', drive: 'driven by', split: 'split from' };
  const fromHTML = a => {
    const pp = a.parent?.pane; if (!pp) return '';
    const p = G.st?.agents.find(x => x.pane === pp);
    return `<span class="c-from" data-go="#/a/${enc(pp)}" title="${FROM[a.parent.via] || 'from'}">↳ <b>${esc(p?.home ? 'Master' : p?.title || pp)}</b></span>`;
  };
  // Sheprd-style row: context ring (spins while working) · title + age · one meta row · one outcome line · image thumbnail
  const ringHTML = (a, s) => {
    const c = a.ctx, p = c?.pct != null ? Math.round(c.pct) : null;
    return `<span class="cring s-${s}${p >= 85 ? ' hi' : p >= 70 ? ' mid' : ''}" style="--p:${p ?? 0}"${p != null ? ` title="Context ${p}%"` : ''}>${c?.compacting ? '<span class="spin"></span>' : p ?? ''}</span>`;
  };
  const outcomeHTML = (a, s) => {
    if (doingOf(a)) return [a.activity?.phrase ? 'doing aphr' : 'doing', nowHTML(a)];
    const q = s === 'blocked' && (a.ask?.questions?.[0]?.question || a.ask?.questions?.[0]?.header);
    if (q) return ['q', esc(q)];
    if (svcIssue(a)) return ['q', `⚠ ${esc(svcIssue(a))}`];
    if ((s === 'idle' || s === 'done') && a.recap?.text && a.recap.i > (G.seenI[a.tid] ?? -1)) return ['', `<b class="ok">✓</b> ${esc(a.recap.text)}`];
    if (a.last?.text) return ['', `${a.last.role === 'user' ? '<b>You: </b>' : ''}${esc(a.last.text.replace(/\*\*|__|`+|^#+\s*/gm, ''))}`];
    return null;
  };
  const cardHTML = a => {
    const s = dispStatus(a), sub = a.subagents?.running, sep = '<i class="msep">·</i>';
    const meta = [(s === 'blocked' || s === 'done') && `<b class="t-${s}">${LABEL[s]}</b>`, a.service && `<b class="t-blocked">${esc(a.service.name)}</b>`, esc(a.wsLabel || a.ws) + (a.tabLabel ? ' · ' + esc(a.tabLabel) : ''),
      fromHTML(a), sub && `<span class="subb">${I.fork}${sub}</span>`, G.res?.cardLine(a)].filter(Boolean).join(sep);
    const thumb = G.art?.chip(a, 'thumb') || '', inl = thumb ? '' : G.art?.chip(a, 'inline') || '', out = outcomeHTML(a, s);
    return `<a class="card s-${s}${a.home ? ' home' : colCls(a.color)}" href="${agentHref(a)}">${ringHTML(a, s)}<div class="cbody">
      <div class="c-title"><span class="tt">${esc(a.title || a.name || a.kind || a.pane)}</span>${a.home ? '<span class="htag">Master</span>' : ''}${a.pinned ? `<span class="cflag">${I.pin}</span>` : ''}${a.muted ? `<span class="cflag">${I.bellx}</span>` : ''}<span class="since" data-since="${a.since}"></span></div>
      <div class="c-meta">${meta}</div>
      ${out || inl ? `<div class="c-last${out?.[0] ? ' ' + out[0] : ''}">${inl}${out ? `<span class="lt">${out[1]}</span>` : ''}</div>` : ''}
    </div>${thumb}</a>`;
  };
  // Briefing on top (home.js: sentence, hero, In-flight tiles); rows below: Ready (finished, not yet seen) and Everyone
  // (Master first). Working agents are tiles and the hero's agent is the hero, so neither repeats as a row; quiet
  // agents fold away at the bottom. Without home.js (still loading) every agent is a row.
  const GRP = [['ready', 'Ready'], ['all', 'Everyone']];
  const groupOf = (a, s) => s === 'done' && !a.home ? 'ready' : 'all';
  const grpEl = k => {
    let g = groups.get(k);
    if (!g) { g = h(`<section class="agrp g-${k}">${k === 'home' ? '' : '<div class="gh"><span class="gt"></span><span class="gn"></span></div>'}<div class="gl"></div></section>`); groups.set(k, g); }
    return g;
  };
  const place = (box, nodes) => { // keyed reorder: move only what is out of place, drop leftovers
    let prev = null;
    for (const n of nodes) { const want = prev ? prev.nextSibling : box.firstChild; if (want !== n) box.insertBefore(n, want); prev = n; }
    let x; while ((x = prev ? prev.nextSibling : box.firstChild)) x.remove();
  };

  function render(st) {
    $('.host', el).textContent = st?.host || location.hostname;
    const w = $('.warn', el);
    if (!st) { w.hidden = false; w.textContent = 'Connecting…'; return; }
    w.hidden = st.herdr !== false; w.textContent = 'herdr is not reachable on the host.';
    const list = sortAgents(st.agents);
    // surfaces
    const inQ = new Set(MODS.queue?.items(st).filter(x => x.kind === 'surf').map(x => String(x.s.id))); // the hero has those
    const surfs = (st.surfaces || []).filter(s => !G.dismissed.has(String(s.id)) && !inQ.has(String(s.id)));
    const shown = surfOpen ? surfs : surfs.slice(0, 2);
    if (patchHTML(surfsEl, shown.map(surfHTML).join('') + (surfs.length > 1 ? `<div class="sf-foot">${surfs.length > 2 ? `<button class="more">${surfOpen ? 'Show less' : `Show ${surfs.length - 2} more`}</button>` : ''}`
      + `<button class="sf-clr" data-n="${surfs.length}" data-ids="${esc(surfs.map(s => s.id).join(' '))}">Clear all</button></div>` : ''))) tick(surfsEl);
    // cards (keyed), placed into their group or the Quiet folder
    const live = new Set(), byG = new Map(), quiet = [];
    const vis = list.filter(a => !(a.service && !svcNeeds(a)) && !isQuiet(a));
    const hb = MODS.home && MODS.queue ? MODS.home.draw(el, st, { list: vis, jobs, failing: list.filter(a => svcIssue(a)) }) : null;
    for (const a of MODS.fleet ? MODS.fleet.order(list) : list) {
      if (a.service && !svcNeeds(a)) continue; // in the Scheduled strip
      if (hb && (hb.tiles.has(a.pane) || a.pane === hb.hero)) continue; // a tile / the hero
      live.add(a.pane);
      const html = cardHTML(a); let c = cards.get(a.pane);
      if (!c) { c = { el: h(`<div class="cwrap" data-pane="${esc(a.pane)}"><div class="cact l"></div><div class="cact r"></div></div>`) }; cards.set(a.pane, c); }
      if (c.html !== html) {
        const n = h(html); tick(n);
        if (c.card) c.card = morph(c.card, n); else { c.el.append(n); c.card = n; }
        c.html = html;
      }
      const asig = `${!!a.pinned}${!!a.muted}${!!a.home}${'pinned' in a}`;
      if (c.asig !== asig) {
        c.asig = asig; const m = 'pinned' in a;
        c.el.children[0].innerHTML = m ? `<button data-act="pin">${I.pin}${a.pinned ? 'Unpin' : 'Pin'}</button><button data-act="mute">${I.bellx}${a.muted ? 'Unmute' : 'Mute'}</button>` : '';
        c.el.children[1].innerHTML = `<button data-act="stop">${I.stop}Stop</button>${a.home ? '' : `<button data-act="close">${I.x}Close</button>`}`;
      }
      if (isQuiet(a)) quiet.push(c.el);
      else { const k = groupOf(a, dispStatus(a)); if (!byG.has(k)) byG.set(k, []); byG.get(k)[a.home ? 'unshift' : 'push'](c.el); }
      MODS.fleet?.deco(c, a);
    }
    for (const [k, c] of cards) if (!live.has(k)) { c.el.remove(); cards.delete(k); }
    const secs = [];
    for (const [k, label] of GRP) {
      const els = byG.get(k); if (!els) continue;
      const g = grpEl(k); secs.push(g);
      if (label) { const t = $('.gt', g), n = $('.gn', g); if (t.textContent !== label) t.textContent = label; if (n.textContent !== String(els.length)) n.textContent = els.length; }
      place($('.gl', g), els);
    }
    place(cardsEl, secs);
    drawBgs();
    $('.empty', el).hidden = list.length > 0;
    const shells = st.shells || [], tests = list.filter(isTest).length;
    quietEl.hidden = !quiet.length && !shells.length;
    place($('.qcards', quietEl), quiet);
    const em = [quiet.length && `${quiet.length} agent${quiet.length === 1 ? '' : 's'}`, shells.length && `${shells.length} shell${shells.length === 1 ? '' : 's'}`].filter(Boolean).join(' · ');
    if ($('summary em', quietEl).textContent !== em) $('summary em', quietEl).textContent = em;
    const qc = $('.qclose', quietEl); qc.hidden = !tests; if (!qc.classList.contains('arm')) qc.textContent = 'Close tests';
    patchHTML($('.qsh', quietEl), shells.map(s => `<a class="shell" href="#/s/${enc(s.pane)}"><span>${esc(s.title || s.pane)}</span><small>${esc(shortPath(s.cwd))}</small></a>`).join(''));
  }
  // Scheduled strip: the next agent wake-up / schedule (GET /api/sched, polled while Home is visible) and one line per
  // background service. Not a card: a tinted strip with time chips; hidden when there is nothing.
  const bgsEl = $('.bgs', el);
  const CLOCK = '<svg viewBox="0 0 24 24"><circle cx="12" cy="13" r="8"/><path d="M12 9v4l2.5 2.5M9 2h6"/></svg>';
  let sched = null, schedAt = 0, schedBusy = false;
  const loadSched = async force => {
    if (schedBusy || G.noSched || (!force && Date.now() - schedAt < 60e3)) return;
    schedBusy = true;
    try { sched = await api('sched'); schedAt = Date.now(); drawBgs(); }
    catch (e) { if (e.status === 404) G.noSched = true; schedAt = Date.now(); }
    finally { schedBusy = false; }
  };
  setInterval(() => { if (!document.hidden && G.view === agentsView) loadSched(); }, 15e3);
  const upcoming = () => {
    const agents = G.st?.agents || [], t = now(), out = [];
    for (const x of sched?.timers || []) {
      if (!x.agentPane || !x.next || x.next < t - 60e3) continue;
      const a = agents.find(a => a.pane === x.agentPane || a.name === x.agentPane);
      out.push({ name: humanName(x.name), next: x.next, href: a ? agentHref(a) : '#/fleet/sched' });
    }
    for (const x of sched?.schedules || []) {
      if (x.paused || !x.next) continue;
      const a = x.lastPane && agents.find(a => a.pane === x.lastPane);
      out.push({ name: x.label, next: x.next, href: a ? agentHref(a) : '#/fleet/sched' });
    }
    return out.sort((a, b) => a.next - b.next);
  };
  function drawBgs() {
    const svcs = (G.st?.agents || []).filter(a => a.service && !svcNeeds(a)), up = upcoming(), n = up[0], rows = [];
    if (n) rows.push(`<a class="bg-row tmr" href="${esc(n.href)}"><span class="bg-ic">${CLOCK}</span><b>${esc(n.name)}</b><span class="bg-chip">${esc(atTime(n.next))}</span><span class="bg-in">${esc(inTime(n.next))}</span>`
      + `<span class="sp"></span>${up.length > 1 ? `<span class="bg-more" data-go="#/fleet/sched">+${up.length - 1} more</span>` : ''}</a>`);
    for (const a of svcs) {
      const v = a.service, err = v.lastError && now() - v.lastError.ts < 3600e3;
      rows.push(`<a class="bg-row svc${a.status === 'working' ? ' busy' : ''}" href="${agentHref(a)}" data-pane="${esc(a.pane)}"><span class="bg-ic">${esc(v.icon || '●')}</span><b>${esc(v.name)}</b>`
        + (v.lastRun ? `<span class="bg-chip">${esc(v.verb || 'ran')} <span data-since="${v.lastRun}" data-ago></span></span>` : '')
        + `<span class="bg-st">${esc(v.status || '')}${err ? ` · <i class="bg-err">error <span data-since="${v.lastError.ts}" data-ago></span></i>` : ''}</span></a>`);
    }
    bgsEl.hidden = !rows.length;
    if (patchHTML(bgsEl, rows.length ? `<span class="bg-lbl">${CLOCK}Scheduled</span>${rows.join('')}` : '')) tick(bgsEl);
  }
  bgsEl.addEventListener('click', e => { const m = e.target.closest('[data-go]'); if (m) { e.preventDefault(); location.hash = m.dataset.go; } });
  longPress(bgsEl, '.bg-row.svc', r => { const a = G.st?.agents.find(a => a.pane === r.dataset.pane); if (!a) return false; lazy('sheets').then(m => m.cardSheet(a)); });

  // background jobs (F11)
  const jobsEl = $('.jobs', el);
  let jobs = [], jobsSig = null, jobsT = 0;
  const durs = ms => { const s = Math.max(0, ms / 1000 | 0); return s < 60 ? s + 's' : s < 3600 ? (s / 60 | 0) + 'm' : (s / 3600 | 0) + 'h ' + ((s % 3600) / 60 | 0) + 'm'; };
  // running jobs sit below the agents; finished ones fold into one row and can be cleared (per phone)
  let jobsOpen = false;
  const jobRow = j => `<button class="job j-${esc(j.status)}" data-id="${esc(j.id)}">
        <span class="jst">${j.status === 'running' ? '<span class="spin"></span>' : j.status === 'completed' ? '✓' : j.status === 'unknown' ? '?' : '✗'}</span>
        <span class="sb"><b>${esc(j.desc || j.cmd)}</b><small>${j.status === 'running' ? `running <span data-since="${j.started}"></span>` : `${esc(j.status)} after ${durs(j.ended - j.started)} · <span data-since="${j.ended}" data-ago></span>`}${j.agentTitle ? ' · ' + esc(j.agentTitle) : ''}</small>${G.res?.jobLine(j) || ''}</span><span class="go">›</span></button>`;
  const drawJobs = () => {
    const cleared = ls.get('jobsCleared', 0);
    const run = []; // running jobs are In-flight tiles (home.js)
    const fin = jobs.filter(j => j.status !== 'running' && j.ended && j.ended > cleared && now() - j.ended < 6 * 3600e3);
    jobsEl.hidden = !run.length && !fin.length;
    patchHTML(jobsEl, jobsEl.hidden ? '' : `<div class="jh"><span>Jobs</span>${fin.length ? '<button class="jclr">Clear finished</button>' : ''}</div>`
      + run.map(jobRow).join('')
      + (fin.length ? `<button class="jfold">${jobsOpen ? '▾' : '▸'} ${fin.length} finished</button>` + (jobsOpen ? fin.map(jobRow).join('') : '') : ''));
    tick(jobsEl);
  };
  const loadJobs = async () => {
    if (G.noJobs) return;
    try { jobs = (await api('jobs')).jobs || []; drawJobs(); render(G.st); }
    catch (e) { if (e.status === 404) G.noJobs = true; }
  };
  jobsEl.addEventListener('click', e => {
    if (e.target.closest('.jclr')) { ls.set('jobsCleared', now()); jobsOpen = false; haptic(); drawJobs(); return; }
    if (e.target.closest('.jfold')) { jobsOpen = !jobsOpen; drawJobs(); return; }
    const b = e.target.closest('.job'); const j = b && jobs.find(x => x.id === b.dataset.id);
    if (j) lazy('sheets').then(m => m.jobSheet(j));
  });
  const jobsCheck = st => {
    const sig = (st?.jobsRunning ?? 0) + ':' + (st?.agents || []).map(a => a.jobs?.total || 0).join(',');
    if (sig === jobsSig) return;
    jobsSig = sig; clearTimeout(jobsT); jobsT = setTimeout(loadJobs, 150);
  };

  // swipe a card: right → Pin/Mute, left → Stop/Close; full swipe fires the first action (F9)
  let sw = null, openW = null;
  const setOff = (w, off, anim) => {
    const c = $('.card', w); w._off = off;
    c.style.transition = anim ? 'transform .2s cubic-bezier(.2,.8,.2,1)' : 'none';
    c.style.transform = off ? `translate3d(${off}px,0,0)` : '';
    w.classList.toggle('sw-l', off > 0); w.classList.toggle('sw-r', off < 0);
    if (!off) setTimeout(() => { if (!w._off) w.classList.remove('sw-l', 'sw-r'); }, 220);
    openW = off ? w : openW === w ? null : openW;
  };
  const agentOf = w => G.st?.agents.find(a => a.pane === w.dataset.pane);
  cbox.addEventListener('click', e => { const f = e.target.closest('.c-from'); if (f) { e.preventDefault(); e.stopPropagation(); location.hash = f.dataset.go; } }, true);
  cbox.addEventListener('touchstart', e => {
    const w = e.target.closest('.cwrap'); sw = null;
    if (!w || e.touches.length !== 1 || e.target.closest('.cact')) return;
    if (openW && openW !== w) setOff(openW, 0, true);
    const t = e.touches[0];
    if (t.clientX < 28) return; // edge swipe = back
    sw = { w, x: t.clientX, y: t.clientY, base: w._off || 0, lock: null, off: 0 };
  }, { passive: true });
  cbox.addEventListener('touchmove', e => {
    if (!sw) return;
    const t = e.touches[0], dx = t.clientX - sw.x, dy = t.clientY - sw.y;
    if (!sw.lock) {
      if (Math.abs(dx) < 10 && Math.abs(dy) < 10) return;
      if (Math.abs(dx) < Math.abs(dy) * 1.3) { sw = null; return; }
      sw.lock = 1;
    }
    e.preventDefault();
    const a = agentOf(sw.w); let off = sw.base + dx;
    if (off > 0 && !('pinned' in (a || {}))) off = 0;
    sw.off = off; setOff(sw.w, off);
  }, { passive: false });
  const swEnd = () => {
    if (!sw?.lock) { sw = null; return; }
    const { w, off } = sw, W = w.clientWidth; sw = null;
    const n = w.children[off > 0 ? 0 : 1].children.length;
    if (Math.abs(off) > W * 0.6) { setOff(w, 0, true); haptic(); const a = agentOf(w); a && cardAct(a, off > 0 ? 'pin' : 'stop'); }
    else if (Math.abs(off) > 60) setOff(w, Math.sign(off) * 76 * n, true);
    else setOff(w, 0, true);
  };
  cbox.addEventListener('touchend', swEnd); cbox.addEventListener('touchcancel', swEnd);
  cbox.addEventListener('click', e => {
    const w = e.target.closest('.cwrap'); if (!w) return;
    const b = e.target.closest('[data-act]');
    if (b) { e.preventDefault(); setOff(w, 0, true); const a = agentOf(w); a && cardAct(a, b.dataset.act); return; }
    if (w._off) { e.preventDefault(); setOff(w, 0, true); }
  }, true);
  longPress(cbox, '.card', c => { const a = agentOf(c.closest('.cwrap')); if (!a) return false; lazy('sheets').then(m => m.cardSheet(a)); });

  return agentsView = {
    el, pane: null,
    mount() { render(G.st); sc.scrollTop = scrollY; jobsSig = null; jobsCheck(G.st); loadSched(true); },
    destroy() { scrollY = sc.scrollTop; },
    keep: true, suspend() { scrollY = sc.scrollTop; }, resume() { render(G.st); sc.scrollTop = scrollY; jobsSig = null; jobsCheck(G.st); loadSched(); },
    onState: st => { render(st); jobsCheck(st); },
    resRedraw() { render(G.st); drawJobs(); }, // phase 2: res.js glance tick
  };
}

// ---------- Tab bar: Agents · History · Automations · Machine (docs/simplify.md C6, C8, C9) ----------
// One bar, moved into whichever tab screen is showing. History = Sessions + Search; Automations = Schedules (with
// system timers and all watches) + Cloud; Machine = Live, Disk, desktop Screen and Claude usage. Each tab reopens its
// last screen; tapping the current tab goes to its first screen (Agents: scroll to top). res.js puts a dot on
// Machine for warn/alert only (a busy GPU during training is normal).
const TABS = [
  ['agents', 'Agents', '#/agents', k => k === 'agents', '<svg viewBox="0 0 24 24"><rect x="4" y="4" width="7" height="7" rx="1.5"/><rect x="13" y="4" width="7" height="7" rx="1.5"/><rect x="4" y="13" width="7" height="7" rx="1.5"/><rect x="13" y="13" width="7" height="7" rx="1.5"/></svg>'],
  ['history', 'History', '#/fleet', k => k === 'fleet/' || k === 'search', '<svg viewBox="0 0 24 24"><path d="M4 12a8 8 0 1 0 2.3-5.6"/><path d="M4 4v4h4M12 8v4l3 2"/></svg>'],
  ['auto', 'Automations', '#/fleet/sched', k => k === 'fleet/sched' || k === 'fleet/cloud', '<svg viewBox="0 0 24 24"><circle cx="12" cy="13" r="8"/><path d="M12 9v4l2.5 2.5M9 2h6"/></svg>'],
  ['machine', 'Machine', '#/r', k => k === 'r' || k.startsWith('r/disk') || k === 'fleet/desk', '<svg viewBox="0 0 24 24"><path d="M4 17a8 8 0 1 1 16 0"/><path d="M12 17l4-5"/></svg>'],
];
let tabEl = null;
const tabLast = {};
function tabBar(key, v) {
  const t = TABS.find(t => t[3](key));
  if (!t) { tabEl?.remove(); return; }
  tabLast[t[0]] = location.hash;
  if (!tabEl) {
    tabEl = h(`<nav class="tabbar">${TABS.map(([k, l, href, , svg]) => `<a href="${href}" data-tab="${k}">${svg}<i class="tdot"></i><span>${l}</span></a>`).join('')}</nav>`);
    tabEl.addEventListener('click', e => {
      const a = e.target.closest('[data-tab]'); if (!a) return;
      e.preventDefault(); haptic();
      const T = TABS.find(t => t[0] === a.dataset.tab), on = a.classList.contains('on');
      if (on && T[0] === 'agents') { $('.scroll', G.view.el)?.scrollTo({ top: 0, behavior: 'smooth' }); return; }
      const to = on ? T[2] : tabLast[T[0]] || T[2];
      if (location.hash !== to) location.hash = to;
    });
  }
  for (const a of tabEl.children) { const on = a.dataset.tab === t[0]; a.classList.toggle('on', on); on ? a.setAttribute('aria-current', 'page') : a.removeAttribute('aria-current'); }
  if (tabEl.parentNode !== v.el) v.el.append(tabEl);
}

// ---------- Screen component ----------
const KEYS = [['Esc', 'esc'], ['Tab', 'tab'], ['⇧Tab', 'shift+tab'], ['↑', 'up'], ['↓', 'down'], ['←', 'left'], ['→', 'right'],
['⏎', 'enter'], ['^C', 'ctrl+c'], ['1', '1'], ['2', '2'], ['3', '3'], ['y', 'y'], ['n', 'n']];
const keyBtns = (list, cls = '') => list.map(([l, k]) => `<button class="key ${cls}" data-k="${k}">${l}</button>`).join('');

class Screen {
  constructor(pane) {
    this.pane = pane; this.rev = null; this.fs = ls.get('fs', 0); // 0 = fit
    this.el = h(`<div class="screen">
      <div class="term-wrap"><pre class="term"></pre></div>
      <div class="scr-tools"><button class="key sm" data-z="-">A−</button><button class="key sm" data-z="0">Fit</button><button class="key sm" data-z="+">A+</button><span class="sp"></span><span class="dim rev"></span></div>
      <div class="keybar">${keyBtns(KEYS)}</div>
      <form class="rawin"><input type="text" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" enterkeyhint="send" placeholder="Type, sends + Enter"><button class="key">Send</button></form>
    </div>`);
    this.pre = $('.term', this.el); this.wrap = $('.term-wrap', this.el);
    $('.keybar', this.el).onclick = e => { const b = e.target.closest('[data-k]'); if (b) { sendKeys(pane, [b.dataset.k]).then(() => this.soon()); } };
    $('.scr-tools', this.el).onclick = e => {
      const b = e.target.closest('[data-z]'); if (!b) return;
      const cur = this.fs || parseFloat(this.pre.style.fontSize) || 11;
      this.fs = b.dataset.z === '0' ? 0 : Math.max(5, Math.min(20, cur + (b.dataset.z === '+' ? 1 : -1)));
      ls.set('fs', this.fs); this.layout();
    };
    if (G.termPrefill) { const inp = $('.rawin input', this.el); inp.value = G.termPrefill; G.termPrefill = null; setTimeout(() => inp.focus(), 350); }
    $('.rawin', this.el).onsubmit = async e => {
      e.preventDefault();
      const inp = $('input', e.target), text = inp.value;
      inp.value = '';
      try { if (text) await post(`a/${enc(pane)}/text`, { text }); await post(`a/${enc(pane)}/keys`, { keys: ['enter'] }); }
      catch (x) { toast(esc(x.message), 'err'); inp.value = text; }
      this.soon();
    };
    this.ro = new ResizeObserver(() => this.layout());
  }
  start() { this.on = true; this.ro.observe(this.wrap); if (G.noLive && Date.now() - G.noLive < 6e4) this.poll(); else lazy('fleet').then(m => this.on && m.liveScreen(this), () => this.poll()); } // fleet: SSE stream, else polling
  stop() { this.on = false; clearTimeout(this.t); this.ro.disconnect(); this.es?.close(); this.es = null; }
  soon() { if (this.es) return; clearTimeout(this.t); this.t = setTimeout(() => this.poll(), 120); }
  async poll() {
    clearTimeout(this.t);
    if (!this.on || document.hidden) return;
    try {
      this.draw(await api(`a/${enc(this.pane)}/screen?ansi=1`));
      this.el.classList.remove('stale');
    } catch (e) { this.el.classList.add('stale'); }
    if (this.on && !this.es) this.t = setTimeout(() => this.poll(), 500);
  }
  draw(r) {
    if (r.revision !== this.rev || r.revision == null) {
      this.rev = r.revision;
      const lines = String(r.text || '').replace(/\r\n?/g, '\n').replace(/\n+$/, '').split('\n');
      this.cols = Math.max(20, ...lines.map(l => stripAnsi(l).length));
      this.pre.innerHTML = ansi(lines.join('\n'));
      this.layout();
    }
  }
  layout() {
    const w = this.wrap.clientWidth - 12;
    const fs = this.fs || Math.max(5, Math.min(13, w / ((this.cols || 80) * 0.615)));
    this.pre.style.fontSize = fs.toFixed(2) + 'px';
  }
}

// ---------- Chat view ----------
const VIS = new Set(['user', 'text', 'tool', 'agent', 'note']);
// highest item index seen per transcript (F4)
G.seenI = ls.get('seenI', {});
const saveSeenI = debounce(() => {
  const disk = ls.get('seenI', {}); // merge (other tabs / reloads)
  for (const t in disk) if (!(G.seenI[t] >= disk[t])) G.seenI[t] = disk[t];
  const k = Object.keys(G.seenI); if (k.length > 300) k.slice(0, k.length - 300).forEach(t => delete G.seenI[t]);
  ls.set('seenI', G.seenI);
}, 500);
const hhmm = ts => ts ? new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) : '';
const EDIT_TOOLS = new Set(['Edit', 'MultiEdit', 'Write', 'NotebookEdit']);
const norm = s => String(s || '').replace(/\s+/g, ' ').trim();
const INTR = /^\[Request interrupted/;

class ChatView {
  constructor({ pane, tid, home }) {
    this.pane = pane || null; this.fixedTid = tid || null; this.tid = null; this.ro = !pane;
    this.home = !!home; this.keep = true; this.target = this.pane; this.atts = [];
    this.dkey = this.pane || this.fixedTid;
    this.rows = new Map(); this.results = new Map(); this.items = []; this.pending = [];
    this.first = 0; this.next = 0; this.stick = true; this.subs = [];
    this.el = h(`<div class="view chat${this.ro ? ' ro' : ''}${this.home ? ' home' : ''}${this.pane ? ' live' : ''}">
      <header class="bar"><div class="bar-in">
        ${this.home ? `<div class="brand"><span class="conn" data-conn="${G.conn}"></span><span class="t1">Master</span></div><span class="pill" hidden></span><button class="ovb" hidden aria-label="Context and Claude usage"></button><span class="sp"></span><button class="ib newchat" aria-label="New master chat">${I.compose}</button>`
        : `<button class="ib back" aria-label="Back">${I.back}</button>
        <button class="ttl" aria-label="Rename"><div class="t1w"><span class="cdot"></span><div class="t1"></div></div><div class="t2"></div></button>
        <button class="ib swb" aria-label="Switch agent">${I.down}</button>
        <span class="pill" hidden></span><button class="ctxb" hidden aria-label="Context"></button>
        <button class="ib subs" hidden aria-label="Subagents">${I.fork}<span class="n"></span></button>
        <button class="ib files" hidden aria-label="Files touched">${I.files}<span class="n"></span></button>`}
        <button class="ib more3" aria-label="More"${this.pane ? '' : ' hidden'}><svg viewBox="0 0 24 24"><circle cx="5" cy="12" r="1.6" fill="currentColor"/><circle cx="12" cy="12" r="1.6" fill="currentColor"/><circle cx="19" cy="12" r="1.6" fill="currentColor"/></svg></button>
        <button class="ib scr" aria-label="Terminal"${this.pane ? '' : ' hidden'}>${I.term}</button>
      </div></header>
      <div class="crumb" hidden></div>
      <div class="scroll msgs">
        <div class="older" hidden><button class="btn sm">Load earlier</button></div>
        <div class="list"></div><div class="pend"></div><div class="stline"></div>${home ? '<div class="nowl" hidden></div>' : ''}
      </div>
      <button class="newpill" hidden>↓ New</button>
      <div class="screenbox" hidden></div>
      <div class="blocked" hidden>
        <div class="bl-h"><span class="pill s-blocked"><i></i>Needs you</span><span class="sp"></span><button class="key sm why">Why?</button><button class="key sm open-scr">Terminal</button></div>
        <div class="bl-opts" hidden></div>
        <div class="bl-scr"><pre class="term"></pre></div>
        <div class="bl-keys">${keyBtns([['1', '1'], ['2', '2'], ['3', '3'], ['Enter', 'enter'], ['Esc', 'esc']], 'big')}${keyBtns([['↑', 'up'], ['↓', 'down'], ['Tab', 'tab'], ['⇧Tab', 'shift+tab']], 'big')}</div>
      </div>
      <footer class="composer">
        <div class="qpills" hidden></div>
        <div class="qk" hidden><button class="key sm bolt" aria-label="Snippets">${I.bolt}</button><div class="qchips"></div></div>
        <div class="vial" hidden></div>
        <div class="atts"></div>
        <div class="mtags" hidden></div>
        <div class="crow">
          <button class="ib attach" aria-label="Attach files">${I.clip}</button>
          <button class="ib mic" aria-label="Voice note"><svg viewBox="0 0 24 24"><rect x="9" y="3" width="6" height="11" rx="3"/><path d="M5.5 11a6.5 6.5 0 0 0 13 0M12 17.5V21"/></svg></button>
          <textarea rows="1" placeholder="Message" enterkeyhint="enter"></textarea>
          <button class="stop" aria-label="Stop" hidden>${I.stop}</button>
          <button class="send off" aria-label="Send">${I.up}</button>
        </div>
        <input type="file" multiple hidden>
        <input type="file" class="vfile" accept="audio/*,video/*,.m4a,.caf,.mp3,.wav,.aac,.ogg,.opus,.webm,.mov,.mp4" hidden>
      </footer>
    </div>`);
    const q = s => $(s, this.el);
    this.sc = q('.msgs'); this.list = q('.list'); this.pend = q('.pend'); this.stl = q('.stline');
    this.ta = q('textarea'); this.sendBtn = q('.send'); this.newPill = q('.newpill');
  }

  mount() {
    const q = s => $(s, this.el);
    if (this.home) {
      q('.newchat').onclick = newMasterChat; // (the status strip is gone from Master's header: Home's briefing has the counts)
    } else {
      q('.back').onclick = () => goBack(this.parentHash());
      q('.ttl').onclick = () => this.agent && !this.agent.home ? renameSheet(this.agent) : switcher();
      q('.swb').onclick = () => switcher();
      // swipe down on the header → switcher; swipe left → next agent that wants you (with a peek)
      let y0 = null, x0 = 0, hz = null;
      const bar = q('.bar'), bin = q('.bar-in');
      bar.addEventListener('touchstart', e => {
        y0 = e.touches.length === 1 ? e.touches[0].clientY : null; x0 = e.touches[0]?.clientX ?? 0; hz = null;
        if (x0 < 28) y0 = null; // edge swipe = back
      }, { passive: true });
      bar.addEventListener('touchmove', e => {
        if (y0 == null) return;
        const dx = e.touches[0].clientX - x0, dy = e.touches[0].clientY - y0;
        if (!hz) {
          if (dx < -12 && Math.abs(dx) > Math.abs(dy) * 1.4) { const n = nextUp(this.pane); hz = { n, el: this.peek(n) }; bin.style.transition = 'none'; }
          else return;
        }
        const d = Math.min(0, dx);
        bin.style.transform = `translate3d(${d * .35}px,0,0)`; bin.style.opacity = String(Math.max(.3, 1 + d / 300));
        hz.el.style.transform = `translate3d(${Math.max(0, 100 + d / innerWidth * 160)}%,0,0)`;
        hz.dx = d;
      }, { passive: true });
      const hend = e => {
        if (hz) {
          const n = hz.n, go = n && hz.dx < -70, { el } = hz; hz = null;
          bin.style.transition = el.style.transition = 'transform .18s ease-out, opacity .18s';
          bin.style.transform = ''; bin.style.opacity = ''; el.style.transform = go ? 'translate3d(0,0,0)' : '';
          setTimeout(() => { el.remove(); bin.style.transition = ''; }, 200);
          if (go) { haptic(); location.hash = agentHref(n); }
          y0 = null; return;
        }
        if (y0 != null && e.type === 'touchend' && e.changedTouches[0].clientY - y0 > 40) switcher(); y0 = null;
      };
      bar.addEventListener('touchend', hend); bar.addEventListener('touchcancel', hend);
      q('.subs').onclick = () => this.subsSheet();
      q('.files').onclick = () => lazy('files').then(m => m.filesSheet(this.tid, this.filesR));
    }
    q('.scr').onclick = () => this.toggleScreen();
    if (this.home) q('.ovb').onclick = () => this.agent && lazy('git').then(m => m.overviewSheet(this.agent)); // git.js: one overview sheet
    else q('.ctxb').onclick = () => this.agent && lazy('mb').then(m => m.ctxSheet(this.agent));
    q('.more3').onclick = () => lazy('v3').then(m => m.chatMenu(this)); // v3: desk, watches, review, fork
    q('.open-scr').onclick = () => this.toggleScreen(true);
    q('.qpills').onclick = e => { const b = e.target.closest('.qx'); if (b) lazy('talk').then(m => m.unsend(this, this.agent?.queued?.[+b.dataset.q])); }; // talk.js
    q('.why').onclick = () => lazy('fleet').then(m => m.whySheet(this.pane)); // fleet: herdr agent.explain
    q('.older button').onclick = () => this.older();
    this.newPill.onclick = () => this.toBottom(true);
    const seenLater = debounce(() => this.trackSeen(), 400);
    this.sc.addEventListener('scroll', () => {
      seenLater();
      this.stick = this.atBottom();
      if (this.stick) this.newPill.hidden = true;
      if (this.sc.scrollTop < 400 && this.first > 0 && this.tid) this.older();
    }, { passive: true });
    new ResizeObserver(() => { if (this.stick) this.toBottom(); }).observe(this.list);
    this.list.addEventListener('click', e => this.onListClick(e));
    longPress(this.list, '.msg.user .bub, .msg.asst', (el, e) => {
      if (e?.target?.closest?.('.cb, pre')) return false; // code stays natively selectable
      const m = el.closest('.msg'), it = this.items.find(x => x.i === +m.dataset.i);
      if (!it) return false;
      lazy('sheets').then(x => x.msgSheet(this, it, m));
    });
    q('.bl-opts').onclick = async e => {
      const b = e.target.closest('[data-o]'); if (!b) return;
      const all = [...q('.bl-opts').querySelectorAll('.opt')]; all.forEach(x => x.disabled = true); b.classList.add('busy'); haptic();
      try { await post(`a/${enc(this.pane)}/choose`, { index: +b.dataset.o }); }
      catch (err) { toast('Failed: ' + esc(err.message), 'err'); }
      all.forEach(x => x.disabled = false); b.classList.remove('busy'); this.blRev = null; this.pollBlocked(true);
    };
    q('.blocked').addEventListener('click', e => { if (e.target.closest('.bl-opts button, .bl-opts [data-o]')) setTimeout(() => this.acted(), 300); });
    q('.bl-keys').onclick = e => { const b = e.target.closest('[data-k]'); if (b) sendKeys(this.pane, [b.dataset.k]).then(() => this.pollBlocked(true)); };
    // quick row (⚡ snippets + suggestion chips): hidden by default; it shows when the agent ended
    // on a direct question, or while the text field is focused and empty (tap into it to reveal)
    q('.qk').onclick = e => {
      const c = e.target.closest('.qchip'); if (c) { this.send(c.dataset.t); return; }
      if (e.target.closest('.bolt')) snippetSheet(this.ta, () => this.autosize());
    };
    q('.qk').addEventListener('mousedown', e => e.preventDefault()); // keep the field focused (and the row up) on tap
    q('.qk').addEventListener('pointerdown', () => { this.qkHold = Date.now(); });
    longPress(q('.qchips'), '.qchip', b => { insertAt(this.ta, b.dataset.t); this.autosize(); });
    // composer
    const ta = this.ta;
    ta.value = ls.get('draft.' + this.dkey, '');
    for (const a of ls.get('att.' + this.dkey, [])) this.addAtt({ ...a, st: 'ok', url: IMG_EXT.test(a.path) ? fileUrl(a.path) : null });
    for (const e of OB) if (e.dkey === this.dkey) this.trackSend(e);
    const saveDraft = debounce(() => ls.set('draft.' + this.dkey, ta.value || null), 300);
    ta.oninput = () => { this.autosize(); saveDraft(); this.updQk(); };
    ta.addEventListener('focus', () => { this.taFocus = true; this.updQk(); });
    ta.addEventListener('blur', () => setTimeout(() => {
      if (document.activeElement === ta || Date.now() - (this.qkHold || 0) < 600) return;
      this.taFocus = false; this.updQk();
    }, 250));
    ta.onkeydown = e => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); this.send(); } };
    this.sendBtn.onclick = () => this.send();
    this.stopBtn = q('.stop'); this.stopBtn.onclick = () => this.stop();
    // long-press send → modifier tray (stackable toggles, pin per agent), plus interrupt / queue while working
    this.mods = new Set(ls.get('pmods.' + this.dkey, []));
    this.modPin = this.mods.size > 0;
    longPress(this.sendBtn, null, () => { if (!this.target) return false; this.modTray(); });
    q('.mtags').onclick = e => { const b = e.target.closest('[data-m]'); if (b) { haptic(); this.setMod(b.dataset.m, false); } };
    this.renderMods();
    const file = q('input[type=file]');
    q('.attach').onclick = () => file.click();
    file.onchange = () => { const fs = [...file.files]; file.value = ''; fs.forEach(f => this.attach(f)); };
    // voice notes (voice.js): MediaRecorder when secure, else the file picker — chosen in the tap (iOS)
    const vfile = q('.vfile');
    q('.mic').onclick = () => window.isSecureContext && window.MediaRecorder && navigator.mediaDevices?.getUserMedia ? lazy('voice').then(m => m.record(this)) : vfile.click();
    vfile.onchange = () => { const f = vfile.files[0]; vfile.value = ''; if (f) lazy('voice').then(m => m.transcribe(this, f)); };
    this.autosize(); this.shownAt = Date.now();
    if (this.pane && ls.get('scr.' + this.pane, false)) this.toggleScreen(true);
    this.onState(G.st);
  }
  destroy() {
    this.dead = true; this.closeStream(); this.saveCache();
    this.screen?.stop(); clearTimeout(this.blT); clearTimeout(this.subT);
  }
  onVisible() { if (this.tid) { this.openStream(); this.refreshSubs(); } this.screen?.start(); this.pollBlocked(); }
  suspend() { this.trackSeen(true); this.scTop = this.sc.scrollTop; this.onHidden(); clearTimeout(this.blT); clearTimeout(this.subT); }
  resume() {
    this.shownAt = Date.now();
    const seen = G.seenI[this.tid];
    const dr = ls.get('draft.' + this.dkey, ''); // may have been prefilled elsewhere (forward)
    if (dr !== this.ta.value) { this.ta.value = dr; this.autosize(); }
    this.onState(G.st);
    if (this.stick) requestAnimationFrame(() => this.toBottom()); else this.sc.scrollTop = this.scTop;
    if (this.loaded) this.sinceCheck(seen);
    if (!document.hidden) this.onVisible();
  }
  onHidden() { this.trackSeen(true); this.closeStream(); this.screen?.stop(); this.saveCache(); }

  // ----- since you looked (F4) -----
  trackSeen(force) {
    if (!this.tid || !this.loaded || !this.items.length || (!force && (G.view !== this || document.hidden)) || this.el.hidden) return;
    let i = null;
    if (this.atBottom()) i = this.next - 1;
    else {
      const r = this.sc.getBoundingClientRect();
      const e = document.elementFromPoint(r.left + r.width / 2, r.bottom - 10)?.closest('[data-i]');
      if (e && this.list.contains(e)) i = +e.dataset.i;
    }
    if (i == null || (G.seenI[this.tid] ?? -1) >= i) return;
    G.seenI[this.tid] = i; saveSeenI();
  }
  sinceCheck(seen) {
    $('.sincediv', this.list)?.remove();
    if (seen == null) { if (this.items.length) { G.seenI[this.tid] = this.next - 1; saveSeenI(); } return; }
    const fresh = this.items.filter(x => x.i > seen && VIS.has(x.k));
    if (fresh.length < 3) return;
    let el = this.list.querySelector(`[data-i="${fresh[0].i}"]`);
    while (el && el.parentElement !== this.list) el = el.parentElement;
    el ||= this.list.firstElementChild;
    if (!el) return;
    // catch-up counts from the new items
    const after = this.items.filter(x => x.i > seen), files = new Set(), agentIds = new Set(this.items.filter(x => x.k === 'agent').map(x => x.id));
    let steps = 0, subs = 0, jobs = 0;
    for (const x of after) {
      if (x.k === 'tool') { steps++; if (EDIT_TOOLS.has(x.name) && x.input?.file_path) files.add(x.input.file_path); }
      else if (x.k === 'result' && agentIds.has(x.id)) subs++;
      else if (x.k === 'note' && x.kind === 'task') jobs++;
    }
    const pl = (n, w) => n ? `${n} ${w}${n > 1 ? 's' : ''}` : '';
    const parts = [pl(steps, 'step'), pl(files.size, 'file'), subs ? `${subs} subagent${subs > 1 ? 's' : ''} finished` : '', jobs ? `${jobs} job${jobs > 1 ? 's' : ''} done` : ''].filter(Boolean);
    const rc = this.agent?.recap, ans = this.agent?.answerI ?? [...this.items].reverse().find(x => x.k === 'text')?.i;
    const seenTs = [...this.items].reverse().find(x => x.i <= seen && x.ts)?.ts;
    const div = h(`<div class="sincediv"><div class="sd-l"><span>New since you looked${seenTs ? ' · ' + hhmm(seenTs) : ''}</span></div>
      <div class="catchup">${parts.length ? `<div class="cu-n">${parts.join(' · ')}</div>` : ''}${rc?.text && rc.i > seen ? `<div class="cu-r"><b>Recap</b> ${esc(rc.text)}</div>` : ''}
      ${ans != null && ans > seen ? `<button class="btn sm cu-j" data-ans="${ans}">Jump to answer ↓</button>` : ''}${this.agent?.mod ? '<button class="btn sm cu-c">Catch me up</button>' : ''}</div></div>`);
    const j = $('.cu-j', div); if (j) j.onclick = () => this.focusItem(+j.dataset.ans);
    const cc = $('.cu-c', div); if (cc) cc.onclick = () => lazy('talk').then(m => m.catchUp(this, seen)); // talk.js: model digest
    this.list.insertBefore(div, el);
    if (this.wantI != null) return;
    requestAnimationFrame(() => requestAnimationFrame(() => {
      const sr = this.sc.getBoundingClientRect(), r = div.getBoundingClientRect();
      if (sr.bottom - r.top > this.sc.clientHeight) { this.sc.scrollTop += r.top - sr.top - 8; this.stick = false; this.newPill.hidden = false; }
    }));
  }

  parentHash() {
    if (this.pane) return '#/agents';
    const p = this.fixedTid.split('~')[0];
    if (p === this.fixedTid) return '#/';
    const a = G.st?.agents.find(a => a.tid === p);
    return a ? '#/a/' + enc(a.pane) : '#/t/' + enc(p);
  }

  onState(st) {
    const q = s => $(s, this.el);
    if (this.home) {
      if (st && st.home !== this.pane && !(st.home == null && G.homePending === this.pane)) { G.route = null; route(); return; }
    }
    if (this.home) updPchips(this.list);
    if (this.pane) {
      const a = st?.agents.find(x => x.pane === this.pane);
      const shell = !a && st?.shells?.find(x => x.pane === this.pane);
      if (shell) { location.replace('#/s/' + enc(this.pane)); return; }
      this.agent = a;
      // worker agents: breadcrumb back to whoever started them
      if (!this.home) {
        const pp = a?.parent?.pane, par = pp && st.agents.find(x => x.pane === pp);
        const sig = pp ? `${pp}|${par?.title}|${a.parent.via}` : '';
        if (sig !== this.parSig) {
          this.parSig = sig; const c = q('.crumb');
          if (pp) { c.hidden = false; c.innerHTML = `<a href="#/a/${enc(pp)}">${I.back}${esc(par?.home ? 'Master' : par?.title || pp)}</a><span>${{ spawn: 'started this agent', review: 'is being reviewed by this agent', fork: 'was forked into this agent', drive: 'is driving this agent', split: 'split off this agent' }[a.parent.via] || 'is this agent’s parent'}</span>`; }
          else c.hidden = true;
        }
      }
      if (!a) {
        if (!this.home) { q('.t1').textContent = G.starting.has(this.pane) ? 'New agent' : this.pane; q('.t2').textContent = ''; }
        if (!this.tid) this.status(G.starting.has(this.pane) ? '<span class="spin"></span> Starting…' : st ? 'Agent not found. It may have been closed.' : 'Connecting…');
        q('.pill').hidden = true; this.setBlocked(false);
        return;
      }
      const s = dispStatus(a);
      if (!this.home) q('.t1').textContent = a.title || a.name || a.kind;
      const dg = doingOf(a), dsig = dg ? 'd' + dg.label + dg.ts : shortPath(a.cwd) + ' · ' + (a.wsLabel || a.ws) + (a.tabLabel ? ' / ' + a.tabLabel : '');
      if (dsig !== this.dsig) {
        this.dsig = dsig;
        const t2 = this.home ? q('.nowl') : q('.t2');
        t2.classList.toggle('doing', !!dg);
        if (dg) { t2.innerHTML = doingHTML(dg); tick(t2); } else if (this.home) t2.textContent = ''; else t2.textContent = dsig;
        if (this.home) t2.hidden = !dg;
      }
      const pill = q('.pill'); pill.hidden = false; pill.className = 'pill s-' + s; pill.innerHTML = `<i></i>${LABEL[s]}`;
      // Master: one button = context ring + plan usage (git.js G.usage.bit); workers: the context ring alone
      const cxb = q(this.home ? '.ovb' : '.ctxb'), cx = this.home ? (ctxHTML(a) || '<span class="ctxg"></span>') + (G.usage?.bit() || '') : ctxHTML(a);
      cxb.hidden = !cx; if (cx !== this.cxSig) { this.cxSig = cx; cxb.innerHTML = cx; }
      setCol(this.el, a.home ? null : a.color);
      this.updChips(a);
      if (!document.hidden) markSeen(a), badge();
      this.setBlocked(a.status === 'blocked');
      if (a.status === 'blocked') this.renderOpts();
      this.setWorking(a);
      if (a.tid && a.tid !== this.tid) this.load(a.tid);
      else if (!a.tid && !this.tid) this.emptyState(true);
      if (a.subagents) this.subCount(a.subagents.running, a.subagents.total);
      if (MODS.v3) MODS.v3.chatState(this, st); else if (a.watches?.length || st.surfaces?.some(x => x.review && x.pane === this.pane)) lazy('v3').then(m => m.chatState(this, G.st));
      if (MODS.talk) MODS.talk.chatState(this, a); else lazy('talk').then(m => m.chatState(this, this.agent)); // TL;DR, catch-up, denials
    } else {
      if (!this.tid) this.load(this.fixedTid);
      if (!this.fixedTid.includes('~')) lazy('fleet').then(m => m.roChat(this, G.st), () => { }); // fleet: Resume / Open live
      this.setVia(st);
      this.crumb();
    }
  }
  // subagent view: compose via the parent session's pane
  setVia(st) {
    const [p, ag] = this.fixedTid.split('~');
    const pa = ag && st?.agents.find(a => a.tid === p || a.session === p);
    this.target = pa?.pane || null;
    this.el.classList.toggle('ro', !this.target);
    this.el.classList.toggle('via', !!this.target);
    if (!pa) return;
    this.viaTitle = pa.home ? 'Master' : pa.title || pa.pane;
    const v = $('.vial', this.el); v.hidden = false; v.textContent = 'Message via ' + this.viaTitle;
    this.updPh();
  }
  crumb() {
    const c = $('.crumb', this.el), [p, ag] = this.fixedTid.split('~');
    if (!ag) { c.hidden = true; return; }
    const a = G.st?.agents.find(a => a.tid === p);
    c.hidden = false;
    c.innerHTML = `<a href="${this.parentHash()}">${I.back}${esc(a?.title || this.parentTitle || 'Parent session')}</a><span>/ subagent${this.target ? '' : ' · read-only'}</span>`;
  }
  status(html) { this.stl.innerHTML = html || ''; }
  // files-touched count in the header (F8); hidden when 0 or unsupported
  async refreshFiles() {
    const b = $('.files', this.el), tid = this.tid;
    if (!b || !tid || tid.includes('~')) return;
    try {
      const r = await api(`t/${enc(tid)}/files`);
      if (tid !== this.tid) return;
      this.filesR = r; const n = r.files?.length || 0;
      b.hidden = !n; $('.n', b).textContent = n || '';
    } catch { b.hidden = true; }
  }
  emptyState(on) {
    if (!!this.empty === on) return;
    this.empty = on;
    this.status(on ? `<div class="emptyc">${I.chat}<b>New conversation</b><span>Say something — it goes straight to the agent.</span></div>` : '');
    if (this.agent) this.updChips(this.agent);
  }
  // quick replies (F3) in the hidden-by-default quick row; with no suggestions the row offers snippets.
  // The row shows on its own only when the agent ended on a direct question (same rule as the
  // triage deck's "asks you"), and then without the "thanks" staple.
  updChips(a) {
    let list = [];
    clearTimeout(this.chipT);
    this.qAsk = false;
    if (a.status === 'working') {
      const age = now() - (a.since || now());
      if (age > 60000) list = [['hows that going?']];
      else this.chipT = setTimeout(() => this.agent && this.updChips(this.agent), 60500 - age);
    } else if (a.status !== 'blocked' && a.chips?.length) {
      list = a.chips.map(c => [c]);
      const txt = [...this.items].reverse().find(x => x.k === 'text')?.text ?? (a.last?.role === 'assistant' ? a.last.text : '');
      this.qAsk = asksYou(txt) || a.chips.some(c => /^(yes|no)\b/i.test(c));
    }
    if (!list.length && a.status !== 'blocked') list = (G.prefs?.snippets || DEF_SNIPS).slice(0, 4).map(x => [x.label || x.text, x.text]);
    const sig = JSON.stringify([list, this.qAsk]);
    if (sig !== this.chipSig) {
      this.chipSig = sig;
      const shown = this.qAsk ? list.filter(([l]) => !/^(thanks?|thank you|ty)[.!]*$/i.test(l)) : list;
      this.qList = list; this.qAskList = shown;
    }
    this.updQk();
  }
  // the quick row: question chips by default, everything (⚡ + chips/snippets) while the empty field is focused
  updQk() {
    const box = $('.qk', this.el); if (!box) return;
    const reveal = this.taFocus && !this.ta.value.trim() && this.agent?.status !== 'blocked';
    const list = reveal ? this.qList || [] : this.qAsk ? this.qAskList || [] : [];
    const on = reveal || list.length > 0;
    const html = list.map(([l, t]) => `<button class="qchip" data-t="${esc(t ?? l)}">${esc(l)}</button>`).join('');
    const qc = $('.qchips', box);
    if (qc.dataset.h !== html) { qc.innerHTML = html; qc.dataset.h = html; }
    box.classList.toggle('ask', !reveal);
    box.hidden = !on;
  }
  subCount(run, tot) {
    const b = $('.subs', this.el);
    if (!b) return;
    b.hidden = !tot; $('.n', b).textContent = run ? run : tot;
    b.classList.toggle('running', !!run);
  }

  // ----- transcript loading -----
  async load(tid) {
    this.closeStream(); this.saveCache();
    this.tid = tid; this.reset(); this.loaded = false;
    const seen = G.seenI[tid];
    const c = G.tc.get(tid) || ls.get('t.' + tid, null);
    if (c?.items?.length) { this.meta(c); this.append(c.items, true); this.first = c.first; this.next = c.total; }
    else this.status('<span class="spin"></span> Loading…');
    try {
      const r = await api(`t/${enc(tid)}?tail=300`);
      if (this.tid !== tid || this.dead) return;
      this.meta(r);
      if (c?.items?.length && r.total >= c.total && r.first <= c.first) {
        this.append(r.items.filter(x => x.i >= this.next));
        const older = r.items.filter(x => x.i < this.first);
        if (older.length) this.prepend(older);
      } else { this.reset(); this.append(r.items, true); }
      this.first = r.first; this.next = Math.max(this.next, r.total);
      this.refreshFiles();
      this.status(''); this.loaded = true; this.emptyState(!r.items.length && !this.items.length);
      this.sinceCheck(seen);
      this.openStream();
      if (this.wantI != null) this.focusItem(this.wantI);
    } catch (e) {
      if (this.tid !== tid || this.dead) return;
      this.status(`Couldn’t load transcript: ${esc(e.message)} <button class="btn sm retry">Retry</button>`);
      $('.retry', this.stl).onclick = () => { this.tid = null; this.load(tid); };
    }
  }
  meta(r) {
    if (r.subagents) { this.subs = r.subagents; this.subCount(this.subs.filter(s => s.running).length, this.subs.length); this.updateAgentRows(); }
    if (this.ro && 'tid' in r) {
      const t1 = $('.t1', this.el);
      t1.textContent = r.agentId ? (this.subTitle() || 'Subagent') : (r.title || 'Transcript');
      $('.t2', this.el).textContent = shortPath(r.cwd);
      if (r.agentId) {
        this.crumb();
        if (!this.subTitle()) {
          const p = this.fixedTid.split('~')[0], c = G.tc.get(p);
          const set = subs => { this.parentSubs = subs || []; t1.textContent = this.subTitle() || 'Subagent'; };
          if (c?.subagents) set(c.subagents);
          else api(`t/${enc(p)}?after=999999999`).then(x => { set(x.subagents); this.parentTitle ||= x.title; this.crumb(); }).catch(() => { });
        }
      }
    }
  }
  subTitle() {
    const ag = this.fixedTid.split('~')[1];
    const sub = (this.parentSubs || this.subs).find(s => s.agentId === ag);
    return sub ? `${sub.type}: ${sub.desc}` : null;
  }
  reset() {
    this.list.textContent = ''; this.rows.clear(); this.results.clear(); this.items = [];
    this.first = this.next = 0; this.stick = true;
  }
  saveCache() {
    if (!this.tid || !this.items.length) return;
    const c = { items: this.items.slice(-800), total: this.next, subagents: this.subs };
    c.first = c.items[0].i;
    G.tc.set(this.tid, c);
    const lite = { items: this.items.slice(-60), total: this.next, subagents: this.subs }; lite.first = lite.items[0].i;
    const tids = ls.get('tids', []).filter(t => t !== this.tid); tids.unshift(this.tid);
    tids.splice(6).forEach(t => ls.set('t.' + t, null));
    ls.set('tids', tids); ls.set('t.' + this.tid, lite);
  }
  openStream() {
    this.closeStream();
    if (this.dead || document.hidden || !this.tid) return;
    const s = this.es = new EventSource(`/api/t/${enc(this.tid)}/stream?after=${this.next}`);
    s.addEventListener('items', e => {
      this.sTry = 0;
      let d; try { d = JSON.parse(e.data); } catch { return; }
      this.append(d.items || []);
      if (d.total > this.next) this.next = d.total;
      if (d.items?.some(x => x.k === 'agent' || (x.k === 'result' && this.rows.get(x.id)?.classList.contains('agent')))) this.refreshSubs();
    });
    s.onerror = () => {
      if (this.es !== s) return;
      s.close(); this.es = null;
      this.sT = setTimeout(() => this.openStream(), Math.min(8000, 500 * 2 ** (this.sTry = (this.sTry || 0) + 1)));
    };
    this.refreshSubs();
  }
  closeStream() { clearTimeout(this.sT); this.es?.close(); this.es = null; }
  async refreshSubs() {
    clearTimeout(this.subT);
    if (this.dead || !this.tid || document.hidden || this.tid.includes('~')) return;
    try { const tid = this.tid, r = await api(`t/${enc(tid)}?after=${this.next}`); if (tid === this.tid) { if (r.subagents) this.meta({ subagents: r.subagents }); this.append(r.items || []); } } catch { }
    if (this.subs.some(s => s.running) && !this.dead) this.subT = setTimeout(() => this.refreshSubs(), 4000);
  }
  older() {
    if (this.first <= 0 || !this.tid) return Promise.resolve();
    return this.olderP ||= this._older().finally(() => { this.olderP = null; });
  }
  async _older() {
    $('.older', this.el).hidden = false;
    try {
      const tid = this.tid, r = await api(`t/${enc(tid)}?before=${this.first}&tail=200`);
      if (tid !== this.tid) return;
      const items = r.items.filter(x => x.i < this.first);
      if (items.length) this.prepend(items);
      this.first = items.length ? Math.min(r.first, items[0].i) : 0;
    } catch (e) { toast('Load failed: ' + esc(e.message), 'err'); }
    finally { $('.older', this.el).hidden = this.first <= 0; }
  }

  // ----- land on an item (?i=N) -----
  focusItem(i) {
    if (!this.loaded) { this.wantI = i; return; }
    this.wantI = null;
    if (!(i >= 0) || i >= this.next) { requestAnimationFrame(() => this.toBottom()); return; } // unknown → bottom
    if (i >= this.first) return this.flashTo(i);
    if (this.first - i <= 600) this.pageTo(i); else this.around(i);
  }
  async pageTo(i) {
    while (this.first > i && this.first > 0) { const f = this.first; await this.older(); if (this.first === f) break; }
    this.flashTo(i);
  }
  async around(i) {
    const tid = this.tid;
    try {
      const r = await api(`t/${enc(tid)}?around=${i}&tail=200`);
      if (tid !== this.tid || this.dead || !r.items?.length || r.items[0].i > i) return;
      this.closeStream(); this.reset(); this.append(r.items);
      this.first = r.items[0].i; $('.older', this.el).hidden = this.first <= 0;
      this.flashTo(i); this.openStream();
    } catch { }
  }
  flashTo(i) {
    let el = this.list.querySelector(`[data-i="${i}"]`);
    if (!el) { const it = this.items.find(x => x.i === i); if (it?.id) el = this.rows.get(it.id); }
    if (!el) { for (const e of this.list.querySelectorAll('[data-i]')) { if (+e.dataset.i <= i) el = e; else break; } }
    if (!el) return;
    const g = el.closest('.grp.fold, .grp.hfold'); if (g) this.unfold(g);
    this.stick = false; this.newPill.hidden = true;
    requestAnimationFrame(() => {
      const sr = this.sc.getBoundingClientRect(), r = el.getBoundingClientRect();
      this.sc.scrollTop += r.top - sr.top - this.sc.clientHeight * 0.3;
      this.stick = false;
      el.classList.remove('flash'); void el.offsetWidth; el.classList.add('flash');
      setTimeout(() => el.classList.remove('flash'), 1300);
    });
  }
  unfold(g) { g.classList.remove('fold', 'hfold'); g.dataset.open = 1; $(':scope > .gmore', g)?.remove(); }

  // ----- rendering -----
  atBottom() { const s = this.sc; return s.scrollHeight - s.scrollTop - s.clientHeight < 60; }
  toBottom(smooth) { this.sc.scrollTo({ top: this.sc.scrollHeight, behavior: smooth ? 'smooth' : 'auto' }); this.stick = true; this.newPill.hidden = true; }

  append(items, initial) {
    if (!items.length) return;
    if (this.empty) this.emptyState(false);
    const atB = initial || this.stick || this.atBottom();
    let vis = false;
    for (const it of items) {
      if (it.i < this.next && this.items.length) continue;
      this.next = it.i + 1; this.items.push(it);
      if (this.items.length === 1) this.first = it.i;
      this.renderItem(it, this.list);
      if (VIS.has(it.k)) vis = true;
      if (it.k === 'tool' && EDIT_TOOLS.has(it.name) && !initial) this.filesSoon ||= setTimeout(() => { this.filesSoon = 0; this.refreshFiles(); }, 2500);
      if (it.k === 'user') this.reconcile(it);
    }
    if (atB) requestAnimationFrame(() => this.toBottom()); else if (vis) this.newPill.hidden = false;
    $('.older', this.el).hidden = this.first <= 0;
    if (this.home) updPchips(this.list);
  }
  prepend(items) {
    const tmp = document.createElement('div');
    for (const it of items) this.renderItem(it, tmp);
    if (this.home) updPchips(tmp);
    const s = this.sc, oh = s.scrollHeight, ot = s.scrollTop;
    this.list.prepend(...tmp.childNodes);
    s.scrollTop = ot + (s.scrollHeight - oh);
    this.items.unshift(...items); this.first = items[0].i;
  }
  group(box) {
    let g = box.lastElementChild;
    if (!g || !g.classList.contains('grp')) g = box.appendChild(h('<div class="grp"></div>'));
    return g;
  }
  addRow(g, el) {
    g.append(el);
    this.gsum(g);
  }
  gsum(g) {
    if (!g || g.dataset.open) return;
    const n = g.querySelectorAll(':scope > .row').length;
    if (!this.home && n <= 4) return;
    g.classList.add(this.home ? 'hfold' : 'fold');
    let m = $(':scope > .gmore', g);
    if (!m) { m = h('<button class="gmore"></button>'); g.prepend(m); }
    m.innerHTML = this.home ? `⋯ ${n} step${n > 1 ? 's' : ''}${$(':scope > .row.tool.run', g) ? ' <span class="spin"></span>' : ''}` : `${n - 3} earlier steps`;
  }
  // agent → agent message (subagent hand-back report, a prompt another agent pasted in, or a steer another agent
  // sent with `from`): a card, not a bubble
  agentMsg(it) {
    // the sender is named up front with its own badge and color (never this chat's color)
    let who, what, href = '', cls = ' am-nc', av;
    if (it.kind === 'report') {
      const s = this.subs.find(x => x.agentId === it.agentId);
      who = s?.desc || 'Subagent'; what = 'subagent report'; av = I.fork;
      if (s?.tid) href = '#/t/' + enc(s.tid);
    } else if (it.kind === 'steer') {
      const p = steerAgent(it), pane = p?.pane || it.agentId;
      who = p ? (p.home ? 'Master' : p.title || pane) : it.from || 'Another agent';
      what = this.home ? 'sent this to Master' : 'sent this to the agent'; av = esc(who.slice(0, 1).toUpperCase());
      if (pane) href = '#/a/' + enc(pane);
      if (colCls(p?.color)) cls = colCls(p.color);
    } else {
      const a = G.st?.agents.find(x => x.pane === this.pane), pp = a?.parent?.pane;
      const p = pp && /spawn|drive/.test(a.parent.via) ? G.st.agents.find(x => x.pane === pp) : null;
      who = p ? (p.home ? 'Master' : p.title || pp) : 'Pasted message';
      what = p ? 'sent this to the agent' : 'not typed in this app';
      av = esc((p ? who : '⎘').slice(0, 1).toUpperCase());
      if (p) { href = '#/a/' + enc(pp); if (colCls(p.color)) cls = colCls(p.color); }
    }
    const long = it.text.length > 700 || it.text.split('\n').length > 12;
    const tag = href ? `a href="${href}"` : 'div';
    return h(`<div class="amsg${cls}" data-i="${it.i}"><${tag} class="am-h"><span class="am-av">${av}</span><span class="am-w"><b>${esc(who)}</b><small>${what}${href ? ' · open' : ''}</small></span>${href ? '<i>›</i>' : ''}</${href ? 'a' : 'div'}><div class="am-b md${long ? ' long' : ''}">${mdx(it.text, this.home)}</div></div>`);
  }
  renderItem(it, box) {
    switch (it.k) {
      case 'user': {
        if (it.kind === 'report' || it.kind === 'paste' || it.kind === 'steer') { box.append(this.agentMsg(it)); break; }
        if (INTR.test(it.text)) { box.append(h(`<div class="note n-info" data-i="${it.i}"><span>Interrupted</span></div>`)); break; }
        const [html, imgs] = textx(it.text, this.home);
        (it.images || []).forEach(u => imgs.add(u));
        const long = it.text.length > 700 || it.text.split('\n').length > 12;
        box.append(h(`<div class="msg user" data-i="${it.i}"><div class="bub${long ? ' long' : ''}">${html}</div>${thumbs(imgs)}</div>`));
        break;
      }
      case 'text':
        box.append(h(`<div class="msg asst md" data-i="${it.i}">${mdx(it.text, this.home)}</div>`));
        break;
      case 'thinking': {
        const el = h(`<div class="row think" data-i="${it.i}"><div class="rh"><span class="chev"></span><b>Thinking</b><span class="sum">${esc(it.text.slice(0, 120))}</span></div></div>`);
        el.item = it; this.addRow(this.group(box), el);
        break;
      }
      case 'tool': {
        if (it.name === 'mcp__sheprd__surface' && it.input?.title) {
          const x = it.input, el = h(surfHTML({ ...x, i: it.i, tid: this.tid, level: x.level }, { inline: true, pane: this.target }));
          el.dataset.id = it.id; el.item = it; this.rows.set(it.id, el); box.append(el);
          break;
        }
        if (it.name === 'Artifact' && !it.input?.asset && (!it.input?.action || it.input.action === 'publish')) { // rich card (art.js)
          const el = h(`<div class="artc" data-id="${esc(it.id)}" data-i="${it.i}"></div>`);
          el.item = it; this.rows.set(it.id, el); box.append(el);
          el.res = this.results.get(it.id); lazy('art').then(m => m.card(el, this), () => { });
          break;
        }
        const el = h(`<div class="row tool run" data-id="${esc(it.id)}" data-i="${it.i}"><div class="rh"><span class="chev"></span><b>${esc(it.name)}</b><span class="sum">${esc(it.summary || '')}</span><span class="st"></span></div></div>`);
        el.item = it; this.rows.set(it.id, el); this.addRow(this.group(box), el);
        const r = this.results.get(it.id); if (r) this.attachResult(el, r);
        if (it.name === 'Write' && /\.(html?|md|markdown|pdf|svg|png|jpe?g|webp|gif|csv)$/i.test(it.input?.file_path || '')) lazy('art').then(m => m.output(el, this), () => { });
        break;
      }
      case 'agent': {
        const el = h(`<div class="row agent run" data-id="${esc(it.id)}" data-i="${it.i}"><div class="ah">${I.fork}<span class="atype">${esc(it.type || 'agent')}</span><span class="adesc">${esc(it.desc || '')}</span><span class="st"></span><span class="go">›</span></div></div>`);
        el.item = it; this.rows.set(it.id, el);
        box.append(el);
        const r = this.results.get(it.id); if (r) this.attachResult(el, r);
        this.updateAgentRows(el);
        break;
      }
      case 'result': {
        this.results.set(it.id, it);
        const el = this.rows.get(it.id); if (el) this.attachResult(el, it);
        break;
      }
      case 'note':
        box.append(h(it.sub
          ? `<div class="note n-${esc(it.kind || 'info')}" data-i="${it.i}"><a class="subnote" href="#/t/${enc(it.sub)}">${I.fork}<span>${esc(it.text)}</span><b>›</b></a></div>`
          : `<div class="note n-${esc(it.kind || 'info')}" data-i="${it.i}"><span>${linkify(it.text)}</span></div>`));
        break;
    }
  }
  attachResult(el, r) {
    el.res = r; el.classList.remove('run'); el.classList.toggle('err', !!r.err); el.onres?.(r);
    if (this.home && el.parentElement?.classList.contains('hfold')) this.gsum(el.parentElement);
    if (el.classList.contains('open')) this.fillBody(el);
    if (el.classList.contains('agent')) this.updateAgentRows(el);
  }
  findSub(it) { return this.subs.find(s => (it.agentId && s.agentId === it.agentId) || s.toolUseId === it.id); }
  updateAgentRows(only) {
    const rows = only ? [only] : [...this.list.querySelectorAll('.row.agent')];
    for (const el of rows) {
      const sub = this.findSub(el.item);
      const running = sub ? sub.running : !el.res;
      el.classList.toggle('run', !!running);
      $('.st', el).innerHTML = running ? '<span class="spin"></span> running' : el.res ? (el.res.err ? 'failed' : 'done') : '';
    }
  }
  onListClick(e) {
    if (e.target.closest('a')) return;
    const more = e.target.closest('.gmore');
    if (more) { this.unfold(more.parentElement); return; }
    const full = e.target.closest('.full');
    if (full) return this.showFull(full);
    const row = e.target.closest('.row');
    if (row?.classList.contains('agent')) {
      const it = row.item, sub = this.findSub(it), tid = it.sub || sub?.tid;
      if (tid) location.hash = '#/t/' + enc(tid); else toast('Subagent transcript not available yet');
      return;
    }
    if (row && e.target.closest('.rh')) {
      row.classList.toggle('open');
      if (row.classList.contains('open')) this.fillBody(row); else $(':scope > .rb', row)?.remove();
      return;
    }
    const n = e.target.closest('.note, .msg.user .bub, .amsg .am-b.long');
    if (n) n.classList.toggle('x');
  }
  fillBody(row) {
    $(':scope > .rb', row)?.remove();
    const it = row.item, r = row.res, ctx = { imgs: new Set() };
    const pre = (t, c = '') => c === 'cmd' ? codeHTML(t, 'bash', c) : `<pre class="${c}">${linkify(t, ctx)}</pre>`;
    let b = '';
    if (it.k === 'thinking') b = `<div class="md think-b">${md(it.text, ctx)}</div>`;
    else {
      const x = it.input || {};
      switch (it.name) {
        case 'Bash': b = (x.description ? `<div class="dim">${esc(x.description)}</div>` : '') + pre(x.command, 'cmd'); break;
        case 'Edit': case 'MultiEdit': {
          const edits = x.edits || [x];
          b = `<div class="dim">${linkify(x.file_path, ctx)}</div>` + edits.map(e =>
            `<pre class="diff">${String(e.old_string ?? '').split('\n').map(l => `<del>- ${esc(l)}</del>`).join('\n')}\n${String(e.new_string ?? '').split('\n').map(l => `<ins>+ ${esc(l)}</ins>`).join('\n')}</pre>`).join('');
          break;
        }
        case 'Write': b = `<div class="dim">${linkify(x.file_path, ctx)}</div>` + pre(x.content); break;
        default: {
          const keys = Object.keys(x);
          if (keys.length) b = pre(keys.length === 1 && typeof x[keys[0]] === 'string' ? x[keys[0]] : JSON.stringify(x, null, 2));
        }
      }
      if (r) {
        b += `<div class="res${r.err ? ' err' : ''}">${r.text ? pre(r.text) : '<div class="dim">(no output)</div>'}`;
        (r.images || []).forEach(u => ctx.imgs.add(u));
        if (r.cut) b += `<button class="btn sm full" data-i="${r.i}">Show full output</button>`;
        b += '</div>';
      } else b += '<div class="dim"><span class="spin"></span> running…</div>';
    }
    row.append(h(`<div class="rb">${b}${thumbs(ctx.imgs)}</div>`));
  }
  async showFull(btn) {
    btn.disabled = true; btn.textContent = 'Loading…';
    try {
      const r = await api(`t/${enc(this.tid)}/full/${btn.dataset.i}`);
      const p = btn.parentElement.querySelector('pre');
      p.innerHTML = linkify(r.text); btn.remove();
    } catch (e) { btn.disabled = false; btn.textContent = 'Retry: ' + e.message; }
  }

  // ----- steer / stop -----
  setWorking(a) {
    const on = a.status === 'working' && this.target === this.pane;
    if (on !== this.working) {
      this.working = on;
      this.el.classList.toggle('wk', on);
      this.stopBtn.hidden = !on;
      this.sendBtn.innerHTML = on ? I.queue : I.up;
    }
    this.updPh();
    const qs = Array.isArray(a.queued) ? a.queued : [], sig = qs.map(x => x.text + (x.id || '')).join('\u0000');
    if (sig !== this.qSig) {
      this.qSig = sig;
      const box = $('.qpills', this.el);
      box.hidden = !qs.length;
      // × takes it back before the agent reads it (talk.js)
      box.innerHTML = qs.map((x, i) => `<span class="qpill"><span class="qt"><b>Queued</b> · ${esc(x.text.replace(/\s+/g, ' ').slice(0, 120))}</span><button class="qx" data-q="${i}" aria-label="Take back">×</button></span>`).join('');
    }
  }
  async stop() {
    const b = this.stopBtn; if (b.disabled) return;
    b.disabled = true; haptic();
    let queued = (this.agent?.queued || []).map(x => x.text);
    for (const p of this.pending) if (p.steer && !queued.some(t => norm(t) === p.text)) queued.push(p.raw);
    const lu = [...this.items].reverse().find(x => x.k === 'user' && !INTR.test(x.text)), lastUser = lu?.text;
    try {
      await post(`a/${enc(this.pane)}/interrupt`);
      this.pending = this.pending.filter(p => { if (p.steer) p.el.remove(); return !p.steer; });
      if (!this.ta.value.trim()) {
        // a tool-use interrupt makes Claude submit its queue; those texts already reached it
        const got = new Set(this.items.filter(x => x.k === 'user' && x.i > (lu?.i ?? -1)).map(x => norm(x.text)));
        queued = queued.filter(t => !got.has(norm(t)));
        this.ta.value = [lastUser, ...queued].filter(Boolean).join('\n');
        this.stopQ = { raw: queued, at: Date.now() };
        this.autosize(); ls.set('draft.' + this.dkey, this.ta.value || null);
        this.ta.focus(); this.ta.setSelectionRange(this.ta.value.length, this.ta.value.length);
      }
      toast('Stopped · edit & resend', 'ok');
    } catch (e) {
      if (e.status === 409) this.setBlocked(true);
      else toast('Stop failed: ' + esc(e.message), 'err');
    } finally { b.disabled = false; }
  }

  // ----- sending -----
  autosize() {
    const t = this.ta; t.style.height = 'auto'; t.style.height = Math.min(t.scrollHeight, 168) + 'px';
    this.updSend();
  }
  updSend() {
    const ok = this.atts.some(a => a.st === 'ok'), up = this.atts.some(a => a.st === 'up');
    // "off" not disabled: an empty Send still takes the long-press that opens the modifier tray
    this.sendBtn.classList.toggle('off', up || (!this.ta.value.trim() && !ok));
  }
  saveAtts() { ls.set('att.' + this.dkey, this.atts.filter(a => a.st === 'ok').map(({ name, path }) => ({ name, path })) || null); }
  addAtt(a) {
    const ext = (a.name.match(/\.(\w{1,5})$/)?.[1] || 'file').toUpperCase();
    a.el = h(`<div class="att${a.st === 'up' ? ' up' : ''}">${a.url ? `<img src="${esc(a.url)}" alt="">` : `<span class="fi">${esc(ext)}</span>`}<span class="an">${esc(a.name)}</span><button class="ax" aria-label="Remove">${I.x}</button></div>`);
    $('.ax', a.el).onclick = () => {
      a.el.remove(); this.atts = this.atts.filter(x => x !== a);
      if (a.url?.startsWith('blob:')) URL.revokeObjectURL(a.url);
      this.saveAtts(); this.updSend();
    };
    this.atts.push(a); $('.atts', this.el).append(a.el); this.updSend();
    return a;
  }
  async attach(f) {
    const img = /^image\//.test(f.type);
    const a = this.addAtt({ name: f.name || 'file', st: 'up', url: img ? URL.createObjectURL(f) : null });
    try {
      let blob = f, name = a.name;
      if (img) try { const s = await shrink(f); if (s) { blob = s; name = name.replace(/\.\w+$/, '') + '.jpg'; } } catch { }
      const r = await api(`upload?name=${enc(name)}`, { method: 'POST', body: blob, headers: { 'content-type': blob.type || 'application/octet-stream' } });
      a.path = r.path; a.st = 'ok'; a.el.classList.remove('up');
      this.saveAtts();
    } catch (e) {
      a.st = 'err'; a.el.classList.remove('up'); a.el.classList.add('err');
      toast('Upload failed: ' + esc(e.message), 'err');
    }
    this.updSend();
  }
  async send(text, mode) {
    let undo = null;
    if (!text) {
      undo = { typed: this.ta.value, atts: this.atts.filter(a => a.st === 'ok').map(({ name, path, url }) => ({ name, path, url })), mods: [...this.mods] };
      const lines = this.atts.filter(a => a.st === 'ok').map(a => `[attached: ${a.path}]`);
      if (this.sendBtn.classList.contains('off') && !mode) return;
      // typed shorthands ("//sub", "//Ro." …) join the tray's modifiers for this send
      let [t, typed] = takeMods(this.ta.value.trim());
      t = t.trim();
      const keys = [...new Set([...this.mods, ...typed])];
      if (t) t += modSuffix(keys);
      text = [t, ...lines].filter(Boolean).join('\n');
      if (text && !this.modPin && this.mods.size) { this.mods.clear(); this.renderMods(); }
    }
    if (!text || !this.target) return;
    haptic();
    this.ta.value = ''; ls.set('draft.' + this.dkey, null);
    this.atts.forEach(a => { a.el.remove(); if (a.url?.startsWith('blob:') && !undo) URL.revokeObjectURL(a.url); });
    this.atts = []; this.saveAtts(); this.autosize();
    const fresh = Date.now() - (this.shownAt || 0) < 2000; // just switched here: show who gets it
    if (fresh) this.flashRecipient();
    const via = this.target !== this.pane;
    let body = text;
    if (via) {
      const ag = this.fixedTid.split('~')[1], sub = (this.parentSubs || this.subs).find(s => s.agentId === ag);
      body = `Message for your subagent "${sub?.desc || ag}" (agentId ${ag}): ${text}`;
    }
    const e = { nonce: rid(), dkey: this.dkey, raw: text, ts: Date.now(), via: via ? this.viaTitle : null,
      steer: !!this.working && !via && mode !== 'interrupt' };
    // a steer goes through the mod into the running turn (read at its next step) when it can
    e.path = `a/${enc(this.target)}/${mode === 'interrupt' ? 'interrupt' : e.steer ? 'steer' : 'prompt'}`;
    e.body = { text: body, nonce: e.nonce };
    // composer sends that start a turn wait 3 s with Undo (steers can already be taken back)
    const hold = undo && mode !== 'interrupt' && !e.steer;
    if (hold) { e.undo = undo; e.hold = Date.now() + UNDO_MS; }
    this.toBottom();
    this.trackSend(e);
    if (hold) obHold(e); else obSend(e);
    this.acted();
  }
  undoSend(e, p) {
    if (!obUnhold(e)) return;
    haptic();
    p.el.remove(); this.pending = this.pending.filter(x => x !== p);
    const u = e.undo, cur = this.ta.value;
    this.ta.value = u.typed + (cur.trim() ? '\n' + cur : '');
    for (const a of u.atts) if (!this.atts.some(x => x.path === a.path)) this.addAtt({ ...a, st: 'ok' });
    this.saveAtts();
    for (const k of u.mods) this.mods.add(k);
    this.saveMods(); this.renderMods();
    this.autosize(); ls.set('draft.' + this.dkey, this.ta.value || null);
    this.ta.focus(); this.ta.setSelectionRange(this.ta.value.length, this.ta.value.length);
  }
  // recipient guard: the send went to a chat you only just switched to
  flashRecipient() {
    const n = this.recipName(); if (!n) return;
    $('.rflash', this.el)?.remove();
    const f = h(`<div class="rflash">→ ${esc(n)}</div>`);
    $('.crow', this.el).append(f);
    setTimeout(() => f.remove(), 1800);
  }
  recipName() {
    if (this.target !== this.pane) return this.viaTitle || '';
    return this.home ? 'Master' : this.agent?.title || this.agent?.name || '';
  }
  updPh() {
    const n = this.recipName(), via = this.target !== this.pane;
    const ph = via ? (n ? 'Message via ' + n : 'Message') : this.working ? (n ? `Steer ${n} · queued for its next step` : 'Steer — queued for its next step') : n ? 'Message ' + n : 'Message';
    if (this.ta.placeholder !== ph) this.ta.placeholder = ph;
  }
  // "Next up": ghost peek while swiping the header, and a pill for 4 s after you act here
  peek(n) {
    const s = n && dispStatus(n);
    const el = h(`<div class="nxpeek${n ? colCls(n.color) : ''}">${n ? `<span class="nx-t"><small class="s-${s}">Next · ${LABEL[s].toLowerCase()}</small><b>${esc(nextName(n))}</b></span><span class="nx-a">›</span>` : '<span class="nx-t"><b class="dim">All clear</b></span>'}</div>`);
    $('.bar', this.el).append(el);
    return el;
  }
  acted() {
    if (this.ro) return;
    clearTimeout(this.nxT); $('.nextpill', this.el)?.remove();
    const n = nextUp(this.pane); if (!n) return;
    const s = dispStatus(n);
    const b = h(`<button class="nextpill${colCls(n.color)}"><span>Next:</span> <b>${esc(nextName(n))}</b> · ${LABEL[s].toLowerCase()} ›</button>`);
    b.onclick = () => { haptic(); b.remove(); location.hash = agentHref(n); };
    $('.composer', this.el).prepend(b);
    this.nxT = setTimeout(() => { b.classList.add('out'); setTimeout(() => b.remove(), 250); }, 4000);
  }
  // ----- prompt modifiers -----
  setMod(k, on) {
    on ? this.mods.add(k) : this.mods.delete(k);
    if (!this.mods.size) this.modPin = false;
    this.saveMods(); this.renderMods();
  }
  saveMods() { ls.set('pmods.' + this.dkey, this.modPin && this.mods.size ? [...this.mods] : null); }
  renderMods() {
    const n = this.mods.size, box = $('.mtags', this.el);
    if (n) this.sendBtn.dataset.n = n; else delete this.sendBtn.dataset.n;
    box.hidden = !n;
    box.innerHTML = n ? (this.modPin ? `<span class="mpin" aria-label="Pinned for this agent">${I.pin}</span>` : '')
      + PMODS.filter(p => this.mods.has(p.k)).map(p => `<button class="mtag" data-m="${p.k}" aria-label="Remove ${esc(p.l)}">${esc(p.l)}<i>×</i></button>`).join('') : '';
  }
  modTray() {
    const has = !!(this.ta.value.trim() || this.atts.some(a => a.st === 'ok'));
    const ov = h(`<div class="pop-ov"><div class="popm mtray">
      <div class="mt-chips">${PMODS.map(p => `<button class="mchip${this.mods.has(p.k) ? ' on' : ''}" data-m="${p.k}" aria-pressed="${this.mods.has(p.k)}">${esc(p.l)}</button>`).join('')}</div>
      <button class="mt-pin${this.modPin ? ' on' : ''}" aria-pressed="${this.modPin}">${I.pin}<span>Keep for this agent</span></button>
      ${this.working && has ? '<button class="mt-act" data-a="int">Interrupt &amp; send now</button><button class="mt-act" data-a="q">Queue (steer)</button>' : has ? '<button class="mt-act" data-a="send">Send</button>' : ''}
    </div></div>`);
    const m = $('.popm', ov);
    const upd = () => {
      m.querySelectorAll('.mchip').forEach(b => { const on = this.mods.has(b.dataset.m); b.classList.toggle('on', on); b.setAttribute('aria-pressed', on); });
      const pb = $('.mt-pin', m); pb.classList.toggle('on', this.modPin); pb.setAttribute('aria-pressed', this.modPin);
    };
    ov.onclick = e => {
      const c = e.target.closest('.mchip');
      if (c) { haptic(); this.setMod(c.dataset.m, !this.mods.has(c.dataset.m)); upd(); return; }
      if (e.target.closest('.mt-pin')) {
        haptic();
        if (!this.mods.size) { toast('Pick a modifier to keep', '', null, 1600); return; }
        this.modPin = !this.modPin; this.saveMods(); this.renderMods(); upd();
        return;
      }
      const a = e.target.closest('.mt-act')?.dataset.a;
      if (e.target.closest('.popm') && !a) return;
      ov.remove(); delete this.sendBtn.dataset.lp; // the long-press guard must not eat the next real tap
      if (a === 'int') this.send(null, 'interrupt'); else if (a) this.send(null, a === 'q' ? 'queue' : 'send');
    };
    document.body.append(ov);
    placeAbove(m, this.sendBtn);
  }
  // pending bubble bound to an outbox entry; resolves on success / HTTP error / expiry
  trackSend(e) {
    const [html, imgs] = textx(e.raw);
    const held = e.undo && e.hold > Date.now();
    const el = h(`<div class="msg user pending"><div class="bub">${html}</div>${thumbs(imgs)}<div class="pst">${held ? `<span class="uring" style="animation-duration:${e.hold - Date.now()}ms"></span>Sending · <button class="undo">Undo</button>` : e.queued ? 'Waiting for connection…' : 'Sending…'}</div></div>`);
    const p = { text: norm(e.raw), raw: e.raw, el, steer: e.steer };
    if (held) $('.undo', el).onclick = ev => { ev.stopPropagation(); this.undoSend(e, p); };
    if (!e.via) this.pending.push(p);
    this.pend.append(el);
    OBW.set(e.nonce, (st, err, r) => {
      if (!el.isConnected) return;
      const pst = $('.pst', el);
      if (st === 'go') { pst.textContent = 'Sending…'; return; }
      if (st === 'ok') { pst.textContent = e.via ? 'Sent via ' + e.via : r?.how === 'held' ? 'Queued · × above takes it back' : r?.via === 'inbox' ? 'Steered · read at its next step' : e.steer ? 'Queued' : 'Sent'; return; }
      if (st === 'wait') { pst.textContent = 'Waiting for connection…'; return; }
      el.classList.add('failed');
      pst.innerHTML = (st === 'expired' ? 'Not sent' : err?.status === 409 ? 'Agent is waiting for an answer.' : 'Not sent: ' + esc(err?.message || '')) + ' · <b>tap to edit</b>';
      el.onclick = () => {
        el.remove(); this.pending = this.pending.filter(x => x !== p);
        this.ta.value = e.raw + (this.ta.value ? '\n' + this.ta.value : ''); this.autosize(); this.ta.focus();
      };
      if (err?.status === 409) this.setBlocked(true);
    });
  }
  reconcile(it) {
    const t = norm(it.text);
    const k = this.pending.findIndex(p => p.text === t || (t && (t.includes(p.text) || p.text.includes(t))));
    if (k >= 0) { this.pending[k].el.remove(); this.pending.splice(k, 1); }
    const q = this.stopQ && Date.now() - this.stopQ.at < 30000 && this.stopQ.raw.find(r => norm(r) === t);
    if (q && this.ta.value.includes(q)) {
      this.ta.value = this.ta.value.replace(q, '').replace(/\n{2,}/g, '\n').trim();
      this.autosize(); ls.set('draft.' + this.dkey, this.ta.value || null);
      toast('Claude already got: ' + esc(q.slice(0, 60)), '', null, 2600);
    }
  }

  // ----- blocked / screen -----
  setBlocked(on) {
    const b = $('.blocked', this.el);
    if (b.hidden === !on) return;
    b.hidden = !on;
    this.el.classList.toggle('is-blocked', on);
    if (on) this.pollBlocked(); else clearTimeout(this.blT);
    if (this.stick) requestAnimationFrame(() => this.toBottom());
  }
  async pollBlocked(soon) {
    clearTimeout(this.blT);
    if (this.dead || $('.blocked', this.el).hidden || document.hidden || this.screen) return;
    if (soon) { this.blT = setTimeout(() => this.pollBlocked(), 150); return; }
    try {
      const r = await api(`a/${enc(this.pane)}/screen?ansi=1`);
      if (r.revision == null || r.revision !== this.blRev) {
        this.blRev = r.revision;
        const lines = String(r.text || '').replace(/\r\n?/g, '\n').split('\n');
        while (lines.length && !stripAnsi(lines.at(-1)).trim()) lines.pop();
        const pre = $('.bl-scr pre', this.el);
        pre.innerHTML = ansi(lines.slice(-16).join('\n'));
        const box = pre.parentElement; box.scrollTop = box.scrollHeight;
        this.blList = r.options || [];
        this.renderOpts();
      }
    } catch { }
    if (!this.dead) this.blT = setTimeout(() => this.pollBlocked(), 600);
  }
  // big option buttons; with an AskUserQuestion hint they become a question card (F3)
  renderOpts() {
    const opts = $('.bl-opts', this.el), q = this.agent?.ask?.questions?.[0];
    // ask_phone (modbridge): the agent waits on a question asked on the phone — options + free-text reply, no terminal
    // talk.js: a permission dialog the phone can answer (the terminal shows it too)
    const perm = this.agent?.talk?.perm;
    if (perm) {
      if (!MODS.talk) { lazy('talk').then(() => { this.blOpts = null; this.renderOpts(); }); return; }
      if ('k' + perm.id !== this.blOpts) { this.blOpts = 'k' + perm.id; MODS.talk.renderPerm(this, opts, perm); }
      $('.bl-scr', this.el).hidden = true;
      return;
    }
    const phone = this.agent?.ask?.phone;
    $('.blocked', this.el).classList.toggle('phone', !!phone);
    if (phone) {
      if (!MODS.mb) { lazy('mb').then(() => { this.blOpts = null; this.renderOpts(); }); return; }
      const psig = 'p' + JSON.stringify(this.agent.ask);
      if (psig !== this.blOpts) { this.blOpts = psig; MODS.mb.renderAsk(this, opts, this.agent.ask); }
      return;
    }
    // talk.js: an AskUserQuestion answered here in full (every question, multiSelect, free text)
    if (q && !this.agent?.plan) {
      if (!MODS.talk) { lazy('talk').then(() => { this.blOpts = null; this.renderOpts(); }); return; }
      const asig = 'a' + JSON.stringify(this.agent.ask);
      if (asig !== this.blOpts) { this.blOpts = asig; MODS.talk.renderAsk(this, opts, this.agent.ask); }
      $('.bl-scr', this.el).hidden = true;
      return;
    }
    let list = this.blList || [];
    if (list.length < 2 && q?.options?.length) list = q.options.map((o, i) => ({ label: o.label, n: i + 1 }));
    const plan = this.agent?.plan;
    const sig = JSON.stringify([list, q?.question, plan?.ts, plan?.text?.length]);
    if (sig === this.blOpts) return;
    // v3: an ExitPlanMode approval renders as a plan card
    if (plan && !MODS.v3) { lazy('v3').then(() => { this.blOpts = null; this.renderOpts(); }); return; }
    this.blOpts = sig;
    if (plan && MODS.v3.renderPlan(this, opts, plan, list)) { $('.bl-scr', this.el).hidden = true; return; }
    opts.hidden = list.length < 2;
    $('.bl-scr', this.el).hidden = !!q && list.length >= 2;
    opts.innerHTML = (q ? `<div class="bl-q">${q.header ? `<div class="bl-qh">${esc(q.header)}</div>` : ''}<div class="bl-qt">${esc(q.question || '')}</div></div>` : '') +
      list.map((o, i) => {
        const d = q?.options?.[i]?.description;
        return `<button class="opt${o.selected ? ' sel' : ''}" data-o="${i}">${o.n != null ? `<span class="on">${o.n}</span>` : ''}<span class="ol">${esc(o.label)}${d ? `<small>${esc(d)}</small>` : ''}</span></button>`;
      }).join('');
  }
  toggleScreen(force) {
    const on = force ?? !this.screen;
    if (!!this.screen === on) return;
    const box = $('.screenbox', this.el);
    this.el.classList.toggle('scr-on', on);
    $('.scr', this.el).innerHTML = on ? I.chat : I.term;
    ls.set('scr.' + this.pane, on || null);
    if (on) { this.screen = new Screen(this.pane); box.replaceChildren(this.screen.el); box.hidden = false; this.screen.start(); clearTimeout(this.blT); }
    else { this.screen.stop(); this.screen = null; box.hidden = true; box.textContent = ''; this.pollBlocked(); requestAnimationFrame(() => this.toBottom()); }
  }
  subsSheet() {
    const render = () => this.subs.length ? this.subs.slice().sort((a, b) => (b.running - a.running) || (b.updated - a.updated)).map(s =>
      `<a class="subrow${s.running ? ' run' : ''}" href="#/t/${enc(s.tid)}"><span class="st">${s.running ? '<span class="spin"></span>' : '<span class="ok"></span>'}</span>
        <span class="sb"><b>${esc(s.type || 'agent')}</b> ${esc(s.desc || '')}<small>${s.running ? 'running' : 'finished'} · <span data-since="${s.updated}" data-ago></span>${s.items ? ` · ${s.items} items` : ''}</small></span><span class="go">›</span></a>`).join('')
      : '<p class="dim">No subagents.</p>';
    const sh = sheet(`<h2>Subagents</h2><div class="subl">${render()}</div>`);
    tick(sh);
    sh.addEventListener('click', e => { if (e.target.closest('a')) sh.close(); });
  }
}

async function shrink(f) {
  if (!/^image\/(jpeg|png|heic|heif|webp)$/.test(f.type) || !window.createImageBitmap) return null;
  const bm = await createImageBitmap(f);
  const k = Math.min(1, 2048 / Math.max(bm.width, bm.height));
  if (k === 1 && (f.size < 1.5e6 || (f.type === 'image/png' && f.size < 4e6))) return null;
  const c = document.createElement('canvas');
  c.width = Math.round(bm.width * k); c.height = Math.round(bm.height * k);
  c.getContext('2d').drawImage(bm, 0, 0, c.width, c.height);
  return new Promise(res => c.toBlob(res, 'image/jpeg', 0.86));
}

// ---------- Screen page (#/s/pane) ----------
class ScreenPage {
  constructor(pane) {
    this.pane = pane;
    this.el = h(`<div class="view scrpage">
      <header class="bar"><div class="bar-in">
        <button class="ib back" aria-label="Back">${I.back}</button>
        <div class="ttl"><div class="t1"></div><div class="t2"></div></div>
        <button class="ib tochat" aria-label="Chat" hidden>${I.chat}</button>
      </div></header><div class="screenbox"></div></div>`);
    this.screen = new Screen(pane);
    $('.screenbox', this.el).append(this.screen.el);
    $('.back', this.el).onclick = () => goBack('#/agents');
    $('.tochat', this.el).onclick = () => { ls.set('scr.' + pane, null); location.hash = '#/a/' + enc(pane); };
  }
  mount() { this.onState(G.st); this.screen.start(); }
  destroy() { this.screen.stop(); }
  onVisible() { this.screen.start(); }
  onHidden() { this.screen.stop(); }
  onState(st) {
    const a = st?.agents.find(x => x.pane === this.pane), s = st?.shells?.find(x => x.pane === this.pane), x = a || s;
    $('.t1', this.el).textContent = x?.title || this.pane;
    $('.t2', this.el).textContent = shortPath(x?.cwd) || '';
    $('.tochat', this.el).hidden = !a;
  }
}

// ---------- One-tap new agent (API.md "One-tap new agent") ----------
// Tap: start Claude (Opus 5.5, auto mode) in ~/Work right away and open its chat with the composer focused; anything
// typed before Claude is ready waits on the server. Hold: the full sheet (folder, worktree, label, first prompt).
// The keyboard: iOS only raises it for a focus() inside the tap, so a hidden proxy field takes focus now and hands it
// to the chat's composer once that mounts.
function wirePlus(btn) {
  if (!btn || btn.dataset.qn) return;
  btn.dataset.qn = 1;
  btn.onclick = quickNew;
  longPress(btn, null, () => { newAgentSheet(); });
}
async function quickNew() {
  if (G.quickBusy || !L.home) return L.home ? undefined : newAgentSheet();
  G.quickBusy = true; haptic();
  let kb = $('#kbproxy');
  if (!kb) { kb = h('<textarea id="kbproxy" aria-hidden="true" tabindex="-1"></textarea>'); document.body.append(kb); }
  kb.focus();
  const t0 = performance.now();
  try {
    const r = await post('new', { cwd: ls.get('newCwd', null) || L.home + '/Work' });
    G.starting.set(r.pane, Date.now());
    location.hash = '#/a/' + enc(r.pane);
    for (let i = 0; i < 60 && G.view?.pane !== r.pane; i++) await new Promise(f => requestAnimationFrame(f));
    G.view?.ta?.focus();
    G.quickMs = Math.round(performance.now() - t0);
  } catch (e) { toast('Couldn’t start an agent: ' + esc(e.message), 'err'); kb.blur(); }
  finally { G.quickBusy = false; }
}
// Haiku just named an agent (API.md "Auto-name"): its title and color dot fade in where it is shown.
const autoNamed = new Set();
function autoNameFx(st) {
  for (const a of st.agents) {
    if (!a.autoNamed || autoNamed.has(a.pane + a.autoNamed) || Date.now() + G.skew - a.autoNamed > 15e3) continue;
    autoNamed.add(a.pane + a.autoNamed);
    requestAnimationFrame(() => {
      const els = [...document.querySelectorAll(`[data-pane="${CSS.escape(a.pane)}"]`)];
      if (G.view?.pane === a.pane) els.push(...G.view.el.querySelectorAll('.ttl .t1w'));
      for (const e of els) { e.classList.remove('an-fx'); void e.offsetWidth; e.classList.add('an-fx'); setTimeout(() => e.classList.remove('an-fx'), 1800); }
    });
  }
}

// ---------- New agent sheet ----------
async function newAgentSheet(prefill) {
  if (prefill instanceof Event) prefill = null;
  const ws = G.st?.workspaces || [];
  const sh = sheet(`<h2>New agent</h2>
    <input type="search" class="dsearch" placeholder="Search folders or type a path" autocapitalize="off" autocorrect="off" spellcheck="false">
    <div class="dirs"><div class="dim pad"><span class="spin"></span> Loading folders…</div></div>
    <div class="form">
      <label><span>Label</span><input class="label" placeholder="optional tab name" autocapitalize="off"></label>
      <label><span>Workspace</span><select class="ws"><option value="">Auto</option>${ws.map(w => `<option value="${esc(w.ws)}">${esc(w.label || w.ws)}</option>`).join('')}<option value="new">New workspace</option></select></label>
      <div class="pwrap"><textarea class="prompt" rows="3" placeholder="First prompt (optional)"></textarea><button class="key sm bolt" aria-label="Snippets">${I.bolt}</button></div>
      <label class="wtl" hidden><span>Worktree</span><input type="checkbox" switch class="wt"><small class="dim">new git branch + worktree</small></label>
      <button class="btn primary start" disabled>Start</button>
    </div>`, 'tall');
  const q = s => $(s, sh), list = q('.dirs'), search = q('.dsearch'), start = q('.start');
  if (prefill) q('.prompt').value = prefill;
  q('.pwrap .bolt').onclick = () => snippetSheet(q('.prompt'));
  let dirs = [], sel = null;
  const draw = () => {
    const f = search.value.trim().toLowerCase();
    let ds = dirs.filter(d => !f || d.path.toLowerCase().includes(f) || d.name.toLowerCase().includes(f));
    let html = '';
    if (/^[~/]/.test(f) && !dirs.some(d => d.path === f)) html += `<button class="dir" data-p="${esc(search.value.trim())}"><b>Use ${esc(search.value.trim())}</b></button>`;
    html += ds.slice(0, 80).map(d => `<button class="dir${d.path === sel ? ' sel' : ''}" data-p="${esc(d.path)}"><b>${esc(d.name)}</b>${d.git ? '<i class="git">git</i>' : ''}<small>${esc(shortPath(d.path))}</small>${d.recent ? `<em data-since="${d.recent}"></em>` : ''}</button>`).join('');
    list.innerHTML = html || '<div class="dim pad">No matches</div>';
    tick(list);
  };
  search.oninput = draw;
  list.onclick = e => {
    const b = e.target.closest('.dir'); if (!b) return;
    sel = b.dataset.p.replace(/^~(?=\/|$)/, L.home); start.disabled = false;
    start.textContent = 'Start in ' + shortPath(sel);
    const w = ws.filter(w => sel.startsWith(w.cwd)).sort((a, b) => b.cwd.length - a.cwd.length)[0];
    if (w && !q('.ws').value) q('.ws').value = w.ws;
    const dd = dirs.find(d => d.path === sel); q('.wtl').hidden = dd ? !dd.git : false; if (q('.wtl').hidden) q('.wt').checked = false;
    draw();
  };
  start.onclick = async () => {
    if (!sel) return;
    start.disabled = true; start.innerHTML = '<span class="spin"></span> Starting…';
    const body = { cwd: sel };
    const label = q('.label').value.trim(), prompt = q('.prompt').value.trim(), w = q('.ws').value;
    if (label) body.label = label; if (prompt) body.prompt = prompt; if (w) body.ws = w;
    try {
      const r = q('.wt').checked ? await lazy('v3').then(m => m.startWorktree(body)) : await post('new', body);
      G.starting.set(r.pane, Date.now());
      sh.close(); location.hash = '#/a/' + enc(r.pane);
    } catch (e) { start.disabled = false; start.textContent = 'Start'; toast('Failed: ' + esc(e.message), 'err'); }
  };
  try { const seen = new Set(); dirs = ((await api('dirs')).dirs || []).filter(d => !seen.has(d.path) && seen.add(d.path)); draw(); }
  catch (e) { list.innerHTML = `<div class="dim pad">Couldn’t load folders: ${esc(e.message)}</div>`; }
}

// ---------- Settings sheet ----------
const b64u = s => { const p = '='.repeat((4 - s.length % 4) % 4); const r = atob((s + p).replace(/-/g, '+').replace(/_/g, '/')); return Uint8Array.from(r, c => c.charCodeAt(0)); };
async function settingsSheet() {
  const st = G.st || {};
  const standalone = navigator.standalone || matchMedia('(display-mode: standalone)').matches;
  const pushOK = 'serviceWorker' in navigator && 'PushManager' in window && window.isSecureContext;
  const sh = sheet(`<h2>Settings</h2>
    <div class="group">
      <div class="kv"><span>Host</span><b>${esc(st.host || '?')}</b></div>
      <div class="kv"><span>Tailscale</span><b>${esc(st.tsName || '?')}</b></div>
      <div class="kv"><span>Connection</span><b><span class="conn" data-conn="${G.conn}"></span> <span class="cs"></span></b></div>
      <div class="kv"><span>herdr</span><b>${st.herdr === false ? 'unreachable' : st.herdr ? 'ok' : '?'}</b></div>
      <div class="kv"><span>Agents / shells</span><b>${st.agents?.length ?? 0} / ${st.shells?.length ?? 0}</b></div>
    </div>
    <h3>Notifications</h3>
    <div class="group">
      <div class="kv"><span>Permission</span><b class="perm">${window.Notification ? Notification.permission : 'unsupported'}</b></div>
      <div class="kv"><span>Installed</span><b>${standalone ? 'yes' : 'no'}</b></div>
      <div class="kv"><span>Subscription</span><b class="subst">…</b></div>
    </div>
    <p class="dim small">iOS only delivers Web Push to apps added to the Home Screen and served over HTTPS. Open <b>${esc(G.st?.tsName ? 'https://' + G.st.tsName : location.origin.replace(/^http:/, 'https:'))}</b> in Safari, Share → Add to Home Screen, then enable notifications from the installed app.</p>
    <div class="btns"><button class="btn primary en"${pushOK ? '' : ' disabled'}>Enable notifications</button><button class="btn test">Send test push</button></div>
    ${G.prefs?.resAlerts ? '<div class="group resal-g"><button class="kv resal" style="width:100%"><span>Resource alerts</span><b>' + (G.prefs.resAlerts.on ? 'on' : 'off') + ' ›</b></button></div>' : ''}
    <h3>App</h3>
    <div class="group"><label class="kv"><span>Haptics</span><input type="checkbox" class="hapt" switch${ls.get('haptics', true) ? ' checked' : ''}></label>
      <label class="kv"><span>Fill to screen bottom <small class="dim">test: removes the band under the composer</small></span><input type="checkbox" class="fillgap" switch${ls.get('fillgap', false) ? ' checked' : ''}></label></div>
    <div class="v3set"></div>
    <div class="usageset"></div>
    <div class="btns"><button class="btn seen">Mark all as seen</button><button class="btn reload">Reload app</button></div>
    <p class="dim small">sheprd ${VERSION} · ${esc(location.origin)}</p>`);
  const q = s => $(s, sh);
  q('.cs').textContent = { on: 'live', retry: 'reconnecting', off: 'offline' }[G.conn];
  const subst = q('.subst');
  const reg = pushOK ? await navigator.serviceWorker.getRegistration() : null;
  const cur = await reg?.pushManager.getSubscription().catch(() => null);
  subst.textContent = !pushOK ? (window.isSecureContext ? 'push unsupported' : 'needs https') : cur ? 'active' : 'none';
  q('.en').onclick = async () => {
    try {
      const p = await Notification.requestPermission();
      q('.perm').textContent = p;
      if (p !== 'granted') throw new Error('permission ' + p);
      const r = reg || await navigator.serviceWorker.ready;
      const { key } = await api('push/key');
      let sub = await r.pushManager.getSubscription();
      if (!sub) sub = await r.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: b64u(key) });
      await post('push/sub', sub.toJSON());
      subst.textContent = 'active'; toast('Notifications enabled', 'ok');
    } catch (e) { toast('Could not enable: ' + esc(e.message), 'err', null, 5000); }
  };
  q('.test').onclick = () => post('push/test').then(() => toast('Test push sent', 'ok')).catch(e => toast(esc(e.message), 'err'));
  q('.seen').onclick = () => { (G.st?.agents || []).forEach(markSeen); badge(); G.view?.onState?.(G.st); toast('All marked as seen'); };
  q('.reload').onclick = () => location.reload();
  lazy('v3').then(m => m.settingsPart(q('.v3set'))).catch(() => { });
  lazy('git').then(m => m.settingsPart(q('.usageset'))).catch(() => { });
  q('.resal')?.addEventListener('click', () => lazy('res').then(m => m.alertsSheet()));
  q('.hapt').onchange = e => { ls.set('haptics', e.target.checked); if (e.target.checked) haptic(); };
  q('.fillgap').onchange = e => {
    ls.set('fillgap', e.target.checked); fitVV();
    const c = $('.chat:not([hidden]) .composer')?.getBoundingClientRect();
    post('clientlog', { t: 'fillgap', on: e.target.checked, h: innerHeight, vvh: vv?.height, scr: `${screen.width}x${screen.height}`, appH: $('#app')?.offsetHeight, comp: c ? `${c.top | 0}-${c.bottom | 0}` : null }).catch(() => {});
  };
}

// Fresh master session; the previous one keeps running as an ordinary agent.
async function newMasterChat() {
  if (!confirm('Start a new master chat?\n\nThe current one keeps running as a normal agent in the Workspace.')) return;
  try {
    const r = await post('home/new');
    G.homePending = r.pane; G.starting.set(r.pane, Date.now());
    G.route = null; location.hash = '#/'; route();
  } catch (e) { toast('Failed: ' + esc(e.message), 'err'); }
}

// ---------- Home (no home agent yet) ----------
class HomeEmpty {
  constructor() {
    this.pane = null;
    this.el = h(`<div class="view homeempty">
      <header class="bar"><div class="bar-in">
        <div class="brand"><span class="conn" data-conn="${G.conn}"></span><span class="t1">Master</span></div>
        <span class="sp"></span><div class="stripbox"></div>
      </div></header>
      <div class="scroll"><div class="he">
        <div class="he-ic">${I.chat}</div>
        <h2>No master agent</h2>
        <p class="dim">Master is a long-running Claude session that dispatches subagents and spawns tabs for you. Optional: every agent you run in herdr is under Agents either way.</p>
        <button class="btn primary start" hidden>Start master</button>
        <a class="btn" href="#/agents" style="margin-top:10px">See your agents</a>
        <p class="dim small st"></p>
      </div></div></div>`);
  }
  mount() {
    $('.stripbox', this.el).onclick = stripClick;
    const b = $('.start', this.el);
    b.onclick = async () => {
      b.disabled = true; b.innerHTML = '<span class="spin"></span> Starting…';
      try {
        const r = await post('home/ensure');
        G.homePending = r.pane; G.starting.set(r.pane, Date.now());
        G.route = null; route();
      } catch (e) { b.disabled = false; b.textContent = 'Start master'; toast('Failed: ' + esc(e.message), 'err'); }
    };
    this.onState(G.st);
  }
  onState(st) {
    if (st?.home || G.homePending) { G.route = null; route(); return; }
    stripFit($('.stripbox', this.el), st ? strip(st) : '');
    $('.start', this.el).hidden = !st;
    $('.st', this.el).textContent = st ? '' : 'Connecting…';
  }
}

// ---------- Doc viewer (#/f?path=) ----------
const TEXT_EXT = /\.(md|markdown|mdx|txt|log|json|jsonl|ya?ml|toml|ini|conf|cfg|csv|tsv|sh|bash|zsh|py|js|mjs|cjs|ts|tsx|jsx|rs|go|c|h|cc|cpp|hpp|java|kt|rb|lua|sql|html?|css|xml|env|diff|patch|lock)$/i;
class DocView {
  constructor(path) {
    this.path = path; this.pane = null;
    const name = path.split('/').pop() || path, dir = path.slice(0, path.length - name.length - 1) || '/';
    this.dir = dir;
    this.el = h(`<div class="view doc">
      <header class="bar"><div class="bar-in">
        <button class="ib back" aria-label="Back">${I.back}</button>
        <div class="ttl"><div class="t1">${esc(name)}</div><div class="t2">${esc(shortPath(dir))}</div></div>
        <a class="ib" href="${esc(fileUrl(path))}" target="_blank" rel="noopener" aria-label="Open raw">${I.ext}</a>
      </div></header>
      <div class="scroll docb"><div class="stline"><span class="spin"></span> Loading…</div></div></div>`);
  }
  async mount() {
    $('.back', this.el).onclick = () => goBack('#/');
    const body = $('.docb', this.el), p = this.path;
    try {
      const r = await fetch(fileUrl(p));
      const ct = (r.headers.get('content-type') || '').toLowerCase();
      if (!r.ok) { let m = 'HTTP ' + r.status; try { m = (await r.json()).error || m; } catch { } throw new Error(m); }
      if (ct.startsWith('image/')) { body.innerHTML = `<div class="docimg"><img class="thumb" src="${esc(fileUrl(p))}" alt=""></div>`; return; }
      if (ct.includes('json') && !/\.jsonl?$/i.test(p)) {
        const j = await r.json();
        if (Array.isArray(j?.entries)) return this.dir_(body, j.entries);
        body.innerHTML = `<pre class="doctext">${linkify(JSON.stringify(j, null, 2))}</pre>`; return;
      }
      const buf = await r.arrayBuffer();
      const head = new Uint8Array(buf, 0, Math.min(buf.byteLength, 4096));
      if (!ct.startsWith('text/') && !TEXT_EXT.test(p) && !ct.includes('json') && head.includes(0)) {
        body.innerHTML = `<div class="stline">No preview for ${esc(ct || 'this file')} (${(buf.byteLength / 1024).toFixed(0)} KB).<br><br><a class="btn" href="${esc(fileUrl(p))}" target="_blank" rel="noopener">Open raw</a></div>`; return;
      }
      let text = new TextDecoder().decode(buf.byteLength > 2e6 ? buf.slice(0, 2e6) : buf);
      if (/\.(md|markdown|mdx)$/i.test(p)) {
        L.base = this.dir;
        try { body.innerHTML = `<div class="md docmd">${mdx(text)}</div>`; } finally { L.base = null; }
      } else body.innerHTML = `<pre class="doctext">${linkify(text)}</pre>`;
      if (buf.byteLength > 2e6) body.insertAdjacentHTML('beforeend', '<div class="stline">Truncated at 2 MB.</div>');
    } catch (e) { body.innerHTML = `<div class="stline">Couldn’t open: ${esc(e.message)}</div>`; }
  }
  dir_(body, entries) {
    const items = entries.map(e => typeof e === 'string' ? { name: e } : { name: e.name ?? e.path ?? String(e), dir: e.dir ?? e.isDir ?? e.is_dir ?? (e.type === 'dir' || e.kind === 'dir'), size: e.size })
      .sort((a, b) => (b.dir ? 1 : 0) - (a.dir ? 1 : 0) || a.name.localeCompare(b.name));
    const base = this.path.replace(/\/$/, '');
    body.innerHTML = `<div class="dirl">${items.map(e => {
      const full = e.name.startsWith('/') ? e.name : base + '/' + e.name, label = e.name.split('/').pop() + (e.dir ? '/' : '');
      return e.dir ? `<a class="path" href="#/f?path=${enc(full)}">${esc(label)}</a>` : pathLink(full, label);
    }).join('')}</div>`;
  }
}

// ---------- router ----------
const cache = new Map(); // warm chat views, LRU (max 4)
// tab screens (Agents · History · Automations · Machine) are siblings: switching tabs doesn't slide
const TAB_KEYS = k => k === 'agents' || k === 'search' || k === 'r' || k === 'r/disk' || k.startsWith('fleet/');
const depth = k => k === '/' || k.startsWith('home') ? 0 : TAB_KEYS(k) ? 1 : k.startsWith('r/disk?') ? 3 : k[0] === 't' || k[0] === 'f' || k[0] === 'd' ? 3 : 2;
// lazily loaded feature modules (search, files, sheets): import once, then re-route
const MODS = {};
const lazy = n => MODS[n] ? Promise.resolve(MODS[n]) : import(`./${n}.js`).then(m => (MODS[n] = m));
function route() {
  const raw = location.hash.slice(1) || '/';
  // item routes: #/?i=N, #/a/<pane>?i=N, #/t/<tid>?i=N
  let hs = raw, wantI = null, rfocus = null;
  const mq = raw.match(/^(\/|\/[at]\/[^?]+)\?(.*)$/);
  if (mq) { hs = mq[1]; const v = new URLSearchParams(mq[2]).get('i'); if (v != null && /^\d+$/.test(v)) wantI = +v; }
  // the master's own pane is the Home view (push links name the pane)
  const hp = hs.match(/^\/a\/(.+)$/), redir = !!hp && !!G.st?.home && decodeURIComponent(hp[1]) === G.st.home;
  if (redir) hs = '/';
  const focus = () => {
    if (mq || redir) history.replaceState(history.state, '', '#' + hs);
    if (wantI != null) G.view?.focusItem?.(wantI);
    rfocus?.();
  };
  const m = hs.match(/^\/(a|t|s)\/(.+)$/);
  let key, make;
  if (m) {
    const arg = decodeURIComponent(m[2]);
    key = m[1] + '/' + arg;
    make = () => m[1] === 'a' ? new ChatView({ pane: arg }) : m[1] === 't' ? new ChatView({ tid: arg }) : new ScreenPage(arg);
  } else if (hs === '/agents') { key = 'agents'; make = getAgentsView; }
  else if (/^\/r(\/disk)?(\?|$)/.test(hs)) { // phase 2 Machine view (res.js)
    if (!MODS.res) { lazy('res').then(() => { G.route = null; route(); }, e => toast('Load failed: ' + esc(e.message), 'err')); return; }
    ({ key, make, focus: rfocus } = MODS.res.route(hs));
  }
  else if (/^\/fleet(\/|$)/.test(hs)) { // fleet.js: sessions, schedules, cloud routines, screen
    if (!MODS.fleet) { lazy('fleet').then(() => { G.route = null; route(); }, e => toast('Load failed: ' + esc(e.message), 'err')); return; }
    ({ key, make } = MODS.fleet.route(hs));
  }
  else if (hs.startsWith('/f?')) { const p = new URLSearchParams(hs.slice(3)).get('path') || '/'; key = 'f' + p; make = () => new DocView(p); }
  else if (/^\/gd?\//.test(hs)) { // branch vs main, git diffs (git.js)
    if (!MODS.git) { lazy('git').then(() => { G.route = null; route(); }, e => toast('Load failed: ' + esc(e.message), 'err')); return; }
    ({ key, make, focus: rfocus } = MODS.git.route(hs));
  }
  else if (hs === '/search' || hs.startsWith('/d/')) {
    const mod = hs === '/search' ? 'search' : 'files';
    if (!MODS[mod]) { lazy(mod).then(() => { G.route = null; route(); }, e => toast('Load failed: ' + esc(e.message), 'err')); return; }
    if (mod === 'search') { key = 'search'; make = () => (G.sv ||= new MODS.search.SearchView()); }
    else {
      const dm = hs.match(/^\/d\/([^?]+)\?(.*)$/), tid = decodeURIComponent(dm?.[1] || ''), p = new URLSearchParams(dm?.[2] || '').get('path') || '';
      key = 'd' + tid + '\n' + p; make = () => new MODS.files.DiffView(tid, p);
    }
  }
  else {
    const home = G.homePending || G.st?.home;
    key = home ? 'home/' + home : 'home-empty';
    make = () => home ? new ChatView({ pane: home, home: true }) : new HomeEmpty();
  }
  if (G.route === key) { focus(); return; }
  const fromDepth = G.route == null ? null : depth(G.route);
  G.route = key; G.navs = (G.navs || 0) + 1;
  closeSheets();
  const prev = G.view;
  if (prev) {
    prev.el.style.transform = ''; prev.el.classList.remove('in-r', 'in-l');
    if (prev.keep) { prev.suspend?.(); prev.el.hidden = true; }
    else { prev.destroy?.(); prev.el.remove(); }
  }
  let v = cache.get(key) || (key === 'agents' && agentsView) || (key === 'search' && G.sv);
  const warm = !!v && v.el.isConnected;
  if (!v) v = make();
  if (v instanceof ChatView) {
    cache.delete(key); cache.set(key, v);
    for (const [k, old] of cache) if (cache.size > 4 && old !== v && old !== prev) { old.destroy(); old.el.remove(); cache.delete(k); }
  }
  G.view = v;
  v.el.hidden = false;
  tabBar(key, v);
  if (fromDepth != null && fromDepth !== depth(key)) {
    v.el.classList.remove('in-r', 'in-l'); void v.el.offsetWidth;
    v.el.classList.add(depth(key) > fromDepth ? 'in-r' : 'in-l');
  }
  if (!v.el.isConnected) app.append(v.el);
  setConn(G.conn);
  if (warm) v.resume?.(); else v.mount?.();
  viewing();
  focus();
  resFollow();
}
// phase 2: resource pollers follow the visible view (res.js, lazy); gated so an idle phone costs nothing
function resFollow() {
  if (G.noRes) return;
  const v = G.view, want = v === agentsView ? 'ws' : v instanceof ChatView && v.pane ? (v.home ? 'home' : 'chat') : null;
  if (MODS.res) { MODS.res.follow(v, want); return; }
  if (!want) return;
  if (want !== 'ws') {
    const g = ls.get('res.glance', null), f = g && Date.now() - g.t < 6e5;
    if (!(want === 'home' ? G.st?.jobsRunning > 0 || (f && g._busy) : G.st?.agents.find(a => a.pane === v.pane)?.jobs?.running > 0 || (f && g._notable?.includes(v.pane)))) return;
  }
  lazy('res').then(resFollow, () => { });
}
// open search inside the tap's gesture so iOS raises the keyboard
function openSearch() { if (!MODS.search) { location.hash = '#/search'; return; } location.hash = '#/search'; route(); }
// navigate even when the hash is unchanged (re-focus an item)
function nav(hash) { if (location.hash === hash) { G.route = null; route(); } else location.hash = hash; }
// re-check every prev cache entry's element is in the DOM before eviction
function swipeBack(v) {
  if (v === agentsView) return '#/';
  if (v instanceof ChatView && v.home) return null;
  if (v instanceof HomeEmpty) return null;
  return 'back';
}
// horizontal swipes: home ←→ workspace; edge-swipe right = back elsewhere
(() => {
  let t = null;
  const skip = el => el.closest('pre, .tbl, .term-wrap, .keybar, .bl-scr, textarea, input, select, .atts, .thumbs, .chips, .strip, .qchips');
  app.addEventListener('touchstart', e => {
    t = null;
    if (e.touches.length !== 1 || document.querySelector('.sheet-ov, .lb') || skip(e.target)) return;
    const p = e.touches[0], v = G.view;
    if (p.clientX >= 28 && e.target.closest('.cwrap')) return; // card swipes
    const home = (v instanceof ChatView && v.home) || v instanceof HomeEmpty;
    t = { x: p.clientX, y: p.clientY, v, home, edge: p.clientX < 28, lock: null, dx: 0, t0: Date.now() };
  }, { passive: true });
  app.addEventListener('touchmove', e => {
    if (!t) return;
    const p = e.touches[0], dx = p.clientX - t.x, dy = p.clientY - t.y;
    if (!t.lock) {
      if (Math.abs(dx) < 10 && Math.abs(dy) < 10) return;
      const ok = Math.abs(dx) > Math.abs(dy) * 1.4 && (t.home ? dx < 0 : dx > 0 && (t.v === agentsView || t.edge));
      if (!ok) { t = null; return; }
      t.lock = 'h'; t.v.el.style.transition = 'none';
    }
    e.preventDefault();
    t.dx = t.home ? Math.min(0, dx) : Math.max(0, dx);
    t.v.el.style.transform = `translate3d(${t.dx}px,0,0)`;
  }, { passive: false });
  const end = () => {
    if (!t || !t.lock) { t = null; return; }
    const { v, dx } = t, w = app.clientWidth, fast = Math.abs(dx) / (Date.now() - t.t0) > 0.5;
    const go = Math.abs(dx) > w * 0.3 || (fast && Math.abs(dx) > 40);
    v.el.style.transition = 'transform .18s ease-out';
    v.el.style.transform = go ? `translate3d(${dx < 0 ? -w : w}px,0,0)` : '';
    const home = t.home; t = null;
    setTimeout(() => {
      v.el.style.transition = '';
      if (!go) return;
      if (home) location.hash = '#/agents';
      else { const b = swipeBack(v); if (b === 'back') goBack(v instanceof ChatView && !v.home ? v.parentHash() : '#/agents'); else if (b) location.hash = b; }
    }, 170);
  };
  app.addEventListener('touchend', end); app.addEventListener('touchcancel', end);
})();
window.addEventListener('hashchange', route);

document.addEventListener('visibilitychange', () => {
  viewing();
  if (!document.hidden) {
    connect(); obFlush(); G.view?.onVisible?.();
    navigator.serviceWorker?.getRegistration().then(r => r?.update()).catch(() => { });
  } else G.view?.onHidden?.();
});
window.addEventListener('online', () => { connect(); obFlush(); });
window.addEventListener('pageshow', e => { if (e.persisted) connect(); });

// ---------- service worker ----------
if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('sw.js').catch(e => console.warn('sw', e));
  let had = !!navigator.serviceWorker.controller;
  navigator.serviceWorker.addEventListener('controllerchange', () => {
    if (had) toast('App updated — tap to reload', 'ok', () => location.reload(), 8000);
    had = true;
  });
  navigator.serviceWorker.addEventListener('message', e => { if (e.data?.nav) nav(e.data.nav.replace(/^\/?#?/, '#')); });
}

if (G.st) { L.ts = G.st.tsName; L.home = homeOf(G.st); }
route();
connect();
badge();
setTimeout(getPrefs, 1500);
setTimeout(() => (window.requestIdleCallback || setTimeout)(() => { lazy('search').catch(() => { }); lazy('sheets').catch(() => { }); lazy('v3').catch(() => { }); lazy('git').catch(() => { }); lazy('queue').catch(() => { }); lazy('home').catch(() => { }); }), 2500);

// shared with the lazily loaded feature modules (search.js, files.js, sheets.js)
export { morph, patchHTML, cardAct, copyText, G, I, $, h, esc, api, post, ls, sheet, toast, haptic, tick, rel, shortPath, agentHref, itemHref, nav, getPrefs, DEF_SNIPS, insertAt, longPress, linkify, md, fileUrl, debounce, closeSheets, goBack, cdot, dispStatus, markSeen, LABEL, setMeta, renameSheet, sendKeys, switcher, newAgentSheet, wirePlus, quickNew };
export { surfHTML, MODS, lazy, DONE_REP, repLabel }; // v3.js, queue.js
export { dismissSurfs }; // queue.js
export { takeMods, modSuffix, PMODS, asksYou }; // queue.js hero reply
export { settingsSheet, openSearch }; // res.js (Machine gear), fleet.js (History)
export { lightbox }; // fleet.js
export { popMenu }; // art.js
export { sortAgents }; // sheets.js
