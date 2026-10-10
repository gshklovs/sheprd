// Lazy: Decide (API.md "Decisions", server/src/decide.rs):
// 1. Home: an "N to decide" card under the briefing (thumbnails, how many picks with pictures / quick calls, about
//    how long, the oldest) → the deck. `homeCard(el, st)` from home.js draw, off `state.decisions`.
// 2. #/decide[/<id>]: a full-screen deck, one decision at a time, with a progress bar. Picture options are a sideways
//    row of big cards (tap to pick; rank and multi number / tick them in tap order); word options are big buttons (a
//    plain pick sends on tap). An optional note (text, or a voice note through voice.js) goes along. Later (or a
//    swipe up) puts it back for 3 h. The pick goes to the agent that asked through the steer path (held while the user
//    has a draft there; an asleep agent is resumed with it).
// 3. After the last one: the Stale section (looks answered / deadline passed): Clear or Keep each. Then a summary.
import { G, $, h, esc, api, post, toast, haptic, goBack, nav, rel, whoName, lightbox, lazy, HOME_NAME } from './app.js';

await new Promise(r => {
  if (document.querySelector('link[data-decide]')) return r();
  const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'decide.css'; l.dataset.decide = 1;
  l.onload = l.onerror = r; document.head.append(l); setTimeout(r, 3000);
});

const enc = encodeURIComponent;
const IC = {
  x: '<svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6L6 18"/></svg>',
  mic: '<svg viewBox="0 0 24 24"><rect x="9" y="3" width="6" height="11" rx="3"/><path d="M5.5 11a6.5 6.5 0 0 0 13 0M12 17.5V21"/></svg>',
  chk: '<svg viewBox="0 0 24 24"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>',
  back: '<svg viewBox="0 0 24 24"><path d="M15 5l-7 7 7 7"/></svg>',
  dec: '<svg viewBox="0 0 24 24"><path d="M4 6h10M4 12h7M4 18h10"/><path d="M15.5 12.5l2.5 2.5 4-5"/></svg>',
};

// ---------- who asked ----------
function who(r) {
  const a = G.st?.agents?.find(x => x.pane === r.pane);
  const name = whoName(a?.title || r.agentTitle || 'An agent', r.pane) || 'An agent';
  const ed = name === HOME_NAME();
  return { name, ed, cls: !ed && a?.color ? 'col-' + a.color : '', init: ed ? HOME_NAME().slice(0, 2).toUpperCase() : (name.match(/[A-Za-z0-9]/)?.[0] || '?').toUpperCase() };
}
const avHTML = w => `<span class="${w.cls}"><span class="dav${w.ed ? ' ed' : ''}">${esc(w.init)}</span></span>`;
const nameHTML = w => `<span class="${w.cls}"><b>${esc(w.name)}</b></span>`;
// markdown → one plain paragraph
const plain = s => String(s || '').replace(/```[\s\S]*?```/g, ' ').replace(/!\[[^\]]*\]\([^)]*\)/g, '').replace(/\[([^\]]+)\]\([^)]*\)/g, '$1')
  .replace(/[*_`>#]+/g, '').replace(/\s*\n\s*[-•]\s*/g, ' · ').replace(/\s+/g, ' ').trim();
const pics = r => (r.decision.options || []).some(o => o.images?.length);
// a short tag for the Send button: "3" from "3 · Jump from messages", a short label as is, else A, B, C…
const tag = (o, k) => o.label.match(/^(\S{1,3}) ·/)?.[1] || (o.label.length <= 14 ? o.label : String.fromCharCode(65 + k));
const dueTxt = t => {
  const ms = t - Date.now(); if (ms <= 0) return 'Past its deadline';
  const d = new Date(t), hrs = ms / 3600e3;
  return 'Due ' + (hrs < 20 ? 'in ' + (hrs < 1 ? Math.max(1, Math.round(ms / 60e3)) + ' min' : Math.round(hrs) + ' h') : d.toLocaleString([], { weekday: 'short', hour: 'numeric', minute: '2-digit' }));
};

// ---------- 1. Home card ----------
export function homeCard(el, st) {
  const d = st?.decisions, n = d?.open || 0;
  let box = $('.dwrap', el);
  if (!n) { if (box) box.hidden = true; return; }
  if (!box) { box = h('<div class="dwrap"></div>'); box.addEventListener('click', () => { haptic(); nav('#/decide'); }); }
  const brief = $('.brief', el); if (brief && box.previousElementSibling !== brief) brief.after(box); // right under the sentence, above Recent
  box.hidden = false;
  const quick = n - (d.pictures || 0), mins = Math.max(1, Math.round(((d.pictures || 0) * 40 + quick * 15) / 60));
  const kinds = [d.pictures ? `${d.pictures} pick${d.pictures > 1 ? 's' : ''} with pictures` : '', quick ? `${quick} quick call${quick > 1 ? 's' : ''}` : ''].filter(Boolean).join(' · ');
  const old = d.oldest ? rel(d.oldest) : '';
  const thumbs = (d.thumbs || []).slice(0, 3);
  const nx = d.next, w = nx ? who(nx) : null;
  const html = `<div class="dentry" role="button" aria-label="${n} to decide: start">
      ${thumbs.length ? `<div class="dstack">${thumbs.map(s => `<img alt="" loading="lazy" src="${esc(s)}">`).join('')}</div>` : `<div class="dico">${IC.dec}</div>`}
      <div class="dtx"><div class="dt1">${n} to decide</div><div class="dt2">${esc(kinds)}<br>about ${mins} min${old && old !== 'now' ? ` · oldest ${esc(old.replace(/d$/, ' days').replace(/^1 days$/, '1 day'))}` : ''}${d.stale ? ` · ${d.stale} stale` : ''}</div></div>
      <span class="dgo">Start</span></div>
    ${nx ? `<div class="dnext">Next up: ${nameHTML(w)} · ${esc(plain(nx.title))}</div>` : ''}`;
  if (box.dataset.h !== html) { box.dataset.h = html; box.innerHTML = html; }
}

// ---------- 2. the deck ----------
export function route(hs) {
  const id = decodeURIComponent(hs.match(/^\/decide\/([^?/]+)/)?.[1] || '');
  return { key: 'decide' + (id ? '/' + id : ''), make: () => new DeckView(id) };
}

class DeckView {
  constructor(start) {
    this.start = start; this.q = null; this.stale = []; this.i = 0; this.sel = []; this.log = [];
    this.el = h(`<div class="view deck">
      <div class="dk-top"><button class="dk-back" aria-label="Previous question" disabled>${IC.back}<span>Back</span></button><div class="dk-prog"></div><button class="dk-x" aria-label="Close">${IC.x}</button></div>
      <div class="dk-flash" aria-live="polite"></div>
      <div class="dk-body"><div class="dk-empty"><span class="spin"></span></div></div>
      <div class="dk-foot" hidden>
        <div class="crow"><textarea rows="1" placeholder="Add a note (optional)" enterkeyhint="done"></textarea><button class="mic" aria-label="Voice note">${IC.mic}</button><input class="vfile" type="file" accept="audio/*,video/*" hidden></div>
        <div class="dk-row"><button class="dbtn sec later">Later</button><button class="dbtn pri send"><span>Send</span></button></div>
        <div class="dk-hint">Swipe up = later · the agent gets it as a message from you</div>
      </div>
    </div>`);
    this.ta = $('textarea', this.el); this.body = $('.dk-body', this.el);
  }
  get pane() { return this.q?.[this.i]?.pane || ''; } // voice.js names the upload after it
  mount() {
    $('.dk-x', this.el).onclick = () => goBack('#/agents');
    $('.dk-back', this.el).onclick = () => this.back();
    $('.later', this.el).onclick = () => this.answered() ? this.skip() : this.later();
    $('.send', this.el).onclick = () => this.answered() ? this.changeOn() : this.sendSel();
    const vfile = $('.vfile', this.el);
    $('.mic', this.el).onclick = () => window.isSecureContext && window.MediaRecorder && navigator.mediaDevices?.getUserMedia ? lazy('voice').then(m => m.record(this)) : vfile.click();
    vfile.onchange = () => { const f = vfile.files[0]; vfile.value = ''; if (f) lazy('voice').then(m => m.transcribe(this, f)); };
    this.ta.addEventListener('input', () => this.autosize());
    this.ta.addEventListener('keydown', e => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); this.ta.blur(); } });
    this.body.addEventListener('click', e => this.tap(e));
    // swipe up (at the bottom of the card) = later; swipe right (not from the screen edge: that's the app's back) = the
    // previous question. Not on the sideways row of picture options, which scrolls.
    let t0 = null;
    this.body.addEventListener('touchstart', e => { const t = e.touches[0]; t0 = e.target.closest('.dk-opts') ? null : { x: t.clientX, y: t.clientY, end: this.body.scrollTop + this.body.clientHeight >= this.body.scrollHeight - 4 }; }, { passive: true });
    this.body.addEventListener('touchend', e => {
      const t = e.changedTouches[0]; if (!t0) return;
      const dy = t.clientY - t0.y, dx = t.clientX - t0.x;
      if (this.cur && !this.answered() && t0.end && dy < -90 && Math.abs(dx) < 60) this.later();
      else if (t0.x >= 28 && dx > 90 && Math.abs(dy) < 50) this.back();
      t0 = null;
    }, { passive: true });
    this.load();
  }
  resume() { if (!this.q) this.load(); }
  destroy() { this.dead = true; }
  onState() { }
  autosize() { const t = this.ta; t.style.height = '34px'; t.style.height = Math.min(96, t.scrollHeight) + 'px'; this.foot(); }
  get cur() { return this.q && this.i < this.q.length ? this.q[this.i] : null; }
  async load() {
    try {
      const j = await api('decisions');
      this.q = j.open || []; this.stale = j.stale || [];
      const k = this.start ? this.q.findIndex(r => r.id === this.start) : -1;
      if (k > 0) this.q.unshift(...this.q.splice(k, 1));
      if (this.start && k < 0 && this.stale.some(r => r.id === this.start)) this.i = this.q.length; // a stale one: straight to Stale
    } catch (e) { this.body.innerHTML = `<div class="dk-empty"><p>${esc(e.message)}</p></div>`; return; }
    this.render();
  }
  prog() {
    const n = this.q.length;
    $('.dk-prog', this.el).innerHTML = n ? this.q.map((r, k) => { const l = this.res(r); return `<i class="${k === this.i ? 'c' : l?.how === 'later' ? 's' : l ? 'd' : ''}"></i>`; }).join('') : '';
    $('.dk-back', this.el).disabled = this.i === 0 || !n;
  }
  // this deck session's history: the latest result for a question (sent / later), or none
  res(r) { for (let k = this.log.length - 1; k >= 0; k--) if (this.log[k].r === r) return this.log[k]; return null; }
  answered() { const r = this.cur; return !!r && this.res(r)?.how === 'sent' && !this.changing; }
  // back to the previous question: an answered one shows the answer (Change answer), a put-off one is open again
  async back() {
    if (!this.q?.length || this.i === 0 || this.busy) return;
    haptic(); this.changing = false;
    this.i = Math.min(this.i, this.q.length) - 1;
    const r = this.q[this.i], l = this.res(r);
    if (l?.how === 'later') {
      this.busy = true;
      try { await post('decide/' + enc(r.id) + '/keep', {}); this.log = this.log.filter(x => x.r !== r); }
      catch (e) { toast(esc(e.message), 'err'); }
      finally { this.busy = false; }
    }
    this.render();
  }
  skip() { this.changing = false; this.next(); } // past an answered one without changing it
  changeOn() { this.changing = true; this.sel = []; this.ta.value = ''; this.mark(); this.card(this.cur); haptic(); }
  render() {
    if (this.dead) return;
    this.prog();
    const r = this.cur;
    $('.dk-foot', this.el).hidden = !r;
    this.body.scrollTop = 0; this.body.classList.remove('out', 'done');
    if (r) return this.card(r);
    if (this.stale.length) return this.staleList();
    this.summary();
  }
  card(r) {
    const d = r.decision, w = who(r), opts = d.options || [], P = pics(r);
    const n = this.q.length;
    const sub = plain(r.body);
    const strip = !P && r.images?.length; // the card's own pictures, options in words
    const optHTML = P || strip
      ? `<div class="dk-opts">${(P ? opts : r.images.map((s, k) => ({ id: null, label: '', images: [s], k }))).map((o, k) => `
          <div class="dk-opt${o.id ? '' : ' pic'}" ${o.id ? `data-o="${esc(o.id)}"` : `data-pic="${k}"`}>
            <div class="img">${o.images?.length ? `<img alt="" loading="lazy" src="${esc(o.images[0])}">` : ''}${(P ? o.images?.length > 1 : true) ? `<span class="pg" data-pg="${k}">${P ? `1 of ${o.images.length}` : `${k + 1} of ${r.images.length}`}</span>` : ''}</div>
            ${o.id ? `<div class="lab"><span class="rk"></span><div><b>${esc(o.label)}</b>${o.sub ? `<i>${esc(o.sub)}</i>` : ''}</div></div>` : ''}
          </div>`).join('')}</div>
         <div class="dk-dots">${(P ? opts : r.images).map((_, k) => `<i class="${k ? '' : 'on'}"></i>`).join('')}</div>`
      : '';
    const words = !P ? opts.map(o => `<button class="bigopt" data-o="${esc(o.id)}"><div>${esc(o.label)}${o.sub ? `<small>${esc(o.sub)}</small>` : ''}</div>${d.kind === 'rank' || d.kind === 'multi' ? '<span class="rk"></span>' : ''}</button>`).join('') : '';
    const hint = d.kind === 'rank' ? 'Tap in the order you want them.' : d.kind === 'multi' ? 'Tap every one that applies.' : P ? 'Tap the one you want.' : '';
    this.body.innerHTML = `
      <div class="dk-who">${avHTML(w)}<span>${nameHTML(w)} · ${esc(rel(r.ts))} · ${this.i + 1} of ${n}</span></div>
      <div class="dk-q">${esc(plain(r.title))}</div>
      ${sub || hint ? `<div class="dk-sub${sub.length > 220 ? ' clamp' : ''}">${esc(sub)}${sub && hint ? ' ' : ''}${hint && (P || d.kind !== 'pick') ? esc(hint) : ''}</div>` : ''}
      ${d.deadline ? `<div class="dk-due">${esc(dueTxt(d.deadline))}</div>` : ''}
      ${optHTML}${strip ? '<div style="height:12px"></div>' : ''}${words}`;
    const row = $('.dk-opts', this.body);
    if (row) row.addEventListener('scroll', () => {
      const c = row.firstElementChild; if (!c) return;
      const k = Math.round(row.scrollLeft / (c.offsetWidth + 12));
      [...$('.dk-dots', this.body).children].forEach((x, j) => x.classList.toggle('on', j === k));
    }, { passive: true });
    this.sel = []; this.ta.value = ''; this.autosize();
    const l = this.res(r);
    if (l?.how === 'sent' && !this.changing) { // answered earlier in this session: show what went
      this.sel = l.choice.slice(); this.mark();
      $('.dk-who', this.body).insertAdjacentHTML('beforebegin', `<div class="dk-was">${IC.chk}<span>You ${l.changed ? 'changed it to' : 'picked'} <b>${esc(l.what)}</b>${l.note ? ` · “${esc(l.note)}”` : ''}${l.held ? ' · waits for your draft' : ''}</span></div>`);
      this.body.classList.add('done');
    } else this.body.classList.remove('done');
    this.foot();
    this.body.classList.remove('out');
  }
  tap(e) {
    const r = this.cur;
    const pg = e.target.closest('[data-pg]');
    if (pg && r) {
      const k = +pg.dataset.pg, P = pics(r);
      const list = P ? r.decision.options[k].images : r.images;
      lightbox(P ? list[0] : list[k], list); return;
    }
    const pic = e.target.closest('[data-pic]'); if (pic && r) { lightbox(r.images[+pic.dataset.pic], r.images); return; }
    const b = e.target.closest('[data-o]');
    if (b && r) return this.pick(b.dataset.o);
    const s = e.target.closest('[data-st]'); if (s) return this.staleAct(s.dataset.id, s.dataset.st);
    if (e.target.closest('.dk-close')) goBack('#/agents');
  }
  pick(id) {
    if (this.answered()) return; // Change answer first
    const r = this.cur, d = r.decision; haptic();
    if (d.kind === 'rank' || d.kind === 'multi') {
      const k = this.sel.indexOf(id);
      if (k >= 0) this.sel.splice(k, 1); else this.sel.push(id);
    } else {
      this.sel = this.sel[0] === id ? [] : [id];
      if (!pics(r) && this.sel.length) { this.mark(); return this.send([id]); } // a word pick sends on tap
    }
    this.mark();
  }
  mark() {
    const d = this.cur.decision;
    this.body.querySelectorAll('[data-o]').forEach(el => {
      const k = this.sel.indexOf(el.dataset.o), rk = $('.rk', el);
      el.classList.toggle('sel', k >= 0);
      if (rk) rk.innerHTML = k < 0 ? '' : d.kind === 'multi' ? IC.chk : d.kind === 'rank' ? String(k + 1) : IC.chk;
    });
    this.foot();
  }
  foot() {
    const r = this.cur; if (!r) return;
    const ans = this.answered();
    $('.later', this.el).textContent = ans ? 'Next' : 'Later';
    $('.crow', this.el).hidden = ans;
    $('.dk-hint', this.el).textContent = ans ? 'Change it and the agent gets a correction from you' : this.changing ? 'The agent gets “The user changed their answer: …”' : 'Swipe up = later · the agent gets it as a message from you';
    if (ans) { const b = $('.send', this.el); b.disabled = false; $('span', b).textContent = 'Change answer'; return; }
    const d = r.decision, opts = d.options || [];
    const note = this.ta.value.trim();
    const tags = this.sel.map(id => { const k = opts.findIndex(o => o.id === id); return tag(opts[k], k); });
    const btn = $('.send', this.el);
    btn.disabled = !tags.length && !note;
    $('span', btn).textContent = !tags.length ? (note ? 'Send note' : (pics(r) || d.kind === 'rank' || d.kind === 'multi' ? 'Pick one' : 'Tap one, or write a note'))
      : d.kind === 'multi' ? (tags.length === 1 ? `Send “${tags[0]}”` : `Send the ${tags.length} ticked`)
      : tags.length === 1 && d.kind !== 'rank' ? `Send “${tags[0]}”` : 'Send ' + tags.join(', ');
  }
  sendSel() { if (this.sel.length || this.ta.value.trim()) this.send(this.sel.slice()); }
  async send(choice) {
    const r = this.cur; if (!r || this.busy) return;
    const w = who(r), note = this.ta.value.trim();
    this.busy = true;
    try {
      const change = !!this.changing;
      const j = await post('decide/' + enc(r.id), change ? { choice, note, change: true } : { choice, note });
      const opts = r.decision.options || [];
      const what = choice.map(id => opts.find(o => o.id === id)?.label || id).join(', ') || 'a note';
      this.log = this.log.filter(x => x.r !== r);
      this.log.push({ r, how: 'sent', what, held: j.held, choice: choice.slice(), note, changed: change });
      this.changing = false;
      this.flash(j.held ? `Waits for your draft in ${w.name}’s box, then goes in` : change ? `Sent the change to ${w.name}` : j.via === 'resume' ? `Waking ${w.name} with your pick` : `Sent to ${w.name}`);
      this.next();
    } catch (e) { toast(esc(e.message), 'err'); }
    finally { this.busy = false; }
  }
  async later() {
    const r = this.cur; if (!r || this.busy) return;
    this.busy = true;
    try { await post('decide/' + enc(r.id) + '/later', {}); this.log = this.log.filter(x => x.r !== r); this.log.push({ r, how: 'later' }); haptic(); this.next(); }
    catch (e) { toast(esc(e.message), 'err'); }
    finally { this.busy = false; }
  }
  // one quiet line under the progress bar instead of stacking toasts over the options
  flash(t) {
    const f = $('.dk-flash', this.el); f.innerHTML = `${IC.chk}<span>${esc(t)}</span>`; f.classList.add('on');
    clearTimeout(this.ft); this.ft = setTimeout(() => f.classList.remove('on'), 2600);
  }
  next() {
    this.body.classList.add('out');
    setTimeout(() => { this.i++; this.render(); }, 200);
  }
  staleList() {
    $('.dk-prog', this.el).innerHTML = '';
    this.body.innerHTML = `<div class="dk-sec">Stale · ${this.stale.length}</div>
      <div class="dk-sub" style="padding:0 32px 10px">These look answered or out of date. Clear them, or keep one open.</div>
      ${this.stale.map(r => { const w = who(r); return `<div class="dk-card">
        <div class="h">${avHTML(w)}${nameHTML(w)}<span class="ago">${esc(rel(r.ts))}</span></div>
        <div class="q">${esc(plain(r.title))}</div>
        <div class="why">${esc(cap(r.decision.reason || 'Out of date'))}.</div>
        <div class="ch"><button class="chipb pri" data-st="clear" data-id="${esc(r.id)}">Clear</button><button class="chipb" data-st="keep" data-id="${esc(r.id)}">Keep open</button></div></div>`; }).join('')}
      <div class="dk-row" style="padding:8px 16px 24px"><button class="dbtn sec" data-st="all" style="flex:1">Clear all</button><button class="dbtn pri dk-close">Done</button></div>`;
  }
  async staleAct(id, act) {
    const ids = act === 'all' ? this.stale.map(r => r.id) : [id];
    for (const x of ids) {
      try { await post(`decide/${enc(x)}/${act === 'keep' ? 'keep' : 'clear'}`, {}); }
      catch (e) { toast(esc(e.message), 'err'); return; }
      const r = this.stale.find(s => s.id === x);
      this.stale = this.stale.filter(s => s.id !== x);
      if (act === 'keep' && r) { r.decision.state = 'open'; this.q.push(r); }
    }
    haptic(); this.render();
  }
  summary() {
    const sent = this.log.filter(l => l.how === 'sent'), later = this.log.filter(l => l.how === 'later');
    $('.dk-prog', this.el).innerHTML = this.q.map(r => `<i class="${this.res(r)?.how === 'later' ? 's' : 'd'}"></i>`).join('');
    $('.dk-back', this.el).disabled = !this.q.length;
    this.body.innerHTML = `<div class="dk-empty"><div class="big">${IC.chk}</div>
        <h2>${this.log.length ? 'All decided' : 'Nothing to decide'}</h2>
        <p>${this.log.length ? [sent.length ? `${sent.length} sent` : '', later.length ? `${later.length} for later` : ''].filter(Boolean).join(' · ') : 'No agent is waiting on a choice from you.'}</p></div>
      ${sent.length ? `<div class="dk-sent">${sent.map(l => { const w = who(l.r); return `<div>${nameHTML(w)}<span>${esc(l.what)}${l.held ? ' · waits for your draft' : ''}</span></div>`; }).join('')}</div>` : ''}
      <div class="dk-row" style="padding:16px"><button class="dbtn pri dk-close">Done</button></div>`;
  }
}
const cap = s => s ? s[0].toUpperCase() + s.slice(1) : s;
