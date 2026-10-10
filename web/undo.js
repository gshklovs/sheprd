// Lazy: Undo last turn (API.md "Undo last turn", server/src/undo.rs). From a live agent's chat (the long-press menu
// on the user's last message, or the chat's ⋯ menu): a sheet says what goes back (the message, each file the turn edited
// and whether Claude's checkpoint can put it back) and what doesn't (shell commands, other tools, other changes git
// sees, commits). Nothing happens before "Undo turn". After it the chat reloads at the earlier state and the message
// is back in the composer, in front of any draft (never over it).
import { G, $, esc, api, post, ls, sheet, toast, haptic, whoName, HOME_NAME } from './app.js';

await new Promise(r => {
  if (document.querySelector('link[data-undo]')) return r();
  const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'undo.css'; l.dataset.undo = 1;
  l.onload = l.onerror = r; document.head.append(l); setTimeout(r, 3000);
});

const enc = encodeURIComponent;
const ICON = '<svg viewBox="0 0 24 24"><path d="M9 14L4 9l5-5"/><path d="M4 9h10.5a5.5 5.5 0 0 1 0 11H11"/></svg>';
const WHY = {
  working: 'It’s working. Undo opens up when the turn ends (or stop it first).',
  blocked: 'It’s waiting on a question or a permission. Answer it first.',
  held: 'A message to it is still waiting to go in.',
  draft: 'Its input box on the PC holds an unsent draft. Send or clear it there first.',
  menu: 'A menu is open in its terminal.',
  unreadable: 'Its terminal can’t be read right now.',
  moved: 'This conversation moved to another session.',
  busy: 'An undo is already running.',
};
const ACT = { restore: 'goes back', delete: 'deleted (the turn made it)', recreate: 'comes back (it was deleted)', missing: 'can’t be put back', same: 'unchanged since' };

/** The chat's last message from the user (or another agent's prompt): only that one offers Undo. */
export function lastPromptI(view) {
  for (let k = view.items.length - 1; k >= 0; k--) { const x = view.items[k]; if (x.k === 'user' && x.kind !== 'report') return x.i; }
  return null;
}
/** A live agent's own chat (not a subagent's, not an asleep transcript). */
export const canUndo = view => !!view.pane && view.target === view.pane && !!view.agent && !(view.fixedTid || '').includes('~');

export function undoSheet(view) {
  const a = view.agent, pane = view.pane, who = view.home || a?.home ? HOME_NAME() : whoName(a?.title || a?.name || pane, pane);
  const sh = sheet(`<h2 class="ud-h">${ICON}Undo last turn</h2><div class="ud-b"><p class="dim"><span class="spin"></span> Looking at the last turn…</p></div>`, 'udsheet');
  const body = $('.ud-b', sh);
  let p = null, alive = true, poll = 0;
  const close0 = sh.close; sh.close = () => { alive = false; clearTimeout(poll); close0(); };
  const pend = () => !!view.el.querySelector('.pend')?.children.length; // a send still in its 3 s undo window

  const draw = () => {
    const files = p.files || [], doable = files.filter(f => f.act !== 'same' && f.act !== 'missing');
    const blocked = p.blocked || (pend() ? 'held' : null);
    const lines = [];
    if (p.commands?.length) lines.push(`<li><b>Not undone:</b> ${p.commands.length} shell command${p.commands.length > 1 ? 's' : ''}. What they changed stays.${p.commands.slice(0, 3).map(c => `<code>${esc(c)}</code>`).join('')}</li>`);
    if (p.other?.length) lines.push(`<li><b>Already happened:</b> ${p.other.map(esc).join(', ')}.</li>`);
    if (p.unchecked?.length) lines.push(`<li><b>No checkpoint, stays edited:</b> ${p.unchecked.map(esc).join(', ')}.</li>`);
    const later = files.filter(f => f.later && f.act !== 'same');
    if (later.length) lines.push(`<li><b>Changed again since the turn:</b> ${later.map(f => esc(f.path)).join(', ')}. Putting files back loses those changes too.</li>`);
    const outside = files.filter(f => !f.inTurn && f.act !== 'same');
    if (outside.length) lines.push(`<li><b>Also goes back</b> (changed since your message, not by this turn’s edits): ${outside.map(f => esc(f.path)).join(', ')}.</li>`);
    if (p.git?.otherN) lines.push(`<li><b>Git also shows</b> ${p.git.otherN} other changed file${p.git.otherN > 1 ? 's' : ''}, left as they are: ${p.git.other.map(esc).join(', ')}${p.git.otherN > p.git.other.length ? '…' : ''}</li>`);
    if (p.git?.commits?.length) lines.push(`<li><b>${p.git.commits.length} commit${p.git.commits.length > 1 ? 's' : ''} since your message stay${p.git.commits.length > 1 ? '' : 's'}</b> in git; files go back only in the working tree.</li>`);
    if (p.images) lines.push(`<li>The picture${p.images > 1 ? 's' : ''} in your message ${p.images > 1 ? 'aren’t' : 'isn’t'} put back.</li>`);
    lines.push(`<li>${esc(who)} restarts in the same tab, about 5 seconds.${a?.home ? ' Messages to her meanwhile wait.' : ''}</li>`);
    const q = p.text.length > 400 ? p.text.slice(0, 400) + '…' : p.text;
    body.innerHTML = `<p class="br-sub"><b>${esc(who)}</b> goes back to before this message. It comes back in your message box to change and send again.</p>
      <blockquote class="br-q ud-q">${esc(q) || '<i>(no text)</i>'}</blockquote>${p.also?.length ? `<p class="ud-also">and ${p.also.length} more message${p.also.length > 1 ? 's' : ''} you sent during the turn</p>` : ''}
      <h3 class="ud-t">Files</h3>
      ${files.length ? `<div class="ud-fl">${files.map(f => `<div class="ud-f${f.act === 'missing' ? ' bad' : ''}"><code>${esc(f.path)}</code><span>${ACT[f.act] || esc(f.act)}</span></div>`).join('')}</div>`
        : `<p class="ud-none">${blocked === 'working' ? 'Still running: the files show once the turn ends.' : p.checkpoint ? 'This turn didn’t edit files.' : 'No Claude checkpoint for this turn: no files go back.'}</p>`}
      ${doable.length ? `<label class="ud-tog"><input type="checkbox" class="ud-files" checked> Put ${doable.length === 1 ? 'this file' : `these ${doable.length} files`} back</label>` : ''}
      <ul class="ud-w">${lines.join('')}</ul>
      ${blocked ? `<p class="ud-why">${esc(WHY[blocked] || blocked)}</p>` : ''}
      <div class="ud-btns"><button class="btn ud-no">Cancel</button><button class="btn danger ud-go"${blocked ? ' disabled' : ''}>Undo turn</button></div>`;
    $('.ud-no', body).onclick = () => sh.close();
    $('.ud-go', body).onclick = go;
    clearTimeout(poll);
    if (blocked && ['working', 'blocked', 'held', 'draft', 'menu', 'busy'].includes(blocked)) poll = setTimeout(load, 2000); // opens up when it can
  };

  const load = async () => {
    try {
      const r = await api(`a/${enc(pane)}/undo`); if (!alive) return;
      const keep = $('.ud-files', body)?.checked; p = r; draw();
      if (keep === false && $('.ud-files', body)) $('.ud-files', body).checked = false; // a re-check keeps the user's choice
    }
    catch (e) { if (alive) body.innerHTML = `<p class="ud-why">Can’t undo: ${esc(e.message)}</p>`; }
  };

  async function go() {
    const b = $('.ud-go', body); if (b.disabled) return;
    const files = $('.ud-files', body)?.checked ?? false;
    clearTimeout(poll);
    b.disabled = true; $('.ud-no', body).disabled = true; b.innerHTML = '<span class="spin"></span> Undoing…';
    try {
      (G.undoing ||= new Map()).set(pane, Date.now() + 90000); // the chat stays while Claude restarts (app.js onState)
      if (view.home) G.homePending = pane; // the home agent's Home stays its chat, never "… isn't running" (cleared once it's back)
      const r = await post(`a/${enc(pane)}/undo`, { uuid: p.uuid, files });
      G.undoing.set(pane, Date.now() + 20000);
      haptic(); sh.close();
      putBack(view, r.text || '');
      if (r.fresh) startOver(view, r.tid); else reload(view, r.tid);
      const nf = files ? (p.files || []).filter(f => f.act !== 'same' && f.act !== 'missing').length : 0;
      toast(`Undone${r.fresh ? ' · a fresh start' : ''}${nf ? ` · ${nf} file${nf > 1 ? 's' : ''} back` : ''}${r.restarted ? '' : ' · it didn’t restart: look at its terminal'}`, r.restarted ? 'ok' : 'err', null, 2600);
    } catch (e) {
      G.undoing?.set(pane, Date.now() + 15000);
      toast('Couldn’t undo: ' + esc(e.message), 'err', null, 5000);
      load();
    }
  }
  load();
}

/** The undone message goes back in the composer, in front of whatever draft is there (never over it). */
function putBack(view, text) {
  if (!text) return;
  const ta = view.ta, cur = ta.value;
  ta.value = cur.trim() ? text + '\n\n' + cur : text;
  ta.dispatchEvent(new Event('input'));
  try { ls.set('draft.' + view.dkey, ta.value); } catch { }
  ta.setSelectionRange?.(ta.value.length, ta.value.length);
}

/** The first prompt was undone: nothing is left to resume, Claude started a fresh session in the pane. The chat
 *  shows empty until the new session appears in the state (onState loads its tid then). */
function startOver(view, tid) {
  tid ||= view.tid;
  if (tid) { G.tc?.delete(tid); ls.set('t.' + tid, null); }
  view.closeStream?.(); view.reset(); view.emptyState?.(true);
}

/** The transcript was cut: drop the cached copy and load it again. */
function reload(view, tid) {
  tid ||= view.tid; if (!tid) return;
  G.tc?.delete(tid); ls.set('t.' + tid, null);
  view.tid = null; view.load(tid);
}
