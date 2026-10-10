// Clean up agents (the user's pick "A with C's catch-up + sleep commitment"; ~/Work/sheprd-design/cleanup/SPEC.md,
// server/src/tidy.rs, API.md "Clean up agents"). Builds on Find and continue (find.js): sleep = close the pane, the
// chat stays and Continue wakes it in place, so a sleeping agent is in Asleep at once.
// A1  Home card under Recent: "N agents are idle", faces, Tidy up / Not now (6 h). Shows with ≥ 3 safe agents.
// A2  Review sheet: Done, nothing left (checked) · Look first (unchecked; tap = its catch-up card) · Staying awake;
//     footer: 5-line note toggle, Put N to sleep, Then catch up on N ›.
// C2  Catch-up: one full-screen card per agent that needs a look: what it asked, its chips, one fix per reason
//     (Answer / Push / Stop :port / Send draft) sent with a commitment to sleep, then Sleep · Note + sleep · Keep
//     (swipe left = sleep, right = keep).
// A3  Undo toast with an 8 s ring; the agents go to the top of Asleep.
import { G, $, h, esc, api, post, toast, haptic, tick, sheet, shortPath, ls, HOME_NAME } from './app.js';
import { md } from './md.js';
import { favHTML, colCls, forget, agents, MOON } from './find.js';

(() => { const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'cleanup.css'; document.head.append(l); })();

const enc = encodeURIComponent;
/** sid → when it was put to sleep (find.js puts these first in Asleep). */
export const SLEPT = new Map();
const IC = {
  broom: '<svg viewBox="0 0 24 24"><path d="M14.5 3.5l-3.2 7.2M7.2 11.2h8.6l1.7 8.8H5.5z M9.6 15.2v4.6M13.4 15.2v4.6"/></svg>',
  check: '<svg viewBox="0 0 24 24"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>',
  q: '<svg viewBox="0 0 24 24"><path d="M4 5h16v11H9l-5 4z"/></svg>',
  draft: '<svg viewBox="0 0 24 24"><path d="M4 20h4L19 9l-4-4L4 16zM14 6l4 4"/></svg>',
  git: '<svg viewBox="0 0 24 24"><circle cx="6" cy="5.5" r="2.2"/><circle cx="6" cy="18.5" r="2.2"/><circle cx="18" cy="8.5" r="2.2"/><path d="M6 7.7v8.6M18 10.7c0 4-6 3-11 6"/></svg>',
  srv: '<svg viewBox="0 0 24 24"><rect x="4" y="4.5" width="16" height="6.5" rx="1.6"/><rect x="4" y="13" width="16" height="6.5" rx="1.6"/><path d="M7.5 7.8h.01M7.5 16.2h.01"/></svg>',
  shell: '<svg viewBox="0 0 24 24"><path d="M5 7l4.5 4.5L5 16M12 17h7"/></svg>',
  sub: '<svg viewBox="0 0 24 24"><circle cx="12" cy="6" r="2.5"/><circle cx="6" cy="18" r="2.5"/><circle cx="18" cy="18" r="2.5"/><path d="M12 8.5v3M12 11.5l-5 4.2M12 11.5l5 4.2"/></svg>',
  doc: '<svg viewBox="0 0 24 24"><path d="M6 3h8l4 4v14H6zM14 3v4h4M9 12h6M9 15.5h6"/></svg>',
  pin: '<svg viewBox="0 0 24 24"><path d="M9 4h6l-1 5 4 4H6l4-4zM12 13v8"/></svg>',
  back: '<svg viewBox="0 0 24 24"><path d="M15 5l-7 7 7 7"/></svg>',
};
const CHIP_IC = { ok: IC.check, q: IC.q, draft: IC.draft, git: IC.git, srv: IC.srv, shell: IC.shell, sub: IC.sub };
const ago = ms => { const m = Math.max(0, Math.round(ms / 60e3)); return m < 60 ? `${Math.max(1, m)}m` : m < 2880 ? `${Math.round(m / 60)}h` : `${Math.round(m / 1440)}d`; };
const kTok = n => n == null ? '' : n >= 1e6 ? (n / 1e6).toFixed(1) + 'M' : Math.round(n / 1e3) + 'k';
const plural = (n, a, b = a + 's') => `${n} ${n === 1 ? a : b}`;
const noteOn = () => ls.get('tidyNote', true) !== false;

// ---------- data: GET /api/tidy (20 s), /api/tidy/pending ----------
let T = null, TAT = 0, TLOAD = null;
export function load(force) {
  if (!force && T && Date.now() - TAT < 20e3) return Promise.resolve(T);
  TLOAD ??= api('tidy').then(r => { T = r; TAT = Date.now(); took(r); return r; }).finally(() => { TLOAD = null; });
  return TLOAD;
}
export const data = () => T;
const redraw = () => G.view?.onState?.(G.st);
/** Pending sleeps / commitments the server reports: undo toasts for commitment sleeps, Asleep order for done ones. */
function took(r) {
  for (const [sid, at] of Object.entries(r.slept || {})) SLEPT.set(sid, at);
  for (const p of r.pending || []) {
    if (p.done) p.sids?.forEach(s => SLEPT.set(s, p.due));
    if (p.by === 'commit' && !SEEN.has(p.id) && !p.done && p.due > Date.now()) { SEEN.add(p.id); undoToast(p); }
  }
  if (r.commits?.length) watch();
}
const SEEN = new Set();
let WT = 0;
function watch() {
  if (WT) return;
  WT = setInterval(async () => {
    if (document.hidden) return;
    try {
      const r = await api('tidy/pending');
      took({ ...r, commits: [] });
      if (!r.commits?.length && !(r.pending || []).some(p => !p.done)) { clearInterval(WT); WT = 0; T = null; forget(); agents(true).then(redraw, () => { }); }
    } catch { }
  }, 2500);
}

// ---------- shared bits ----------
const face = (r, sz) => favHTML({ name: r.name, color: r.color, home: r.home }, { sz });
const chipHTML = x => `<span class="tchip k-${esc(x.kind)}">${CHIP_IC[x.kind] || ''}${esc(x.text)}</span>`;
const ctxHTML = r => r.ctx?.pct != null ? `<span class="tctx" style="--p:${Math.round(r.ctx.pct)}"><i></i>${Math.round(r.ctx.pct)}%</span>` : '';
const fold = s => String(s || '').replace(/\s+/g, ' ').trim();

/** Puts these agents to sleep after the undo window; shows the undo toast. */
async function sleepNow(rows, note) {
  const panes = rows.map(r => r.pane);
  try {
    const r = await post('tidy/sleep', { panes, note });
    SEEN.add(r.id);
    if (T) for (const k of ['safe', 'check']) T[k] = T[k].filter(x => !panes.includes(x.pane));
    redraw();
    undoToast({ ...r, names: r.names, n: r.n, note });
    return r;
  } catch (e) { toast('Couldn’t put them to sleep: ' + esc(e.message), 'err'); return null; }
}

// ---------- A3: the undo toast ----------
let TOAST = null;
function undoToast(p) {
  TOAST?.remove();
  const n = p.n || p.names?.length || 1;
  const left = () => Math.max(0, Math.ceil((p.due - Date.now()) / 1000));
  const head = n === 1 ? `${esc(p.names?.[0] || 'It')} is asleep` : `${n} agents asleep`;
  const sub = `${p.note ? 'Notes saved · ' : ''}${n === 1 ? 'it’s' : 'they’re'} in Asleep`;
  const t = TOAST = h(`<div class="tdy-toast" role="status"><span class="mn">${MOON}</span><div class="tx"><b>${head}</b><small>${sub}</small></div>`
    + `<button class="tt-undo">Undo<span class="ring" style="--f:1"><svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="10"/></svg><em>${left()}</em></span></button></div>`);
  document.body.append(t);
  requestAnimationFrame(() => t.classList.add('in'));
  const total = Math.max(1, p.due - (p.at || Date.now()));
  const end = () => { clearInterval(iv); t.classList.remove('in'); setTimeout(() => t.remove(), 300); if (TOAST === t) TOAST = null; };
  const iv = setInterval(() => {
    const ms = p.due - Date.now();
    $('.ring', t).style.setProperty('--f', Math.max(0, ms / total)); $('em', t).textContent = left();
    if (ms <= 0) { (p.sids || []).forEach(s => SLEPT.set(s, Date.now())); end(); setTimeout(() => { T = null; forget(); agents(true).then(redraw, () => { }); load(true).then(redraw, () => { }); }, 1200); }
  }, 250);
  $('.tt-undo', t).onclick = async () => {
    haptic(); end();
    try { await post('tidy/undo', { id: p.id }); toast(n === 1 ? 'Undone: it’s still awake' : 'Undone: they’re still awake', 'ok', null, 1600); }
    catch (e) { toast(esc(e.message), 'err'); }
    T = null; load(true).then(redraw, () => { });
  };
}

// ---------- A1: the Home card ----------
let HIDE = 0;
/** Called from find.js drawAgents (Agents view render): the card right under Recent. */
export function drawCard(el, st) {
  if (!T || Date.now() - TAT > 30e3) load().then(() => { if (G.view?.el === el) drawCard(el, G.st); }, () => { });
  let box = $('.tdy-card', el);
  const safe = T?.safe || [], check = T?.check || [];
  const show = T && safe.length >= 3 && !(T.hiddenUntil > Date.now()) && HIDE < Date.now();
  if (!show) { box?.remove(); return; }
  if (!box) {
    box = h('<section class="tdy-card"></section>');
    box.addEventListener('click', e => {
      if (e.target.closest('.tc-go')) { haptic(); review(); }
      else if (e.target.closest('.tc-no')) {
        haptic(); HIDE = Date.now() + 6 * 3600e3; box.classList.add('out'); setTimeout(() => box.remove(), 260);
        post('tidy/hide', { hours: 6 }).then(r => { if (T) T.hiddenUntil = r.hiddenUntil; }, () => { });
      }
    });
  }
  const after = $('.frec', el) || $('.brief', el);
  if (after && box.previousElementSibling !== after) after.after(box);
  const n = safe.length + check.length, faces = safe.slice(0, 6), more = n - faces.length;
  const sub = check.length ? `${safe.length} ${safe.length === 1 ? 'is' : 'are'} done with nothing left · ${check.length} need${check.length === 1 ? 's' : ''} a look first` : `${safe.length === 1 ? 'It is' : 'All are'} done with nothing left`;
  const html = `<div class="tc-top"><span class="tc-ic">${IC.broom}</span><div class="tc-tx"><b>${plural(n, 'agent')} ${n === 1 ? 'is' : 'are'} idle</b><small>${sub}</small></div></div>`
    + `<div class="tc-faces">${faces.map(r => `<span class="tcf${colCls(r.color)}">${face(r, 30)}</span>`).join('')}${more > 0 ? `<span class="tc-more">+${more}</span>` : ''}</div>`
    + '<div class="tc-btns"><button class="btn primary tc-go">Tidy up</button><button class="btn tc-no">Not now</button></div>';
  if (box._h !== html) { box._h = html; box.innerHTML = html; }
}

// ---------- A2: the review sheet ----------
const rowHTML = (r, sec, on) => `<div class="tr${colCls(r.color)}" data-pane="${esc(r.pane)}" data-sec="${sec}">`
  + `<button class="tck${on ? ' on' : ''}" aria-label="${on ? 'Selected' : 'Not selected'}" aria-pressed="${on}">${IC.check}</button>${face(r, 34)}`
  + `<div class="tb"><div class="t1"><b class="nm">${esc(r.name)}</b><span class="tm">${ago(r.idleMs)}</span></div>`
  + `<div class="t2">${esc(fold(r.last) || shortPath(r.cwd || ''))}</div>`
  + `${r.reasons.length || r.ctx?.pct != null ? `<div class="t3">${r.reasons.map(chipHTML).join('')}${ctxHTML(r)}</div>` : ''}</div></div>`;
/** "Master and Backup never sleep · 2 working · 1 pinned" */
function keepWhy(keep) {
  const by = k => keep.filter(r => r.reasons[0]?.kind === k);
  const lock = by('lock').map(r => r.name);
  const out = [];
  if (lock.length) out.push(`${lock.length > 2 ? lock.slice(0, -1).join(', ') + ' and ' + lock.at(-1) : lock.join(' and ')} never sleep${lock.length === 1 ? 's' : ''}`);
  const work = by('work'), w = work.filter(r => r.reasons[0].text === 'working').length, act = work.filter(r => r.reasons[0].text === 'active recently').length;
  const need = work.length - w - act;
  if (w) out.push(`${w} working`);
  if (need) out.push(`${need} waiting on you`);
  if (act) out.push(`${act} active in the last hour`);
  const pin = by('pin'), pinned = pin.filter(r => r.reasons[0].text === 'pinned').length, kept = pin.filter(r => r.reasons[0].text === 'kept for today').length, timed = pin.length - pinned - kept;
  if (pinned) out.push(`${pinned} pinned`);
  if (kept) out.push(`${kept} kept for today`);
  if (timed) out.push(`${timed} with a timer`);
  return out.join(' · ');
}
export async function review() {
  const t = await load(true).catch(e => { toast('Couldn’t load: ' + esc(e.message), 'err'); return null; });
  if (!t) return;
  const on = new Set(t.safe.map(r => r.pane));
  const sh = sheet(`<div class="ts-h"><h2>Tidy up</h2><button class="ts-x">Cancel</button></div>
    <p class="ts-sub">Sleeping closes the pane. The chat stays, and Continue wakes it in place.</p>
    <div class="ts-body"></div>`, 'tall tdy-sheet');
  const body = $('.ts-body', sh), shEl = $('.sheet', sh);
  const foot = h(`<div class="ts-foot"><label class="tsw"><input type="checkbox"${noteOn() ? ' checked' : ''}><i></i><span>Save a 5-line note for each first</span></label>
    <div class="ts-btns"><button class="btn primary tf-go"></button><button class="btn tf-next" hidden></button></div></div>`);
  shEl.append(foot);
  const draw = () => {
    const tt = T || t;
    const html = (tt.safe.length ? `<div class="ts-gh"><span>Done, nothing left <em>${tt.safe.length}</em></span><button class="ts-all">${tt.safe.every(r => on.has(r.pane)) ? 'Uncheck all' : 'Check all'}</button></div>`
      + `<div class="ts-list">${tt.safe.map(r => rowHTML(r, 'safe', on.has(r.pane))).join('')}</div>` : '')
      + (tt.check.length ? `<div class="ts-gh"><span>Look first <em>${tt.check.length}</em></span></div><div class="ts-list">${tt.check.map(r => rowHTML(r, 'check', on.has(r.pane))).join('')}</div>` : '')
      + (tt.keep.length ? `<div class="ts-gh"><span>Staying awake <em>${tt.keep.length}</em></span></div><div class="ts-keep"><span class="tk-f">${tt.keep.slice(0, 7).map(r => `<span class="tcf${colCls(r.color)}">${face(r, 26)}</span>`).join('')}</span><span class="tk-w">${esc(keepWhy(tt.keep))}</span></div>` : '')
      + (!tt.safe.length && !tt.check.length ? '<p class="ts-none">Nothing idle to tidy right now.</p>' : '');
    if (body._h !== html) { body._h = html; body.innerHTML = html; tick(body); }
    const n = [...on].filter(p => tt.safe.some(r => r.pane === p) || tt.check.some(r => r.pane === p)).length;
    const go = $('.tf-go', foot), nx = $('.tf-next', foot), left = tt.check.filter(r => !on.has(r.pane));
    go.textContent = n ? `Put ${n} to sleep` : 'Nothing selected'; go.disabled = !n;
    nx.hidden = !left.length; nx.textContent = n ? `Then catch up on ${left.length} ›` : `Catch up on ${left.length} ›`;
  };
  draw();
  sh.addEventListener('click', async e => {
    if (e.target.closest('.ts-x')) { sh.close(); return; }
    const tt = T || t;
    if (e.target.closest('.ts-all')) { haptic(); const all = tt.safe.every(r => on.has(r.pane)); tt.safe.forEach(r => all ? on.delete(r.pane) : on.add(r.pane)); draw(); return; }
    const row = e.target.closest('.tr');
    if (row) {
      const p = row.dataset.pane;
      if (row.dataset.sec === 'check' && !e.target.closest('.tck')) { // a Look first row opens its catch-up card
        const me = tt.check.find(r => r.pane === p), list = [me, ...tt.check.filter(r => r !== me && !on.has(r.pane))];
        sh.close(); setTimeout(() => catchUp(list, 0), 220); return;
      }
      haptic(); on.has(p) ? on.delete(p) : on.add(p); draw(); return;
    }
    const note = $('.tsw input', foot).checked;
    if (e.target.closest('.tf-go') || e.target.closest('.tf-next')) {
      const next = !!e.target.closest('.tf-next'), rows = [...tt.safe, ...tt.check].filter(r => on.has(r.pane)), left = tt.check.filter(r => !on.has(r.pane));
      haptic(); sh.close();
      if (rows.length) await sleepNow(rows, note);
      if (next && left.length) setTimeout(() => catchUp(left, 0), 240);
    }
  });
  $('.tsw input', foot).onchange = e => ls.set('tidyNote', e.target.checked);
}

// ---------- C2: catch-up, one card at a time ----------
function fixHTML(r) {
  const q = r.reasons.find(x => x.kind === 'q');
  const fixes = r.reasons.filter(x => x.fix && x.kind !== 'q');
  let out = '';
  if (q) {
    out += q.fix?.reply ? `<h4>Suggested reply</h4><button class="cu-sug">${esc(q.fix.reply)}</button>` : '';
    out += '<form class="cu-ans"><input type="text" enterkeyhint="send" autocomplete="off" autocapitalize="sentences" placeholder="Answer, then it sleeps…"><button class="btn primary" type="submit">Send</button></form>';
  }
  if (fixes.length) out += `<div class="cu-fixes">${fixes.map(x => `<button class="btn cu-fix" data-k="${esc(x.fix.kind)}" data-text="${esc(x.fix.text || '')}">${esc(x.fix.label || x.fix.kind)}</button>`).join('')}</div>`;
  if (q || fixes.length) out += '<p class="cu-why">Sent with a promise to sleep: once it’s done and asks nothing new, its pane closes.</p>';
  return out;
}
function cardHTML(r) {
  const q = r.reasons.some(x => x.kind === 'q'), full = r.full || r.last || '';
  const pct = r.ctx?.pct != null ? Math.round(r.ctx.pct) : null;
  return `<div class="cu-card${colCls(r.color)}" data-pane="${esc(r.pane)}"><div class="cu-h">${face(r, 46)}<div><b class="nm">${esc(r.name)}</b><small>${esc(shortPath(r.cwd || ''))} · last spoke ${ago(r.idleMs)} ago</small></div></div>`
    + (pct != null ? `<div class="cu-ctx"><span>Context</span><i class="bar"><b style="width:${Math.min(100, pct)}%"></b></i><span>${pct}%${r.ctx.tokens ? ' · ' + kTok(r.ctx.tokens) : ''}</span></div>` : '')
    + (full ? `<h4>${q ? 'It asked you' : 'Its last message'}</h4><div class="cu-msg${q ? ' ask' : ''}">${md(full)}</div>` : '')
    + `<div class="cu-chips">${r.reasons.filter(x => x.kind !== 'q').map(chipHTML).join('')}</div>`
    + fixHTML(r) + '</div>';
}
let FLOW = null;
export function catchUp(list, start = 0) {
  if (!list?.length) return;
  FLOW?.remove();
  const f = FLOW = h(`<div class="tdy-flow"><header class="cu-bar"><button class="ib cu-back" aria-label="Back">${IC.back}</button><b>Clean up</b><span class="sp"></span><button class="cu-close">Close</button></header>
    <div class="cu-sc"><div class="cu-prog"></div><div class="cu-step"></div><div class="cu-slot"></div>
    <div class="cu-tiles"><button data-t="sleep">${MOON}<b>Sleep</b><small></small></button><button data-t="note">${IC.doc}<b>Note + sleep</b><small>5 lines saved</small></button><button data-t="keep">${IC.pin}<b>Keep</b><small>skip till next time</small></button></div>
    <div class="cu-foot"></div></div></div>`);
  document.body.append(f);
  requestAnimationFrame(() => f.classList.add('in'));
  let k = start, busy = false;
  const done = new Set();
  const close = () => { f.classList.remove('in'); setTimeout(() => f.remove(), 260); if (FLOW === f) FLOW = null; T = null; load(true).then(redraw, () => { }); };
  const show = dir => {
    const r = list[k];
    $('.cu-prog', f).innerHTML = list.map((_, i) => `<i class="${i < k ? 'f' : i === k ? 'cur' : ''}"></i>`).join('');
    $('.cu-step', f).textContent = `${k + 1} of ${list.length} to look at`;
    const slot = $('.cu-slot', f), n = h(cardHTML(r));
    slot.querySelectorAll('.go-l, .go-r').forEach(x => x.remove()); // a quick tap: the one before is still leaving
    const old = slot.querySelector('.cu-card');
    if (old && dir) { old.classList.add('go-' + dir); setTimeout(() => old.remove(), 300); n.classList.add('enter'); slot.append(n); requestAnimationFrame(() => requestAnimationFrame(() => n.classList.remove('enter'))); }
    else { slot.innerHTML = ''; slot.append(n); }
    tick(n);
    $('[data-t="sleep"] small', f).textContent = r.reasons.some(x => x.kind === 'q') ? 'drop the question' : 'close the pane';
    const rest = list.slice(k + 1).map(x => x.name);
    $('.cu-foot', f).innerHTML = `Swipe the card: ← sleep · → keep${rest.length ? `<br>Next: ${rest.map(esc).join(' · ')}` : '<br>Last one'}`;
    $('.cu-sc', f).scrollTop = 0;
  };
  const next = dir => { done.add(list[k].pane); busy = false; if (k + 1 >= list.length) { close(); toast('All caught up', 'ok', null, 1400); return; } k++; show(dir); };
  const act = async (t, dir) => {
    if (busy) return; busy = true; haptic();
    const r = list[k];
    if (t === 'keep') { try { await post('tidy/snooze', { pane: r.pane }); } catch (e) { toast(esc(e.message), 'err'); busy = false; return; } toast(`Kept ${esc(r.name)} till tomorrow`, 'ok', null, 1200); }
    else if (!(await sleepNow([r], t === 'note'))) { busy = false; return; }
    next(dir || (t === 'keep' ? 'r' : 'l'));
  };
  const fix = async (kind, text) => {
    if (busy) return; busy = true; haptic();
    const r = list[k];
    try {
      const x = await post('tidy/fix', { pane: r.pane, kind, text, note: noteOn() });
      toast(x?.via === 'held' ? `Held for its draft · ${esc(r.name)} sleeps once it’s done` : `Sent · ${esc(r.name)} sleeps once it’s done`, 'ok', null, 1800);
      watch();
      next('l');
    } catch (e) { toast('Couldn’t send: ' + esc(e.message), 'err'); busy = false; }
  };
  f.addEventListener('click', e => {
    if (e.target.closest('.cu-close')) { close(); return; }
    if (e.target.closest('.cu-back')) { if (k > 0) { k--; show('r'); } else close(); return; }
    const t = e.target.closest('.cu-tiles [data-t]'); if (t) { act(t.dataset.t); return; }
    const s = e.target.closest('.cu-sug'); if (s) { const i = $('.cu-ans input', f); i.value = s.textContent; i.focus(); return; }
    const x = e.target.closest('.cu-fix'); if (x) { fix(x.dataset.k, x.dataset.text || undefined); }
  });
  f.addEventListener('submit', e => { e.preventDefault(); const v = $('.cu-ans input', f)?.value.trim(); if (v) { $('.cu-ans input', f).blur(); fix('answer', v); } });
  // swipe the card: left = sleep, right = keep
  let g = null;
  f.addEventListener('pointerdown', e => { const c = e.target.closest('.cu-card'); if (!c || e.target.closest('input, button, a, form, .cu-msg pre')) return; g = { c, x: e.clientX, y: e.clientY, id: e.pointerId, lock: false, dx: 0 }; });
  f.addEventListener('pointermove', e => {
    if (!g || e.pointerId !== g.id) return;
    const dx = e.clientX - g.x, dy = e.clientY - g.y;
    if (!g.lock) { if (Math.hypot(dx, dy) < 10) return; if (Math.abs(dx) < Math.abs(dy) * 1.2) { g = null; return; } g.lock = true; g.c.classList.add('drag'); try { g.c.setPointerCapture(e.pointerId); } catch { } }
    g.dx = dx; g.c.style.transform = `translate3d(${dx}px,0,0) rotate(${dx / g.c.clientWidth * 4}deg)`;
    g.c.dataset.sw = dx < -30 ? 'l' : dx > 30 ? 'r' : '';
  });
  const up = e => {
    if (!g || (e && e.pointerId !== g.id)) return;
    const { c, dx, lock } = g; g = null; c.classList.remove('drag'); c.dataset.sw = '';
    if (lock && Math.abs(dx) > c.clientWidth * .3) { act(dx < 0 ? 'sleep' : 'keep', dx < 0 ? 'l' : 'r'); return; }
    c.style.transform = '';
  };
  f.addEventListener('pointerup', up); f.addEventListener('pointercancel', up);
  show();
}

// ---------- the asleep chat: the handoff note above Continue (find.js) ----------
export async function noteInto(w, sid) {
  if (!w || $('.fnote', w)) return;
  try {
    const r = await api('tidy/note/' + enc(sid));
    if (!r?.note || !w.isConnected || $('.fnote', w)) return;
    const lines = r.note.split('\n').map(l => { const m = l.match(/^(\w+):\s*(.*)$/); return m ? `<div><b>${esc(m[1])}</b> ${esc(m[2])}</div>` : `<div>${esc(l)}</div>`; }).join('');
    const n = h(`<div class="fnote"><span class="fn-h">${IC.doc}Note from when it went to sleep</span>${lines}</div>`);
    const at = $('.wb', w); at ? at.before(n) : w.append(n);
  } catch { }
}
export const _test = { keepWhy, cardHTML, rowHTML };
