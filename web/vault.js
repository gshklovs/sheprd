// Vault (docs/API.md "Vault"): write-only secrets for agents and other tools (they read ~/.config/sheprd/vault/<name>).
// The phone can paste/replace or delete a value; it never sees one back — only "set · ••••<last 4> · updated <date>".
// A value lives only in its password field until Save: read once, sent, the field cleared and removed. Nothing goes
// to localStorage, drafts, telemetry (the fetch wrapper records method + path pattern only) or feedback notes.
import { $, h, esc, sheet, toast, haptic } from './app.js';

const NAME = /^[a-z0-9_]{1,64}$/;
const HELP = {};
const READONLY = new Set();
const HDR = { 'x-sheprd': 'vault' };

async function call(method, path, body) {
  const o = { method, headers: { ...HDR }, cache: 'no-store', credentials: 'same-origin' };
  if (body !== undefined) { o.headers['content-type'] = 'application/json'; o.body = body; }
  const r = await fetch('/api/vault' + path, o);
  let j = null; try { j = await r.json(); } catch { }
  if (!r.ok) throw new Error(j?.error || 'HTTP ' + r.status);
  return j;
}

function when(ms) {
  if (!ms) return '';
  const d = new Date(ms), now = new Date();
  const day = d.toLocaleDateString(undefined, { month: 'short', day: 'numeric', ...(d.getFullYear() !== now.getFullYear() ? { year: 'numeric' } : {}) });
  return d.toDateString() === now.toDateString() ? 'today ' + d.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' }) : day;
}
function status(s) {
  if (!s.set) return 'not set';
  return ['set', s.tail ? '••••' + esc(s.tail) : '', 'updated ' + when(s.updated)].filter(Boolean).join(' · ');
}

// A password field that no password manager, autocorrect or draft system should touch.
const secretInput = (cls, ph) => `<input class="${cls}" type="password" placeholder="${ph}" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" data-private data-1p-ignore data-lpignore="true" data-form-type="other" enterkeyhint="done">`;

// Settings → Vault: one row with the count, opens the sheet.
export async function settingsPart(box) {
  box.innerHTML = `<h3>Vault</h3><div class="group"><button class="kv vlt-open" data-ev="vault.open" style="width:100%"><span>Secrets</span><b class="vlt-n">… ›</b></button></div>
    <p class="dim small">Write-only: paste a key here and tools on the PC read it. The phone never shows it again.</p>`;
  const n = $('.vlt-n', box);
  const upd = () => call('GET', '').then(r => { n.textContent = `${r.slots.filter(s => s.set).length} of ${r.slots.length} set ›`; }, () => { n.textContent = '›'; });
  upd();
  $('.vlt-open', box).onclick = () => vaultSheet(upd);
}

export function vaultSheet(onClose) {
  const ov = sheet(`<h2>Vault</h2>
    <p class="dim small vlt-lead">Secrets for tools on the PC, one file each in <b>~/.config/sheprd/vault/</b> (owner-only). Values are write-only: you can replace or delete them, never read them back.</p>
    <div class="group vlt-list" data-private><div class="dim pad"><span class="spin"></span></div></div>
    <div class="btns"><button class="btn vlt-add" data-ev="vault.add">Add slot</button></div>`, 'vault');
  const list = $('.vlt-list', ov);
  let open = null; // the slot whose editor is showing (name only)
  const close0 = ov.close;
  ov.close = () => { wipe(); close0(); onClose?.(); };

  // drop every field that could hold a value (on close, after save, when another editor opens)
  function wipe() { ov.querySelectorAll('input[type=password]').forEach(i => { i.value = ''; i.closest('.vlt-ed')?.remove(); }); }

  async function load() {
    try {
      const r = await call('GET', '');
      draw(r.slots);
    } catch (e) { list.innerHTML = `<div class="dim pad">Couldn’t load: ${esc(e.message)}</div>`; }
  }
  function draw(slots) {
    wipe();
    list.innerHTML = slots.map(s => `<div class="vlt-row" data-name="${esc(s.name)}">
        <button class="vlt-h" data-ev="vault.slot"${READONLY.has(s.name) && !s.set ? ' disabled' : ''}>
          <span class="vlt-t"><span class="vlt-name">${esc(s.name)}</span><span class="vlt-st${s.set ? ' on' : ''}">${status(s)}</span></span>
          ${READONLY.has(s.name) && !s.set ? '' : '<span class="vlt-ch">›</span>'}
        </button></div>`).join('');
    list.querySelectorAll('.vlt-row').forEach(row => {
      const s = slots.find(x => x.name === row.dataset.name);
      $('.vlt-h', row).onclick = () => { if (open === s.name) { open = null; wipe(); return; } edit(row, s); };
    });
    if (open) { const row = list.querySelector(`.vlt-row[data-name="${open}"]`); const s = slots.find(x => x.name === open); if (row && s) edit(row, s); else open = null; }
  }

  function edit(row, s) {
    wipe(); open = s.name;
    const ro = READONLY.has(s.name);
    const ed = h(`<div class="vlt-ed">
        <p class="dim small">${esc(HELP[s.name] || 'Custom secret')}</p>
        ${ro ? '' : `<div class="form">${secretInput('vlt-in', s.set ? 'Paste a new value to replace' : 'Paste value')}</div>`}
        <div class="btns">${ro ? '' : '<button class="btn primary vlt-save" data-ev="vault.save" disabled>Save</button>'}
          ${s.set ? '<button class="btn danger vlt-del" data-ev="vault.delete">Delete</button>' : ''}</div></div>`);
    row.append(ed);
    const inp = $('.vlt-in', ed), save = $('.vlt-save', ed);
    if (inp) {
      inp.oninput = () => { save.disabled = !inp.value.trim(); };
      inp.onkeydown = e => { if (e.key === 'Enter') { e.preventDefault(); if (!save.disabled) save.click(); } };
      save.onclick = async () => {
        if (!inp.value.trim()) return;
        save.disabled = true; inp.disabled = true; save.textContent = 'Saving…';
        try {
          await call('PUT', '/' + s.name, JSON.stringify({ value: inp.value }));
          inp.value = ''; haptic(); toast(`Saved ${esc(s.name)}`, 'ok', null, 1500);
          open = null; load();
        } catch (e) {
          inp.disabled = false; save.disabled = !inp.value.trim(); save.textContent = 'Save';
          toast('Not saved: ' + esc(e.message), 'err', null, 4000);
        }
      };
      setTimeout(() => inp.focus(), 50);
    }
    const del = $('.vlt-del', ed);
    if (del) del.onclick = async () => {
      if (!confirm(`Delete ${s.name}?\n\nThe file is removed from the PC; tools that need it will stop working until you paste it again.`)) return;
      del.disabled = true;
      try { await call('DELETE', '/' + s.name); toast(`Deleted ${esc(s.name)}`, 'ok', null, 1500); open = null; load(); }
      catch (e) { del.disabled = false; toast('Not deleted: ' + esc(e.message), 'err', null, 4000); }
    };
  }

  $('.vlt-add', ov).onclick = () => {
    wipe(); open = null;
    list.querySelector('.vlt-new')?.remove();
    const row = h(`<div class="vlt-row vlt-new"><div class="vlt-ed">
        <div class="form"><input class="vlt-nm" placeholder="slot_name (a-z, 0-9, _)" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" maxlength="64"></div>
        <p class="dim small vlt-nerr" hidden></p>
        <div class="form">${secretInput('vlt-in', 'Paste value')}</div>
        <div class="btns"><button class="btn primary vlt-save" data-ev="vault.save" disabled>Save</button><button class="btn vlt-cancel" data-ev="vault.cancel">Cancel</button></div></div></div>`);
    list.append(row);
    const nm = $('.vlt-nm', row), inp = $('.vlt-in', row), save = $('.vlt-save', row), err = $('.vlt-nerr', row);
    const taken = () => !!list.querySelector(`.vlt-row:not(.vlt-new)[data-name="${nm.value}"]`);
    const upd = () => {
      const bad = nm.value && !NAME.test(nm.value) ? 'Use a–z, 0–9 and _ only (up to 64).' : taken() ? 'That slot exists: tap it to replace.' : nm.value === 'request' ? 'Reserved.' : '';
      err.hidden = !bad; err.textContent = bad;
      save.disabled = !!bad || !nm.value || !inp.value.trim();
    };
    nm.oninput = () => { nm.value = nm.value.toLowerCase().replace(/[\s-]/g, '_'); upd(); };
    inp.oninput = upd;
    $('.vlt-cancel', row).onclick = () => { inp.value = ''; row.remove(); };
    save.onclick = async () => {
      upd(); if (save.disabled) return;
      save.disabled = true; inp.disabled = true; save.textContent = 'Saving…';
      try {
        await call('PUT', '/' + nm.value, JSON.stringify({ value: inp.value }));
        inp.value = ''; haptic(); toast(`Saved ${esc(nm.value)}`, 'ok', null, 1500);
        row.remove(); load();
      } catch (e) {
        inp.disabled = false; save.textContent = 'Save'; upd();
        toast('Not saved: ' + esc(e.message), 'err', null, 4000);
      }
    };
    setTimeout(() => nm.focus(), 50);
  };

  load();
  return ov;
}
