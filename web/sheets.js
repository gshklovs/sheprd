// Lazily loaded sheets: snippets + recent prompts (F13), jobs (F11), message + card actions (F9, F12).
import { G, I, $, h, esc, api, post, ls, sheet, toast, haptic, tick, getPrefs, DEF_SNIPS, insertAt, cardAct, cdot, itemHref, copyText, dispStatus, LABEL, shortPath, agentHref, newAgentSheet, sortAgents, steerAgent, lazy, whoName, HOME_NAME } from './app.js';

// ---------- snippets + recent prompts (F13) ----------
export async function snippetSheet(ta, done) {
  const sh = sheet(`<div class="tabs"><button class="tab on" data-t="s">Snippets</button><button class="tab" data-t="r">Recent</button><span class="sp"></span><button class="btn sm edit">Edit</button></div>
    <div class="snl"><div class="dim pad"><span class="spin"></span></div></div>`, 'tall');
  const list = $('.snl', sh), editB = $('.edit', sh);
  let tab = 's', edit = false, recent = null;
  const prefs = await getPrefs();
  let snips = (prefs.snippets || DEF_SNIPS).slice();
  const pick = text => { sh.close(); insertAt(ta, text); done?.(); };
  const save = async () => {
    try { await post('prefs', { snippets: snips }); prefs.snippets = snips.slice(); }
    catch { ls.set('snips', snips); prefs.snippets = snips.slice(); }
  };
  const draw = () => {
    editB.hidden = tab !== 's';
    editB.textContent = edit ? 'Done' : 'Edit';
    if (tab === 'r') {
      if (!recent) { list.innerHTML = '<div class="dim pad"><span class="spin"></span> Loading…</div>'; return; }
      list.innerHTML = recent.length ? recent.map((p, k) => `<button class="snr" data-k="${k}"><span class="snt">${esc(p.text)}</span>${p.n > 1 ? `<i class="badge">×${p.n}</i>` : ''}</button>`).join('')
        : '<p class="dim pad">No recent prompts.</p>';
      return;
    }
    list.innerHTML = snips.map((x, k) => edit
      ? `<div class="snr ed" data-k="${k}"><span class="sb"><b>${esc(x.label || x.text)}</b><span class="snt">${esc(x.text)}</span></span>
          <button class="ib mv" data-mv="-1" aria-label="Move up"${k ? '' : ' disabled'}>↑</button><button class="ib mv" data-mv="1" aria-label="Move down"${k < snips.length - 1 ? '' : ' disabled'}>↓</button><button class="ib del" aria-label="Delete">${I.x}</button></div>`
      : `<button class="snr" data-k="${k}"><span class="sb"><b>${esc(x.label || x.text)}</b>${x.label && x.label !== x.text ? `<span class="snt">${esc(x.text)}</span>` : ''}</span></button>`).join('')
      + (edit ? `<form class="snadd"><input class="sl" placeholder="Label (optional)" autocomplete="off"><textarea class="stx" rows="2" placeholder="Snippet text"></textarea><button class="btn primary">Add snippet</button></form>` : '')
      + (!snips.length && !edit ? '<p class="dim pad">No snippets. Tap Edit to add some.</p>' : '');
    const f = $('.snadd', list);
    if (f) f.onsubmit = e => {
      e.preventDefault();
      const text = $('.stx', f).value.trim(); if (!text) return;
      snips.push({ id: Math.random().toString(36).slice(2, 8), label: $('.sl', f).value.trim() || text.slice(0, 40), text });
      save(); draw();
    };
  };
  sh.addEventListener('click', e => {
    const t = e.target.closest('.tab');
    if (t) {
      tab = t.dataset.t; sh.querySelectorAll('.tab').forEach(x => x.classList.toggle('on', x === t)); draw();
      if (tab === 'r' && !recent) api('prompts?limit=30').then(r => { recent = r.prompts || []; draw(); }, e => { list.innerHTML = `<p class="dim pad">Couldn’t load: ${esc(e.message)}</p>`; });
      return;
    }
    if (e.target.closest('.edit')) { edit = !edit; draw(); return; }
    const row = e.target.closest('.snr'); if (!row) return;
    const k = +row.dataset.k;
    if (tab === 'r') return pick(recent[k].text);
    if (!edit) return pick(snips[k].text);
    const mv = e.target.closest('[data-mv]');
    if (mv) { const j = k + +mv.dataset.mv; [snips[k], snips[j]] = [snips[j], snips[k]]; save(); draw(); return; }
    if (e.target.closest('.del')) { snips.splice(k, 1); save(); draw(); }
  });
  draw();
}

// ---------- workspace card long-press menu (F9) ----------
export function cardSheet(a) {
  const m = 'pinned' in a;
  const rows = [['rename', a.home ? 'Rename' : 'Rename & color'], m && ['pin', a.pinned ? 'Unpin' : 'Pin to top'], m && ['mute', a.muted ? 'Unmute' : 'Mute done pushes'],
    a.ctx && ['ctx', a.ctx.pct != null ? `Context ${Math.round(a.ctx.pct)}%${a.ctx.costUsd != null ? ' · $' + a.ctx.costUsd.toFixed(2) : ''}` : 'Context & limits'],
    (a.mod || a.status === 'idle' || a.status === 'done') && ['compact', 'Compact now'], ['term', 'Terminal'], ['desk', 'Open on desk'], ['mode', 'Mode (plan/auto) · ⇧Tab'], ['stop', 'Stop (Esc)'], ['copy', 'Copy pane id'], !a.home && ['close', 'Close tab']].filter(Boolean);
  // the raw tool call behind the activity phrase (server/src/activity.rs)
  const raw = a.status === 'working' && a.doing?.label ? `<p class="rawdo"${a.activity?.phrase ? ` title="${esc(a.activity.id)}"` : ''}>${a.activity?.phrase ? `${esc(a.activity.icon || '')} ${esc(a.activity.phrase)} · ` : ''}<code>${esc(a.doing.label)}</code></p>` : '';
  const sh = sheet(`<h2>${cdotT(a)}${esc(a.home ? HOME_NAME() : a.title || a.pane)}</h2>${raw}<div class="acts">${rows.map(([k, l]) => `<button class="act${k === 'close' ? ' danger' : ''}" data-a="${k}">${esc(l)}</button>`).join('')}</div>`);
  sh.addEventListener('click', e => { const b = e.target.closest('[data-a]'); if (!b) return; sh.close(); setTimeout(() => cardAct(a, b.dataset.a), b.dataset.a === 'rename' ? 0 : 120); });
}
const cdotT = a => cdot(a.color);
const COLORS_CLS = c => ['red', 'orange', 'yellow', 'green', 'cyan', 'blue', 'purple', 'pink'].includes(c) ? ' col-' + c : '';

// ---------- background job output (F11) ----------
export function jobSheet(j) {
  const sh = sheet(`<h2 class="jt">${esc(j.desc || j.cmd)}</h2>
    <div class="dim small jm"></div>
    ${j.desc ? `<pre class="jcmd">${esc(j.cmd)}</pre>` : ''}
    <pre class="jout"><span class="spin"></span></pre>
    <div class="btns"><a class="btn sm" href="${itemHref(j.tid, j.i, j.pane)}">↗ in chat</a>${j.out ? `<a class="btn sm" href="#/f?path=${encodeURIComponent(j.out)}">Open full log</a>` : ''}</div>`, 'tall');
  const out = $('.jout', sh);
  $('.jm', sh).innerHTML = `${esc(j.status)} · ${esc(whoName(j.agentTitle, j.pane) || '')} · started <span data-since="${j.started}" data-ago></span>`;
  tick(sh);
  let t = 0, alive = true;
  const poll = async () => {
    if (!alive || !j.out) { if (!j.out) out.textContent = '(no output file)'; return; }
    try {
      const r = await fetch('/api/file?path=' + encodeURIComponent(j.out) + '&tail=6000');
      const text = r.ok ? await r.text() : `HTTP ${r.status}`;
      const atB = out.scrollHeight - out.scrollTop - out.clientHeight < 40;
      out.textContent = text || '(empty)';
      if (atB) out.scrollTop = out.scrollHeight;
    } catch (e) { out.textContent = 'Couldn’t load: ' + e.message; }
    if (alive && j.status === 'running') t = setTimeout(poll, 2000);
  };
  poll();
  const close0 = sh.close; sh.close = () => { alive = false; clearTimeout(t); close0(); };
  sh.addEventListener('click', e => { if (e.target.closest('a')) sh.close(); });
}

// ---------- message long-press actions (F12) ----------
export function msgSheet(view, it, el) {
  const card = el.classList.contains('amsg'); // another agent's message (a steer with `from`, a report, a paste)
  const user = it.k === 'user' && !card, text = it.text || '';
  const plain = user ? text : (el.innerText || text).trim();
  const canCompose = !!view.target && !view.el.classList.contains('ro');
  const rows = [['copy', 'Copy'], !user && ['md', 'Copy as Markdown'], navigator.share && ['share', 'Share…'],
    !user && ['say', 'Read aloud'], canCompose && ['quote', 'Quote in reply'], ['fwd', 'Send to agent…'], user && canCompose && ['edit', 'Edit & resend'],
    canBranch(view, it, card) && ['branch', 'Branch from here…'], user && undoable(view, it) && ['undo', 'Undo last turn…']].filter(Boolean);
  const sh = sheet(`<div class="mprev">${esc(plain.slice(0, 220))}${plain.length > 220 ? '…' : ''}</div><div class="acts">${rows.map(([k, l]) => `<button class="act" data-a="${k}">${l}</button>`).join('')}</div>`);
  const ta = view.ta, put = v => { ta.value = v; ta.dispatchEvent(new Event('input')); ta.focus(); ta.setSelectionRange(ta.value.length, ta.value.length); };
  sh.addEventListener('click', e => {
    const b = e.target.closest('[data-a]'); if (!b) return;
    const a = b.dataset.a;
    if (a === 'fwd') { forwardSheet(view, text, sh, fwdSource(view, it, user, card)); return; }
    if (a === 'branch') { sh.close(); branchSheet(view, it, plain); return; }
    if (a === 'undo') { sh.close(); lazy('undo').then(m => m.undoSheet(view), e => toast('Load failed: ' + esc(e.message), 'err')); return; }
    sh.close();
    if (a === 'copy') copyText(plain);
    else if (a === 'md') copyText(text);
    else if (a === 'say') lazy('talk').then(m => m.speak(plain, 'Message'));
    else if (a === 'share') navigator.share({ text }).catch(() => { });
    else if (a === 'quote') put(text.split('\n').map(l => '> ' + l).join('\n') + '\n\n' + ta.value);
    else if (a === 'edit') put(text + (ta.value ? '\n' + ta.value : ''));
  });
}
// Undo last turn (undo.js): only on the chat's last prompt, in a live agent's own chat
const undoable = (view, it) => !!view.pane && view.target === view.pane && !!view.agent && !(view.fixedTid || '').includes('~')
  && it.i === [...view.items].reverse().find(x => x.k === 'user' && x.kind !== 'report')?.i;
// ---------- Branch from here (API.md "Branch") ----------
// An agent's own message in a main transcript (not a subagent's, not another agent's card) can start a branch: a new
// agent with this chat up to that message, then the user's reply. The original agent is left as it is.
const canBranch = (view, it, card) => !card && it.k === 'text' && !!view.tid && !view.tid.includes('~');
export function branchSheet(view, it, plain) {
  const who = view.home ? HOME_NAME() : whoName(view.agent?.title || view.agent?.name || view.zz?.name || '') || 'this agent';
  const q = plain.length > 280 ? '…' + plain.slice(-280) : plain;
  const sh = sheet(`<h2 class="br-h">${I.fork || ''}Branch from here</h2>
    <p class="br-sub">A new agent starts with this chat up to here, then your reply. ${esc(who)} stays as it is.</p>
    <blockquote class="br-q">${esc(q)}</blockquote>
    <form class="br-f"><textarea class="br-ta" rows="3" placeholder="Your reply in the branch" enterkeyhint="send" aria-label="Your reply"></textarea>
    <button class="btn primary br-go" disabled>Branch</button></form>`, 'brsheet');
  const ta = $('.br-ta', sh), go = $('.br-go', sh), f = $('.br-f', sh);
  const key = 'brdraft.' + view.tid + '.' + it.i;
  ta.value = ls.get(key, '') || ''; go.disabled = !ta.value.trim();
  ta.oninput = () => { go.disabled = !ta.value.trim(); ls.set(key, ta.value); ta.style.height = 'auto'; ta.style.height = Math.min(ta.scrollHeight, 220) + 'px'; };
  ta.onkeydown = e => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) f.requestSubmit(); };
  setTimeout(() => ta.focus(), 60);
  f.onsubmit = async e => {
    e.preventDefault();
    const text = ta.value.trim(); if (!text || go.disabled && go.dataset.busy) return;
    go.disabled = true; go.dataset.busy = 1; go.innerHTML = '<span class="spin"></span> Branching…';
    try {
      const r = await post(`t/${encodeURIComponent(view.tid)}/branch`, { i: it.i, text });
      ls.set(key, null); haptic(); sh.close();
      G.starting.set(r.pane, Date.now());
      lazy('find').then(m => m.agents?.(true)).catch(() => { }); // the branch joins Recent / Agents / search at once
      location.hash = '#/a/' + encodeURIComponent(r.pane);
      toast(`Branched · ${esc(r.title)}`, 'ok', null, 2200);
    } catch (err) {
      delete go.dataset.busy; go.disabled = false; go.textContent = 'Branch';
      toast('Couldn’t branch: ' + esc(err.message), 'err');
    }
  };
}
// Who a forwarded message is from: `pane` (when it is a live agent) lets the receiver see it as that agent's card.
function fwdSource(view, it, user, card) {
  if (card) {
    const p = it.kind === 'steer' ? steerAgent(it) : null, pane = p?.pane || (it.kind === 'steer' ? it.agentId : null) || null;
    return { pane, name: p ? (p.home ? HOME_NAME() : whoName(p.title || p.name || p.pane, p.pane)) : whoName(it.from) || (it.kind === 'report' ? 'a subagent' : 'another agent') };
  }
  if (user) return { pane: null, name: 'me' };
  const live = view.target && view.target === view.pane; // not a subagent's transcript
  return { pane: live ? view.pane : null, name: view.home ? HOME_NAME() : view.agent?.title || view.agent?.name || 'an agent' };
}
// Send to agent: one tap sends it (POST /steer, so a busy agent gets it mid-turn and an idle one as a prompt). Without
// a note it goes as the source agent's message (`from`: the receiver's chat shows that agent's card); with a note it
// is your message quoting it. "Edit first" keeps the old way: a draft in the receiver's composer.
function forwardSheet(view, text, prev, src) {
  const quote = `Forwarded from ${src.name}:\n` + text.split('\n').map(l => '> ' + l).join('\n');
  const list = sortAgents((G.st?.agents || []).filter(a => a.pane !== view.pane && a.pane !== src.pane && !a.service));
  const sh = sheet(`<h2>Send to agent</h2><input class="fw-note" type="text" placeholder="Add a note (optional)" autocomplete="off" enterkeyhint="done">
    <div class="swl">${list.map(a => {
    const s = dispStatus(a);
    return `<button class="swrow${COLORS_CLS(a.color)}" data-p="${esc(a.pane)}"><span class="pill s-${s}"><i></i>${LABEL[s]}</span><span class="sb"><b>${cdot(a.color)}${a.home ? HOME_NAME() : esc(a.title || a.name || a.pane)}</b><small>${esc(shortPath(a.cwd))}</small></span></button>`;
  }).join('')}</div><div class="fw-alt"><button class="btn sm fw-edit">Edit first…</button><button class="btn sm newag">${I.plus} New agent</button></div>`);
  prev.close();
  const note = sh.querySelector('.fw-note');
  let editing = false;
  sh.addEventListener('click', async e => {
    if (e.target.closest('.fw-edit')) { editing = !editing; e.target.closest('.fw-edit').classList.toggle('on', editing); toast(editing ? 'Pick the agent: it opens with a draft to edit' : 'Pick the agent to send', '', null, 1600); return; }
    if (e.target.closest('.newag')) { sh.close(); newAgentSheet((note.value.trim() ? note.value.trim() + '\n\n' : '') + quote + '\n\n'); return; }
    const r = e.target.closest('[data-p]'); if (!r) return;
    const a = list.find(x => x.pane === r.dataset.p), who = a.home ? HOME_NAME() : a.title || a.name || a.pane, n = note.value.trim();
    sh.close(); haptic();
    if (editing) {
      const cur = ls.get('draft.' + a.pane, '');
      ls.set('draft.' + a.pane, (n ? n + '\n\n' : '') + quote + '\n\n' + (cur || ''));
      location.hash = agentHref(a);
      toast('Draft ready — review and send', 'ok', null, 2200);
      return;
    }
    const body = !n && src.pane ? { text, from: src.pane } : { text: (n ? n + '\n\n' : '') + quote };
    try {
      await post(`a/${encodeURIComponent(a.pane)}/steer`, { ...body, nonce: Math.random().toString(36).slice(2) });
      toast(`Sent to ${esc(who)} · tap to open`, 'ok', () => { location.hash = agentHref(a); }, 2600);
    } catch (err) {
      toast(err.status === 409 ? `${esc(who)} is waiting on a question — answer it first` : 'Not sent: ' + esc(err.message), 'err');
    }
  });
}
