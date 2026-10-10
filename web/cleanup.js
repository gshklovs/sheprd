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
// A4  "Just put to sleep": for 6 h after a sleep,
//     a card under Recent lists them with one-tap Wake (resume in its old workspace and tab, no message sent) and
//     Wake all, until Hide. The sheet says what Sleep does under the button, with a "What does sleep do?" expander.
// C3  Catch up from Home (server/src/catchup.rs, API.md "Catch up"): "Catch up on N agents waiting on you" under the
//     Decide card whenever an agent's last message waits on the user (Tidy's per-turn verdict), minus agents with an open
//     decision (those are the Decide deck's). The same cards with Answer (no promise to sleep), Continue, Sleep, Keep.
//     Every card (and the asleep wake card, Just put to sleep) shows "You said" (the user's last prompt, 3 lines, tap to
//     expand; "Last from you" + "then <agent> sent…" when an agent wrote after them) and "Waiting on you for: …".
import { G, $, h, esc, api, post, toast, haptic, tick, sheet, shortPath, ls, whoName, HOME_NAME } from './app.js';
import { md } from './md.js';
import { favHTML, colCls, forget, agents, cached, nameOf, MOON } from './find.js';

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
  go: '<svg viewBox="0 0 24 24"><path d="M7 4.5v15l12-7.5z"/></svg>',
  chev: '<svg viewBox="0 0 24 24"><path d="M9 5l7 7-7 7"/></svg>',
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
const clip = (s, n) => { s = fold(s); return s.length > n ? s.slice(0, n - 1).trimEnd() + '…' : s; };

// ---------- "You said" and "Waiting on you for" (catch-up cards, the asleep wake card, Just put to sleep) ----------
/** the user's last prompt to it: 3 lines, tap to expand. After an agent's message: "Last from you" + "then X sent…". */
export function saidHTML(s, { row = false } = {}) {
  if (!s || (!s.text && !s.then)) return '';
  const lab = s.then ? 'Last from you' : 'You said', when = s.ts ? ` · ${ago(Date.now() - s.ts)} ago` : '';
  const then = s.then ? `<div class="cs-then">then <b>${esc(whoName(s.then.from) || s.then.from)}</b> sent… <span>“${esc(clip(s.then.text, row ? 60 : 110))}”</span></div>` : '';
  if (row) return `<span class="cs-row">${s.text ? `<button type="button" class="cu-said cs-r" aria-expanded="false"><span class="cs-t"><em>${lab}:</em> ${esc(s.text.trim())}</span></button>` : ''}${then}</span>`;
  return `<div class="cs">`
    + (s.text ? `<button type="button" class="cu-said" aria-expanded="false"><span class="cs-t"><em>${lab}<i>${when}</i></em> ${esc(s.text.trim())}</span></button>` : `<div class="cu-said"><em>${lab}</em></div>`) + then + '</div>';
}
export const waitHTML = w => w ? `<button type="button" class="cu-wait" aria-expanded="false"><span><b>Waiting on you for:</b> ${esc(w)}</span></button>` : '';
// tap a clamped "You said", the wait line or the question to read all of it (capture: inside a row link it must not
// open the chat; a link inside the question still works)
document.addEventListener('click', e => {
  const b = e.target.closest('.cu-said, .cu-wait, .cu-q'), l = e.target.closest('a'); if (!b || (l && b.contains(l))) return;
  e.preventDefault(); e.stopPropagation();
  const o = b.classList.toggle('open'); b.setAttribute('aria-expanded', o);
}, true);
/** sid → {said, wait, at}: asleep agents' last prompt (GET /api/catchup/said), fetched in one batch. */
const SAID = new Map();
let SAIDQ = null;
export function saidFor(sids) {
  const want = sids.filter(s => !(SAID.get(s)?.at > Date.now() - 5 * 60e3));
  if (!want.length) return Promise.resolve(SAID);
  if (SAIDQ) return SAIDQ.then(() => saidFor(sids));
  return SAIDQ = api('catchup/said?sids=' + want.slice(0, 20).map(encodeURIComponent).join(',')).then(r => {
    for (const s of want.slice(0, 20)) SAID.set(s, { said: r.said?.[s] || null, wait: r.wait?.[s] || null, at: Date.now() });
    return SAID;
  }).finally(() => { SAIDQ = null; });
}
/** The asleep chat's wake card (find.js): what the user last said to it and what it waits on, above the buttons. */
export async function saidInto(w, sid) {
  if (!w || $('.wsaid', w)) return;
  try {
    const x = (await saidFor([sid])).get(sid);
    if (!x || !w.isConnected || $('.wsaid', w)) return;
    const html = waitHTML(x.wait) + saidHTML(x.said);
    if (html) $('.w1', w).after(h(`<div class="wsaid">${html}</div>`));
  } catch { }
}

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
  const sub = `${p.note ? 'Notes saved · ' : ''}Chat${n === 1 ? '' : 's'} kept · wake ${n === 1 ? 'it' : 'them'} any time`;
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
  if (!T || Date.now() - TAT > 30e3) load().then(() => { if (G.view?.el === el) { drawCard(el, G.st); drawJust(el, G.st); } }, () => { });
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
  const j = $('.tdy-just', el); if (j && j.previousElementSibling !== box) box.after(j);
  const n = safe.length + check.length, faces = safe.slice(0, 6), more = n - faces.length;
  const sub = check.length ? `${safe.length} ${safe.length === 1 ? 'is' : 'are'} done with nothing left · ${check.length} need${check.length === 1 ? 's' : ''} a look first` : `${safe.length === 1 ? 'It is' : 'All are'} done with nothing left`;
  const html = `<div class="tc-top"><span class="tc-ic">${IC.broom}</span><div class="tc-tx"><b>${plural(n, 'agent')} ${n === 1 ? 'is' : 'are'} idle</b><small>${sub}</small></div></div>`
    + `<div class="tc-faces">${faces.map(r => `<span class="tcf${colCls(r.color)}">${face(r, 30)}</span>`).join('')}${more > 0 ? `<span class="tc-more">+${more}</span>` : ''}</div>`
    + '<div class="tc-btns"><button class="btn primary tc-go">Tidy up</button><button class="btn tc-no">Not now</button></div>';
  if (box._h !== html) { box._h = html; box.innerHTML = html; }
}

// ---------- C3: Catch up from Home ----------
let C = null, CAT = 0, CLOAD = null, CSIG = '';
/** sid → question key acted on here (Answer / Continue / Sleep / Keep): off the entry until the server catches up. */
const ACTED = new Map();
function loadCatch(force) {
  if (!force && C && Date.now() - CAT < 15e3) return Promise.resolve(C);
  CLOAD ??= api('catchup').then(r => { C = r; CAT = Date.now(); return r; }).finally(() => { CLOAD = null; });
  return CLOAD;
}
/** Agents whose last message waits on the user, minus open decisions (the server leaves those to Decide) and what the user just did. */
export const catchRows = () => (C?.rows || []).filter(r => ACTED.get(r.sid) !== r.key);
// a turn ending, starting or a decision closing can change the list: fetch again then
const csig = st => (st?.agents || []).map(a => `${a.pane}:${a.status}:${a.completionSeq}`).join('|') + '|' + (st?.decisions?.open || 0) + '|' + (st?.decisions?.stale || 0);
/** Called from home.js draw: one terse row under the Decide card (else under the briefing), above the hero. */
export function drawCatch(el, st) {
  const sig = csig(st);
  if (sig !== CSIG || !C || Date.now() - CAT > 15e3) {
    const force = sig !== CSIG; CSIG = sig;
    loadCatch(force).then(() => { if (G.view?.el === el) drawCatch(el, G.st); }, () => { });
  }
  let box = $('.cu-entry', el);
  const rows = catchRows(), n = rows.length;
  if (!n) { box?.remove(); return; }
  if (!box) {
    box = h('<button type="button" class="cu-entry"></button>');
    box.onclick = () => { haptic(); const l = catchRows(); if (l.length) catchUp(l, 0, { home: true }); };
  }
  const d = $('.dwrap', el), after = d && !d.hidden ? d : $('.brief', el);
  if (after && box.previousElementSibling !== after) after.after(box);
  const r0 = rows[0], what = r0.wait || r0.reasons?.find(x => x.kind === 'q')?.text || '';
  const html = `<span class="ce-tx"><b>Catch up on ${plural(n, 'agent')} waiting on you</b>`
    + `<span class="ce-s"><span class="ce-f">${rows.slice(0, 3).map(r => `<span class="tcf${colCls(r.color)}">${face(r, 20)}</span>`).join('')}</span><small>${esc(r0.name)}${what ? ': ' + esc(what) : ''}</small></span></span><span class="ce-go">${IC.chev}</span>`;
  if (box._h !== html) { box._h = html; box.innerHTML = html; }
}

// ---------- what Sleep does: one line under the button + an expander ----------
const WHAT = `<details class="ts-what"><summary><span>Closes their tabs. Chats are kept; wake any time from Asleep.</span> <u>What does sleep do?</u></summary>`
  + '<ul><li><b>Closes its tab</b> in herdr. Anything running only in that tab stops (a dev server, a background shell).</li>'
  + '<li><b>Keeps the whole conversation</b> and the 5-line note if it’s on. Nothing is deleted.</li>'
  + '<li><b>Wake it any time</b> from Agents › Asleep or search: it reopens in its old workspace and tab and picks up where it left off.</li>'
  + '<li>Changed your mind? Undo for 8 s, then Wake from “Just put to sleep” at the top of Agents.</li></ul></details>';
/** The tab name it had when that is not its display name ("api" for "Fix Login Bug"): the user knows agents by
 *  their herdr tab too, so Tidy shows both. */
export const tabOf = r => { const t = String(r?.tab || '').trim(), k = x => String(x || '').toLowerCase().replace(/[^a-z0-9]/g, ''); return t && !k(r.name).includes(k(t)) && !k(t).includes(k(r.name)) ? t : ''; };
const withTab = r => tabOf(r) ? `${r.name} (${tabOf(r)})` : r.name;
/** "Docs Site, Sheprd Setup and Nightly Report" / "Docs Site, Sheprd Setup +4 more" */
export function namesLine(names) {
  if (names.length <= 1) return names[0] || '';
  if (names.length <= 3) return names.slice(0, -1).join(', ') + ' and ' + names.at(-1);
  return `${names.slice(0, 2).join(', ')} +${names.length - 2} more`;
}

// ---------- A2: the review sheet ----------
const rowHTML = (r, sec, on) => `<div class="tr${colCls(r.color)}" data-pane="${esc(r.pane)}" data-sec="${sec}">`
  + `<button class="tck${on ? ' on' : ''}" aria-label="${on ? 'Selected' : 'Not selected'}" aria-pressed="${on}">${IC.check}</button>${face(r, 34)}`
  + `<div class="tb"><div class="t1"><b class="nm">${esc(r.name)}${tabOf(r) ? ` <span class="ttab">tab ${esc(tabOf(r))}</span>` : ''}</b><span class="tm">${ago(r.idleMs)}</span></div>`
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
    <div class="ts-body"></div>`, 'tall tdy-sheet');
  const body = $('.ts-body', sh), shEl = $('.sheet', sh);
  const foot = h(`<div class="ts-foot"><label class="tsw"><input type="checkbox"${noteOn() ? ' checked' : ''}><i></i><span>Save a 5-line note for each first</span></label>
    <div class="ts-btns"><button class="btn primary tf-go"></button>${WHAT}<button class="btn tf-next" hidden></button></div></div>`);
  shEl.append(foot);
  let ARM = 0, ARMT = 0;
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
    const picked = [...tt.safe, ...tt.check].filter(r => on.has(r.pane));
    const gh = n ? `<b>Put ${n} to sleep</b><small>${esc(namesLine(picked.map(withTab)))}</small>` : '<b>Nothing selected</b>';
    if (go._h !== gh) { go._h = gh; go.innerHTML = gh; }
    go.disabled = !n;
    nx.hidden = !left.length; nx.textContent = n ? `Then catch up on ${left.length} ›` : `Catch up on ${left.length} ›`;
  };
  const disarm = () => { if (!ARM) return; ARM = 0; clearTimeout(ARMT); $('.tf-go', foot).classList.remove('arm'); $('.tf-go', foot)._h = null; };
  draw();
  sh.addEventListener('click', async e => {
    if (e.target.closest('.ts-x')) { sh.close(); return; }
    const tt = T || t;
    if (e.target.closest('.ts-all')) { haptic(); const all = tt.safe.every(r => on.has(r.pane)); tt.safe.forEach(r => all ? on.delete(r.pane) : on.add(r.pane)); disarm(); draw(); return; }
    const row = e.target.closest('.tr');
    if (row) {
      const p = row.dataset.pane;
      if (row.dataset.sec === 'check' && !e.target.closest('.tck')) { // a Look first row opens its catch-up card
        const me = tt.check.find(r => r.pane === p), list = [me, ...tt.check.filter(r => r !== me && !on.has(r.pane))];
        sh.close(); setTimeout(() => catchUp(list, 0), 220); return;
      }
      haptic(); on.has(p) ? on.delete(p) : on.add(p); disarm(); draw(); return;
    }
    const note = $('.tsw input', foot).checked;
    if (e.target.closest('.tf-go') || e.target.closest('.tf-next')) {
      const next = !!e.target.closest('.tf-next'), rows = [...tt.safe, ...tt.check].filter(r => on.has(r.pane)), left = tt.check.filter(r => !on.has(r.pane));
      // more than one tab closes: the first tap names every one of them, the second (within 6 s) puts them to sleep
      if (rows.length > 1 && !(ARM > Date.now())) {
        haptic(); ARM = Date.now() + 6000;
        const go = $('.tf-go', foot); go.classList.add('arm'); go._h = `<b>Close these ${rows.length} tabs?</b><small>${esc(rows.map(withTab).join(', '))}</small><small class="tf-tap">Tap again to put them to sleep</small>`; go.innerHTML = go._h;
        clearTimeout(ARMT); ARMT = setTimeout(() => { ARM = 0; go.classList.remove('arm'); go._h = null; draw(); }, 6000);
        return;
      }
      ARM = 0; clearTimeout(ARMT);
      haptic(); sh.close();
      if (rows.length) await sleepNow(rows, note);
      if (next && left.length) setTimeout(() => catchUp(left, 0), 240);
    }
  });
  $('.tsw input', foot).onchange = e => ls.set('tidyNote', e.target.checked);
}

// ---------- C2: catch-up, one card at a time ----------
function fixHTML(r, home) {
  const q = r.reasons.find(x => x.kind === 'q');
  const fixes = home ? [] : r.reasons.filter(x => x.fix && x.kind !== 'q');
  let out = '';
  if (q) {
    out += q.fix?.reply ? `<button class="cu-sug" aria-label="Suggested reply">${esc(q.fix.reply)}</button>` : '';
    out += `<form class="cu-ans"><input type="text" enterkeyhint="send" autocomplete="off" autocapitalize="sentences" placeholder="${home ? 'Answer…' : 'Answer, then it sleeps…'}"><button class="btn primary" type="submit">Send</button></form>`;
  }
  if (fixes.length) out += `<div class="cu-fixes">${fixes.map(x => `<button class="btn cu-fix" data-k="${esc(x.fix.kind)}" data-text="${esc(x.fix.text || '')}">${esc(x.fix.label || x.fix.kind)}</button>`).join('')}</div>`;
  return out;
}
/** What used to fill the card (context gauge, folder, how sending works): behind a small More. */
function moreHTML(r, home) {
  const q = r.reasons.some(x => x.kind === 'q'), pct = r.ctx?.pct != null ? Math.round(r.ctx.pct) : null;
  const why = home ? (q ? 'Answer goes to it as your message (held while you have a draft there). It stays awake.' : '')
    : (q || r.reasons.some(x => x.fix) ? 'Sent with a promise to sleep: once it’s done and asks nothing new, its pane closes.' : '');
  return `<details class="cu-more"><summary>More</summary><div class="mi"><span>${esc(shortPath(r.cwd || ''))}${tabOf(r) ? ' · tab ' + esc(tabOf(r)) : ''}</span>`
    + (pct != null ? `<span>Context <span class="tn">${pct}%${r.ctx.tokens ? ' · ' + kTok(r.ctx.tokens) : ''}</span></span>` : '')
    + (why ? `<span class="cu-why">${why}</span>` : '') + '</div></details>';
}
/** The compact card : face, name + job, time · Waiting on you for (1 line) · You said (2 lines)
 *  · its question (4 lines, tap = all) · reply chip · composer · fixes · More (folder, context, how it's sent). */
function cardHTML(r, home = false) {
  const q = r.reasons.some(x => x.kind === 'q'), full = r.full || r.last || '';
  const said = saidHTML(r.said), wait = q ? waitHTML(r.wait) : '';
  return `<div class="cu-card${colCls(r.color)}" data-pane="${esc(r.pane)}"><div class="cu-h">${face(r, 36)}<div><div class="h1"><b class="nm">${esc(r.name)}</b><span class="tm">${ago(r.idleMs)}</span></div><small>${esc(r.job || shortPath(r.cwd || ''))}</small></div></div>`
    + wait + (said ? `<div class="cu-sec">${said}</div>` : '')
    + (full ? `<div class="cu-sec"><div class="cu-q" role="button" aria-expanded="false" aria-label="${q ? 'It asked you' : 'Its last message'}">${md(full)}</div></div>` : '')
    + `<div class="cu-chips">${r.reasons.filter(x => x.kind !== 'q').map(chipHTML).join('')}</div>`
    + fixHTML(r, home) + moreHTML(r, home) + '</div>';
}
let FLOW = null;
/** The catch-up flow. `home`: opened from Home's "Catch up on N" (catch-up rows, not Tidy's): Answer goes to the agent
 *  without a promise to sleep, the tiles are Continue · Sleep · Keep (until it asks something new). */
export function catchUp(list, start = 0, { home = false } = {}) {
  if (!list?.length) return;
  FLOW?.remove();
  const tiles = home
    ? `<button data-t="cont">${IC.go}<b>Continue</b><small>it carries on</small></button><button data-t="sleep">${MOON}<b>Sleep</b><small>close its tab</small></button><button data-t="keep">${IC.pin}<b>Keep</b><small>till it asks again</small></button>`
    : `<button data-t="sleep">${MOON}<b>Sleep</b><small></small></button><button data-t="note">${IC.doc}<b>Note + sleep</b><small>5 lines saved</small></button><button data-t="keep">${IC.pin}<b>Keep</b><small>skip till next time</small></button>`;
  const f = FLOW = h(`<div class="tdy-flow${home ? ' cu-home' : ''}"><header class="cu-bar"><button class="ib cu-back" aria-label="Back">${IC.back}</button><b>${home ? 'Catch up' : 'Clean up'}</b><span class="cu-step"></span><span class="sp"></span><button class="cu-close">Close</button><div class="cu-prog"></div></header>
    <div class="cu-sc"><div class="cu-slot"></div>
    <div class="cu-tiles">${tiles}</div>
    <div class="cu-foot"></div></div></div>`);
  document.body.append(f);
  requestAnimationFrame(() => f.classList.add('in'));
  let k = start, busy = false;
  const done = new Set();
  const close = () => {
    f.classList.remove('in'); setTimeout(() => f.remove(), 260); if (FLOW === f) FLOW = null; T = null; load(true).then(redraw, () => { });
    if (home) loadCatch(true).then(redraw, () => { });
  };
  const show = dir => {
    const r = list[k];
    $('.cu-prog', f).innerHTML = list.map((_, i) => `<i class="${i < k ? 'f' : i === k ? 'cur' : ''}"></i>`).join('');
    $('.cu-step', f).textContent = home ? `${k + 1} of ${list.length} waiting on you` : `${k + 1} of ${list.length} to look at`;
    const slot = $('.cu-slot', f), n = h(cardHTML(r, home));
    slot.querySelectorAll('.go-l, .go-r').forEach(x => x.remove()); // a quick tap: the one before is still leaving
    const old = slot.querySelector('.cu-card');
    if (old && dir) { old.classList.add('go-' + dir); setTimeout(() => old.remove(), 300); n.classList.add('enter'); slot.append(n); requestAnimationFrame(() => requestAnimationFrame(() => n.classList.remove('enter'))); }
    else { slot.innerHTML = ''; slot.append(n); }
    tick(n);
    const cq = $('.cu-q', n); if (cq) requestAnimationFrame(() => cq.classList.toggle('long', cq.scrollHeight > cq.clientHeight + 2));
    $('[data-t="sleep"] small', f).textContent = r.reasons.some(x => x.kind === 'q') ? 'drop the question' : 'close its tab';
    const rest = list.slice(k + 1).map(x => x.name);
    $('.cu-foot', f).innerHTML = `Sleep closes its tab (chat kept, wake from Asleep). Swipe ← sleep · → keep.${rest.length ? `<br>Next: ${rest.map(esc).join(' · ')}` : ''}`;
    $('.cu-sc', f).scrollTop = 0;
  };
  const next = dir => { done.add(list[k].pane); if (home) { ACTED.set(list[k].sid, list[k].key); redraw(); } busy = false; if (k + 1 >= list.length) { close(); toast('All caught up', 'ok', null, 1400); return; } k++; show(dir); };
  /** Home: Answer / Continue through POST /api/catchup/send (awake: the steer path, held on a draft or menu; asleep: resume). */
  const send = async (kind, text) => {
    const r = list[k];
    const x = await post('catchup/send', { sid: r.sid, pane: r.pane, text, kind });
    const held = x?.held ? ' · held for your draft there' : '';
    toast(kind === 'continue' ? `${esc(r.name)} carries on${held}` : `Sent to ${esc(r.name)}${held}`, 'ok', null, 1600);
  };
  const act = async (t, dir) => {
    if (busy) return; busy = true; haptic();
    const r = list[k];
    if (t === 'keep') {
      try { await post(home ? 'catchup/keep' : 'tidy/snooze', { pane: r.pane }); } catch (e) { toast(esc(e.message), 'err'); busy = false; return; }
      toast(home ? `Kept ${esc(r.name)} · back when it asks something new` : `Kept ${esc(r.name)} till tomorrow`, 'ok', null, 1400);
    } else if (t === 'cont') {
      try { await send('continue', 'continue'); } catch (e) { toast('Couldn’t send: ' + esc(e.message), 'err'); busy = false; return; }
    } else if (!(await sleepNow([r], home ? noteOn() : t === 'note'))) { busy = false; return; }
    next(dir || (t === 'sleep' || t === 'note' ? 'l' : 'r'));
  };
  const fix = async (kind, text) => {
    if (busy) return; busy = true; haptic();
    const r = list[k];
    if (home) {
      try { await send('answer', text); next('r'); } catch (e) { toast('Couldn’t send: ' + esc(e.message), 'err'); busy = false; }
      return;
    }
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
  f.addEventListener('focusin', e => { if (!e.target.closest('.cu-ans input')) return; f.classList.add('kb'); setTimeout(() => $('.cu-ans', f)?.scrollIntoView({ block: 'nearest' }), 320); });
  f.addEventListener('focusout', e => { if (e.target.closest('.cu-ans input')) setTimeout(() => { if (!f.contains(document.activeElement) || !document.activeElement.matches('.cu-ans input')) f.classList.remove('kb'); }, 50); });
  f.addEventListener('submit', e => { e.preventDefault(); const v = $('.cu-ans input', f)?.value.trim(); if (v) { $('.cu-ans input', f).blur(); fix('answer', v); } });
  // swipe the card: left = sleep, right = keep
  let g = null;
  f.addEventListener('pointerdown', e => { const c = e.target.closest('.cu-card'); if (!c || e.target.closest('input, button, a, form, .cu-q pre')) return; g = { c, x: e.clientX, y: e.clientY, id: e.pointerId, lock: false, dx: 0 }; });
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

// ---------- A4: Just put to sleep: one-tap Wake for 6 h (or until Hide) ----------
const WAKING = new Map(); // sid → when Wake was tapped (the row says Waking… until its pane reports the session)
const clock = t => new Date(t).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
/** Agents Tidy put to sleep in the last `justMs` (after the last Hide) that are still asleep, newest first. */
function justList(st) {
  const L = cached();
  if (!T || !L) return [];
  const since = Math.max(Date.now() - (T.justMs || 6 * 3600e3), T.justHidden || 0);
  const live = new Set((st?.agents || []).map(a => a.session).filter(Boolean));
  const at = new Map();
  for (const [sid, t] of SLEPT) if (t > since) at.set(sid, t);
  for (const [sid, t] of Object.entries(T.slept || {})) if (t > since) at.set(sid, Math.max(t, at.get(sid) || 0));
  for (const [sid, t] of WAKING) if (Date.now() - t > 90e3) WAKING.delete(sid);
  const seen = new Set();
  return L.agents.filter(a => a.asleep && at.has(a.sid) && !live.has(a.sid) && !seen.has(a.sid) && seen.add(a.sid)).map(a => ({ ...a, sleptAt: at.get(a.sid) })).sort((a, b) => b.sleptAt - a.sleptAt);
}
/** Session ids shown under "Just put to sleep" (find.js leaves them out of Asleep). */
export const justSids = st => new Set(justList(st).map(a => a.sid));
/** Called from find.js drawAgents: the card under the Tidy card (else under Recent). */
export function drawJust(el, st) {
  let box = $('.tdy-just', el);
  if (box && !cached()) return; // the agent list is reloading (after a wake): keep the card as it is, no flicker
  const list = justList(st);
  if (!list.length) { box?.remove(); return; }
  if (!box) { box = h('<section class="tdy-just"></section>'); box.addEventListener('click', onJust); }
  const after = $('.tdy-card', el) || $('.frec', el) || $('.brief', el);
  if (after && box.previousElementSibling !== after) after.after(box);
  const n = list.length, waking = list.filter(a => WAKING.has(a.sid)).length;
  if (list.some(a => !SAID.has(a.sid))) saidFor(list.map(a => a.sid)).then(() => { if (box.isConnected) drawJust(el, G.st); }, () => { });
  const said = a => saidHTML(SAID.get(a.sid)?.said, { row: true });
  const html = `<div class="tj-h"><span class="tj-ic">${MOON}</span><div class="tj-tx"><b>Just put to sleep</b><small>${plural(n, 'agent')} · ${esc(clock(Math.min(...list.map(a => a.sleptAt))))} · chats kept</small></div><button class="tj-hide">Hide</button></div>`
    + `<div class="tj-list">${list.map(a => `<div class="tj-row${colCls(a.color)}" data-sid="${esc(a.sid)}">${favHTML(a, { sz: 34 })}`
      + `<a class="tj-b" href="#/t/${enc(a.sid)}"><b class="nm">${esc(nameOf(a))}</b><small>${tabOf({ ...a, name: nameOf(a) }) ? `tab ${esc(a.tab)} · ` : ''}${esc(a.job || shortPath(a.cwd || ''))}</small>${said(a)}</a>`
      + (WAKING.has(a.sid) ? '<button class="btn cont tj-wake" disabled><span class="spin"></span>Waking</button>' : `<button class="btn cont tj-wake" data-sid="${esc(a.sid)}">Wake</button>`) + '</div>').join('')}</div>`
    + (n - waking > 1 ? `<button class="btn tj-all">Wake all ${n - waking}</button>` : '')
    + '<p class="tj-why">Wake reopens it in its old workspace and tab, right where it left off. Nothing is sent to it.</p>';
  if (box._h !== html) { box._h = html; box.innerHTML = html; }
}
/** Resume each (POST /api/sessions/<sid>/resume, no message: it comes back idle), one after another. */
async function wake(list) {
  haptic();
  list.forEach(a => WAKING.set(a.sid, Date.now()));
  redraw();
  const bad = [], up = [];
  for (const a of list) {
    try { // the server never starts a second Claude: a session already running somewhere comes back `already`
      const r = await post(`sessions/${enc(a.sid)}/resume`, { wake: true });
      if (r.already) up.push({ a, pane: r.pane }); else if (!r.live) G.starting.set(r.pane, Date.now());
    } catch (e) { WAKING.delete(a.sid); bad.push(`${nameOf(a)}: ${e.message}`); }
  }
  const ok = list.length - bad.length - up.length;
  if (list.length === 1 && up.length) { toast(`${esc(nameOf(list[0]))} is already awake`, 'ok', null, 1600); location.hash = '#/a/' + enc(up[0].pane); }
  else if (up.length) toast(`${esc(up.map(x => nameOf(x.a)).join(', '))} ${up.length === 1 ? 'is' : 'are'} already awake`, 'ok', null, 2200);
  if (ok) toast(ok === 1 && list.length === 1 ? `Waking ${esc(nameOf(list[0]))} in its old tab…` : `Waking ${ok} in their old tabs…`, 'ok', null, 1800);
  if (bad.length) toast('Couldn’t wake ' + esc(bad.join(' · ')), 'err');
  forget(); agents(true).then(redraw, () => { });
}
function onJust(e) {
  const box = e.currentTarget;
  const list = justList(G.st).filter(a => !WAKING.has(a.sid));
  const w = e.target.closest('.tj-wake[data-sid]');
  if (w) { e.preventDefault(); const a = list.find(x => x.sid === w.dataset.sid); if (a) wake([a]); return; }
  if (e.target.closest('.tj-all')) { if (list.length) wake(list); return; }
  if (e.target.closest('.tj-hide')) {
    haptic(); box.classList.add('out'); setTimeout(() => box.remove(), 260);
    if (T) T.justHidden = Date.now();
    post('tidy/just/hide', {}).then(r => { if (T) T.justHidden = r.justHidden; toast('Hidden · they’re still in Asleep', 'ok', null, 1400); }, () => { });
  }
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
export const _test = { keepWhy, cardHTML, rowHTML, namesLine, justList, tabOf, saidHTML, catchRows };
