/**
 * fast-comment OBS overlay — danmaku(ニコ生風・流れるコメント)テンプレート
 * 依存なし。Plain ES2020 JS。
 *
 * 画面全体を右→左へコメントが流れる透過オーバーレイ。OBS の「ブラウザソース」に
 *   http://127.0.0.1:11180/?template=danmaku
 * を指定して使う。背景は透過なので、ゲーム/カメラ等の上に重ねられる。
 * 配信される /ws のコメントは Rust 側で NG 判定済み(Hide は届かない)。
 *
 * URL params:
 *   ?template=danmaku   (axum router が消費)
 *   ?channel=<id>        指定チャンネルのみ表示
 *   ?dur=7               画面を横断する秒数(小さいほど速い, 2..30)
 *   ?size=30             文字サイズ px (12..96)
 *   ?opacity=92          全体の不透明度 percent (10..100)
 *   ?name=0              名前を前置 1/0 (default 0 = 本文のみ=ニコ生風)
 *   ?outline=1           縁取り 1/0 (default 1)
 *   ?max=240             同時表示の最大数(DOM 肥大防止)
 *   ?only=gift           SuperChat/Bits/メンバーのみ流す
 *   ?ws=ws://127.0.0.1:11180/ws   WS エンドポイント上書き
 */

(function () {
  'use strict';

  // ---- Config from URL params ----
  const params = new URLSearchParams(location.search);
  const CHANNEL_FILTER = params.get('channel') || null;
  const DURATION_SEC = boundedNumberParam('dur', 7, 2, 30);
  const FONT_SIZE = Math.round(boundedNumberParam('size', 30, 12, 96));
  const OPACITY = boundedNumberParam('opacity', 92, 10, 100) / 100;
  const SHOW_NAME = params.get('name') === '1';
  const OUTLINE = params.get('outline') !== '0';
  const MAX_ACTIVE = positiveIntParam('max', 240);
  const ONLY_GIFT = params.get('only') === 'gift';
  const WS_URL = buildWsUrl(params.get('ws') || 'ws://127.0.0.1:11180/ws');

  const root = document.getElementById('danmaku');
  if (!root) return;
  root.style.opacity = String(OPACITY);

  // ---- Lane(行)management ----
  let viewportW = 0;
  let laneHeight = 0;
  let laneCount = 1;
  // 各レーンが次に空く時刻(performance.now ベース, ms)。
  let laneFreeAt = [];

  // 文字幅の実測(レーンが空く時刻の計算に使う)。canvas measureText で概算。
  let measureCtx = null;
  try {
    measureCtx = document.createElement('canvas').getContext('2d');
  } catch (_e) {
    measureCtx = null;
  }

  function measureWidth(text, fontSize) {
    if (!measureCtx) return text.length * fontSize; // 全角主体を想定し安全側(広め)に推定
    measureCtx.font = 'bold ' + fontSize + 'px sans-serif';
    return measureCtx.measureText(text).width;
  }

  function recomputeLanes() {
    viewportW = window.innerWidth || 1920;
    const viewportH = window.innerHeight || 1080;
    laneHeight = Math.round(FONT_SIZE * 1.45);
    laneCount = Math.max(1, Math.floor(viewportH / laneHeight));
    if (laneFreeAt.length !== laneCount) laneFreeAt = new Array(laneCount).fill(0);
  }

  // 最も長く空いている(= freeAt が最小の)レーンを選ぶ。
  // 全レーンが埋まっている高負荷時は「最も早く空く」レーンに相乗りする(劣化許容)。
  function pickLane() {
    let best = 0;
    let bestFree = laneFreeAt[0] || 0;
    for (let i = 1; i < laneCount; i++) {
      const f = laneFreeAt[i] || 0;
      if (f < bestFree) {
        best = i;
        bestFree = f;
      }
    }
    return best;
  }

  function spawn(msg) {
    const kind = msg.kind || 'normal';
    const body = messageText(msg).trim();
    const amountText = (msg.amount && msg.amount.rawText) || '';
    const author = (msg.author && msg.author.name) || '';

    // 表示コア文字列。投げ銭は本文が空でも金額で、メンバーはラベルで必ず可視化する
    // (?only=gift 指定時に、金額だけ(本文なし)の SuperChat が消えないように)。
    let core = body;
    if (kind === 'superChat' || kind === 'bits') {
      core = [amountText, body].filter(Boolean).join(' ');
    } else if (kind === 'membership') {
      core = body || 'メンバー加入';
    }
    if (!core) return;
    const text = SHOW_NAME && author ? author + ': ' + core : core;

    const now = performance.now();
    const lane = pickLane();
    const w = measureWidth(text, FONT_SIZE);
    // 等速。画面幅 + 自分の幅を DURATION_SEC で割った速度(px/s)。
    const speed = (viewportW + w) / DURATION_SEC;
    // 前のコメントが「自分の幅 + 1文字ぶんの間隔」流れ切るまで、このレーンは塞がっている扱い。
    const clearMs = ((w + FONT_SIZE) / speed) * 1000;
    laneFreeAt[lane] = now + clearMs;

    const el = document.createElement('div');
    el.className = 'danmaku-item';
    if (OUTLINE) el.classList.add('outline');

    if (kind === 'superChat' || kind === 'bits') {
      el.classList.add('superchat');
    } else if (kind === 'membership') {
      el.classList.add('member');
    } else {
      // 通常コメントのみ著者色を反映(投げ銭/メンバーはクラス色を優先)。
      const color = msg.author && msg.author.displayColor;
      el.style.color = isSafeHexColor(color) ? color : '#ffffff';
    }

    el.style.top = lane * laneHeight + 'px';
    el.style.fontSize = FONT_SIZE + 'px';
    el.style.setProperty('--dur', DURATION_SEC + 's');
    el.textContent = text;

    // アニメ終了で自分を撤去。
    el.addEventListener('animationend', function () {
      el.remove();
    });

    root.appendChild(el);

    // 上限超過は古いもの(先頭)から捨てる(アニメ未終了でも DOM 肥大を防ぐ)。
    while (root.childElementCount > MAX_ACTIVE && root.firstElementChild) {
      root.firstElementChild.remove();
    }
  }

  // ---- WebSocket(default テンプレと同じ再接続戦略)----
  let ws = null;
  let reconnectDelay = 1000;
  let stableTimer = null;

  function connect() {
    ws = new WebSocket(WS_URL);

    ws.addEventListener('open', function () {
      // 接続が 5 秒安定したらバックオフをリセット(open→close フラッピング対策)。
      stableTimer = setTimeout(function () {
        reconnectDelay = 1000;
      }, 5000);
    });

    ws.addEventListener('message', function (ev) {
      let batch;
      try {
        batch = JSON.parse(ev.data);
      } catch (_e) {
        return;
      }
      if (!Array.isArray(batch)) batch = [batch];
      batch.forEach(handleMessage);
    });

    ws.addEventListener('close', function () {
      clearTimeout(stableTimer);
      stableTimer = null;
      ws = null;
      setTimeout(connect, reconnectDelay);
      reconnectDelay = Math.min(reconnectDelay * 2, 16000);
    });

    ws.addEventListener('error', function () {
      if (ws) ws.close();
    });
  }

  // ---- Message handler ----
  function handleMessage(msg) {
    if (!msg) return;
    if (CHANNEL_FILTER && msg.channel !== CHANNEL_FILTER) return;
    if (ONLY_GIFT && !isGiftMessage(msg)) return;
    // システムメッセージ(接続通知など)は配信画面に流さない。
    if ((msg.kind || 'normal') === 'system') return;
    spawn(msg);
  }

  // ---- Helpers ----
  function messageText(msg) {
    const fragments = (msg && msg.fragments) || [];
    return fragments.map(fragmentText).join('');
  }

  function fragmentText(frag) {
    if (!frag) return '';
    if (frag.type === 'text') return frag.text || '';
    return frag.name || '';
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

  function isGiftMessage(msg) {
    const kind = msg && msg.kind;
    return kind === 'superChat' || kind === 'membership' || kind === 'bits';
  }

  // ---- Start ----
  recomputeLanes();
  window.addEventListener('resize', recomputeLanes);
  connect();
})();
