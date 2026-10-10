// Lazily loaded: what the Claude mod reports (modbridge) — ask_phone question card, context gauge sheet, Compact now.
import { $, esc, post, toast, haptic, sheet, HOME_NAME } from './app.js';

const enc = encodeURIComponent;

// ---------- ask_phone: the agent is waiting on a question asked on the phone ----------
// Options are one-tap answers; the text box sends a free-form reply. Both go to POST /api/a/:pane/answer.
export function renderAsk(view, box, ask) {
  const q = ask.questions?.[0] || {};
  box.hidden = false;
  box.innerHTML = `<div class="bl-q"><div class="bl-qh">Asked on your phone</div><div class="bl-qt">${esc(q.question || '')}</div></div>` +
    (q.options || []).map((o, i) => `<button class="opt" data-pa="${i}"><span class="on">${i + 1}</span><span class="ol">${esc(o.label)}</span></button>`).join('') +
    `<form class="bl-ans"><textarea rows="1" placeholder="${q.options?.length ? 'Or type a reply…' : 'Reply…'}" enterkeyhint="send"></textarea><button class="btn primary sm">Reply</button></form>`;
  const ta = $('textarea', box);
  ta.oninput = () => { ta.style.height = 'auto'; ta.style.height = Math.min(ta.scrollHeight, 120) + 'px'; };
  ta.onkeydown = e => { if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); answer(view, box, ta.value); } };
  $('.bl-ans', box).onsubmit = e => { e.preventDefault(); answer(view, box, ta.value); };
  box.querySelectorAll('[data-pa]').forEach(b => {
    b.onclick = () => answer(view, box, q.options[+b.dataset.pa].label, b);
  });
}

async function answer(view, box, text, btn) {
  text = String(text || '').trim();
  const ask = view.agent?.ask;
  if (!text || !ask?.phone) return;
  const all = [...box.querySelectorAll('button, textarea')];
  all.forEach(x => x.disabled = true); btn?.classList.add('busy'); haptic();
  try {
    await post(`a/${enc(view.pane)}/answer`, { text, id: ask.id });
    toast('Answered', 'ok', null, 1400);
  } catch (e) {
    all.forEach(x => x.disabled = false); btn?.classList.remove('busy');
    toast(e.status === 404 ? 'That question is no longer open.' : 'Not sent: ' + esc(e.message), 'err');
  }
}

// ---------- context gauge sheet + Compact now ----------
// `ctx.est`: no mod report for this session; the server estimated it from the transcript's newest response.
const RL = { five_hour: '5-hour limit', seven_day: 'Weekly limit' };
const kfmt = n => n == null ? '?' : n >= 1e6 ? (n / 1e6).toFixed(n % 1e6 ? 1 : 0) + 'M' : n >= 1e3 ? Math.round(n / 1e3) + 'k' : String(n);
const inMs = t => {
  const d = Date.parse(t) - Date.now(); if (!(d > 0)) return '';
  const m = Math.round(d / 60000), h = Math.floor(m / 60);
  return h >= 24 ? `${Math.floor(h / 24)}d ${h % 24}h` : h ? `${h}h ${m % 60}m` : `${Math.max(m, 1)}m`;
};
export const bar = (p, label, sub) => {
  const v = Math.max(0, Math.min(100, p ?? 0));
  return `<div class="cxr"><div class="cxl"><b>${esc(label)}</b><span>${p == null ? '—' : Math.round(p) + '%'}</span></div>
    <div class="cxbar${v >= 85 ? ' hi' : v >= 70 ? ' mid' : ''}"><i style="width:${v}%"></i></div>${sub ? `<div class="cxs">${sub}</div>` : ''}</div>`;
};

export function ctxSheet(a) {
  const c = a.ctx || {};
  const est = c.est ? ' · estimated from transcript' : '';
  const rows = [bar(c.pct, 'Context', c.compacting ? 'Compacting…' : c.pct == null ? 'Measured after the next response' : `${kfmt(c.tokens)} of ${kfmt(c.window)} tokens${est}`)]
    .concat((c.rateLimits || []).map(r => bar(r.pct, RL[r.kind] || r.kind.replace(/_/g, ' '), r.resetsAt && inMs(r.resetsAt) ? 'resets in ' + inMs(r.resetsAt) : '')));
  const sh = sheet(`<h2>${esc(a.home ? HOME_NAME() : a.title || a.pane)}</h2>
    ${c.costUsd != null ? `<p class="dim small">Session cost so far: <b>$${c.costUsd.toFixed(2)}</b>${c.est ? ' (main conversation, API prices; estimated from transcript)' : ''}</p>` : ''}
    <div class="cxl-list">${rows.join('')}</div>
    ${a.mod || a.status === 'idle' || a.status === 'done' ? `<textarea class="cxi" rows="2" placeholder="Optional: what the summary should keep"></textarea>
    <div class="btns col"><button class="btn primary go">Compact now</button></div>
    ${a.status === 'working' ? '<p class="dim small">Runs when the current turn ends.</p>' : ''}` : '<p class="dim small">Compacting needs the agent idle or the sheprd mod connected.</p>'}`, 'compact');
  const go = $('.go', sh);
  if (go) go.onclick = () => { sh.close(); compactNow(a, $('.cxi', sh)?.value); };
}

export async function compactNow(a, instructions) {
  haptic();
  try {
    const r = await post(`a/${enc(a.pane)}/compact`, instructions?.trim() ? { instructions: instructions.trim() } : {});
    toast(r.how === 'deferred' ? 'Will compact when this turn ends' : r.via === 'herdr' ? 'Sent /compact' : 'Compacting…', 'ok', null, 1800);
  } catch (e) {
    toast(e.status === 409 && e.message === 'busy' ? 'Busy — try when the turn ends' : 'Compact failed: ' + esc(e.message), 'err');
  }
}
