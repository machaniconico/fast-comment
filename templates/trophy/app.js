/**
 * fast-comment OBS overlay - trophy template
 * No dependencies. Plain ES2020 JS.
 *
 * URL params:
 *   ?template=trophy
 *   ?ws=ws://127.0.0.1:11180/ws   override WS endpoint
 *   ?duration=<seconds>           how long one unlock stays visible (default 5)
 *
 * Only `{"achievements":[…]}` frames are used. Chat batches are ignored.
 */

(function () {
  'use strict';

  const SVG_NS = 'http://www.w3.org/2000/svg';
  const LEAVE_MS = 300;

  const TIERS = {
    bronze: { label: 'ブロンズ', symbol: 'tier-bronze' },
    silver: { label: 'シルバー', symbol: 'tier-silver' },
    gold: { label: 'ゴールド', symbol: 'tier-gold' },
    platinum: { label: 'プラチナ', symbol: 'tier-platinum' },
  };

  // ---- fc-trophy:begin ----
  // Pure helpers. templates/trophy.test.mjs evaluates this block without a DOM.
  const TROPHY_DEFAULT_DURATION_MS = 5000;

  const TROPHY_MAX_DURATION_SECONDS = 3600;
  // 一斉解除で待ち行列が際限なく伸びないよう、古い方から捨てる。
  const TROPHY_MAX_QUEUE = 50;

  function trophyDurationMs(raw) {
    if (raw === null || raw === undefined) return TROPHY_DEFAULT_DURATION_MS;
    const text = String(raw).trim();
    if (text === '') return TROPHY_DEFAULT_DURATION_MS;
    const seconds = Number(text);
    if (!Number.isFinite(seconds) || seconds <= 0) return TROPHY_DEFAULT_DURATION_MS;
    // setTimeout は 2^31-1 ms を超えると即時発火するので、1 時間で頭打ちにする。
    return Math.min(seconds, TROPHY_MAX_DURATION_SECONDS) * 1000;
  }

  // Only an object with an "achievements" array qualifies. Chat arrays and
  // moderation frames are intentionally ignored.
  function trophyFrameUnlocks(frame) {
    if (!frame || typeof frame !== 'object' || Array.isArray(frame)) return [];
    if (!Array.isArray(frame.achievements)) return [];
    return frame.achievements.filter(function (item) {
      return !!item && typeof item === 'object' && !Array.isArray(item);
    });
  }

  function trophyViewerLabel(subject) {
    if (subject && subject.kind === 'streamer') return '配信者';
    const name = subject && typeof subject.name === 'string' ? subject.name.trim() : '';
    return name || '視聴者';
  }

  // FIFO queue: unlocks are shown strictly in arrival order, one at a time.
  function trophyCreateQueue() {
    const items = [];
    return {
      enqueue: function (unlocks, durationMs) {
        unlocks.forEach(function (unlock) {
          items.push({ unlock: unlock, durationMs: durationMs });
          if (items.length > TROPHY_MAX_QUEUE) items.shift();
        });
      },
      shift: function () {
        return items.length > 0 ? items.shift() : null;
      },
      size: function () {
        return items.length;
      },
    };
  }
  // ---- fc-trophy:end ----

  const params = new URLSearchParams(location.search);
  const ITEM_DURATION_MS = trophyDurationMs(params.get('duration'));
  const WS_URL = buildWsUrl(params.get('ws') || 'ws://127.0.0.1:11180/ws');

  const overlay = document.getElementById('overlay');
  const queue = trophyCreateQueue();
  let showing = false;

  // ---- WebSocket ----
  let ws = null;
  let reconnectDelay = 1000;
  let stableTimer = null;

  function connect() {
    ws = new WebSocket(WS_URL);

    ws.addEventListener('open', () => {
      // Reset backoff only after the connection has been stable for 5 s.
      stableTimer = setTimeout(() => {
        reconnectDelay = 1000;
      }, 5000);
    });

    ws.addEventListener('message', (ev) => {
      let frame;
      try {
        frame = JSON.parse(ev.data);
      } catch {
        return;
      }
      const unlocks = trophyFrameUnlocks(frame);
      if (unlocks.length === 0) return;
      queue.enqueue(unlocks, ITEM_DURATION_MS);
      showNext();
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

  function showNext() {
    if (showing) return;
    const item = queue.shift();
    if (!item) return;
    showing = true;
    showUnlock(item.unlock, item.durationMs);
  }

  function showUnlock(unlock, durationMs) {
    const tier = Object.prototype.hasOwnProperty.call(TIERS, unlock.tier) ? unlock.tier : 'bronze';
    const meta = TIERS[tier];

    const card = document.createElement('article');
    card.className = 'trophy-card tier-' + tier;
    card.setAttribute('role', 'status');

    const badge = document.createElement('div');
    badge.className = 'tier-badge';
    badge.setAttribute('aria-hidden', 'true');

    const svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('class', 'tier-icon');
    svg.setAttribute('viewBox', '0 0 48 48');
    const use = document.createElementNS(SVG_NS, 'use');
    use.setAttribute('href', '#' + meta.symbol);
    svg.appendChild(use);
    badge.appendChild(svg);

    const copy = document.createElement('div');
    copy.className = 'trophy-copy';

    const kicker = document.createElement('div');
    kicker.className = 'trophy-kicker';
    const tierLabel = document.createElement('span');
    tierLabel.textContent = meta.label;
    const unlockLabel = document.createElement('span');
    unlockLabel.className = 'unlock-label';
    unlockLabel.textContent = '実績解除';
    kicker.appendChild(tierLabel);
    kicker.appendChild(unlockLabel);

    const name = document.createElement('div');
    name.className = 'trophy-name';
    name.textContent = typeof unlock.name === 'string' && unlock.name.trim()
      ? unlock.name.trim()
      : '実績解除';

    const who = document.createElement('div');
    who.className = 'trophy-who';
    const whoName = document.createElement('span');
    whoName.textContent = trophyViewerLabel(unlock.subject);
    who.appendChild(whoName);

    copy.appendChild(kicker);
    copy.appendChild(name);
    copy.appendChild(who);
    card.appendChild(badge);
    card.appendChild(copy);
    overlay.appendChild(card);

    setTimeout(() => {
      card.classList.add('leaving');
      setTimeout(() => {
        if (card.parentNode) card.parentNode.removeChild(card);
        showing = false;
        showNext();
      }, LEAVE_MS);
    }, durationMs);
  }

  function buildWsUrl(base) {
    const url = new URL(base, location.href);
    // 実績解除の {"achievements":[…]} フレームを受け取る(opt-in)。
    url.searchParams.set('achievementEvents', '1');
    return url.toString();
  }

  // ---- Start ----
  connect();
})();
