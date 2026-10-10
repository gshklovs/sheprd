// Lazy: Apps tab (#/apps) — your self-hosted apps as a 2-column grid of tiles: the app's own 4:3 thumbnail (fetched
// and cached by the server, served same-origin), its name with a health dot (server-side check) and a one-line
// description. Tap opens the app (its https Tailscale Serve form when one exists, else its URL: http on a tailnet IP
// opens in Safari). Long-press: Open, Copy link, Refresh, Remove. Agents register with POST /api/launcher
// (server/src/launcher.rs, API.md "Apps (launcher)"); the empty state shows the one-liner.
import { G, I, $, h, esc, api, sheet, toast, haptic, tick, longPress, copyText, agentHref, nav, whoName, goBack, HOME_NAME } from './app.js';

await new Promise(r => {
  if (document.querySelector('link[data-appstab]')) return r();
  const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'apps-tab.css'; l.dataset.appstab = 1;
  l.onload = l.onerror = r; document.head.append(l); setTimeout(r, 3000);
});

const enc = encodeURIComponent;
const pl = (n, w) => `${n} ${w}${n === 1 ? '' : 's'}`;
const RF = '<svg viewBox="0 0 24 24"><path d="M20 12a8 8 0 1 1-2.3-5.6M20 4v5h-5"/></svg>';
const TRASH = '<svg viewBox="0 0 24 24"><path d="M4 7h16M10 11v6M14 11v6M6 7l1 12a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2l1-12M9 7V4h6v3"/></svg>';
const HEX = /^#[0-9a-f]{3}([0-9a-f]{3})?$/i;
const col = c => HEX.test(c || '') ? c : '';
const hostOf = u => { try { const x = new URL(u); return x.host + (x.pathname === '/' ? '' : x.pathname); } catch { return u; } };
const healthWord = x => {
  const hl = x.health;
  if (!hl) return ['wait', 'Checking…'];
  if (hl.state === 'up') return ['up', `Up · ${hl.ms} ms`];
  if (hl.state === 'error') return ['bad', `Answers with an error (${hl.error || 'HTTP ' + hl.code})`];
  return ['bad', `Not answering (${hl.error || 'down'})`];
};

let V = null;
export function route() { return { key: 'apps', make: () => (V ||= new AppsView()), focus: null }; }

function tileHTML(x) {
  const [hs, hw] = healthWord(x), bg = col(x.background), ac = col(x.accent);
  const style = (bg ? `--ap-bg:${bg};` : '') + (ac ? `--ap-ac:${ac};` : '');
  const art = x.thumb ? `<img src="${esc(x.thumb)}" alt="" loading="lazy" decoding="async">`
    : `<span class="ap-mono" aria-hidden="true">${esc((x.name.match(/[\p{L}\p{N}]/u) || ['•'])[0].toUpperCase())}</span>`;
  return `<a class="ap-tile" href="${esc(x.open)}" target="_blank" rel="noopener" data-id="${esc(x.id)}" data-ev="app-open"${style ? ` style="${style}"` : ''}>
    <span class="ap-th">${art}</span>
    <span class="ap-nm"><i class="ap-dot ${hs}" title="${esc(hw)}" aria-label="${esc(hw)}"></i><b>${esc(x.name)}</b></span>
    <span class="ap-ds">${esc(x.description || hostOf(x.open))}</span>
  </a>`;
}

function registerHelp(line) {
  return `<p class="ap-hint">Any agent on ${esc(G.st?.host || 'this computer')} can add its app. Serve an <code>app.json</code>
    (<code>id</code>, <code>name</code>, <code>description</code>, <code>url</code>, <code>thumbnail</code>: an 800×600 PNG you design) and run:</p>
    <pre class="ap-cmd">${esc(line)}</pre>
    <button class="btn sm ap-cp" data-ev="apps-copy-register">Copy command</button>
    <p class="ap-hint small">Or send the fields themselves in the JSON instead of <code>manifest</code>. Same id again = update.</p>`;
}

class AppsView {
  constructor() {
    this.keep = true; this.d = null; this.at = 0; this.polls = 0;
    this.el = h(`<div class="view appsv">
      <header class="bar"><div class="bar-in">
        <button class="ib back" aria-label="Back">${I.back}</button>
        <div class="ttl"><div class="t1">Apps</div><div class="t2">${esc(G.st?.host || '')}</div></div>
        <button class="ib add" aria-label="Add an app" data-ev="apps-add">${I.plus}</button>
        <button class="ib rf" aria-label="Refresh" data-ev="apps-refresh">${RF}</button>
      </div></header>
      <div class="scroll apsc"><div class="ap-list"><div class="stline"><span class="spin"></span> Loading…</div></div></div></div>`);
    this.list = $('.ap-list', this.el);
  }
  mount() {
    $('.back', this.el).onclick = () => goBack('#/agents');
    $('.rf', this.el).onclick = () => { haptic(); this.load(); };
    $('.add', this.el).onclick = () => { haptic(); addSheet(this.d?.register); };
    this.list.addEventListener('click', e => {
      const cp = e.target.closest('.ap-cp'); if (cp) { copyText(this.d?.register || ''); haptic(); toast('Copied', 'ok', null, 1200); return; }
      const t = e.target.closest('.ap-tile'); if (!t) return;
      if (Date.now() - (+t.dataset.lp || 0) < 900) { e.preventDefault(); return; } // that was a long-press
    });
    longPress(this.list, '.ap-tile', t => { const x = this.d?.apps.find(a => a.id === t.dataset.id); if (!x) return false; appSheet(x, this); });
    this.list.addEventListener('contextmenu', e => { if (e.target.closest('.ap-tile')) e.preventDefault(); });
    this.load();
  }
  resume() { if (this.scY != null) $('.apsc', this.el).scrollTop = this.scY; if (Date.now() - this.at > 15000) { this.polls = 0; this.load(); } }
  suspend() { this.scY = $('.apsc', this.el).scrollTop; clearTimeout(this.pt); }
  onState() { }
  async load() {
    const rf = $('.rf', this.el); rf.classList.add('busy');
    try {
      this.d = await api('launcher'); this.at = Date.now();
      this.render();
      clearTimeout(this.pt);
      if (this.d.checking && ++this.polls < 6 && this.el.isConnected) this.pt = setTimeout(() => this.load(), 2500); // health checks running
      return this.d;
    } catch (e) {
      if (!this.d) this.list.innerHTML = `<div class="stline">Couldn’t load: ${esc(e.message)}</div>`;
      else toast('Apps: ' + esc(e.message), 'err');
    } finally { rf.classList.remove('busy'); }
  }
  render() {
    const d = this.d;
    $('.t2', this.el).textContent = `${G.st?.host || ''} · ${pl(d.apps.length, 'app')}`;
    if (!d.apps.length) {
      this.list.innerHTML = `<div class="ap-empty"><div class="ap-eic" aria-hidden="true">${GRID}</div><h3>No apps yet</h3>${registerHelp(d.register)}</div>`;
      return;
    }
    const html = `<div class="ap-grid">${d.apps.map(tileHTML).join('')}</div>`;
    if (this.list.dataset.sig !== html) { this.list.innerHTML = html; this.list.dataset.sig = html; tick(this.list); }
    this.sheet?.redraw?.();
  }
}
const GRID = '<svg viewBox="0 0 24 24"><rect x="4" y="4" width="7" height="5.5" rx="1.5"/><rect x="13" y="4" width="7" height="5.5" rx="1.5"/><rect x="4" y="13" width="7" height="5.5" rx="1.5"/><rect x="13" y="13" width="7" height="5.5" rx="1.5"/></svg>';

function addSheet(line) {
  const sh = sheet(`<h2>Add an app</h2>${registerHelp(line || '')}`, 'apsh');
  $('.ap-cp', sh).onclick = () => { copyText(line || ''); haptic(); toast('Copied', 'ok', null, 1200); };
}

function appSheet(x, view) {
  const run = x.running, a = run?.pane && G.st?.agents.find(y => y.pane === run.pane);
  const draw = () => {
    const [hs, hw] = healthWord(x), bg = col(x.background);
    const where = x.https ? 'Opens over HTTPS (Tailscale Serve)' : x.reach === 'local' ? 'Only listens on this computer: the phone can’t open it'
      : x.open.startsWith('http:') ? 'Plain http: opens in Safari' : '';
    return `<div class="aps-top"${bg ? ` style="--ap-bg:${bg}"` : ''}><span class="ap-th sm">${x.thumb ? `<img src="${esc(x.thumb)}" alt="">` : ''}</span>
      <div class="tb"><b>${esc(x.name)}</b><span>${esc(x.description || '')}</span></div></div>
      <div class="aps-meta"><div><i class="ap-dot ${hs}"></i>${esc(hw)}</div><div class="mono">${esc(hostOf(x.open))}</div>${where ? `<div>${esc(where)}</div>` : ''}
      ${a ? `<button class="aps-run" data-a="owner" data-ev="app-owner">Served by ${esc(a.home ? HOME_NAME() : whoName(a.title || a.pane, a.pane))} ›</button>` : run ? `<div>Served by ${esc(run.agentTitle || 'an agent')}</div>` : ''}
      ${x.pullError ? `<div class="bad">app.json: ${esc(x.pullError)}</div>` : ''}${x.thumbError ? `<div class="bad">${esc(x.thumbError)}</div>` : ''}</div>
      <div class="acts">
        <a class="act" href="${esc(x.open)}" target="_blank" rel="noopener" data-a="open" data-ev="app-open-sheet">${I.ext}Open</a>
        <button class="act" data-a="copy" data-ev="app-copy">${I.copy}Copy link</button>
        ${x.https && x.url !== x.https ? `<button class="act" data-a="copyhttp" data-ev="app-copy-http">${I.copy}Copy its own URL</button>` : ''}
        ${x.manifest ? `<button class="act" data-a="refresh" data-ev="app-refresh">${RF}Refresh from app.json</button>` : ''}
        <button class="act danger" data-a="rm" data-ev="app-remove">${TRASH}Remove</button>
      </div>`;
  };
  const sh = sheet(draw(), 'apsh');
  const body = $('.sh-body', sh);
  sh.addEventListener('click', async e => {
    const b = e.target.closest('[data-a]'); if (!b) return;
    const k = b.dataset.a;
    if (k === 'open') { sh.close(); return; }
    if (k === 'copy' || k === 'copyhttp') { await copyText(k === 'copy' ? x.open : x.url); haptic(); toast('Copied', 'ok', null, 1200); return; }
    if (k === 'owner') { sh.close(); nav(agentHref(a)); return; }
    if (k === 'refresh') {
      b.disabled = true; b.innerHTML = '<span class="spin"></span> Refreshing…';
      try { const r = await api(`launcher/${enc(x.id)}/refresh`, { method: 'POST' }); Object.assign(x, r.app); body.innerHTML = draw(); view.load(); toast('Refreshed', 'ok', null, 1200); }
      catch (err) { toast('Refresh failed: ' + esc(err.message), 'err'); body.innerHTML = draw(); }
      return;
    }
    if (k === 'rm') {
      if (!b.classList.contains('arm')) { b.classList.add('arm'); b.innerHTML = `${TRASH}Remove ${esc(x.name)}? Tap again`; haptic(); setTimeout(() => { if (b.isConnected) { b.classList.remove('arm'); b.innerHTML = `${TRASH}Remove`; } }, 3500); return; }
      try { await api(`launcher/${enc(x.id)}`, { method: 'DELETE' }); sh.close(); toast(`Removed ${esc(x.name)}`, 'ok', null, 1500); view.load(); }
      catch (err) { toast('Couldn’t remove: ' + esc(err.message), 'err'); }
    }
  });
  return sh;
}
