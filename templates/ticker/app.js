/**
 * fast-comment OBS overlay - ticker template
 * No dependencies. Plain ES2020 JS.
 *
 * URL params:
 *   ?template=ticker
 *   ?channel=<id>
 *   ?max=20
 *   ?ws=ws://127.0.0.1:11180/ws
 */

(function () {
  'use strict';

  // ---- Config from URL params ----
  const params = new URLSearchParams(location.search);
  const CHANNEL_FILTER = params.get('channel') || null;
  const MAX_QUEUE = positiveIntParam('max', 20);
  const WS_URL   = buildWsUrl(params.get('ws') || 'ws://127.0.0.1:11180/ws');

  const tickerText = document.getElementById('ticker-text');

  // ---- Queue tracking ----
  const queue = [];
  let queueIndex = 0;

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

    const text = commentText(msg);
    if (!text) return;

    queue.push(text);
    while (queue.length > MAX_QUEUE) {
      queue.shift();
      queueIndex = Math.max(0, queueIndex - 1);
    }

    queueIndex = queue.length - 1;
    renderCurrent();
  }

  function renderCurrent() {
    if (queue.length === 0) {
      tickerText.textContent = '';
      return;
    }

    queueIndex = queueIndex % queue.length;
    tickerText.textContent = queue[queueIndex];
  }

  function showNext() {
    if (queue.length === 0) return;
    queueIndex = (queueIndex + 1) % queue.length;
    renderCurrent();
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

  function commentText(msg) {
    const author = (msg.author && msg.author.name) || '';
    const body = messageText(msg);
    if (!author) return body;
    if (!body) return author;
    return `${author}: ${body}`;
  }

  function messageText(msg) {
    const fragments = msg.fragments || [];
    return fragments.map(fragmentText).join('');
  }

  function fragmentText(frag) {
    if (frag.type === 'text') return frag.text || '';
    return frag.name || '';
  }

  // ---- Start ----
  setInterval(showNext, 4000);
  connect();
})();
