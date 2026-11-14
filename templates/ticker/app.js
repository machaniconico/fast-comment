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
  const ROTATE_MS = 4000;
  const WS_URL   = buildWsUrl(params.get('ws') || 'ws://127.0.0.1:11180/ws');

  const tickerText = document.getElementById('ticker-text');

  // ---- Queue tracking ----
  const queue = [];
  let queueIndex = 0;
  let rotationTimer = null;

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

  // ---- fc-attrs:begin ----
  // コメント要素に data-* 属性を付ける(自作 CSS 用)。msg が null なら全属性を外す。
  // 値は setAttribute で渡す(HTML 文字列には連結しない)。要素を使い回しても前の値が残らない。
  // 同一ブロックを 6 テンプレの app.js に複製し、templates/obsAttrs.test.mjs が一致と挙動を検証する。
  function fcApplyAttrs(el, msg) {
    if (!msg) {
      el.removeAttribute('data-service');
      el.removeAttribute('data-user');
      el.removeAttribute('data-owner');
      el.removeAttribute('data-user-id');
      el.removeAttribute('data-kind');
      el.removeAttribute('data-role');
      return;
    }
    const author = msg.author || null;
    const roles = (author && author.roles) || {};
    el.setAttribute('data-service', msg.platform === 'niconico' ? 'niconama' : String(msg.platform || ''));
    if (author && author.name) el.setAttribute('data-user', String(author.name));
    else el.removeAttribute('data-user');
    el.setAttribute('data-owner', roles.broadcaster ? 'true' : 'false');
    if (author && author.id) el.setAttribute('data-user-id', String(author.id));
    else el.removeAttribute('data-user-id');
    el.setAttribute('data-kind', String(msg.kind || 'normal').toLowerCase());
    const list = ['broadcaster', 'moderator', 'member', 'subscriber', 'vip'].filter((r) => roles[r]);
    if (list.length) el.setAttribute('data-role', list.join(' '));
    else el.removeAttribute('data-role');
  }
  // ---- fc-attrs:end ----

  function applyModeration(events) {
    fcModLog.add(events);
    let changed = false;
    for (let i = queue.length - 1; i >= 0; i--) {
      if (fcModAnyMatch(events, queue[i].msg)) {
        queue.splice(i, 1);
        if (i < queueIndex) queueIndex -= 1;
        changed = true;
      }
    }
    if (!changed) return;
    if (queue.length === 0) {
      clearTimeout(rotationTimer);
      rotationTimer = null;
      queueIndex = 0;
    }
    renderCurrent();
  }

  // ---- Message handler ----
  function handleMessage(msg) {
    if (fcModLog.matches(msg)) return;
    if ((msg.kind || 'normal') === 'system') return;
    // Channel filter
    if (CHANNEL_FILTER && msg.channel !== CHANNEL_FILTER) return;

    const text = commentText(msg);
    if (!text) return;

    const wasEmpty = queue.length === 0;
    queue.push({ text: text, msg: msg });
    while (queue.length > MAX_QUEUE) {
      queue.shift();
      queueIndex = Math.max(0, queueIndex - 1);
    }

    if (wasEmpty && rotationTimer === null) showNext();
  }

  function renderCurrent() {
    if (queue.length === 0) {
      tickerText.textContent = '';
      fcApplyAttrs(tickerText, null);
      return;
    }

    queueIndex = queueIndex % queue.length;
    tickerText.textContent = queue[queueIndex].text;
    fcApplyAttrs(tickerText, queue[queueIndex].msg);
  }

  function showNext() {
    rotationTimer = null;
    if (queue.length === 0) return;
    queueIndex = (queueIndex + 1) % queue.length;
    renderCurrent();
    rotationTimer = setTimeout(showNext, ROTATE_MS);
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
    // 削除・BAN の {"moderation":[…]} フレームを受け取る(opt-in。未対応の改造テンプレには届かない)。
    url.searchParams.set('modEvents', '1');
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
  connect();
})();
