/**
 * fast-comment OBS overlay — default template
 * No dependencies. Plain ES2020 JS.
 *
 * URL params:
 *   ?template=default   (unused here, consumed by axum router)
 *   ?channel=<id>        optional filter: only show this channel
 *   ?max=8               max visible rows (default 8)
 *   ?ttl=12000           ms before a row auto-exits (default 12000)
 *   ?ws=ws://127.0.0.1:11180/ws  override WS endpoint
 */

(function () {
  'use strict';

  // ---- Config from URL params ----
  const params = new URLSearchParams(location.search);
  const CHANNEL_FILTER = params.get('channel') || null;
  const MAX_ROWS = positiveIntParam('max', 8);
  const TTL_MS   = positiveIntParam('ttl', 12000);
  const WS_URL   = buildWsUrl(params.get('ws') || 'ws://127.0.0.1:11180/ws');

  const overlay = document.getElementById('overlay');
  const LEAVE_MS = cssDurationMs('--leave-ms', 400);

  // ---- Active row tracking ----
  // Each entry: { el, timerId }
  const rows = [];

  // ---- WebSocket ----
  let ws = null;
  let reconnectDelay = 1000;
  let stableTimer = null;

  function connect() {
    ws = new WebSocket(WS_URL);

    ws.addEventListener('open', () => {
      // Reset backoff only after the connection has been stable for 5 s.
      // This prevents a fast open→close flapping loop from keeping the delay
      // pinned at 1000 ms (the timer is cancelled in the close handler).
      stableTimer = setTimeout(() => {
        reconnectDelay = 1000;
      }, 5000);
    });

    ws.addEventListener('message', (ev) => {
      let batch;
      try {
        batch = JSON.parse(ev.data);
      } catch {
        return;
      }
      if (!Array.isArray(batch)) batch = [batch];
      batch.forEach(handleMessage);
    });

    ws.addEventListener('close', () => {
      // Cancel the stability timer so a premature close does not reset the
      // backoff before we have had a genuinely stable connection.
      clearTimeout(stableTimer);
      stableTimer = null;
      ws = null;
      setTimeout(connect, reconnectDelay);
      reconnectDelay = Math.min(reconnectDelay * 2, 16000);
    });

    ws.addEventListener('error', () => {
      ws && ws.close();
    });
  }

  // ---- Message handler ----
  function handleMessage(msg) {
    // Channel filter
    if (CHANNEL_FILTER && msg.channel !== CHANNEL_FILTER) return;

    const el = buildRow(msg);
    addRow(el);
  }

  // ---- Build DOM row ----
  function buildRow(msg) {
    const div = document.createElement('div');
    div.className = 'comment';
    const kind = msg.kind || 'normal';
    const isSystem = kind === 'system';

    // Kind class for CSS highlights
    if (kind === 'superChat')  div.classList.add('superchat');
    if (kind === 'membership') div.classList.add('membership');
    if (kind === 'bits')       div.classList.add('bits');
    if (isSystem) div.classList.add('system');

    // Platform dot
    const dot = document.createElement('span');
    dot.className = 'platform-dot ' + (msg.platform || '');
    div.appendChild(dot);

    // Badges
    const badges = (msg.author && msg.author.badges) || [];
    badges.forEach((badge) => {
      if (isHttpUrl(badge.imageUrl)) {
        const img = document.createElement('img');
        img.className = 'badge-img';
        img.src = badge.imageUrl;
        img.alt = badge.label || '';
        img.title = badge.label || '';
        div.appendChild(img);
      }
    });

    // Author name
    const author = document.createElement('span');
    author.className = 'author';
    const color = msg.author && msg.author.displayColor;
    if (isSafeHexColor(color) && kind === 'normal') {
      author.style.color = color;
    }
    author.textContent = (msg.author && msg.author.name) || (isSystem ? 'System' : '');
    div.appendChild(author);

    // Separator
    const sep = document.createElement('span');
    sep.className = 'sep';
    sep.textContent = ': ';
    div.appendChild(sep);

    // Amount (SuperChat / Bits)
    if (msg.amount) {
      const amt = document.createElement('span');
      amt.className = 'amount';
      amt.textContent = msg.amount.rawText || '';
      div.appendChild(amt);
    }

    // Fragments
    const fragWrap = document.createElement('span');
    fragWrap.className = 'fragments';
    const fragments = msg.fragments || [];
    fragments.forEach((frag) => {
      if (frag.type === 'text') {
        fragWrap.appendChild(document.createTextNode(frag.text));
      } else if (frag.type === 'emote' && isHttpUrl(frag.url)) {
        const img = document.createElement('img');
        img.className = 'emote';
        img.src = frag.url;
        img.alt = frag.name || '';
        img.title = frag.name || '';
        fragWrap.appendChild(img);
      }
    });
    div.appendChild(fragWrap);

    return div;
  }

  // ---- Row lifecycle ----
  function addRow(el) {
    // Evict oldest if over limit
    while (rows.length >= MAX_ROWS) {
      evictOldest();
    }

    overlay.appendChild(el);

    const timerId = setTimeout(() => removeRow(el), TTL_MS);
    rows.push({ el, timerId });
  }

  function evictOldest() {
    const oldest = rows.shift();
    if (!oldest) return;
    clearTimeout(oldest.timerId);
    startLeave(oldest.el);
  }

  function removeRow(el) {
    const idx = rows.findIndex((r) => r.el === el);
    if (idx !== -1) {
      clearTimeout(rows[idx].timerId);
      rows.splice(idx, 1);
    }
    startLeave(el);
  }

  function startLeave(el) {
    el.classList.add('leaving');
    // Remove from DOM after the CSS-driven exit animation.
    setTimeout(() => {
      if (el.parentNode) el.parentNode.removeChild(el);
    }, LEAVE_MS);
  }

  function positiveIntParam(name, fallback) {
    const raw = params.get(name);
    if (raw === null) return fallback;

    const value = Number(raw);
    if (!Number.isFinite(value) || value <= 0) return fallback;
    const intValue = Math.floor(value);
    return intValue > 0 ? intValue : fallback;
  }

  function buildWsUrl(base) {
    const url = new URL(base, location.href);
    if (CHANNEL_FILTER) {
      url.searchParams.set('channel', CHANNEL_FILTER);
    }
    return url.toString();
  }

  function isSafeHexColor(value) {
    return typeof value === 'string' && /^#[0-9a-fA-F]{3,8}$/.test(value);
  }

  function isHttpUrl(value) {
    if (typeof value !== 'string') return false;

    try {
      const url = new URL(value);
      return url.protocol === 'http:' || url.protocol === 'https:';
    } catch {
      return false;
    }
  }

  function cssDurationMs(name, fallback) {
    const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    const match = value.match(/^([0-9]*\.?[0-9]+)(ms|s)$/);
    if (!match) return fallback;

    const duration = Number(match[1]);
    if (!Number.isFinite(duration) || duration < 0) return fallback;
    return match[2] === 's' ? duration * 1000 : duration;
  }

  // ---- Start ----
  connect();
})();
