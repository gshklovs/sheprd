// Fleet (lazy): Sessions (+ Resume), Schedules (recurring agents), Cloud routines, Screen (desktop
// screenshots), the parent/child agent tree in the workspace, "Why is it blocked?" and the live
// terminal stream. Routes: #/fleet, #/fleet/sched, #/fleet/cloud, #/fleet/desk. API.md "Fleet".
import { G, I, $, h, esc, api, post, ls, sheet, toast, haptic, tick, rel, shortPath, goBack, debounce, closeSheets, lightbox, dispStatus, LABEL, lazy, sicon, schedIconSheet, longPress } from './app.js';

await new Promise(r => {
  if (document.querySelector('link[data-fleet]')) return r();
  const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'fleet.css'; l.dataset.fleet = 1;
  l.onload = l.onerror = r; document.head.append(l); setTimeout(r, 3000);
});

const enc = encodeURIComponent;
const sleep = ms => new Promise(r => setTimeout(r, ms));
const del = p => api(p, { method: 'DELETE' });
const kb = n => n >= 1 << 20 ? (n / (1 << 20)).toFixed(1) + ' MB' : Math.max(1, Math.round(n / 1024)) + ' KB';
const ago = ts => ts ? `<span data-since="${ts}" data-ago></span>` : '';
const when = ms => {
  if (!ms) return '—';
  const d = new Date(ms), now = new Date(), t = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  const days = Math.round((new Date(d).setHours(0, 0, 0, 0) - new Date(now).setHours(0, 0, 0, 0)) / 864e5);
  return days === 0 ? 'today ' + t : days === 1 ? 'tomorrow ' + t : days === -1 ? 'yesterday ' + t
    : d.toLocaleDateString([], { weekday: 'short', month: 'short', day: 'numeric' }) + ' ' + t;
};
const inT = ms => { const s = Math.round((ms - Date.now()) / 1000); return s <= 0 ? 'now' : s < 3600 ? `in ${Math.ceil(s / 60)} min` : s < 86400 ? `in ${Math.floor(s / 3600)}h ${Math.round((s % 3600) / 60)}m` : `in ${Math.round(s / 86400)}d`; };
const isoMs = s => s ? Date.parse(s) || 0 : 0;

// ---------- router ----------
const TABS = [['', 'Sessions'], ['asks', 'Asks'], ['sched', 'Schedules'], ['cloud', 'Cloud'], ['desk', 'Screen']];
// segments per tab-bar tab (app.js tabBar): History · Automations · Machine; "#…" entries live outside fleet.js
const SEGS = { '': [['', 'Sessions'], ['asks', 'Asks'], ['#/search', 'Search']], sched: [['sched', 'Schedules'], ['cloud', 'Cloud']],
  desk: [['#/r', 'Live'], ['#/r/disk', 'Disk'], ['desk', 'Screen'], ['#usage', 'Usage']] };
SEGS.cloud = SEGS.sched;
SEGS.asks = SEGS[''];
const VIEWS = {};
export function route(hs) {
  const seg = (hs.match(/^\/fleet\/?([a-z]*)/) || [])[1] || '';
  const k = TABS.some(t => t[0] === seg) ? seg : '';
  const make = () => (VIEWS[k] ||= new ({ '': SessionsView, asks: AsksView, sched: SchedView, cloud: CloudView, desk: DeskView })[k]());
  return { key: 'fleet/' + k, make, focus: null };
}
function shell(cls, on, title, extra = '') {
  return h(`<div class="view fleet ${cls}">
    <header class="bar"><div class="bar-in">
      <button class="ib back" aria-label="Back">${I.back}</button>
      <div class="ttl"><div class="t1">${esc(title)}</div><div class="t2">${esc(G.st?.host || '')}</div></div>${extra}
    </div></header>
    <div class="scroll fsc"><div class="fseg">${SEGS[on].map(([k, l]) => `<button data-fl="${k}"${k === on ? ' class="on"' : ''}>${l}</button>`).join('')}</div><div class="fbody"></div></div>
  </div>`);
}
function wire(v) {
  $('.back', v.el).onclick = () => goBack('#/agents');
  $('.fseg', v.el).onclick = e => {
    const b = e.target.closest('[data-fl]'); if (!b || b.classList.contains('on')) return;
    const k = b.dataset.fl;
    if (k === '#usage') { lazy('git').then(m => m.usageSheet()); return; }
    location.replace(k[0] === '#' ? k : '#/fleet' + (k ? '/' + k : ''));
  };
  v.body = $('.fbody', v.el);
}

// ---------- resume ----------
export async function resume(sid, btn) {
  const live = G.st?.agents.find(a => a.session === sid || a.tid === sid);
  if (live) { location.hash = '#/a/' + enc(live.pane); return; }
  haptic();
  if (btn) { btn.disabled = true; btn.innerHTML = '<span class="spin"></span>'; }
  try {
    const r = await post(`sessions/${enc(sid)}/resume`);
    if (!r.live) G.starting.set(r.pane, Date.now());
    toast(r.live ? 'Already running — opening it' : 'Resuming in a new tab', 'ok', null, 1600);
    location.hash = '#/a/' + enc(r.pane);
  } catch (e) { toast('Resume failed: ' + esc(e.message), 'err'); if (btn) { btn.disabled = false; btn.textContent = 'Resume'; } }
}

// read-only transcript (old chat): a bar with Resume / Open live
export function roChat(view, st) {
  const sid = view.fixedTid; if (!sid || sid.includes('~')) return;
  const a = st?.agents.find(x => x.session === sid || x.tid === sid);
  const sig = a ? 'live:' + a.pane : 'ro';
  if (view.flSig === sig) return;
  view.flSig = sig;
  let bar = $('.fl-rbar', view.el);
  if (!bar) { bar = h('<div class="fl-rbar"></div>'); view.el.insertBefore(bar, $('.composer', view.el)); }
  bar.innerHTML = a ? `<span>Live in ${esc(a.tabLabel || a.pane)}</span><a class="btn sm primary" href="#/a/${enc(a.pane)}">Open live chat</a>`
    : `<span>Past session · read-only</span><button class="btn sm primary">Resume</button>`;
  const b = $('button', bar); if (b) b.onclick = () => resume(sid, b);
}

// ---------- Sessions ----------
class SessionsView {
  constructor() {
    this.keep = true; this.pane = null; this.q = ''; this.rows = []; this.total = 0; this.all = ls.get('fl.all', false);
    this.el = shell('sess', '', 'Sessions');
    wire(this);
    this.body.innerHTML = `<form class="fsearch" role="search"><span class="sic">${I.search}</span><input type="search" placeholder="Search titles, prompts, folders" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" enterkeyhint="search"></form>
      <label class="ftog"><input type="checkbox" switch class="hl"${this.all ? ' checked' : ''}><span>Include headless (claude -p) runs</span></label>
      <div class="slist"></div><div class="stline more" hidden><button class="btn sm">Show more</button></div><div class="stline small sfoot"></div>`;
    this.inp = $('input[type=search]', this.el); this.list = $('.slist', this.el);
    const run = debounce(() => this.load(), 200);
    this.inp.oninput = () => { this.q = this.inp.value.trim(); run(); };
    $('.fsearch', this.el).onsubmit = e => { e.preventDefault(); this.inp.blur(); this.load(); };
    $('.hl', this.el).onchange = e => { this.all = e.target.checked; ls.set('fl.all', this.all || null); this.load(); };
    $('.more button', this.el).onclick = () => this.load(true);
    this.list.addEventListener('click', e => {
      const b = e.target.closest('[data-res]'); if (b) { e.preventDefault(); resume(b.dataset.res, b); }
    });
  }
  mount() { this.load(); }
  resume() { this.load(); }
  onVisible() { this.load(); }
  async load(more) {
    const off = more ? this.rows.length : 0;
    const ac = this.ac = new AbortController();
    try {
      const r = await api(`sessions?limit=50&offset=${off}&q=${enc(this.q)}${this.all ? '&all=1' : ''}`, { signal: ac.signal });
      if (ac !== this.ac) return;
      this.rows = more ? this.rows.concat(r.sessions) : r.sessions; this.total = r.total;
      this.render(r);
    } catch (e) { if (e.name !== 'AbortError') this.list.innerHTML = `<div class="stline">${e.status === 404 ? 'This server has no session index yet (update the server).' : 'Failed: ' + esc(e.message)}</div>`; }
  }
  render(r) {
    const live = new Map((G.st?.agents || []).map(a => [a.session, a]));
    this.list.innerHTML = this.rows.length ? this.rows.map(s => {
      const a = live.get(s.sid), st = a ? dispStatus(a) : null;
      const href = a ? '#/a/' + enc(a.pane) : '#/t/' + enc(s.sid);
      return `<div class="srow${a ? ' live' : ''}"><a class="sb" href="${href}">
          <div class="st1">${a ? `<span class="pill s-${st}"><i></i>${LABEL[st]}</span>` : ''}${s.headless ? '<i class="tag">-p</i>' : ''}<b>${esc(s.title || 'Untitled session')}</b></div>
          <div class="st2">${esc(shortPath(s.cwd))}${s.branch ? ` · ${esc(s.branch)}` : ''} · ${ago(s.ts)} · ${s.prompts} prompt${s.prompts === 1 ? '' : 's'} · ${kb(s.size)}</div>
          ${s.last ? `<div class="st3">${esc(s.last)}</div>` : ''}</a>
        ${a ? `<a class="btn sm" href="${href}">Open</a>` : s.cwdOk ? `<button class="btn sm primary" data-res="${esc(s.sid)}">Resume</button>` : '<span class="dim small gone">folder gone</span>'}</div>`;
    }).join('') : `<div class="stline">${this.q ? `No sessions match “${esc(this.q)}”.` : 'No sessions yet.'}</div>`;
    $('.more', this.el).hidden = this.rows.length >= this.total;
    $('.sfoot', this.el).textContent = `${this.total} session${this.total === 1 ? '' : 's'} · ${r.indexed} indexed`;
    tick(this.list);
  }
}

// ---------- My asks (asks.rs): every request sent from the phone, followed until it's done ----------
const ASK_ST = { sent: 'Sent', working: 'Working', replied: 'Replied', lost: 'Agent closed', done: 'Done', dropped: 'Dropped' };
class AsksView {
  constructor() {
    this.keep = true; this.pane = null; this.showClosed = false;
    this.el = shell('asks', 'asks', 'My asks');
    wire(this);
    this.body.innerHTML = '<div class="alist"></div>';
    this.list = $('.alist', this.el);
    this.list.onclick = e => this.act(e);
    this.load();
  }
  onVisible() { this.load(); }
  resume() { this.load(); }
  async load() {
    try { this.d = await api('asks'); this.render(); }
    catch (e) { this.list.innerHTML = `<div class="stline">${e.status === 404 ? 'This server has no asks ledger yet (update the server).' : 'Failed: ' + esc(e.message)}</div>`; }
  }
  row(k) {
    const live = G.st?.agents.find(a => a.pane === k.pane);
    const href = live ? '#/a/' + enc(k.pane) + (k.answer_i != null ? '?i=' + k.answer_i : '') : k.session ? '#/t/' + enc(k.session) : '';
    const open = !['done', 'dropped'].includes(k.status), stale = Date.now() - k.status_ts > 10 * 60e3;
    const btns = !open ? '' : [
      k.status === 'replied' ? `<button class="btn sm primary" data-ak="done" data-ev="ask-done">Done</button>` : '',
      live && (k.status === 'replied' || stale) ? `<button class="btn sm" data-ak="nudge" data-ev="ask-nudge">Nudge</button>` : '',
      `<button class="btn sm" data-ak="dropped" data-ev="ask-drop">Drop</button>`].join('');
    return `<div class="arow s-${k.status}" data-id="${esc(k.id)}">
      <${href ? `a href="${href}"` : 'div'} class="ab">
        <div class="at1"><span class="apill">${ASK_ST[k.status] || esc(k.status)}</span><b>${esc(k.agent)}</b><span class="dim">${ago(k.ts)}${k.nudges ? ` · nudged ${k.nudges}×` : ''}</span></div>
        <div class="at2">${esc(k.text)}</div>
        ${k.outcome ? `<div class="at3">↳ ${esc(k.outcome)}</div>` : k.status === 'lost' ? '<div class="at3">↳ the agent closed before replying</div>' : ''}
      </${href ? 'a' : 'div'}>${btns ? `<div class="aacts">${btns}</div>` : ''}</div>`;
  }
  render() {
    const all = this.d.asks || [];
    const check = all.filter(k => k.status === 'replied' || k.status === 'lost');
    const wait = all.filter(k => k.status === 'sent' || k.status === 'working');
    const closed = all.filter(k => k.status === 'done' || k.status === 'dropped');
    const sec = (t, list, note) => list.length ? `<div class="ahead">${t} <span class="dim">${list.length}</span></div>${note ? `<div class="stline small">${note}</div>` : ''}${list.map(k => this.row(k)).join('')}` : '';
    this.list.innerHTML = (all.length ? '' : '<div class="stline">Nothing yet. Requests you send agents from the phone show up here until you mark them done.</div>')
      + sec('Check these', check, check.length ? 'The agent answered: mark Done, Nudge if it isn\'t really done, or Drop.' : '')
      + sec('Waiting on agents', wait)
      + (closed.length ? `<button class="ahead atog" data-ak="closed">${this.showClosed ? '▾' : '▸'} Closed this week <span class="dim">${closed.length}</span></button>${this.showClosed ? closed.map(k => this.row(k)).join('') : ''}` : '');
    tick(this.list);
  }
  async act(e) {
    const b = e.target.closest('[data-ak]'); if (!b) return;
    e.preventDefault();
    if (b.dataset.ak === 'closed') { this.showClosed = !this.showClosed; this.render(); return; }
    const id = b.closest('.arow').dataset.id; haptic(); b.disabled = true;
    try {
      if (b.dataset.ak === 'nudge') { await post(`asks/${enc(id)}/nudge`); toast('Nudged', 'ok', null, 1400); }
      else await post(`asks/${enc(id)}`, { status: b.dataset.ak });
    } catch (x) { toast(esc(x.message), 'err'); }
    this.load();
  }
}

// ---------- Schedules ----------
class SchedView {
  constructor() {
    this.keep = true; this.pane = null; this.list = [];
    this.el = shell('sched', 'sched', 'Schedules', `<button class="ib add" aria-label="New schedule">${I.plus}</button>`);
    wire(this);
    $('.add', this.el).onclick = () => schedSheet(null, () => this.load());
    this.body.addEventListener('click', e => this.onClick(e));
    // long-press a schedule / timer card → pick its icon
    longPress(this.body, '.schc', c => schedIconSheet(c.dataset.key, c.dataset.icon, c.dataset.by, () => this.load()));
    // every "Tell me when…" watch, cancellable (was only in Settings)
    this.wbox = h('<div class="fwatch"><h3>Watches</h3><div class="wl"></div><p class="dim small">Add one from an agent’s menu: Tell me when…</p></div>');
    $('.fsc', this.el).append(this.wbox);
  }
  mount() { this.load(); this.timer(); }
  resume() { this.load(); this.timer(); }
  suspend() { clearInterval(this.t); }
  destroy() { clearInterval(this.t); }
  onVisible() { this.load(); }
  timer() { clearInterval(this.t); this.t = setInterval(() => { if (!document.hidden && G.view === this) this.load(); }, 20000); }
  async load() {
    lazy('v3').then(m => m.watchList($('.wl', this.wbox), null), () => { });
    try { const r = await api('sched'); this.list = r.schedules || []; this.timers = r.timers || []; this.render(); }
    catch (e) { this.body.innerHTML = `<div class="stline">${e.status === 404 ? 'This server has no scheduler (update the server).' : 'Failed: ' + esc(e.message)}</div>`; }
  }
  render() {
    const by = new Map((G.st?.agents || []).map(a => [a.pane, a]));
    const html = this.list.length ? this.list.map(s => {
      const a = s.lastPane && by.get(s.lastPane);
      const st = s.pending ? (s.pending.working ? 'running' : 'starting') : s.lastStatus;
      return `<div class="schc${s.paused ? ' paused' : ''}" data-id="${esc(s.id)}" data-key="s:${esc(s.id)}" data-icon="${esc(s.icon || 'clock')}" data-by="${esc(s.iconBy || '')}">
        <div class="sh1"><span class="sic">${sicon(s.icon)}</span><b>${esc(s.label)}</b><span class="sp"></span>${s.paused ? '<span class="pill">Paused</span>' : s.next ? `<span class="nx">${inT(s.next)}</span>` : ''}</div>
        <div class="sh2">${esc(s.desc)}${s.next && !s.paused ? ` · next ${esc(when(s.next))}` : ''}</div>
        <div class="sh3">${esc(shortPath(s.cwd))}${s.reuse ? ' · reuses its pane' : ''}</div>
        <div class="sh4">${esc(s.prompt)}</div>
        ${s.lastRun ? `<div class="sh5">Last run ${ago(s.lastRun)}${st ? ' · ' + esc(st) : ''}${a ? ` · <a href="#/a/${enc(a.pane)}">open</a>` : ''} · ${s.runs} run${s.runs === 1 ? '' : 's'}</div>` : '<div class="sh5">Never run yet</div>'}
        <div class="sacts"><button class="btn sm" data-a="run">Run now</button><button class="btn sm" data-a="pause">${s.paused ? 'Resume' : 'Pause'}</button><button class="btn sm" data-a="edit">Edit</button><button class="btn sm danger" data-a="del">Delete</button></div>
      </div>`;
    }).join('') : `<div class="fempty"><p>No schedules yet.</p><p class="dim small">Start an agent on a schedule: “every weekday 8:00”, “every 2h”, “mon,wed 19:30”. You get a push when each run finishes.</p><button class="btn primary">New schedule</button></div>`;
    this.body.innerHTML = html + this.timersHtml(by);
    tick(this.body);
    const nb = $('.fempty .btn', this.body); if (nb) nb.onclick = () => schedSheet(null, () => this.load());
  }
  // systemd user timers (read-only): agent wake-ups (`herdr agent prompt <pane> …`) show the agent and link to it.
  timersHtml(by) {
    const ts = this.timers || []; if (!ts.length) return '';
    const byName = new Map((G.st?.agents || []).filter(a => a.name).map(a => [a.name, a]));
    return `<h3 class="schh">System timers</h3>` + ts.map(t => {
      const a = t.agentPane && (by.get(t.agentPane) || byName.get(t.agentPane));
      const title = t.agentPane ? (a ? (a.title || a.name || a.pane) : `agent ${t.agentPane}`) : t.name;
      const st = t.next ? `<span class="nx">${inT(t.next)}</span>` : '<span class="pill">Idle</span>';
      return `<div class="schc tmr${t.next ? '' : ' paused'}" data-key="t:${esc(t.name)}" data-icon="${esc(t.icon || 'clock')}" data-by="${esc(t.iconBy || '')}">
        <div class="sh1"><span class="sic">${sicon(t.icon)}</span><b>${esc(title)}</b><span class="sp"></span>${st}</div>
        <div class="sh2">${t.next ? 'next ' + esc(when(t.next)) : 'no next run'}${t.last ? ` · last ${esc(when(t.last))}` : ''}</div>
        <div class="sh3">${esc(t.unit)}${t.transient ? ' · transient' : ''}${t.spec ? ' · ' + esc(t.spec) : ''}</div>
        ${t.agentPane ? `<div class="sh4">Prompts ${a ? `<a href="#/a/${enc(a.pane)}">${esc(a.title || a.pane)}</a>` : esc(t.agentPane) + ' (not running)'}: ${esc(t.agentPrompt || '')}</div>` : ''}
        <div class="sh4"><code>${esc(t.command)}</code></div>
      </div>`;
    }).join('');
  }
  async onClick(e) {
    const b = e.target.closest('[data-a]'); if (!b) return;
    const s = this.list.find(x => x.id === b.closest('.schc')?.dataset.id); if (!s) return;
    haptic();
    try {
      if (b.dataset.a === 'edit') return schedSheet(s, () => this.load());
      if (b.dataset.a === 'del') {
        if (b.dataset.sure !== '1') { b.dataset.sure = '1'; b.textContent = 'Delete?'; setTimeout(() => { b.dataset.sure = ''; b.textContent = 'Delete'; }, 3000); return; }
        await del(`sched/${enc(s.id)}`); toast('Deleted', 'ok', null, 1400);
      } else if (b.dataset.a === 'pause') await post(`sched/${enc(s.id)}`, { paused: !s.paused });
      else if (b.dataset.a === 'run') {
        b.disabled = true; b.innerHTML = '<span class="spin"></span>';
        const r = await post(`sched/${enc(s.id)}/run`);
        toast('Started', 'ok', () => { location.hash = '#/a/' + enc(r.pane); }, 2400);
      }
    } catch (err) { toast(esc(err.message), 'err'); }
    this.load();
  }
}

async function schedSheet(s, done) {
  const sh = sheet(`<h2>${s ? 'Edit schedule' : 'New schedule'}</h2>
    <div class="form fsch">
      <label><span>When</span><input class="when" placeholder="every weekday 8:00 · every 2h · mon,fri 19:30" autocapitalize="off" autocorrect="off" value="${esc(s?.when || '')}"></label>
      <div class="wprev dim small"></div>
      <label><span>Folder</span><input class="cwd" list="fl-dirs" placeholder="~/Work/project" autocapitalize="off" autocorrect="off" spellcheck="false" value="${esc(shortPath(s?.cwd || ''))}"></label>
      <datalist id="fl-dirs"></datalist>
      <textarea class="prompt" rows="4" placeholder="What should the agent do each time?">${esc(s?.prompt || '')}</textarea>
      <label><span>Label</span><input class="label" placeholder="optional" value="${esc(s?.label || '')}"></label>
      <label class="ftog"><input type="checkbox" switch class="reuse"${s ? (s.reuse ? ' checked' : '') : ' checked'}><span>Reuse the same pane when it is idle</span></label>
      <button class="btn primary save">${s ? 'Save' : 'Create'}</button>
    </div>`, 'tall');
  const q = x => $(x, sh), prev = q('.wprev');
  api('dirs').then(r => { q('#fl-dirs').innerHTML = (r.dirs || []).map(d => `<option value="${esc(shortPath(d.path))}">`).join(''); }).catch(() => { });
  let ok = !!s;
  const check = debounce(async () => {
    const w = q('.when').value.trim(); if (!w) { prev.textContent = ''; ok = false; return; }
    try {
      const r = await post('sched/parse', { when: w }); ok = true;
      prev.classList.remove('err'); prev.innerHTML = `<b>${esc(r.desc)}</b> · <code>${esc(r.cron)}</code><br>next: ${r.next.map(n => esc(when(n))).join(', ')}`;
    } catch (e) { ok = false; prev.classList.add('err'); prev.textContent = e.message; }
  }, 250);
  q('.when').oninput = check; if (s) check();
  q('.save').onclick = async () => {
    const body = { when: q('.when').value, cwd: q('.cwd').value.trim(), prompt: q('.prompt').value, label: q('.label').value, reuse: q('.reuse').checked };
    if (!ok) { toast('Fix the “When” first', 'err'); return; }
    const b = q('.save'); b.disabled = true;
    try { await post(s ? `sched/${enc(s.id)}` : 'sched', body); sh.close(); toast(s ? 'Saved' : 'Schedule created', 'ok', null, 1500); done?.(); }
    catch (e) { toast(esc(e.message), 'err'); b.disabled = false; }
  };
}

// ---------- Cloud routines ----------
class CloudView {
  constructor() {
    this.keep = true; this.pane = null; this.d = null; this.open = new Set();
    this.el = shell('cloud', 'cloud', 'Cloud routines', `<button class="ib rf" aria-label="Refresh"><svg viewBox="0 0 24 24"><path d="M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7"/></svg></button>`);
    wire(this);
    $('.rf', this.el).onclick = () => this.refresh();
    this.body.addEventListener('click', e => this.onClick(e));
  }
  mount() { this.load(); }
  resume() { this.load(); }
  suspend() { clearTimeout(this.t); }
  destroy() { clearTimeout(this.t); }
  async load() {
    clearTimeout(this.t);
    try { this.d = await api('cloud'); } catch (e) { this.body.innerHTML = `<div class="stline">${e.status === 404 ? 'This server has no cloud routines support.' : 'Failed: ' + esc(e.message)}</div>`; return; }
    this.render();
    if (this.d.busy && G.view === this) this.t = setTimeout(() => this.load(), 2500);
  }
  async refresh() {
    haptic();
    try { await post('cloud/refresh'); } catch (e) { if (e.status !== 409) { toast(esc(e.message), 'err'); return; } }
    this.load();
  }
  render() {
    const d = this.d || {};
    const head = !d.capable ? `<div class="fnote warn">The claude CLI was not found on this machine, so routines cannot be listed.</div>`
      : d.available === false ? `<div class="fnote warn"><b>Not available.</b> ${esc(d.reason || '')}</div>`
      : `<div class="fnote">${d.busy ? `<span class="spin"></span> Asking Claude for your routines…` : d.fetchedAt ? `Fetched ${ago(d.fetchedAt)}${d.reason ? ` · last refresh failed: ${esc(d.reason)}` : ''}` : 'Not fetched yet.'}
          <span class="dim small">Listed through Claude Code's RemoteTrigger tool (one short Haiku turn per refresh, read-only).</span>
          ${d.busy ? '' : `<button class="btn sm primary rf2">${d.fetchedAt ? 'Refresh' : 'Fetch routines'}</button>`}</div>`;
    const rs = d.routines || [];
    const by = k => rs.filter(r => k ? r.enabled : !r.enabled);
    const card = r => {
      const lr = r.lastRun, runs = d.runs?.[r.id], open = this.open.has(r.id);
      const sched = r.cron ? `cron ${esc(r.cron)}` : r.runOnceAt ? `once · ${esc(when(isoMs(r.runOnceAt)))}` : esc(r.kind || '');
      return `<div class="rtc${r.enabled ? '' : ' off'}" data-id="${esc(r.id)}">
        <div class="sh1"><b>${esc(r.name || r.id)}</b><span class="sp"></span><span class="pill${r.enabled ? ' s-done' : ''}">${r.enabled ? 'On' : r.endedReason ? esc(r.endedReason.replace(/_/g, ' ')) : 'Off'}</span></div>
        <div class="sh2">${sched}${r.enabled && r.nextRunAt ? ` · next ${esc(when(isoMs(r.nextRunAt)))}` : ''}</div>
        ${lr ? `<div class="sh5">Last run ${ago(isoMs(lr.firedAt))} · <span class="rs-${esc(lr.status.split(' ')[0])}">${esc(lr.status)}</span>${lr.failure ? ' · ' + esc(lr.failure) : ''}</div>` : ''}
        <div class="sh4${open ? ' open' : ''}">${esc(r.prompt)}</div>
        <div class="sacts"><button class="btn sm" data-a="more">${open ? 'Less' : 'Prompt'}</button><button class="btn sm" data-a="runs"${d.busy ? ' disabled' : ''}>Runs</button>${r.url ? `<a class="btn sm" href="${esc(r.url)}" target="_blank" rel="noopener">claude.ai ${I.ext}</a>` : ''}</div>
        ${runs ? `<div class="rruns">${runs.error ? `<div class="dim small">${esc(runs.error)}</div>` : runs.runs?.length ? runs.runs.map(x => `<a class="rrun" href="${esc(x.url || '#')}" target="_blank" rel="noopener"><span>${esc(x.title || x.id)}</span><small>${esc(x.status || '')} · ${ago(isoMs(x.created))}</small></a>`).join('') : `<div class="dim small">${esc(runs.summary || 'No runs recorded.')}</div>`}</div>` : ''}
      </div>`;
    };
    const on = by(true), off = by(false);
    this.body.innerHTML = head + (rs.length ? (on.length ? `<h3 class="fh3">Active (${on.length})</h3>` + on.map(card).join('') : '<div class="stline">No active routines.</div>')
      + (off.length ? `<details class="fold"><summary>Finished or off (${off.length})</summary>${off.map(card).join('')}</details>` : '')
      + (d.hasMore ? '<div class="stline small">Showing the newest page of routines.</div>' : '') : '');
    const b = $('.rf2', this.body); if (b) b.onclick = () => this.refresh();
    tick(this.body);
  }
  async onClick(e) {
    const b = e.target.closest('[data-a]'); if (!b) return;
    const id = b.closest('.rtc')?.dataset.id; if (!id) return;
    if (b.dataset.a === 'more') { this.open.has(id) ? this.open.delete(id) : this.open.add(id); this.render(); return; }
    haptic();
    try { await post(`cloud/${enc(id)}/runs`); } catch (err) { if (err.status !== 409) toast(esc(err.message), 'err'); }
    this.load();
  }
}

// ---------- Screen (desktop screenshots) ----------
class DeskView {
  constructor() {
    this.keep = true; this.pane = null; this.d = null;
    this.el = shell('desk', 'desk', 'Screen', `<button class="ib rf" aria-label="Refresh"><svg viewBox="0 0 24 24"><path d="M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7"/></svg></button>`);
    wire(this);
    $('.rf', this.el).onclick = () => this.load();
    this.body.addEventListener('click', e => this.onClick(e));
  }
  mount() { this.load(); }
  resume() { this.load(); }
  onVisible() { this.load(); }
  async load() {
    try { this.d = await api('desk'); this.render(); }
    catch (e) { this.body.innerHTML = `<div class="stline">${e.status === 404 ? 'This server cannot take screenshots (update it).' : 'Failed: ' + esc(e.message)}</div>`; }
  }
  render() {
    const d = this.d, can = d.available && d.state !== 'locked' && d.state !== 'off' && d.state !== 'nodesktop';
    const banner = { locked: '🔒 The desktop is locked. Nothing can be captured until you unlock it.', off: 'The screen is off (idle). Nothing can be captured right now.',
      screensaver: 'The screensaver is running; a capture shows it.', nodesktop: 'No desktop session found (Hyprland is not reachable).' }[d.state];
    const shotBtn = (t, label, sub = '') => `<button class="drow" data-t="${esc(t)}"${can ? '' : ' disabled'}><span class="sb"><b>${esc(label)}</b>${sub ? `<small>${sub}</small>` : ''}</span><span class="cam">${I.img}</span></button>`;
    this.body.innerHTML = `${banner ? `<div class="fnote${d.state === 'screensaver' ? '' : ' warn'}">${esc(banner)}</div>` : ''}
      ${!d.available && d.state !== 'nodesktop' ? '<div class="fnote warn">grim is not installed.</div>' : ''}
      <div class="dim small fpriv">Nothing is captured unless you tap. Shots stay on the computer (last ${6}).</div>
      ${shotBtn('screen', 'Whole screen', d.outputs.map(o => esc(o.name)).join(' · '))}
      ${d.outputs.length > 1 ? d.outputs.map(o => shotBtn('output:' + o.name, o.name, `${o.w}×${o.h}${o.focused ? ' · focused' : ''}`)).join('') : ''}
      <h3 class="fh3">Windows (${d.windows.length})</h3>
      ${d.windows.map(w => shotBtn('window:' + w.id, w.title || w.class, `${esc(w.class)} · workspace ${esc(w.ws)}${w.visible ? ' · on screen' : ''}`)).join('') || '<div class="stline">No windows.</div>'}
      ${d.shots?.length ? `<h3 class="fh3">Recent shots</h3><div class="dshots">${d.shots.map(s => `<button class="dshot" data-src="${esc(s.url)}"><img loading="lazy" src="${esc(s.url)}" alt=""><small>${ago(s.ts)}</small></button>`).join('')}</div>` : ''}`;
    tick(this.body);
  }
  async onClick(e) {
    const s = e.target.closest('[data-src]');
    if (s) { lightbox(s.dataset.src, (this.d?.shots || []).map(x => x.url)); return; }
    const b = e.target.closest('[data-t]'); if (!b || b.disabled) return;
    haptic(); b.disabled = true; const cam = $('.cam', b); cam.innerHTML = '<span class="spin"></span>';
    try {
      const r = await post('desk/shot', { target: b.dataset.t });
      await this.load();
      lightbox(r.url, (this.d?.shots || []).map(x => x.url));
    } catch (err) { toast(esc(err.message), 'err'); await this.load(); }
  }
}

// ---------- Why is it blocked? ----------
export async function whySheet(pane) {
  const sh = sheet(`<h2>Why is it blocked?</h2><div class="why"><div class="dim pad"><span class="spin"></span> Asking herdr…</div></div>`, 'tall');
  const box = $('.why', sh);
  try {
    const r = await api(`a/${enc(pane)}/explain`);
    const rule = x => `<div class="wrule"><div><b>${esc(x.id)}</b> → ${esc(x.state)} <span class="dim small">region ${esc(x.region)} · priority ${esc(x.priority)}</span></div>
      ${x.patterns?.length ? `<div class="dim small">looks for ${x.patterns.map(p => `<code>${esc(p)}</code>`).join(' ')}</div>` : ''}
      ${x.preview ? `<pre class="wpre">${esc(x.preview)}</pre>` : ''}</div>`;
    box.innerHTML = `<div class="wsum">herdr detects <b>${esc(r.state || '?')}</b>${r.status && r.status !== r.state ? `, sheprd shows <b>${esc(r.status)}</b>` : ''}.</div>
      ${r.sheprdReason ? `<div class="fnote">${esc(r.sheprdReason)}</div>` : ''}
      ${r.rule ? `<h3 class="fh3">Matched rule</h3>${rule(r.rule)}` : '<div class="fnote warn">No detection rule matched.</div>'}
      ${r.also?.length ? `<h3 class="fh3">Also matched</h3>${r.also.map(rule).join('')}` : ''}
      ${r.fallback || r.warning || r.skipped ? `<div class="fnote warn">${esc([r.fallback, r.warning, r.skipped].filter(Boolean).join(' · '))}</div>` : ''}
      <h3 class="fh3">Screen herdr reads</h3><pre class="wpre scr">${esc(r.screen || '(empty)')}</pre>
      ${r.notMatched?.length ? `<details class="fold"><summary>Blocked rules that did not match (${r.notMatched.length})</summary>${r.notMatched.map(rule).join('')}</details>` : ''}
      <div class="dim small">Manifest ${esc(r.manifest?.version || '')} · ${esc(r.agent || '')}</div>`;
    const pre = $('.wpre.scr', box); pre.scrollTop = pre.scrollHeight;
  } catch (e) { box.innerHTML = `<div class="stline">${e.status === 404 ? 'No agent in that pane (or the server has no explain endpoint).' : 'Failed: ' + esc(e.message)}</div>`; }
}

// ---------- live terminal stream ----------
// Screen (app.js) calls this from start(); on any failure it falls back to its own polling.
export function liveScreen(s) {
  if (s.es || !window.EventSource) { if (!s.es) s.poll(); return; }
  let got = false;
  const es = s.es = new EventSource(`/api/a/${enc(s.pane)}/live`);
  const back = () => { es.close(); if (s.es === es) s.es = null; if (s.on) s.poll(); };
  es.addEventListener('screen', e => { got = true; try { s.draw(JSON.parse(e.data)); s.el.classList.remove('stale'); s.el.classList.add('streaming'); } catch { } });
  es.addEventListener('fallback', () => { G.noLive = Date.now(); back(); });
  es.addEventListener('gone', back);
  es.onerror = () => { if (!got) { G.noLive = Date.now(); back(); } else s.el.classList.add('stale'); };
}

// ---------- workspace: parent/child tree ----------
const COLL = new Set(ls.get('fl.coll', []));
const saveColl = () => ls.set('fl.coll', COLL.size ? [...COLL] : null);
// Reorders the sorted card list so workers follow their parent (depth-first); collapsed subtrees drop out.
export function order(list) {
  // Flat status order (needs you, working, done, idle); the "↳ started by …" line on
  // each card shows the relationship, so workers are no longer regrouped under parents.
  for (const a of list) TREE.set(a.pane, { d: 0, n: 0 });
  return list;
}
export function orderTree(list) {
  const inList = new Set(list.map(a => a.pane)), kids = new Map();
  for (const a of list) { const p = a.parent?.pane; if (p && inList.has(p) && p !== a.pane) { if (!kids.has(p)) kids.set(p, []); kids.get(p).push(a); } }
  if (!kids.size) { for (const a of list) TREE.set(a.pane, { d: 0, n: 0 }); return list; }
  const out = [], seen = new Set();
  const count = p => (kids.get(p) || []).reduce((n, k) => n + 1 + count(k.pane), 0);
  const worst = p => (kids.get(p) || []).flatMap(k => [dispStatus(k), worst(k.pane)]).flat();
  const walk = (a, d) => {
    if (seen.has(a.pane)) return; seen.add(a.pane);
    const n = count(a.pane);
    TREE.set(a.pane, { d, n, via: a.parent?.via, coll: n && COLL.has(a.pane), hid: n && COLL.has(a.pane) ? worst(a.pane) : null });
    out.push(a);
    if (!COLL.has(a.pane)) for (const k of kids.get(a.pane) || []) walk(k, d + 1);
    else (function mark(p) { for (const k of kids.get(p) || []) { seen.add(k.pane); mark(k.pane); } })(a.pane);
  };
  for (const a of list) if (!(a.parent?.pane && inList.has(a.parent.pane))) walk(a, 0);
  for (const a of list) if (!seen.has(a.pane)) walk(a, 0); // cycles (shouldn't happen)
  return out;
}
const TREE = new Map();
const VIA = { spawn: 'started by', review: 'reviewing', fork: 'forked from', drive: 'driven by', split: 'split from' };
export function deco(c, a) {
  const t = TREE.get(a.pane) || { d: 0, n: 0 };
  const sig = JSON.stringify(t);
  if (c.flSig === sig) return;
  c.flSig = sig;
  c.el.style.setProperty('--fl-d', Math.min(t.d, 4));
  c.el.classList.toggle('fl-kid', t.d > 0);
  c.el.title = t.d > 0 && t.via ? `${VIA[t.via] || t.via} ${a.parent?.pane}` : '';
  let tw = c.el.querySelector(':scope > .fl-tw');
  if (!t.n) { tw?.remove(); return; }
  if (!tw) { tw = h('<button class="fl-tw"></button>'); c.el.append(tw); tw.onclick = e => { e.preventDefault(); e.stopPropagation(); haptic(); COLL.has(a.pane) ? COLL.delete(a.pane) : COLL.add(a.pane); saveColl(); G.view?.onState?.(G.st); }; }
  const hb = t.hid ? t.hid.filter(s => s === 'blocked').length : 0, hw = t.hid ? t.hid.filter(s => s === 'working').length : 0;
  tw.innerHTML = t.coll ? `▸ ${t.n} worker${t.n > 1 ? 's' : ''} hidden${hb ? ` · <b class="c-b">${hb} need${hb > 1 ? '' : 's'} you</b>` : ''}${hw ? ` · ${hw} working` : ''}` : `▾ ${t.n} worker${t.n > 1 ? 's' : ''}`;
}
