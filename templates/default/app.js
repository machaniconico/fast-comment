/**
 * fast-comment OBS overlay — default template
 * No dependencies. Plain ES2020 JS.
 *
 * URL params:
 *   ?avatar=1           show author avatar (default 0)
 *   ?template=default   (unused here, consumed by axum router)
 *   ?channel=<id>        optional filter: only show this channel
 *   ?max=8               max visible rows (default 8)
 *   ?ttl=12000           ms before a row auto-exits (default 12000)
 *   ?font=100            font scale percent (default 100)
 *   ?bg=0                overlay background opacity percent (default 0)
 *   ?pos=bottom          top or bottom (default bottom)
 *   ?icon=1              show platform dot: 1/0 (default 1)
 *   ?only=gift           show SuperChat/Bits/membership only
 *   ?ws=ws://127.0.0.1:11180/ws  override WS endpoint
 */

(function () {
  'use strict';

  // ---- Config from URL params ----
  const params = new URLSearchParams(location.search);
  const SHOW_AVATAR = params.get('avatar') === '1';
  const CHANNEL_FILTER = params.get('channel') || null;
  const MAX_ROWS = positiveIntParam('max', 8);
  const TTL_MS   = positiveIntParam('ttl', 12000);
  const FONT_SCALE = boundedNumberParam('font', 100, 50, 200) / 100;
  const BG_OPACITY = boundedNumberParam('bg', 0, 0, 100) / 100;
  const POSITION = params.get('pos') === 'top' ? 'top' : 'bottom';
  const SHOW_PLATFORM = params.get('icon') !== '0';
  const ONLY_GIFT = params.get('only') === 'gift';
  const WS_URL   = buildWsUrl(params.get('ws') || 'ws://127.0.0.1:11180/ws');

  const overlay = document.getElementById('overlay');
  applyAppearance();
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
      const modEvents = fcModFrameEvents(batch);
      if (modEvents) {
        applyModeration(modEvents);
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

  // ---- fc-moderation:begin ----
  // 配信側・アプリ発の削除 / タイムアウト / BAN / ローカル非表示(`/ws?modEvents=1` の
  // {"moderation":[…]} フレーム)。視聴者に見える面なので該当コメントは DOM から完全に除く。
  // 同梱テンプレ共通のコピー(各テンプレは単体の静的サイトなので共有ファイルにしない)。
  // templates/obsModeration.test.mjs が全コピーの一致と挙動を検証する。
  const FC_MOD_MAX = 512;
  const FC_MOD_TTL_MS = 10000;

  // {"moderation":[…]} フレームならイベント配列、それ以外(chat 配列など)は null。
  function fcModFrameEvents(frame) {
    if (!frame || typeof frame !== 'object' || Array.isArray(frame)) return null;
    if (!Array.isArray(frame.moderation)) return null;
    return frame.moderation.filter(function (e) { return !!e && typeof e === 'object'; });
  }

  // userCleared / chatCleared は beforeMs(サーバー時刻)以前のコメントだけ。null は境界なし。
  // userCleared は certain の有無に関わらず除く。
  function fcModMatches(ev, msg) {
    if (!ev || !msg || ev.platform !== msg.platform || ev.channel !== msg.channel) return false;
    const within = ev.beforeMs == null ||
      (typeof msg.timestampMs === 'number' && msg.timestampMs <= ev.beforeMs);
    if (ev.type === 'messageDeleted') return !!ev.messageId && ev.messageId === msg.id;
    if (ev.type === 'userCleared') {
      return !!ev.authorId && !!msg.author && ev.authorId === msg.author.id && within;
    }
    if (ev.type === 'chatCleared') return within;
    return false;
  }

  function fcModAnyMatch(events, msg) {
    for (let i = 0; i < events.length; i++) {
      if (fcModMatches(events[i], msg)) return true;
    }
    return false;
  }

  // イベントがコメントより先に届いた時のための直近記録(最大 512 件、記録から 10 秒、単調時計)。
  function fcModCreateLog(now) {
    const clock = now || function () { return performance.now(); };
    let entries = [];
    return {
      add: function (events) {
        const t = clock();
        events.forEach(function (event) { entries.push({ event: event, at: t }); });
        if (entries.length > FC_MOD_MAX) entries = entries.slice(entries.length - FC_MOD_MAX);
      },
      matches: function (msg) {
        if (entries.length === 0) return false;
        const t = clock();
        entries = entries.filter(function (e) {
          const age = t - e.at;
          return age >= 0 && age <= FC_MOD_TTL_MS;
        });
        return entries.some(function (e) { return fcModMatches(e.event, msg); });
      },
      size: function () { return entries.length; },
    };
  }

  // 要素に紐づけたコメント(複数可)のどれかが対象なら要素ごと除く。除いた要素を返す。
  function fcModRemoveFrom(container, events) {
    const removed = [];
    if (!container) return removed;
    Array.prototype.slice.call(container.children).forEach(function (el) {
      const msgs = el.__fcMsgs || [];
      if (msgs.some(function (m) { return fcModAnyMatch(events, m); })) {
        if (el.parentNode) el.parentNode.removeChild(el);
        removed.push(el);
      }
    });
    return removed;
  }

  const fcModLog = fcModCreateLog();
  // ---- fc-moderation:end ----

  function applyModeration(events) {
    fcModLog.add(events);
    fcModRemoveFrom(overlay, events).forEach(function (el) {
      const idx = rows.findIndex(function (r) { return r.el === el; });
      if (idx !== -1) {
        clearTimeout(rows[idx].timerId);
        rows.splice(idx, 1);
      }
    });
  }

  // ---- Message handler ----
  function handleMessage(msg) {
    if (fcModLog.matches(msg)) return;
    // Channel filter
    if (CHANNEL_FILTER && msg.channel !== CHANNEL_FILTER) return;
    if (ONLY_GIFT && !isGiftMessage(msg)) return;

    const el = buildRow(msg);
    el.__fcMsgs = [msg];
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
    appendAvatar(div, msg);
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

  function appendAvatar(parent, msg) {
    const url = msg.author && msg.author.avatarUrl;
    if (!SHOW_AVATAR || typeof url !== 'string' || !/^https?:\/\//i.test(url)) return;
    const img = document.createElement('img');
    img.className = 'avatar';
    img.alt = '';
    img.loading = 'lazy';
    img.referrerPolicy = 'no-referrer';
    img.onerror = () => { img.style.display = 'none'; };
    img.src = url;
    parent.appendChild(img);
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
    // 削除・BAN の {"moderation":[…]} フレームを受け取る(opt-in。未対応の改造テンプレには届かない)。
    url.searchParams.set('modEvents', '1');
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

  // ---- Start ----
  connect();
})();
