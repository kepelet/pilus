// Injected into a Browser page only when an agent may use that tab (it opened
// the tab, or the person lent it): see browser_tab.rs. A tab the person opened
// and keeps gets no hooks and no `__pilus`, so an ordinary site pays no cost
// for the capture below and `pilus ctl browser ...` has nothing to call.
//
// It (1) streams console output and requests to the window over `window.ipc`,
// and (2) defines `__pilus`, which the window calls to look at or act on the
// page. Every answer goes back the same way, tagged with the id of the call
// that asked. The script is idempotent: lending a tab injects it into a page
// that is already open, and a second copy does nothing.
//
// Everything here is page-visible and the page is not trusted: the window
// treats whatever arrives as untrusted data.
(() => {
  if (window.__pilus || !window.ipc) return;

  const post = window.ipc.postMessage.bind(window.ipc);
  const clip = (value, max) => {
    const text = String(value);
    return text.length > max ? text.slice(0, max) + '…' : text;
  };
  // The window refuses a message over its own cap; keep a result under it.
  const RESULT_MAX = 400000;
  const send = (message) => {
    try {
      post(JSON.stringify(message));
    } catch (_) {
      /* a message that cannot be encoded is dropped */
    }
  };

  // ---- the person --------------------------------------------------------
  // `isTrusted` is true only for input the browser generated itself. The
  // events the window dispatches for an agent are not, so a real click or
  // keypress is how the window learns that the person has taken over. Told at
  // most once a second.
  let touched = 0;
  for (const type of ['pointerdown', 'keydown']) {
    addEventListener(
      type,
      (event) => {
        if (!event.isTrusted || Date.now() - touched < 1000) return;
        touched = Date.now();
        send({ t: 'user' });
      },
      true
    );
  }

  // ---- console ---------------------------------------------------------
  const show = (value) => {
    if (typeof value === 'string') return value;
    if (value instanceof Error) return value.stack || String(value);
    try {
      const text = JSON.stringify(value);
      return text === undefined ? String(value) : text;
    } catch (_) {
      return String(value);
    }
  };
  const log = (level, text) =>
    send({ t: 'log', level, at: Date.now(), text: clip(text, 2000) });
  for (const level of ['log', 'info', 'warn', 'error', 'debug']) {
    const original = console[level].bind(console);
    console[level] = (...args) => {
      log(level, args.map(show).join(' '));
      original(...args);
    };
  }
  addEventListener('error', (event) =>
    log(
      'error',
      `${event.message} (${event.filename}:${event.lineno}:${event.colno})`
    )
  );
  addEventListener('unhandledrejection', (event) =>
    log('error', 'Unhandled rejection: ' + show(event.reason))
  );

  // ---- requests ----------------------------------------------------------
  const absolute = (url) => {
    try {
      return new URL(String(url), location.href).href;
    } catch (_) {
      return String(url);
    }
  };
  const request = (entry) =>
    send({ t: 'net', at: Date.now(), ...entry, url: clip(entry.url, 1000) });

  const originalFetch = window.fetch;
  if (originalFetch) {
    window.fetch = function (input, init) {
      const started = performance.now();
      const method = String(
        (init && init.method) || (input && input.method) || 'GET'
      ).toUpperCase();
      const url = absolute(input && input.url ? input.url : input);
      const ms = () => Math.round(performance.now() - started);
      return originalFetch.apply(this, arguments).then(
        (response) => {
          request({
            kind: 'fetch',
            method,
            url,
            status: response.status,
            ms: ms(),
            type: response.headers.get('content-type'),
          });
          return response;
        },
        (error) => {
          request({
            kind: 'fetch',
            method,
            url,
            status: null,
            ms: ms(),
            error: clip(error, 200),
          });
          throw error;
        }
      );
    };
  }

  const xhrs = new WeakMap();
  const open = XMLHttpRequest.prototype.open;
  const sendXhr = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function (method, url) {
    xhrs.set(this, { method: String(method).toUpperCase(), url: absolute(url) });
    return open.apply(this, arguments);
  };
  XMLHttpRequest.prototype.send = function () {
    const info = xhrs.get(this);
    if (info) {
      const started = performance.now();
      this.addEventListener('loadend', () =>
        request({
          kind: 'xhr',
          method: info.method,
          url: info.url,
          status: this.status || null,
          ms: Math.round(performance.now() - started),
          error: this.status ? undefined : 'failed',
        })
      );
    }
    return sendXhr.apply(this, arguments);
  };

  // Everything else the page loads (the document, scripts, styles, images).
  // The platform gives timing and size but not always a status.
  const seen = (list) => {
    for (const entry of list.getEntries()) {
      if (entry.initiatorType === 'fetch') continue;
      if (entry.initiatorType === 'xmlhttprequest') continue;
      request({
        kind: entry.entryType === 'navigation' ? 'document' : entry.initiatorType || 'resource',
        method: 'GET',
        url: entry.name,
        status: entry.responseStatus || null,
        ms: Math.round(entry.duration),
        size: entry.transferSize || undefined,
      });
    }
  };
  for (const type of ['navigation', 'resource']) {
    try {
      new PerformanceObserver(seen).observe({ type, buffered: true });
    } catch (_) {
      /* this kind of entry is not offered: fetch and XHR are still recorded */
    }
  }

  // ---- looking at the page ---------------------------------------------
  const refs = [];
  const visible = (el) => {
    if (!(el instanceof Element)) return false;
    const box = el.getBoundingClientRect();
    if (box.width === 0 || box.height === 0) return false;
    const style = getComputedStyle(el);
    return style.visibility !== 'hidden' && style.display !== 'none';
  };
  const words = (text) => String(text || '').trim().replace(/\s+/g, ' ');

  const resolve = (selector) => {
    if (typeof selector === 'string' && selector[0] === '@') {
      const el = refs[Number(selector.slice(1))];
      if (!el || !el.isConnected) {
        throw new Error(`${selector} is gone; take a new snapshot`);
      }
      return el;
    }
    const el = document.querySelector(selector);
    if (!el) throw new Error(`nothing matches ${selector}`);
    return el;
  };

  // How an element reads in a snapshot and in an action's answer.
  const describe = (el) => {
    const tag = el.tagName.toLowerCase();
    const parts = [tag];
    const type = el.getAttribute('type');
    if (tag === 'input' && type) parts[0] = `input[${type}]`;
    if (tag === 'a' && el.getAttribute('href')) parts.push(`href=${clip(el.getAttribute('href'), 80)}`);
    for (const name of ['id', 'name', 'placeholder', 'role']) {
      const value = el.getAttribute(name);
      if (value) parts.push(name === 'id' ? `#${value}` : `${name}=${clip(value, 40)}`);
    }
    if ('value' in el && el.value !== '' && type !== 'password' && tag !== 'button') {
      parts.push(`value=${JSON.stringify(clip(el.value, 60))}`);
    }
    if (el.checked) parts.push('checked');
    if (el.disabled) parts.push('disabled');
    const label = words(
      el.getAttribute('aria-label') ||
        (tag === 'input' || tag === 'textarea' ? '' : el.innerText) ||
        el.title ||
        el.alt
    );
    if (label) parts.push(JSON.stringify(clip(label, 80)));
    return parts.join(' ');
  };

  const CONTROLS =
    'a[href],button,input:not([type=hidden]),select,textarea,summary,[role=button],[role=link],[role=checkbox],[role=tab],[role=menuitem],[role=switch],[onclick],[contenteditable=""],[contenteditable=true]';
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'SVG', 'HEAD']);
  const LINES = 800;

  const snapshot = () => {
    refs.length = 0;
    const out = [`url: ${location.href}`, `title: ${document.title}`];
    const visit = (el) => {
      if (out.length >= LINES) return;
      if (SKIP.has(el.tagName.toUpperCase()) || !visible(el)) return;
      if (el.matches(CONTROLS)) {
        refs.push(el);
        out.push(`[@${refs.length - 1}] ${describe(el)}`);
        return;
      }
      if (/^H[1-6]$/.test(el.tagName)) {
        const text = words(el.innerText);
        if (text) out.push('#'.repeat(Number(el.tagName[1])) + ' ' + clip(text, 120));
        return;
      }
      // Text that sits directly in an element, which is the page's prose.
      let own = '';
      for (const node of el.childNodes) {
        if (node.nodeType === Node.TEXT_NODE) own += ' ' + node.textContent;
      }
      own = words(own);
      if (own) out.push('· ' + clip(own, 160));
      for (const child of el.children) visit(child);
    };
    if (document.body) visit(document.body);
    if (out.length >= LINES) out.push('… (snapshot cut short; use text or html with a selector)');
    return out.join('\n');
  };

  const cut = (text) =>
    text.length > 200000
      ? text.slice(0, 200000) + `\n… (${text.length - 200000} more characters)`
      : text;

  // ---- the agent's cursor ------------------------------------------------
  // A pointer that glides to what the agent is about to act on and pulses
  // where it clicks, so the person watching sees what is being done. It is
  // cosmetic: it never takes part in the page's events, and it is built with
  // DOM calls and the Web Animations API only, so a page's CSP (which can
  // forbid inline styles and `innerHTML`) cannot stop it. It lives in a
  // closed shadow root so the page's CSS cannot reach it, and it leaves the
  // DOM a few seconds after the last action, with one timer that is cleared by
  // the next action, so an idle page runs nothing.
  const cursor = (() => {
    const NS = 'http://www.w3.org/2000/svg';
    const ACCENT = '#3b82f6';
    const calm = () => {
      try {
        return matchMedia('(prefers-reduced-motion: reduce)').matches;
      } catch (_) {
        return false;
      }
    };
    let host = null;
    let pointer = null;
    let at = null;
    let gone = 0;

    const build = () => {
      host = document.createElement('div');
      Object.assign(host.style, {
        all: 'initial',
        position: 'fixed',
        left: '0',
        top: '0',
        width: '0',
        height: '0',
        pointerEvents: 'none',
        zIndex: '2147483647',
      });
      const root = host.attachShadow({ mode: 'closed' });
      pointer = document.createElement('div');
      Object.assign(pointer.style, { position: 'absolute', left: '0', top: '0', willChange: 'transform' });
      const svg = document.createElementNS(NS, 'svg');
      for (const [name, value] of [['width', '16'], ['height', '22'], ['viewBox', '-1 -1 16 22']]) {
        svg.setAttribute(name, value);
      }
      svg.style.overflow = 'visible';
      svg.style.filter = 'drop-shadow(0 1px 2px rgba(0,0,0,.45))';
      const arrow = document.createElementNS(NS, 'path');
      arrow.setAttribute('d', 'M0 0 L0 17 L4.5 13 L7.5 20 L10.5 18.7 L7.6 12 L13.5 12 Z');
      arrow.setAttribute('fill', '#fff');
      arrow.setAttribute('stroke', '#111');
      arrow.setAttribute('stroke-width', '1.3');
      arrow.setAttribute('stroke-linejoin', 'round');
      svg.appendChild(arrow);
      pointer.appendChild(svg);
      root.appendChild(pointer);
      (document.documentElement || document.body).appendChild(host);
    };

    const place = (x, y) => `translate(${x - 1}px, ${y - 1}px)`;
    const frame = (ms) => new Promise((done) => setTimeout(done, ms));

    // Keep the pointer on screen until the agent has been quiet a while.
    const linger = () => {
      clearTimeout(gone);
      gone = setTimeout(() => {
        const fade = pointer && !calm() ? pointer.animate([{ opacity: 1 }, { opacity: 0 }], 300) : null;
        setTimeout(() => {
          if (host) host.remove();
          host = pointer = at = null;
        }, fade ? 300 : 0);
      }, 3000);
    };

    // Glide to (x, y) in viewport coordinates and settle there.
    const to = async (x, y) => {
      try {
        if (!host || !host.isConnected) {
          host = null;
          build();
          at = at || { x: Math.max(0, x - 80), y: Math.max(0, y + 60) };
          pointer.style.transform = place(at.x, at.y);
        }
        const from = at;
        const distance = Math.hypot(x - from.x, y - from.y);
        const ms = calm() ? 0 : Math.min(650, 160 + distance * 0.45);
        at = { x, y };
        pointer.style.transform = place(x, y);
        if (ms > 0 && distance > 1) {
          pointer.animate([{ transform: place(from.x, from.y) }, { transform: place(x, y) }], {
            duration: ms,
            easing: 'cubic-bezier(.3,.1,.2,1)',
          });
          await frame(ms + 30);
        }
        linger();
      } catch (_) {
        /* the cursor is decoration; an action goes on without it */
      }
    };

    // The press: the pointer dips and a ring spreads from its tip.
    const press = async () => {
      try {
        if (!host || !at || calm()) return;
        pointer.animate(
          [{ transform: place(at.x, at.y) + ' scale(1)' }, { transform: place(at.x, at.y) + ' scale(.82)' }, { transform: place(at.x, at.y) + ' scale(1)' }],
          { duration: 220, easing: 'ease-out' }
        );
        const ring = document.createElement('div');
        Object.assign(ring.style, {
          position: 'absolute',
          left: `${at.x - 20}px`,
          top: `${at.y - 20}px`,
          width: '40px',
          height: '40px',
          borderRadius: '50%',
          boxSizing: 'border-box',
          border: `2px solid ${ACCENT}`,
          background: 'rgba(59,130,246,.22)',
        });
        pointer.parentNode.appendChild(ring);
        ring.animate(
          [{ transform: 'scale(.2)', opacity: 1 }, { transform: 'scale(1.25)', opacity: 0 }],
          { duration: 520, easing: 'ease-out' }
        );
        setTimeout(() => ring.remove(), 540);
        await frame(110);
      } catch (_) {
        /* decoration only */
      }
    };

    const toElement = (el) => {
      const box = el.getBoundingClientRect();
      return to(box.x + box.width / 2, box.y + box.height / 2);
    };
    return { to, press, toElement };
  })();

  // ---- acting on the page ------------------------------------------------
  const mouse = (el, type) => {
    const box = el.getBoundingClientRect();
    const init = {
      bubbles: true,
      cancelable: true,
      view: window,
      button: 0,
      clientX: box.x + box.width / 2,
      clientY: box.y + box.height / 2,
    };
    el.dispatchEvent(
      type.startsWith('pointer')
        ? new PointerEvent(type, { ...init, pointerId: 1, isPrimary: true })
        : new MouseEvent(type, init)
    );
  };

  // Where the page (or the element being scrolled) is, so an agent knows
  // whether another step would move it.
  const whereIs = (target) => {
    const page = !target;
    const el = target || document.scrollingElement || document.documentElement;
    const viewH = page ? innerHeight : el.clientHeight;
    const viewW = page ? innerWidth : el.clientWidth;
    const maxY = Math.max(0, el.scrollHeight - viewH);
    const maxX = Math.max(0, el.scrollWidth - viewW);
    const y = Math.round(page ? scrollY : el.scrollTop);
    const x = Math.round(page ? scrollX : el.scrollLeft);
    return {
      x,
      y,
      max_y: Math.round(maxY),
      max_x: Math.round(maxX),
      viewport: { width: Math.round(viewW), height: Math.round(viewH) },
      at_top: y <= 0,
      at_bottom: y >= maxY - 1,
    };
  };

  const ops = {
    snapshot,
    html: (selector) => cut((selector ? resolve(selector) : document.documentElement).outerHTML),
    text: (selector) => cut((selector ? resolve(selector) : document.body).innerText),
    click: async (selector) => {
      const el = resolve(selector);
      if (!visible(el)) throw new Error('that element is not visible');
      if (el.disabled) throw new Error('that element is disabled');
      el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
      await cursor.toElement(el);
      mouse(el, 'pointerdown');
      mouse(el, 'mousedown');
      if (el.focus) el.focus();
      mouse(el, 'pointerup');
      mouse(el, 'mouseup');
      el.click();
      await cursor.press();
      return { clicked: describe(el) };
    },
    scroll: async (direction, pixels, selector, within) => {
      if (selector) {
        const el = resolve(selector);
        el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
        await cursor.toElement(el);
        return { into_view: describe(el), ...whereIs(null) };
      }
      const target = within ? resolve(within) : null;
      const now = whereIs(target);
      const stepY = pixels || Math.max(40, Math.round(now.viewport.height * 0.8));
      const stepX = pixels || Math.max(40, Math.round(now.viewport.width * 0.8));
      let top = now.y;
      let left = now.x;
      if (direction === 'down') top += stepY;
      else if (direction === 'up') top -= stepY;
      else if (direction === 'right') left += stepX;
      else if (direction === 'left') left -= stepX;
      else if (direction === 'top') top = 0;
      else if (direction === 'bottom') top = now.max_y;
      else throw new Error('scroll needs a direction or an element');
      // The pointer rests over what is being scrolled, as a hand on a wheel.
      const area = target ? target.getBoundingClientRect() : { x: 0, y: 0, width: innerWidth, height: innerHeight };
      await cursor.to(area.x + area.width / 2, area.y + area.height / 2);
      (target || window).scrollTo({ top: Math.max(0, top), left: Math.max(0, left), behavior: 'instant' });
      const after = whereIs(target);
      return { scrolled: { x: after.x - now.x, y: after.y - now.y }, ...after };
    },
    fill: async (selector, text) => {
      const el = resolve(selector);
      if (el.disabled || el.readOnly) throw new Error('that field cannot be edited');
      el.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: 'instant' });
      await cursor.toElement(el);
      if (el.focus) el.focus();
      if (el.isContentEditable) {
        el.textContent = text;
      } else if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement) {
        // The prototype's setter, so a framework that wraps `value` still sees
        // the change.
        const proto = Object.getPrototypeOf(el);
        Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, text);
      } else {
        throw new Error('that element is not a field');
      }
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      return { filled: describe(el) };
    },
    press: (key, selector) => {
      const el = selector ? resolve(selector) : document.activeElement || document.body;
      if (el.focus) el.focus();
      const init = { key, code: key.length === 1 ? 'Key' + key.toUpperCase() : key, bubbles: true, cancelable: true };
      const down = new KeyboardEvent('keydown', init);
      el.dispatchEvent(down);
      if (!down.defaultPrevented && (key.length === 1 || key === 'Enter')) {
        el.dispatchEvent(new KeyboardEvent('keypress', init));
      }
      el.dispatchEvent(new KeyboardEvent('keyup', init));
      // A synthetic Enter does not submit a form the way a real one does.
      if (key === 'Enter' && !down.defaultPrevented && el.form && el instanceof HTMLInputElement) {
        el.form.requestSubmit();
      }
      return { pressed: key, on: describe(el) };
    },
    wait: (selector, gone, timeoutMs) =>
      new Promise((resolveWait, rejectWait) => {
        const limit = Math.min(timeoutMs || 10000, 25000);
        let observer = null;
        let poll = 0;
        let timer = 0;
        const cleanup = () => {
          clearTimeout(timer);
          clearInterval(poll);
          if (observer) observer.disconnect();
        };
        const check = () => {
          try {
            const el = selector[0] === '@' ? refs[Number(selector.slice(1))] : document.querySelector(selector);
            const there = !!el && el.isConnected && visible(el);
            if (gone ? there : !there) return;
          } catch (error) {
            cleanup();
            rejectWait(error);
            return;
          }
          cleanup();
          resolveWait({ [gone ? 'gone' : 'found']: selector });
        };
        timer = setTimeout(() => {
          cleanup();
          rejectWait(new Error(`timed out after ${limit} ms waiting for ${selector}${gone ? ' to go' : ''}`));
        }, limit);
        observer = new MutationObserver(check);
        if (document.documentElement) {
          observer.observe(document.documentElement, { subtree: true, childList: true, attributes: true, characterData: true });
        }
        // Styles change without a mutation; look now and then as well.
        poll = setInterval(check, 200);
        check();
      }),
  };

  const plain = (value) => {
    if (value === undefined) return null;
    if (value instanceof Element) return describe(value);
    try {
      return JSON.parse(JSON.stringify(value));
    } catch (_) {
      return String(value);
    }
  };

  // Answer call `id` with whatever `work` produces, or with its failure.
  const answer = (id, work) => {
    Promise.resolve()
      .then(work)
      .then(
        (value) => {
          const message = { t: 'r', id, ok: true, v: plain(value) };
          const text = JSON.stringify(message);
          if (text.length > RESULT_MAX) {
            send({ t: 'r', id, ok: false, e: `the result is ${text.length} characters; return less of it` });
          } else {
            post(text);
          }
        },
        (error) => send({ t: 'r', id, ok: false, e: clip((error && error.message) || error, 500) })
      );
    return 'started';
  };

  Object.defineProperty(window, '__pilus', {
    value: Object.freeze({
      run: (id, name, args) => answer(id, () => ops[name](...args)),
      // `work` is built in the window's script, so it is not subject to the
      // page's CSP the way an `eval` here would be.
      eval: (id, work) => answer(id, work),
    }),
    enumerable: false,
  });
})();
