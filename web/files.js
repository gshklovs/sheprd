// F8 files touched: sheet per session + diff page #/d/<tid>?path= (lazy).
import { G, I, $, h, esc, api, sheet, tick, shortPath, itemHref, goBack, fileUrl, ls, toast, haptic, nav } from './app.js';

const enc = encodeURIComponent;
const pl = (n, w) => `${n} ${w}${n === 1 ? '' : 's'}`;
const split = p => { const n = p.split('/').pop() || p; return [n, p.slice(0, p.length - n.length - 1) || '/']; };
const docHref = p => '#/f?path=' + enc(p);

export async function filesSheet(tid, pre) {
  const sh = sheet(`<h2>Files touched</h2><div class="fl"><div class="dim pad"><span class="spin"></span> Loading…</div></div>`, 'tall');
  const box = $('.fl', sh);
  const draw = r => {
    const files = r.files || [], git = r.git;
    const mine = new Set(files.map(f => f.path));
    const also = git ? (git.changed || []).filter(c => !mine.has(git.cwd.replace(/\/$/, '') + '/' + c.path)) : [];
    let html = git ? `<div class="fgit">${I.fork}<b>${esc(git.branch || 'detached')}</b><span>${pl((git.changed || []).length, 'changed file')} · ${esc(shortPath(git.cwd))}</span></div>` : '';
    html += files.length ? files.map(f => {
      const [n, d] = split(f.path);
      const what = [f.edits && pl(f.edits, 'edit'), f.writes && pl(f.writes, 'write')].filter(Boolean).join(' · ');
      return `<a class="frow" href="#/d/${enc(tid)}?path=${enc(f.path)}"><span class="sb"><b>${esc(n)}</b><small>${esc(shortPath(d))}</small></span>
        <span class="fm">${f.subs?.length ? `<i class="fsub" title="also touched by a subagent">${I.fork}</i>` : ''}<span>${esc(what)}</span><span data-since="${f.lastTs}"></span></span><span class="go">›</span></a>`;
    }).join('') : '<p class="dim pad">No file edits in this session.</p>';
    if (also.length) html += `<h3>Also changed in repo</h3>` + also.map(c => {
      const abs = git.cwd.replace(/\/$/, '') + '/' + c.path, [n, d] = split(c.path);
      return `<a class="frow" href="${docHref(abs)}"><span class="sb"><b>${esc(n)}</b><small>${esc(d === '/' ? '' : d)}</small></span><span class="fst">${esc(c.st)}</span><span class="go">›</span></a>`;
    }).join('');
    box.innerHTML = html; tick(box);
  };
  const load = () => api(`t/${enc(tid)}/files`).then(draw, e => {
    box.innerHTML = `<p class="dim pad">Couldn’t load: ${esc(e.message)} <button class="btn sm retry">Retry</button></p>`;
    $('.retry', box).onclick = load;
  });
  if (pre) draw(pre);
  load();
  sh.addEventListener('click', e => { if (e.target.closest('a')) sh.close(); });
}

const lines = (s, sign, tag) => String(s ?? '').replace(/\n$/, '').split('\n').map((l, n) => `<${tag} data-n="${n}">${sign} ${esc(l)}</${tag}>`).join('\n');
// 1-based line in `text` where `part` starts, or null
const lineOf = (text, part) => { if (text == null || !part) return null; const k = text.indexOf(part); return k < 0 ? null : text.slice(0, k).split('\n').length; };

export class DiffView {
  constructor(tid, path) {
    this.tid = tid; this.path = path; this.pane = null;
    const [n, d] = split(path);
    this.el = h(`<div class="view diffv">
      <header class="bar"><div class="bar-in">
        <button class="ib back" aria-label="Back">${I.back}</button>
        <div class="ttl"><div class="t1">${esc(n)}</div><div class="t2">${esc(shortPath(d))}</div></div>
        <a class="ib" href="${docHref(path)}" aria-label="Open file">${I.files}</a>
      </div></header>
      <div class="scroll docb"><div class="stline"><span class="spin"></span> Loading…</div></div></div>`);
  }
  mount() {
    const pt = this.tid.split('~')[0], a = G.st?.agents.find(x => x.tid === pt);
    $('.back', this.el).onclick = () => goBack(a ? '#/a/' + enc(a.pane) : '#/t/' + enc(pt));
    this.load();
  }
  async load() {
    const body = $('.docb', this.el);
    try {
      const r = await api(`t/${enc(this.tid)}/files/diff?path=${enc(this.path)}`);
      const ch = r.changes || [];
      if (!ch.length) { body.innerHTML = '<div class="stline">No changes to this file in this session.</div>'; return; }
      body.innerHTML = `<div class="dsum dim">${pl(ch.length, 'change')}, oldest first</div>` + ch.map(c => {
        const sub = c.tid && c.tid.includes('~');
        const diff = c.tool === 'Write' ? lines(c.content, '+', 'ins') : lines(c.old, '-', 'del') + '\n' + lines(c.new, '+', 'ins');
        return `<div class="hunk"><div class="hk-h"><b>${esc(c.tool)}</b>${sub ? `<i class="fsub">${I.fork}</i>` : ''}<span data-since="${c.ts}"></span><span class="sp"></span>
          <a class="btn sm" href="${itemHref(c.tid || this.tid, c.i)}">↗ in chat</a></div><pre class="diff">${diff}</pre></div>`;
      }).join('') + `<div class="btns"><a class="btn" href="${docHref(this.path)}">${I.files} Open file</a></div>`;
      tick(body);
      this.ch = ch;
      body.querySelectorAll('.hunk').forEach((x, k) => { x.dataset.k = k; });
      body.onclick = e => { const l = e.target.closest('pre.diff ins, pre.diff del'); if (l) this.comment(l); };
    } catch (e) {
      body.innerHTML = `<div class="stline">Couldn’t load: ${esc(e.message)} <button class="btn sm retry">Retry</button></div>`;
      $('.retry', body).onclick = () => { body.innerHTML = '<div class="stline"><span class="spin"></span></div>'; this.load(); };
    }
  }
  // v3 diff-line comments: tap a line, write a note; it goes into the agent's draft
  // as "In <file>:<line>: <note>" so several can be batched before sending.
  agent() { const pt = this.tid.split('~')[0]; return G.st?.agents.find(x => x.tid === pt); }
  async fileText() {
    if (this.text === undefined) this.text = await fetch(fileUrl(this.path)).then(r => r.ok ? r.text() : null, () => null);
    return this.text;
  }
  async comment(lineEl) {
    const a = this.agent();
    if (!a) { toast('This agent is not running: comments go to a live agent', 'err'); return; }
    const hunk = lineEl.closest('.hunk'), c = this.ch[+hunk.dataset.k], n = +lineEl.dataset.n;
    const del = lineEl.tagName === 'DEL', text = await this.fileText();
    const base = lineOf(text, c.tool === 'Write' ? c.content : c.new);
    const line = base == null ? null : base + (del ? 0 : n);
    const cwd = String(a.cwd || '').replace(/\/$/, ''), rel = cwd && this.path.startsWith(cwd + '/') ? this.path.slice(cwd.length + 1) : this.path;
    const code = lineEl.textContent.slice(2).trim();
    const where = line != null ? `${rel}:${line}` : rel;
    document.querySelectorAll('.cmf').forEach(f => f.remove());
    document.querySelectorAll('.diff .cm-on').forEach(x => x.classList.remove('cm-on'));
    lineEl.classList.add('cm-on');
    const f = h(`<form class="cmf"><div class="cm-w"><b>${esc(where)}</b>${del ? ' (removed line)' : ''}<code>${esc(code.slice(0, 120))}</code></div>
      <textarea rows="2" placeholder="Comment for ${esc(a.home ? 'Master' : a.title || a.pane)}"></textarea>
      <div class="btns"><button class="btn primary sm">Add to draft</button><button type="button" class="btn sm no">Cancel</button></div></form>`);
    hunk.append(f);
    const ta = $('textarea', f); ta.focus();
    $('.no', f).onclick = () => { f.remove(); lineEl.classList.remove('cm-on'); };
    f.onsubmit = e => {
      e.preventDefault();
      const note = ta.value.trim(); if (!note) return;
      const msg = `In ${where}${line == null || del ? ` (\`${code.slice(0, 80)}\`)` : ''}: ${note}`;
      const k = 'draft.' + a.pane, cur = ls.get(k, '');
      ls.set(k, cur ? cur.replace(/\s*$/, '') + '\n' + msg : msg);
      haptic(); f.remove(); lineEl.classList.remove('cm-on'); lineEl.classList.add('cm-done');
      this.count = (this.count || 0) + 1; this.bar(a);
    };
  }
  bar(a) {
    let b = $('.cmbar', this.el);
    if (!b) { b = h('<div class="cmbar"><span></span><button class="btn primary sm">Send to agent</button></div>'); this.el.append(b); }
    $('span', b).textContent = `${this.count} comment${this.count > 1 ? 's' : ''} in ${a.home ? 'Master' : a.title || a.pane}’s draft`;
    $('button', b).onclick = () => nav(a.home ? '#/' : '#/a/' + encodeURIComponent(a.pane));
  }
}
