// The page collector (design section 2): runs in the page, injected over CDP before any page script. Two jobs:
// 1. a deterministic page: fixed clock, seeded random, no timer past a second, no interval;
// 2. record layout shifts from the first frame on, and dump the layout tree for the Rust rules.
// `__AT__` (epoch ms), `__SEED__` and `__FIXED__` (false in the interactive viewer: a live clock) are replaced by the tool. Nothing here decides pass or fail.
(() => {
  if (__FIXED__) {
    const at = __AT__;
    const RealDate = Date;
    const Fixed = function (...a) { return a.length ? new RealDate(...a) : new RealDate(at); };
    Fixed.prototype = RealDate.prototype; Fixed.now = () => at; Fixed.parse = RealDate.parse; Fixed.UTC = RealDate.UTC;
    window.Date = Fixed;
    let s = __SEED__;
    const rnd = () => { s = (s + 0x6d2b79f5) | 0; let t = Math.imul(s ^ (s >>> 15), 1 | s); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
    Math.random = rnd;
    try {
      crypto.getRandomValues = (a) => { for (let i = 0; i < a.length; i++) a[i] = Math.floor(rnd() * 256); return a; };
      crypto.randomUUID = () => '00000000-0000-4000-8000-' + Math.floor(rnd() * 0xffffffffffff).toString(16).padStart(12, '0');
    } catch { /* a frozen crypto: leave it */ }
    const st = window.setTimeout.bind(window);
    window.setTimeout = (f, ms, ...r) => (ms >= 1000 ? 0 : st(f, ms, ...r));
    window.setInterval = () => 0;
  }

  const S = (window.__shakedown = { shifts: [] });
  const name = (n) => (n ? `${n.nodeName.toLowerCase()}${n.id ? '#' + n.id : ''}${typeof n.className === 'string' && n.className ? '.' + n.className.trim().split(/\s+/).slice(0, 2).join('.') : ''}` : '?');
  const box = (r) => `${Math.round(r.x)},${Math.round(r.y)} ${Math.round(r.width)}x${Math.round(r.height)}`;
  try {
    new PerformanceObserver((l) => {
      for (const e of l.getEntries()) {
        // hadRecentInput is not consulted: no person is typing, our own actions are judged separately, and mobile
        // emulation marks the shifts of a plain page load as input-driven.
        S.shifts.push({ value: e.value, sources: e.sources.map((x) => `${name(x.node)} ${box(x.previousRect)} -> ${box(x.currentRect)}`) });
      }
    }).observe({ type: 'layout-shift', buffered: true });
  } catch { /* no layout-shift entries: nothing to record */ }

  const TAP = 'a,button,input:not([type=hidden]),select,textarea,[role=button]';
  const PROGRESS = 'progress,meter,[role=progressbar]';
  const r2 = (v) => Math.round(v * 100) / 100;
  const rc = (r) => ({ x: r2(r.left), y: r2(r.top), w: r2(r.width), h: r2(r.height) });
  const clip = (v) => v === 'hidden' || v === 'clip';

  // `ignores`: [{rule, match, why}] from shakedown.toml, matched here because only the page can run a selector.
  S.tree = (ignores) => {
    const de = document.documentElement;
    const nodes = [], bad = [];
    // Probing must see decoration too: `pointer-events:none` elements are skipped by hit tests otherwise.
    // A constructable stylesheet, not a <style> element: a page's Content-Security-Policy blocks inline styles.
    const pe = new CSSStyleSheet();
    pe.replaceSync('*,*::before,*::after{pointer-events:auto!important}');
    document.adoptedStyleSheets = [...document.adoptedStyleSheets, pe];
    try {
    // `elementFromPoint` is null outside the viewport: scroll the point into view, probe, and scroll back, so a line
    // below the fold is judged like one above it.
    const hitAt = (x, y, el) => {
      const w = de.clientWidth, h = de.clientHeight;
      if (x >= 0 && y >= 0 && x < w && y < h) return document.elementFromPoint(x, y);
      // The page may scroll inside an element (an app shell): scroll the element itself into view, then put back
      // every scroller that moved. In `devices serve` this runs inside an iframe, and `scrollIntoView` scrolls every
      // ancestor too: this page's scrollers, then each parent window and the parent's scrollers. Same-origin
      // parents are saved and restored here; a cross-origin parent cannot be reached, so `container: 'nearest'`
      // (Chrome 140+) keeps the scroll inside the nearest scroller to begin with.
      const saved = [];
      let reach = true;
      for (let win = window, node = el; win;) {
        saved.push([win, win.scrollX, win.scrollY]);
        for (let n = node && node.parentElement; n; n = n.parentElement) saved.push([n, n.scrollLeft, n.scrollTop]);
        let frame = null;
        try { frame = win.frameElement; } catch { reach = false; /* cross-origin parent */ }
        // A cross-origin parent also reads as `null` here: only the top window is truly the end of the chain.
        if (!frame) { if (win !== win.top) reach = false; break; }
        win = frame.ownerDocument.defaultView;
        node = frame;
      }
      const before = el.getBoundingClientRect();
      const probe = (opts) => {
        el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant', ...opts });
        const after = el.getBoundingClientRect();
        return [x + after.left - before.left, y + after.top - before.top];
      };
      const off = (px, py) => px < 0 || py < 0 || px >= w || py >= h;
      try {
        let [px, py] = probe({ container: 'nearest' });
        // Nested scrollers: the nearest one alone may not bring it into view. Scroll them all (restored below), but
        // only when every parent can be restored.
        if (off(px, py) && reach) [px, py] = probe({});
        // Still off screen: unknown, so skip it rather than call it covered or clear.
        if (off(px, py)) return null;
        return document.elementFromPoint(px, py);
      } finally {
        for (const [n, l, t] of saved) {
          if (n.scrollTo && n.window === n) n.scrollTo({ left: l, top: t, behavior: 'instant' });
          else { n.scrollLeft = l; n.scrollTop = t; }
        }
      }
    };
    const label = (e) => `<${e.tagName.toLowerCase()}${typeof e.className === 'string' && e.className.trim() ? ' class="' + e.className.trim() + '"' : ''}>`;
    const marks = (e) => {
      const out = [];
      const attr = e.getAttribute('data-shakedown-ignore');
      if (attr !== null) {
        const why = (e.getAttribute('data-shakedown-why') || '').trim();
        if (!why) bad.push(label(e));
        for (const rule of attr.split(/[\s,]+/).filter(Boolean)) out.push({ rule, why: why || '(none)' });
      }
      for (const ig of ignores) { try { if (e.matches(ig.match)) out.push({ rule: ig.rule, why: ig.why }); } catch { /* bad selector: reported by the tool */ } }
      return out;
    };
    // Only text that is actually on top: a dialog (anything with a painted background, or an image) covers the page
    // behind it. Another text element merely sitting over this one does not hide it: that is an overlap.
    const painted = (n) => {
      const bg = getComputedStyle(n).backgroundColor;
      return (bg && bg !== 'transparent' && !/,\s*0\)$/.test(bg)) || n.matches('img,video,canvas,svg');
    };
    // Three points along the line: a dropdown over only part of a line covers it too.
    const covered = (e, x, y) => {
      const hit = hitAt(x, y, e);
      if (!hit) return null;
      if (e.contains(hit) || hit.contains(e)) return false;
      for (let n = hit; n && n !== document.body && n !== document.documentElement; n = n.parentElement) {
        if (n.contains(e)) break;
        if (painted(n)) return true;
      }
      return false;
    };
    const top = (e, r) => {
      const y = r.top + r.height / 2;
      if (covered(e, r.left + r.width / 2, y) !== false) return false;
      return [r.left + 1, r.right - 1].every((x) => covered(e, x, y) !== true);
    };
    // Text hidden on purpose (the usual visually-hidden pattern): `.sr-only`, a clip or clip-path down to nothing, or
    // a 1x1 box with hidden overflow, on the element or any ancestor. Reading it aloud is its job; it is not "cut off".
    const hiddenBox = (e) => {
      const cs = getComputedStyle(e);
      if (e.classList.contains('sr-only')) return true;
      const zero = (v) => /^(0|1)(px)?$/.test(v);
      const m = /^rect\(([^)]*)\)$/.exec(cs.clip);
      if (m && m[1].split(/[\s,]+/).filter(Boolean).every(zero)) return true;
      if (/^inset\((50|100)%\)$/.test(cs.clipPath) || /^circle\(0(px)?\b/.test(cs.clipPath)) return true;
      const r = e.getBoundingClientRect();
      return r.width <= 1 && r.height <= 1 && (clip(cs.overflowX) || clip(cs.overflowY));
    };
    const hiddenOnPurpose = (e) => {
      for (let n = e; n && n !== document.body && n !== document.documentElement; n = n.parentElement) {
        if (hiddenBox(n)) return true;
      }
      return false;
    };
    const decorEls = [];
    const walk = (parent, path, inherited, scrollX, decor) => {
      const kids = parent.children;
      for (let i = 0; i < kids.length; i++) {
        const e = kids[i];
        const id = path ? `${path}.${i}` : String(i);
        const cs = getComputedStyle(e);
        if (cs.display === 'none') continue;
        const ign = inherited.concat(marks(e));
        // aria-hidden="true" subtrees are decoration (drifting layers): two of them never overlap each other, and
        // one counts only where it is painted over real text (see `decorEls` below).
        const dec = decor || e.getAttribute('aria-hidden') === 'true';
        const sx = scrollX || e.hasAttribute('data-scroll-x') || e.hasAttribute('data-shakedown-scroll-x');
        const tag = e.tagName.toLowerCase();
        const rect = e.getBoundingClientRect();
        if (cs.visibility !== 'hidden' && e.checkVisibility() && (rect.width || rect.height)) {
          const lines = [], topLines = [];
          const hid = hiddenOnPurpose(e);
          let own = '';
          for (const n of e.childNodes) {
            if (n.nodeType !== 3 || !n.textContent.trim()) continue;
            own += ' ' + n.textContent.trim();
            const g = document.createRange(); g.selectNodeContents(n);
            for (const r of g.getClientRects()) {
              if (r.width <= 1 || r.height <= 1) continue;
              if (hid) continue;
              lines.push(rc(r));
              if (!dec && top(e, r)) topLines.push(rc(r));
            }
          }
          own = own.replace(/\s+/g, ' ').trim();
          let kind = 'container';
          if (e.matches(TAP)) kind = 'tap';
          else if (e.matches(PROGRESS)) kind = 'progress';
          else if (['img', 'svg', 'canvas', 'video', 'picture'].includes(tag)) kind = 'image';
          else if (lines.length) kind = 'text';
          const clipped = !hid && e.children.length === 0 && own.length > 2
            && ((clip(cs.overflowX) && e.scrollWidth > e.clientWidth + 1) || (clip(cs.overflowY) && e.scrollHeight > e.clientHeight + 1));
          if (dec && lines.length) decorEls.push({ e, id });
          nodes.push({
            id, label: label(e), text: own.slice(0, 30), kind, rect: rc(rect), lines, top_lines: topLines,
            font_size: parseFloat(cs.fontSize) || 0, clipped, decor: dec, scroll_x: sx,
            inline: kind === 'tap' && tag === 'a' && cs.display === 'inline',
            ignore: ign,
          });
        }
        // svg and canvas draw their own insides.
        if (tag !== 'svg' && tag !== 'canvas') walk(e, id, ign, sx, dec);
      }
    };
    walk(document.body, '', [], false, false);
    // A decorative line is "on top" only where it is painted over a real (not aria-hidden) text line.
    const byId = new Map(nodes.map((n) => [n.id, n]));
    for (const { e, id } of decorEls) {
      const d = byId.get(id);
      for (const r of d.lines) {
        const over = nodes.some((n) => !n.decor && n.lines.some((q) => {
          const x0 = Math.max(r.x, q.x), x1 = Math.min(r.x + r.w, q.x + q.w);
          const y0 = Math.max(r.y, q.y), y1 = Math.min(r.y + r.h, q.y + q.h);
          if (x1 - x0 <= 0 || y1 - y0 <= 0) return false;
          const hit = hitAt((x0 + x1) / 2, (y0 + y1) / 2, e);
          return !!hit && e.contains(hit);
        }));
        if (over) d.top_lines.push(r);
      }
    }
    } finally {
      document.adoptedStyleSheets = document.adoptedStyleSheets.filter((x) => x !== pe);
    }
    return {
      viewport_w: de.clientWidth, viewport_h: de.clientHeight, content_w: de.scrollWidth,
      nodes, shifts: S.shifts, bad_ignores: bad,
    };
  };
})();
