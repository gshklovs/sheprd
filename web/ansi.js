// ANSI SGR → HTML (escaped). Handles 16/256/truecolor fg/bg, bold, dim, italic, underline, inverse.
import { esc } from './md.js';

const P16_ = ['#3a3a3c', '#ff5f57', '#32d74b', '#e5c07b', '#4f9dff', '#d17cf5', '#5ad1d8', '#d6d6d6',
  '#7c7c80', '#ff7b72', '#5af78e', '#f4f99d', '#7cc4ff', '#ff8ad8', '#9aedfe', '#ffffff'];
// Omarchy looks (theme.js) supply the desktop terminal's 16 colours as --ansi-N.
const P16 = P16_.map((c, i) => `var(--ansi-${i}, ${c})`);
const LV = [0, 95, 135, 175, 215, 255];
function c256(n) {
  if (n < 16) return P16[n];
  if (n < 232) { n -= 16; return `rgb(${LV[(n / 36) | 0]},${LV[((n / 6) | 0) % 6]},${LV[n % 6]})`; }
  const g = 8 + 10 * (n - 232); return `rgb(${g},${g},${g})`;
}

const RE = /\x1b\[([\d;:?]*)([A-Za-z@`])|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?|\x1b[()][\w]|\x1b[=>78DEHMc]|[\x00-\x08\x0b-\x1f\x7f]/g;

export function stripAnsi(s) { return String(s ?? '').replace(RE, ''); }

export function ansi(s) {
  s = String(s ?? '').replace(/\r\n?/g, '\n');
  let fg = null, bg = null, b = 0, d = 0, it = 0, u = 0, inv = 0;
  let out = '', open = '', last = 0;
  const style = () => {
    let f = fg, g = bg;
    if (inv) { f = bg || 'var(--term-bg)'; g = fg || 'var(--term-fg)'; }
    let st = '';
    if (f) st += `color:${f};`;
    if (g) st += `background:${g};`;
    if (b) st += 'font-weight:700;';
    if (d) st += 'opacity:.6;';
    if (it) st += 'font-style:italic;';
    if (u) st += 'text-decoration:underline;';
    return st;
  };
  const text = t => {
    if (!t) return;
    const st = style();
    if (st !== open) { if (open) out += '</span>'; if (st) out += `<span style="${st}">`; open = st; }
    out += esc(t);
  };
  for (const m of s.matchAll(RE)) {
    text(s.slice(last, m.index));
    last = m.index + m[0].length;
    if (m[2] !== 'm') continue;
    const p = m[1] === '' ? [0] : m[1].split(/[;:]/).map(x => +x || 0);
    for (let k = 0; k < p.length; k++) {
      const n = p[k];
      if (n === 0) { fg = bg = null; b = d = it = u = inv = 0; }
      else if (n === 1) b = 1; else if (n === 2) d = 1; else if (n === 3) it = 1; else if (n === 4) u = 1;
      else if (n === 7) inv = 1; else if (n === 22) b = d = 0; else if (n === 23) it = 0; else if (n === 24) u = 0;
      else if (n === 27) inv = 0;
      else if (n >= 30 && n <= 37) fg = P16[n - 30]; else if (n === 39) fg = null;
      else if (n >= 40 && n <= 47) bg = P16[n - 40]; else if (n === 49) bg = null;
      else if (n >= 90 && n <= 97) fg = P16[n - 82]; else if (n >= 100 && n <= 107) bg = P16[n - 92];
      else if (n === 38 || n === 48) {
        let c = null;
        if (p[k + 1] === 5) { c = c256(p[k + 2] & 255); k += 2; }
        else if (p[k + 1] === 2) { c = `rgb(${p[k + 2] & 255},${p[k + 3] & 255},${p[k + 4] & 255})`; k += 4; }
        if (n === 38) fg = c; else bg = c;
      }
    }
  }
  text(s.slice(last));
  if (open) out += '</span>';
  return out;
}
