// Axocoatl element picker — injected into proxied pages.
// Same-origin with the parent dashboard (via the proxy), so postMessage
// works unrestricted. The picker is "sticky": one click captures the
// element + its ancestor chain; the user keeps picking via the parent
// dashboard's hierarchy panel (hover/select level commands).
(function () {
  if (window.__axoTapLoaded) return;
  window.__axoTapLoaded = true;

  const STYLE = `
    .axo-tap-hover  { outline: 2px solid #7c5cff !important; outline-offset: 1px !important;
                     cursor: crosshair !important; background: rgba(124,92,255,.08) !important; }
    .axo-tap-locked { outline: 2px solid #f1c40f !important; outline-offset: 1px !important;
                     background: rgba(241,196,15,.12) !important; }
    .axo-tap-banner { position: fixed !important; top: 8px !important; left: 50% !important;
                     transform: translateX(-50%) !important; background: #161725 !important;
                     color: #fff !important; padding: 6px 12px !important; border-radius: 6px !important;
                     font: 12px/1.4 system-ui, sans-serif !important; z-index: 2147483647 !important;
                     border: 1px solid #7c5cff !important; box-shadow: 0 4px 14px rgba(0,0,0,.4) !important;
                     pointer-events: none !important; }`;
  function ensureStyle() {
    if (document.getElementById('axo-tap-style')) return;
    const s = document.createElement('style');
    s.id = 'axo-tap-style';
    s.textContent = STYLE;
    (document.head || document.documentElement).appendChild(s);
  }

  // States: idle → 'picking' (hover-highlight) → 'locked' (after click; user
  // is choosing a level in the parent hierarchy panel).
  let mode = 'idle';
  let hoverEl = null;     // current mouse-over element while picking
  let chain = [];         // root → ... → clicked, all DOM nodes
  let lockedEl = null;    // currently outlined element in 'locked' mode
  let banner = null;

  function cssEscape(s) {
    if (window.CSS && CSS.escape) return CSS.escape(s);
    return String(s).replace(/[^\w-]/g, c => '\\' + c);
  }

  function shortLabel(el) {
    if (!el || el.nodeType !== 1) return '';
    let s = el.tagName.toLowerCase();
    if (el.id) s += '#' + el.id;
    if (el.classList && el.classList.length) {
      s += '.' + Array.from(el.classList).slice(0, 3).join('.');
    }
    return s;
  }

  function selectorFor(el) {
    if (!el || el.nodeType !== 1) return '';
    if (el === document.documentElement) return 'html';
    if (el === document.body) return 'body';
    if (el.id) return '#' + cssEscape(el.id);
    let parts = [];
    let cur = el;
    let depth = 0;
    while (cur && cur.nodeType === 1 && depth < 5) {
      let part = cur.tagName.toLowerCase();
      if (cur.id) { parts.unshift('#' + cssEscape(cur.id)); break; }
      if (cur.classList && cur.classList.length) {
        part += '.' + Array.from(cur.classList).slice(0, 2).map(cssEscape).join('.');
      } else if (cur.parentElement) {
        const idx = Array.prototype.indexOf.call(cur.parentElement.children, cur) + 1;
        part += `:nth-child(${idx})`;
      }
      parts.unshift(part);
      cur = cur.parentElement;
      depth += 1;
    }
    return parts.join(' > ');
  }

  function snippetFor(el) {
    if (!el) return '';
    let html = el.outerHTML || '';
    if (html.length > 1500) html = html.slice(0, 1500) + '\n…';
    return html;
  }

  function buildChain(el) {
    const arr = [];
    let cur = el;
    while (cur && cur.nodeType === 1) {
      arr.unshift(cur);              // root first, clicked last
      cur = cur.parentElement;
    }
    return arr;
  }

  function serializeChain(arr) {
    return arr.map(node => ({
      tag: node.tagName ? node.tagName.toLowerCase() : '',
      id: node.id || '',
      classes: node.classList ? Array.from(node.classList) : [],
      label: shortLabel(node),
      selector: selectorFor(node),
    }));
  }

  function clearHover() {
    if (hoverEl) { try { hoverEl.classList.remove('axo-tap-hover'); } catch {} hoverEl = null; }
  }
  function clearLocked() {
    if (lockedEl) { try { lockedEl.classList.remove('axo-tap-locked'); } catch {} lockedEl = null; }
  }

  function showBanner(msg) {
    hideBanner();
    banner = document.createElement('div');
    banner.className = 'axo-tap-banner';
    banner.textContent = msg;
    document.body.appendChild(banner);
  }
  function hideBanner() { if (banner) { try { banner.remove(); } catch {} banner = null; } }

  function startPicking() {
    if (mode !== 'idle') return;
    mode = 'picking';
    ensureStyle();
    document.addEventListener('mousemove', onMove, true);
    document.addEventListener('click', onClick, true);
    document.addEventListener('keydown', onKey, true);
    showBanner('Pick an element — Esc to cancel');
  }

  function stop() {
    mode = 'idle';
    clearHover();
    clearLocked();
    hideBanner();
    chain = [];
    document.removeEventListener('mousemove', onMove, true);
    document.removeEventListener('click', onClick, true);
    document.removeEventListener('keydown', onKey, true);
  }

  function onMove(e) {
    if (mode !== 'picking') return;
    let el = e.target;
    if (!el || el === hoverEl) return;
    if (el.classList && (el.classList.contains('axo-tap-banner') || el.classList.contains('axo-tap-locked'))) return;
    clearHover();
    if (el && el.classList) el.classList.add('axo-tap-hover');
    hoverEl = el;
  }

  function onClick(e) {
    if (mode !== 'picking') return;
    e.preventDefault();
    e.stopPropagation();
    clearHover();
    chain = buildChain(e.target);
    const selectedIndex = chain.length - 1;
    lockedEl = chain[selectedIndex];
    if (lockedEl && lockedEl.classList) lockedEl.classList.add('axo-tap-locked');
    showBanner('Element captured — pick a level in the panel');
    mode = 'locked';
    try {
      parent.postMessage({
        kind: 'axo-tap:picked',
        url: location.href,
        chain: serializeChain(chain),
        selectedIndex,
        selector: selectorFor(chain[selectedIndex]),
        html: snippetFor(chain[selectedIndex]),
      }, '*');
    } catch {}
  }

  function onKey(e) {
    if (mode === 'idle') return;
    if (e.key === 'Escape') {
      stop();
      try { parent.postMessage({ kind: 'axo-tap:cancelled' }, '*'); } catch {}
    }
  }

  function setLockedLevel(level, opts) {
    if (!chain.length) return;
    const i = Math.max(0, Math.min(chain.length - 1, level | 0));
    clearLocked();
    lockedEl = chain[i];
    if (lockedEl && lockedEl.classList) lockedEl.classList.add('axo-tap-locked');
    if (opts && opts.scroll) {
      try { lockedEl.scrollIntoView({ block: 'center', behavior: 'smooth' }); } catch {}
    }
  }

  window.addEventListener('message', (e) => {
    const d = e.data || {};
    if (d.kind === 'axo-tap:start') {
      stop();
      startPicking();
    } else if (d.kind === 'axo-tap:cancel') {
      stop();
    } else if (d.kind === 'axo-tap:select-level' && typeof d.level === 'number') {
      setLockedLevel(d.level, { scroll: true });
      // Reply with the new level's html so the parent panel can show it.
      try {
        parent.postMessage({
          kind: 'axo-tap:level',
          level: d.level,
          selector: selectorFor(chain[d.level]),
          html: snippetFor(chain[d.level]),
        }, '*');
      } catch {}
    } else if (d.kind === 'axo-tap:hover-level' && typeof d.level === 'number') {
      // Temporary outline; doesn't change the locked selection. Implemented
      // by toggling the hover class on chain[level] briefly.
      const el = chain[d.level];
      if (el && el.classList) {
        clearHover();
        el.classList.add('axo-tap-hover');
        hoverEl = el;
      }
    } else if (d.kind === 'axo-tap:unhover') {
      clearHover();
    } else if (d.kind === 'axo-tap:confirm') {
      stop();
    }
  });

  try { parent.postMessage({ kind: 'axo-tap:ready' }, '*'); } catch {}
})();
