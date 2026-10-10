// Lazy: Send to the home agent (API.md "Send to the home agent", server/src/send.rs). A link, a note, a photo or a
// voice note, plus an optional note of your own → POST /api/send, delivered to the home agent when its input box is
// free. Opened from the home agent's chat menu and #/?send=1 (the share page's "Send something else"). iOS can't
// list web apps in the share sheet, so "Send from any app's Share button…" explains a one-time Shortcut.
import { I, $, esc, sheet, toast, haptic, HOME_NAME } from './app.js';

await new Promise(r => {
  if (document.querySelector('link[data-send]')) return r();
  const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'send.css'; l.dataset.send = 1;
  l.onload = l.onerror = r; document.head.append(l); setTimeout(r, 3000);
});

const IC = {
  link: '<svg viewBox="0 0 24 24"><path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"/></svg>',
  note: '<svg viewBox="0 0 24 24"><path d="M5 4h14v12l-4 4H5zM15 20v-4h4M8 9h8M8 13h5"/></svg>',
  photo: '<svg viewBox="0 0 24 24"><rect x="3" y="5" width="18" height="15" rx="2.5"/><circle cx="12" cy="12.5" r="3.5"/><path d="M8 5l1.5-2h5L16 5"/></svg>',
  mic: '<svg viewBox="0 0 24 24"><rect x="9" y="3" width="6" height="11" rx="3"/><path d="M5.5 11a6.5 6.5 0 0 0 13 0M12 17.5V21"/></svg>',
  paste: '<svg viewBox="0 0 24 24"><rect x="6" y="4" width="12" height="17" rx="2"/><path d="M9 4V3h6v1M9 10h6M9 14h4"/></svg>',
};
let voiceMod = null;
const vm = () => voiceMod ? Promise.resolve(voiceMod) : import('./voice.js').then(m => (voiceMod = m));
const PH = 'Paste a link or write a note';
// The compose sheet. `c`: {cls, h, sub, ev, go, how, note?: placeholder of the note field, init?: {text, photo}}.
// `send({text, photo, voice, note})` posts and answers the server's reply; `after(r)` runs once it went.
function composeSheet(c, send, after) {
  const sh = sheet(`<div class="st">
      <h2 class="st-h">${c.h}</h2>
      <p class="st-sub">${c.sub}</p>
      <div class="st-field"><textarea class="st-ta" rows="3" placeholder="${PH}" autocomplete="off" autocapitalize="sentences" enterkeyhint="done"></textarea>
        <div class="st-kind" hidden></div></div>
      <div class="st-photo" hidden><img alt=""><button class="ib st-px" aria-label="Remove photo">${I.x}</button></div>
      <div class="crow st-row">
        <button class="st-b st-paste" data-ev="${c.ev}-paste">${IC.paste}<span>Paste</span></button>
        <label class="st-b" data-ev="${c.ev}-photo">${IC.photo}<span>Photo</span><input type="file" accept="image/*" class="st-file" hidden></label>
        <button class="st-b mic" data-ev="${c.ev}-voice">${IC.mic}<span>Voice</span></button>
        <input type="file" class="vfile" accept="audio/*,video/*,.m4a,.mp3,.wav,.caf,.webm,.ogg" hidden>
      </div>
      ${c.note ? `<div class="st-field st-nf"><textarea class="st-note" rows="1" placeholder="${esc(c.note)}" autocomplete="off" autocapitalize="sentences" aria-label="${esc(c.noteLabel || 'Note (optional)')}"></textarea></div>` : ''}
      <p class="st-err" hidden></p>
      <button class="btn primary st-go" data-ev="${c.ev}-submit" disabled>${c.go}</button>
      <button class="st-how" data-ev="${c.ev}-shortcut">${c.how}</button>
      ${c.alt ? `<button class="st-how st-alt" data-ev="${c.ev}-alt">${c.alt}</button>` : ''}
    </div>`, c.cls);
  const ta = $('.st-ta', sh), go = $('.st-go', sh), kind = $('.st-kind', sh), err = $('.st-err', sh), ph = $('.st-photo', sh), nt = $('.st-note', sh);
  let photo = null;
  // voice.js draws its record / transcribe chip before `.crow` and lands the text in `ta`
  const v = { el: $('.st', sh), ta, pane: '', autosize: () => grow() };
  const grow = () => { ta.style.height = 'auto'; ta.style.height = Math.min(ta.scrollHeight, 200) + 'px'; if (nt) { nt.style.height = 'auto'; nt.style.height = Math.min(nt.scrollHeight, 120) + 'px'; } };
  let kindWas = null;
  const upd = () => { // idempotent: also runs on voice.js chip changes (MutationObserver below)
    const t = ta.value.trim(), u = t.match(/https?:\/\/\S+/)?.[0];
    const k = u ? `${IC.link}<span>Link · ${esc(host(u))}</span>` : photo ? `${IC.photo}<span>Photo${t ? ' with a caption' : ''}</span>` : t && $('.vnote.ok', sh) ? `${IC.mic}<span>Voice note</span>` : t ? `${IC.note}<span>Note</span>` : '';
    if (k !== kindWas) { kindWas = k; kind.innerHTML = k; kind.hidden = !k; }
    if (!go.classList.contains('busy')) go.disabled = !t && !photo && !nt?.value.trim();
    grow();
  };
  const setPhoto = f => { photo = f; $('img', ph).src = URL.createObjectURL(f); ph.hidden = false; ta.placeholder = 'Add a caption (optional)'; upd(); };
  ta.addEventListener('input', () => { err.hidden = true; upd(); });
  nt?.addEventListener('input', () => { err.hidden = true; upd(); });
  ta.addEventListener('keydown', e => { if (e.key === 'Enter' && !e.shiftKey && !nt && /^https?:\/\/\S+$/.test(ta.value.trim())) { e.preventDefault(); go.click(); } });
  $('.st-paste', sh).onclick = async () => {
    try { const t = await navigator.clipboard.readText(); if (t) { ta.value = ta.value.trim() ? ta.value.trim() + '\n' + t : t; upd(); haptic(); } else toast('The clipboard is empty'); }
    catch { ta.focus(); toast('Long-press the field and tap Paste'); }
  };
  $('.st-file', sh).onchange = e => {
    const f = e.target.files?.[0]; e.target.value = ''; if (!f) return;
    if (!/^image\//.test(f.type) && !/\.(heic|heif|jpe?g|png|webp|gif)$/i.test(f.name)) { toast('Pick a photo'); return; }
    setPhoto(f);
  };
  $('.st-px', sh).onclick = () => { URL.revokeObjectURL($('img', ph).src); photo = null; ph.hidden = true; ta.placeholder = PH; upd(); };
  $('.mic', sh).onclick = () => {
    // iOS: the file picker must open inside the tap, so pick the path synchronously when the module is loaded
    if (voiceMod) { voiceMod.canRecord() ? voiceMod.record(v) : $('.vfile', sh).click(); return; }
    vm().then(m => m.canRecord() ? m.record(v) : $('.vfile', sh).click(), e => toast('Voice: ' + esc(e.message), 'err'));
  };
  $('.vfile', sh).onchange = e => { const f = e.target.files?.[0]; e.target.value = ''; if (f) vm().then(m => m.transcribe(v, f)); };
  vm().catch(() => { });
  new MutationObserver(ms => { if (ms.some(m => m.target.closest?.('.vnote') || [...m.addedNodes].some(n => n.classList?.contains('vnote')))) upd(); })
    .observe(v.el, { childList: true, subtree: true, attributes: true, attributeFilter: ['class'] });
  if (c.init?.text) ta.value = c.init.text;
  if (c.init?.photo) setPhoto(c.init.photo); else upd();
  go.onclick = async () => {
    const text = ta.value.trim(), note = nt?.value.trim() || '';
    if (!text && !photo && !note) return;
    const voice = !!$('.vnote.ok', sh) && !/https?:\/\//.test(text);
    go.disabled = true; go.classList.add('busy'); go.innerHTML = `<span class="spin"></span> ${c.busy}`; err.hidden = true;
    try {
      const r = await send({ text, photo, voice, note });
      haptic(); sh.close();
      after(r);
    } catch (e) {
      err.textContent = c.fail + e.message; err.hidden = false;
      go.classList.remove('busy'); go.disabled = false; go.textContent = 'Try again';
      return;
    }
  };
  setTimeout(() => ta.focus(), 320);
  return sh;
}

async function postForm(url, fields, photo) {
  const fd = new FormData();
  for (const [k, v] of Object.entries(fields)) if (v) fd.append(k, v);
  if (photo) fd.append('files', photo, photo.name || 'photo.jpg');
  const res = await fetch(url, { method: 'POST', body: fd });
  const r = await res.json().catch(() => null);
  if (!res.ok) throw new Error(r?.error || 'HTTP ' + res.status);
  return r;
}

// What you share plus an optional note of your own, straight to the home agent.
export function sendSheet(init) {
  const name = HOME_NAME(), nonce = 'es' + Date.now().toString(36) + Math.random().toString(36).slice(2, 8);
  const sh = composeSheet({
    cls: 'sharesh', h: 'Send to ' + esc(name), sub: 'Something to build, or to ask about. It goes straight to ' + esc(name) + '.', ev: 'send',
    go: 'Send to ' + esc(name), busy: 'Sending…', fail: 'Not sent: ', how: 'Send from any app’s Share button…',
    note: `Note for ${name} (optional): build this, or what is it?`, noteLabel: `Note for ${name} (optional)`, init,
  }, ({ text, photo, note }) => postForm('/api/send?format=json', { text, note, nonce }, photo),
  r => toast(esc(r.message || `Sent to ${name} ✓`), r.queued ? '' : 'ok', null, r.queued ? 4200 : 2600));
  $('.st-how', sh).onclick = () => shortcutSheet();
  return sh;
}

// The Share-button path: a Shortcut that asks for a note and posts to /api/send. An importable .shortcut file has to
// be signed by Apple's `shortcuts sign` (macOS/iOS only), so it is built by hand once.
function shortcutSheet() {
  const name = esc(HOME_NAME()), url = location.origin + '/api/send?format=text';
  const sh = sheet(`<div class="st sc">
    <h2 class="st-h">Send to ${name} from the Share button</h2>
    <p class="st-sub">A one-time Shortcut, about two minutes. It only works where this server is reachable (your tailnet).</p>
    <ol class="sc-steps">
      <li>Open <b>Shortcuts</b>, tap <b>+</b>, name it <b>Send to ${name}</b>.</li>
      <li>Tap <b>ⓘ</b> → turn on <b>Show in Share Sheet</b>. Receive: <b>URLs, Text, Images, PDFs, Media</b> (or Any).</li>
      <li>Add <b>Ask for Input</b>: Input Type <b>Text</b>, Prompt <b>Note for ${name}</b>, Default Answer left <b>empty</b> (leave it empty and tap Done to send without one).</li>
      <li>Add <b>Get Contents of URL</b>:<br>URL <span class="sc-url"></span><button class="btn sm sc-copy" data-ev="send-shortcut-copy">Copy</button><br>
        Method <b>POST</b> · Request Body <b>Form</b><br>field <code>text</code> (Text) = <b>Shortcut Input</b><br>field <code>file</code> (File) = <b>Shortcut Input</b><br>field <code>note</code> (Text) = <b>Provided Input</b></li>
      <li>Add <b>Show Notification</b> with <b>Contents of URL</b> (it says “Sent to ${name} ✓” or why not).</li>
    </ol>
    <p class="st-sub">Then in Safari, Photos, Voice Memos or Files: Share → <b>Send to ${name}</b>. Voice memos are transcribed on the server (needs whisper.cpp). The note is yours; what you share reaches ${name} marked as untrusted content.</p>
  </div>`, 'sharesh');
  $('.sc-url', sh).textContent = url;
  $('.sc-copy', sh).onclick = async () => { try { await navigator.clipboard.writeText(url); toast('Copied'); } catch { toast(esc(url)); } };
}
