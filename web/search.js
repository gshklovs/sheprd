// F10 global search across transcripts: #/search (lazy).
import { G, I, $, h, esc, api, ls, tick, shortPath, itemHref, goBack, debounce, cdot, lazy } from './app.js';

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
        <form class="sform" role="search"><span class="sic">${I.search}</span><input type="search" placeholder="Search all conversations" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" enterkeyhint="search"></form>
      </div></header>
      <div class="scroll sres"></div></div>`);
    this.inp = $('input', this.el); this.box = $('.sres', this.el);
    $('.back', this.el).onclick = () => goBack('#/agents');
    const run = debounce(() => this.run(), 250);
    this.inp.oninput = () => { this.q = this.inp.value.trim(); if (this.q.length < 2) { this.ac?.abort(); this.recent(); } else run(); };
    $('.sform', this.el).onsubmit = e => { e.preventDefault(); this.inp.blur(); this.remember(); this.run(); };
    this.box.addEventListener('click', e => {
      const rb = e.target.closest('[data-resume]'); // fleet: resume a past session
      if (rb) { e.preventDefault(); lazy('fleet').then(m => m.resume(rb.dataset.resume, rb)); return; }
      const r = e.target.closest('[data-rq]');
      if (r) { this.inp.value = this.q = r.dataset.rq; this.run(); return; }
      if (e.target.closest('a.sr')) this.remember();
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
      : `<div class="stline">Search prompts and answers in every session.</div>`;
  }
  async run() {
    const q = this.q; if (q.length < 2) return;
    this.ac?.abort(); const ac = this.ac = new AbortController();
    if (!this.box.querySelector('.sgrp')) this.box.innerHTML = '<div class="stline"><span class="spin"></span> Searching…</div>';
    this.el.classList.add('busy');
    try {
      const r = await api(`search?q=${encodeURIComponent(q)}&limit=60`, { signal: ac.signal });
      if (ac !== this.ac) return;
      this.render(r, q);
    } catch (e) {
      if (e.name === 'AbortError' || ac !== this.ac) return;
      this.box.innerHTML = `<div class="stline">Search failed: ${esc(e.message)}</div>`;
    } finally { if (ac === this.ac) this.el.classList.remove('busy'); }
  }
  render(r, q) {
    const res = r.results || [];
    if (!res.length) { this.box.innerHTML = `<div class="stline">No matches for “${esc(q)}”.</div>`; return; }
    const groups = new Map();
    for (const x of res) { let g = groups.get(x.tid); if (!g) groups.set(x.tid, g = []); g.push(x); }
    const by = new Map((G.st?.agents || []).map(a => [a.tid, a]));
    this.box.innerHTML = [...groups].map(([tid, xs]) => {
      const a = by.get(tid) || (xs[0].pane && G.st?.agents.find(y => y.pane === xs[0].pane));
      return `<div class="sgrp"><div class="sg-h">${a ? cdot(a.color) : ''}<b>${esc(a?.home ? 'Master' : a?.title || xs[0].title || 'Untitled session')}</b>${a ? '<i class="live">live</i>' : ''}<span class="sp"></span><span data-since="${xs[0].ts}"></span>${a ? '' : `<button class="btn sm sres-r" data-resume="${esc(tid)}">Resume</button>`}</div>
        <div class="sg-c">${esc(shortPath(xs[0].cwd))}</div>
        ${xs.map(x => `<a class="sr ${x.role === 'user' ? 'u' : 'a'}" href="${itemHref(x.tid, x.i, x.pane)}"><span class="who">${x.role === 'user' ? 'You' : 'Claude'}</span><span class="sn">${hl(x.snippet, q)}</span></a>`).join('')}</div>`;
    }).join('') + (r.partial ? '<div class="stline small">Showing first matches</div>' : '');
    tick(this.box);
  }
}
