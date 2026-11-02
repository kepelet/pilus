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

  const ops = {
    snapshot,
    html: (selector) => cut((selector ? resolve(selector) : document.documentElement).outerHTML),
    text: (selector) => cut((selector ? resolve(selector) : document.body).innerText),
    click: (selector) => {
      const el = resolve(selector);
      if (!visible(el)) throw new Error('that element is not visible');
      if (el.disabled) throw new Error('that element is disabled');
      el.scrollIntoView({ block: 'center', inline: 'center' });
      mouse(el, 'pointerdown');
      mouse(el, 'mousedown');
      if (el.focus) el.focus();
      mouse(el, 'pointerup');
      mouse(el, 'mouseup');
      el.click();
      return { clicked: describe(el) };
    },
    fill: (selector, text) => {
      const el = resolve(selector);
      if (el.disabled || el.readOnly) throw new Error('that field cannot be edited');
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
