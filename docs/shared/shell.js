/* <dx-shell> — vanilla custom element for darkmux's shared chrome.
 *
 * Pages mount the shell at the top of <body>:
 *
 *   <dx-shell active-tab="flow">
 *     <button class="dx-pill" slot="pills">...</button>
 *   </dx-shell>
 *
 * Children with slot="pills" get relocated into the shell's pill row on
 * connectedCallback. The shell renders brand + tabs into its own light
 * DOM (no shadow root — so global styles in shell.css apply uniformly).
 *
 * Static-page assumption: no disconnectedCallback. If a future page
 * adds SPA navigation that tears down + re-creates <dx-shell> nodes
 * within a single document load, add listener cleanup here.
 *
 * Per #168. */

(function () {
  'use strict';

  const TABS = [
    { id: 'flow',     href: '../flow/',     label: 'Flow' },
    { id: 'topology', href: '../topology/', label: 'Topology' },
    { id: 'lab',      href: '../lab/',      label: 'Lab' },
  ];

  /* Store-status pill (#170): polls the daemon's /flow-status endpoint
   * every 30s and renders the substrate's health (ok/warn/fail) as a
   * compact pill that opens a detail modal on click.
   *
   * Daemon discovery: defaults to the well-known local-bind address.
   * Pages can override per-instance with `<dx-shell daemon-base="…">`
   * or document-wide with `<meta name="darkmux-daemon-base" content="…">`.
   * Fail-soft — if the daemon is offline, the pill shows "no daemon"
   * and clicking opens a modal with the start command.  */
  const DEFAULT_DAEMON_BASE = 'http://127.0.0.1:8765';
  const POLL_INTERVAL_MS = 30_000;

  function resolveDaemonBase(el) {
    if (el.hasAttribute('daemon-base')) return el.getAttribute('daemon-base');
    const meta = document.querySelector('meta[name="darkmux-daemon-base"]');
    if (meta && meta.content) return meta.content;
    return DEFAULT_DAEMON_BASE;
  }

  class DxShell extends HTMLElement {
    constructor() {
      super();
      this._wired = false;
    }

    connectedCallback() {
      if (this._wired) return;
      this._wired = true;

      const active = this.getAttribute('active-tab') || '';
      const pillChildren = Array.from(this.querySelectorAll('[slot="pills"]'));
      pillChildren.forEach((el) => el.removeAttribute('slot'));

      this.innerHTML = '';

      const brand = document.createElement('a');
      brand.className = 'dx-shell-brand';
      brand.href = 'https://darkmux.com/';
      brand.title = 'darkmux home';
      brand.innerHTML = '<span class="accent">darkmux</span>';
      this.appendChild(brand);

      const nav = document.createElement('nav');
      nav.className = 'dx-shell-tabs';
      nav.setAttribute('aria-label', 'darkmux views');
      TABS.forEach((t) => {
        const a = document.createElement('a');
        a.className = 'dx-shell-tab';
        a.href = t.href;
        a.textContent = t.label;
        a.dataset.tabId = t.id;
        if (t.id === active) a.setAttribute('aria-current', 'page');
        nav.appendChild(a);
      });
      this.appendChild(nav);

      const pillRow = document.createElement('div');
      pillRow.className = 'dx-shell-pills';
      pillChildren.forEach((el) => pillRow.appendChild(el));
      this.appendChild(pillRow);
      this._pillRow = pillRow;

      this._installStorePill();
    }

    /** Build + install the store-status pill in the pill row, start polling. */
    _installStorePill() {
      if (this.hasAttribute('no-store-pill')) return; // opt-out for tests

      this._daemonBase = resolveDaemonBase(this);

      const pill = document.createElement('button');
      pill.type = 'button';
      pill.id = 'dx-store-pill';
      pill.className = 'dx-pill dx-store-pill';
      pill.title = 'flow substrate status — click for details';
      pill.innerHTML = '<span class="dx-store-dot"></span><span class="dx-store-label">store: …</span>';
      pill.addEventListener('click', () => this._openStoreModal());
      this._pillRow.insertBefore(pill, this._pillRow.firstChild);
      this._storePill = pill;
      this._storeData = null;

      // Await the initial poll before scheduling the interval so the
      // page-load fetch can't race a later tick and produce out-of-order
      // renders (QA S4). Monotonic request id guards against any further
      // late-resolver interleaving.
      this._storePollSeq = 0;
      this._pollStoreStatus().finally(() => {
        this._storePollTimer = setInterval(() => this._pollStoreStatus(), POLL_INTERVAL_MS);
      });
    }

    async _pollStoreStatus() {
      const myId = ++this._storePollSeq;
      try {
        const r = await fetch(this._daemonBase + '/flow-status', { cache: 'no-store' });
        if (!r.ok) throw new Error('http ' + r.status);
        const data = await r.json();
        if (myId < this._storePollSeq) return; // a newer poll already landed
        this._storeData = data;
        this._renderStorePill(data);
      } catch (err) {
        if (myId < this._storePollSeq) return;
        this._storeData = null;
        this._renderStorePillOffline();
      }
    }

    _renderStorePill(data) {
      const pill = this._storePill;
      if (!pill) return;
      const state = (data && data.overall_state) || 'unknown';
      pill.classList.remove('good', 'warn', 'bad', 'offline');
      const label = pill.querySelector('.dx-store-label');
      if (state === 'ok')   { pill.classList.add('good'); label.textContent = 'store: ok'; }
      else if (state === 'warn') { pill.classList.add('warn'); label.textContent = 'store: warn'; }
      else if (state === 'fail') { pill.classList.add('bad');  label.textContent = 'store: fail'; }
      else                       { pill.classList.add('offline'); label.textContent = 'store: ?'; }
    }

    _renderStorePillOffline() {
      const pill = this._storePill;
      if (!pill) return;
      pill.classList.remove('good', 'warn', 'bad');
      pill.classList.add('offline');
      const label = pill.querySelector('.dx-store-label');
      label.textContent = 'store: no daemon';
    }

    _openStoreModal() {
      const existing = document.getElementById('dx-store-modal');
      if (existing) { existing.remove(); return; }
      const data = this._storeData;
      const bg = document.createElement('div');
      bg.id = 'dx-store-modal';
      bg.className = 'dx-modal-bg';

      // Single close path so the document-level Escape listener always
      // gets removed regardless of how the modal was dismissed (QA S2).
      const esc = (e) => { if (e.key === 'Escape') closeModal(); };
      const closeModal = () => {
        document.removeEventListener('keydown', esc);
        bg.remove();
      };
      bg.addEventListener('click', (e) => { if (e.target === bg) closeModal(); });

      const inner = document.createElement('div');
      inner.className = 'dx-modal';
      const close = document.createElement('button');
      close.className = 'dx-modal-close';
      close.type = 'button';
      close.textContent = '✕';
      close.addEventListener('click', closeModal);
      inner.appendChild(close);
      const h = document.createElement('h2');
      h.textContent = 'flow substrate status';
      inner.appendChild(h);
      const body = document.createElement('div');
      body.className = 'dx-modal-body';
      if (!data) {
        body.innerHTML = '<p>No data — the local daemon is not reachable.</p>' +
          '<p>Start it with <code>darkmux serve</code> on this machine, then reopen this modal.</p>';
      } else {
        // Every value-from-`data` substitution goes through escapeHtml.
        // Even fields the operator thinks are numbers (xlen, day_files,
        // total_bytes) get coerced via String() + escape because the JSON
        // returned by `/flow-status` is third-party-controlled when the
        // daemon-base attribute points elsewhere (QA S1).
        const safe = (v) => escapeHtml(v == null ? '' : String(v));
        const redisRow = data.redis ? (
          `  <dt>redis</dt><dd>${data.redis.reachable ? '✓ reachable' : '✗ unreachable'} · ` +
          `xlen ${safe(data.redis.xlen ?? '?')} / max_len ${safe(data.redis.max_len ?? 'unbounded')} · ` +
          `<code>${safe(data.redis.url)}</code></dd>`
        ) : '  <dt>redis</dt><dd>(not configured)</dd>';
        body.innerHTML =
          '<dl class="dx-kv">' +
          `  <dt>state</dt><dd>${safe(data.overall_state)}</dd>` +
          `  <dt>schema</dt><dd>${safe(data.schema_version)}</dd>` +
          `  <dt>composition</dt><dd><code>${safe(data.sinks?.composition || '?')}</code></dd>` +
          redisRow +
          `  <dt>disk</dt><dd>${safe(data.disk?.day_files ?? 0)} day file(s), ${safe(formatBytes(data.disk?.total_bytes))} · <code>${safe(data.disk?.flows_dir || '?')}</code></dd>` +
          `  <dt>schema skew</dt><dd>${data.schema?.skew_detected ? '⚠ ' + safe(data.schema?.skew_reason || 'detected') : '✓ none'}</dd>` +
          '</dl>';
        if (data.warn_reasons?.length) {
          body.innerHTML += '<p class="dx-warn"><strong>warn:</strong> ' +
            data.warn_reasons.map(safe).join(', ') + '</p>';
        }
        if (data.fail_reasons?.length) {
          body.innerHTML += '<p class="dx-fail"><strong>fail:</strong> ' +
            data.fail_reasons.map(safe).join(', ') + '</p>';
        }
        body.innerHTML += '<p class="dx-hint">CLI: <code>darkmux flow status</code> for the full breakdown.</p>';
      }
      inner.appendChild(body);
      bg.appendChild(inner);
      document.body.appendChild(bg);
      document.addEventListener('keydown', esc);
    }

    /** Page API: add a pill to the row. */
    addPill(el) {
      if (!this._pillRow) this.connectedCallback();
      this._pillRow.appendChild(el);
    }

    /** Page API: lookup a pill by id within the shell's row. */
    getPill(id) {
      return this._pillRow ? this._pillRow.querySelector('#' + id) : null;
    }
  }

  function escapeHtml(s) {
    if (s == null) return '';
    return String(s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }

  function formatBytes(n) {
    // Coerce defensively — the value comes from third-party JSON when
    // <dx-shell daemon-base> points away from the local daemon. Non-finite
    // input returns the same '?' the modal uses elsewhere.
    const num = Number(n);
    if (!Number.isFinite(num) || num <= 0) return '0 B';
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    let i = 0; let v = num;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
    return v.toFixed(v >= 10 || i === 0 ? 0 : 1) + ' ' + units[i];
  }

  if (!customElements.get('dx-shell')) {
    customElements.define('dx-shell', DxShell);
  }
})();
