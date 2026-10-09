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
// Several questions go one at a time, like Claude's TUI: a step row (tap to jump, ✓ = answered), Back / Next,
// and Submit pinned at the bottom of the panel (the panel scrolls; see .blocked in style.css). An option's
// `preview` (ASCII mockup) stays folded behind a "Preview" chip; the picked option's preview opens, height-capped.
// "2", "c", "C)", "Option C" or an option's exact label → that option's index; anything else → -1.
// A letter goes to the option labelled with it ("C — rows in a box"), else to the n-th option when no label uses letters.
export function pickOpt(q, text) {
  const os = q?.options || [], t = String(text || '').trim().replace(/[.!)]+$/, '').replace(/^option\s+/i, '');
  if (!os.length || !t) return -1;
  if (/^\d{1,2}$/.test(t)) return +t >= 1 && +t <= os.length ? +t - 1 : -1;
  const lead = o => o.label.match(/^(?:option\s+)?([A-Za-z])(?:[).:\s\u2014\u2013-]|$)/i)?.[1]?.toUpperCase();
  if (/^[a-z]$/i.test(t)) {
    const L = t.toUpperCase(), j = os.findIndex(o => lead(o) === L);
    if (j >= 0) return j;
    const n = L.charCodeAt(0) - 65;
    return !os.some(lead) && n < os.length ? n : -1;
  }
  return os.findIndex(o => o.label.trim().toLowerCase() === t.toLowerCase());
}
// A draft of the answers per pane + question set (localStorage, 7 days): a failed Submit, a re-render or a reload
// re-opens the card with every pick and typed answer, and the same questions asked again start from them.
const DRAFT_MS = 7 * 864e5;
function draftKey(pane, qs) {
  const t = JSON.stringify([pane, qs.map(q => [q.question, (q.options || []).map(o => o.label), !!q.multiSelect])]);
  let x = 5381; for (let i = 0; i < t.length; i++) x = (x * 33 + t.charCodeAt(i)) | 0;
  return 'akd.' + (x >>> 0).toString(36);
}
function pruneDrafts() {
  try {
    for (let i = localStorage.length - 1; i >= 0; i--) {
      const k = localStorage.key(i);
      if (k?.startsWith('mm.akd.') && !(Date.now() - (ls.get(k.slice(3))?.ts || 0) < DRAFT_MS)) localStorage.removeItem(k);
    }
  } catch { }
}
// The driver's 409 reasons (server/src/talk.rs answer_ask) in words for the card.
export function askErr(e) {
  const m = String(e?.message || '');
  if (e?.status === 409 && /no_question|no longer open/.test(m)) return 'The question is no longer open in the terminal. Your answers are kept here.';
  if (e?.status === 409) return `Not sent: ${m}. Your answers are kept: tap Submit again, or use the Terminal.`;
  return `Not sent (${m || 'network error'}). Your answers are kept: tap Submit again.`;
}
export function renderAsk(view, box, ask) {
  const qs = ask.questions || [];
  const st = qs.map(() => ({ labels: [], text: '', pv: -1 }));
  const one = qs.length === 1 && !qs[0].multiSelect;
  const many = qs.length > 1;
  let cur = 0;
  const dk = draftKey(view.pane, qs), draft = ls.get(dk);
  let restored = false;
  if (draft && Array.isArray(draft.st) && draft.st.length === qs.length && Date.now() - (draft.ts || 0) < DRAFT_MS) {
    draft.st.forEach((d, k) => {
      const ok = (qs[k].options || []).map(o => o.label);
      st[k].labels = (d.labels || []).filter(l => ok.includes(l)).slice(0, qs[k].multiSelect ? 99 : 1);
      st[k].text = String(d.text || '');
      st[k].pv = Number.isInteger(d.pv) ? d.pv : -1;
      if (st[k].labels.length || st[k].text.trim()) restored = true;
    });
    cur = Math.min(Math.max(0, draft.cur | 0), qs.length - 1);
  }
  const saveDraft = () => {
    const any = st.some(x => x.labels.length || x.text.trim());
    ls.set(dk, any ? { ts: Date.now(), cur, st } : null);
  };
  box.hidden = false;
  box.innerHTML = (many ? `<div class="ak-steps" role="tablist">${qs.map((q, k) => `<button class="ak-st" data-st="${k}" role="tab"><b>${k + 1}</b>${esc(q.header || 'Question')}</button>`).join('')}</div>` : '') +
    qs.map((q, k) => `<div class="ak-q" data-k="${k}"${k !== cur ? ' hidden' : ''}>
      <div class="bl-q">${q.header || many ? `<div class="bl-qh">${esc(q.header || 'Question')}${many ? ` · ${k + 1}/${qs.length}` : ''}${q.multiSelect ? ' · pick any' : ''}</div>` : ''}<div class="bl-qt">${esc(q.question || '')}</div></div>
      ${(q.options || []).map((o, i) => `<button class="opt${q.multiSelect ? ' ak-m' : ''}${one ? ' ak-1' : ''}" data-ao="${i}"><span class="on">${q.multiSelect ? '' : i + 1}</span><span class="ol">${esc(o.label)}${o.description ? `<small>${esc(o.description)}</small>` : ''}</span>${o.preview ? '<span class="ak-pvb" aria-label="Show preview">Preview</span>' : ''}</button>${o.preview ? `<pre class="ak-pv" data-pv="${i}" hidden>${esc(o.preview)}</pre>` : ''}`).join('')}
      <textarea class="ak-t" rows="1" data-k="${k}" placeholder="${q.options?.length ? 'Or type an answer…' : 'Answer…'}" enterkeyhint="${one ? 'send' : k < qs.length - 1 ? 'next' : 'done'}"></textarea>
    </div>`).join('') +
    `<div class="ak-err" role="alert" hidden></div>` +
    (one ? '' : `<div class="ak-nav">${many ? '<button class="btn ak-back">Back</button><button class="btn ak-next">Next</button>' : ''}<button class="btn primary ak-go" disabled>Submit answers</button></div>`);
  const go = $('.ak-go', box), back = $('.ak-back', box), next = $('.ak-next', box), err = $('.ak-err', box);
  box.querySelectorAll('.ak-t').forEach(ta => { ta.value = st[+ta.dataset.k].text; });
  const showErr = m => { err.textContent = m || ''; err.hidden = !m; };
  const done = k => !!(st[k].labels.length || st[k].text.trim());
  const ready = () => st.every((x, k) => done(k));
  const upd = () => {
    box.querySelectorAll('.ak-q').forEach((el, k) => {
      el.hidden = k !== cur;
      el.querySelectorAll('[data-ao]').forEach(b => b.classList.toggle('sel', st[k].labels.includes(qs[k].options[+b.dataset.ao].label)));
      el.querySelectorAll('.ak-pv').forEach(p => { p.hidden = +p.dataset.pv !== st[k].pv; p.previousElementSibling.classList.toggle('pvo', !p.hidden); });
    });
    box.querySelectorAll('.ak-st').forEach((b, k) => { b.classList.toggle('cur', k === cur); b.classList.toggle('done', done(k)); b.setAttribute('aria-selected', k === cur); });
    if (go) {
      go.disabled = !ready();
      const last = cur === qs.length - 1;
      go.hidden = many && !last && !ready(); // the last step, or everything answered (edit, then submit from anywhere)
      if (back) back.hidden = cur === 0;
      if (next) { next.hidden = last; next.classList.toggle('primary', go.hidden); }
    }
    saveDraft();
  };
  // the keyboard goes down when the card moves on: a focused textarea of a hidden step kept the panel's
  // keyboard layout, and the next tap landed on the shifted layout instead of Next (2026-10-07)
  const unfocus = () => { const a = document.activeElement; if (a && box.contains(a) && a.matches('textarea')) a.blur(); };
  let autoT = 0;
  const step = k => {
    if (k < 0 || k >= qs.length || k === cur) return;
    unfocus();
    cur = k; upd();
    const sc = box.closest('.blocked'); if (sc) sc.scrollTop = 0; // the step row and the new question's top
    box.querySelector('.ak-st.cur')?.scrollIntoView?.({ block: 'nearest', inline: 'nearest' });
  };
  const send = async btn => {
    unfocus(); showErr('');
    saveDraft();
    const all = [...box.querySelectorAll('button, textarea')]; all.forEach(x => x.disabled = true); btn?.classList.add('busy'); haptic();
    try {
      await post(`a/${enc(view.pane)}/askanswer`, { answers: st.map(x => ({ labels: x.labels, text: x.text.trim() || undefined })) });
      ls.set(dk, null);
      toast('Answered', 'ok', null, 1400);
    } catch (e) {
      // the card stays as it was (every pick and typed answer, the draft kept) with the reason in it
      all.forEach(x => x.disabled = false); btn?.classList.remove('busy'); upd();
      showErr(askErr(e));
      err.scrollIntoView?.({ block: 'nearest' });
      toast('Not sent: your answers are kept', 'err');
    }
  };
  let adv = null;
  box.querySelectorAll('.ak-q').forEach((el, k) => {
    const q = qs[k], ta = $('.ak-t', el);
    el.querySelectorAll('[data-ao]').forEach(b => {
      b.onclick = e => {
        const i = +b.dataset.ao, l = q.options[i].label;
        if (e.target.closest('.ak-pvb')) { st[k].pv = st[k].pv === i ? -1 : i; upd(); return; } // peek without choosing
        autoT = 0;
        if (!q.multiSelect) unfocus();
        if (q.multiSelect) st[k].labels = st[k].labels.includes(l) ? st[k].labels.filter(x => x !== l) : [...st[k].labels, l];
        else { st[k].labels = [l]; st[k].text = ''; ta.value = ''; }
        if (q.options[i].preview && (!q.multiSelect || st[k].labels.includes(l))) st[k].pv = i;
        else if (st[k].pv === i) st[k].pv = -1; // unticked: fold its preview again
        upd();
        if (one) send(b);
        // single-select: on to the next unanswered question (like Enter in the TUI); stay when there are previews to compare
        else if (many && !q.multiSelect && !q.options.some(o => o.preview)) {
          clearTimeout(adv);
          adv = setTimeout(() => { const n = st.findIndex((x, j) => j > k && !done(j)); if (cur === k && n > 0) { step(n); autoT = Date.now(); } }, 280);
        }
      };
    });
    ta.oninput = () => {
      ta.style.height = 'auto'; ta.style.height = Math.min(ta.scrollHeight, 120) + 'px';
      st[k].text = ta.value; autoT = 0;
      if (!q.multiSelect && ta.value.trim()) st[k].labels = [];
      upd();
    };
    // the keyboard is up: keep the field in view inside the (now shorter) panel
    ta.onfocus = () => setTimeout(() => ($('.ak-nav', box) || ta).scrollIntoView({ block: 'nearest' }), 350);
    ta.onkeydown = e => {
      if (e.key !== 'Enter' || e.shiftKey || e.isComposing) return;
      if (one && ta.value.trim()) { e.preventDefault(); send(); }
      else if (many && k < qs.length - 1) { e.preventDefault(); step(k + 1); }
    };
  });
  // A reply typed in the chat's composer while this card is up answers the card instead of racing it (2026-10-06: a
  // typed "C" reached the terminal, the card's tap then got a 409 and the agent had to ask again). An option by its
  // number, letter or label is picked; any other text becomes the current question's typed answer. One plain
  // question is sent at once; otherwise the card waits for Submit.
  box.akType = text => {
    const k = cur, q = qs[k], el = box.querySelector(`.ak-q[data-k="${k}"]`), i = pickOpt(q, text);
    if (i >= 0) el.querySelector(`[data-ao="${i}"]`).click();
    else { const ta = $('.ak-t', el); ta.value = text; ta.oninput(); if (one) send(); }
    if (!one) toast(i >= 0 ? `Picked “${esc(q.options[i].label)}” — Submit when ready` : 'Added as your answer — Submit when ready', 'ok', null, 2200);
    return true;
  };
  // Back / Next / Submit / the step row act on the tap itself: pointerdown keeps the focus (no keyboard-close
  // reflow) and the lift fires the button even when the layout moved under the finger meanwhile (the keyboard,
  // the field scrolling into view): a click needs press and release on the same spot of the page, and taps on
  // Next while typing were lost that way (2026-10-07). A Next right after a pick moved on by itself is that same
  // intent, not a skip.
  const tapAct = (b, fn) => {
    let fired = 0;
    b.addEventListener('pointerdown', e => {
      if (e.button) return;
      e.preventDefault();
      const x = e.clientX, y = e.clientY, t = Date.now(), id = e.pointerId;
      const end = () => { document.removeEventListener('pointerup', up, true); document.removeEventListener('pointercancel', cancel, true); };
      const cancel = c => { if (c.pointerId === id) end(); }; // a scroll that started on the button is no tap
      const up = u => {
        if (u.pointerId !== id) return;
        end();
        if (Math.hypot(u.clientX - x, u.clientY - y) < 14 && Date.now() - t < 900 && !b.disabled && !b.hidden) { fired = Date.now(); fn(); }
      };
      document.addEventListener('pointerup', up, true); document.addEventListener('pointercancel', cancel, true);
    });
    b.onclick = () => { if (Date.now() - fired > 900) fn(); }; // keyboard / assistive activation
  };
  box.querySelectorAll('.ak-st').forEach(b => tapAct(b, () => step(+b.dataset.st)));
  if (back) tapAct(back, () => { clearTimeout(adv); step(cur - 1); });
  if (next) tapAct(next, () => { const skip = Date.now() - autoT < 700; autoT = 0; if (skip) return; clearTimeout(adv); step(cur + 1); });
  if (go) tapAct(go, () => send(go));
  pruneDrafts();
  upd();
  if (restored && many) toast('Restored your unsent answers', 'ok', null, 1800);
}
