// Find and continue agents (lazy; API.md "Find and continue").
// 1. Every agent has a short name and a one-line job (Haiku, server/src/find.rs). The Agents tab lists Awake and Asleep
//    agents (Continue on asleep ones) under a find field; search (#/search) matches name, job, folder and chats.
// 2. Home's Recent grid (2 × 3): pinned agents (≤ 5; the home agent by default) then the last ones you talked to,
//    awake or asleep; long-press = Pin to Home / Open chat / Rename and edit job line / Hide from Recent.
//    The Awake and Asleep lists below leave out whoever is already in Recent.
// 3. Agent-message cards name their sender ("Nightly Backup ›"): tap opens its chat even when it is asleep (closed pane);
//    an asleep agent's chat shows "Asleep 2h · <job>", a "Fell asleep at …" line and a wake card: Continue resumes it
//    in place (`claude --resume` in its old workspace and tab, then "continue"), Message… wakes it with your words.
import { G, I, $, h, esc, api, post, toast, haptic, tick, rel, shortPath, openSearch, sheet, longPress, agentHref, whoName, isMasterName, foldable, HOME_NAME } from './app.js';

(() => { const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'find.css'; document.head.append(l); })();

const enc = encodeURIComponent;
export const MOON = '<svg class="moon-i" viewBox="0 0 24 24"><path d="M20.5 14.6A8.5 8.5 0 0 1 9.4 3.5a8.5 8.5 0 1 0 11.1 11.1z"/></svg>';
export const CHEV = '<svg class="chev-i" viewBox="0 0 24 24"><path d="M9 6l6 6-6 6"/></svg>';
const COLORS = ['red', 'orange', 'yellow', 'green', 'cyan', 'blue', 'purple', 'pink'];
export const colCls = c => COLORS.includes(c) ? ' col-' + c : '';
const isHome = a => !!a?.home || isMasterName(a?.short || a?.name || a?.title); // the home agent (herdr name master*)
export const nameOf = a => isHome(a) ? HOME_NAME() : a?.short || a?.name || a?.title || 'Agent';
const initial = a => isHome(a) ? '' : (nameOf(a).replace(/[^\p{L}\p{N}]/gu, '')[0] || '?').toUpperCase();
const working = a => a && !a.asleep && (a.status === 'working');
/** The agent's disc: pastel fill + initial; asleep = faded with a moon, working = a blue arc. */
export const favHTML = (a, { sz } = {}) => `<span class="fav${isHome(a) ? ' xed' : ''}${a?.asleep ? ' zz' : ''}"${sz ? ` style="--sz:${sz}px"` : ''}>${initial(a)}`
  + `${working(a) ? '<i class="xwk"></i>' : ''}${a?.asleep ? `<i class="moon">${MOON}</i>` : ''}</span>`;
/** "at 6:05 AM" / "yesterday at 9:17 AM" / "on Oct 4 at 9:17 AM" */
const atClock = ms => {
  const d = new Date(ms), t = d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
  const days = Math.round((new Date().setHours(0, 0, 0, 0) - new Date(ms).setHours(0, 0, 0, 0)) / 864e5);
  return days === 0 ? 'at ' + t : days === 1 ? 'yesterday at ' + t : `on ${d.toLocaleDateString([], { month: 'short', day: 'numeric' })} at ${t}`;
};

// ---------- data: GET /api/agents (awake + asleep), /api/agents/resolve ----------
let LIST = null, AT = 0, LOADING = null;
const RES = new Map(); // resolve key → Promise<row|null>
/** Every agent, awake and asleep, with names, job lines and pins (cached 15 s). */
export function agents(force) {
  if (!force && LIST && Date.now() - AT < 15e3) return Promise.resolve(LIST);
  LOADING ??= api('agents').then(r => { LIST = r; AT = Date.now(); return r; }).finally(() => { LOADING = null; });
  return LOADING;
}
export const cached = () => LIST;
export const forget = () => { LIST = null; RES.clear(); };
export function resolve(q) {
  const k = JSON.stringify(q);
  if (!RES.has(k)) {
    const qs = Object.entries(q).filter(([, v]) => v).map(([k, v]) => `${k}=${enc(v)}`).join('&');
    RES.set(k, api('agents/resolve?' + qs).then(r => r.agent || null, () => null));
  }
  return RES.get(k);
}

/** Wake an asleep agent (or prompt an awake one) and open its chat. `text` = its first message ("continue"). */
export async function continueAgent(sid, { text = 'continue', btn, name } = {}) {
  haptic();
  const label = btn?.innerHTML;
  if (btn) { btn.disabled = true; btn.innerHTML = '<span class="spin"></span>'; }
  try {
    // wake: a session already running in some pane is only opened (server-side: never a second Claude, nothing sent)
    const r = await post(`sessions/${enc(sid)}/resume`, text ? { text, wake: true } : { wake: true });
    if (!r.live) G.starting.set(r.pane, Date.now());
    forget();
    toast(r.live ? `${esc(name || 'It')} is already awake` : `Waking ${esc(name || 'the agent')}…`, 'ok', null, 1600);
    const to = '#/a/' + enc(r.pane);
    // in place: the asleep chat is replaced by the live one (Back goes where it went before)
    if (G.view?.fixedTid === sid) location.replace(to); else location.hash = to;
  } catch (e) {
    toast(`Couldn’t wake it: ${esc(e.message)}`, 'err');
    if (btn) { btn.disabled = false; btn.innerHTML = label; }
  }
}

// ---------- 3a: the sender of an agent-message card ----------
/** Status line of a sender: "Working", "Asleep 2h", "Idle"… */
const stHTML = a => a.asleep ? `<span class="zzt">${MOON}Asleep <span data-since="${a.ts}"></span></span>`
  : a.status === 'working' ? '<span class="wkt">Working</span>' : a.status === 'blocked' ? '<span class="bkt">Needs you</span>'
    : a.status === 'done' ? '<span class="dnt">Done</span>' : 'Idle';
/** The card header for a sender row (awake or asleep). `what` = "sent this to Master". */
export function senderHead(a, what, pane, from) {
  const href = a.asleep ? '#/t/' + enc(a.sid) : '#/a/' + enc(a.pane || pane);
  return `<a class="am-h fx" href="${href}" data-sp="${esc(pane || '')}" data-sn="${esc(from || '')}">${favHTML(a, { sz: 30 }).replace('class="fav', 'class="am-av fav')}`
    + `<span class="am-w"><span class="snd">${esc(nameOf(a))}${CHEV}</span><small>${stHTML(a)} · ${esc(what)}</small></span>`
    + (a.asleep ? `<span class="opn" data-cont="${esc(a.sid)}" data-nm="${esc(nameOf(a))}" role="button">Continue${CHEV}</span>` : `<span class="opn">Open${CHEV}</span>`) + '</a>';
}
/** Cards whose sender has no live pane: look it up (closed pane or name), then draw its header. */
export function fixSenders(list) {
  if (!list._fx) {
    list._fx = 1;
    list.addEventListener('click', e => {
      const c = e.target.closest('.am-h [data-cont]');
      if (c) { e.preventDefault(); e.stopPropagation(); continueAgent(c.dataset.cont, { btn: c, name: c.dataset.nm }); return; }
      const u = e.target.closest('.am-h[data-unres]');
      if (u) { e.preventDefault(); e.stopPropagation(); toast('That agent is gone: no chat to open', 'err'); }
    }, true);
  }
  for (const hd of list.querySelectorAll('.am-h[data-unres]:not([data-asked])')) {
    hd.dataset.asked = 1;
    resolve({ pane: hd.dataset.sp, name: hd.dataset.sn }).then(a => {
      if (!a || !hd.isConnected) return;
      const card = hd.closest('.amsg');
      const n = h(senderHead(a, hd.dataset.what, a.pane, hd.dataset.sn));
      hd.replaceWith(n); tick(n);
      if (card && colCls(a.color)) { card.classList.remove('am-nc'); card.classList.add(colCls(a.color).trim()); }
    });
  }
}

// ---------- 3b: an asleep agent's chat ----------
/** Called on every state for a read-only main-session chat (#/t/<sid>, app.js ChatView.onState). */
export function asleepChat(view, st) {
  const sid = view.fixedTid; if (!sid || sid.includes('~')) return;
  const live = st?.agents.find(x => x.session === sid || x.tid === sid);
  if (live) { // awake (woken here or anywhere): its live chat instead
    if (G.view === view && view.zzGo !== live.pane) { view.zzGo = live.pane; location.replace('#/a/' + enc(live.pane)); }
    return;
  }
  if (!view.zzAsk) {
    view.zzAsk = resolve({ sid }).then(a => { view.zz = a || { sid, asleep: true, name: null }; draw(view); });
    return;
  }
  if (view.zz) draw(view);
}
function draw(view) {
  const a = view.zz, el = view.el, nm = a.name ? nameOf(a) : $('.t1', el).textContent || 'This agent';
  el.classList.add('zzv');
  if (a.color && el.dataset.col !== a.color) { if (el.dataset.col) el.classList.remove('col-' + el.dataset.col); el.dataset.col = a.color; el.classList.add('col-' + a.color); }
  const t1 = $('.t1', el), t2 = $('.t2', el);
  if (a.name && t1.textContent !== nm) t1.textContent = nm;
  const sub = `<span class="zzt">${MOON}Asleep${a.ts ? ` <span data-since="${a.ts}"></span>` : ''}</span> · ${esc(a.job || shortPath(a.cwd || '') || 'past session')}`;
  if (t2.dataset.zz !== sub) { t2.dataset.zz = sub; t2.innerHTML = sub; tick(t2); }
  // "Fell asleep at …" after the last message
  const last = view.items?.at(-1);
  if (view.loaded && view.items?.length) {
    let d = $('.fasleep-div', view.list);
    const txt = `${MOON}Fell asleep ${esc(atClock(last?.ts || a.ts || Date.now()))}`;
    if (!d) d = h('<div class="fasleep-div"><span></span></div>');
    if (d.firstChild.innerHTML !== txt) d.firstChild.innerHTML = txt;
    if (view.list.lastElementChild !== d) view.list.append(d);
  } else if (!view.zzT) view.zzT = setTimeout(() => { view.zzT = 0; if (view.el.isConnected && view.zz) draw(view); }, 400);
  if (!$('.fwake', el)) {
    const w = h(`<div class="fwake"><div class="w1"><span class="mn">${MOON}</span><div><b>${esc(nm)} is asleep</b>`
      + `<small>Same conversation, same folder${a.cwd ? ': ' + esc(shortPath(a.cwd)) : ''}</small></div></div>`
      + `<div class="wb"><button class="btn wmsg">Message…</button><button class="btn primary wgo">Continue</button></div>`
      + `<form class="wform" hidden><textarea rows="2" placeholder="Message ${esc(nm)}" enterkeyhint="send"></textarea><button class="btn primary">Send</button></form>`
      + '<div class="wn">Continue wakes it right here with its whole history. It reads the last message and carries on.</div></div>');
    el.insertBefore(w, $('.composer', el));
    tidy().then(m => { m.saidInto(w, a.sid); m.noteInto(w, a.sid); }, () => { }); // what the user last said to it and what it waits on; its handoff note
    $('.wgo', w).onclick = e => continueAgent(a.sid, { btn: e.currentTarget, name: nm });
    const f = $('.wform', w), ta = $('textarea', f);
    $('.wmsg', w).onclick = () => { f.hidden = false; $('.wb', w).hidden = true; $('.wn', w).textContent = 'Sending wakes it with your message, right here with its whole history.'; ta.focus(); };
    f.onsubmit = e => { e.preventDefault(); const t = ta.value.trim(); if (t) continueAgent(a.sid, { text: t, btn: $('button', f), name: nm }); };
    ta.onkeydown = e => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) f.requestSubmit(); };
  }
}

// ---------- 1a: Awake / Asleep on the Agents tab ----------
// Clean up agents (cleanup.js): the tidy card, the order of just-slept agents, the note in an asleep chat
let TIDY = null, SLEPT = null, TM = null;
const tidy = () => TIDY ??= import('./cleanup.js').then(m => { SLEPT = m.SLEPT; TM = m; return m; });
const ASLEEP_N = 5;
const lastLine = a => String(a.last?.text || '').replace(/\*\*|__|`+|^#+\s*/gm, '').replace(/\s+/g, ' ').trim();
/** One asleep agent row: disc with a moon, name + age, job line, last line, Continue. */
/** "↳ from <parent>" on a branch (Branch from here); a tap opens the parent, awake or asleep. */
export const fromHTML = a => a?.branchOf ? `<span class="xfrom" data-go="${a.branchOf.pane ? '#/a/' + enc(a.branchOf.pane) : '#/t/' + enc(a.branchOf.sid)}">↳ from <b>${esc(whoName(a.branchOf.name))}</b></span>` : '';
document.addEventListener('click', e => { const f = e.target.closest('.xfrom[data-go], .gfrom[data-go]'); if (f) { e.preventDefault(); e.stopPropagation(); location.hash = f.dataset.go; } }, true);
export const sleepRow = (a, { hl = x => esc(x), why = '' } = {}) => `<a class="xrow zz${colCls(a.color)}" href="#/t/${enc(a.sid)}" data-sid="${esc(a.sid)}">${favHTML(a)}`
  + `<span class="xb"><span class="f1"><span class="xnm">${hl(nameOf(a))}</span><span class="tm" data-since="${a.ts}"></span></span>`
  + `<span class="f2">${fromHTML(a)}${a.job ? hl(a.job) : esc(shortPath(a.cwd || ''))}</span>`
  + `<span class="f3"><span class="zzt">${MOON}</span><span class="xlt">${why || esc(lastLine(a) || 'Asleep')}</span></span></span>`
  + `<button class="btn cont" data-cont="${esc(a.sid)}" data-nm="${esc(nameOf(a))}">Continue</button></a>`;
let drawT = 0;
/** Called from app.js render(): the find field above the rows and the Asleep group below them. `skip` = sids shown elsewhere. */
export function drawAgents(el, st, skip = recentSids(st)) {
  const cbox = $('.cbox', el); if (!cbox) return;
  drawRecent(el, st);
  tidy().then(m => { m.drawCard(el, G.st); m.drawJust(el, G.st); }, () => { }); // cleanup.js: the "N agents are idle" card under Recent, then "Just put to sleep"
  let ff = $('.ffind', cbox);
  if (!ff) {
    ff = h(`<button class="ffind">${I.search}<span>Find an agent: name, job, folder, chat</span></button>`);
    cbox.prepend(ff); ff.onclick = () => openSearch();
    cbox.addEventListener('click', e => {
      const c = e.target.closest('.g-zz [data-cont]');
      if (c) { e.preventDefault(); e.stopPropagation(); continueAgent(c.dataset.cont, { btn: c, name: c.dataset.nm }); }
    }, true);
  }
  const L = cached();
  if (!L || Date.now() - AT > 15e3) agents().then(() => { clearTimeout(drawT); drawT = setTimeout(() => G.view?.el === el && G.view.onState?.(G.st), 30); }, () => { });
  if (!L) return;
  const live = new Set((st?.agents || []).map(a => a.session).filter(Boolean));
  // just put to sleep (cleanup.js) first, as of then
  const just = TM?.justSids(st) || new Set(); // listed with one-tap Wake under "Just put to sleep"
  const zz = L.agents.filter(a => a.asleep && !live.has(a.sid) && !skip.has(a.sid) && !just.has(a.sid)).map(a => SLEPT?.get(a.sid) > a.ts ? { ...a, ts: SLEPT.get(a.sid) } : a).sort((a, b) => b.ts - a.ts);
  let sec = $('.g-zz', cbox);
  if (!zz.length) { sec?.remove(); return; }
  if (!sec) { sec = h('<section class="agrp g-zz"><div class="gh"><span class="gt">Asleep</span><span class="gn"></span></div><div class="gb"><div class="gl fzl"></div><div class="fhint"></div></div></section>'); foldable(sec, 'zz'); } // folds from its header (app.js); search is its own view
  const cards = $('.cards', cbox); if (sec.previousElementSibling !== cards) cards.after(sec);
  const shown = zz.slice(0, ASLEEP_N), older = zz.length - shown.length + (L.older || 0);
  const html = shown.map(a => sleepRow(a)).join('');
  if (sec._h !== html) { sec._h = html; $('.fzl', sec).innerHTML = html; tick(sec); }
  $('.gn', sec).textContent = zz.length + (L.older || 0);
  const hint = `Asleep = its pane is closed. Continue picks it up in place, whole conversation and all.${older ? ` <a href="#/fleet">${older} older in History ›</a>` : ''}`;
  if ($('.fhint', sec).innerHTML !== hint) $('.fhint', sec).innerHTML = hint;
}

// ---------- 1b: search agents by name, job, folder ----------
const norm = s => String(s || '').toLowerCase();
/** Agents matching every word of `q` in name, job, folder or title; awake first, then most recent. */
export async function findAgents(q) {
  const L = await agents().catch(() => null), toks = norm(q).split(/\s+/).filter(Boolean);
  if (!L || !toks.length) return [];
  const out = [];
  for (const a of L.agents) {
    const f = { name: norm(nameOf(a)) + ' ' + norm(a.title), job: norm(a.job), folder: norm(a.cwd) };
    if (!toks.every(t => Object.values(f).some(v => v.includes(t)))) continue;
    const why = Object.keys(f).filter(k => toks.some(t => f[k].includes(t)));
    out.push({ a, why, score: (why.includes('name') ? 4 : 0) + (why.includes('job') ? 2 : 0) + (a.asleep ? 0 : 1) });
  }
  return out.sort((x, y) => y.score - x.score || (y.a.ts || 0) - (x.a.ts || 0));
}
/** Highlights every word of `q` in `text` (escaped). */
export function hlWords(text, q) {
  const t = String(text ?? ''), toks = norm(q).split(/\s+/).filter(x => x.length > 0);
  if (!toks.length) return esc(t);
  const re = new RegExp(toks.map(x => x.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|'), 'gi');
  let out = '', k = 0;
  for (const m of t.matchAll(re)) { out += esc(t.slice(k, m.index)) + '<mark>' + esc(m[0]) + '</mark>'; k = m.index + m[0].length; }
  return out + esc(t.slice(k));
}
/** An awake agent's row in search results (asleep ones use sleepRow). */
export const awakeRow = (a, hl) => `<a class="xrow${colCls(a.color)}" href="${a.home ? '#/' : '#/a/' + enc(a.pane)}">${favHTML(a)}`
  + `<span class="xb"><span class="f1"><span class="xnm">${hl(nameOf(a))}</span></span><span class="f2">${fromHTML(a)}${a.job ? hl(a.job) : ''}</span>`
  + `<span class="f3">${stHTML(a)}<span>·</span><span class="xlt">${hl(shortPath(a.cwd || ''))}</span></span></span><span class="xchev">${CHEV}</span></a>`;

// ---------- 2a: Recent on Home ----------
const RECENT_N = 6;
const PIN = '<svg class="pin-i" viewBox="0 0 24 24"><path d="M9 4h6l-1 5 4 4H6l4-4zM12 13v8"/></svg>';
/** Rows of the Recent grid: live state merged over the cached /api/agents list. */
function recentList(st) {
  const L = cached(); if (!L) return [];
  const live = new Map((st?.agents || []).filter(a => a.session).map(a => [a.session, a]));
  const rows = L.agents.map(r => {
    const a = live.get(r.sid);
    return a ? { ...r, branchOf: a.branchOf || r.branchOf, pane: a.pane, status: a.status, since: a.since, color: a.color, job: a.job ?? r.job, name: a.home ? HOME_NAME() : whoName(a.short || a.title || r.name, a.pane), home: !!a.home, pinned: a.pinned ?? r.pinned, asleep: false, service: !!a.service }
      : live.size && !r.asleep ? null : r; // gone since the list was fetched: asleep now, or not yet listed
  }).filter(Boolean).filter(r => !r.hidden);
  const when = r => r.lu || 0;
  const pins = rows.filter(r => r.pinned).sort((a, b) => (b.home - a.home) || when(b) - when(a) || (b.ts || 0) - (a.ts || 0));
  const rest = rows.filter(r => !r.pinned && !r.service && when(r) > 0).sort((a, b) => when(b) - when(a));
  return pins.concat(rest).slice(0, RECENT_N);
}
/** Session ids on the Recent grid (app.js leaves them out of Awake; drawAgents out of Asleep). */
export const recentSids = st => new Set(recentList(st).map(r => r.sid));
/* Recent grid cell: the job line under the name and the time
   at the top right; "now" while it works, "needs you" when blocked. The status shows on the disc (arc, moon). */
const cellJob = r => esc(r.job || shortPath(r.cwd || '') || (r.asleep ? 'Asleep' : 'Idle'));
const cellTime = r => {
  if (!r.asleep && r.status === 'working') return '<span class="gtm wk">now</span>';
  if (!r.asleep && r.status === 'blocked') return '<span class="gtm bk">needs you</span>';
  const t = r.asleep ? r.ts : r.status === 'done' ? (r.since || r.ts) : (r.lu || r.ts || r.since);
  return t ? `<span class="gtm" data-since="${t}"></span>` : '';
};
const hrefOf = r => r.asleep ? '#/t/' + enc(r.sid) : r.home ? '#/' : '#/a/' + enc(r.pane);
let MISS = '', MISS_T = 0;
export function drawRecent(el, st) {
  const brief = $('.brief', el); if (!brief) return;
  // an agent that got its session since the list was fetched (a branch, a new agent): fetch again, at most every 3 s
  const L0 = cached(), miss = L0 ? (st?.agents || []).filter(a => a.session && !a.service && !L0.agents.some(r => r.sid === a.session)).map(a => a.session).join() : '';
  if (miss && miss !== MISS) { // once per new set, so one that never lists can't make it poll
    MISS = miss; clearTimeout(MISS_T);
    MISS_T = setTimeout(() => agents(true).then(() => G.view?.el === el && G.view.onState?.(G.st), () => { }), Math.max(0, 3e3 - (Date.now() - AT)));
  }
  let box = $('.frec', el);
  const rows = recentList(st);
  if (!rows.length) { box?.remove(); return; }
  if (!box) {
    box = h('<section class="frec"><div class="gh"><span class="gt">Agents</span><span class="sp"></span><button class="fedit">Edit</button></div><div class="fgrid"></div></section>');
    $('.fedit', box).onclick = () => editSheet();
    longPress(box, '.fgc', c => { const r = recentList(G.st).find(x => x.sid === c.dataset.sid); if (!r) return false; pinSheet(r, c); });
  }
  const prev = box.previousElementSibling; // right under the briefing, or under Decide's card (decide.js) that sits there
  if (prev !== brief && !(prev?.classList.contains('dwrap') && prev.previousElementSibling === brief)) brief.after(box);
  const html = rows.map(r => `<a class="fgc${colCls(r.color)}" href="${hrefOf(r)}" data-sid="${esc(r.sid)}">${favHTML(r, { sz: 30 })}<span class="gb"><b>${esc(nameOf(r))}</b><small>${cellJob(r)}</small>${r.branchOf ? `<small class="gfrom" data-go="${r.branchOf.pane ? '#/a/' + enc(r.branchOf.pane) : '#/t/' + enc(r.branchOf.sid)}">↳ from ${esc(whoName(r.branchOf.name))}</small>` : ''}</span>${cellTime(r)}</a>`).join('');
  const g = $('.fgrid', box);
  if (g._h !== html) { g._h = html; g.innerHTML = html; }
  tick(g);
}

// ---------- 2b: the long-press sheet ----------
const ICO = {
  chat: '<svg viewBox="0 0 24 24"><path d="M4 5h16v11H9l-5 4z"/></svg>',
  pencil: '<svg viewBox="0 0 24 24"><path d="M4 20h4L19 9l-4-4L4 16zM14 6l4 4"/></svg>',
  hide: '<svg viewBox="0 0 24 24"><path d="M5 12h14"/></svg>',
};
async function setMeta(r, body) {
  try { await post(`agents/${enc(r.sid)}/meta`, body); forget(); await agents(true); G.view?.onState?.(G.st); return true; }
  catch (e) { toast(esc(e.message), 'err'); return false; }
}
const statusLine = r => (r.asleep ? `${MOON}Asleep <span data-since="${r.ts}"></span>` : r.status === 'working' ? '<span class="wkt">Working</span>'
  : r.status === 'done' ? `Done <span data-since="${r.since || r.ts}" data-ago></span>` : r.status === 'blocked' ? 'Needs you' : 'Idle')
  + (r.cwd ? ' · ' + esc(shortPath(r.cwd)) : '');
const chip = r => `<span class="fchip${colCls(r.color)}${r.home ? ' xed' : ''}"><span class="xpn">${PIN}</span><i class="d"></i>${esc(nameOf(r))}</span>`;
export function pinSheet(r, cell) {
  cell?.classList.add('press'); setTimeout(() => cell?.classList.remove('press'), 400);
  const L = cached(), max = L?.maxPins || 5;
  let pins = (L?.agents || []).filter(x => x.pinned), n = pins.length;
  const ax = () => r.pinned ? `${n} of ${max} pinned` : n >= max ? `${max} of ${max}: unpin one first` : `${n} of ${max} pinned`;
  const sh = sheet(`<div class="fsh-top${colCls(r.color)}">${favHTML(r, { sz: 46 })}<div class="tb"><b>${esc(nameOf(r))}</b><span>${esc(r.job || '')}</span><small>${statusLine(r)}</small></div></div>
    <div class="acts fsh-acts">
      <button class="act xpri" data-a="pin">${PIN}${r.pinned ? 'Unpin from Home' : 'Pin to Home'}<span class="xax">${ax()}</span></button>
      <button class="act" data-a="open">${ICO.chat}Open chat</button>
      <button class="act" data-a="edit">${ICO.pencil}${r.home ? 'Edit job line' : 'Rename and edit job line'}</button>
      <button class="act danger" data-a="hide">${ICO.hide}Hide from Recent</button>
    </div>
    ${pins.length ? `<div class="fpinned">Pinned:${pins.map(chip).join('')}</div>` : ''}`, 'fpsheet');
  tick(sh);
  agents(true).then(L2 => { pins = L2.agents.filter(x => x.pinned); n = pins.length; const x = $('.xax', sh); if (x) x.textContent = ax(); }, () => { }); // fresh count
  sh.addEventListener('click', async e => {
    const b = e.target.closest('[data-a]'); if (!b) return;
    const a = b.dataset.a;
    if (a === 'pin') {
      if (!r.pinned && n >= max) { toast(`${max} agents are pinned: unpin one first`, 'err'); return; }
      haptic(); if (await setMeta(r, { pinned: !r.pinned })) { sh.close(); toast(r.pinned ? 'Unpinned' : 'Pinned to Home', 'ok', null, 1200); }
    } else if (a === 'open') { sh.close(); location.hash = hrefOf(r); }
    else if (a === 'edit') { sh.close(); setTimeout(() => editMeta(r), 280); }
    else if (a === 'hide') { haptic(); if (await setMeta(r, { hidden: true, ...(r.pinned ? { pinned: false } : {}) })) { sh.close(); toast('Hidden from Recent', 'ok', null, 1200); } }
  });
  return sh;
}
/** Rename (asleep: stored, typed in on Continue; awake: /api/a/<pane>/meta) and the job line (the user's, kept). */
export function editMeta(r) {
  const sh = sheet(`<h2>${r.home ? 'Job line' : 'Name and job line'}</h2><form class="fmeta">
    ${r.home ? '' : `<label>Name<input name="name" value="${esc(nameOf(r))}" maxlength="100" autocomplete="off"></label>`}
    <label>Job line<input name="job" value="${esc(r.job || '')}" maxlength="120" placeholder="What it does for you, in a line" autocomplete="off"></label>
    <small>Clear the job line to let it be written for you again.</small>
    <div class="btns"><button class="btn primary">Save</button></div></form>`);
  const f = $('form', sh);
  f.onsubmit = async e => {
    e.preventDefault();
    const name = f.name?.value.trim(), job = f.job.value.trim(), body = {};
    if (job !== (r.job || '')) body.job = job;
    if (name && name !== nameOf(r)) {
      if (r.asleep) body.name = name;
      else try { await post(`a/${enc(r.pane)}/meta`, { title: name }); } catch (x) { toast(esc(x.message), 'err'); return; }
    }
    if (Object.keys(body).length && !(await setMeta(r, body))) return;
    forget(); agents(true).then(() => G.view?.onState?.(G.st));
    sh.close(); toast('Saved', 'ok', null, 1000);
  };
}
/** Recent → Edit: hidden agents come back from here; pins at a glance. */
function editSheet() {
  const L = cached(); if (!L) return;
  const hid = L.agents.filter(a => a.hidden), pins = L.agents.filter(a => a.pinned);
  const sh = sheet(`<h2>Recent</h2><p class="fsh-note">Pinned agents always stay in Recent (${pins.length} of ${L.maxPins || 5}); the rest are the last ones you talked to. Hold one for more.</p>
    ${pins.length ? `<div class="fpinned">Pinned:${pins.map(chip).join('')}</div>` : ''}
    ${hid.length ? `<h3 class="sh3">Hidden</h3><div class="acts">${hid.map(a => `<button class="act" data-show="${esc(a.sid)}">Show ${esc(nameOf(a))} again</button>`).join('')}</div>` : ''}`);
  sh.addEventListener('click', async e => {
    const b = e.target.closest('[data-show]'); if (!b) return;
    const a = L.agents.find(x => x.sid === b.dataset.show);
    if (a && await setMeta(a, { hidden: false })) b.remove();
  });
}
