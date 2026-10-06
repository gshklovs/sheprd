// Home "Briefing": the top of the Agents view.
// 1. A live sentence built from a template, at most 3 clauses plus "+N more": needs-you first, then progress
//    (job %, task list), then finished turns (with Haiku's outcome line), then the most interesting activities
//    (state `activity.phrase`, server/src/activity.rs), then a recent service run ("backup ran 2m ago").
// 2. The hero: the Needs-you queue's top item (queue.js items/spec/actions), answered in place. Acting holds the
//    item and the next one slides in; swipe right = primary action, left = snooze 15 min. Tapping the card's body
//    opens the agent's chat at the item (there is no deck any more).
//    With nothing open it turns into a calm "All clear · N working" line.
// 3. In flight: one tile per working agent / running background job, with live progress: a job's own log
//    (tqdm "63%|", "945/1500 [", "iter 945/1500") gives % and "done ≈ 2:55"; an agent's task list (state todo)
//    gives a segmented bar; otherwise an indeterminate bar and the now-doing line.
// app.js keeps the rows (Ready, Everyone, Quiet), swipes, long-press and the keyed morph.
import { G, $, h, esc, morph, patchHTML, tick, agentHref, dispStatus, longPress, lazy, MODS, nav } from './app.js';

(() => { const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'home.css'; document.head.append(l); })();

const COLORS = ['red', 'orange', 'yellow', 'green', 'cyan', 'blue', 'purple', 'pink'];
const colCls = c => COLORS.includes(c) ? ' col-' + c : '';
const nm = a => a.home ? 'Master' : a.title || a.name || a.pane;
const one = s => String(s || '').replace(/\*\*|__|`+|^#+\s*/gm, '').replace(/\s+/g, ' ').trim();
const hm = ms => {
  const d = new Date(ms), t = d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
  const days = Math.round((new Date(ms).setHours(0, 0, 0, 0) - new Date().setHours(0, 0, 0, 0)) / 864e5);
  return days <= 0 ? t : days === 1 ? 'tmrw ' + t : d.toLocaleDateString([], { weekday: 'short' }) + ' ' + t;
};
const cmdName = c => one(c).replace(/^(cd [^&;]+(&&|;)\s*)+/, '').replace(/^(python3?|uv run|bash|node)\s+/, '').split(/\s+/).slice(0, 3).join(' ');
const refresh = () => G.view?.onState?.(G.st);

// ---------- job progress from the job's log tail ----------
const PROG = new Map(); // job id → {pct, cur, tot, at}
let progAt = 0, progBusy = false;
export function parseProg(text) {
  const L = String(text || '').split(/[\r\n]+/).filter(l => l.trim()).slice(-60).reverse();
  for (const l of L) {
    let m = l.match(/(\d{1,3}(?:\.\d+)?)%\s*\|/); // tqdm
    if (m) { const n = l.match(/\|\s*(\d+)\s*\/\s*(\d+)\s*\[/); return { pct: +m[1], cur: n ? +n[1] : null, tot: n ? +n[2] : null }; }
    m = l.match(/\b(?:iter(?:ation)?s?|steps?|epochs?|eps?|episodes?|updates?|batch(?:es)?)\b[^\d\n]{0,4}(\d+)\s*(?:\/|of)\s*(\d+)/i) || l.match(/(?:^|\s|\[)(\d+)\s*\/\s*(\d+)\s*\[/);
    if (m && +m[2] > 0 && +m[1] <= +m[2]) return { pct: +m[1] / +m[2] * 100, cur: +m[1], tot: +m[2] };
  }
  return null;
}
async function loadProg(jobs) {
  if (progBusy || Date.now() - progAt < 10e3) return;
  progBusy = true; progAt = Date.now();
  let changed = false;
  try {
    for (const j of jobs) {
      if (!j.out) continue;
      try {
        const r = await fetch('/api/file?path=' + encodeURIComponent(j.out) + '&tail=4000');
        const p = r.ok ? parseProg(await r.text()) : null, old = PROG.get(j.id);
        if (JSON.stringify(p) !== JSON.stringify(old && { pct: old.pct, cur: old.cur, tot: old.tot })) changed = true;
        if (p) PROG.set(j.id, p); else PROG.delete(j.id);
      } catch { }
    }
  } finally { progBusy = false; }
  if (changed && G.view?.el?.classList.contains('agents')) refresh();
}
const etaOf = (j, p) => {
  const el = Date.now() - (j.started || 0);
  if (!p || p.pct < 2 || p.pct >= 100 || el < 60e3) return null;
  return j.started + el / (p.pct / 100);
};

// ---------- activity (server/src/activity.rs: taxonomy rules, the agent's own todo step, or Haiku) ----------
// how interesting a category is for the sentence: concrete outside-world work beats reading and waiting
const ACTW = { needs: 3, browsing: 3, jobs: 3, design: 2.5, coding: 2, docs: 2, data: 2, verifying: 2, git: 1.5, research: 1.5, mail: 1, comms: 1, ops: 1, planning: 1, waiting: .3, thinking: 0 };
const actOf = a => a?.activity?.phrase ? a.activity : null;
const actScore = a => { const x = actOf(a); return x ? (ACTW[x.cat] ?? 1) + (x.src === 'haiku' || x.src === 'todo' ? 2 : 0) + (x.conf || 0) : -1; };
const cap1 = s => s ? s[0].toUpperCase() + s.slice(1) : s;
/** "🧪 Running the tests" for tiles and rows; the raw tool label stays in the title (and in the chat header). */
export const actHTML = a => { const x = actOf(a); return x ? `<span class="aico" aria-hidden="true">${esc(x.icon || '•')}</span>${esc(cap1(x.phrase))}` : ''; };
const ago = ms => { const m = Math.round((Date.now() - ms) / 60e3); return m < 1 ? 'just now' : m < 60 ? `${m}m ago` : `${Math.round(m / 60)}h ago`; };

// ---------- the sentence ----------
const tint = (a, text = nm(a), href = a.pane ? agentHref(a) : '') => href ? `<a class="bn${colCls(a.color)}" href="${href}">${esc(text)}</a>` : `<b class="bn${colCls(a.color)}">${esc(text)}</b>`;
function briefHTML(st, hero, tiles, ready, failing) {
  const C = [];
  const needs = [...new Map(hero.map(x => [x.pane, x.a])).values()];
  const href0 = hero[0] && MODS.queue.hrefOf(hero[0]);
  // an item can name what it needs ("Build agent needs you: approve the migration.")
  const say = hero[0]?.say ? `: ${esc(hero[0].say.replace(/[.!]+$/, ''))}` : '';
  if (needs.length) C.push({ s: true, h: needs.length === 1 ? `${tint(needs[0], nm(needs[0]), href0)} needs you${say}.` : `${tint(needs[0], nm(needs[0]), href0)}${say ? ` needs you${say};` : ''} and ${needs.length - 1} more need you.` });
  for (const a of failing) C.push({ h: `${tint(a, a.service?.name || nm(a))} is failing` });
  const plain = [];
  for (const t of tiles) {
    const x = !t.j && actOf(t.a);
    if (t.j && t.p) C.push({ h: `${tint(t.a || { color: null, pane: '' }, t.title)} is at ${Math.round(t.p.pct)}%` });
    else if (!t.j && t.a.todo?.total) {
      const n = `${Math.min(t.a.todo.done + 1, t.a.todo.total)} of ${t.a.todo.total}`;
      C.push({ h: x ? `${tint(t.a)} is ${esc(x.phrase)} (${n})` : `${tint(t.a)} is on ${n}` });
    } else plain.push(t);
  }
  // finished: the newest one with an outcome line says what it did
  if (ready.length) {
    const r = ready.find(a => a.outcome?.text) || ready[0];
    const did = r.outcome?.text ? `${tint(r)} ${esc(one(r.outcome.text))}` : `${tint(r)} finished`;
    C.push({ h: ready.length > 1 ? `${did}, and ${ready.length - 1} more finished` : did });
  }
  // working without progress: one clause each, most interesting first (the cap turns the rest into "+N more")
  plain.sort((a, b) => (b.j ? 1.5 : actScore(b.a)) - (a.j ? 1.5 : actScore(a.a)));
  for (const t of plain) {
    const x = !t.j && actOf(t.a);
    C.push({ h: t.j ? (t.a ? `${tint(t.a, t.title)} is running` : `${esc(t.title)} is running`) : `${tint(t.a, t.title)} is ${x ? esc(x.phrase) : 'working'}` });
  }
  // room left: the latest background-service run ("backup ran 2m ago")
  if (C.filter(c => !c.s).length < 3) {
    const sv = (st?.agents || []).filter(a => a.service && !a.service.error && !a.service.stale && a.service.lastRun && Date.now() - a.service.lastRun < 15 * 60e3)
      .sort((a, b) => b.service.lastRun - a.service.lastRun)[0];
    if (sv && C.length) C.push({ h: `${tint(sv, sv.title || sv.service.name)} ${esc(sv.service.verb || 'ran')} ${ago(sv.service.lastRun)}`, svc: true });
  }
  if (!C.length) return '<span class="bcalm">Nothing needs you.</span>';
  const shown = C.slice(0, 3), more = C.slice(3).filter(c => !c.svc).length;
  let out = '', rest = shown;
  if (shown[0].s) { out = shown[0].h + ' '; rest = shown.slice(1); }
  if (rest.length) {
    out += (rest.length === 1 ? rest[0].h : rest.slice(0, -1).map(c => c.h).join(', ') + ', and ' + rest.at(-1).h) + '.';
  }
  return out + (more > 0 ? ` <span class="bmore">+${more} more</span>` : '');
}

// ---------- hero ----------
const HERO = { box: null, it: null, n: 0, drag: null };
function heroHTML(it, n, idx) {
  const tr = MODS.queue, sp = tr.spec(it), acts = sp.acts.filter(Boolean), a = it.a;
  const p = a.ctx?.pct != null ? Math.round(a.ctx.pct) : null;
  const par = a.parent?.pane && G.st?.agents.find(x => x.pane === a.parent.pane);
  const btn = (x, k) => x.reply != null
    ? `<button class="hb${k ? '' : ' pri'}" data-reply="${esc(x.reply)}">${esc(x.label)}</button>`
    : `<button class="hb${k ? '' : ' pri'}${x.cls ? ' ' + x.cls : ''}" data-hact="${k}"${x.wait ? ' disabled' : ''}>${x.wait ? '<span class="spin"></span>' : ''}${esc(x.label)}</button>`;
  const box = sp.surf ? ` data-sfkey="${esc(tr.surfKey(sp.surf))}" data-pane="${esc(it.pane)}"` : '';
  const md = sp.md ? one(sp.md) : '';
  const dots = n > 1 ? `<span class="hpg" aria-label="${idx + 1} of ${n}">${n <= 5 ? Array.from({ length: n }, (_, k) => `<i${k === idx ? ' class="on"' : ''}></i>`).join('') : `<span>${idx + 1}/${n}</span>`}</span>` : '';
  const rep = it.kind === 'phone' || it.kind === 'surf';
  return `<div class="h-top"><span class="cring s-${dispStatus(a)}" style="--p:${p ?? 0}">${p ?? ''}</span>
      <a class="h-tt" href="${MODS.queue.hrefOf(it)}"><b class="tt">${esc(it.nm)}</b><small><b>${esc(sp.pill)}</b> · <span data-since="${it.ts || a.since}"></span>${par ? ` · ↳ ${esc(par.home ? 'Master' : nm(par))}` : ''}</small></a>${dots}</div>
    ${sp.ctx ? `<div class="h-q">${sp.ctx}</div>` : ''}
    ${md ? `<div class="h-md">${esc(md.slice(0, 400))}</div>` : ''}
    ${sp.code ? `<pre class="h-code">${esc(sp.code)}</pre>` : ''}
    <div class="h-acts${acts.length > 2 ? ' n3' : ''}"${box}>${acts.map(btn).join('')}</div>
    ${rep ? `<form class="h-rep"><input type="text" enterkeyhint="send" autocomplete="off" autocapitalize="sentences" placeholder="Reply to ${esc(it.nm)}…"><button type="submit" aria-label="Send"><svg viewBox="0 0 24 24"><path d="M12 19V5M5.5 11.5L12 5l6.5 6.5"/></svg></button></form>` : ''}`;
}
function calmHTML(st, working) {
  const sn = MODS.queue?.snoozed() || 0;
  return `<div class="hcalm"><span class="ok"><svg viewBox="0 0 24 24"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg></span><b>All clear</b><span>· ${working ? `${working} working` : 'nothing running'}</span><span class="sp"></span>${sn ? `<button class="hsn">${sn} snoozed</button>` : ''}</div>`;
}
function drawHero(box, items, working) {
  const it = items[0] || null; HERO.it = it; HERO.n = items.length;
  const cur = box.querySelector(':scope > .hcard:not(.out)');
  if (!it) {
    const html = calmHTML(G.st, working);
    if (cur) { cur.classList.add('out', 'go-l'); setTimeout(() => cur.remove(), 320); }
    let c = box.querySelector(':scope > .hcalmw');
    if (!c) { c = h('<div class="hcalmw"></div>'); box.append(c); }
    patchHTML(c, html);
    return;
  }
  box.querySelector(':scope > .hcalmw')?.remove();
  const html = heroHTML(it, items.length, 0);
  if (cur && cur.dataset.key === it.key) {
    if (!cur.contains(document.activeElement) && !HERO.drag && patchHTML(cur, html)) tick(cur);
    return;
  }
  const n = h(`<section class="hcard${colCls(it.a.color)}${it.kind === 'surf' ? ' k-surf' : ''}" data-key="${esc(it.key)}" data-pane="${esc(it.pane)}"></section>`);
  n.innerHTML = html; tick(n);
  if (cur) {
    const dir = cur.dataset.gone || 'l';
    cur.style.transform = ''; cur.classList.add('out', 'go-' + dir);
    setTimeout(() => cur.remove(), 340);
    n.classList.add('in'); box.prepend(n);
    requestAnimationFrame(() => requestAnimationFrame(() => n.classList.remove('in')));
  } else box.prepend(n);
  if (it.kind === 'plan' || it.kind === 'menu') MODS.queue.loadMenu(it);
}
function wireHero(box) {
  HERO.box = box;
  const cur = () => HERO.it, card = () => box.querySelector(':scope > .hcard:not(.out)');
  const mark = d => { const c = card(); if (c) c.dataset.gone = d; };
  box.addEventListener('click', e => {
    const tr = MODS.queue; if (!tr) return;
    if (HERO.dragged) { e.preventDefault(); e.stopPropagation(); return; }
    if (e.target.closest('.hsn')) { tr.unsnooze(); return; }
    const it = cur(); if (!it) return;
    const b = e.target.closest('[data-hact]');
    if (b && !b.disabled) { mark('r'); tr.heroAct(it, +b.dataset.hact); return; }
    if (e.target.closest('[data-reply]')) { mark('r'); setTimeout(() => { tr.hold(it.key); refresh(); }, 0); return; } // app.js sends it
    if (e.target.closest('a, button, input, form, pre')) return;
    e.preventDefault(); nav(tr.hrefOf(it)); // the card's body opens the chat at the item
  });
  box.addEventListener('submit', async e => {
    e.preventDefault(); const it = cur(), inp = $('input', e.target); if (!it || !inp?.value.trim()) return;
    inp.blur(); mark('r'); if (!(await MODS.queue.heroReply(it, inp.value))) return;
    inp.value = '';
  });
  // swipe the card: right = primary action, left = snooze (next item)
  box.addEventListener('pointerdown', e => {
    const c = e.target.closest('.hcard:not(.out)'); if (!c || e.button > 0 || e.target.closest('input, form, pre, button, a')) return;
    HERO.drag = { c, x: e.clientX, y: e.clientY, id: e.pointerId, lock: null }; HERO.dragged = false;
  });
  box.addEventListener('pointermove', e => {
    const g = HERO.drag; if (!g || e.pointerId !== g.id) return;
    const dx = e.clientX - g.x, dy = e.clientY - g.y;
    if (!g.lock) {
      if (Math.hypot(dx, dy) < 10) return;
      if (Math.abs(dx) < Math.abs(dy) * 1.2) { HERO.drag = null; return; }
      g.lock = 1; HERO.dragged = true; g.c.classList.add('drag'); try { g.c.setPointerCapture(e.pointerId); } catch { }
    }
    g.dx = dx; g.c.style.transform = `translate3d(${dx}px,0,0) rotate(${dx / g.c.clientWidth * 4}deg)`;
    g.c.style.setProperty('--sr', Math.max(0, Math.min(1, dx / (g.c.clientWidth * .3))));
    g.c.style.setProperty('--sl', Math.max(0, Math.min(1, -dx / (g.c.clientWidth * .3))));
  });
  const end = e => {
    const g = HERO.drag; if (!g || (e && e.pointerId !== g.id)) return;
    HERO.drag = null; g.c.classList.remove('drag'); setTimeout(() => { HERO.dragged = false; }, 350);
    const it = cur(), w = g.c.clientWidth, dx = g.dx || 0;
    g.c.style.removeProperty('--sr'); g.c.style.removeProperty('--sl');
    if (it && g.lock && dx > w * .3) { g.c.dataset.gone = 'r'; if (MODS.queue.heroAct(it, 0)) return; }
    else if (it && g.lock && dx < -w * .3) { g.c.dataset.gone = 'l'; MODS.queue.heroSnooze(it); return; }
    g.c.style.transform = '';
  };
  box.addEventListener('pointerup', end); box.addEventListener('pointercancel', end);
  longPress(box, '.h-top', el => { const a = HERO.it?.a; if (!a) return false; lazy('sheets').then(m => m.cardSheet(a)); });
}

// ---------- In flight tiles ----------
const TILES = new Map(); // key → {el, html}
function tileHTML(t) {
  const { a, j, p, title } = t;
  let big, bar, l1 = '', l2 = '';
  const doing = actOf(a) ? actHTML(a) : a?.doing?.label ? `<span class="dspin"></span>${esc(a.doing.label)}` : '';
  if (j) {
    const eta = etaOf(j, p);
    if (p) { big = `${Math.round(p.pct)}<small>%</small>`; bar = `<div class="pbar"><i style="width:${Math.min(100, p.pct).toFixed(1)}%"></i></div>`; }
    else { big = `<span data-since="${j.started}"></span>`; bar = '<div class="pbar ind"><i></i></div>'; }
    l1 = [p?.cur != null && `${p.cur.toLocaleString()} / ${p.tot.toLocaleString()}`, eta ? `done ≈ ${hm(eta)}` : !p && 'running'].filter(Boolean).join(' · ');
    const gpu = j.res?.gpuUtil != null ? `GPU ${Math.round(j.res.gpuUtil)}%` : '';
    l2 = [a ? '↳ ' + esc(nm(a)) : '', gpu].filter(Boolean).join(' · ') || `<span class="mono">${esc(cmdName(j.cmd))}</span>`;
  } else if (a.todo?.total) {
    const { done, total } = a.todo;
    big = `${done}<small>/${total}</small>`;
    bar = `<div class="pbar seg">${Array.from({ length: Math.min(total, 12) }, (_, k) => { const at = Math.floor(k * total / Math.min(total, 12)); return `<i class="${at < done ? 'f' : at === done ? 'cur' : ''}"></i>`; }).join('')}</div>`;
    l1 = a.todo.cur ? `${actOf(a) ? `<span class="aico" aria-hidden="true">${esc(a.activity.icon || '•')}</span>` : '<span class="dspin"></span>'}${esc(a.todo.cur)}` : doing;
    l2 = `<span data-since="${a.since}"></span>${a.subagents?.running ? ` · ${a.subagents.running} subagent${a.subagents.running > 1 ? 's' : ''}` : ''}`;
  } else {
    big = `<span data-since="${a.since}"></span>`; bar = '<div class="pbar ind"><i></i></div>';
    l1 = doing || 'Working';
    l2 = [esc(a.wsLabel || a.ws || ''), a.subagents?.running ? `${a.subagents.running} subagent${a.subagents.running > 1 ? 's' : ''}` : ''].filter(Boolean).join(' · ');
  }
  return `<a class="ftile${colCls(a?.color)}${j ? ' fjob' : ''}"${a ? ` href="${agentHref(a)}${j?.i != null ? '?i=' + j.i : ''}" data-pane="${esc(a.pane)}"` : ''} data-tile="${esc(t.key)}"${j ? ` data-job="${esc(j.id)}"` : ''}>
    <b class="tt">${esc(title)}</b><div class="fbig">${big}</div>${bar}<small class="l1${l1.includes('aico') ? ' doing aphr' : l1.includes('dspin') ? ' doing' : ''}"${a?.doing?.label ? ` title="${esc(a.doing.label)}"` : ''}>${l1}</small><small class="l2">${l2}</small></a>`;
}
const place = (box, nodes) => {
  let prev = null;
  for (const n of nodes) { const want = prev ? prev.nextSibling : box.firstChild; if (want !== n) box.insertBefore(n, want); prev = n; }
  let x; while ((x = prev ? prev.nextSibling : box.firstChild)) x.remove();
};
function drawTiles(box, tiles) {
  const live = new Set(), nodes = [];
  for (const t of tiles) {
    const html = tileHTML(t); live.add(t.key);
    let c = TILES.get(t.key);
    if (!c) { c = { el: h(html), html }; tick(c.el); TILES.set(t.key, c); }
    else if (c.html !== html) { const n = h(html); tick(n); c.el = morph(c.el, n); c.html = html; }
    nodes.push(c.el);
  }
  for (const k of TILES.keys()) if (!live.has(k)) TILES.delete(k);
  place(box, nodes);
}

// ---------- entry (app.js Agents view render) ----------
// o: { list: visible agents (sorted, no quiet, no quiet services), jobs: [/api/jobs], failing: [service agents in error] }
// → { hero: pane of the hero item | null, tiles: Set of panes drawn as tiles }
let wired = null, lastJobs = [];
export function draw(el, st, o) {
  lastJobs = o.jobs || [];
  const brief = $('.brief', el), heroBox = $('.hero', el), tilesBox = $('.tiles', el), tilesSec = $('.tsec', el);
  if (wired !== el) { wired = el; wireHero(heroBox); tilesBox.addEventListener('click', e => { const t = e.target.closest('.ftile.fjob:not([href])'); if (t) { e.preventDefault(); const j = lastJobs.find(x => x.id === t.dataset.job); if (j) lazy('sheets').then(m => m.jobSheet(j)); } }); longPress(tilesBox, '.ftile[data-pane]', t => { const a = G.st?.agents.find(x => x.pane === t.dataset.pane); if (!a) return false; lazy('sheets').then(m => m.cardSheet(a)); }); }
  const items = MODS.queue ? MODS.queue.items(st).filter(x => x.rank <= 2) : [];
  const byPane = new Map((st?.agents || []).map(a => [a.pane, a]));
  // tiles: running jobs (with their owner), then working agents without a running job
  const run = (o.jobs || []).filter(j => j.status === 'running');
  const tiles = [], tilePanes = new Set();
  for (const j of run) {
    const a = j.pane && byPane.get(j.pane);
    tiles.push({ key: 'j:' + j.id, a: a || null, j, p: PROG.get(j.id) || null, title: j.desc || (a ? nm(a) : cmdName(j.cmd)) });
    if (a) tilePanes.add(a.pane);
  }
  for (const a of o.list) if (!a.home && dispStatus(a) === 'working' && !tilePanes.has(a.pane)) { tiles.push({ key: 'a:' + a.pane, a, j: null, p: null, title: nm(a) }); tilePanes.add(a.pane); }
  const workingPanes = new Set(o.list.filter(a => !a.home && dispStatus(a) === 'working').map(a => a.pane));
  if (run.length) loadProg(run);
  const ready = o.list.filter(a => !a.home && dispStatus(a) === 'done');
  if (patchHTML(brief, `<div class="kick">${esc(new Date().toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }))} · ${o.list.length} agent${o.list.length === 1 ? '' : 's'}${run.length ? ` · ${run.length} job${run.length > 1 ? 's' : ''}` : ''}</div><h1 class="bl">${briefHTML(st, items, tiles, ready, o.failing || [])}</h1>`)) tick(brief);
  drawHero(heroBox, items, tiles.length);
  tilesSec.hidden = !tiles.length;
  drawTiles(tilesBox, tiles);
  return { hero: items[0]?.pane || null, tiles: workingPanes };
}
// re-render when the triage queue changes underneath us (held, snoozed, a menu loaded)
lazy('queue').then(tr => tr.setHook(refresh));
export const _test = { briefHTML, parseProg, tileHTML };
