/**
 * fast-comment OBS overlay - donation template
 * No dependencies. Plain ES2020 JS.
 *
 * URL params:
 *   ?template=donation
 *   ?channel=<id>
 *   ?max=4
 *   ?ttl=7000
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
  const MAX_CARDS = Math.min(positiveIntParam('max', 4), 12);
  const MAX_CURRENCIES = 200;
  const TTL_MS = positiveIntParam('ttl', 7000);
  const FONT_SCALE = boundedNumberParam('font', 100, 50, 200) / 100;
  const BG_OPACITY = boundedNumberParam('bg', 0, 0, 100) / 100;
  const POSITION = params.get('pos') === 'top' ? 'top' : 'bottom';
  const WS_URL = buildWsUrl(params.get('ws') || 'ws://127.0.0.1:11180/ws');

  const overlay = document.getElementById('overlay');
  const toastStack = document.getElementById('toast-stack');
  const totalsPanel = document.getElementById('totals');
  const LEAVE_MS = cssDurationMs('--leave-ms', 350);

  // Each entry: { el, timerId }
  const cards = [];
  const totals = new Map();

  applyAppearance();
  renderTotals();

  // ---- WebSocket ----
  let ws = null;
  let reconnectDelay = 1000;
  let stableTimer = null;
  let reconnectTimer = null;

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
      const modEvents = fcModFrameEvents(batch);
      if (modEvents) {
        applyModeration(modEvents);
        return;
      }
      if (!Array.isArray(batch)) batch = [batch];
      batch.forEach(handleMessage);
    });

    ws.addEventListener('close', () => {
      clearTimeout(stableTimer);
      stableTimer = null;
      ws = null;
      scheduleReconnect();
    });

    ws.addEventListener('error', () => {
      if (ws) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
        ws.close();
      }
    });
  }

  function scheduleReconnect() {
    clearTimeout(reconnectTimer);
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 30000);
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

  // 合計(totals)は投げ銭の集計なので戻さない。表示中のカードだけ除く。
  function applyModeration(events) {
    fcModLog.add(events);
    fcModRemoveFrom(toastStack, events).forEach(function (el) {
      const idx = cards.findIndex(function (c) { return c.el === el; });
      if (idx !== -1) {
        clearTimeout(cards[idx].timerId);
        cards.splice(idx, 1);
      }
    });
  }

  // ---- Message handler ----
  function handleMessage(msg) {
    if (CHANNEL_FILTER && msg.channel !== CHANNEL_FILTER) return;

    const donation = donationInfo(msg);
    if (!donation) return;

    // 合計は到着順に関係なく数える(削除→コメントの順でも額は同じ)。後着抑止はカードだけ。
    recordDonation(donation);
    renderTotals();
    if (fcModLog.matches(msg)) return;
    const card = buildCard(msg, donation);
    card.__fcMsgs = [msg];
    addCard(card);
  }

  // ---- Build DOM card ----
  function buildCard(msg, donation) {
    const card = document.createElement('article');
    card.className = 'donation-card ' + donation.typeClass;

    const header = document.createElement('div');
    header.className = 'card-header';
    card.appendChild(header);

    const platformDot = document.createElement('span');
    platformDot.className = 'platform-dot ' + (msg.platform || '');
    header.appendChild(platformDot);

    const author = document.createElement('span');
    author.className = 'author';
    author.textContent = (msg.author && msg.author.name) || donation.label;
    header.appendChild(author);

    const kind = document.createElement('span');
    kind.className = 'kind';
    kind.textContent = donation.label;
    header.appendChild(kind);

    if (donation.rawText) {
      const amount = document.createElement('span');
      amount.className = 'amount';
      amount.textContent = donation.rawText;
      header.appendChild(amount);
    }

    const body = document.createElement('p');
    body.className = 'message';
    body.textContent = messageText(msg);
    card.appendChild(body);

    return card;
  }

  // ---- Card lifecycle ----
  function addCard(card) {
    while (cards.length >= MAX_CARDS) {
      evictOldest();
    }

    toastStack.appendChild(card);

    const timerId = setTimeout(() => removeCard(card), TTL_MS);
    cards.push({ el: card, timerId });
  }

  function evictOldest() {
    const oldest = cards.shift();
    if (!oldest) return;
    clearTimeout(oldest.timerId);
    startLeave(oldest.el);
  }

  function removeCard(card) {
    const idx = cards.findIndex((entry) => entry.el === card);
    if (idx !== -1) {
      clearTimeout(cards[idx].timerId);
      cards.splice(idx, 1);
    }
    startLeave(card);
  }

  function startLeave(card) {
    card.classList.add('leaving');
    setTimeout(() => {
      if (card.parentNode) card.parentNode.removeChild(card);
    }, LEAVE_MS);
  }

  // ---- Totals ----
  function recordDonation(donation) {
    const key = donation.currency || donation.label;
    const existing = totals.get(key);
    if (!existing && totals.size >= MAX_CURRENCIES) return;

    const current = existing || {
      label: key,
      count: 0,
      amount: 0,
      hasNumericAmount: false,
    };

    current.count += 1;
    if (Number.isFinite(donation.value)) {
      current.amount += donation.value;
      current.hasNumericAmount = true;
    }
    totals.set(key, current);
  }

  function renderTotals() {
    totalsPanel.textContent = '';

    const title = document.createElement('div');
    title.className = 'totals-title';
    title.textContent = 'DONATIONS';
    totalsPanel.appendChild(title);

    if (totals.size === 0) {
      const empty = document.createElement('div');
      empty.className = 'totals-empty';
      empty.textContent = '0';
      totalsPanel.appendChild(empty);
      return;
    }

    Array.from(totals.values()).forEach((entry) => {
      const row = document.createElement('div');
      row.className = 'total-row';

      const label = document.createElement('span');
      label.className = 'total-label';
      label.textContent = entry.label;
      row.appendChild(label);

      const value = document.createElement('span');
      value.className = 'total-value';
      if (entry.hasNumericAmount) {
        value.textContent = `${formatAmount(entry.amount)} / ${entry.count}`;
      } else {
        value.textContent = String(entry.count);
      }
      row.appendChild(value);

      totalsPanel.appendChild(row);
    });
  }

  // ---- Donation detection ----
  function donationInfo(msg) {
    if (!msg || typeof msg !== 'object') return null;

    const kind = normalizeKind(msg.kind);
    const amount = amountInfo(msg);

    if (kind === 'superchat') {
      return {
        typeClass: 'superchat',
        label: 'SuperChat',
        rawText: amount.rawText,
        value: amount.value,
        currency: amount.currency,
      };
    }

    if (kind === 'bits') {
      return {
        typeClass: 'bits',
        label: 'Bits',
        rawText: amount.rawText,
        value: amount.value,
        currency: amount.currency || 'BITS',
      };
    }

    if (kind === 'membership') {
      return {
        typeClass: 'membership',
        label: 'Membership',
        rawText: amount.rawText,
        value: amount.value,
        currency: amount.currency,
      };
    }

    if (msg.donation != null || msg.amount != null) {
      return {
        typeClass: 'donation',
        label: 'Donation',
        rawText: amount.rawText,
        value: amount.value,
        currency: amount.currency,
      };
    }

    return null;
  }

  function normalizeKind(kind) {
    if (typeof kind !== 'string') return '';
    return kind.replace(/[-_]/g, '').toLowerCase();
  }

  function amountInfo(msg) {
    const source = objectValue(msg.amount) || objectValue(msg.donation) || {};
    const rawText = stringValue(source.rawText)
      || stringValue(source.displayText)
      || stringValue(source.text)
      || stringValue(source.label)
      || primitiveAmountText(msg.amount)
      || primitiveAmountText(msg.donation)
      || stringValue(msg.amountRawText)
      || stringValue(msg.amountText)
      || '';
    const currency = stringValue(source.currency)
      || stringValue(source.currencyCode)
      || stringValue(source.currencySymbol)
      || stringValue(msg.currency)
      || currencyFromText(rawText)
      || '';
    const value = numericAmount(source.value)
      ?? numericAmount(source.amount)
      ?? numericAmount(source.amountMicros, 1000000)
      ?? numericAmount(msg.amount)
      ?? numericAmount(msg.donation)
      ?? numericAmount(msg.amountValue)
      ?? numericAmount(msg.amountNumeric);

    return { rawText, currency, value };
  }

  function objectValue(value) {
    return value && typeof value === 'object' ? value : null;
  }

  function stringValue(value) {
    return typeof value === 'string' ? value : '';
  }

  function primitiveAmountText(value) {
    if (value == null || typeof value === 'object') return '';
    return String(value);
  }

  function numericAmount(value, divisor) {
    const number = Number(value);
    if (!Number.isFinite(number)) return null;
    return divisor ? number / divisor : number;
  }

  function currencyFromText(text) {
    if (typeof text !== 'string') return '';
    const match = text.match(/[A-Z]{3}|[$¥€£]/);
    return match ? match[0] : '';
  }

  function messageText(msg) {
    const fragments = msg.fragments || [];
    if (!Array.isArray(fragments)) return '';
    return fragments.map(fragmentText).join('');
  }

  function fragmentText(frag) {
    if (!frag || typeof frag !== 'object') return '';
    if (frag.type === 'text') return frag.text || '';
    return frag.name || '';
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

  function applyAppearance() {
    document.documentElement.style.setProperty('--font-scale', String(FONT_SCALE));
    document.documentElement.style.setProperty('--overlay-bg-opacity', String(BG_OPACITY));
    overlay.classList.toggle('pos-top', POSITION === 'top');
    overlay.classList.toggle('pos-bottom', POSITION !== 'top');
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

  function formatAmount(value) {
    if (!Number.isFinite(value)) return '';
    if (Math.abs(value) >= 100 || Number.isInteger(value)) return String(Math.round(value));
    return value.toFixed(2).replace(/\.?0+$/, '');
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
