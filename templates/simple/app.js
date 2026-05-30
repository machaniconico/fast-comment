/**
 * fast-comment OBS overlay - simple template
 * No dependencies. Plain ES2020 JS.
 *
 * URL params:
 *   ?template=simple
 *   ?channel=<id>
 *   ?max=8
 *   ?ttl=12000
 *   ?font=100
 *   ?bg=0
 *   ?pos=bottom
 *   ?icon=1
 *   ?only=gift
 *   ?ws=ws://127.0.0.1:11180/ws
 */

(function () {
  'use strict';

  const params = new URLSearchParams(location.search);
  const CHANNEL_FILTER = params.get('channel') || null;
  const MAX_ROWS = positiveIntParam('max', 8);
  const TTL_MS = positiveIntParam('ttl', 12000);
  const FONT_SCALE = boundedNumberParam('font', 100, 50, 200) / 100;
  const BG_OPACITY = boundedNumberParam('bg', 0, 0, 100) / 100;
  const POSITION = params.get('pos') === 'top' ? 'top' : 'bottom';
  const SHOW_PLATFORM = params.get('icon') !== '0';
  const ONLY_GIFT = params.get('only') === 'gift';
  const WS_URL = buildWsUrl(params.get('ws') || 'ws://127.0.0.1:11180/ws');

  const overlay = document.getElementById('overlay');
  applyAppearance();
  const LEAVE_MS = cssDurationMs('--leave-ms', 220);

  const rows = [];
  let ws = null;
  let reconnectDelay = 1000;
  let stableTimer = null;

  function connect() {
    ws = new WebSocket(WS_URL);

    ws.addEventListener('open', () => {
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

  function handleMessage(msg) {
    if (CHANNEL_FILTER && msg.channel !== CHANNEL_FILTER) return;
    if (ONLY_GIFT && !isGiftMessage(msg)) return;

    const el = buildRow(msg);
    addRow(el);
  }

  function buildRow(msg) {
    const div = document.createElement('div');
    div.className = 'comment';
    const kind = msg.kind || 'normal';
    const isSystem = kind === 'system';

    if (kind === 'superChat') div.classList.add('superchat');
    if (kind === 'membership') div.classList.add('membership');
    if (kind === 'bits') div.classList.add('bits');
    if (isSystem) div.classList.add('system');

    const dot = document.createElement('span');
    dot.className = 'platform-dot ' + (msg.platform || '');
    div.appendChild(dot);

    const author = document.createElement('span');
    author.className = 'author';
    const color = msg.author && msg.author.displayColor;
    if (isSafeHexColor(color) && kind === 'normal') {
      author.style.color = color;
    }
    author.textContent = (msg.author && msg.author.name) || (isSystem ? 'System' : '');
    div.appendChild(author);

    const sep = document.createElement('span');
    sep.className = 'sep';
    sep.textContent = ':';
    div.appendChild(sep);

    if (msg.amount) {
      const amt = document.createElement('span');
      amt.className = 'amount';
      amt.textContent = msg.amount.rawText || '';
      div.appendChild(amt);
    }

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

  function addRow(el) {
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

  function boundedNumberParam(name, fallback, min, max) {
    const raw = params.get(name);
    if (raw === null) return fallback;

    const value = Number(raw);
    if (!Number.isFinite(value)) return fallback;
    return Math.min(max, Math.max(min, value));
  }

  function applyAppearance() {
    document.documentElement.style.setProperty('--font-scale', String(FONT_SCALE));
    document.documentElement.style.setProperty('--overlay-bg-opacity', String(BG_OPACITY));
    overlay.classList.toggle('pos-top', POSITION === 'top');
    overlay.classList.toggle('pos-bottom', POSITION !== 'top');
    overlay.classList.toggle('hide-platform', !SHOW_PLATFORM);
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

  function isGiftMessage(msg) {
    const kind = msg && msg.kind;
    return kind === 'superChat' || kind === 'membership' || kind === 'bits';
  }

  function cssDurationMs(name, fallback) {
    const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    const match = value.match(/^([0-9]*\.?[0-9]+)(ms|s)$/);
    if (!match) return fallback;

    const duration = Number(match[1]);
    if (!Number.isFinite(duration) || duration < 0) return fallback;
    return match[2] === 's' ? duration * 1000 : duration;
  }

  connect();
})();
