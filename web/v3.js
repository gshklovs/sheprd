// v3 (lazy): plan card, chat "⋯" menu (open on desk, tell me when…, review, fork),
// watches, review result cards in the reviewed chat, re-ping setting.
import { G, I, $, h, esc, api, post, ls, sheet, toast, haptic, tick, md, nav, surfHTML, agentHref, lazy, HOME_NAME } from './app.js';

const enc = encodeURIComponent;
const sleep = ms => new Promise(r => setTimeout(r, ms));

// ---------- plan card (ExitPlanMode) ----------
// Maps the approval menu Claude shows to our three buttons by label, so it keeps
// working across Claude versions ("Yes, and use auto mode" / "Yes, and auto-accept
// edits", "Yes, manually approve edits", "Tell Claude what to change" / "No, keep planning").
export function planChoices(list) {
  const L = list.map((o, i) => ({ i, l: String(o.label || '').toLowerCase() }));
  const auto = L.find(o => /^yes/.test(o.l) && /auto|bypass/.test(o.l));
  const manual = L.find(o => /^yes/.test(o.l) && o !== auto && !/bypass/.test(o.l));
  const keep = L.find(o => /keep planning|tell claude|^no\b/.test(o.l));
  return { auto: auto?.i ?? null, manual: manual?.i ?? null, keep: keep?.i ?? null };
}

// Renders the plan card into the blocked panel's option box. Returns false when
// the menu does not look like a plan approval (the caller falls back to options).
export function renderPlan(view, opts, plan, list) {
  const c = planChoices(list);
  if (list.length >= 2 && c.auto == null && c.manual == null) return false;
  const open = view.planOpen ?? true;
  const btn = (k, i, label, cls = '') => i == null ? '' : `<button class="opt pl-b ${cls}" data-${k === 'keep' ? 'pk' : 'o'}="${i}"><span class="ol">${label}</span></button>`;
  opts.hidden = false;
  opts.innerHTML = `<div class="plan${open ? ' open' : ''}">
      <button class="pl-h"><span class="pl-t">Plan</span><span class="pl-s">${esc(firstLine(plan.text))}</span><span class="pl-c">${open ? 'Hide' : 'Show'}</span></button>
      <div class="pl-md md">${md(plan.text, null)}</div>
    </div>
    ${list.length < 2 ? '<div class="dim small pl-wait"><span class="spin"></span> Waiting for the approval menu…</div>' : ''}
    ${btn('o', c.manual, 'Approve')}${btn('o', c.auto, 'Approve + auto-accept edits', 'pl-auto')}${btn('keep', c.keep, 'Keep planning…', 'pl-keep')}
    <form class="pl-note" hidden><textarea rows="2" placeholder="What should change? (optional)"></textarea><button class="btn primary sm">Send</button></form>`;
  $('.pl-h', opts).onclick = () => { view.planOpen = !open; view.blOpts = null; view.renderOpts(); };
  const note = $('.pl-note', opts);
  const keepB = $('[data-pk]', opts);
  if (keepB) keepB.onclick = e => { e.stopPropagation(); note.hidden = !note.hidden; if (!note.hidden) $('textarea', note).focus(); };
  note.onsubmit = async e => {
    e.preventDefault(); haptic();
    const text = $('textarea', note).value.trim(), b = $('button', note);
    b.disabled = true; b.innerHTML = '<span class="spin"></span>';
    try {
      await post(`a/${enc(view.pane)}/choose`, { index: +keepB.dataset.pk });
      if (text) await promptWhenFree(view.pane, text);
      toast(text ? 'Sent: keep planning' : 'Plan rejected, still planning', 'ok', null, 1800);
    } catch (err) { toast('Failed: ' + esc(err.message), 'err'); b.disabled = false; b.textContent = 'Send'; }
  };
  return true;
}
const firstLine = t => String(t || '').split('\n').map(l => l.replace(/^#+\s*/, '').trim()).find(Boolean) || '';

// The agent leaves the menu a moment after the choice: retry while it is still blocked.
async function promptWhenFree(pane, text) {
  for (let k = 0; k < 20; k++) {
    try { return await post(`a/${enc(pane)}/prompt`, { text }); }
    catch (e) { if (e.status !== 409) throw e; await sleep(400); }
  }
  throw new Error('agent still busy');
}

// ---------- chat ⋯ menu ----------
export function chatMenu(view) {
  const a = view.agent; if (!a) return;
  const n = a.watches?.length || 0;
  const rows = [
    a.home && ['new', 'New chat'], // was the pencil in the home agent's header
    a.home && ['send', 'Send to ' + HOME_NAME() + '…'], // link / note / photo / voice + a note, and the Share-button Shortcut (send.js)
    ['desk', 'Open on desk'],
    ['watch', 'Tell me when…' + (n ? ` (${n} set)` : '')],
    !a.home && ['review', 'Review this'],
    ['git', 'Branch & CI' + (a.ci?.state === 'fail' ? ' · CI red' : a.ci?.pr ? ` · PR #${a.ci.pr}` : '')], // git.js
    a.mod && ['catch', 'Catch me up'], // talk.js
    ['mode', 'Mode (plan/auto) · ⇧Tab'], // was the ⇧Tab key above the composer
    ['fork', 'Fork session'],
    view.target === view.pane && a.session && ['undo', 'Undo last turn…'], // undo.js
    ['term', view.screen ? 'Back to chat' : 'Terminal'],
    // moved here from the crowded chat header
    view.subs?.length && ['subs', `Subagents · ${view.subs.filter(s => s.running).length ? view.subs.filter(s => s.running).length + ' running of ' : ''}${view.subs.length}`],
    view.filesR?.files?.length && ['files', `Files touched · ${view.filesR.files.length}`],
  ].filter(Boolean);
  const sh = sheet(`<h2>${esc(a.home ? HOME_NAME() : a.title || a.pane)}</h2><div class="acts">${rows.map(([k, l]) => `<button class="act" data-a="${k}">${esc(l)}</button>`).join('')}</div>`);
  sh.addEventListener('click', e => {
    const b = e.target.closest('[data-a]'); if (!b) return;
    const k = b.dataset.a;
    if (k === 'watch') { sh.close(); setTimeout(() => watchSheet(a), 120); return; }
    sh.close();
    if (k === 'desk') focusDesk(a);
    else if (k === 'review') review(a);
    else if (k === 'fork') fork(a);
    else if (k === 'undo') lazy('undo').then(m => m.undoSheet(view), e => toast('Load failed: ' + esc(e.message), 'err'));
    else if (k === 'mode') cycleMode(a);
    else if (k === 'catch') lazy('talk').then(m => m.catchUp(view));
    else if (k === 'git') location.hash = '#/g/' + enc(a.pane);
    else if (k === 'term') view.toggleScreen();
    else if (k === 'new') view.newChat?.();
    else if (k === 'send') lazy('send').then(m => m.sendSheet(), e => toast('Load failed: ' + esc(e.message), 'err'));
    else if (k === 'subs') view.subsSheet();
    else if (k === 'files') lazy('files').then(m => m.filesSheet(view.tid, view.filesR));
  });
}

// Claude Code's shift+tab cycles default → auto-accept → plan mode; the terminal footer shows which
export function cycleMode(a) {
  haptic();
  post(`a/${enc(a.pane)}/keys`, { keys: ['shift+tab'] }).then(() => toast('Mode switched (⇧Tab) — tap again to cycle', '', null, 1800), e => toast('Failed: ' + esc(e.message), 'err'));
}

export function focusDesk(a) {
  haptic();
  post(`a/${enc(a.pane)}/focus`).then(() => toast('Showing on the desk', 'ok', null, 1600), e => toast('Failed: ' + esc(e.message), 'err'));
}

async function review(a) {
  if (!confirm(`Start a reviewer agent on ${a.title || a.pane}'s changes?\n\nIts findings come back here as a card.`)) return;
  haptic();
  try {
    const r = await post(`a/${enc(a.pane)}/review`);
    G.starting.set(r.pane, Date.now());
    toast(`Reviewer started (${r.files} file${r.files === 1 ? '' : 's'}) — tap to watch`, 'ok', () => nav('#/a/' + enc(r.pane)), 4000);
  } catch (e) { toast('Failed: ' + esc(e.message), 'err'); }
}

async function fork(a) {
  if (!confirm(`Fork ${a.home ? HOME_NAME() : a.title || a.pane}?\n\nA new tab resumes this conversation as a separate session (same folder).`)) return;
  haptic();
  try {
    const r = await post(`a/${enc(a.pane)}/fork`);
    G.starting.set(r.pane, Date.now());
    nav('#/a/' + enc(r.pane));
    toast('Forked', 'ok', null, 1400);
  } catch (e) { toast('Failed: ' + esc(e.message), 'err'); }
}

// ---------- "Tell me when…" ----------
export async function watchSheet(a) {
  const sh = sheet(`<h2>Tell me when…</h2>
    <div class="acts">
      <button class="act" data-w="done">${esc(a.home ? HOME_NAME() : 'It')} is done</button>
      <button class="act" data-w="blocked">It needs me</button>
      <button class="act" data-w="both">Done or needs me</button>
    </div>
    <h3>Output matches</h3>
    <form class="form wout"><input class="pat" placeholder="text in its terminal, e.g. Build finished" autocapitalize="off" autocorrect="off" spellcheck="false">
      <label class="kv rx"><span>Regex</span><input type="checkbox" switch class="re"></label>
      <button class="btn primary">Watch output</button></form>
    <div class="wjobs"></div>
    <h3>Active</h3><div class="wlist"><div class="dim pad"><span class="spin"></span></div></div>
    <p class="dim small">Each watch sends one push, then clears itself.</p>`, 'tall');
  const add = async body => {
    haptic();
    try { const r = await post('watches', { pane: a.pane, ...body }); toast('Watching: ' + esc(r.watch.label), 'ok', null, 1800); sh.close(); }
    catch (e) { toast('Failed: ' + esc(e.message), 'err'); }
  };
  sh.querySelectorAll('[data-w]').forEach(b => b.onclick = () => add({ kind: 'state', until: b.dataset.w === 'both' ? ['done', 'blocked'] : [b.dataset.w] }));
  $('.wout', sh).onsubmit = e => {
    e.preventDefault();
    const p = $('.pat', sh).value.trim(); if (!p) return;
    add({ kind: 'output', pattern: p, regex: $('.re', sh).checked });
  };
  api('jobs').then(r => {
    const js = (r.jobs || []).filter(j => j.pane === a.pane && j.status === 'running');
    if (!js.length) return;
    const box = $('.wjobs', sh);
    box.innerHTML = `<h3>Background job ends</h3><div class="acts">${js.map(j => `<button class="act" data-job="${esc(j.id)}">${esc(j.desc || j.cmd)}</button>`).join('')}</div>`;
    box.onclick = e => { const b = e.target.closest('[data-job]'); if (b) add({ kind: 'job', job: b.dataset.job }); };
  }).catch(() => { });
  watchList($('.wlist', sh), a.pane);
}

// Active watches (of one pane, or all), each with a cancel button.
export async function watchList(box, pane) {
  const draw = list => {
    list = pane ? list.filter(w => w.pane === pane) : list;
    box.innerHTML = list.length ? `<div class="group">${list.map(w => `<div class="kv wrow"><span>${pane ? '' : `<b>${esc(w.title)}</b> · `}${esc(w.label)} · <i data-since="${w.created}"></i></span><button class="btn sm" data-x="${esc(w.id)}">Cancel</button></div>`).join('')}</div>`
      : '<p class="dim small">None.</p>';
    tick(box);
  };
  let list = [];
  try { list = (await api('watches')).watches || []; draw(list); } catch (e) { box.innerHTML = `<p class="dim small">Couldn’t load: ${esc(e.message)}</p>`; }
  box.onclick = async e => {
    const b = e.target.closest('[data-x]'); if (!b) return;
    try { await post(`watches/${enc(b.dataset.x)}/cancel`); list = list.filter(w => w.id !== b.dataset.x); draw(list); toast('Watch cancelled', '', null, 1200); }
    catch (err) { toast('Failed: ' + esc(err.message), 'err'); }
  };
}

// ---------- per-chat state: watch dot, reviewer results ----------
export function chatState(view, st) {
  const a = view.agent, more = $('.more3, .wmk', view.el); // home agent: the name carries the menu's dot
  if (more) more.classList.toggle('on', !!a?.watches?.length);
  if (!view.pane || view.dead) return;
  if (!view.loaded) { clearTimeout(view.rvT); view.rvT = setTimeout(() => chatState(view, G.st), 700); return; }
  // A reviewer's result is a surface redirected to this chat: show it inline at its index.
  for (const s of st?.surfaces || []) {
    if (!s.review || s.pane !== view.pane) continue;
    if ($(`.rvc[data-id="${CSS.escape(String(s.id))}"]`, view.list)) continue;
    const el = h(`<div class="rvc" data-id="${esc(s.id)}">${surfHTML({ ...s, replies: [] }, { inline: true, pane: view.pane })}
      <div class="rv-f"><span class="dim small">from ${esc(s.review.title || s.review.pane)}</span><button class="btn sm rv-fix">Send fixes…</button></div></div>`);
    $('.rv-fix', el).onclick = () => {
      const ta = view.ta;
      ta.value = `Please address these review findings${s.review.title ? ` (from ${s.review.title})` : ''}:\n\n## ${s.title}\n${s.body || ''}\n` + (ta.value ? '\n' + ta.value : '');
      ta.dispatchEvent(new Event('input')); ta.focus();
    };
    const after = s.i == null ? null : [...view.list.children].find(c => +(c.dataset.i ?? c.querySelector('[data-i]')?.dataset.i ?? -1) > s.i);
    view.list.insertBefore(el, after || null);
  }
}

// ---------- settings: re-ping + all watches ----------
export async function settingsPart(box) {
  box.innerHTML = `<h3>Reminders</h3>
    <div class="group"><label class="kv"><span>Re-ping when an agent waits on you</span><select class="rp"><option value="0">Off</option><option value="15">after 15 min</option><option value="30">after 30 min</option><option value="60">after 1 h</option><option value="120">after 2 h</option></select></label></div>
    <p class="dim small">One reminder push per wait. Pushes are skipped while that agent is focused on the unlocked desk.</p>
    <h3>Watches</h3><div class="wall"><div class="dim pad"><span class="spin"></span></div></div>`;
  const sel = $('.rp', box);
  try {
    const s = await api('settings');
    if (![...sel.options].some(o => +o.value === s.repingMin)) sel.append(h(`<option value="${s.repingMin}">after ${s.repingMin} min</option>`));
    sel.value = String(s.repingMin);
  } catch { sel.disabled = true; }
  sel.onchange = () => post('settings', { repingMin: +sel.value }).then(() => toast('Saved', 'ok', null, 1000), e => toast('Failed: ' + esc(e.message), 'err'));
  watchList($('.wall', box), null);
}

// ---------- new agent in a git worktree ----------
export async function startWorktree(body) {
  const r = await post('new/worktree', body);
  toast(`Worktree ${esc(r.branch)} · ${esc(r.path.replace(/^\/home\/[^/]+/, '~'))}`, 'ok', null, 3000);
  return r;
}

export { agentHref };
