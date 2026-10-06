// Talking to agents (lazy, server/src/talk.rs): take back a queued message, TL;DR pinned above a long
// answer, "Catch me up" digest card, permission prompts / auto-mode denials answered here, read aloud.
import { G, $, h, esc, post, ls, toast, haptic, md } from './app.js';

const enc = encodeURIComponent;
const norm = s => String(s || '').replace(/\s+/g, ' ').trim();

let cssOn = false;
function css() {
  if (cssOn) return; cssOn = true;
  const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'talk.css'; document.head.append(l);
}
css();

const PLAY = '<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z" fill="currentColor"/></svg>';
const PAUSE = '<svg viewBox="0 0 24 24"><path d="M6 5h4v14H6zM14 5h4v14h-4z" fill="currentColor"/></svg>';
const SPK = '<svg viewBox="0 0 24 24"><path d="M4 9v6h4l5 4V5L8 9H4z" fill="currentColor"/><path d="M16 8.5a5 5 0 0 1 0 7M18.5 6a8.5 8.5 0 0 1 0 12" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/></svg>';

// ---------- per-chat state hook (called from ChatView.onState) ----------
export function chatState(view, a) {
  if (!view.pane || !a) return;
  if (view.talkSeen0 === undefined) view.talkSeen0 = G.seenI[view.tid || a.tid] ?? null;
  const t = a.talk || {};
  // TL;DR: remember it per transcript item, pin it above that answer
  if (t.tldr?.text && t.tldr.i != null && a.tid) {
    const k = 'tldr.' + a.tid, m = ls.get(k, {});
    if (m[t.tldr.i] !== t.tldr.text) {
      m[t.tldr.i] = t.tldr.text;
      const keys = Object.keys(m).map(Number).sort((x, y) => x - y);
      keys.slice(0, Math.max(0, keys.length - 30)).forEach(x => delete m[x]);
      ls.set(k, m);
    }
  }
  placeTldrs(view);
  if (!view.talkObs) {
    view.talkObs = new MutationObserver(() => { if (!view.talkRaf) view.talkRaf = requestAnimationFrame(() => { view.talkRaf = 0; placeTldrs(view); }); });
    view.talkObs.observe(view.list, { childList: true });
  }
  renderCards(view, a);
}

function placeTldrs(view) {
  const tid = view.tid || view.agent?.tid; if (!tid) return;
  const m = ls.get('tldr.' + tid, {});
  for (const [i, text] of Object.entries(m)) {
    const el = view.list.querySelector(`.msg.asst[data-i="${i}"]`);
    if (!el) continue;
    const prev = el.previousElementSibling;
    if (prev?.classList.contains('tldr') && prev.dataset.t === text) continue;
    if (prev?.classList.contains('tldr')) prev.remove();
    const card = h(`<div class="tldr" data-i="${esc(i)}"><div class="tl-h"><b>TL;DR</b><span class="sp"></span>
      <button class="tl-say" aria-label="Read the TL;DR aloud">${SPK}</button><button class="tl-all" aria-label="Read the whole answer aloud">${PLAY}<span>answer</span></button></div>
      <div class="tl-t"></div></div>`);
    card.dataset.t = text;
    $('.tl-t', card).textContent = text;
    $('.tl-say', card).onclick = e => { e.stopPropagation(); speak(text, 'TL;DR'); };
    $('.tl-all', card).onclick = e => { e.stopPropagation(); speak(el.innerText || '', 'Answer'); };
    el.before(card);
  }
}

// ---------- cards at the end of the chat: catch-up digest, auto-mode denials ----------
function renderCards(view, a) {
  const t = a.talk || {};
  const dg = t.digest && ls.get('dg.gone', null) !== t.digest.id ? t.digest : null;
  const denied = (t.denied || []).filter(d => !view.talkHidden?.has(d.id));
  const sig = JSON.stringify([dg, denied]);
  if (sig === view.talkSig) return;
  view.talkSig = sig;
  let box = $('.tkcards', view.el);
  if (!box) { box = h('<div class="tkcards"></div>'); view.pend.after(box); }
  box.innerHTML = (dg ? `<div class="tk-card dg ${dg.state}" data-id="${esc(dg.id)}">
      <div class="tk-h"><b>Catch-up</b>${dg.since ? `<span class="dim"> since ${hhmm(dg.since)}</span>` : ''}<span class="sp"></span>
        ${dg.state === 'done' ? `<button class="tk-say" aria-label="Read aloud">${SPK}</button>` : ''}<button class="tk-x" aria-label="Dismiss">×</button></div>
      <div class="tk-b">${dg.state === 'pending' ? '<span class="spin"></span> Asking the agent…' : dg.state === 'done' ? md(dg.text, { imgs: new Set() }) : esc(dg.text || 'No answer.')}</div></div>` : '')
    + denied.map(d => `<div class="tk-card den" data-id="${esc(d.id)}">
      <div class="tk-h"><b>Auto mode blocked ${esc(d.tool)}</b><span class="sp"></span><button class="tk-x" aria-label="Dismiss">×</button></div>
      <pre class="tk-cmd">${esc(d.summary || '')}</pre>${d.reason ? `<div class="tk-why dim small">${esc(d.reason)}</div>` : ''}
      <div class="btns"><button class="btn sm primary tk-allow">Allow once</button></div></div>`).join('');
  box.onclick = async e => {
    const card = e.target.closest('.tk-card'); if (!card) return;
    const id = card.dataset.id;
    if (e.target.closest('.tk-say')) { speak($('.tk-b', card).innerText, 'Catch-up'); return; }
    if (card.classList.contains('dg')) { if (e.target.closest('.tk-x')) { ls.set('dg.gone', id); view.talkSig = null; renderCards(view, view.agent || a); } return; }
    const allow = e.target.closest('.tk-allow');
    if (!allow && !e.target.closest('.tk-x')) return;
    haptic();
    (view.talkHidden ||= new Set()).add(id); view.talkSig = null; renderCards(view, view.agent || a);
    try {
      await post(`a/${enc(view.pane)}/perm`, { id, decision: allow ? 'allow' : 'dismiss' });
      if (allow) toast('Allowed once — the agent will retry it', 'ok', null, 1800);
    } catch (err) {
      view.talkHidden.delete(id); view.talkSig = null; renderCards(view, view.agent || a);
      toast(err.status === 409 ? 'The agent’s mod is not connected' : 'Failed: ' + esc(err.message), 'err');
    }
  };
}
const hhmm = t => new Date(t).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });

// ---------- "Catch me up" ----------
export async function catchUp(view, seen) {
  const a = view.agent; if (!a) return;
  seen ??= view.talkSeen0 ?? G.seenI[view.tid];
  const it = seen != null ? [...view.items].reverse().find(x => x.i <= seen && x.ts) : null;
  const last = seen != null ? [...view.items].reverse().find(x => x.i <= seen && (x.k === 'text' || x.k === 'user') && x.text) : null;
  haptic();
  ls.set('dg.gone', null);
  try {
    await post(`a/${enc(view.pane)}/catchup`, { since: it?.ts || undefined, after: last ? norm(last.text).slice(0, 200) : undefined });
    view.talkSig = null;
    requestAnimationFrame(() => view.toBottom?.(true));
  } catch (e) {
    toast(e.status === 409 ? 'Catch-up needs the agent’s sheprd mod (start a new session)' : 'Failed: ' + esc(e.message), 'err');
  }
}

// ---------- take back a queued message ----------
export async function unsend(view, q) {
  if (!q?.text) return;
  haptic();
  try {
    const r = await post(`a/${enc(view.pane)}/unsend`, q.id ? { id: q.id, text: q.text } : { text: q.text });
    const text = r.text || q.text;
    // drop the matching "Sending…" bubble and put the words back in the composer
    const k = view.pending.findIndex(p => p.text === norm(text));
    if (k >= 0) { view.pending[k].el.remove(); view.pending.splice(k, 1); }
    const ta = view.ta;
    ta.value = text + (ta.value.trim() ? '\n' + ta.value : '');
    ta.dispatchEvent(new Event('input'));
    toast('Taken back — edit & resend', 'ok', null, 1800);
  } catch (e) {
    const m = e.message;
    toast(m === 'already_sent' ? 'Too late — the agent already has it' : m === 'desk_draft' ? 'Someone is typing at the desk — not touching the input box'
      : m === 'no_mod' ? 'The agent’s mod is not connected' : 'Could not take it back: ' + esc(m), 'err');
  }
}

// ---------- permission prompt (the agent waits; the terminal dialog is open too) ----------
export function renderPerm(view, box, p) {
  box.hidden = false;
  box.innerHTML = `<div class="bl-q"><div class="bl-qh">Permission</div><div class="bl-qt">Allow <b>${esc(p.tool)}</b>?</div>
      <pre class="tk-cmd">${esc(p.summary || '')}</pre></div>
    <button class="opt" data-pd="allow"><span class="on">1</span><span class="ol">Allow</span></button>
    ${p.always ? '<button class="opt" data-pd="always"><span class="on">2</span><span class="ol">Allow always<small>adds the rule Claude suggests</small></span></button>' : ''}
    <button class="opt" data-pd="deny"><span class="on">${p.always ? 3 : 2}</span><span class="ol">Deny</span></button>`;
  box.querySelectorAll('[data-pd]').forEach(b => {
    b.onclick = async () => {
      const all = [...box.querySelectorAll('button')]; all.forEach(x => x.disabled = true); b.classList.add('busy'); haptic();
      try {
        await post(`a/${enc(view.pane)}/perm`, { id: p.id, decision: b.dataset.pd });
        toast(b.dataset.pd === 'deny' ? 'Denied' : 'Allowed', 'ok', null, 1400);
      } catch (e) {
        all.forEach(x => x.disabled = false); b.classList.remove('busy');
        toast(e.status === 404 ? 'Already answered' : 'Failed: ' + esc(e.message), 'err');
      }
    };
  });
}

// ---------- read aloud (speechSynthesis) ----------
let player = null;
const say = { chunks: [], k: 0, paused: false, on: false };

function plain(text) {
  return String(text || '')
    .replace(/```[\s\S]*?```/g, ' (code block) ')
    .replace(/`([^`]+)`/g, '$1')
    .replace(/!\[[^\]]*\]\([^)]*\)/g, '')
    .replace(/\[([^\]]+)\]\([^)]*\)/g, '$1')
    .replace(/https?:\/\/\S+/g, 'a link')
    .replace(/^\s{0,3}#{1,6}\s*/gm, '')
    .replace(/^\s*[-*+]\s+/gm, '')
    .replace(/^\s*\|.*\|\s*$/gm, ' ')
    .replace(/[*_~>#|]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
}

// Sentences packed into ≤ 220-char pieces (long utterances stall on iOS).
export function chunks(text) {
  const out = []; let cur = '';
  for (const s of plain(text).match(/[^.!?;:]+[.!?;:]*\s*/g) || []) {
    if ((cur + s).length > 220 && cur) { out.push(cur.trim()); cur = ''; }
    if (s.length > 220) { for (let k = 0; k < s.length; k += 220) out.push(s.slice(k, k + 220)); continue; }
    cur += s;
  }
  if (cur.trim()) out.push(cur.trim());
  return out;
}

export function speak(text, label = 'Reading') {
  const ss = window.speechSynthesis;
  if (!ss) { toast('Read-aloud is not available here', 'err'); return; }
  ss.cancel();
  say.chunks = chunks(text); say.k = 0; say.paused = false; say.on = true;
  if (!say.chunks.length) return;
  showPlayer(label);
  next();
}

function next() {
  const ss = window.speechSynthesis;
  if (!say.on || say.k >= say.chunks.length) { stop(); return; }
  const u = new SpeechSynthesisUtterance(say.chunks[say.k]);
  u.rate = ls.get('sayRate', 1.05);
  u.onend = () => { if (!say.on) return; say.k++; next(); };
  u.onerror = () => { if (say.on && !say.paused) { say.k++; next(); } };
  ss.speak(u);
  upd();
}

function toggle() {
  const ss = window.speechSynthesis; if (!ss) return;
  if (say.paused) { say.paused = false; ss.resume(); if (!ss.speaking) next(); }
  else { say.paused = true; ss.pause(); }
  upd();
}

export function stop() {
  say.on = false; say.paused = false;
  window.speechSynthesis?.cancel();
  player?.remove(); player = null;
}

function showPlayer(label) {
  player?.remove();
  player = h(`<div class="sayer" role="region" aria-label="Read aloud"><button class="sy-pp" aria-label="Pause">${PAUSE}</button>
    <span class="sy-l"></span><span class="sy-n dim"></span><button class="sy-x" aria-label="Stop">×</button></div>`);
  $('.sy-l', player).textContent = label;
  $('.sy-pp', player).onclick = toggle;
  $('.sy-x', player).onclick = stop;
  document.body.append(player);
}

function upd() {
  if (!player) return;
  const pp = $('.sy-pp', player);
  pp.innerHTML = say.paused ? PLAY : PAUSE; pp.setAttribute('aria-label', say.paused ? 'Play' : 'Pause');
  $('.sy-n', player).textContent = `${Math.min(say.k + 1, say.chunks.length)}/${say.chunks.length}`;
}

// ---------- AskUserQuestion, answered here in full ----------
// Every question with its options (one tap for a single single-select question), multiSelect ticks,
// and a free-text answer per question (Claude's "Type something"). POST /api/a/:pane/askanswer drives the dialog.
export function renderAsk(view, box, ask) {
  const qs = ask.questions || [];
  const st = qs.map(() => ({ labels: [], text: '' }));
  const one = qs.length === 1 && !qs[0].multiSelect;
  box.hidden = false;
  box.innerHTML = qs.map((q, k) => `<div class="ak-q" data-k="${k}">
      <div class="bl-q">${q.header ? `<div class="bl-qh">${esc(q.header)}${qs.length > 1 ? ` · ${k + 1}/${qs.length}` : ''}${q.multiSelect ? ' · pick any' : ''}</div>` : ''}<div class="bl-qt">${esc(q.question || '')}</div></div>
      ${(q.options || []).map((o, i) => `<button class="opt${q.multiSelect ? ' ak-m' : ''}" data-ao="${i}"><span class="on">${q.multiSelect ? '' : i + 1}</span><span class="ol">${esc(o.label)}${o.description ? `<small>${esc(o.description)}</small>` : ''}</span></button>`).join('')}
      <textarea class="ak-t" rows="1" placeholder="${q.options?.length ? 'Or type an answer…' : 'Answer…'}" enterkeyhint="${one ? 'send' : 'done'}"></textarea>
    </div>`).join('') + (one ? '' : '<button class="btn primary ak-go" disabled>Submit answers</button>');
  const go = $('.ak-go', box);
  const ready = () => st.every(x => x.labels.length || x.text.trim());
  const upd = () => {
    box.querySelectorAll('.ak-q').forEach((el, k) => el.querySelectorAll('[data-ao]').forEach(b => b.classList.toggle('sel', st[k].labels.includes(qs[k].options[+b.dataset.ao].label))));
    if (go) go.disabled = !ready();
  };
  const send = async btn => {
    const all = [...box.querySelectorAll('button, textarea')]; all.forEach(x => x.disabled = true); btn?.classList.add('busy'); haptic();
    try {
      await post(`a/${enc(view.pane)}/askanswer`, { answers: st.map(x => ({ labels: x.labels, text: x.text.trim() || undefined })) });
      toast('Answered', 'ok', null, 1400);
    } catch (e) {
      all.forEach(x => x.disabled = false); btn?.classList.remove('busy'); upd();
      toast(e.status === 409 ? 'Could not answer: ' + esc(e.message) + ' — try the Terminal' : 'Failed: ' + esc(e.message), 'err');
    }
  };
  box.querySelectorAll('.ak-q').forEach((el, k) => {
    const q = qs[k], ta = $('.ak-t', el);
    el.querySelectorAll('[data-ao]').forEach(b => {
      b.onclick = () => {
        const l = q.options[+b.dataset.ao].label;
        if (q.multiSelect) st[k].labels = st[k].labels.includes(l) ? st[k].labels.filter(x => x !== l) : [...st[k].labels, l];
        else { st[k].labels = [l]; st[k].text = ''; ta.value = ''; }
        upd();
        if (one) send(b);
      };
    });
    ta.oninput = () => {
      ta.style.height = 'auto'; ta.style.height = Math.min(ta.scrollHeight, 120) + 'px';
      st[k].text = ta.value;
      if (!q.multiSelect && ta.value.trim()) st[k].labels = [];
      upd();
    };
    ta.onkeydown = e => { if (one && e.key === 'Enter' && !e.shiftKey && !e.isComposing && ta.value.trim()) { e.preventDefault(); send(); } };
  });
  if (go) go.onclick = () => send(go);
  upd();
}
