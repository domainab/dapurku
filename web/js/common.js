/* Dapurku — helper bersama (API, keranjang, notifikasi realtime). */
(function () {
  var DK = (window.DK = {});

  DK.esc = function (s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  };
  DK.rp = function (n) { return 'Rp' + Number(n || 0).toLocaleString('id-ID'); };
  DK.q = function (name) { return new URLSearchParams(location.search).get(name); };
  DK.jam = function (t) {
    if (!t) return '';
    var d = new Date(t * 1000);
    return String(d.getHours()).padStart(2, '0') + '.' + String(d.getMinutes()).padStart(2, '0');
  };

  DK.api = function (path, opts) {
    opts = opts || {};
    var init = { method: opts.method || 'GET', headers: {}, credentials: 'same-origin' };
    if (opts.body !== undefined) {
      init.method = opts.method || 'POST';
      init.headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(opts.body);
    }
    return fetch(path, init).then(function (r) {
      return r.text().then(function (txt) {
        var data = null;
        try { data = txt ? JSON.parse(txt) : null; } catch (e) { /* bukan JSON */ }
        if (!r.ok) {
          var err = new Error((data && data.error) || 'Permintaan gagal (' + r.status + ')');
          err.status = r.status;
          throw err;
        }
        return data;
      });
    });
  };

  /* ---------- penyimpanan lokal (aman kalau diblokir) ---------- */
  function lsGet(k, d) { try { var v = localStorage.getItem(k); return v == null ? d : JSON.parse(v); } catch (e) { return d; } }
  function lsSet(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch (e) { /* abaikan */ } }
  DK.ls = { get: lsGet, set: lsSet };

  /* ---------- meja & pesanan aktif ---------- */
  DK.getMeja = function () {
    var m = DK.q('meja');
    if (m) { lsSet('dk.meja', m); return m; }
    return lsGet('dk.meja', null);
  };
  DK.setMeja = function (m) { lsSet('dk.meja', m); };
  DK.activeOrder = function () { return lsGet('dk.order', null); };
  DK.setActiveOrder = function (o) { lsSet('dk.order', o); };

  /* ---------- keranjang ---------- */
  DK.cart = {
    lines: function () { return lsGet('dk.cart', []); },
    save: function (l) { lsSet('dk.cart', l); window.dispatchEvent(new CustomEvent('dk:cart')); },
    key: function (l) { return [l.menu_id, l.level == null ? '' : l.level, (l.extras || []).slice().sort().join('+'), l.catatan || ''].join('|'); },
    add: function (line) {
      var all = this.lines(), k = this.key(line), hit = null;
      all.forEach(function (x) { if (DK.cart.key(x) === k) hit = x; });
      if (hit) hit.qty += line.qty; else all.push(line);
      this.save(all);
    },
    setQty: function (idx, qty) {
      var all = this.lines();
      if (!all[idx]) return;
      if (qty <= 0) all.splice(idx, 1); else all[idx].qty = Math.min(50, qty);
      this.save(all);
    },
    clear: function () { this.save([]); },
    count: function () { return this.lines().reduce(function (a, l) { return a + l.qty; }, 0); },
    total: function () { return this.lines().reduce(function (a, l) { return a + l.qty * l.harga; }, 0); },
    qtyOf: function (menuId) { return this.lines().reduce(function (a, l) { return a + (l.menu_id === menuId ? l.qty : 0); }, 0); }
  };

  /* ---------- suara ---------- */
  DK.muted = function () { return lsGet('dk.mute', false); };
  DK.setMuted = function (v) { lsSet('dk.mute', !!v); };
  DK.chime = function (force) {
    if (!force && DK.muted()) return;
    try {
      var Ctx = window.AudioContext || window.webkitAudioContext;
      if (!Ctx) return;
      var ctx = DK._ctx || (DK._ctx = new Ctx());
      if (ctx.state === 'suspended') ctx.resume();
      var t = ctx.currentTime;
      [880, 1175].forEach(function (f, i) {
        var o = ctx.createOscillator(), g = ctx.createGain(), s = t + i * 0.16;
        o.type = 'sine'; o.frequency.value = f;
        g.gain.setValueAtTime(0.0001, s);
        g.gain.exponentialRampToValueAtTime(0.25, s + 0.02);
        g.gain.exponentialRampToValueAtTime(0.0001, s + 0.38);
        o.connect(g); g.connect(ctx.destination);
        o.start(s); o.stop(s + 0.42);
      });
    } catch (e) { /* browser memblokir audio sebelum ada interaksi */ }
  };

  /* ---------- toast ---------- */
  var BELL = '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 9a6 6 0 0 1 12 0c0 5 2 6 2 6H4s2-1 2-6z"></path><path d="M10 19a2 2 0 0 0 4 0"></path></svg>';
  DK.toast = function (n) {
    var wrap = document.getElementById('toast-wrap');
    if (!wrap) {
      wrap = document.createElement('div');
      wrap.id = 'toast-wrap';
      wrap.className = 'toast-wrap';
      wrap.setAttribute('aria-live', 'polite');
      document.body.appendChild(wrap);
    }
    var el = document.createElement(n.href ? 'a' : 'div');
    if (n.href) el.href = n.href;
    el.className = 'toast';
    el.setAttribute('role', 'status');
    el.innerHTML = '<span class="ic">' + BELL + '</span><span><b>' + DK.esc(n.judul) + '</b><span class="t">' + DK.esc(n.isi || '') + '</span></span>';
    wrap.appendChild(el);
    function close() { el.classList.add('out'); setTimeout(function () { el.remove(); }, 260); }
    el.addEventListener('click', function () { if (!n.href) close(); });
    setTimeout(close, n.ms || 7000);
  };

  /* ---------- notifikasi browser ---------- */
  DK.notifPermission = function () { return 'Notification' in window ? Notification.permission : 'unsupported'; };
  DK.askNotif = function () {
    if (!('Notification' in window)) return Promise.resolve('unsupported');
    return Notification.requestPermission();
  };
  DK.systemNotify = function (n) {
    try {
      if ('Notification' in window && Notification.permission === 'granted' && document.hidden) {
        new Notification(n.judul, { body: n.isi, tag: 'dapurku-' + n.id });
      }
    } catch (e) { /* abaikan */ }
  };

  /* ---------- SSE ---------- */
  DK.listen = function (url, onNotif) {
    if (!('EventSource' in window)) return null;
    var es = new EventSource(url);
    es.addEventListener('notif', function (ev) {
      try { onNotif(JSON.parse(ev.data)); } catch (e) { /* abaikan data rusak */ }
    });
    return es;
  };

  /* Pantau satu pesanan: toast + bunyi + notifikasi browser untuk pembeli. */
  DK.watchOrder = function (token, opts) {
    opts = opts || {};
    return DK.listen('/api/orders/' + encodeURIComponent(token) + '/events', function (n) {
      DK.chime();
      DK.systemNotify(n);
      if (!opts.noToast) DK.toast({ judul: n.judul, isi: n.isi, href: opts.href });
      window.dispatchEvent(new CustomEvent('dk:notif', { detail: n }));
      if (opts.onNotif) opts.onNotif(n);
    });
  };

  /* ---------- login staf ---------- */
  DK.requireLogin = function () {
    return DK.api('/api/me').catch(function (e) {
      if (e.status === 401) {
        location.href = '/login?next=' + encodeURIComponent(location.pathname);
      }
      throw e;
    });
  };
  DK.logout = function () {
    return DK.api('/api/logout', { method: 'POST', body: {} }).then(function () { location.href = '/login'; });
  };

  DK.icons = {
    plus: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" aria-hidden="true"><path d="M12 5v14M5 12h14"></path></svg>',
    check: '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5 12.5l4.5 4.5L19 7.5"></path></svg>',
    arrow: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5 12h14M13 6l6 6-6 6"></path></svg>',
    back: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M19 12H5M11 6l-6 6 6 6"></path></svg>',
    close: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"><path d="M6 6l12 12M18 6L6 18"></path></svg>',
    search: '<svg width="19" height="19" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><circle cx="11" cy="11" r="6.5"></circle><path d="M16 16l4 4"></path></svg>',
    table: '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 9h16M6 9v10M18 9v10M9 9V6h6v3"></path></svg>',
    pot: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="M8 7c0-1 1-1.5 1-2.5M12 7c0-1 1-1.5 1-2.5M16 7c0-1 1-1.5 1-2.5"></path><path d="M3 11h18a9 9 0 0 1-18 0z"></path></svg>',
    download: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 4v11M7 10l5 5 5-5M5 20h14"></path></svg>'
  };
})();
