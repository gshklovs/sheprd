// Looks: "ios" (style.css as is), "ipal" (the iOS look in an Omarchy palette: same system font, sizes,
// rounded grouped panels; colours from the palette, held at AAA body / AA secondary contrast), "desk"
// (Hyprland-style square, mono chrome, in the palette) and "site" (the omarchy.org website's design
// language). The palette (Settings → Look → Palette, `mm-pal`) is "desktop" (follows omarchy-theme-set,
// GET /api/theme), "auto" (one Omarchy theme by day, one by night, following iOS appearance) or a fixed
// installed theme by name (GET /api/theme?name=). Classic script, loaded in <head> before
// the first paint: it applies the cached palette synchronously so there is no flash, then refreshes
// from the server. theme.css holds the structure (corners, borders, fonts); this file only computes
// colour tokens and sets them inline on <html>, which beats style.css's :root and its light-mode block.
(function () {
  'use strict';
  var KL = 'mm-look', KT = 'mm-omtheme', KP = 'mm-pal', KPT = 'mm-pt:', KAL = 'mm-pal-day', KAD = 'mm-pal-night', KTL = 'mm-themes', DEF = 'desk';
  var AUTO_DAY = 'flexoki-light', AUTO_NIGHT = 'tokyo-night';
  var LOOKS = [['ipal', 'iOS + Omarchy palette'], ['ios', 'iOS (original)'], ['desk', 'Omarchy desktop'], ['site', 'omarchy.org']];
  var PAL_LOOKS = { ipal: 1, desk: 1 };
  var root = document.documentElement;
  function get(k) { try { return localStorage.getItem(k); } catch (e) { return null; } }
  function set(k, v) { try { localStorage.setItem(k, v); } catch (e) { } }

  // ---------- colour math ----------
  function rgb(h) {
    h = String(h || '').replace('#', '');
    if (h.length === 3) h = h.replace(/./g, '$&$&');
    var n = parseInt(h.slice(0, 6), 16);
    return isNaN(n) ? null : [n >> 16 & 255, n >> 8 & 255, n & 255];
  }
  function hex(c) { return '#' + c.map(function (x) { x = Math.max(0, Math.min(255, Math.round(x))); return (x < 16 ? '0' : '') + x.toString(16); }).join(''); }
  function mix(a, b, t) { var x = rgb(a), y = rgb(b); return hex([0, 1, 2].map(function (i) { return x[i] * t + y[i] * (1 - t); })); } // t of a
  function alpha(a, t) { return hex(rgb(a)) + ('0' + Math.round(t * 255).toString(16)).slice(-2); }
  function lum(h) { var c = rgb(h).map(function (v) { v /= 255; return v <= .03928 ? v / 12.92 : Math.pow((v + .055) / 1.055, 2.4); }); return .2126 * c[0] + .7152 * c[1] + .0722 * c[2]; }
  function cr(a, b) { var x = lum(a), y = lum(b); return (Math.max(x, y) + .05) / (Math.min(x, y) + .05); }
  function hsl(h) {
    var c = rgb(h).map(function (v) { return v / 255; }), mx = Math.max.apply(0, c), mn = Math.min.apply(0, c), l = (mx + mn) / 2, d = mx - mn, s = 0, H = 0;
    if (d) {
      s = d / (1 - Math.abs(2 * l - 1));
      H = mx === c[0] ? ((c[1] - c[2]) / d + 6) % 6 : mx === c[1] ? (c[2] - c[0]) / d + 2 : (c[0] - c[1]) / d + 4;
    }
    return [H * 60, s, l];
  }
  function hueDist(a, b) { var d = Math.abs(hsl(a)[0] - hsl(b)[0]) % 360; return d > 180 ? 360 - d : d; }
  // Nudge c toward the far end (white on dark, black on light) until it reads at ratio `min` on `on`.
  function ensure(c, on, min) {
    var far = lum(on) < .2 ? '#ffffff' : '#000000', t = 0, x = c;
    while (cr(x, on) < min && t < 1) { t += .04; x = mix(far, c, t); }
    return x;
  }
  function low(c, ons) { return Math.min.apply(0, ons.map(function (o) { return cr(c, o); })); }
  // ensure() against several surfaces at once (the page and the cards)
  function ensureAll(c, ons, min) {
    var dk = ons.every(function (o) { return lum(o) < .2; }), far = dk ? '#ffffff' : '#000000', t = 0, x = c;
    while (low(x, ons) < min && t < 1) { t += .03; x = mix(far, c, t); }
    return x;
  }
  function best(on, list) { return list.reduce(function (a, b) { return cr(b, on) > cr(a, on) ? b : a; }); }

  // ---------- palettes → tokens ----------
  // omarchy.org (tokyo-night = the site's :root default; flexoki-light for iOS light mode)
  var SITE = {
    dark: { mode: 'dark', name: 'omarchy.org', s: { deep: '#13141c', bg: '#1a1b26', surf: '#1f2230', surf2: '#24283b', sub: '#24283b', strong: '#414868', text: '#c0caf5', text2: '#a9b1d6', muted: '#8b93b8', brand: '#9ece6a', ink: '#0c0e10' },
      colors: { background: '#1a1b26', foreground: '#c0caf5', accent: '#9ece6a', red: '#f7768e', orange: '#ff9e64', yellow: '#e0af68', green: '#73daca', cyan: '#7dcfff', blue: '#7aa2f7', magenta: '#bb9af7', bright_magenta: '#ff7eb6', darker_background: '#13141c', muted: '#565f89' }, border: { colors: ['#9ece6a'] } },
    light: { mode: 'light', name: 'omarchy.org', s: { deep: '#f2efe4', bg: '#fffcf0', surf: '#f9f6ea', surf2: '#f2efe4', sub: '#e5e2d8', strong: '#b7b5ac', text: '#100f0f', text2: '#454340', muted: '#555450', brand: '#205ea6', ink: '#ffffff' },
      colors: { background: '#fffcf0', foreground: '#100f0f', accent: '#205ea6', red: '#d14d41', orange: '#d0772b', yellow: '#ad8301', green: '#66800b', cyan: '#24837b', blue: '#4385be', magenta: '#ce5d97', bright_magenta: '#a02f6f', dark_background: '#f2efe4', muted: '#878580' }, border: { colors: ['#205ea6'] } }
  };
  var AG = ['red', 'orange', 'yellow', 'green', 'cyan', 'blue', 'purple', 'pink'];

  // kind: "desk" (flat, square), "site" (omarchy.org tokens) or "ipal" (the iOS look's grouped structure)
  function tokens(T, kind) {
    var P = T.colors || {}, bg = P.background, fg = P.foreground, dark = T.mode !== 'light', S = kind === 'site' && T.s;
    var tint = function (t) { return mix(fg, bg, t); }, v = {}, on;
    if (kind === 'ipal') {
      // iOS structure: a page and lifted grouped cards. Dark: page below the theme background, cards = the
      // theme background (near-black themes lift the cards instead). Light: cards = the theme background,
      // page a shade darker. Separation held at iOS's own ratios (~1.25 dark, ~1.1 light).
      var page, card;
      if (dark) {
        if (lum(bg) < .012) { page = bg; card = mix(fg, bg, .1); }
        else {
          page = P.darker_background && lum(P.darker_background) < lum(bg) ? P.darker_background : mix('#000000', bg, .45);
          card = bg;
          for (var i = 0; cr(card, page) < 1.22 && i < 20; i++) page = mix('#000000', page, .12);
        }
      } else {
        card = bg; page = P.dark_background && cr(P.dark_background, bg) >= 1.06 && cr(P.dark_background, bg) <= 1.2 ? P.dark_background : bg;
        for (var j = 0; cr(card, page) < 1.08 && j < 30; j++) page = mix(fg, page, .015);
      }
      v.bg = page; v.bg2 = card; v.bg1 = dark ? mix(card, page, .55) : card;
      v.bg3 = mix(fg, card, dark ? .1 : .08); v.bg4 = mix(fg, card, dark ? .18 : .16);
      on = [page, card, v.bg3]; // text is held on all three (chips and small buttons sit on bg3)
      v.fg = ensureAll(cr(fg, card) >= 7 || !P.bright_foreground ? fg : P.bright_foreground, on, 7);
      v.fg2 = ensureAll(mix(v.fg, card, .78), on, 6);
      v.fg3 = ensureAll(mix(v.fg, card, .5), on, 3);
      v.sep = alpha(fg, dark ? .17 : .2); v.line = alpha(fg, dark ? .3 : .34);
      var acs = [P.accent, P.blue, P.cyan, P.magenta].filter(function (c) { return c && hsl(c)[1] > .25 && cr(c, fg) > 1.25; });
      v.accent = ensureAll(acs[0] || P.accent || P.blue || fg, on, 4.5); v['on-accent'] = best(v.accent, [page, '#000000', '#ffffff']);
      v.bar = alpha(page, .86); v.code = dark ? mix('#000000', card, .28) : mix(fg, card, .045);
      v['term-bg'] = bg;
    } else {
      v.bg = bg; v.bg1 = S ? S.surf : tint(.03); v.bg2 = S ? S.surf : tint(.055); v.bg3 = S ? S.surf2 : tint(.12); v.bg4 = S ? S.strong : tint(.2);
      on = [v.bg2];
      if (S) v.fg = S.text; else v.fg = ensure(cr(fg, bg) >= 7 || !P.bright_foreground ? fg : P.bright_foreground, v.bg2, 7);
      var f2 = S ? S.text2 : mix(fg, bg, .72); v.fg2 = ensure(f2, v.bg2, 4.6);
      v.fg3 = S ? alpha(S.muted, .75) : ensure(mix(fg, bg, .42), bg, 2.3);
      v.sep = S ? S.sub : alpha(fg, dark ? .14 : .2);
      v.line = S ? S.strong : alpha(fg, dark ? .26 : .32); // control borders (theme.css)
      var ac = P.accent || P.blue || fg; v.accent = ensure(ac, v.bg2, 4.5); v['on-accent'] = S ? S.ink : best(v.accent, [bg, '#000000', '#ffffff']);
      v.bar = bg; v.code = S ? S.deep : dark ? (P.darker_background || mix('#000000', bg, .3)) : (P.dark_background || tint(.04));
      v['term-bg'] = S ? S.deep : bg;
    }
    v.bub = v.accent; v.bubfg = v['on-accent'];
    v['term-fg'] = fg;
    // status: needs-you / done / working / error stay distinct hues even when a theme repeats colours
    var IOS = { red: '#ff453a', blocked: '#ff9f0a', done: '#30d158', work: '#0a84ff' }, chosen = [];
    function pick(k, cands) {
      var c = cands.filter(Boolean).filter(function (x) { return hsl(x)[1] > .18 && chosen.every(function (y) { return hueDist(x, y) >= 28; }); })[0];
      if (!c) c = dark ? IOS[k] : mix(IOS[k], '#000000', .75);
      chosen.push(c); return ensureAll(c, on, 4.5);
    }
    v.red = pick('red', [P.red, P.bright_red]);
    v.blocked = pick('blocked', [P.orange, P.yellow, P.bright_yellow]);
    v.done = pick('done', [P.green, P.bright_green, P.cyan]);
    v.work = pick('work', [P.blue, P.accent, P.cyan, P.bright_blue, P.magenta]);
    v.idle = ensure(P.muted || P.dark_foreground || tint(.45), v.bg, 3);
    v['on-blocked'] = best(v.blocked, [bg, '#000000', '#ffffff']);
    v['on-red'] = best(v.red, [bg, '#000000', '#ffffff']);
    // agent identity colours, plus a text-safe (--ct-*) and an on-colour (--co-*) companion
    var orange = P.orange || mix(P.red || '#ff453a', P.yellow || '#ffd60a', .5), mag = P.magenta || '#bf5af2';
    var A = { red: P.red, orange: orange, yellow: P.yellow, green: P.green, cyan: P.cyan, blue: P.blue, purple: mag,
      pink: P.bright_magenta && hueDist(P.bright_magenta, mag) > 12 ? P.bright_magenta : mix(mag, P.red || '#ff375f', .5) };
    AG.forEach(function (k) {
      var c = A[k] || '#888888'; v['c-' + k] = c; v['ct-' + k] = ensureAll(c, on, 4.5); v['co-' + k] = best(c, [bg, fg, '#000000', '#ffffff']);
    });
    v['k-code'] = A.blue; v['k-cache'] = A.cyan; v['k-build'] = A.purple; v['k-agent'] = orange; v['k-media'] = A.pink; v['k-other'] = v.idle;
    // terminal: the desktop terminal's own 16 colours when the theme ships them
    var an = T.ansi && T.ansi.length === 16 ? T.ansi : [P.lighter_background || tint(.25), P.red, P.green, P.yellow, P.blue, P.magenta, P.cyan, fg,
      P.muted || tint(.5), P.bright_red || P.red, P.bright_green || P.green, P.bright_yellow || P.yellow, P.bright_blue || P.blue, P.bright_magenta || P.magenta, P.bright_cyan || P.cyan, P.bright_foreground || fg];
    an.forEach(function (c, i) { if (c) v['ansi-' + i] = c; });
    var bc = (T.border && T.border.colors || []).filter(Boolean);
    if (!bc.length) bc = [v.accent];
    v.hb = bc.length > 1 ? 'linear-gradient(' + (T.border.angle || 90) + 'deg, ' + bc.join(', ') + ')' : 'linear-gradient(' + bc[0] + ', ' + bc[0] + ')';
    v['hb-solid'] = bc[0].slice(0, 7);
    return v;
  }

  // ---------- apply ----------
  var applied = [];
  function meta(name, content) {
    var ms = document.querySelectorAll('meta[name="' + name + '"]');
    ms.forEach(function (m) { if (!m.dataset.orig) m.dataset.orig = m.getAttribute('content') + '|' + (m.getAttribute('media') || ''); });
    ms.forEach(function (m) {
      if (content == null) { var o = m.dataset.orig.split('|'); m.setAttribute('content', o[0]); if (o[1]) m.setAttribute('media', o[1]); }
      else { m.setAttribute('content', content); m.removeAttribute('media'); }
    });
  }
  function look() { var l = get(KL); return l === 'ios' || l === 'desk' || l === 'site' || l === 'ipal' ? l : DEF; }
  function pal() { return get(KP) || 'desktop'; }
  function lightOS() { return matchMedia('(prefers-color-scheme: light)').matches; }
  function day() { return get(KAL) || AUTO_DAY; }
  function night() { return get(KAD) || AUTO_NIGHT; }
  function parse(s) { try { return JSON.parse(s || 'null'); } catch (e) { return null; } }
  function theme() { return parse(get(KT)); }
  function named(n) { return parse(get(KPT + n)); }
  function ok(T) { return T && T.colors && T.colors.background ? T : null; }
  // the palette's theme name right now (null = the desktop's)
  function palName() { var p = pal(); return p === 'desktop' ? null : p === 'auto' ? (lightOS() ? day() : night()) : p; }
  function palTheme() { var n = palName(); return (n && ok(named(n))) || ok(theme()); }
  function apply() {
    var l = look(), T = null;
    if (PAL_LOOKS[l]) T = palTheme();
    if (l === 'site') T = SITE[lightOS() ? 'light' : 'dark'];
    applied.forEach(function (k) { root.style.removeProperty('--' + k); });
    applied = [];
    if (!T) { // iOS look, or no palette known yet
      delete root.dataset.look; delete root.dataset.pal; delete root.dataset.mode; root.style.removeProperty('color-scheme');
      meta('theme-color', null); meta('color-scheme', null); return;
    }
    var v = tokens(T, l);
    Object.keys(v).forEach(function (k) { root.style.setProperty('--' + k, v[k]); applied.push(k); });
    root.dataset.pal = l; // colour rules (theme.css)
    if (l === 'ipal') delete root.dataset.look; else root.dataset.look = l; // structure rules: desk / site only
    root.dataset.mode = T.mode === 'light' ? 'light' : 'dark';
    root.style.colorScheme = root.dataset.mode;
    meta('theme-color', v.bg); meta('color-scheme', root.dataset.mode);
  }
  function save(T) {
    if (!T || !T.colors) return;
    var old = theme();
    if (old && old.sig === T.sig) return;
    set(KT, JSON.stringify(T)); apply();
  }
  function getJSON(u) { return fetch(u, { cache: 'no-store' }).then(function (r) { return r.ok ? r.json() : null; }); }
  function fetchNamed(n) {
    return getJSON('/api/theme?name=' + encodeURIComponent(n)).then(function (T) {
      if (!ok(T)) return;
      var old = named(n);
      if (old && old.sig === T.sig) return;
      set(KPT + n, JSON.stringify(T)); apply();
    });
  }
  function refresh() {
    var p = pal(), ns = p === 'auto' ? [day(), night()] : p === 'desktop' ? [] : [p];
    return Promise.all([getJSON('/api/theme').then(save)].concat(ns.map(fetchNamed))).catch(function () { });
  }

  // ---------- Settings → Look (+ Palette) ----------
  function pretty(n) { return String(n || '').split(/[-_ ]+/).map(function (w) { return w.charAt(0).toUpperCase() + w.slice(1); }).join(' '); }
  function esc(s) { return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) { return '&#' + c.charCodeAt(0) + ';'; }); }
  function palLabel() {
    var p = pal(), T = theme();
    return p === 'desktop' ? 'Follows desktop' + (T && T.name ? ' · ' + pretty(T.name) : '') : p === 'auto' ? pretty(day()) + ' / ' + pretty(night()) : pretty(p);
  }
  function swatch(t) {
    return '<i aria-hidden="true" style="flex:none;width:44px;height:30px;display:inline-flex;align-items:center;justify-content:center;gap:4px;border-radius:8px;background:' + esc(t.bg)
      + ';box-shadow:inset 0 0 0 1px ' + esc(alpha(t.fg || '#888888', .25)) + ';font:600 13px/1 system-ui;font-style:normal;color:' + esc(t.fg) + '">Aa<b style="width:8px;height:8px;border-radius:50%;background:' + esc(t.accent || t.fg) + '"></b></i>';
  }
  function row(attr, title, sub, tick, pre) {
    return '<button class="kv" ' + attr + ' style="width:100%;text-align:left;gap:12px;justify-content:flex-start;min-height:52px">' + (pre || '')
      + '<span style="display:flex;flex-direction:column;gap:1px;flex:1;min-width:0"><b style="font-weight:600;color:var(--fg);text-align:left;font-size:16px">' + title + '</b>'
      + (sub ? '<small class="dim" style="font-size:13px">' + sub + '</small>' : '') + '</span>'
      + '<b aria-hidden="true" style="color:var(--accent);flex:none">' + (tick || '') + '</b></button>';
  }
  function settingsPart(el) {
    if (!el) return;
    var list = parse(get(KTL)) || [];
    function draw() {
      var cur = look(), p = pal(), T = theme();
      var h = '<h3>Look</h3><div class="group mm-looks">' + LOOKS.map(function (x) {
        var sub = x[0] === 'ipal' ? 'system font and layout, ' + palLabel()
          : x[0] === 'desk' ? 'square, mono, Hyprland border · ' + palLabel()
            : x[0] === 'site' ? 'JetBrains Mono, Geist, square corners' : 'system colours, follows light/dark';
        return row('data-look="' + x[0] + '"', x[1], esc(sub), x[0] === cur ? '✓' : '');
      }).join('') + '</div>';
      if (PAL_LOOKS[cur]) {
        var auto = p === 'auto';
        h += '<h3>Palette</h3><div class="group mm-pals">'
          + row('data-pal="desktop"', 'Follow desktop', esc((T && T.name ? pretty(T.name) + ' now · ' : '') + 'changes with omarchy-theme-set'), p === 'desktop' ? '✓' : '')
          + row('data-pal="auto"', 'Match iPhone light/dark', esc(pretty(day()) + ' by day, ' + pretty(night()) + ' by night'), auto ? '✓' : '')
          + list.map(function (t) {
            var tick = auto ? (t.name === day() ? '☀︎' : t.name === night() ? '☾' : '') : p === t.name ? '✓' : '';
            return row('data-pal="' + esc(t.name) + '" data-mode="' + esc(t.mode) + '"', esc(pretty(t.name)), esc(t.mode + (t.user ? ' · your theme' : '') + (auto ? ' · tap to use by ' + (t.mode === 'light' ? 'day' : 'night') : '')), tick, swatch(t));
          }).join('') + '</div>';
      }
      el.innerHTML = h;
      el.querySelectorAll('[data-look]').forEach(function (b) {
        b.onclick = function () { set(KL, b.dataset.look); apply(); draw(); refresh().then(draw); };
      });
      el.querySelectorAll('[data-pal]').forEach(function (b) {
        b.onclick = function () {
          var n = b.dataset.pal;
          if (pal() === 'auto' && b.dataset.mode) set(b.dataset.mode === 'light' ? KAL : KAD, n);
          else set(KP, n);
          apply(); draw(); refresh().then(draw);
        };
      });
    }
    draw();
    getJSON('/api/themes').then(function (r) {
      if (!r || !r.themes) return;
      list = r.themes; set(KTL, JSON.stringify(list)); draw();
    }).catch(function () { });
  }

  window.mmTheme = { apply: save, refresh: refresh, settingsPart: settingsPart, look: look, pal: pal, tokens: tokens,
    setLook: function (l, p) { set(KL, l); if (p) set(KP, p); apply(); return refresh(); } };
  apply();
  refresh();
  matchMedia('(prefers-color-scheme: light)').addEventListener('change', function () { var l = look(); if (l === 'site' || (PAL_LOOKS[l] && pal() === 'auto')) apply(); });
  document.addEventListener('visibilitychange', function () { if (!document.hidden) refresh(); });
})();
