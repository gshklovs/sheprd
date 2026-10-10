// Needs-you queue (lazy; was the triage deck, removed 2026-10-05 as "not that good compared to
// looking at a chat"). One list of open items across all agents — permission prompts, auto-mode denials,
// AskUserQuestion / ask_phone questions, plan approvals, generic menus, surfaces with replies and finished turns not
// yet seen — in one order (answer → denied → reply → done) with one count. Readers: the Home hero (home.js, which
// answers the top item in place through the same endpoints the chat uses), "Next up" and the Master strip. Opening an
// item always means its agent's chat at the item (hrefOf).
import { G, $, h, esc, md, api, post, ls, toast, haptic, tick, patchHTML, nav, agentHref, dispStatus, markSeen, DONE_REP, repLabel, dismissSurfs, takeMods, modSuffix, PMODS, asksYou, whoName, HOME_NAME } from './app.js';
const nmOf = a => a.home ? HOME_NAME() : whoName(a.title || a.name || a.pane, a.pane);

const enc = encodeURIComponent;
const SNOOZE = 15 * 60e3, HOLD = 30e3, SURF_AGE = 12 * 3600e3;
const rid = () => Math.random().toString(36).slice(2, 10) + Date.now().toString(36);
const one = s => String(s || '').replace(/\*\*|__|`+|^#+\s*/gm, '').replace(/\s+/g, ' ').trim();
const surfKey = s => s.tid && s.i != null ? `t:${s.tid}:${s.i}` : 's:' + s.id; // same key as app.js REPLIED
// quick-reply labels without a leading emoji / arrow ("↩️ continue" → "continue")
const bare = s => String(s || '').replace(/^[\p{Extended_Pictographic}\p{Emoji_Presentation}\u{FE0F}\u{20E3}\u2190-\u21FF\s]+/u, '').trim();
const asks = asksYou; // the agent's last paragraph asks something (app.js)


// ---------- local state ----------
let snz = ls.get('tri.snooze', {});            // item key → snoozed until (ms)
const held = new Map();                          // item key → acted at (hidden until the server state catches up)
const menus = new Map();                         // pane → {sig, list, q} screen menu (plans, generic prompts)
const answers = new Map();                       // `${tid}:${answerI}` → full final answer text (state only has a 240-char snippet)
const saveSnz = () => { const t = Date.now(); for (const k in snz) if (snz[k] < t) delete snz[k]; ls.set('tri.snooze', snz); };

// ---------- open items ----------
function itemsOf(st) {
  const out = [], t = Date.now(), agents = st?.agents || [], replied = ls.get('replied', {});
  for (const a of agents) {
    const nm = nmOf(a), tk = a.talk || {}, base = { a, nm, pane: a.pane };
    if (a.status === 'blocked') {
      const q = a.ask?.questions;
      if (tk.perm) out.push({ ...base, kind: 'perm', key: 'perm:' + tk.perm.id, rank: 0, ts: tk.perm.ts || a.since, p: tk.perm });
      else if (a.ask?.phone) out.push({ ...base, kind: 'phone', key: `phone:${a.pane}:${a.ask.id || a.ask.ts}`, rank: 0, ts: a.ask.ts || a.since });
      else if (a.plan) out.push({ ...base, kind: 'plan', key: `plan:${a.pane}:${a.plan.ts}`, rank: 0, ts: a.plan.ts || a.since });
      else if (q?.length) out.push({ ...base, kind: 'ask', key: `ask:${a.pane}:${a.ask.ts}`, rank: 0, ts: a.ask.ts || a.since });
      else out.push({ ...base, kind: 'menu', key: `menu:${a.pane}:${a.since}`, rank: 0, ts: a.since });
    }
    for (const d of tk.denied || []) out.push({ ...base, kind: 'denied', key: 'denied:' + d.id, rank: 1, ts: d.ts, p: d });
    if (dispStatus(a) === 'done') out.push({ ...base, kind: 'done', key: `done:${a.pane}:${a.completionSeq}`, rank: 3 + (a.muted ? 1 : 0), ts: a.since });
  }
  for (const s of st?.surfaces || []) {
    if (!s.replies?.length || replied[surfKey(s)] || replied['s:' + s.id] || G.dismissed?.has(String(s.id)) || t - s.ts > SURF_AGE) continue;
    const a = agents.find(x => x.pane === s.pane); if (!a) continue;
    out.push({ a, nm: a.home ? HOME_NAME() : a.title || a.name || a.pane, pane: a.pane, kind: 'surf', key: 'surf:' + surfKey(s), rank: 2, ts: s.ts, s });
  }
  for (const [k, at] of held) if (t - at > HOLD) held.delete(k);
  return out.filter(x => !held.has(x.key) && !(snz[x.key] > t)).sort((x, y) => x.rank - y.rank || (x.ts || 0) - (y.ts || 0));
}
const snoozedN = () => { const t = Date.now(); return Object.values(snz).filter(v => v > t).length; };
export function count(st = G.st) { return itemsOf(st).length; }
// an item opens its agent's chat at the item: the surface's tool row, the finished turn's answer, else the bottom
export function hrefOf(it) {
  const qi = it.kind === 'surf' ? it.s.i : it.kind === 'done' ? it.a.answerI : null;
  return agentHref(it.a) + (qi != null ? '?i=' + qi : '');
}

// ---------- per-kind card content + actions ----------
// act: {label, sub?, run: () => Promise, open?: true (navigates), reply?: text (surface reply via app.js)}
function planChoices(list) { // same mapping as v3.js planChoices
  const L = list.map((o, i) => ({ i, l: String(o.label || '').toLowerCase() }));
  const auto = L.find(o => /^yes/.test(o.l) && /auto|bypass/.test(o.l));
  const manual = L.find(o => /^yes/.test(o.l) && o !== auto && !/bypass/.test(o.l));
  return { auto: auto?.i ?? null, manual: manual?.i ?? null };
}
function spec(it) {
  const { a, pane } = it, P = `a/${enc(pane)}/`, open = (label = 'Open chat') => ({ label, open: true });
  const choose = (o, i) => ({ label: o.label, run: () => post(P + 'choose', { index: i }) });
  switch (it.kind) {
    case 'perm': {
      const p = it.p;
      return { pill: 'Permission', ctx: `Allow <b>${esc(p.tool)}</b>?`, code: p.summary, acts: [
        { label: 'Allow', run: () => post(P + 'perm', { id: p.id, decision: 'allow' }) },
        p.always && { label: 'Allow always', sub: 'adds the rule Claude suggests', run: () => post(P + 'perm', { id: p.id, decision: 'always' }) },
        { label: 'Deny', cls: 'neg', run: () => post(P + 'perm', { id: p.id, decision: 'deny' }) }] };
    }
    case 'denied': {
      const p = it.p;
      return { pill: 'Blocked by auto mode', ctx: `<b>${esc(p.tool)}</b>${p.reason ? ' — ' + esc(p.reason) : ''}`, code: p.summary, acts: [
        { label: 'Allow once', run: () => post(P + 'perm', { id: p.id, decision: 'allow' }) },
        { label: 'Dismiss', cls: 'neg', run: () => post(P + 'perm', { id: p.id, decision: 'dismiss' }) }] };
    }
    case 'phone': {
      const q = a.ask.questions?.[0] || {}, os = q.options || [];
      const acts = os.slice(0, os.length > 3 ? 2 : 3).map(o => ({ label: o.label, run: () => post(P + 'answer', { text: o.label, id: a.ask.id }) }));
      if (!os.length || os.length > 3) acts.push(open(os.length ? `${os.length - 2} more options…` : 'Reply in chat'));
      return { pill: 'Question', ctx: esc(q.question || ''), acts };
    }
    case 'ask': {
      const qs = a.ask.questions, q = qs[0], os = q.options || [];
      const simple = qs.length === 1 && !q.multiSelect && os.length;
      const acts = simple ? os.slice(0, os.length > 3 ? 2 : 3).map(o => ({ label: o.label, sub: o.description, run: () => post(P + 'askanswer', { answers: [{ labels: [o.label] }] }) })) : [];
      if (!simple || os.length > 3) acts.push(open(simple ? `${os.length - 2} more options…` : qs.length > 1 ? `Answer ${qs.length} questions` : 'Answer in chat'));
      return { pill: q.header ? 'Question · ' + q.header : 'Question', ctx: esc(q.question || ''), acts };
    }
    case 'plan': {
      const m = menus.get(pane), c = m ? planChoices(m.list) : {};
      const ld = !m;
      const pl = String(a.plan.text || '').split('\n'), h0 = pl.findIndex(l => l.trim());
      return { pill: 'Plan', ctx: esc(one(pl[h0] || 'Plan ready')), md: pl.slice(h0 + 1).join('\n').trim(), acts: [
        c.manual != null || ld ? { label: 'Approve', wait: ld, run: () => post(P + 'choose', { index: c.manual }) } : null,
        c.auto != null || ld ? { label: 'Approve + auto-accept', wait: ld, run: () => post(P + 'choose', { index: c.auto }) } : null,
        open('Keep planning…')] };
    }
    case 'menu': {
      const m = menus.get(pane), list = m?.list || [];
      return { pill: 'Needs you', ctx: esc(m?.q || (a.doing?.label ? 'Waiting on ' + a.doing.label.split(' · ')[0] : 'Waiting for you')), code: m?.body || a.doing?.label?.split(' · ').slice(1).join(' · ') || '',
        acts: !m ? [{ label: 'Loading options…', wait: true }, open('Open chat')]
          : list.length >= 2 ? list.slice(0, 3).map(choose) : [open('Open chat')] };
    }
    case 'surf': {
      const s = it.s;
      // a "done" reply confirms the user acted: never the primary (swipe-right) action — opening the chat is
      const todo = s.replies.some(r => DONE_REP.test(r.trim()));
      const reps = s.replies.slice(0, todo ? 2 : 3).map(r => ({ label: repLabel(r), reply: r }));
      return { pill: todo ? 'Your turn' : 'Reply', ctx: `<b>${esc(s.title)}</b>`, md: s.body || '', code: s.command, surf: s, imgs: s.images,
        acts: [...(todo ? [open('Open chat'), ...reps] : reps), { label: 'Dismiss', cls: 'neg', run: async () => dismissSurfs([s.id]) }] };
    }
    case 'done': {
      // swipe right / acts[0] = Mark seen (quiet); the card itself leads with Reply and the agent's suggested follow-ups
      const tk = a.talk || {};
      const tl = tk.tldr?.text && (a.answerI == null || tk.tldr.i >= a.answerI) ? tk.tldr.text : '';
      const full = answerOf(a), snip = a.last?.role === 'assistant' ? a.last.text : a.recap?.text || '';
      const ans = full ?? snip;
      const seen = new Set(), chips = [];
      for (const c of [tk.suggest, ...(a.chips || [])]) {
        const t = bare(c), k = t.toLowerCase();
        if (!t || seen.has(k) || /^(thanks?|thank you|ty)[.!]*$/i.test(t)) continue; // "thanks" = Mark seen here
        seen.add(k); chips.push(t);
      }
      const q = asks(ans) || chips.some(c => /^(yes|no)\b/i.test(c)) || !!a.ask?.questions?.length;
      return { pill: q ? 'Done · asks you' : 'Done', ctx: tl ? '<b class="tl">TL;DR</b> ' + esc(tl) : ans ? '' : 'Turn finished',
        md: full != null ? full : '', more: full == null ? snip : '', loading: full == null && a.answerI != null && !!a.tid,
        done: { chips: chips.slice(0, 4), q }, acts: [
          { label: 'Mark seen', run: async () => { markSeen(a); G.view?.onState?.(G.st); } },
          open('Open chat')] };
    }
  }
}

// screen menus for plan approvals and generic prompts (labels → choose index)
// the dialog text above the numbered options: its question line and the lines above it (command, description)
const qOf = text => {
  const raw = String(text || '').replace(/\x1b\[[0-9;?]*[A-Za-z]/g, '').split('\n');
  const L = raw.map(l => l.replace(/[│╭╮╰╯─]/g, '').trim());
  const k = L.findIndex(l => /^(❯\s*)?1\.\s/.test(l));
  let j = k - 1; while (j >= 0 && !L[j]) j--;
  if (k < 0 || j < 0) return { q: '', body: '' };
  const box = [];
  for (let i = j - 1; i >= 0 && i >= j - 8; i--) { if (/[╭─]{3}/.test(raw[i]) || /^[>●✻]/.test(L[i])) break; box.unshift(L[i]); }
  return { q: L[j], body: box.join('\n').trim() };
};
async function loadMenu(it) {
  if (menus.has(it.pane) && menus.get(it.pane).sig === it.key) return;
  try {
    const r = await api(`a/${enc(it.pane)}/screen`);
    menus.set(it.pane, { sig: it.key, list: r.options || [], ...qOf(r.text) });
  } catch { menus.set(it.pane, { sig: it.key, list: [], q: '', body: '' }); }
  hook?.();
}

// the full final answer (markdown) for a done card: the chat's cached transcript, else one small fetch
function answerOf(a) {
  if (a.answerI == null || !a.tid) return null;
  const k = a.tid + ':' + a.answerI;
  if (answers.has(k)) return answers.get(k);
  const it = G.tc?.get?.(a.tid)?.items?.find(x => x.i === a.answerI && x.k === 'text');
  if (it) { answers.set(k, it.text); return it.text; }
  return null;
}
// ---------- readers ----------
// The hero acts on its top item in place; acting holds the item (hidden until the server catches up) so the next
// one slides in.
let hook = null;
export const setHook = fn => { hook = fn; };
export const items = st => itemsOf(st);
export { spec, surfKey, loadMenu, answerOf };
export function hold(key) { held.set(key, Date.now()); }
export function heroAct(it, k) {
  const x = spec(it).acts.filter(Boolean)[k];
  if (!x || x.wait) { toast('Still loading the options…', '', null, 1200); return false; }
  haptic();
  if (x.open) { if (it.kind === 'done') markSeen(it.a); nav(hrefOf(it)); return true; }
  held.set(it.key, Date.now()); hook?.();
  Promise.resolve().then(x.run).then(null, e => {
    held.delete(it.key); hook?.();
    toast(e.status === 404 ? 'Already answered' : 'Failed: ' + esc(e.message), 'err');
  });
  return true;
}
export function heroSnooze(it) { haptic(); snz[it.key] = Date.now() + SNOOZE; saveSnz(); hook?.(); }
// typed answer: ask_phone takes free text; surfaces and finished turns get it as a prompt (steer while working)
export async function heroReply(it, text) {
  let t = String(text || '').trim(); if (!t) return false;
  const [x, ks] = takeMods(t); t = x.trim(); if (!t) return false;
  const a = it.a; haptic(); held.set(it.key, Date.now()); hook?.();
  try {
    if (it.kind === 'phone') await post(`a/${enc(a.pane)}/answer`, { text: t, id: a.ask.id });
    else await post(`a/${enc(a.pane)}/${a.status === 'working' ? 'steer' : 'prompt'}`, { text: t + modSuffix([...new Set([...(ls.get('pmods.' + a.pane, null) || []), ...ks])]), nonce: rid() });
    markSeen(a); toast('Sent to ' + esc(it.nm), 'ok', null, 1600); return true;
  } catch (e) { held.delete(it.key); hook?.(); toast('Not sent: ' + esc(e.message), 'err'); return false; }
}
export const snoozed = snoozedN;
export function unsnooze() { snz = {}; saveSnz(); hook?.(); }
