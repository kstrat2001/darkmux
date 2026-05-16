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

  if (!customElements.get('dx-shell')) {
    customElements.define('dx-shell', DxShell);
  }
})();
