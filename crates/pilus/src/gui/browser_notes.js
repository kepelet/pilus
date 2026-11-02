// The picker behind the Browser's annotate button (browser_notes.rs).
//
// The window injects this on demand: when the person turns annotating on, and
// again after a page they have notes on finishes loading. It is never part of
// the document-start script, so a page the person is only reading runs none of
// it. It works on any page, since the person is the one pointing.
//
// Picking only reports which element was chosen. The note itself is written in
// the window's own field, not in the page, so the page can neither read it nor
// imitate the prompt. What this posts is page-derived and the window treats it
// as untrusted text.
(() => {
  if (window.__pilusNotes || !window.ipc) return;

  const post = (message) => {
    try {
      window.ipc.postMessage(JSON.stringify(message));
    } catch (_) {
      /* a message that cannot be encoded is dropped */
    }
  };
  const clip = (value, max) => {
    const text = String(value == null ? '' : value);
    return text.length > max ? text.slice(0, max) + '…' : text;
  };
  const words = (text) => String(text || '').trim().replace(/\s+/g, ' ');
  const ACCENT = '#3b82f6';

  // ---- our own layer -------------------------------------------------------
  // A closed shadow root keeps the page's styles out of the outline and the
  // badges, and ours out of the page. Nothing in it takes the pointer.
  let host = null;
  let layer = null;
  const ensureLayer = () => {
    if (host && host.isConnected) return layer;
    host = document.createElement('div');
    host.style.cssText =
      'all:initial;position:fixed;inset:0;z-index:2147483647;pointer-events:none;';
    const root = host.attachShadow({ mode: 'closed' });
    layer = document.createElement('div');
    root.appendChild(layer);
    (document.documentElement || document).appendChild(host);
    return layer;
  };
  const inLayer = (node) => node === host;

  const box = document.createElement('div');
  box.style.cssText = `position:fixed;pointer-events:none;box-sizing:border-box;border:2px solid ${ACCENT};background:${ACCENT}22;border-radius:3px;display:none;`;
  const tag = document.createElement('div');
  tag.style.cssText = `position:fixed;pointer-events:none;font:11px/1.3 ui-monospace,Menlo,monospace;color:#fff;background:${ACCENT};padding:1px 5px;border-radius:3px;white-space:nowrap;max-width:60vw;overflow:hidden;text-overflow:ellipsis;display:none;`;

  // ---- naming an element ---------------------------------------------------
  const esc = (value) => (window.CSS && CSS.escape ? CSS.escape(value) : String(value));
  const unique = (selector) => {
    try {
      return document.querySelectorAll(selector).length === 1;
    } catch (_) {
      return false;
    }
  };
  // A selector an agent can hand to `pilus ctl browser click`: the shortest
  // that matches this element alone, from an id, else tag and classes, else a
  // position among its siblings, climbing until it is unique.
  const selectorFor = (el) => {
    const parts = [];
    let node = el;
    for (let depth = 0; node && node.nodeType === 1 && depth < 8; depth++) {
      if (node === document.documentElement) {
        parts.unshift('html');
        break;
      }
      if (node.id && unique('#' + esc(node.id))) {
        parts.unshift('#' + esc(node.id));
        break;
      }
      let part = node.tagName.toLowerCase();
      const classes = Array.from(node.classList || [])
        .filter((name) => /^[A-Za-z_-][\w-]{0,30}$/.test(name))
        .slice(0, 2);
      if (classes.length) part += classes.map((name) => '.' + esc(name)).join('');
      const parent = node.parentElement;
      if (parent) {
        const same = Array.from(parent.children).filter((child) => child.tagName === node.tagName);
        if (same.length > 1) part += `:nth-of-type(${same.indexOf(node) + 1})`;
      }
      parts.unshift(part);
      if (unique(parts.join(' > '))) break;
      node = parent;
    }
    return parts.join(' > ');
  };
  // How the element reads in the note: `button#go.primary`.
  const labelFor = (el) => {
    let text = el.tagName.toLowerCase();
    if (el.id) text += '#' + el.id;
    const classes = Array.from(el.classList || []).slice(0, 2);
    if (classes.length) text += '.' + classes.join('.');
    return clip(text, 80);
  };

  // ---- picking ---------------------------------------------------------------
  let picking = false;
  let target = null;
  let trail = [];

  const draw = () => {
    if (!target || !target.isConnected) {
      box.style.display = tag.style.display = 'none';
      return;
    }
    const rect = target.getBoundingClientRect();
    box.style.display = 'block';
    box.style.left = rect.left + 'px';
    box.style.top = rect.top + 'px';
    box.style.width = rect.width + 'px';
    box.style.height = rect.height + 'px';
    tag.textContent = `${labelFor(target)}  ${Math.round(rect.width)}×${Math.round(rect.height)}`;
    tag.style.display = 'block';
    tag.style.left = Math.max(0, Math.min(rect.left, innerWidth - 160)) + 'px';
    const above = rect.top - 20;
    tag.style.top = (above < 0 ? rect.bottom + 2 : above) + 'px';
  };

  const pointed = (event) => {
    const el = event.target;
    return el instanceof Element && !inLayer(el) ? el : null;
  };

  const hover = (event) => {
    const el = pointed(event);
    if (!el || el === target) return;
    target = el;
    trail = [];
    draw();
  };

  // The page must not act on a pick: no navigation, no handler, no focus.
  const swallow = (event) => {
    event.preventDefault();
    event.stopImmediatePropagation();
  };

  const choose = (event) => {
    swallow(event);
    const el = target || pointed(event);
    if (!el) return;
    const rect = el.getBoundingClientRect();
    post({
      t: 'pick',
      selector: clip(selectorFor(el), 300),
      label: labelFor(el),
      text: clip(words(el.innerText || el.value || el.alt || el.title), 300),
      html: clip(el.outerHTML, 500),
      url: clip(location.href, 1000),
      x: Math.round(rect.left),
      y: Math.round(rect.top),
      w: Math.round(rect.width),
      h: Math.round(rect.height),
    });
    stop();
  };

  const key = (event) => {
    if (event.key === 'Escape') {
      swallow(event);
      post({ t: 'pick_end' });
      stop();
    } else if (event.key === 'ArrowUp' && target && target.parentElement) {
      // Widen the pick to the element's container.
      swallow(event);
      trail.push(target);
      target = target.parentElement;
      draw();
    } else if (event.key === 'ArrowDown' && trail.length) {
      swallow(event);
      target = trail.pop();
      draw();
    }
  };

  const EVENTS = [
    ['mousemove', hover],
    ['mouseover', hover],
    ['click', choose],
    ['mousedown', swallow],
    ['mouseup', swallow],
    ['pointerdown', swallow],
    ['pointerup', swallow],
    ['auxclick', swallow],
    ['contextmenu', swallow],
    ['keydown', key],
    ['scroll', draw],
    ['resize', draw],
  ];

  const start = () => {
    if (picking) return;
    picking = true;
    target = null;
    trail = [];
    const into = ensureLayer();
    into.appendChild(box);
    into.appendChild(tag);
    for (const [type, handler] of EVENTS) addEventListener(type, handler, true);
    document.documentElement.style.cursor = 'crosshair';
    // Keys (Esc, arrows) go to the page only when it holds the keyboard.
    if (document.body && document.activeElement !== document.body) {
      try {
        document.activeElement.blur();
      } catch (_) {
        /* nothing to blur */
      }
    }
  };

  function stop() {
    if (!picking) return;
    picking = false;
    target = null;
    for (const [type, handler] of EVENTS) removeEventListener(type, handler, true);
    document.documentElement.style.cursor = '';
    box.remove();
    tag.remove();
  }

  // ---- the numbered badges on elements that have a note ---------------------
  let marks = [];
  let queued = 0;
  const badges = [];
  const place = () => {
    queued = 0;
    if (!marks.length && !badges.length) return;
    const into = ensureLayer();
    for (const badge of badges) badge.remove();
    badges.length = 0;
    for (const mark of marks) {
      let el = null;
      try {
        el = document.querySelector(mark.selector);
      } catch (_) {
        /* a selector that no longer parses marks nothing */
      }
      if (!el) continue;
      const rect = el.getBoundingClientRect();
      if (rect.width === 0 && rect.height === 0) continue;
      const frame = document.createElement('div');
      frame.style.cssText = `position:fixed;pointer-events:none;box-sizing:border-box;border:2px dashed ${ACCENT};border-radius:3px;left:${rect.left}px;top:${rect.top}px;width:${rect.width}px;height:${rect.height}px;`;
      const number = document.createElement('div');
      number.textContent = String(mark.n);
      number.style.cssText = `position:absolute;left:-2px;top:-10px;min-width:18px;height:18px;padding:0 4px;box-sizing:border-box;border-radius:9px;background:${ACCENT};color:#fff;font:600 11px/18px -apple-system,system-ui,sans-serif;text-align:center;`;
      frame.appendChild(number);
      into.appendChild(frame);
      badges.push(frame);
    }
  };
  const schedule = () => {
    if (!queued) queued = requestAnimationFrame(place);
  };
  let watching = false;
  const mark = (list) => {
    marks = Array.isArray(list)
      ? list
          .filter((item) => item && typeof item.selector === 'string')
          .map((item) => ({ n: Number(item.n) || 0, selector: item.selector }))
      : [];
    // Scroll and resize are watched only while there is something to place.
    if (marks.length && !watching) {
      watching = true;
      addEventListener('scroll', schedule, true);
      addEventListener('resize', schedule);
    } else if (!marks.length && watching) {
      watching = false;
      removeEventListener('scroll', schedule, true);
      removeEventListener('resize', schedule);
    }
    place();
    return 'marked';
  };

  Object.defineProperty(window, '__pilusNotes', {
    value: Object.freeze({
      start: () => (start(), 'picking'),
      stop: () => (stop(), 'stopped'),
      mark,
    }),
    enumerable: false,
  });
})();
