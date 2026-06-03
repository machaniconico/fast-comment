/**
 * fast-comment OBS overlay - bubble template
 * No dependencies. Plain ES2020 JS.
 *
 * URL params:
 *   ?template=bubble
 *   ?channel=<id>
 *   ?max=30
 *   ?ws=ws://127.0.0.1:11180/ws
 */

(function () {
  'use strict';

  // ---- Config from URL params ----
  const params = new URLSearchParams(location.search);
  const CHANNEL_FILTER = params.get('channel') || null;
  const MAX_CARDS = Math.min(positiveIntParam('max', 30), 30);
  const WS_URL   = buildWsUrl(params.get('ws') || 'ws://127.0.0.1:11180/ws');

  const log = document.getElementById('log');

  // ---- Active card tracking ----
  const cards = [];

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

    const card = buildCard(msg);
    addCard(card);
  }

  // ---- Build DOM card ----
  function buildCard(msg) {
    const card = document.createElement('div');
    card.className = 'bubble-card';

    if (isPaidMessage(msg)) {
      card.classList.add('paid');
    }

    const author = document.createElement('span');
    author.className = 'author';
    author.textContent = (msg.author && msg.author.name) || '';
    card.appendChild(author);

    const body = document.createElement('span');
    body.className = 'body';
    body.textContent = messageText(msg);
    card.appendChild(body);

    return card;
  }

  // ---- Card lifecycle ----
  function addCard(card) {
    while (cards.length >= MAX_CARDS) {
      evictOldest();
    }

    log.appendChild(card);
    cards.push(card);
  }

  function evictOldest() {
    const oldest = cards.shift();
    if (oldest && oldest.parentNode) {
      oldest.parentNode.removeChild(oldest);
    }
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

  function messageText(msg) {
    const fragments = msg.fragments || [];
    return fragments.map(fragmentText).join('');
  }

  function fragmentText(frag) {
    if (frag.type === 'text') return frag.text || '';
    return frag.name || '';
  }

  function isPaidMessage(msg) {
    const kind = msg && msg.kind;
    return kind === 'superchat' || kind === 'superChat' || kind === 'bits' || (msg && msg.amount != null);
  }

  // ---- Start ----
  connect();
})();
