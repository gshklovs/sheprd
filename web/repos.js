// Lazy: Repos tab (#/repos) — the repos being worked on now (live agents' repos + ~/Work repos active in the last
// 14 days): one card per repo (branch chips, dirty / ahead badges, the agents' pastel discs, last commit); tap opens the
// repo sheet (GitHub / PR / Vercel links, branches & worktrees, last commits, agents, Deploy to Vercel with its log).
// Preview is the default; production asks again. No token: the sheet shows the one-line command, nothing runs.
// API.md "Repos". GitHub repo and PR pages open the GitHub app (github.com's apple-app-site-association covers
// /<o>/<r> and /pull/N; /tree/<branch> stays in the browser).
import { G, I, $, h, esc, api, post, sheet, toast, haptic, tick, shortPath, goBack, copyText, agentHref } from './app.js';

await new Promise(r => {
  if (document.querySelector('link[data-repos]')) return r();
  const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'repos.css'; l.dataset.repos = 1;
  l.onload = l.onerror = r; document.head.append(l); setTimeout(r, 3000);
});

const enc = encodeURIComponent;
const ago = ts => ts ? `<span data-since="${ts}" data-ago></span>` : '';
const pl = (n, w) => `${n} ${w}${n === 1 ? '' : 's'}`;
const ext = (href, label, ev, cls = 'btn sm') => `<a class="${cls}" href="${esc(href)}" target="_blank" rel="noopener" data-ev="${ev}">${label}</a>`;
const BR = '<svg viewBox="0 0 24 24"><circle cx="6" cy="5" r="2"/><circle cx="6" cy="19" r="2"/><circle cx="18" cy="8" r="2"/><path d="M6 7v10M18 10c0 4-6 3-11 7"/></svg>';
const GH = '<svg viewBox="0 0 24 24" class="ghi"><path d="M12 2a10 10 0 0 0-3.2 19.5c.5.1.7-.2.7-.5v-1.7c-2.8.6-3.4-1.3-3.4-1.3-.5-1.2-1.1-1.5-1.1-1.5-.9-.6.1-.6.1-.6 1 .1 1.5 1 1.5 1 .9 1.6 2.4 1.1 3 .9.1-.7.4-1.1.6-1.4-2.2-.3-4.6-1.1-4.6-5 0-1.1.4-2 1-2.7-.1-.3-.4-1.3.1-2.7 0 0 .8-.3 2.7 1a9.4 9.4 0 0 1 5 0c1.9-1.3 2.7-1 2.7-1 .5 1.4.2 2.4.1 2.7.6.7 1 1.6 1 2.7 0 3.9-2.3 4.7-4.6 5 .4.3.7.9.7 1.9V21c0 .3.2.6.7.5A10 10 0 0 0 12 2z"/></svg>';
const VC = '<svg viewBox="0 0 24 24" class="vc"><path d="M12 3l10 18H2z"/></svg>';

let V = null;
export function route(hs) {
  const d = new URLSearchParams(hs.split('?')[1] || '').get('d');
  return {
    key: 'repos', make: () => (V ||= new ReposView()),
    focus: d ? () => { history.replaceState(history.state, '', '#/repos'); V?.openDeploy(d); } : null,
  };
}

const agentOf = pane => G.st?.agents.find(a => a.pane === pane);
const COLS = ['red', 'orange', 'yellow', 'green', 'cyan', 'blue', 'purple', 'pink'];
// an agent's pastel avatar disc (identity, never status): its initial in the agent's /color
function disc(x) {
  const a = agentOf(x.pane) || x, home = a.home, t = home ? 'Master' : a.title || x.title || a.pane || '?';
  const c = COLS.includes(a.color) ? ' col-' + a.color : '';
  return `<i class="rp-av${c}${home ? ' home' : ''}" title="${esc(t)}">${esc((t.match(/[A-Za-z0-9]/) || ['•'])[0].toUpperCase())}</i>`;
}
const nameOf = x => { const a = agentOf(x.pane) || x; return a.home ? 'Master' : a.title || a.pane; };

function badges(r) {
  let s = '';
  if (r.dirty > 0) s += `<span class="rp-b dirty">${r.dirty} changed</span>`;
  if (r.ahead > 0 || r.behind > 0) s += `<span class="rp-b">${r.ahead ? '↑' + r.ahead : ''}${r.ahead && r.behind ? ' ' : ''}${r.behind ? '↓' + r.behind : ''}</span>`;
  else if (r.gone) s += '<span class="rp-b">upstream gone</span>';
  else if (r.branch && !r.upstream) s += '<span class="rp-b">not pushed</span>';
  return s;
}

function cardHTML(r) {
  const wtb = (r.worktrees || []).filter(w => !w.main && w.branch && w.branch !== r.branch).map(w => w.branch);
  const chips = [`<span class="rp-br cur">${BR}${esc(r.branch || (r.head || 'detached'))}</span>`]
    .concat(wtb.slice(0, 2).map(b => `<span class="rp-br">${esc(b)}</span>`))
    .concat(wtb.length > 2 ? [`<span class="rp-br more">+${wtb.length - 2}</span>`] : []).join('');
  const dep = r.deploy ? `<div class="rp-dep"><span class="spin"></span>Deploying ${esc(r.deploy.branch)} to ${esc(r.deploy.target)}…</div>`
    : r.deploys?.[0] && Date.now() - (r.deploys[0].ended || 0) < 6 * 3600e3
      ? `<div class="rp-dep ${r.deploys[0].state}">${r.deploys[0].state === 'ok' ? '✓ Deployed' : '✕ Deploy failed'} · ${esc(r.deploys[0].branch)} · ${ago(r.deploys[0].ended)}</div>` : '';
  return `<button class="rp-card" data-root="${esc(r.root)}" data-ev="repo-open">
    <div class="rp-h"><b class="rp-n">${esc(r.name)}</b>${r.github ? '<span class="rp-gh" title="GitHub">' + GH + '</span>' : ''}${r.vercel?.linked || r.vercel?.config ? '<span class="rp-vc" title="Vercel">' + VC + '</span>' : ''}<span class="rp-ags">${(r.agents || []).slice(0, 5).map(disc).join('')}${r.agents?.length > 5 ? `<i class="rp-av more">+${r.agents.length - 5}</i>` : ''}</span></div>
    <div class="rp-chips">${chips}${badges(r)}</div>
    ${r.last ? `<div class="rp-last"><span class="rp-sub">${esc(r.last.subject)}</span><span class="rp-age">${ago(r.last.ts)}</span></div>` : '<div class="rp-last dim">No commits yet</div>'}
    ${dep}
  </button>`;
}

class ReposView {
  constructor() {
    this.keep = true; this.d = null; this.at = 0;
    this.el = h(`<div class="view reposv">
      <header class="bar"><div class="bar-in">
        <button class="ib back" aria-label="Back">${I.back}</button>
        <div class="ttl"><div class="t1">Repos</div><div class="t2">${esc(G.st?.host || '')}</div></div>
        <button class="ib rf" aria-label="Refresh" data-ev="repos-refresh"><svg viewBox="0 0 24 24"><path d="M20 12a8 8 0 1 1-2.3-5.6M20 4v5h-5"/></svg></button>
      </div></header>
      <div class="scroll rpsc"><div class="rp-list"><div class="stline"><span class="spin"></span> Loading…</div></div></div></div>`);
    this.list = $('.rp-list', this.el);
  }
  mount() {
    $('.back', this.el).onclick = () => goBack('#/agents');
    $('.rf', this.el).onclick = () => { haptic(); this.load(true); };
    this.list.addEventListener('click', e => {
      const c = e.target.closest('.rp-card'); if (!c) return;
      const r = this.d?.repos.find(x => x.root === c.dataset.root); if (r) repoSheet(r, this);
    });
    this.load();
  }
  resume() { if (this.scY != null) $('.rpsc', this.el).scrollTop = this.scY; if (Date.now() - this.at > 15000) this.load(); }
  suspend() { this.scY = $('.rpsc', this.el).scrollTop; }
  onState() { } // agent discs read G.st when drawn
  async load(force) {
    const rf = $('.rf', this.el); rf.classList.add('busy');
    try {
      this.d = await api('repos' + (force ? '?refresh=1' : '')); this.at = Date.now();
      this.render();
      clearTimeout(this.pt);
      if (this.d.pending) this.pt = setTimeout(() => this.load(), 4000); // PRs are being fetched
      return this.d;
    } catch (e) {
      if (!this.d) this.list.innerHTML = `<div class="stline">Couldn’t load: ${esc(e.message)}</div>`;
      else toast('Repos: ' + esc(e.message), 'err');
    } finally { rf.classList.remove('busy'); }
  }
  render() {
    const d = this.d, live = d.repos.filter(r => r.live), rest = d.repos.filter(r => !r.live);
    $('.t2', this.el).textContent = `${G.st?.host || ''} · ${pl(d.repos.length, 'repo')}`;
    let html = '';
    if (live.length) html += `<h3 class="rp-sec">Agents working in</h3>${live.map(cardHTML).join('')}`;
    if (rest.length) html += `<h3 class="rp-sec">Recent in ${esc(d.root || '~/Work')}</h3>${rest.map(cardHTML).join('')}`;
    if (!d.repos.length) html = `<div class="rp-empty">No repos with agents or recent commits in ${esc(d.root || '~/Work')}.</div>`;
    if (d.gh && d.gh !== 'ok') html += `<p class="rp-foot">${d.gh === 'missing' ? 'Install the GitHub CLI (gh) to see PRs.' : 'Run <code>gh auth login</code> to see PRs.'}</p>`;
    this.list.innerHTML = html;
    tick(this.list);
    this.sheet?.redraw?.();
  }
  // a push / card link "#/repos?d=<id>": open that deploy's repo sheet with its log
  async openDeploy(id) {
    const d = this.d || await this.load();
    let r = d?.repos.find(x => x.deploy?.id === id || x.deploys?.some(y => y.id === id));
    if (!r) { const j = await api('repos/deploy/' + enc(id)).catch(() => null); if (j) r = d?.repos.find(x => x.root === j.repo); }
    if (r) repoSheet(r, this, id); else toast('That deploy is gone', 'err');
  }
}

function branchRows(r) {
  const wtOf = b => (r.worktrees || []).find(w => w.branch === b.name);
  return (r.branches || []).map(b => {
    const w = wtOf(b), ags = w ? (r.agents || []).filter(a => a.worktree === w.path) : [];
    const meta = [b.ahead > 0 ? '↑' + b.ahead : '', b.behind > 0 ? '↓' + b.behind : '', b.gone ? 'upstream gone' : b.upstream ? '' : 'local only',
      w ? (w.main ? 'checked out' : 'worktree ' + shortPath(w.path)) : '', w?.dirty ? `${w.dirty} changed` : ''].filter(Boolean).join(' · ');
    return `<div class="rp-row${b.name === r.branch ? ' cur' : ''}">
      <div class="rp-rl"><b>${esc(b.name)}</b>${ags.map(disc).join('')}<small>${esc(meta)}${meta ? ' · ' : ''}${ago(b.ts)}</small></div>
      <div class="rp-rr">${b.pr ? ext(b.pr.url, `#${b.pr.number}${b.pr.state === 'OPEN' ? '' : ' ' + b.pr.state.toLowerCase()}`, 'repo-pr', 'rp-pr' + (b.pr.state === 'OPEN' ? ' open' : '')) : ''}${b.url ? ext(b.url, I.ext, 'repo-branch-gh', 'rp-ic') : ''}</div>
    </div>`;
  }).join('') + ((r.worktrees || []).filter(w => w.detached).map(w => `<div class="rp-row"><div class="rp-rl"><b>${esc(w.head || 'detached')}</b><small>detached worktree ${esc(shortPath(w.path))}${w.dirty ? ` · ${w.dirty} changed` : ''}</small></div></div>`).join(''))
    + (r.branchesMore ? `<div class="rp-row dim small">+${r.branchesMore} older branches</div>` : '');
}

function deployHTML(r, d, sel) {
  const v = d.vercel || {}, rv = r.vercel || {};
  const info = [rv.linked ? `Project <b>${esc(rv.name || rv.projectName || rv.projectId)}</b>${rv.monorepo ? ' (monorepo link)' : ''}` : rv.config ? 'vercel.json, not linked yet: the first deploy links or creates project <b>' + esc(r.name) + '</b>' : 'No Vercel config: the first deploy links or creates project <b>' + esc(r.name) + '</b>'];
  const prod = rv.production || rv.lastProduction;
  const urls = (prod ? `<a class="rp-url" href="${esc(prod)}" target="_blank" rel="noopener" data-ev="repo-prod-url"><span>Production</span>${esc(prod.replace(/^https:\/\//, ''))}</a>` : '')
    + (rv.previews || []).slice(0, 3).map(p => `<a class="rp-url" href="${esc(p.url)}" target="_blank" rel="noopener" data-ev="repo-preview-url"><span>${esc(p.branch)} ${ago(p.ts)}</span>${esc(p.url.replace(/^https:\/\//, ''))}</a>`).join('');
  if (!v.token) {
    return `<div class="rp-dbox grp"><div class="small">${info.join('')}</div>${urls}
      <div class="rp-tok"><b>Add Vercel token</b><p class="small dim">${v.tokenError ? esc(v.tokenError) : `Create a token at vercel.com/account/tokens, then run this once on ${esc(G.st?.host || 'the machine')} (it asks for the token without echoing it; the file is ${esc(v.tokenPath)}, mode 600):`}</p>
      <pre class="rp-cmd">${esc(v.setup || '')}</pre><button class="btn sm cp" data-ev="repo-token-copy">Copy command</button></div></div>`;
  }
  const wt = (r.worktrees || []).find(w => w.branch === sel), dirty = wt?.dirty || 0;
  const opts = (r.branches || []).map(b => `<option${b.name === sel ? ' selected' : ''}>${esc(b.name)}</option>`).join('');
  const busy = !!r.deploy;
  return `<div class="rp-dbox grp"><div class="small">${info.join('')}</div>${urls}
    <label class="rp-sel"><span>Branch</span><select data-ev="repo-branch">${opts}</select></label>
    ${dirty ? `<p class="rp-warn small">${pl(dirty, 'uncommitted change')} in ${esc(shortPath(wt.path))} won’t ship — only committed code deploys.</p>` : ''}
    <div class="btns col"><button class="btn primary dp" data-ev="repo-deploy-preview"${busy || !v.cli ? ' disabled' : ''}>${busy ? 'Deploy running…' : 'Deploy preview'}</button>
    <button class="btn dprod" data-ev="repo-deploy-prod"${busy || !v.cli ? ' disabled' : ''}>Production…</button></div>
    ${v.cli ? '' : '<p class="small dim">No Vercel CLI: install node (npx) or vercel.</p>'}</div>`;
}

function repoSheet(r, view, focusId) {
  const d = view.d;
  let sel = r.branch || r.branches?.[0]?.name || '';
  const pr = r.github?.prs?.find(p => p.state === 'OPEN' && r.branches.find(b => b.name === r.branch)?.pr?.number === p.number) || r.github?.prs?.find(p => p.state === 'OPEN');
  const links = [
    r.github ? ext(r.github.url, GH + 'GitHub', 'repo-gh') : '',
    pr ? ext(pr.url, `PR #${pr.number}`, 'repo-pr') : r.github?.branchUrl && r.branch ? ext(r.github.branchUrl, BR + esc(r.branch), 'repo-branch-gh') : '',
    r.vercel?.dashboard ? ext(r.vercel.dashboard, VC + 'Vercel', 'repo-vercel') : '',
  ].filter(Boolean).join('');
  const sh = sheet(`<h2 class="rp-t">${esc(r.name)}</h2>
    <div class="rp-path small dim">${esc(shortPath(r.root))}${r.origin ? ' · ' + esc(r.origin.replace(/^https:\/\/|\.git$/g, '')) : ''}</div>
    ${links ? `<div class="rp-links">${links}</div>` : ''}
    ${r.agents?.length ? `<h3>Agents</h3><div class="rp-grp grp">${r.agents.map(a => `<a class="rp-row rp-agent" href="${agentOf(a.pane) ? agentHref(agentOf(a.pane)) : '#/a/' + enc(a.pane)}" data-ev="repo-agent">${disc(a)}<div class="rp-rl"><b>${esc(nameOf(a))}</b><small>${esc(shortPath(a.worktree))}</small></div><span class="go">›</span></a>`).join('')}</div>` : ''}
    <h3>Deploy to Vercel</h3><div class="rp-dep-slot"></div>
    <div class="rp-logw" hidden><div class="rp-lh"><span class="rp-ls"></span></div><pre class="rp-log"></pre><div class="rp-lres"></div></div>
    <h3>Branches &amp; worktrees</h3><div class="rp-grp grp">${branchRows(r)}</div>
    <h3>Last commits</h3><div class="rp-grp grp">${(r.commits || []).map(c => `<div class="rp-row"><div class="rp-rl"><b class="rp-cs">${esc(c.subject)}</b><small><code>${esc(c.short)}</code> · ${esc(c.author)} · ${ago(c.ts)}</small></div></div>`).join('') || '<div class="rp-row dim small">No commits</div>'}</div>`, 'tall reposh');
  const slot = $('.rp-dep-slot', sh);
  const drawDeploy = () => {
    slot.innerHTML = deployHTML(r, view.d || d, sel); tick(slot);
    const s = $('select', slot); if (s) s.onchange = () => { sel = s.value; drawDeploy(); };
    const cp = $('.cp', slot); if (cp) cp.onclick = () => { copyText((view.d || d).vercel.setup); haptic(); toast('Copied', 'ok', null, 1200); };
    const dp = $('.dp', slot); if (dp) dp.onclick = () => go('preview');
    const pp = $('.dprod', slot); if (pp) pp.onclick = () => confirmProd();
  };
  // log polling
  let job = null, alive = true, timer = null;
  const logw = $('.rp-logw', sh), pre = $('.rp-log', sh), res = $('.rp-lres', sh), ls_ = $('.rp-ls', sh);
  const follow = async (id, fresh) => {
    job = { id, next: 0 }; clearTimeout(timer);
    logw.hidden = false; if (fresh) pre.textContent = ''; res.innerHTML = '';
    const step = async () => {
      if (!alive || !sh.isConnected || job?.id !== id) return;
      let j;
      try { j = await api(`repos/deploy/${enc(id)}?after=${job.next}`); } catch (e) { ls_.textContent = 'Log: ' + e.message; timer = setTimeout(step, 3000); return; }
      if (job.next === 0) pre.textContent = '';
      if (j.lines?.length) { const atEnd = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 8; pre.textContent += j.lines.join('\n') + '\n'; if (atEnd) pre.scrollTop = pre.scrollHeight; }
      job.next = j.next;
      ls_.innerHTML = `${j.state === 'running' ? '<span class="spin"></span>' : j.state === 'ok' ? '✓ ' : '✕ '}${esc(j.target)} · ${esc(j.branch)} <code>${esc((j.sha || '').slice(0, 7))}</code>${j.expired ? ' · log expired' : ''}`;
      logw.className = 'rp-logw ' + j.state;
      if (j.state === 'running') { timer = setTimeout(step, 1000); return; }
      r.deploy = null;
      if (j.state === 'ok' && j.url) res.innerHTML = `<div class="btns">${ext(j.url, (j.target === 'production' ? 'Open production' : 'Open preview') + ' ' + I.ext, 'repo-open-deploy', 'btn primary sm')}<button class="btn sm cpu" data-ev="repo-copy-url">Copy link</button></div>`;
      else if (j.state === 'failed') res.innerHTML = `<p class="rp-err small">${esc(j.error || 'Deploy failed')} — the log’s last lines are above.</p>`;
      const cu = $('.cpu', res); if (cu) cu.onclick = () => { copyText(j.url); haptic(); toast('Copied', 'ok', null, 1200); };
      view.load().then(() => { const nr = view.d?.repos.find(x => x.root === r.root); if (nr) Object.assign(r, nr); drawDeploy(); });
    };
    step();
  };
  const go = async (target, confirm) => {
    haptic();
    $('.dp', slot)?.setAttribute('disabled', ''); $('.dprod', slot)?.setAttribute('disabled', '');
    try {
      const j = await post('repos/deploy', { repo: r.root, branch: sel, target, ...(confirm ? { confirm: 'production' } : {}) });
      r.deploy = { id: j.id, branch: sel, target, state: 'running' };
      drawDeploy(); follow(j.id, true);
      setTimeout(() => logw.scrollIntoView({ block: 'nearest', behavior: 'smooth' }), 100);
    } catch (e) {
      if (e.status === 409) toast('A deploy of this repo is already running', 'err');
      else toast('Not deployed: ' + esc(e.message), 'err');
      drawDeploy();
    }
  };
  const confirmProd = () => {
    const c = sheet(`<h2 class="rp-ct">Deploy ${esc(r.name)}@${esc(sel)} to PRODUCTION?</h2>
      <p class="small dim">This replaces what’s live${r.vercel?.production ? ' at ' + esc(r.vercel.production.replace(/^https:\/\//, '')) : ''}. Only committed code on <b>${esc(sel)}</b> ships.</p>
      <div class="btns col"><button class="btn rp-danger yes" data-ev="repo-deploy-prod-confirm">Deploy to production</button><button class="btn no" data-ev="repo-deploy-prod-cancel">Cancel</button></div>`, 'compact');
    $('.yes', c).onclick = () => { c.close(); go('production', true); };
    $('.no', c).onclick = () => c.close();
  };
  drawDeploy();
  tick(sh);
  sh.addEventListener('click', e => { if (e.target.closest('.rp-agent')) sh.close(); });
  const runId = focusId || r.deploy?.id;
  if (runId) follow(runId, true);
  view.sheet = { redraw: () => { const nr = view.d?.repos.find(x => x.root === r.root); if (nr && !job) { Object.assign(r, nr); drawDeploy(); } } };
  const close = sh.close; sh.close = () => { alive = false; clearTimeout(timer); view.sheet = null; close(); };
  new MutationObserver((_, o) => { if (!sh.isConnected) { alive = false; clearTimeout(timer); if (view.sheet) view.sheet = null; o.disconnect(); } }).observe(document.body, { childList: true });
}
