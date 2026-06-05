/**
 * fast-comment OBS overlay - ranking template
 * Speaker comment-count ranking. No dependencies. Plain ES2020 JS.
 *
 * URL params:
 *   ?template=ranking
 *   ?channel=<id>
 *   ?max=10
 *   ?font=100
 *   ?bg=0
 *   ?pos=bottom
 *   ?ws=ws://127.0.0.1:11180/ws
 */

(function () {
  'use strict';

  // ---- Config from URL params ----
  const params = new URLSearchParams(location.search);
  const CHANNEL_FILTER = params.get('channel') || null;
  const MAX_ROWS = Math.min(Math.max(positiveIntParam('max', 10), 1), 50);
  const FONT_SCALE = boundedNumberParam('font', 100, 50, 200) / 100;
  const BG_OPACITY = boundedNumberParam('bg', 0, 0, 100) / 100;
  const POSITION = params.get('pos') === 'top' ? 'top' : 'bottom';
  const WS_URL = buildWsUrl(params.get('ws') || 'ws://127.0.0.1:11180/ws');

  const overlay = document.getElementById('overlay');
  const listEl = document.getElementById('ranking-list');

  // key -> { name, count }
  const speakers = new Map();

  applyAppearance();
  renderRanking();

  // ---- WebSocket ----
  let ws = null;

  function connect() {
    ws = new WebSocket(WS_URL);

    ws.addEventListener('message', (ev) => {
      let batch;
      try {
        batch = JSON.parse(ev.data);
      } catch {
        return;
      }
      if (!Array.isArray(batch)) batch = [batch];
      batch.forEach(handleMessage);
      renderRanking();
    });

    ws.addEventListener('close', () => {
      ws = null;
      setTimeout(connect, 1500);
    });

    ws.addEventListener('error', () => {
      ws && ws.close();
    });
  }

  // ---- Message handler ----
  function handleMessage(msg) {
    if (!msg || typeof msg !== 'object') return;
    if (CHANNEL_FILTER && msg.channel !== CHANNEL_FILTER) return;

    const author = msg.author && typeof msg.author === 'object' ? msg.author : null;
    if (!author) return;

    const name = typeof author.name === 'string' ? author.name.trim() : '';
    const id = typeof author.id === 'string' ? author.id : '';
    if (name === '' && id === '') return;

    const platform = typeof msg.platform === 'string' ? msg.platform : '';
    const key = platform + ':' + (id || name);

    const current = speakers.get(key) || { name: name || '(名無し)', count: 0 };
    current.count += 1;
    if (name !== '') current.name = name;
    speakers.set(key, current);
  }

  // ---- Render ----
  function renderRanking() {
    listEl.textContent = '';

    const rows = Array.from(speakers.values())
      .map((entry, index) => ({ entry, index }))
      .sort(
        (a, b) =>
          b.entry.count - a.entry.count ||
          a.entry.name.localeCompare(b.entry.name, 'ja-JP') ||
          a.index - b.index
      )
      .slice(0, MAX_ROWS);

    if (rows.length === 0) {
      const empty = document.createElement('li');
      empty.className = 'ranking-empty';
      empty.textContent = 'まだ発言がありません';
      listEl.appendChild(empty);
      return;
    }

    rows.forEach((row, i) => {
      const li = document.createElement('li');
      li.className = 'ranking-row';

      const rank = document.createElement('span');
      rank.className = 'rank rank-' + (i + 1);
      rank.textContent = String(i + 1);
      li.appendChild(rank);

      const speaker = document.createElement('span');
      speaker.className = 'speaker-name';
      speaker.textContent = row.entry.name;
      li.appendChild(speaker);

      const count = document.createElement('span');
      count.className = 'speaker-count';
      count.textContent = String(row.entry.count);
      li.appendChild(count);

      listEl.appendChild(li);
    });
  }

  // ---- Appearance ----
  function applyAppearance() {
    document.documentElement.style.setProperty('--font-scale', String(FONT_SCALE));
    document.documentElement.style.setProperty('--overlay-bg-opacity', String(BG_OPACITY));
    overlay.classList.toggle('pos-top', POSITION === 'top');
    overlay.classList.toggle('pos-bottom', POSITION !== 'top');
  }

  // ---- Helpers ----
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

  function buildWsUrl(base) {
    const url = new URL(base, location.href);
    if (CHANNEL_FILTER) {
      url.searchParams.set('channel', CHANNEL_FILTER);
    }
    return url.toString();
  }

  // ---- Start ----
  connect();
})();
