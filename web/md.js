// Small, safe markdown + linkifier. Everything is HTML-escaped; only our own tags are emitted.
const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
export const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ESC[c]);

// link context, set by the app from /api/state. paneLabel(p) → display label for a herdr pane id.
export const L = { ts: null, home: '/home', base: null, paneLabel: p => p, isPane: p => /^w\d+:p\d+$/.test(p) };

const LOCAL = /^https?:\/\/(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\])(?=[:/?#]|$)/i;
export const fixUrl = u => (L.ts ? u.replace(LOCAL, 'http://' + L.ts) : u);

export const IMG_EXT = /\.(png|jpe?g|gif|webp|svg|bmp|avif|heic)$/i;
// video/audio play inline (<video>/<audio>, server answers Range: ranged.rs) and open the app's player, never a raw tab
export const VID_EXT = /\.(mp4|m4v|mov|webm)$/i;
export const AUD_EXT = /\.(mp3|m4a|aac|wav|ogg|oga|opus|flac)$/i;
export const isMedia = u => { let p = String(u || ''); const q = p.match(/[?&]path=([^&#]+)/); if (q) try { p = decodeURIComponent(q[1]); } catch { } return VID_EXT.test(p.split(/[?#]/)[0]) ? 'video' : AUD_EXT.test(p.split(/[?#]/)[0]) ? 'audio' : null; };
const BIN_EXT = /\.(pdf|zip|gz|tgz|xz|bz2|zst|7z|tar|mkv|bin|so|o|a|exe|whl|pt|pth|onnx|npz|npy|pkl|db|sqlite|ttf|otf|woff2?|ico|xlsx?|docx?|pptx?)$/i;
const absPath = p => p.replace(/^~(?=\/|$)/, L.home);
export const fileUrl = p => '/api/file?path=' + encodeURIComponent(absPath(p));
export const docUrl = p => '#/f?path=' + encodeURIComponent(absPath(p));

// URL | (prefix)(absolute or ~ path under a known root) | herdr pane id (only when ctx.panes)
const LINK = /(https?:\/\/[^\s<>"'`\u0000]+)|(^|[\s(\[{"'=,>:])((?:~|\/(?:home|tmp|etc|usr|var|opt|srv|mnt|root|run|nix|proc|data))(?:\/[^\s<>"'`()\[\]{},;|*:\u0000]+)+)/g;
const PANE = 'w\\d+:p[0-9A-Za-z]{1,3}';
const LINKP = new RegExp(LINK.source + '|\\b(' + PANE + ')\\b', 'g');

function trimUrl(u) {
  u = u.replace(/[.,;:!?*~]+$/, '');
  // drop unbalanced trailing ) or ]
  while (/[)\]]$/.test(u)) {
    const o = u.endsWith(')') ? '(' : '[', c = u.slice(-1);
    if (u.split(o).length >= u.split(c).length) break;
    u = u.slice(0, -1);
  }
  return u;
}

export const aUrl = (u, text) =>
  `<a href="${esc(fixUrl(u))}" target="_blank" rel="noopener">${text ?? esc(u)}</a>`;

// local path → image (lightbox), binary (raw, new tab) or doc viewer link
function pathA(p, ctx, inner) {
  if (IMG_EXT.test(p)) {
    const href = fileUrl(p);
    if (ctx) ctx.imgs.add(href);
    return `<a href="${esc(href)}" class="path pimg">${inner}</a>`;
  }
  if (VID_EXT.test(p) || AUD_EXT.test(p)) {
    const href = fileUrl(p);
    if (ctx) ctx.imgs.add(href);
    return `<a href="${esc(href)}" class="path pvid">${inner}</a>`;
  }
  if (BIN_EXT.test(p)) return `<a href="${esc(fileUrl(p))}" target="_blank" rel="noopener" class="path">${inner}</a>`;
  return `<a href="${esc(docUrl(p))}" class="path">${inner}</a>`;
}
function aPath(p, ctx, text) {
  p = p.replace(/[.,;!?]+$/, '');
  return [pathA(p, ctx, text ?? esc(p)), p];
}
const paneA = p => L.isPane(p) ? `<a class="pchip" data-pane="${esc(p)}" href="#/a/${encodeURIComponent(p)}"><span class="pcl">${esc(L.paneLabel(p))}</span></a>` : esc(p);

// pane ids mentioned in transcript items (text, tool summaries, results), most recent mention first
const PANE_G = new RegExp('\\b' + PANE + '\\b', 'g');
export function panesMentioned(items, max = 300) {
  const out = [];
  for (let k = items.length - 1; k >= 0 && k >= items.length - max; k--) {
    const it = items[k];
    const t = it.k === 'text' ? it.text : it.k === 'tool' ? it.summary : it.k === 'result' ? it.text : null;
    if (!t) continue;
    const found = String(t).match(PANE_G);
    if (found) for (let j = found.length - 1; j >= 0; j--) if (!out.includes(found[j])) out.push(found[j]);
  }
  return out;
}

// Linkify plain text (no markdown): escape everything, wrap URLs, paths (and pane ids if ctx.panes).
export function linkify(s, ctx) {
  s = String(s ?? '');
  let out = '', last = 0;
  for (const m of s.matchAll(ctx?.panes ? LINKP : LINK)) {
    out += esc(s.slice(last, m.index));
    if (m[1]) {
      const u = trimUrl(m[1]);
      out += aUrl(u) + esc(m[1].slice(u.length));
    } else if (m[4]) out += paneA(m[4]);
    else {
      const [a, p] = aPath(m[3], ctx);
      out += esc(m[2]) + a + esc(m[3].slice(p.length));
    }
    last = m.index + m[0].length;
  }
  return out + esc(s.slice(last));
}

function resolveRel(u) {
  if (!L.base || /^[a-z][\w+.-]*:/i.test(u) || u.startsWith('#')) return null;
  const parts = (L.base + '/' + u.replace(/[?#].*$/, '')).split('/'), out = [];
  for (const x of parts) { if (x === '..') out.pop(); else if (x && x !== '.') out.push(x); }
  return '/' + out.join('/');
}

// ---------- inline ----------
function inl(s, ctx) {
  const ph = [];
  const put = x => '\u0000' + (ph.push(x) - 1) + '\u0001';
  s = s.replace(/(`+)([^`]|[^`][\s\S]*?[^`])\1(?!`)/g, (m, b, c) => {
    c = c.replace(/^ (.*) $/, '$1');
    const t = c.trim();
    if (/^https?:\/\/\S+$/.test(t)) return put(aUrl(t, `<code>${esc(c)}</code>`));
    if (/^(~|\/(home|tmp|etc|usr|var|opt|srv|mnt|root|run|nix|data))\/\S+$/.test(t) && !/[*]/.test(t))
      return put(aPath(t.replace(/:\d+(:\d+)?$/, ''), ctx, `<code>${esc(c)}</code>`)[0]);
    if (ctx?.panes && new RegExp('^' + PANE + '$').test(t)) return put(paneA(t));
    return put(`<code>${esc(c)}</code>`);
  });
  s = s.replace(/(!?)\[([^\]\n]*)\]\(<?([^)\s>]+)>?(?:\s+"[^"]*")?\)/g, (m, bang, t, u) => {
    let local = /^(~|\/)/.test(u) && !u.startsWith('/api/') ? u : null;
    if (!local && !/^(https?:|\/api\/|#)/.test(u)) local = resolveRel(u);
    if (local) {
      if (bang) return put(isMedia(local) ? mediaEl(fileUrl(local)) : `<img class="thumb" loading="lazy" alt="${esc(t)}" src="${esc(fileUrl(local))}">`);
      return put(pathA(local, ctx, esc(t) || esc(u)));
    }
    if (!/^(https?:|\/api\/|#)/.test(u)) return put(esc(m));
    const href = fixUrl(u);
    if (bang) return put(`<img class="thumb" loading="lazy" alt="${esc(t)}" src="${esc(href)}">`);
    return put(`<a href="${esc(href)}"${u[0] === '#' ? '' : ' target="_blank" rel="noopener"'}>${esc(t) || esc(u)}</a>`);
  });
  s = s.replace(ctx?.panes ? LINKP : LINK, (m, url, pre, p, pane) => {
    if (url) { const u = trimUrl(url); return put(aUrl(u)) + url.slice(u.length); }
    if (typeof pane === 'string') return put(paneA(pane)); // (without the pane group this argument is the match offset)
    const [a, pp] = aPath(p, ctx);
    return pre + put(a) + p.slice(pp.length);
  });
  s = esc(s)
    .replace(/\*\*(?=\S)([\s\S]*?\S)\*\*/g, '<b>$1</b>')
    .replace(/(^|[^\w])__(?=\S)([\s\S]*?\S)__(?!\w)/g, '$1<b>$2</b>')
    .replace(/(^|[^\w*])\*(?=[^\s*])([^*\n]*?[^\s*])\*(?![\w*])/g, '$1<i>$2</i>')
    .replace(/(^|[^\w])_(?=[^\s_])([^_\n]*?[^\s_])_(?!\w)/g, '$1<i>$2</i>')
    .replace(/~~(?=\S)([\s\S]*?\S)~~/g, '<s>$1</s>');
  return s.replace(/\u0000(\d+)\u0001/g, (m, n) => ph[n]);
}

// ---------- blocks ----------
const RE_FENCE = /^(\s*)(`{3,}|~{3,})\s*([^`\s]*)/;
const RE_LIST = /^(\s*)([-*+]|\d{1,9}[.)])(\s+|$)/;
const RE_HR = /^\s{0,3}([-*_])(\s*\1){2,}\s*$/;
const RE_H = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
const RE_SEP = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;
const isBlockStart = (l, n) =>
  RE_FENCE.test(l) || RE_H.test(l) || RE_HR.test(l) || /^\s*>/.test(l) || RE_LIST.test(l) ||
  (l.includes('|') && n !== undefined && RE_SEP.test(n) && n.includes('-'));

const cells = l => {
  l = l.trim().replace(/^\|/, '').replace(/\|$/, '');
  return l.split(/(?<!\\)\|/).map(c => c.trim().replace(/\\\|/g, '|'));
};

export function md(src, ctx, depth = 0) {
  const lines = String(src ?? '').replace(/\r\n?/g, '\n').split('\n');
  let out = '', i = 0;
  while (i < lines.length) {
    const l = lines[i];
    let m;
    if ((m = l.match(RE_FENCE))) {
      const fc = m[2][0], fl = m[2].length, ind = m[1].length, buf = [];
      const close = new RegExp(`^\\s*${fc === '`' ? '`' : '~'}{${fl},}\\s*$`);
      i++;
      while (i < lines.length && !close.test(lines[i])) buf.push(lines[i++].slice(Math.min(ind, lines[i - 1].search(/\S|$/))));
      i++;
      out += codeHTML(buf.join('\n'), m[3]);
      continue;
    }
    if (!l.trim()) { i++; continue; }
    if ((m = l.match(RE_H))) {
      const n = Math.min(6, m[1].length + 1);
      out += `<h${n}>${inl(m[2], ctx)}</h${n}>`; i++; continue;
    }
    if (RE_HR.test(l)) { out += '<hr>'; i++; continue; }
    if (/^\s*>/.test(l)) {
      const buf = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) buf.push(lines[i++].replace(/^\s*> ?/, ''));
      out += `<blockquote>${depth < 8 ? md(buf.join('\n'), ctx, depth + 1) : esc(buf.join('\n'))}</blockquote>`;
      continue;
    }
    if (l.includes('|') && i + 1 < lines.length && RE_SEP.test(lines[i + 1]) && lines[i + 1].includes('-')) {
      const head = cells(l), al = cells(lines[i + 1]).map(c => (/^:-+:$/.test(c) ? 'center' : /-:$/.test(c) ? 'right' : ''));
      i += 2;
      const st = k => (al[k] ? ` style="text-align:${al[k]}"` : '');
      let t = '<div class="tbl"><table><thead><tr>' + head.map((c, k) => `<th${st(k)}>${inl(c, ctx)}</th>`).join('') + '</tr></thead><tbody>';
      while (i < lines.length && lines[i].includes('|') && lines[i].trim()) {
        t += '<tr>' + cells(lines[i++]).map((c, k) => `<td${st(k)}>${inl(c, ctx)}</td>`).join('') + '</tr>';
      }
      out += t + '</tbody></table></div>';
      continue;
    }
    if ((m = l.match(RE_LIST))) {
      const base = m[1].length, ordered = /\d/.test(m[2]);
      const items = [];
      let cur = null;
      while (i < lines.length) {
        const x = lines[i];
        const lm = x.match(RE_LIST);
        if (lm && lm[1].length <= base + 1 && /\d/.test(lm[2]) === ordered) {
          cur = { lines: [x.slice(lm[0].length)], ind: lm[0].length, start: parseInt(lm[2]) };
          items.push(cur); i++; continue;
        }
        if (!x.trim()) {
          // blank: continue only if next non-blank line is indented or another item
          let j = i + 1;
          while (j < lines.length && !lines[j].trim()) j++;
          if (j < lines.length && (lines[j].search(/\S/) > base || (RE_LIST.test(lines[j]) && lines[j].search(/\S/) <= base + 1))) {
            cur.lines.push(''); i++; continue;
          }
          break;
        }
        const ind = x.search(/\S/);
        if (ind > base) { cur.lines.push(x.slice(Math.min(ind, cur.ind))); i++; continue; }
        if (isBlockStart(x, lines[i + 1])) break;
        cur.lines.push(x.trim()); i++; // lazy continuation
      }
      const tag = ordered ? 'ol' : 'ul';
      const startAttr = ordered && items[0].start !== 1 ? ` start="${items[0].start}"` : '';
      out += `<${tag}${startAttr}>` + items.map(it => {
        let body = it.lines.join('\n');
        let task = '';
        const tm = body.match(/^\[([ xX])\]\s+/);
        if (tm) { task = `<span class="task${tm[1] === ' ' ? '' : ' on'}"></span>`; body = body.slice(tm[0].length); }
        return `<li>${task}${depth < 8 ? md(body, ctx, depth + 1) : esc(body)}</li>`;
      }).join('') + `</${tag}>`;
      continue;
    }
    const buf = [l];
    i++;
    while (i < lines.length && lines[i].trim() && !isBlockStart(lines[i], lines[i + 1])) buf.push(lines[i++]);
    out += `<p>${inl(buf.join('\n'), ctx).replace(/\n/g, L.base ? ' ' : '<br>')}</p>`;
  }
  return out;
}

// an inline player: plays in place (playsinline), first frame as the poster (#t=0.001 makes iOS draw it), ⤢ = the
// fullscreen player with Save (app.js mediaViewer)
export const mediaEl = u => isMedia(u) === 'audio'
  ? `<div class="vthumb aud"><audio controls preload="metadata" src="${esc(u)}"></audio><button class="vopen" data-media="${esc(u)}" aria-label="Open player">⤢</button></div>`
  : `<div class="vthumb"><video class="vid" controls playsinline preload="metadata" src="${esc(u)}#t=0.001"></video><button class="vopen" data-media="${esc(u)}" aria-label="Open player">⤢</button></div>`;
export const thumbs = urls =>
  urls && (urls.length || urls.size) ? `<div class="thumbs">${[...urls].map(u => isMedia(u) ? mediaEl(u) : `<img class="thumb" loading="lazy" src="${esc(u)}" alt="">`).join('')}</div>` : '';

// markdown with image-path thumbnails appended
export function mdx(src, panes) {
  const ctx = { imgs: new Set(), panes };
  return md(src, ctx) + thumbs(ctx.imgs);
}
export function textx(src, panes) {
  const ctx = { imgs: new Set(), panes };
  return [linkify(src, ctx), ctx.imgs];
}
export const pathLink = (p, label) => pathA(p, null, esc(label ?? p));

// A code block with Copy / Terminal buttons; each line is its own tap target
// (tap = copy that line). The raw text rides in data-raw for exact copying.
export function codeHTML(text, lang = '', cls = 'code') {
  const lines = String(text ?? '').replace(/\n+$/, '').split('\n');
  return `<div class="cb"><div class="cb-bar"><span class="cb-lang">${esc(lang || '')}</span><button class="cb-copy" type="button">Copy</button><button class="cb-term" type="button">Terminal</button></div><pre class="${cls}" data-raw="${esc(lines.join('\n'))}"${lang ? ` data-lang="${esc(lang)}"` : ''}><code>${lines.map(l => `<span class="ln">${linkify(l, null) || ' '}</span>`).join('\n')}</code></pre></div>`;
}

export const IOS = typeof navigator !== 'undefined'
  && (/iP(hone|ad|od)/.test(navigator.userAgent) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1));

