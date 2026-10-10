// Lazy: branch vs main (#/g/<pane>), per-file git diff (#/gd/<pane>?scope=&path=), CI/PR watch with Fix,
// usage rings (the Master header's context + usage button, the overview sheet, Settings part). API.md "Git, CI and usage".
// Commit / Push / Open PR / Fix send the agent a prompt; the server never writes to a repo.
import { G, I, $, h, esc, api, post, sheet, toast, haptic, tick, shortPath, goBack, ls, nav, copyText, HOME_NAME } from './app.js';
import { bar, compactNow } from './mb.js';

await new Promise(r => {
  if (document.querySelector('link[data-git]')) return r();
  const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'git.css'; l.dataset.git = 1;
  l.onload = l.onerror = r; document.head.append(l); setTimeout(r, 3000);
});

const enc = encodeURIComponent;
const pl = (n, w) => `${n} ${w}${n === 1 ? '' : 's'}`;
const agentOf = pane => G.st?.agents.find(a => a.pane === pane);
const nameOf = (a, pane) => a ? (a.home ? HOME_NAME() : a.title || a.pane) : pane;
const usd = c => c == null ? '—' : c >= 100 ? '$' + Math.round(c) : '$' + c.toFixed(2);
const kn = n => n >= 1e6 ? (n / 1e6).toFixed(1) + 'M' : n >= 1e3 ? Math.round(n / 1e3) + 'k' : String(n | 0);
const inMs = t => {
  const d = Date.parse(t) - Date.now(); if (!(d > 0)) return '';
  const m = Math.round(d / 60000), hh = Math.floor(m / 60);
  return hh >= 24 ? `${Math.floor(hh / 24)}d ${hh % 24}h` : hh ? `${hh}h ${m % 60}m` : `${Math.max(m, 1)}m`;
};
const nonce = () => Math.random().toString(36).slice(2) + Date.now().toString(36);
const CHK = { pass: '✓', fail: '✕', pending: '•', skipping: '–', cancel: '–' };

// ---------- routes ----------
export function route(hs) {
  const gd = hs.match(/^\/gd\/([^?]+)\?(.*)$/);
  if (gd) {
    const pane = decodeURIComponent(gd[1]), q = new URLSearchParams(gd[2]);
    const path = q.get('path') || '', scope = q.get('scope') || 'branch';
    return { key: 'dg' + pane + '\n' + scope + '\n' + path, make: () => new GitDiffView(pane, path, scope) };
  }
  const m = hs.match(/^\/g\/([^?]+)(?:\?(.*))?$/), pane = decodeURIComponent(m?.[1] || '');
  const fix = new URLSearchParams(m?.[2] || '').get('fix') === '1';
  return { key: 'g/' + pane, make: () => new GitView(pane), focus: fix ? () => { history.replaceState(history.state, '', '#/g/' + enc(pane)); G.view?.wantFix?.(); } : null };
}

// Sends `text` to the agent like the composer would (steer while it works, else a prompt), then shows the chat.
async function sendToAgent(pane, text) {
  const a = agentOf(pane);
  if (!a) { toast('This agent is not running', 'err'); return false; }
  try {
    await post(`a/${enc(pane)}/${a.status === 'working' ? 'steer' : 'prompt'}`, { text, nonce: nonce() });
    haptic(); toast('Sent to ' + esc(nameOf(a, pane)), 'ok', null, 1600);
    nav(a.home ? '#/' : '#/a/' + enc(pane));
    return true;
  } catch (e) {
    toast(e.status === 409 ? 'The agent is waiting on a question — answer it first' : 'Not sent: ' + esc(e.message), 'err');
    return false;
  }
}

// A canned prompt the user can edit before it goes to the agent.
function promptSheet(pane, title, text, note) {
  const sh = sheet(`<h2>${esc(title)}</h2>${note ? `<p class="dim small">${note}</p>` : ''}
    <textarea class="gp-t" rows="6"></textarea>
    <div class="btns col"><button class="btn primary go">Send to ${esc(nameOf(agentOf(pane), pane))}</button></div>`, 'compact');
  const ta = $('.gp-t', sh); ta.value = text;
  $('.go', sh).onclick = async () => {
    const t = ta.value.trim(); if (!t) return;
    $('.go', sh).disabled = true;
    if (await sendToAgent(pane, t)) sh.close(); else $('.go', sh).disabled = false;
  };
}

const ACTIONS = {
  commit: ['Commit', 'Commit the current changes with a good commit message (stage what belongs to this work; leave unrelated files alone). Do not push.'],
  push: ['Push', 'Push this branch to its remote (set the upstream if it has none). Do not force-push.'],
  pr: ['Open PR', 'Open a pull request for this branch with gh (`gh pr create`): a clear title and a description of what changed and why, against the default branch. Push first if needed. Reply with the PR link.'],
};

// ---------- Branch view ----------
class GitView {
  constructor(pane) {
    this.pane = pane;
    const a = agentOf(pane);
    this.el = h(`<div class="view gitv">
      <header class="bar"><div class="bar-in">
        <button class="ib back" aria-label="Back">${I.back}</button>
        <div class="ttl"><div class="t1">Branch</div><div class="t2">${esc(nameOf(a, pane))}</div></div>
        <button class="ib rf" aria-label="Refresh"><svg viewBox="0 0 24 24"><path d="M20 12a8 8 0 1 1-2.3-5.6M20 4v5h-5"/></svg></button>
      </div></header>
      <div class="scroll docb"><div class="g-top"><div class="stline"><span class="spin"></span> Loading…</div></div><div class="g-ci"></div><div class="g-files"></div></div></div>`);
  }
  mount() {
    const a = agentOf(this.pane);
    $('.back', this.el).onclick = () => goBack(a?.home ? '#/' : '#/a/' + enc(this.pane));
    $('.rf', this.el).onclick = () => { haptic(); this.load(true); };
    this.load();
  }
  resume() { this.load(); }
  wantFix() { this.fixWanted = true; if (this.ci) this.maybeFix(); }
  async load(force) {
    const top = $('.g-top', this.el);
    const gp = api(`a/${enc(this.pane)}/git`).then(r => { this.r = r; this.draw(); }, e => {
      top.innerHTML = `<div class="stline">Couldn’t load: ${esc(e.message)}</div>`;
    });
    const cp = api(`a/${enc(this.pane)}/git/ci${force ? '?refresh=1' : ''}`).then(c => { this.ci = c; this.drawCi(); this.maybeFix(); }, e => {
      $('.g-ci', this.el).innerHTML = `<div class="dim small pad">CI: ${esc(e.message)}</div>`;
    });
    await Promise.all([gp, cp]);
  }
  draw() {
    const r = this.r, top = $('.g-top', this.el), files = $('.g-files', this.el);
    if (!r.repo) { top.innerHTML = `<div class="g-card"><b>Not a git repository</b><div class="dim small">${esc(shortPath(r.cwd || ''))}</div></div>`; files.innerHTML = ''; return; }
    $('.t2', this.el).textContent = `${shortPath(r.root)} · ${r.branch || 'detached'}`;
    const vs = r.base ? `${r.ahead ?? '?'} ahead · ${r.behind ?? '?'} behind <code>${esc(r.base)}</code>` : 'no main/master branch to compare with';
    const up = r.upstream ? (r.unpushed ? `${pl(r.unpushed, 'commit')} not pushed to <code>${esc(r.upstream)}</code>` : `up to date with <code>${esc(r.upstream)}</code>`) + (r.unpulled ? ` · ${r.unpulled} to pull` : '')
      : r.branch ? 'not pushed yet (no upstream)' : '';
    const canPush = !!r.branch && (r.unpushed > 0 || (!r.upstream && (r.ahead > 0 || r.onDefault)));
    const hasPr = !!this.ci?.pr && this.ci.pr.state === 'OPEN';
    const canPr = !!r.branch && !r.onDefault && !hasPr && r.ahead > 0;
    top.innerHTML = `<div class="g-card">
        <div class="g-br">${I.fork}<b>${esc(r.branch || (r.head || '').slice(0, 7) || '?')}</b>${r.detached ? '<span class="g-tag">detached</span>' : ''}${r.onDefault ? '<span class="g-tag">default branch</span>' : ''}</div>
        <div class="g-vs">${vs}</div>${up ? `<div class="g-vs">${up}</div>` : ''}
      </div>
      <div class="g-acts">
        <button class="btn sm" data-g="commit"${r.work?.length ? '' : ' disabled'}>Commit${r.work?.length ? ` · ${r.work.length}` : ''}</button>
        <button class="btn sm" data-g="push"${canPush ? '' : ' disabled'}>Push</button>
        <button class="btn sm" data-g="pr"${canPr ? '' : ' disabled'}>${hasPr ? 'PR open' : 'Open PR'}</button>
      </div>`;
    top.querySelectorAll('[data-g]').forEach(b => b.onclick = () => {
      const [t, p] = ACTIONS[b.dataset.g];
      promptSheet(this.pane, t, p, 'The agent runs it — the server never writes to your repo.');
    });
    const fileRow = (f, scope) => {
      const n = f.path.split('/').pop(), d = f.path.slice(0, Math.max(0, f.path.length - n.length - 1));
      const nums = f.bin ? '<span class="g-bin">bin</span>' : f.add != null ? `<span class="g-add">+${f.add}</span><span class="g-del">−${f.del}</span>` : '';
      return `<a class="frow" href="#/gd/${enc(this.pane)}?scope=${scope}&path=${enc(f.path)}"><span class="g-st g-st-${esc((f.st || 'M')[0] === '?' ? 'U' : (f.st || 'M')[0])}">${esc(f.untracked ? 'U' : (f.st || 'M')[0])}</span>
        <span class="sb"><b>${esc(n)}</b><small>${esc(d)}</small></span><span class="fm">${nums}</span><span class="go">›</span></a>`;
    };
    let html = '';
    if (r.work?.length) html += `<h3>Uncommitted · ${r.work.length}${r.workMore ? '+' : ''}</h3>` + r.work.map(f => fileRow(f, 'work')).join('');
    if (r.files?.length) {
      const add = r.files.reduce((s, f) => s + (f.add || 0), 0), del = r.files.reduce((s, f) => s + (f.del || 0), 0);
      html += `<h3>On this branch vs ${esc(r.base || 'main')} · ${pl(r.files.length, 'file')} <span class="g-add">+${add}</span> <span class="g-del">−${del}</span></h3>` + r.files.map(f => fileRow(f, 'branch')).join('');
    } else if (r.base && !r.onDefault) html += `<p class="dim small">No committed changes vs ${esc(r.base)}.</p>`;
    if (!r.work?.length && !r.files?.length && r.onDefault) html += '<p class="dim small">Working tree clean.</p>';
    if (r.commits?.length) html += `<h3>${r.mergeBase ? 'Commits since ' + esc(r.base) : 'Recent commits'} · ${r.commits.length}${r.commitsMore ? '+' : ''}</h3><div class="g-commits">` +
      r.commits.map(c => `<div class="g-cm"><code>${esc(c.short)}</code><span class="g-subj">${esc(c.subject)}</span><span class="dim" data-since="${c.ts}"></span></div>`).join('') + '</div>';
    files.innerHTML = html; tick(files);
  }
  drawCi() {
    const c = this.ci, box = $('.g-ci', this.el);
    if (this.r) this.draw(); // PR state changes the action buttons
    if (c.gh !== 'ok') {
      box.innerHTML = `<div class="g-card g-conn"><b>${c.gh === 'missing' ? 'GitHub CLI not installed' : 'Connect GitHub'}</b>
        <div class="dim small">${c.gh === 'missing' ? esc(c.hint) : 'Run <code>gh auth login</code> in a terminal on the desk to see PRs and CI here.'}</div>
        ${c.gh === 'missing' ? '' : '<div class="btns"><button class="btn sm cp">Copy command</button></div>'}</div>`;
      $('.cp', box)?.addEventListener('click', () => copyText('gh auth login'));
      return;
    }
    if (!c.branch) { box.innerHTML = ''; return; }
    const pr = c.pr;
    const watch = `<label class="kv g-watch"><span>Watch CI${pr ? '' : ' (until a PR shows up)'}</span><input type="checkbox" switch${c.watch ? ' checked' : ''}></label>`;
    if (!pr) {
      box.innerHTML = `<div class="g-card"><div class="g-ci-h"><b>No pull request</b><span class="dim small">for ${esc(c.branch)}</span></div></div><div class="group">${watch}</div>`;
    } else {
      const st = c.state || 'none', fail = st === 'fail';
      const checks = (c.checks || []).map(k => `<a class="g-chk g-${esc(k.bucket)}" ${k.link ? `href="${esc(k.link)}" target="_blank" rel="noopener"` : ''}><i>${CHK[k.bucket] || '•'}</i><span>${esc(k.name)}</span><small>${esc(k.workflow || '')}</small></a>`).join('');
      box.innerHTML = `<div class="g-card g-ci-${esc(st)}">
          <div class="g-ci-h"><a href="${esc(pr.url)}" target="_blank" rel="noopener"><b>PR #${pr.number}</b> ${esc(pr.title)} ${I.ext || '↗'}</a></div>
          <div class="g-ci-s">${pr.state !== 'OPEN' ? esc(pr.state.toLowerCase()) : pr.isDraft ? 'draft · ' : ''}${pr.state === 'OPEN' ? { fail: 'CI failing', pending: 'CI running', pass: 'CI green', none: 'no checks' }[st] : ''}
            ${c.ts ? `<span class="dim" data-since="${c.ts}"></span>` : ''}</div>
          ${checks ? `<div class="g-chks">${checks}</div>` : ''}
          ${fail ? '<div class="btns"><button class="btn primary sm fix">Fix — send the failing log to the agent</button></div>' : ''}
        </div><div class="group">${watch}</div>`;
      $('.fix', box)?.addEventListener('click', () => this.fix());
    }
    $('.g-watch input', box).onchange = e => post(`a/${enc(this.pane)}/git/watch`, { on: e.target.checked })
      .then(() => toast(e.target.checked ? 'Watching CI — a push when it goes red' : 'CI watch off', 'ok', null, 1600), x => toast('Failed: ' + esc(x.message), 'err'));
    tick(box);
  }
  maybeFix() { if (this.fixWanted && this.ci?.state === 'fail') { this.fixWanted = false; this.fix(); } }
  async fix() {
    haptic();
    const b = $('.g-ci .fix', this.el), old = b?.innerHTML;
    if (b) { b.disabled = true; b.innerHTML = '<span class="spin"></span> Fetching the failing log…'; }
    try {
      const r = await api(`a/${enc(this.pane)}/git/fix`);
      promptSheet(this.pane, 'Fix CI', r.text, 'Includes the failing jobs’ log tail. Edit before sending if you like.');
    } catch (e) { toast('Couldn’t build the fix prompt: ' + esc(e.message), 'err'); }
    if (b) { b.disabled = false; b.innerHTML = old; }
  }
}

// ---------- one file's diff (unified) ----------
function parseUnified(text) {
  const hunks = []; let cur = null, nl = 0;
  for (const l of String(text || '').split('\n')) {
    const m = l.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$/);
    if (m) { cur = { head: l, ctx: m[2].trim(), lines: [] }; hunks.push(cur); nl = +m[1]; continue; }
    if (!cur) continue;
    if (l.startsWith('\\')) continue;
    const s = l[0];
    if (s === '+') cur.lines.push({ t: 'ins', s: l.slice(1), n: nl++ });
    else if (s === '-') cur.lines.push({ t: 'del', s: l.slice(1), n: nl });
    else if (s === ' ' || l === '') cur.lines.push({ t: 'ctx', s: l.slice(1), n: nl++ });
  }
  return hunks;
}

class GitDiffView {
  constructor(pane, path, scope) {
    this.pane = pane; this.path = path; this.scope = scope;
    const n = path.split('/').pop(), d = path.slice(0, Math.max(0, path.length - n.length - 1));
    this.el = h(`<div class="view diffv">
      <header class="bar"><div class="bar-in">
        <button class="ib back" aria-label="Back">${I.back}</button>
        <div class="ttl"><div class="t1">${esc(n)}</div><div class="t2">${esc(d ? d + ' · ' : '')}${scope === 'work' ? 'uncommitted' : 'branch vs main'}</div></div>
      </div></header>
      <div class="scroll docb"><div class="stline"><span class="spin"></span> Loading…</div></div></div>`);
  }
  mount() {
    $('.back', this.el).onclick = () => goBack('#/g/' + enc(this.pane));
    this.load();
  }
  async load() {
    const body = $('.docb', this.el);
    try {
      const r = await api(`a/${enc(this.pane)}/git/diff?scope=${this.scope}&path=${enc(this.path)}`);
      this.root = r.root;
      const hs = parseUnified(r.diff);
      if (r.bin) { body.innerHTML = '<div class="stline">Binary file changed.</div>'; return; }
      if (!hs.length) { body.innerHTML = '<div class="stline">No changes.</div>'; return; }
      let add = 0, del = 0; hs.forEach(x => x.lines.forEach(l => { if (l.t === 'ins') add++; else if (l.t === 'del') del++; }));
      this.hs = hs;
      body.innerHTML = `<div class="dsum dim">${pl(hs.length, 'hunk')} · <span class="g-add">+${add}</span> <span class="g-del">−${del}</span>${r.cut ? ' · cut' : ''} · tap a line to comment</div>` + hs.map((x, k) =>
        `<div class="hunk" data-k="${k}"><div class="hk-h"><b>line ${x.lines.find(l => l.t !== 'del')?.n ?? ''}</b><span class="g-hctx">${esc(x.ctx)}</span></div><pre class="diff">${x.lines.map((l, j) =>
          l.t === 'ctx' ? `<span class="g-ctx" data-j="${j}">  ${esc(l.s)}</span>` : `<${l.t} data-j="${j}">${l.t === 'ins' ? '+' : '-'} ${esc(l.s)}</${l.t}>`).join('\n')}</pre></div>`).join('');
      body.onclick = e => { const l = e.target.closest('pre.diff ins, pre.diff del, pre.diff .g-ctx'); if (l) this.comment(l); };
    } catch (e) {
      body.innerHTML = `<div class="stline">Couldn’t load: ${esc(e.message)} <button class="btn sm retry">Retry</button></div>`;
      $('.retry', body).onclick = () => { body.innerHTML = '<div class="stline"><span class="spin"></span></div>'; this.load(); };
    }
  }
  // like the session diff page: the note goes into the agent's draft as "In <path>:<line>: <note>"
  comment(lineEl) {
    const a = agentOf(this.pane);
    if (!a) { toast('This agent is not running: comments go to a live agent', 'err'); return; }
    const hunk = lineEl.closest('.hunk'), l = this.hs[+hunk.dataset.k].lines[+lineEl.dataset.j];
    const del = l.t === 'del', where = `${this.path}:${l.n}`, code = l.s.trim();
    document.querySelectorAll('.cmf').forEach(f => f.remove());
    document.querySelectorAll('.diff .cm-on').forEach(x => x.classList.remove('cm-on'));
    lineEl.classList.add('cm-on');
    const f = h(`<form class="cmf"><div class="cm-w"><b>${esc(where)}</b>${del ? ' (removed line)' : ''}<code>${esc(code.slice(0, 120))}</code></div>
      <textarea rows="2" placeholder="Comment for ${esc(nameOf(a, this.pane))}"></textarea>
      <div class="btns"><button class="btn primary sm">Add to draft</button><button type="button" class="btn sm no">Cancel</button></div></form>`);
    hunk.append(f);
    const ta = $('textarea', f); ta.focus();
    $('.no', f).onclick = () => { f.remove(); lineEl.classList.remove('cm-on'); };
    f.onsubmit = e => {
      e.preventDefault();
      const note = ta.value.trim(); if (!note) return;
      const msg = `In ${where}${del ? ` (removed \`${code.slice(0, 80)}\`)` : ''}: ${note}`;
      const k = 'draft.' + a.pane, cur = ls.get(k, '');
      ls.set(k, cur ? cur.replace(/\s*$/, '') + '\n' + msg : msg);
      haptic(); f.remove(); lineEl.classList.remove('cm-on'); lineEl.classList.add('cm-done');
      this.count = (this.count || 0) + 1;
      let b = $('.cmbar', this.el);
      if (!b) { b = h('<div class="cmbar"><span></span><button class="btn primary sm">Send to agent</button></div>'); this.el.append(b); }
      $('span', b).textContent = `${this.count} comment${this.count > 1 ? 's' : ''} in ${nameOf(a, this.pane)}’s draft`;
      $('button', b).onclick = () => nav(a.home ? '#/' : '#/a/' + enc(a.pane));
    };
  }
}

// ---------- usage rings ----------
const U = { d: null, at: 0, p: null };
export function loadUsage(maxAge = 60000) {
  if (U.d && Date.now() - U.at < maxAge) return Promise.resolve(U.d);
  return U.p ||= api('usage').then(d => {
    const old = JSON.stringify(U.d?.limits);
    U.d = d; U.at = Date.now(); U.p = null;
    try { ls.set('usage.last', { limits: d.limits, today: d.today, at: U.at }); } catch { }
    if (old !== JSON.stringify(d.limits)) G.view?.onState?.(G.st);
    return d;
  }, e => { U.p = null; throw e; });
}
const lim = (d, k) => d?.limits?.find(x => x.kind === k);
const lvl = p => p == null ? '' : p >= 85 ? ' hi' : p >= 70 ? ' mid' : '';
// two concentric rings: outer = 5-hour, inner = 7-day
function rings(f, s, size = 26) {
  const r1 = 11, r2 = 6.5, c1 = 2 * Math.PI * r1, c2 = 2 * Math.PI * r2;
  const arc = (r, c, p, cls) => `<circle class="ur-bg" r="${r}" cx="14" cy="14"/>` + (p == null ? '' : `<circle class="ur-fg${cls}" r="${r}" cx="14" cy="14" stroke-dasharray="${(Math.min(100, p) / 100 * c).toFixed(1)} ${c.toFixed(1)}"/>`);
  return `<svg class="urings" viewBox="0 0 28 28" width="${size}" height="${size}">${arc(r1, c1, f, lvl(f))}${arc(r2, c2, s, lvl(s))}</svg>`;
}
// ---- Master header icon (feedback f1a126f673e1): ONE 32px "tank" for this chat's context + both plan windows ----
// centre: a disc that fills from the bottom with context, its % on top (a spinner while compacting);
// thick SOLID inner ring = 5-hour; thin outer ring cut into 7 day-segments = weekly. Each part takes its own level
// colour (amber from 70 %, red from 85 %), so the one near its limit is the one that changes colour. Cached usage,
// refreshed in the background; tapping it opens overviewSheet().
const PT = (r, deg) => { const t = (deg - 90) * Math.PI / 180; return [(16 + r * Math.cos(t)).toFixed(2), (16 + r * Math.sin(t)).toFixed(2)]; };
const ARC = (r, a0, a1) => { const [x0, y0] = PT(r, a0), [x1, y1] = PT(r, a1); return `M${x0} ${y0}A${r} ${r} 0 ${a1 - a0 > 180 ? 1 : 0} 1 ${x1} ${y1}`; };
const P100 = p => Math.min(100, Math.max(0, p ?? 0));
export function tankSVG(c, f, s, busy) {
  const cl = lvl(c), y = (16 + 8.4 - 16.8 * P100(c) / 100).toFixed(2), SEG = 360 / 7, GAP = 11, fa = P100(s) * 3.6;
  let wk = '';
  for (let i = 0; i < 7; i++) {
    const a0 = i * SEG + GAP / 2, a1 = (i + 1) * SEG - GAP / 2;
    wk += `<path class="tk-bg" d="${ARC(15, a0, a1)}"/>`;
    if (s != null && fa > a0) wk += `<path class="tk-wk${lvl(s)}" d="${ARC(15, a0, Math.max(a0 + 1.5, Math.min(a1, fa)))}"/>`;
  }
  const five = `<circle class="tk-bg tk-5" r="11.4" cx="16" cy="16"/>` + (f == null || f <= 0 ? '' :
    `<circle class="tk-5${lvl(f)}" r="11.4" cx="16" cy="16" pathLength="100" stroke-dasharray="${Math.max(1.5, P100(f)).toFixed(1)} 100" transform="rotate(-90 16 16)"/>`);
  return `<svg class="tank" viewBox="0 0 32 32" aria-hidden="true"><defs><clipPath id="tk-clip"><circle cx="16" cy="16" r="8.4"/></clipPath></defs>` +
    `<circle class="tk-in" cx="16" cy="16" r="8.4"/>` + (c == null ? '' : `<rect class="tk-lq${cl}" x="7.6" y="${y}" width="16.8" height="16.8" clip-path="url(#tk-clip)"/>`) +
    five + wk + (busy || c == null ? '' : `<text x="16" y="16.5">${Math.round(c)}</text>`) + '</svg>';
}
function fused(a) {
  const d = U.d || ls.get('usage.last', null);
  if (!U.d || Date.now() - U.at > 60000) loadUsage().catch(() => { });
  const c = a?.ctx?.pct, f = lim(d, 'five_hour')?.pct, s = lim(d, 'seven_day')?.pct, busy = !!a?.ctx?.compacting;
  const pc = x => x != null ? Math.round(x) + '%' : '—';
  return `<span class="ovf" title="Context ${busy ? 'compacting' : pc(c)} · 5-hour ${pc(f)} · weekly ${pc(s)}">${tankSVG(c, f, s, busy)}${busy ? '<span class="spin"></span>' : ''}</span>`;
}
G.usage = { fused };
G.view?.onState?.(G.st); // the Master header may already be up without its usage half
// keep the header fresh while the Master chat is open
setInterval(() => { if (!document.hidden && G.view?.home) loadUsage().catch(() => { }); }, 60000);

const SRC = { mod: 'reported by Claude', 'mod+est': 'reported + estimated since', est: 'estimated from transcripts', transcript: 'no % yet — cost from transcripts' };
const LIML = { five_hour: 'Session · 5-hour limit', seven_day: 'Weekly limit', seven_day_opus: 'Weekly · Opus' };
function limRow(x) {
  if (!x) return '';
  const sub = [x.resetsAt && inMs(x.resetsAt) ? 'resets in ' + inMs(x.resetsAt) : '', `${usd(x.cost)} · ${kn(x.tokens)} tokens in window`, SRC[x.src] || ''].filter(Boolean).join(' · ');
  return `<div class="u-lim">${rings(x.pct, null, 44)}<div><b>${esc(LIML[x.kind] || x.label + ' limit')}</b><span class="u-pct${lvl(x.pct)}">${x.pct != null ? Math.round(x.pct) + '%' : '—'}</span><div class="dim small">${esc(sub)}</div></div></div>`;
}
function daysChart(days) {
  if (!days?.length) return '';
  const max = Math.max(...days.map(d => d.cost), 0.01);
  return `<div class="u-days">${days.map(d => `<div class="u-day" title="${esc(d.date)} ${usd(d.cost)}"><i style="height:${Math.max(2, d.cost / max * 100).toFixed(0)}%"></i><small>${esc(d.date.slice(8))}</small></div>`).join('')}</div>`;
}
// one overview: this chat's context (+ Compact now), the plan's limits, today, every agent's context. `a` = the
// chat it was opened from (the Master header button); null from Machine / Settings / History (no "This chat" part).
const ring = c => c?.pct == null ? '<span class="ctxg"></span>' : `<span class="ctxg${c.pct >= 85 ? ' hi' : c.pct >= 70 ? ' mid' : ''}" style="--p:${Math.round(c.pct)}">${Math.round(c.pct)}%</span>`;
const ktok = c => c?.tokens != null && c?.window ? `${kn(c.tokens)} of ${kn(c.window)}` : '';
function chatPart(a) {
  const c = a.ctx || {}, p = c.pct;
  const hint = c.compacting ? 'Compacting…' : p == null ? 'Measured after the next response' : p >= 85 ? 'Nearly full: compact now' : p >= 60 ? 'Getting full: compact soon' : 'Plenty of room';
  const sub = [ktok(c) && ktok(c) + ' tokens', c.costUsd != null && usd(c.costUsd) + ' so far', c.est && 'estimated from transcript'].filter(Boolean).join(' · ');
  const can = a.mod || a.status === 'idle' || a.status === 'done';
  return `<h3>This chat · ${esc(nameOf(a))}</h3><div class="ov-chat">${bar(p, 'Context', esc(sub))}
    <div class="ov-hint${p >= 85 ? ' hi' : p >= 60 ? ' mid' : ''}">${hint}</div>
    ${can ? `<details class="ov-cx"><summary class="btn sm${p >= 60 ? ' primary' : ''}">Compact now…</summary><textarea class="cxi" rows="2" placeholder="Optional: what the summary should keep"></textarea>
    <div class="btns col"><button class="btn primary go">Compact now</button></div>${a.status === 'working' ? '<p class="dim small">Runs when the current turn ends.</p>' : ''}</details>` : '<p class="dim small">Compacting needs the agent idle or the sheprd mod connected.</p>'}</div>`;
}
export async function overviewSheet(a) {
  const sh = sheet(`<h2>Context &amp; usage</h2>${a ? chatPart(a) : ''}<div class="u-b"><div class="stline"><span class="spin"></span></div></div>`, 'tall');
  const go = $('.go', sh);
  if (go) go.onclick = () => { sh.close(); compactNow(a, $('.cxi', sh)?.value); };
  sh.addEventListener('click', e => { if (e.target.closest('a')) sh.close(); });
  const box = $('.u-b', sh);
  const agents = (G.st?.agents || []).filter(x => x.ctx || x.session);
  let d = null, err = '';
  try { d = await loadUsage(15000); } catch (e) { err = e.message; }
  const rows = agents.map(x => ({ a: x, c: x.ctx, cost: x.ctx?.costUsd ?? d?.sessions?.[x.session]?.cost }))
    .filter(r => r.c?.pct != null || r.cost != null)
    .sort((x, y) => (y.a.home ? 1 : 0) - (x.a.home ? 1 : 0) || (y.c?.pct ?? -1) - (x.c?.pct ?? -1));
  box.innerHTML = (d ? `<h3>Claude plan</h3><div class="u-lims">${(d.limits || []).map(limRow).join('') || '<p class="dim small">No limits reported yet.</p>'}</div>
      <h3>Today · ${usd(d.today?.cost)} · <span class="dim small">${kn(d.today?.tokens || 0)} tokens</span></h3>
      ${daysChart(d.days)}` : `<p class="dim pad">Usage unavailable: ${esc(err)}</p>`) + `
      <h3>Agents · context</h3>
      ${rows.length ? `<div class="ov-ags">${rows.map(r => `<a class="frow u-row ov-ag" href="${r.a.home ? '#/' : '#/a/' + enc(r.a.pane)}">${ring(r.c)}<span class="sb"><b>${esc(nameOf(r.a))}</b><small>${[ktok(r.c) && ktok(r.c) + ' tokens', r.c?.est && 'est.'].filter(Boolean).join(' · ') || 'no context yet'}</small></span><span class="fm">${usd(r.cost)}</span></a>`).join('')}</div>` : '<p class="dim small">No context data for running agents yet.</p>'}
      <p class="dim small">Cost is the API-list-price equivalent of the transcripts’ usage rows (on a subscription it is a gauge, not a bill). Limits come from the sheprd mod; without it they are estimated from the last reported percentage.</p>`;
}
export const usageSheet = () => overviewSheet(null);
export async function settingsPart(box) {
  box.innerHTML = '<h3>Claude usage</h3><div class="group"><button class="kv u-open" style="width:100%"><span>Loading…</span><b>›</b></button></div>';
  try {
    const d = await loadUsage(30000), f = lim(d, 'five_hour'), s = lim(d, 'seven_day');
    const p = x => x?.pct != null ? Math.round(x.pct) + '%' : usd(x?.cost);
    $('.u-open', box).innerHTML = `<span class="u-set">${rings(f?.pct, s?.pct, 28)} 5h ${p(f)} · 7d ${p(s)}</span><b>today ${usd(d.today?.cost)} ›</b>`;
  } catch { $('.u-open span', box).textContent = 'Unavailable'; }
  $('.u-open', box).onclick = () => usageSheet();
}
