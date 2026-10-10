// Find (#/search, lazy): agents first (name, job line, folder: find.js over GET /api/agents, instant), then messages
// (F10 transcript search, GET /api/search), one row per conversation with its first hit highlighted.
import { G, I, $, h, esc, api, ls, tick, shortPath, itemHref, goBack, debounce, cdot, lazy } from './app.js';
import { findAgents, hlWords, sleepRow, awakeRow, favHTML, nameOf, colCls, continueAgent, cached, CHEV, MOON } from './find.js';

const hl = (text, q) => {
  const t = String(text ?? ''), lo = t.toLowerCase(), ql = q.toLowerCase();
  let out = '', k = 0, j;
  while (ql && (j = lo.indexOf(ql, k)) >= 0) { out += esc(t.slice(k, j)) + '<mark>' + esc(t.slice(j, j + q.length)) + '</mark>'; k = j + q.length; }
  return out + esc(t.slice(k));
};

export class SearchView {
  constructor() {
    this.pane = null; this.keep = true; this.q = '';
    this.el = h(`<div class="view searchv">
      <header class="bar"><div class="bar-in">
        <button class="ib back" aria-label="Back">${I.back}</button>
        <form class="sform" role="search"><span class="sic">${I.search}</span><input type="search" placeholder="Find an agent: name, job, folder, chat" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" enterkeyhint="search"></form>
      </div></header>
      <div class="scroll sres"></div></div>`);
    this.inp = $('input', this.el); this.box = $('.sres', this.el);
    $('.back', this.el).onclick = () => goBack('#/agents');
    const run = debounce(() => this.run(), 250);
    this.inp.oninput = () => { this.q = this.inp.value.trim(); if (this.q.length < 2) { this.ac?.abort(); this.recent(); } else run(); };
    $('.sform', this.el).onsubmit = e => { e.preventDefault(); this.inp.blur(); this.remember(); this.run(); };
    this.box.addEventListener('click', e => {
      const c = e.target.closest('[data-cont]'); // find.js: wake an asleep agent
      if (c) { e.preventDefault(); e.stopPropagation(); this.remember(); continueAgent(c.dataset.cont, { btn: c, name: c.dataset.nm }); return; }
      const rb = e.target.closest('[data-resume]'); // fleet: resume a past session
      if (rb) { e.preventDefault(); lazy('fleet').then(m => m.resume(rb.dataset.resume, rb)); return; }
      const r = e.target.closest('[data-rq]');
      if (r) { this.inp.value = this.q = r.dataset.rq; this.run(); return; }
      if (e.target.closest('a.sr, a.xrow')) this.remember();
    });
    this.box.addEventListener('touchstart', () => this.inp.blur(), { passive: true });
  }
  mount() { this.recent(); this.inp.focus(); }
  resume() { if (!this.q) this.inp.focus(); }
  suspend() { }
  remember() {
    if (this.q.length < 2) return;
    const r = ls.get('recentQ', []).filter(x => x !== this.q); r.unshift(this.q); ls.set('recentQ', r.slice(0, 10));
  }
  recent() {
    const r = ls.get('recentQ', []);
    this.box.innerHTML = r.length ? `<h3 class="sh3">Recent searches</h3>${r.map(q => `<button class="rq" data-rq="${esc(q)}">${I.search}<span>${esc(q)}</span></button>`).join('')}`
      : `<div class="stline">Find an agent by its name, what it does, its folder, or anything said in its chat.</div>`;
  }
  async run() {
    const q = this.q; if (q.length < 2) return;
    this.ac?.abort(); const ac = this.ac = new AbortController();
    this.el.classList.add('busy');
    // agents at once (cached list), messages when the server answers
    findAgents(q).then(ag => { if (ac === this.ac) { this.ag = ag; this.render(this.msgs, q); } });
    if (!this.box.querySelector('.fres')) this.box.innerHTML = '<div class="stline"><span class="spin"></span> Searching…</div>';
    this.msgs = null;
    try {
      const r = await api(`search?q=${encodeURIComponent(q)}&limit=60`, { signal: ac.signal });
      if (ac !== this.ac) return;
      this.msgs = r; this.ag = await findAgents(q); this.render(r, q);
    } catch (e) {
      if (e.name === 'AbortError' || ac !== this.ac) return;
      this.msgs = { results: [], err: e.message }; this.render(this.msgs, q);
    } finally { if (ac === this.ac) this.el.classList.remove('busy'); }
  }
  render(r, q) {
    const ag = this.ag || [], hl = t => hlWords(t, q), WHY = { name: 'name', job: 'job', folder: 'folder' };
    const agHTML = ag.length ? `<div class="fres-h">Agents<span class="gn">${ag.length}</span></div><div class="fres">${ag.slice(0, 12).map(({ a, why }) => a.asleep
      ? sleepRow(a, { hl, why: `Asleep <span>·</span> ${hl(shortPath(a.cwd || ''))}` })
      : awakeRow(a, hl)).join('')}</div>` : '';
    let msHTML = '';
    if (!r) msHTML = '<div class="stline"><span class="spin"></span> Searching chats…</div>';
    else if (r.err) msHTML = `<div class="stline">Chat search failed: ${esc(r.err)}</div>`;
    else {
      const groups = new Map();
      for (const x of r.results || []) { let g = groups.get(x.tid); if (!g) groups.set(x.tid, g = []); g.push(x); }
      const L = cached()?.agents || [], live = G.st?.agents || [];
      const rows = [...groups].map(([tid, xs]) => {
        const x = xs[0], la = live.find(y => y.tid === tid || y.session === tid || (x.pane && y.pane === x.pane));
        const a = L.find(y => y.sid === tid) || (la && { ...la, name: la.short || la.title, sid: la.session, asleep: false }) || { sid: tid, name: x.title || 'Untitled session', asleep: true, cwd: x.cwd };
        const asleep = !la, st = asleep ? `<span class="zzt">${MOON}Asleep</span>` : la.status === 'working' ? '<span class="wkt">Working</span>' : la.status === 'blocked' ? 'Needs you' : 'Idle';
        return `<a class="xrow xsr${colCls(a.color)}${asleep ? ' zz' : ''}" href="${itemHref(x.tid, x.i, la?.pane)}">${favHTML({ ...a, asleep }, { sz: 28 })}<span class="xb">`
          + `<span class="f1"><span class="xnm">${esc(nameOf(a))}</span><span class="tm" data-since="${x.ts}"></span></span>`
          + `<span class="snip"><span class="xwho${x.role === 'user' ? ' u' : ''}">${x.role === 'user' ? 'You' : 'Claude'}</span>${hlWords(x.snippet, q)}</span>`
          + `<span class="f3">${st}${xs.length > 1 ? ` · ${xs.length} matches` : ''} · ${esc(shortPath(x.cwd || ''))}</span></span>`
          + (asleep ? `<button class="btn cont ghost" data-cont="${esc(tid)}" data-nm="${esc(nameOf(a))}">Continue</button>` : `<span class="xchev">${CHEV}</span>`) + '</a>';
      });
      msHTML = rows.length ? `<div class="fres-h">Messages<span class="gn">${(r.results || []).length}${r.partial ? '+' : ''}</span></div><div class="fres">${rows.join('')}</div>` : '';
    }
    this.box.innerHTML = agHTML + msHTML + (r && !ag.length && !(r.results || []).length && !r.err ? `<div class="stline">No agents or messages match “${esc(q)}”.</div>`
      : ag.some(x => x.a.asleep) || (r?.results || []).length ? '<div class="stline small">Asleep agents continue in place, with their whole conversation.</div>' : '');
    tick(this.box);
  }
}
