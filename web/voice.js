// Voice notes (lazy): record on the phone, transcribe on the box (POST /api/voice → whisper.cpp),
// land the text in the composer for review. Never sends anything by itself.
// Capture paths, best first:
//   1. MediaRecorder — only in a secure context (https via tailscale serve, or localhost).
//   2. File input (plain http): iOS offers "Take Video" (mic audio, video dropped on the box),
//      "Choose Files" (a Voice Memos recording saved to Files) or the photo library.
// The mic button in app.js picks the path synchronously (iOS needs the tap for input.click()).
import { esc, toast, haptic, insertAt } from './app.js';

await new Promise(r => {
  if (document.querySelector('link[data-voice]')) return r();
  const l = document.createElement('link'); l.rel = 'stylesheet'; l.href = 'voice.css'; l.dataset.voice = 1;
  l.onload = l.onerror = r; document.head.append(l); setTimeout(r, 3000);
});

const MAX_MS = 5 * 60e3;
export const canRecord = () => !!(window.isSecureContext && window.MediaRecorder && navigator.mediaDevices?.getUserMedia);
const mmss = s => { s = Math.max(0, Math.round(s)); return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`; };
const mic = '<svg viewBox="0 0 24 24"><rect x="9" y="3" width="6" height="11" rx="3"/><path d="M5.5 11a6.5 6.5 0 0 0 13 0M12 17.5V21"/></svg>';

// one panel per chat view, above the composer row
function panel(v) {
  let p = v.el.querySelector('.vnote');
  if (!p) { p = document.createElement('div'); p.className = 'vnote'; p.hidden = true; v.el.querySelector('.crow').before(p); }
  return p;
}
const S = new WeakMap(); // view → {rec, stream, timer, xhr, blob, name, inserted, watch}
const st = v => { let s = S.get(v); if (!s) S.set(v, s = {}); return s; };

function show(v, cls, html, on = {}) {
  const p = panel(v);
  p.className = 'vnote ' + cls; p.innerHTML = html; p.hidden = false;
  p.onclick = e => { const b = e.target.closest('[data-v]'); if (b && on[b.dataset.v]) on[b.dataset.v](); };
  v.el.querySelector('.mic')?.classList.toggle('on', cls === 'rec');
  return p;
}
function hide(v) {
  const s = st(v); clearInterval(s.timer); clearInterval(s.watch);
  const p = v.el.querySelector('.vnote'); if (p) { p.hidden = true; p.innerHTML = ''; }
  v.el.querySelector('.mic')?.classList.remove('on');
}
const busy = v => { const s = st(v); return !!(s.rec || s.xhr); };

// Mic tap in a secure context: start; tap again (or Done) to stop.
export async function record(v) {
  const s = st(v);
  if (s.rec) return stop(v);
  if (s.xhr) return;
  let stream;
  try { stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true } }); }
  catch (e) { toast('Microphone unavailable: ' + esc(e.message || e.name), 'err'); return; }
  const type = ['audio/mp4', 'audio/webm;codecs=opus', 'audio/webm', 'audio/ogg'].find(t => MediaRecorder.isTypeSupported?.(t)) || '';
  const rec = new MediaRecorder(stream, type ? { mimeType: type } : undefined), chunks = [];
  Object.assign(s, { rec, stream, cancel: false });
  rec.ondataavailable = e => e.data.size && chunks.push(e.data);
  rec.onstop = () => {
    stream.getTracks().forEach(t => t.stop());
    s.rec = null; clearInterval(s.timer);
    if (s.cancel) { hide(v); return; }
    const mt = rec.mimeType || type || 'audio/mp4';
    const blob = new Blob(chunks, { type: mt });
    send(v, blob, 'note.' + (mt.includes('webm') ? 'webm' : mt.includes('ogg') ? 'ogg' : 'm4a'), (Date.now() - t0) / 1000);
  };
  const t0 = Date.now();
  rec.start(1000); haptic?.();
  const draw = () => {
    const el = v.el.querySelector('.vnote .vt'); const ms = Date.now() - t0;
    if (el) el.textContent = mmss(ms / 1000);
    if (ms > MAX_MS) stop(v);
  };
  show(v, 'rec', `<span class="vdot"></span><span class="vt">0:00</span><span class="vl">Recording</span><span class="sp"></span>
    <button class="btn sm" data-v="cancel">Cancel</button><button class="btn sm primary" data-v="stop">Done</button>`,
    { cancel: () => { s.cancel = true; rec.stop(); }, stop: () => stop(v) });
  s.timer = setInterval(draw, 250);
}
function stop(v) { const s = st(v); if (s.rec?.state === 'recording') { s.rec.stop(); haptic?.(); } }

// A picked/recorded file (from the hidden input in app.js).
export function transcribe(v, file) {
  if (!file) return;
  if (busy(v)) { toast('Still working on the last note'); return; }
  send(v, file, file.name || 'note', null);
}

function send(v, blob, name, secs) {
  const s = st(v); Object.assign(s, { blob, name });
  const pane = v.pane || v.agent?.pane || '';
  const len = secs ? mmss(secs) + ' ' : '';
  const xhr = s.xhr = new XMLHttpRequest();
  const t0 = Date.now();
  const p = show(v, 'up', `<span class="dspin"></span><span class="vl">Uploading ${len}note…</span><span class="vp"></span><span class="sp"></span>
    <button class="btn sm" data-v="cancel">Cancel</button>`, { cancel: () => xhr.abort() });
  xhr.upload.onprogress = e => { const el = p.querySelector('.vp'); if (el && e.lengthComputable) el.textContent = Math.round(e.loaded / e.total * 100) + '%'; };
  xhr.upload.onload = () => {
    if (s.xhr !== xhr) return;
    const big = blob.size > 4e6 ? ' (audio only is kept)' : '';
    show(v, 'tx', `<span class="dspin"></span><span class="vl">Transcribing ${len}note on the box…${big}</span><span class="vt"></span><span class="sp"></span>
      <button class="btn sm" data-v="cancel">Cancel</button>`, { cancel: () => xhr.abort() });
    const t1 = Date.now();
    clearInterval(s.timer);
    s.timer = setInterval(() => { const el = v.el.querySelector('.vnote .vt'); if (el) el.textContent = Math.round((Date.now() - t1) / 1000) + ' s'; }, 500);
  };
  xhr.onload = () => {
    s.xhr = null; clearInterval(s.timer);
    let j = null; try { j = JSON.parse(xhr.responseText); } catch { }
    if (xhr.status !== 200 || !j) return fail(v, j?.error || `HTTP ${xhr.status}`);
    const d = j.duration ?? secs ?? 0;
    if (!j.text) return fail(v, j.empty ? `No speech heard in the ${mmss(d)} note` : 'Nothing transcribed', true);
    land(v, j.text, d, (Date.now() - t0) / 1000);
  };
  xhr.onerror = () => { s.xhr = null; fail(v, 'Upload failed — is the box reachable?'); };
  xhr.onabort = () => { s.xhr = null; hide(v); };
  xhr.open('POST', `/api/voice?pane=${encodeURIComponent(pane)}&name=${encodeURIComponent(name)}`);
  xhr.setRequestHeader('content-type', blob.type || 'application/octet-stream');
  xhr.send(blob);
}

function fail(v, msg, empty) {
  const s = st(v);
  show(v, 'err', `<span class="vl">${esc(msg)}</span><span class="sp"></span>
    ${empty ? '' : '<button class="btn sm" data-v="retry">Retry</button>'}<button class="btn sm" data-v="again">${mic}Re-record</button><button class="btn sm" data-v="x">✕</button>`,
    { retry: () => s.blob && send(v, s.blob, s.name, null), again: () => again(v), x: () => hide(v) });
}

// Transcript → composer (at the cursor), editable; a chip offers re-record / undo.
function land(v, text, secs, took) {
  const s = st(v), ta = v.ta;
  insertAt(ta, text); v.autosize?.();
  s.inserted = text; haptic?.();
  show(v, 'ok', `<span class="vok">${mic}</span><span class="vl">${mmss(secs)} note · ${took.toFixed(1)} s</span><span class="sp"></span>
    <button class="btn sm" data-v="again">Re-record</button><button class="btn sm" data-v="x">✕</button>`,
    { again: () => again(v), x: () => hide(v) });
  // the chip goes away once the text is sent or edited away
  clearInterval(s.watch);
  s.watch = setInterval(() => { if (!v.el.isConnected || !ta.value.includes(s.inserted)) hide(v); }, 1000);
}

// Drop the landed transcript (if still there unchanged) and capture again.
// Must stay synchronous up to input.click() for iOS.
function again(v) {
  const s = st(v), ta = v.ta;
  if (s.inserted && ta.value.includes(s.inserted)) {
    ta.value = ta.value.replace(s.inserted, '').replace(/[ \t]{2,}/g, ' ').replace(/^\s+|\s+$/g, '');
    ta.dispatchEvent(new Event('input'));
  }
  s.inserted = null; hide(v);
  if (canRecord()) record(v); else v.el.querySelector('.vfile')?.click();
}
