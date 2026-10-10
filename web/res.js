// Phase 2: machine resources (lazy). #/r Live, #/r/disk Disk, process sheet, alerts sheet, plus the
// workspace tiles / card line / job line, home GPU segment and chat gauge. docs/phase2-ux.md, API.md "Phase 2".
// Idle cost ≈ 0: every poller stops when its view is hidden or left; the server stops sampling 10 s later.
import { G, I, $, h, esc, api, post, ls, sheet, toast, haptic, rel, shortPath, agentHref, itemHref, longPress, closeSheets, goBack, dispStatus, copyText, getPrefs, debounce, newAgentSheet, settingsSheet, lazy, whoName, HOME_NAME } from './app.js';

// styles ship with the module so nothing paints unstyled
await new Promise(r => {
  if (document.querySelector('link[data-res]')) return r();
  const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'res.css'; l.dataset.res = 1;
  l.onload = l.onerror = r; document.head.append(l); setTimeout(r, 3000);
});

const enc = encodeURIComponent;
const GB = 2 ** 30, MB = 2 ** 20;
const fb = b => b == null ? '—' : b >= 100 * GB ? Math.round(b / GB) + 'G' : b >= GB ? (b / GB).toFixed(1) + 'G' : b >= MB ? Math.round(b / MB) + 'M' : b > 0 ? Math.max(1, Math.round(b / 1024)) + 'K' : '0';
const fbs = b => b == null ? '—' : b >= 100 * GB ? Math.round(b / GB) + ' GB' : b >= GB ? (b / GB).toFixed(1) + ' GB' : Math.max(0, Math.round(b / MB)) + ' MB';
const g1 = b => (b / GB).toFixed(1);
const pc = v => v == null ? '—' : Math.round(v) + '%';
const kn = n => n >= 1e6 ? (n / 1e6).toFixed(1) + 'M' : n >= 1e3 ? Math.round(n / 1e3) + 'k' : String(n | 0);
const dur = ms => { const s = Math.max(0, ms / 1000 | 0); return s < 60 ? s + 's' : s < 3600 ? (s / 60 | 0) + 'm' : s < 86400 ? (s / 3600 | 0) + 'h ' + ((s % 3600) / 60 | 0) + 'm' : (s / 86400 | 0) + 'd ' + ((s % 86400) / 3600 | 0) + 'h'; };
const ago = ms => ms ? (rel(ms) === 'now' ? 'just now' : rel(ms) + ' ago') : '';
const mcol = p => p >= 95 ? 'var(--red)' : p >= 85 ? 'var(--warn)' : 'var(--work)';
const gname = n => String(n || 'GPU').replace(/^NVIDIA\s+/, '').replace(/^GeForce\s+/, '');
const put = (el, html) => { if (el && el._h !== html) { el._h = html; el.innerHTML = html; } };
const COLORS = ['red', 'orange', 'yellow', 'green', 'cyan', 'blue', 'purple', 'pink'];
const ccls = c => COLORS.includes(c) ? ' col-' + c : '';
const agentOf = p => p ? G.st?.agents.find(a => a.pane === p) : null;
const atitle = (a, p) => a ? (a.home ? HOME_NAME() : whoName(a.title || a.name || a.pane, a.pane)) : p || '';
const DOT = { working: 'var(--work)', blocked: 'var(--attn)', done: 'var(--done)', idle: 'var(--idle)', unknown: 'var(--fg3)' };
const R = {
  chip: '<svg viewBox="0 0 24 24"><rect x="6" y="6" width="12" height="12" rx="2"/><path d="M9 2v4M15 2v4M9 18v4M15 18v4M2 9h4M2 15h4M18 9h4M18 15h4"/></svg>',
  bell: '<svg viewBox="0 0 24 24"><path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9M10.3 21a1.9 1.9 0 0 0 3.4 0"/></svg>',
  ask: '<svg viewBox="0 0 24 24"><path d="M21 12a8 8 0 0 1-11.6 7.1L4 20l1-4.6A8 8 0 1 1 21 12z"/><path d="M9.5 9.5a2.5 2.5 0 1 1 3.3 2.4c-.5.2-.8.6-.8 1.1M12 16h.01"/></svg>',
  log: '<svg viewBox="0 0 24 24"><path d="M5 5h14M5 10h14M5 15h9M5 20h6"/></svg>',
  refresh: '<svg viewBox="0 0 24 24"><path d="M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7"/></svg>',
  gauge: '<svg viewBox="0 0 24 24"><path d="M3.5 17a9 9 0 1 1 17 0"/><path d="M12 15l4.5-5.5"/><circle cx="12" cy="15.5" r="1.4"/></svg>',
  pulse: '<svg viewBox="0 0 24 24"><path d="M3 12h4l2.5-6 5 12 2.5-6h4"/></svg>',
};
const GPUI = R.chip.replace('<svg', '<svg class="gl"');
const isGpuRule = r => r === 'gpu-stall' || r === 'vram-full' || r === 'gpu-hot' || r === 'watch-stall';

function spark(vals, { h = 30, w = 100, max = 100, fill = true } = {}) {
  const v = (vals || []).filter(x => x != null);
  const box = `<svg class="spark" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">`;
  if (v.length < 2) return box + '</svg>';
  const n = v.length, pts = v.map((y, i) => `${(i / (n - 1) * w).toFixed(1)},${(h - 1 - Math.min(y, max) / max * (h - 2)).toFixed(1)}`).join(' ');
  return box + (fill ? `<polygon points="0,${h} ${pts} ${w},${h}" fill="currentColor" stroke="none" opacity=".18"/>` : '') + `<polyline points="${pts}" vector-effect="non-scaling-stroke"/></svg>`;
}
const autoMax = v => Math.max(10, ...(v || []).filter(x => x != null)) * 1.15;

// ---------- pollers: pause while the app is hidden, resume at once when shown ----------
const POLLERS = new Set();
class Poller {
  constructor(fn, ms, onPause) { this.fn = fn; this.ms = ms; this.onPause = onPause; this.on = false; this.seq = 0; POLLERS.add(this); }
  start() { if (this.on) return; this.on = true; this.kick(); }
  stop() { this.on = false; clearTimeout(this.t); this.seq++; }
  kick() { clearTimeout(this.t); if (!this.on) return; if (document.hidden) { this.onPause?.(); return; } this.run(); }
  async run() {
    const my = ++this.seq;
    try { await this.fn(); } catch (e) { console.warn('res poll', e); }
    if (this.on && my === this.seq && !document.hidden) this.t = setTimeout(() => this.kick(), this.ms);
  }
}
document.addEventListener('visibilitychange', () => {
  for (const p of POLLERS) if (p.on) { if (document.hidden) { clearTimeout(p.t); p.seq++; p.onPause?.(); } else p.kick(); }
});

// ---------- glance cache (workspace / home / chat), kept in localStorage for gates + first paint ----------
const GL = { d: ls.get('res.glance', null), errAt: 0 };
const gpuBusy = d => !!d && ((d.gpu?.[0]?.util ?? 0) >= 30 || [...(d.agents || []), ...(d.closed || [])].some(a => (a.gpuMem || 0) >= 512 * MB));
function setGlance(d) {
  const lite = { ...d, agents: (d.agents || []).map(({ top, ...a }) => a), closed: (d.closed || []).map(({ top, ...a }) => a), other: d.other ? { ...d.other, top: [] } : null };
  lite.t = Date.now(); lite._busy = gpuBusy(d); lite._notable = lite.agents.filter(a => a.notable?.length).map(a => a.pane);
  GL.d = lite; GL.errAt = 0; ls.set('res.glance', lite);
}
const fresh = (ms = 30000) => GL.d && Date.now() - GL.d.t < ms;
const alertsFor = pane => (GL.d?.alerts || []).filter(a => a.pane === pane);
const noRes = e => { if (e?.status === 404) { G.noRes = true; return true; } return false; };

// ---------- hooks used by app.js (cards, jobs, home strip) ----------
const JR = new Map(); // job id → res (from /api/jobs while the workspace polls)
function cardLine(a) {
  if (FW.kind !== 'ws' || !fresh(60000)) return '';
  const r = GL.d.agents?.find(x => x.pane === a.pane);
  if (!r?.notable?.length) return '';
  const al = new Set(alertsFor(a.pane).map(x => x.rule));
  const hot = k => (k === 'gpu' && [...al].some(isGpuRule)) || (k === 'cpu' && al.has('runaway')) || (k === 'write' && al.has('agent-write')) ? ' class="hot"' : '';
  const parts = r.notable.map(k => k === 'gpu' ? `${GPUI}<b${hot(k)}>${fb(r.gpuMem)}${r.gpuUtil != null ? ' ' + Math.round(r.gpuUtil) + '%' : ''}</b>`
    : k === 'cpu' ? `<span${hot(k)}>CPU <b>${Math.round(r.cpu)}%</b></span>` : k === 'rss' ? fb(r.rss) : k === 'write' ? `<span${hot(k)}>W ${fb(r.wBps)}/s</span>` : '').filter(Boolean);
  return `<span class="resl${GL.errAt && Date.now() - GL.errAt > 10000 ? ' stale' : ''}" data-rpane="${esc(a.pane)}" role="link" aria-label="Resources">${parts.join(' · ')}<span class="go">›</span></span>`;
}
function jobLine(j) {
  if (j.status !== 'running' || FW.kind !== 'ws') return '';
  const r = JR.get(j.id) || j.res; if (!r) return '';
  return `<span class="jlive" data-rpid="${r.pid}" role="link">${r.gpuMem ? `${GPUI}<b>${r.gpuUtil != null ? Math.round(r.gpuUtil) + '%' : '—'}</b> · ${fb(r.gpuMem)} · ` : ''}CPU ${r.gpuMem ? '' : '<b>'}${Math.round(r.cpu)}%${r.gpuMem ? '' : '</b>'}${r.gpuMem ? '' : ' · ' + fb(r.rss)}</span>`;
}
function gpuSeg() {
  if (FW.kind !== 'home' || !fresh(30000) || !GL.d._busy) return '';
  const g = GL.d.gpu?.[0], hot = (GL.d.alerts || []).some(a => isGpuRule(a.rule)) || (g?.tempC ?? 0) >= 80;
  return `<button class="seg gpu${hot ? ' hot' : ''}" data-s="gpu" aria-label="GPU">${R.chip}${g?.util != null ? Math.round(g.util) + '%' : fb(g?.memUsed)}</button>`;
}
G.res = { cardLine, jobLine, gpuSeg };

// ---------- follow the visible view (called by app.js on every route / state) ----------
const FW = { v: null, want: null, kind: null, sig: '' };
const glP = new Poller(glanceTick, 5000, () => paint());
function gate(v, want) {
  if (want === 'ws') return true;
  const f = fresh(600000);
  if (want === 'home') return G.st?.jobsRunning > 0 || !!(f && GL.d?._busy);
  if (want === 'chat') return agentOf(v?.pane)?.jobs?.running > 0 || !!(f && GL.d?._notable?.includes(v?.pane));
  return false;
}
export function follow(v, want) {
  if (G.noRes) want = null;
  const kind = want && gate(v, want) ? want : null;
  if (FW.v === v && FW.kind === kind) return;
  const prev = FW;
  if (prev.kind === 'home' && prev.v) patchStrip(prev.v, '');
  Object.assign(FW, { v, want, kind, sig: '' });
  if (want === 'chat' && !kind) { const g = $('.gauge', v.el); if (g) g.hidden = true; }
  if (!kind) { glP.stop(); return; }
  if (kind === 'ws') wsAttach(v);
  paint();
  if (!glP.on) glP.start(); else if (!fresh(4500)) glP.kick();
}
async function glanceTick() {
  try { setGlance(await api('res?tier=glance')); }
  catch (e) { if (noRes(e)) { glP.stop(); follow(FW.v, null); return; } GL.errAt ||= Date.now(); }
  if (FW.kind === 'ws' && G.st?.jobsRunning > 0) {
    try { const js = (await api('jobs')).jobs || []; JR.clear(); for (const j of js) if (j.res) JR.set(j.id, j.res); } catch { }
  }
  if (FW.want && FW.want !== 'ws' && !gate(FW.v, FW.want)) { const v = FW.v, w = FW.want; FW.v = null; follow(v, w); return; }
  paint();
}
function paint() {
  const v = FW.v; if (!v || !FW.kind) return;
  if (FW.kind === 'ws') {
    gaugeTint();
    const sig = JSON.stringify([(G.st?.agents || []).map(cardLine), [...JR], GL.errAt > 0]);
    if (sig !== FW.sig) { FW.sig = sig; v.resRedraw?.(); }
  } else if (FW.kind === 'home') patchStrip(v, gpuSeg());
  else if (FW.kind === 'chat') {
    const r = fresh(60000) && GL.d.agents?.find(x => x.pane === v.pane);
    let b = $('.gauge', v.el);
    if (!b) {
      b = h(`<button class="ib gauge" hidden aria-label="Machine resources">${R.gauge}</button>`);
      const at = $('.files', v.el) || $('.scr', v.el); at?.before(b);
      b.onclick = () => { location.hash = '#/r?pane=' + enc(v.pane); };
    }
    b.hidden = !r?.notable?.length;
    b.classList.toggle('hot', alertsFor(v.pane).length > 0);
  }
}
function patchStrip(v, html) {
  const box = $('.stripbox .strip', v.el); if (!box) return;
  const cur = $('.seg.gpu', box);
  if (cur && !html) cur.remove();
  else if (cur && cur.outerHTML !== html) cur.outerHTML = html;
  else if (!cur && html) $('.seg.ws', box)?.insertAdjacentHTML('beforebegin', html);
}
function wsAttach(v) {
  if (v.el._resClicks) return;
  v.el._resClicks = 1;
  v.el.addEventListener('click', e => {
    const l = e.target.closest('.resl[data-rpane], .jlive[data-rpid]'); if (!l) return;
    e.preventDefault(); e.stopPropagation(); haptic();
    location.hash = l.dataset.rpane ? '#/r?pane=' + enc(l.dataset.rpane) : '#/r?pid=' + l.dataset.rpid;
  }, true);
}
// The tab bar's Machine tab (app.js tabBar): a dot, amber (warn) / red (alert) only, never a banner.
// Busy (a training run pinning the GPU) is normal and stays plain.
function gaugeTint() {
  const b = document.querySelector('.tabbar [data-tab="machine"]'); if (!b) return;
  const d = fresh(120000) ? GL.d : null, al = d?.alerts || [], lv = d?.headline?.level;
  const red = lv === 'alert' || al.some(a => ['mem-low', 'disk-low', 'runaway'].includes(a.rule) || isGpuRule(a.rule));
  const amb = !red && (lv === 'warn' || al.length > 0);
  b.classList.toggle('hot', amb); b.classList.toggle('alert', red);
}

// ---------- router entry ----------
let LV = null, DV = null;
export function route(hs) {
  const [p, qs] = hs.split('?'), q = new URLSearchParams(qs || '');
  if (p === '/r') return { key: 'r', make: () => (LV ||= new LiveView()), focus: () => { if (qs) history.replaceState(history.state, '', '#/r'); LV?.focus(q); } };
  const path = q.get('path');
  if (!path) return { key: 'r/disk', make: () => (DV ||= new DiskView(null)), focus: null };
  return { key: 'r/disk?' + path, make: () => new DiskView(path), focus: null };
}

// Machine tab segments: Live · Disk · Screen (fleet.js desktop shots) · Usage (git.js sheet)
function segHTML(on) { return `<div class="segc">${[['live', 'Live'], ['disk', 'Disk'], ['desk', 'Screen'], ['usage', 'Usage']].map(([k, l]) => `<button data-seg="${k}"${on === k ? ' class="on"' : ''}>${l}</button>`).join('')}</div>`; }
function segClick(e) {
  const b = e.target.closest('[data-seg]'); if (!b || b.classList.contains('on')) return false;
  const k = b.dataset.seg;
  if (k === 'usage') { lazy('git').then(m => m.usageSheet()); return true; }
  location.replace({ disk: '#/r/disk', desk: '#/fleet/desk' }[k] || '#/r'); return true;
}
const noServer = '<div class="stline">The Machine view needs the phase-2 server (GET /api/res is missing).</div>';

// keyed children: replace a row only when its html changed; vanished rows fade out
function sync(box, items) {
  const cur = box._m ||= new Map(), live = new Set(); let prev = null;
  for (const [k, html] of items) {
    live.add(k); let c = cur.get(k);
    if (!c) { c = { el: h(html), html }; cur.set(k, c); }
    else if (c.html !== html) { const n = h(html); if (c.el.parentNode === box) c.el.replaceWith(n); c.el = n; c.html = html; }
    const want = prev ? prev.nextSibling : box.firstChild;
    if (want !== c.el) box.insertBefore(c.el, want);
    prev = c.el;
  }
  for (const [k, c] of cur) if (!live.has(k)) { cur.delete(k); c.el.classList.add('gone'); setTimeout(() => c.el.remove(), 400); }
}

// ---------- Live view (#/r) ----------
const SORTK = { cpu: a => a.cpu || 0, rss: a => a.rss || 0, gpu: a => a.gpuMem || 0 };
class LiveView {
  constructor() {
    this.pane = null; this.keep = true; this.d = null; this.okAt = 0; this.errAt = 0; this.tog = new Map(); this.watched = new Set();
    try { for (const [k, v] of JSON.parse(sessionStorage.getItem('mm.res.open') || '[]')) this.tog.set(k, v); } catch { }
    const sk = ls.get('res.sort', null); this.sortPick = sk && Date.now() - sk.t < 3600e3 ? sk.k : null;
    this.el = h(`<div class="view resv live">
      <header class="bar"><div class="bar-in">
        <button class="ib back" aria-label="Back">${I.back}</button>
        <div class="ttl"><div class="t1">Machine</div><div class="t2">${esc(G.st?.host || '')}</div></div>
        <span class="livedot"><i></i><span>2s</span></span>
        <button class="ib gear" aria-label="Settings">${I.gear}</button>
      </div></header>
      <div class="rstale" hidden></div>
      <div class="scroll rsc">
        ${segHTML('live')}
        <button class="hl lv-ok"><span class="hlt">Sampling…</span></button>
        <div class="rc gpuc"><div class="rc-h"><b>GPU</b></div><div class="bigv">—</div></div>
        <div class="two"><div class="rc cpuc"><div class="rc-h"><b>CPU</b></div><div class="bigv">—</div></div><div class="rc memc"><div class="rc-h"><b>RAM</b></div><div class="bigv">—</div></div></div>
        <div class="sortrow"><span class="lbl">By agent</span><span class="sp"></span><button class="sc" data-k="cpu">CPU</button><button class="sc" data-k="rss">RAM</button><button class="sc" data-k="gpu">GPU</button></div>
        <div class="grpbox ags"><div class="legend"><span class="cols"><span>CPU</span><span>RAM</span><span class="vc">VRAM</span></span><span class="lsp"></span></div><div class="gl"></div></div>
        <div class="closedb"></div>
        <div class="grpbox other"><div class="gl"></div></div>
        <div class="foot2">Sampling every 2 s while open · stops when you leave</div>
      </div></div>`);
    this.sc = $('.rsc', this.el);
    this.poll = new Poller(() => this.tick(), 2000, () => this.el.classList.add('paused'));
  }
  mount() {
    if (this.mounted) return this.resume();
    this.mounted = true;
    $('.back', this.el).onclick = () => goBack('#/agents');
    $('.gear', this.el).onclick = settingsSheet;
    this.sc.addEventListener('click', e => this.onClick(e));
    longPress(this.sc, '.pr[data-pid], .gp[data-pid]', el => { this.procMenu(+el.dataset.pid); });
    this.resume();
  }
  resume() {
    if (G.noRes) { this.sc.innerHTML = noServer; return; }
    if (!this.d && fresh(60000)) { this.d = GL.d; this.render(); this.el.classList.add('paused'); }
    if (this.scY != null) this.sc.scrollTop = this.scY;
    this.loadWatches();
    this.poll.start();
    clearInterval(this.tk); this.tk = setInterval(() => this.staleCheck(), 1000);
  }
  suspend() { this.scY = this.sc.scrollTop; this.poll.stop(); clearInterval(this.tk); }
  destroy() { this.suspend(); }
  onState() { if (this.d && !document.hidden) this.render(); }
  async loadWatches() {
    try { const r = await api('res/watch'); this.watched = new Set((r.watches || []).map(w => w.pid)); this.render(); } catch { }
  }
  async tick() {
    try {
      const d = await api('res?tier=live' + (this.full ? '&pane=' + enc(this.full) : ''));
      this.d = d; this.okAt = Date.now(); this.errAt = 0; setGlance(d);
      this.el.classList.remove('paused', 'stale');
      const dot = $('.livedot', this.el); dot.classList.remove('beat'); void dot.offsetWidth; dot.classList.add('beat');
      this.render();
      if (this.want) this.applyWant();
    } catch (e) {
      if (noRes(e)) { this.poll.stop(); this.sc.innerHTML = noServer; return; }
      this.errAt ||= Date.now();
    }
    this.staleCheck();
  }
  staleCheck() {
    const b = $('.rstale', this.el), on = !!(this.d && this.errAt && Date.now() - (this.okAt || this.errAt) > 6000);
    b.hidden = !on; this.el.classList.toggle('stale', on);
    if (on) b.textContent = `Not updating · last sample ${this.okAt ? Math.round((Date.now() - this.okAt) / 1000) + ' s ago' : 'unknown'}`;
  }
  sortKey(d) { return this.sortPick && (this.sortPick !== 'gpu' || d.gpu?.length) ? this.sortPick : gpuBusy(d) ? 'gpu' : 'cpu'; }
  isOpen(g, def) { return this.tog.has(g) ? this.tog.get(g) : def; }
  focus(q) {
    const f = q.get('f'), pane = q.get('pane'), pid = q.get('pid');
    if (!f && !pane && !pid) return;
    this.want = { f, pane, pid: pid && /^\d+$/.test(pid) ? +pid : null };
    if (this.want.pane) { this.full = this.want.pane; this.tog.set('a:' + pane, true); this.tog.set('c:' + pane, true); }
    if (this.want.pid != null) { processSheet(this.want.pid); this.want.pid = null; }
    if (pid && !/^\d+$/.test(pid)) toast('Process not found', 'err');
    if (this.d && this.okAt) this.applyWant();
  }
  applyWant() {
    const w = this.want; this.want = null; if (!w) return;
    let el = null;
    if (w.f) el = $({ cpu: '.cpuc', mem: '.memc', gpu: '.gpuc' }[w.f] || '.gpuc', this.el);
    if (w.pane) {
      el = this.sc.querySelector(`.gh[data-g="a:${CSS.escape(w.pane)}"], .gh[data-g="c:${CSS.escape(w.pane)}"]`);
      if (!el) toast('No processes for ' + esc(w.pane), '', null, 2400);
    }
    if (!el) return;
    requestAnimationFrame(() => {
      const sr = this.sc.getBoundingClientRect(), r = el.getBoundingClientRect();
      this.sc.scrollTop += r.top - sr.top - this.sc.clientHeight * 0.3;
      el.classList.remove('flash'); void el.offsetWidth; el.classList.add('flash');
      setTimeout(() => el.classList.remove('flash'), 1300);
    });
  }
  onClick(e) {
    if (segClick(e)) return;
    const t = e.target;
    const sc = t.closest('.sc[data-k]');
    if (sc) { this.sortPick = sc.dataset.k; ls.set('res.sort', { k: sc.dataset.k, t: Date.now() }); haptic(); this.render(); return; }
    const ch = t.closest('[data-chat]');
    if (ch) { const a = agentOf(ch.dataset.chat); if (a) location.hash = agentHref(a); return; }
    const am = t.closest('[data-askm]');
    if (am) { const c = this.d?.closed?.find(x => x.pane === am.dataset.askm); askAgent(G.st?.home, `About the closed tab ${am.dataset.askm}: its processes are still running (${c?.procs ?? '?'} procs, ${fb(c?.rss)} RAM${c?.gpuMem ? ', ' + fb(c.gpuMem) + ' VRAM' : ''}). `); return; }
    const w = t.closest('[data-watch]');
    if (w) { watchPid(+w.dataset.watch).then(ok => { if (ok) { this.watched.add(+w.dataset.watch); this.render(); } }); return; }
    const gh = t.closest('.gh[data-g]');
    if (gh) { const g = gh.dataset.g; this.tog.set(g, !gh.classList.contains('open')); this.saveTog(); this.render(); return; }
    const pr = t.closest('[data-pid]');
    if (pr) { processSheet(+pr.dataset.pid); return; }
    const hl = t.closest('.hl');
    if (hl && this.d?.headline) { const x = this.d.headline; if (x.pid) processSheet(x.pid); else if (x.pane) { this.want = { pane: x.pane }; this.tog.set('a:' + x.pane, true); this.tog.set('c:' + x.pane, true); this.render(); this.applyWant(); } }
  }
  saveTog() { try { sessionStorage.setItem('mm.res.open', JSON.stringify([...this.tog])); } catch { } }
  async procMenu(pid) {
    const p = allProcs(this.d).find(x => x.p.pid === pid); if (!p) return false;
    const owner = p.grp === 'agent' ? p.pane : G.st?.home;
    const sh = sheet(`<h2 class="jt">${esc(p.p.label)}</h2><div class="acts"><button class="act" data-a="pid">Copy PID ${pid}</button><button class="act" data-a="cmd">Copy command</button><button class="act" data-a="ask">Ask ${esc(atitle(agentOf(owner), HOME_NAME()))} about it…</button></div>`, 'compact');
    let cmd = p.p.label; api('res/p/' + pid).then(r => { cmd = r.cmd || cmd; }, () => { });
    sh.addEventListener('click', e => {
      const b = e.target.closest('[data-a]'); if (!b) return;
      sh.close();
      if (b.dataset.a === 'pid') copyText(String(pid));
      else if (b.dataset.a === 'cmd') copyText(cmd);
      else askAgent(owner, askText(p.p, cmd));
    });
  }
  render() {
    const d = this.d; if (!d) return;
    const q = s => $(s, this.el), g = d.gpu?.[0], noGpu = d.gpuErr === 'no-nvidia' || (!d.gpu?.length && !d.gpuErr);
    q('.t2').textContent = [G.st?.host, d.cpu?.ncpu && d.cpu.ncpu + ' cores', g && gname(g.name)].filter(Boolean).join(' · ');
    q('.livedot span').textContent = this.el.classList.contains('paused') ? 'paused' : Math.round((d.interval || 2000) / 1000) + 's';
    // headline
    const hl = d.headline || { text: '', level: 'ok' }, hle = q('.hl');
    hle.className = 'hl lv-' + (hl.level || 'ok');
    let txt = esc(hl.text || '').replace(/· (.+?) in /, '· <b>$1</b> in ').replace(/\(?\b(w\d+:p\w+)\b\)?/g, (m, p) => { const a = agentOf(p); return `<span class="pchip${ccls(a?.color)} ps-${a ? dispStatus(a) : 'gone'}"><span class="pcl">${esc(p)}</span></span>`; });
    put(hle, `${hl.level === 'alert' || hl.level === 'warn' ? R.bell : /^GPU|VRAM/.test(hl.text) ? R.chip : R.pulse}<span class="hlt">${txt}</span>${hl.pid || hl.pane ? '<span class="go">›</span>' : ''}`);
    this.renderGpu(d, g, noGpu);
    // cpu
    const c = d.cpu || {};
    put(q('.cpuc'), `<div class="rc-h"><b>CPU</b><span class="r${(c.tempC ?? 0) >= 85 ? ' hot' : ''}">${c.tempC != null ? c.tempC + '°C' : ''}</span></div>
      <div class="bigv">${c.pct == null ? '—' : Math.round(c.pct)}<small>% of ${c.ncpu || '?'}</small></div>
      <div class="cores">${(c.cores || []).map(x => `<i style="height:${Math.max(4, Math.min(100, x)).toFixed(0)}%;background:${mcol(x)}"></i>`).join('')}</div>
      <div class="tiny">load ${(c.load || []).map(x => x.toFixed(1)).join(' ')}</div>`);
    // ram
    const m = d.mem || {}, tmp = (d.volumes || []).find(v => v.ram), up = m.total ? m.used / m.total * 100 : 0, tp = tmp && m.total ? Math.min(tmp.used / m.total * 100, up) : 0;
    put(q('.memc'), `<div class="rc-h"><b>RAM</b><span class="r">swap ${m.swapUsed ? fb(m.swapUsed) : '0'}</span></div>
      <div class="bigv">${m.used != null ? g1(m.used) : '—'}<small>/ ${m.total ? g1(m.total) : '—'} G</small></div>
      <div class="mbar2"><i style="width:${(up - tp).toFixed(1)}%;background:${up >= 85 ? mcol(up) : 'var(--work)'}"></i><i style="width:${tp.toFixed(1)}%;background:var(--warn)"></i></div>
      <div class="tiny">${tmp ? `${esc(tmp.mount)} holds ${g1(tmp.used)} G <span class="sq">■</span>` : `${fb(m.avail)} available`}</div>`);
    // sort chips
    const sk = this.sortKey(d);
    this.el.querySelectorAll('.sc').forEach(b => { b.classList.toggle('on', b.dataset.k === sk); b.hidden = b.dataset.k === 'gpu' && noGpu; });
    this.el.classList.toggle('nogpu', noGpu);
    this.renderGroups(d, sk, noGpu);
  }
  renderGpu(d, g, noGpu) {
    const el = $('.gpuc', this.el);
    el.classList.toggle('one', noGpu);
    if (noGpu) { put(el, `<div class="nogpu">${R.chip}No NVIDIA GPU (nvidia-smi not found)</div>`); return; }
    const failed = !!d.gpuErr;
    if (!failed) this.lastGpu = g; else g = this.lastGpu || g;
    el.classList.toggle('dimmed', failed);
    if (!g) { put(el, `<div class="nogpu">GPU not responding — retrying</div>`); return; }
    const holders = gpuHolders(d), tot = g.memTotal || 1;
    const hot = (g.tempC ?? 0) >= 80;
    const hist = g.hist?.util || [], span = Math.round(hist.length * (d.interval || 2000) / 60000);
    const big = holders.find(x => x.grp !== 'other' && x.gpuMem >= 512 * MB && !this.watched.has(x.pid));
    put(el, `<div class="rc-h"><b>GPU</b><span class="dim">${esc(gname(g.name))}</span><span class="r"><span class="${hot ? 'hot' : ''}">${g.tempC != null ? g.tempC + '°C' : '—'}</span>${g.powerW != null ? ` · ${Math.round(g.powerW)}/${g.powerCapW ?? '?'} W` : ''}${g.fanPct != null ? ` · fan ${g.fanPct}%` : ''}</span></div>
      ${failed ? `<div class="gerr">GPU not responding — retrying</div>` : ''}
      <div class="gpu-top"><div class="bigv">${g.util == null ? '—' : Math.round(g.util)}<small>%</small></div>${spark(hist, { h: 34, w: 200 })}</div>
      <div class="vl"><span>VRAM <b>${g1(g.memUsed)}</b> of ${g1(g.memTotal)} GB</span><span>${span ? span + ' min' : ''}</span></div>
      <div class="vram">${holders.map(x => `<i style="width:${(x.gpuMem / tot * 100).toFixed(2)}%;background:${x.col}"></i>`).join('')}</div>
      ${holders.map(x => `<button class="gp" data-pid="${x.pid}"><span class="sw" style="background:${x.col}"></span><span class="nm">${esc(x.label)}<small>${esc(x.who)}</small></span><span class="num b">${fb(x.gpuMem)}</span><span class="num">${pc(x.gpuUtil)}</span><span class="go">›</span></button>`).join('')}
      ${big ? `<div class="offer">${R.bell}<span>Notify me when this run ends or stalls</span><button class="btn sm" data-watch="${big.pid}">Watch</button></div>` : ''}`);
  }
  renderGroups(d, sk, noGpu) {
    const key = SORTK[sk], cols = (r, quiet) => `<span class="cols${quiet ? ' q' : ''}"><span class="${sk === 'cpu' ? 'b' : ''}">${pc(r.cpu)}</span><span class="${sk === 'rss' ? 'b' : ''}">${fb(r.rss)}</span><span class="vc${sk === 'gpu' ? ' b' : ''}">${r.gpuMem ? fb(r.gpuMem) : '—'}</span></span>`;
    const procRow = (p, other) => {
      const labs = [p.job ? '<span class="lab jb">job</span>' : '', p.detached ? '<span class="lab">detached</span>' : '', other && p.tag ? `<span class="lab">${esc(p.tag)}</span>` : ''].join('');
      const sub = p.chain?.length ? esc(p.chain.join(' › ')) + ' ›' : p.name && p.name !== p.label ? esc(p.name) : '';
      return `<div class="pr" data-pid="${p.pid}" style="--d:${Math.min(2, p.depth || 0)}"><span class="pn"><div>${esc(p.label)}${p.n > 1 ? ' ×' + p.n : ''}</div>${sub || labs ? `<small>${sub}${labs}</small>` : ''}</span>${cols(p)}<span class="go">›</span></div>`;
    };
    const rest = (r, top) => {
      const n = r.procs - top.reduce((s, p) => s + (p.n || 1), 0); if (n <= 0) return '';
      const cpu = r.cpu - top.reduce((s, p) => s + (p.cpu || 0), 0), rss = r.rss - top.reduce((s, p) => s + (p.rss || 0), 0);
      return `<div class="more2">+ ${n} small${cpu >= 0.5 ? ' · ' + pc(cpu) : ''}${rss > MB ? ' · ' + fb(rss) : ''}</div>`;
    };
    // agents
    const ags = [...(d.agents || [])].sort((a, b) => key(b) - key(a));
    const items = [];
    ags.forEach((r, k) => {
      const a = agentOf(r.pane), s = a ? dispStatus(a) : 'unknown', g = 'a:' + r.pane, quiet = !r.notable?.length;
      const open = this.isOpen(g, k === 0 && !quiet);
      items.push([g, `<div class="gh${ccls(a?.color)}${open ? ' open' : ''}" data-g="${esc(g)}"><span class="tri"></span><span class="gt"><b>${esc(atitle(a, r.pane))}</b><small><i style="background:${DOT[s]}"></i>${esc(r.pane)} · ${r.procs} proc${r.procs === 1 ? '' : 's'}</small></span>${cols(r, quiet)}${a ? `<button class="chatb" data-chat="${esc(r.pane)}" aria-label="Chat">${I.chat}</button>` : '<span class="lsp"></span>'}</div>`]);
      if (open) {
        const top = [...(r.top || [])];
        for (const p of top) items.push(['p:' + p.pid, procRow(p)]);
        const m = rest(r, top); if (m) items.push(['m:' + r.pane, m]);
      }
    });
    sync($('.ags .gl', this.el), items);
    $('.ags', this.el).hidden = !ags.length;
    // closed tabs
    const cb = $('.closedb', this.el), cl = d.closed || [];
    const citems = [];
    for (const c of cl) {
      const g = 'c:' + c.pane, open = this.isOpen(g, true);
      citems.push([g, `<div class="grpbox closed"><div class="gh${open ? ' open' : ''}" data-g="${esc(g)}"><span class="tri"></span><span class="gt"><b>Closed tab ${esc(c.pane)} · still running</b><small>${c.procs} proc${c.procs === 1 ? '' : 's'}${c.gpuMem ? ' · holds ' + fb(c.gpuMem) + ' VRAM' : ''}</small></span>${cols(c)}<span class="lsp"></span></div>
        ${open ? (c.top || []).map(p => procRow(p)).join('') + `<div class="askrow"><button class="btn sm" data-askm="${esc(c.pane)}">${R.ask}Ask ${esc(HOME_NAME())} about it…</button></div>` : ''}</div>`]);
    }
    sync(cb, citems);
    // other
    const o = d.other;
    const ob = $('.other', this.el); ob.hidden = !o;
    if (o) {
      const otop = [...(o.top || [])].sort((a, b) => key(b) - key(a)).slice(0, 8);
      const allTop = [...ags.flatMap(a => a.top || []), ...cl.flatMap(c => c.top || []), ...otop].sort((a, b) => key(b) - key(a))[0];
      const auto = !!allTop && otop.includes(allTop) && key(allTop) > 0;
      const open = this.isOpen('o', auto);
      const tags = [...new Set((o.top || []).map(p => p.tag).filter(Boolean))].slice(0, 3).join(', ');
      const oi = [['o', `<div class="gh${open ? ' open' : ''}" data-g="o"><span class="tri"></span><span class="gt"><b class="ot">Other — not from an agent</b><small>${o.procs} procs${tags ? ' · ' + esc(tags) : ''}</small></span>${cols(o, true)}<span class="lsp"></span></div>`]];
      if (open) for (const p of otop) oi.push(['p:' + p.pid, procRow({ ...p, depth: 0 }, true)]);
      sync($('.gl', ob), oi);
    }
  }
}
function allProcs(d) {
  if (!d) return [];
  return [...(d.agents || []).flatMap(a => (a.top || []).map(p => ({ p, grp: 'agent', pane: a.pane }))),
  ...(d.closed || []).flatMap(c => (c.top || []).map(p => ({ p, grp: 'closed', pane: c.pane }))),
  ...(d.other?.top || []).map(p => ({ p, grp: 'other', pane: null }))];
}
function gpuHolders(d) {
  const out = [], desk = [];
  for (const { p, grp, pane } of allProcs(d)) {
    if (!(p.gpuMem > 0)) continue;
    if (grp === 'other') { desk.push(p); continue; }
    const a = agentOf(pane);
    out.push({ pid: p.pid, label: p.label, who: grp === 'closed' ? 'closed tab ' + pane : atitle(a, pane), gpuMem: p.gpuMem, gpuUtil: p.gpuUtil, grp, col: grp === 'closed' ? 'var(--warn)' : a?.color && COLORS.includes(a.color) ? `var(--p-${a.color})` : 'var(--accent)' });
  }
  out.sort((a, b) => b.gpuMem - a.gpuMem);
  if (desk.length) {
    desk.sort((a, b) => b.gpuMem - a.gpuMem);
    const tags = [...new Set(desk.map(p => p.tag).filter(Boolean))];
    out.push({ pid: desk[0].pid, label: desk.slice(0, 2).map(p => p.label).join(', ') + (desk.length > 2 ? ' +' + (desk.length - 2) : ''), who: tags.includes('desktop') || tags.includes('browser') ? 'desktop' : 'other', gpuMem: desk.reduce((s, p) => s + p.gpuMem, 0), gpuUtil: Math.max(...desk.map(p => p.gpuUtil ?? 0)), grp: 'other', col: 'var(--k-other)' });
  }
  return out;
}

// ---------- shared actions ----------
function askAgent(pane, text) {
  const a = agentOf(pane) || agentOf(G.st?.home);
  if (!a) { toast('No agent to ask', 'err'); return; }
  const cur = ls.get('draft.' + a.pane, '');
  ls.set('draft.' + a.pane, cur ? cur.replace(/\s*$/, '\n\n') + text : text);
  closeSheets(); haptic();
  location.hash = agentHref(a);
  toast('Draft ready — review and send', 'ok', null, 2200);
}
const askText = (p, cmd) => `About pid ${p.pid} (${String(cmd || p.label).slice(0, 120)}, ${pc(p.cpu)} CPU${p.gpuMem ? ', ' + fbs(p.gpuMem) + ' VRAM' : ''}): `;
async function watchPid(pid) {
  try { const r = await post('res/watch', { pid }); haptic(); toast('Watching — you get a push when it ends or stalls', 'ok', null, 2600); return r.key || true; }
  catch (e) { toast(e.status === 404 ? 'That process is gone' : 'Watch failed: ' + esc(e.message), 'err'); return false; }
}

// ---------- process sheet (#/r?pid=N) ----------
export function processSheet(pid) {
  closeSheets();
  const sh = sheet(`<div class="psh"><h2 class="pt">pid ${pid}</h2><div class="stline"><span class="spin"></span></div></div>`, 'tall rsh');
  const body = $('.psh', sh), hist = { cpu: [], rss: [] };
  let d = null, gone = false, jobs = null;
  const poll = new Poller(async () => {
    try {
      const r = await api('res/p/' + pid);
      d = r; if (r.ended) { gone = true; poll.stop(); }
      hist.cpu.push(r.cpu); hist.rss.push(r.rss); if (hist.cpu.length > 60) { hist.cpu.shift(); hist.rss.shift(); }
    } catch (e) {
      if (e.status === 404 || e.status === 400) {
        if (!d) { poll.stop(); sh.close(); toast('Process not found', 'err'); return; }
        gone = true; d.ended ||= Date.now(); poll.stop();
      } else return;
    }
    draw();
  }, 2000);
  poll.start();
  const close0 = sh.close; sh.close = () => { poll.stop(); POLLERS.delete(poll); close0(); };
  const draw = () => {
    if (!d) return;
    const a = d.group === 'agent' ? agentOf(d.pane) : null, s = a ? dispStatus(a) : null;
    const gT = (LV?.d || GL.d)?.gpu?.[0]?.memTotal;
    const who = d.group === 'agent' ? `<i style="background:${DOT[s || 'unknown']}"></i>${s ? esc(s) + ' · ' : ''}<b class="ach${ccls(a?.color)}" data-chat="${esc(d.pane || '')}">${esc(whoName(d.agentTitle, d.pane) || atitle(a, d.pane))}</b> · ${esc(d.pane || '')}`
      : d.group === 'closed' ? `<i style="background:var(--warn)"></i>closed tab ${esc(d.pane || '')} · still running` : '<i style="background:var(--fg3)"></i>not from an agent';
    const pills = [d.tag ? `<span class="lab${d.tag === 'training' ? ' tr' : ''}">${esc(d.tag)}</span>` : '', d.job ? `<span class="lab jb">job · ${esc(d.job.desc || d.job.id)}</span>` : '', d.detached ? '<span class="lab">detached</span>' : '',
      d.ended ? `<span class="lab endp">Ended ${esc(ago(d.ended))}</span>` : ''].join('');
    const live = !!a, links = [];
    if (live) links.push(`<a class="btn" href="${d.job ? itemHref(d.job.tid, d.job.i, d.pane) : agentHref(a)}">${I.chat}Chat</a>`);
    if (live) links.push(`<a class="btn" href="#/s/${enc(d.pane)}">${I.term}Terminal</a>`);
    if (d.job) links.push(`<button class="btn" data-a="log">${R.log}Job log</button>`);
    const owner = live ? d.pane : G.st?.home;
    put(body, `<h2 class="pt">${esc(d.label)}${d.n > 1 ? ' ×' + d.n : ''}</h2>
      <div class="who${ccls(a?.color)}">${who}</div>
      ${pills ? `<div class="pills">${pills}</div>` : ''}
      <div class="stats${gone ? ' gone' : ''}">
        <div class="stat"><div class="cap">CPU</div><div class="v">${pc(d.cpu)}</div>${spark(hist.cpu, { h: 18, max: autoMax(hist.cpu) })}</div>
        <div class="stat"><div class="cap">RAM</div><div class="v">${fb(d.rss)}</div><span class="rsp">${spark(hist.rss, { h: 18, max: Math.max(...hist.rss, 1) * 1.15 })}</span></div>
        <div class="stat"><div class="cap">VRAM</div><div class="v">${d.gpuMem ? fb(d.gpuMem) : '—'}</div><div class="s2">${d.gpuMem ? `GPU ${pc(d.gpuUtil)}${gT ? ' · of ' + g1(gT) : ''}` : 'no GPU memory'}</div></div>
      </div>
      ${links.length ? `<div class="links n${links.length}">${links.join('')}</div>` : ''}
      <div class="pacts">${gone ? '' : `<button class="btn${d.watched ? ' on' : ''}" data-a="watch">${R.bell}${d.watched ? 'Watching' : 'Watch this run'}</button>`}<button class="btn" data-a="ask" data-owner="${esc(owner || '')}">${R.ask}Ask ${live ? 'agent' : HOME_NAME()}…</button></div>
      <h3>Command</h3>
      <div class="pcmd" data-a="copy">${esc(d.cmd || d.label)}</div>
      <div class="pkv">${d.cwd ? `cwd <b>${esc(shortPath(d.cwd))}</b> · ` : ''}pid <b>${d.pid}</b>${d.start ? ` · up <b>${dur((d.ended || Date.now()) - d.start)}</b>` : ''}<br>${d.io ? `wrote <b>${fbs(d.io.write)}</b>${d.io.wBps ? ' · ' + fbs(d.io.wBps).replace(' ', ' ') + '/s' : ''} · ` : ''}${d.threads ?? '?'} threads · state ${esc(d.state || '?')}</div>
      ${d.ancestors?.length ? `<h3>Parents</h3><div class="chain">${d.ancestors.map(x => x.pid > 1 ? `<a data-pid="${x.pid}">${esc(x.name)} ${x.pid}</a>` : esc(x.name)).join(' › ')} › <b>${esc(d.name)} ${d.pid}</b></div>` : ''}
      ${d.children?.length ? `<h3>Children (${d.children.length})</h3><div class="lbox">${d.children.map(c => `<div class="pr" data-pid="${c.pid}" style="--d:0"><span class="pn"><div>${esc(c.label)}${c.n > 1 ? ' ×' + c.n : ''}</div><small>${esc(c.name)}</small></span><span class="cols"><span>${pc(c.cpu)}</span><span>${fb(c.rss)}</span></span><span class="go">›</span></div>`).join('')}</div>` : ''}`);
  };
  sh.addEventListener('click', async e => {
    const t = e.target, b = t.closest('[data-a]');
    if (t.closest('a.btn')) { sh.close(); return; }
    const ch = t.closest('[data-chat]'); if (ch) { const a = agentOf(ch.dataset.chat); if (a) { sh.close(); location.hash = agentHref(a); } return; }
    const pp = !b && t.closest('[data-pid]'); if (pp) { processSheet(+pp.dataset.pid); return; }
    if (!b || !d) return;
    const act = b.dataset.a;
    if (act === 'copy') copyText(d.cmd || d.label);
    else if (act === 'ask') askAgent(b.dataset.owner, askText(d, d.cmd));
    else if (act === 'log') {
      try { jobs ||= (await api('jobs')).jobs || []; } catch { jobs = []; }
      const j = jobs.find(x => x.id === d.job?.id);
      if (!j) { toast('Job not found', 'err'); return; }
      sh.close(); import('./sheets.js').then(m => m.jobSheet(j));
    } else if (act === 'watch') {
      b.disabled = true;
      if (d.watched) {
        try { await api('res/watch/' + enc(`${d.pid}@${d.start}`), { method: 'DELETE' }); d.watched = false; LV?.watched.delete(d.pid); haptic(); } catch (x) { toast('Failed: ' + esc(x.message), 'err'); }
      } else if (await watchPid(d.pid)) { d.watched = true; LV?.watched.add(d.pid); }
      b.disabled = false; draw();
    }
  });
  return sh;
}

// ---------- Disk view (#/r/disk, #/r/disk?path=) ----------
const KCOL = { code: 'var(--k-code)', vcs: 'var(--k-code)', cache: 'var(--k-cache)', build: 'var(--k-build)', toolchain: 'var(--k-build)', agent: 'var(--k-agent)', worktree: 'var(--k-agent)', 'agent-scratch': 'var(--k-agent)', media: 'var(--k-media)' };
const kcol = k => KCOL[k] || 'var(--k-other)';
const KLAB = { cache: 'cache', build: 'build output', worktree: 'agent worktree', 'stale-experiment': 'old experiment', 'agent-scratch': 'agent scratch', code: 'code', toolchain: 'toolchain', media: 'media', agent: 'agent data', vcs: 'git data' };
const shortP = p => { const s = shortPath(p); if (s.length <= 34) return s; const seg = s.split('/'); let out = seg.pop(); while (seg.length && (seg.at(-1) + '/' + out).length < 32) out = seg.pop() + '/' + out; return '…/' + out; };
const RECL = new Set(['cache', 'build']);
class DiskView {
  constructor(path) {
    this.path = path; this.keep = !path; this.pane = null; this.sel = new Map(); this.selMode = false; this.allOpen = false; this.d = null;
    const name = path ? path.replace(/\/+$/, '').split('/').pop() : '';
    this.el = h(`<div class="view resv disk${path ? ' drill' : ''}">
      <header class="bar"><div class="bar-in">
        <button class="ib back" aria-label="Back">${I.back}</button>
        <button class="ttl up"><div class="t1">${path ? esc(name) : 'Machine'}</div><div class="t2">${path ? esc(shortPath(path.slice(0, path.length - name.length - 1) || '/')) : esc((G.st?.host || '') + ' · home folder')}</div></button>
      </div></header>
      <div class="scroll rsc">${path ? '' : segHTML('disk')}<div class="dbody"><div class="stline"><span class="spin"></span></div></div></div>
      <div class="selbar" hidden><span class="sl"></span><span class="sp"></span><button class="btn sm" data-a="copylist">Copy list</button><button class="btn sm primary" data-a="cleanup">Ask an agent…</button></div>
    </div>`);
    this.sc = $('.rsc', this.el); this.body = $('.dbody', this.el);
    this.poll = new Poller(() => this.load(), 1000);
  }
  mount() {
    if (this.mounted) return this.resume();
    this.mounted = true;
    $('.back', this.el).onclick = () => goBack(this.path ? '#/r/disk' : '#/agents');
    $('.up', this.el).onclick = () => {
      if (!this.path) return;
      const par = this.path.replace(/\/+$/, '').split('/').slice(0, -1).join('/');
      location.hash = !par || par === this.d?.root || par.length < (this.d?.root || '/home/x').length ? '#/r/disk' : '#/r/disk?path=' + enc(par);
    };
    this.sc.addEventListener('click', e => this.onClick(e));
    $('.selbar', this.el).addEventListener('click', e => { const b = e.target.closest('[data-a]'); if (b?.dataset.a === 'copylist') copyText([...this.sel.keys()].join('\n')); else if (b) this.cleanupSheet(); });
    longPress(this.sc, '[data-path]', el => { this.folderSheet(el.dataset.path, +el.dataset.bytes); });
    this.resume();
  }
  resume() {
    if (G.noRes) { this.body.innerHTML = noServer; return; }
    if (this.scY != null) this.sc.scrollTop = this.scY;
    this.load();
  }
  suspend() { this.scY = this.sc.scrollTop; this.poll.stop(); }
  destroy() { this.poll.stop(); POLLERS.delete(this.poll); }
  async load() {
    try {
      const d = await api('disk' + (this.path ? '?path=' + enc(this.path) : ''));
      this.d = d; this.err = null; this.render();
      if (d.scanning) this.poll.start(); else this.poll.stop();
    } catch (e) {
      if (noRes(e)) { this.poll.stop(); this.body.innerHTML = noServer; return; }
      this.poll.stop();
      if (e.status === 404) { this.body.innerHTML = `<div class="stline">This folder no longer exists.<br><br><button class="btn sm" data-a="up">Up</button></div>`; return; }
      if (!this.d) this.body.innerHTML = `<div class="stline">Couldn’t load: ${esc(e.message)} <button class="btn sm" data-a="retry">Retry</button></div>`;
    }
  }
  async rescan(path) {
    try { await post('disk/scan', path ? { path } : {}); toast('Scanning…', '', null, 1400); }
    catch (e) { if (e.status !== 409) { toast('Scan failed: ' + esc(e.message), 'err'); return; } }
    this.load();
  }
  onClick(e) {
    if (!this.path && segClick(e)) return;
    const t = e.target;
    const ch = t.closest('[data-chat]'); if (ch) { e.stopPropagation(); const a = agentOf(ch.dataset.chat); if (a) location.hash = agentHref(a); else toast('That agent is closed', '', null, 1600); return; }
    const b = t.closest('[data-a]');
    if (b) {
      const a = b.dataset.a;
      if (a === 'select') { this.selMode = !this.selMode; if (!this.selMode) this.sel.clear(); this.render(); }
      else if (a === 'rescan') this.rescan(this.path);
      else if (a === 'cancel') api('disk/scan', { method: 'DELETE' }).then(() => this.load(), x => toast(esc(x.message), 'err'));
      else if (a === 'all') { this.allOpen = !this.allOpen; this.render(); }
      else if (a === 'retry') this.load();
      else if (a === 'up') $('.up', this.el).click();
      else if (a === 'scx') { b.classList.toggle('open'); }
      return;
    }
    const r = t.closest('[data-path]'); if (!r) return;
    if (this.selMode) {
      const p = r.dataset.path;
      if (this.sel.has(p)) this.sel.delete(p); else this.sel.set(p, { bytes: +r.dataset.bytes, kind: r.dataset.kind || '' });
      haptic(); this.render(); return;
    }
    location.hash = '#/r/disk?path=' + enc(r.dataset.path);
  }
  ck(p) { return this.selMode ? `<span class="ck${this.sel.has(p) ? ' on' : ''}"></span>` : ''; }
  render() {
    const d = this.d; if (!d) return;
    const out = [], n = d.node, sc = d.scanning;
    if (!this.path) {
      out.push(`<div class="lbox vols">${(d.volumes || []).map(v => { const p = v.used / v.total * 100; return `<div class="vol"><div class="vt"><b>${esc(v.mount)}</b><small>${esc(v.fs)}</small>${v.ram ? '<span class="ram">RAM</span>' : ''}<span class="r">${v.ram ? `<b>${g1(v.used)} GB</b> of ${g1(v.total)}` : `<b>${fbs(v.avail)}</b> free of ${Math.round(v.total / GB)}`}</span></div><div class="vbar"><i style="width:${p.toFixed(1)}%;background:${v.ram ? 'var(--warn)' : mcol(p)}"></i></div></div>`; }).join('')}</div>`);
    }
    // scan status
    if (!d.scannedAt && sc) out.push(`<div class="scanb">${this.path ? '' : 'Scanning your home folder for the first time — low priority, ~3 s'}<div class="sct">${kn(sc.files)} files · ${fbs(sc.bytes)}</div><div class="pbar"><i></i></div></div>`);
    else if (!n && sc) out.push(`<div class="scanb">Scanning ${esc(shortPath(sc.path || this.path || ''))}… ${kn(sc.files)} files<div class="pbar"><i></i></div></div>`);
    if (d.error) out.push(`<div class="scerr">${esc(d.error)} · <button data-a="rescan">Retry</button></div>`);
    if (!n) {
      if (!sc) out.push(`<div class="stline">Not scanned yet. <button class="btn sm" data-a="rescan">Scan</button></div>`);
      if (!this.path) out.push('<div class="sec">Worth a look</div><div class="lbox skel"><i></i><i></i><i></i></div><div class="sec">Browse ~</div><div class="lbox skel"><i></i><i></i><i></i><i></i></div>');
      return this.paint(out);
    }
    // findings
    const fs = d.findings || [], ftot = fs.reduce((s, f) => s + f.bytes, 0);
    const finds = `<div class="sec">Worth a look${fs.length ? ' · ' + fbs(ftot) : ''}${fs.length ? `<button class="r" data-a="select">${this.selMode ? 'Done' : 'Select'}</button>` : ''}</div>` + (fs.length ? `<div class="lbox">${fs.map(f => `<div class="fr${ccls(agentOf(f.agent?.pane)?.color)}" data-path="${esc(f.path)}" data-bytes="${f.bytes}" data-kind="${esc(KLAB[f.kind] || f.kind)}">${this.ck(f.path)}<span class="kd${RECL.has(f.kind) ? ' hatch' : ''}" style="background-color:${kcol(f.kind)}"></span><span class="fb"><div>${esc(shortP(f.path))}</div><small>${esc(f.why || KLAB[f.kind] || f.kind)}${f.ageDays >= 3 ? ` · ${f.ageDays} days${f.kind === 'worktree' ? '' : ' old'}` : ''}${f.agent ? ` · <span class="achip" data-chat="${esc(f.agent.pane)}">${I.chat}${esc(f.agent.title || f.agent.pane)}</span>` : ''}</small></span><span class="sz">${fbs(f.bytes)}</span><span class="go">›</span></div>`).join('')}</div>`
      : `<div class="lbox"><div class="empty1">Nothing obvious to reclaim (≥ 64 MB)</div></div>`);
    if (!this.path) out.push(finds);
    // agent scratch
    if (!this.path && d.scratch?.length) out.push(`<div class="sec">Agent scratch</div><div class="lbox">${d.scratch.map(s => `<div class="scx" data-a="scx"><div class="scr1"><span class="mono">${esc(shortPath(s.path))}</span>${s.ram ? '<span class="ram">RAM</span>' : `<span class="nt">${esc((s.note || '').replace(/ are /, ' '))}</span>`}<span class="r">${fbs(s.bytes)}</span></div>${s.top?.length ? `<div class="sct2">${s.ram ? `<div class="nt">${esc(s.note || '')}</div>` : ''}${s.top.map(x => `<div class="scr2"><span>${esc(x.pane ? atitle(agentOf(x.pane), x.pane) : x.session || shortP(x.path))}</span><span class="r">${fbs(x.bytes)}</span></div>`).join('')}</div>` : ''}</div>`).join('')}</div>`);
    // browse
    const kids = n.children || [], more = n.more, lim = this.path ? 12 : 8, show = this.allOpen ? kids : kids.slice(0, lim), mx = Math.max(1, ...kids.map(k => k.bytes));
    const total = kids.length + (more?.count || 0);
    out.push(`<div class="sec">${this.path ? 'Inside' : 'Browse ~'} · ${fbs(n.bytes)}<span class="r2">${sc ? `Updating · ${kn(sc.files)} files <button class="upd" data-a="cancel" aria-label="Cancel scan">${I.x}</button>` : `${esc(ago(d.scannedAt))} <button class="upd" data-a="rescan" aria-label="Rescan">${R.refresh}</button>`}</span></div>`);
    out.push(`<div class="strip2">${kids.filter(k => k.bytes / n.bytes > 0.005).map(k => `<i style="width:${(k.bytes / n.bytes * 100).toFixed(2)}%;background:${kcol(k.kind)}"></i>`).join('')}${more?.bytes ? `<i style="width:${(more.bytes / n.bytes * 100).toFixed(2)}%;background:var(--bg4)"></i>` : ''}</div>`);
    out.push(`<div class="lbox">${show.map(k => `<div class="br" data-path="${esc(k.path)}" data-bytes="${k.bytes}" data-kind="${esc(KLAB[k.kind] || k.kind)}">${this.ck(k.path)}<span class="bn">${esc(k.name)}${k.agent ? ` <span class="achip sm${ccls(agentOf(k.agent.pane)?.color)}" data-chat="${esc(k.agent.pane)}">${I.chat}</span>` : ''}</span><span class="bb"><i class="${k.reclaim ? 'hatch' : ''}" style="width:${(k.bytes / mx * 100).toFixed(1)}%;background-color:${kcol(k.kind)}"></i></span><span class="bz">${fbs(k.bytes)}</span><span class="go">›</span></div>`).join('')}
      ${!this.allOpen && kids.length > lim ? `<button class="br all" data-a="all">Show all ${total}</button>` : ''}
      ${(this.allOpen || kids.length <= lim) && more?.count ? `<div class="br more">+ ${more.count} smaller · ${fbs(more.bytes)}</div>` : ''}</div>`);
    if (this.path) out.push(finds.replace('Worth a look', 'Worth a look here'));
    this.paint(out);
  }
  paint(out) {
    put(this.body, out.join(''));
    const sb = $('.selbar', this.el), tot = [...this.sel.values()].reduce((s, x) => s + x.bytes, 0);
    sb.hidden = !this.selMode || !this.sel.size;
    $('.sl', sb).textContent = `${this.sel.size} selected · ${fbs(tot)}`;
    this.el.classList.toggle('selm', this.selMode);
  }
  folderSheet(path, bytes) {
    const sh = sheet(`<h2 class="jt">${esc(shortPath(path))}</h2><p class="dim small jm">${fbs(bytes)}</p><div class="acts"><a class="act" href="#/f?path=${enc(path)}">Open in Files</a><button class="act" data-a="copy">Copy path</button><button class="act" data-a="scan">Rescan this folder</button><button class="act" data-a="ask">Ask an agent about this folder…</button></div>`, 'compact');
    sh.addEventListener('click', e => {
      const b = e.target.closest('[data-a], a'); if (!b) return;
      sh.close();
      if (b.dataset.a === 'copy') copyText(path);
      else if (b.dataset.a === 'scan') this.rescan(path);
      else if (b.dataset.a === 'ask') askAgent(G.st?.home, `About ${shortPath(path)} (${fbs(bytes)} on disk): `);
    });
  }
  cleanupSheet() {
    const items = [...this.sel].map(([p, x]) => `- ${shortPath(p)} (${fbs(x.bytes)}${x.kind ? ', ' + x.kind : ''})`).join('\n');
    const text = `Please clean up these to free disk space:\n${items}\nShow me exactly what you would delete and wait for my yes before deleting anything. Prefer \`trash\` over \`rm\`.`;
    const home = agentOf(G.st?.home), root = this.d?.root || '~';
    let to = home ? 'master' : 'new';
    const sh = sheet(`<h2>Ask an agent to clean up</h2>
      <div class="tochips"><span class="dim">To:</span><button class="chip${to === 'master' ? ' on' : ''}" data-to="master"${home ? '' : ' disabled'}>${esc(HOME_NAME())}</button><button class="chip${to === 'new' ? ' on' : ''}" data-to="new">New agent in ~</button></div>
      <textarea class="cuta" rows="7">${esc(text)}</textarea>
      <div class="btns col"><button class="btn primary send1"></button><button class="btn edit1">Edit in chat</button><button class="btn sm cpl">Copy list</button></div>
      <p class="dim small">Nothing is deleted until you confirm in the chat.</p>`, 'tall');
    const ta = $('.cuta', sh), send = $('.send1', sh);
    const upd = () => { send.textContent = to === 'master' ? 'Send to ' + HOME_NAME() : 'Start agent in ~'; sh.querySelectorAll('[data-to]').forEach(b => b.classList.toggle('on', b.dataset.to === to)); };
    upd();
    sh.addEventListener('click', async e => {
      const c = e.target.closest('[data-to]'); if (c && !c.disabled) { to = c.dataset.to; upd(); return; }
      if (e.target.closest('.cpl')) { copyText([...this.sel.keys()].join('\n')); return; }
      if (e.target.closest('.edit1')) {
        sh.close(); this.selMode = false; this.sel.clear(); this.render();
        if (to === 'master') askAgent(home.pane, ta.value); else newAgentSheet(ta.value);
        return;
      }
      if (e.target.closest('.send1')) {
        send.disabled = true;
        try {
          if (to === 'master') { await post(`a/${enc(home.pane)}/prompt`, { text: ta.value, nonce: Math.random().toString(36).slice(2) + Date.now().toString(36) }); sh.close(); toast('Sent to ' + esc(HOME_NAME()), 'ok'); location.hash = '#/'; }
          else { const r = await post('new', { cwd: root, prompt: ta.value, label: 'cleanup' }); G.starting.set(r.pane, Date.now()); sh.close(); location.hash = '#/a/' + enc(r.pane); }
          this.selMode = false; this.sel.clear(); this.render();
        } catch (x) { send.disabled = false; toast('Not sent: ' + esc(x.message), 'err'); }
      }
    });
  }
}

// ---------- Settings → Resource alerts ----------
const RULES = [
  ['gpu-stall', 'GPU stalled', r => `a process holding ≥ 512 MB VRAM idles below 5 % for ${r.min} min`, 'min', [3, 5, 10], v => v + ' min'],
  ['vram-full', 'VRAM almost full', r => `VRAM ≥ ${r.pct} % for 60 s`, 'pct', [90, 95, 98], v => v + ' %'],
  ['mem-low', 'RAM low', r => `less than ${r.availMB >= 1024 ? r.availMB / 1024 + ' GB' : r.availMB + ' MB'} available for 30 s`, 'availMB', [512, 1024, 2048], v => v >= 1024 ? v / 1024 + ' GB' : v + ' MB'],
  ['disk-low', 'Disk low', r => `/ or /home under ${r.freeGB} GB free (or ≥ ${r.pct ?? 92} % used)`, 'freeGB', [5, 10, 20], v => v + ' GB'],
  ['runaway', 'Runaway agent', r => `one agent ≥ ${r.pct ?? 300} % CPU for ${r.min} min (jobs excluded)`, 'min', [5, 10, 20], v => v + ' min'],
  ['agent-write', 'Agent writing a lot', r => `one agent wrote ≥ ${r.gb} GB in ${r.min ?? 10} min`, 'gb', [2, 5, 10], v => v + ' GB'],
  ['gpu-hot', 'GPU hot', r => `GPU ≥ ${r.c} °C for 2 min`, 'c', [80, 85, 90], v => v + ' °C'],
];
export async function alertsSheet() {
  const p = await getPrefs();
  const ra = JSON.parse(JSON.stringify(p.resAlerts || { on: false, rules: {} }));
  ra.rules ||= {};
  const sh = sheet(`<h2>Resource alerts</h2><div class="ralb"></div>`, 'tall rsh');
  const box = $('.ralb', sh);
  let watches = [];
  const save = debounce(async () => {
    try { await post('prefs', { resAlerts: ra }); if (G.prefs) G.prefs.resAlerts = JSON.parse(JSON.stringify(ra)); }
    catch (e) { toast('Not saved: ' + esc(e.message), 'err'); }
  }, 400);
  const draw = () => {
    put(box, `<div class="group"><label class="kv"><span class="ral-m"><b>Push me about this machine</b><small>Opt-in. Off = nothing runs.</small></span><input type="checkbox" switch data-m${ra.on ? ' checked' : ''}></label></div>
      <div class="group rules${ra.on ? '' : ' off'}">${RULES.map(([k, name, desc, f, pre, lab]) => {
        const r = ra.rules[k] ||= {}; return `<div class="rrow"><label class="kv"><span class="ral-m"><b>${name}</b><small>${esc(desc(r))}</small></span><input type="checkbox" switch data-r="${k}"${r.on ? ' checked' : ''}${ra.on ? '' : ' disabled'}></label>
        <div class="pre">${pre.map(v => `<button class="sc${r[f] === v ? ' on' : ''}" data-k="${k}" data-f="${f}" data-v="${v}"${ra.on && r.on ? '' : ' disabled'}>${lab(v)}</button>`).join('')}</div></div>`;
      }).join('')}</div>
      <h3>Watched runs</h3>
      <div class="group">${watches.length ? watches.map(w => `<div class="kv"><span class="ral-m"><b>${esc(w.label)}</b><small>${esc(atitle(agentOf(w.pane), w.pane || ''))} · watched ${esc(ago(w.since))}</small></span><button class="ib unw" data-key="${esc(w.key)}" aria-label="Stop watching">${I.x}</button></div>`).join('') : '<div class="kv"><span>None. Use “Watch this run” on a process.</span></div>'}</div>
      <p class="dim small">Checks every 30 s while any alert is on (~0.2 % CPU). Off = nothing runs. A watched run pushes once when it ends or stalls, even with this switch off.</p>`);
  };
  draw();
  api('res/watch').then(r => { watches = r.watches || []; draw(); }, () => { });
  box.addEventListener('change', e => {
    const t = e.target;
    if (t.matches('[data-m]')) { ra.on = t.checked; haptic(); }
    else if (t.dataset.r) { ra.rules[t.dataset.r].on = t.checked; }
    else return;
    save(); draw();
  });
  box.addEventListener('click', async e => {
    const c = e.target.closest('.sc[data-k]');
    if (c) { ra.rules[c.dataset.k][c.dataset.f] = +c.dataset.v; haptic(); save(); draw(); return; }
    const u = e.target.closest('.unw');
    if (u) {
      try { await api('res/watch/' + enc(u.dataset.key), { method: 'DELETE' }); watches = watches.filter(w => w.key !== u.dataset.key); draw(); }
      catch (x) { toast('Failed: ' + esc(x.message), 'err'); }
    }
  });
}
