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
 *   ?area=full|top|bottom 表示縦帯(既定 full、上半分/下半分に制限可能)
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
  const AREA = normalizeArea(params.get('area'));
  // E4: 同一本文の連投を ×N に集約する(既定 ON。'0' で OFF)。
  const COALESCE = params.get('coalesce') !== '0';
  // E5: 投げ銭/メンバーを固定強調弾幕にする(既定 ON。'0' で OFF)。
  const PIN_GIFTS = params.get('pin') !== '0';

  // E4 の集約窓口(ms)。同じ本文の連投がこの時間内なら 1 つにまとめる。
  const COALESCE_WINDOW_MS = 1500;
  // E5 の固定表示秒数と同時固定数上限。
  const PIN_SEC = 8;
  const PIN_MAX = 6;

  const root = document.getElementById('danmaku');
  const pinLayer = document.getElementById('danmaku-pins');
  if (!root) return;
  root.style.opacity = String(OPACITY);

  // E4: recentByText は key=本文 core(名前前置/金額を含まない素の本文)
  // value={el, base, count, at} の Map。element 撤去時に掃除して肥大を防ぐ。
  const recentByText = new Map();

  // ---- Display row management ----
  let laneHeight = 0;
  let laneCount = 1;
  let nextLane = 0;
  // 表示縦帯(画面上半分/下半分/全体)。recomputeLanes で更新。
  let y0 = 0;

  function recomputeLanes() {
    const viewportH = window.innerHeight || 1080;
    laneHeight = Math.round(FONT_SIZE * 1.45);
    let y1;
    if (AREA === 'top') {
      y0 = 0;
      y1 = Math.floor(viewportH / 2);
    } else if (AREA === 'bottom') {
      y0 = Math.floor(viewportH / 2);
      y1 = viewportH;
    } else {
      y0 = 0;
      y1 = viewportH;
    }
    const bandH = y1 - y0;
    laneCount = Math.max(1, Math.floor(bandH / laneHeight));
    nextLane %= laneCount;
  }

  function spawn(msg) {
    const kind = msg.kind || 'normal';
    const isGift = kind === 'superChat' || kind === 'bits' || kind === 'membership';

    // E5: 投げ銭/メンバーは固定強調弾幕へ。PIN_GIFTS OFF なら従来通り流す。
    if (PIN_GIFTS && isGift) {
      pinGift(msg);
      return;
    }

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

    // E4: 流れるコメント(非gift/非system)のみ集約対象。
    //      既に同じ本文 core の弾幕が COALESCE_WINDOW_MS 内にあり DOM 接続中なら、
    //      その弾幕の textContent を書き換えて count++ し、新規 spawn を中断する。
    //      配置済み要素の位置は変更しない。
    if (COALESCE && !isGift) {
      const existing = recentByText.get(core);
      const now = performance.now();
      if (
        existing &&
        now - existing.at <= COALESCE_WINDOW_MS &&
        existing.el && existing.el.isConnected
      ) {
        existing.count += 1;
        existing.at = now;
        existing.el.textContent =
          existing.base + (existing.count >= 2 ? ' ×' + existing.count : '');
        // 古いエントリの間引き(Map 肥大防止の保険)。
        pruneRecentByText(now);
        return;
      }
      pruneRecentByText(now);
    }

    const now = performance.now();
    // 他コメントの位置・文字幅・文字サイズは参照しない単純な行順送り。
    // 混雑時の重なりは許容し、重複判定や回避配置は行わない。
    const lane = nextLane;
    nextLane = (nextLane + 1) % laneCount;

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

    el.style.top = (y0 + lane * laneHeight) + 'px';
    el.style.fontSize = FONT_SIZE + 'px';
    el.style.setProperty('--dur', DURATION_SEC + 's');
    el.textContent = text;

    // アニメ終了で自分を撤去。fly(座標) と fade(opacity) の2アニメで2回発火するので、
    // 座標側(danmaku-fly)の時だけ撤去し二重/早期撤去を防ぐ。
    // E4: 撤去時に recentByText から該当エントリ(value.el===el)も削除して肥大を防ぐ。
    el.addEventListener('animationend', function (e) {
      if (e.animationName === 'danmaku-fly') {
        removeRecentByTextEl(el);
        el.remove();
      }
    });

    root.appendChild(el);

    // E4: 新規生成後に recentByText へ登録。集約キーは本文 core。
    //      base には実際に表示した文字列(SHOW_NAME 前置込みの text)を保存する。
    if (COALESCE && !isGift) {
      recentByText.set(core, { el: el, base: text, count: 1, at: now });
    }

    // 上限超過は古いもの(先頭)から捨てる(アニメ未終了でも DOM 肥大を防ぐ)。
    while (root.childElementCount > MAX_ACTIVE && root.firstElementChild) {
      const oldest = root.firstElementChild;
      // E4: 上限間引きで捨てられる要素も recentByText から外す。
      removeRecentByTextEl(oldest);
      oldest.remove();
    }
  }

  // E4: recentByText から value.el===el のエントリを削除。
  function removeRecentByTextEl(el) {
    for (const [key, value] of recentByText) {
      if (value && value.el === el) {
        recentByText.delete(key);
      }
    }
  }

  // E4: COALESCE_WINDOW_MS を過ぎた古いエントリを間引く(Map 肥大防止の保険)。
  function pruneRecentByText(now) {
    if (recentByText.size < 32) return;
    for (const [key, value] of recentByText) {
      if (value && now - value.at > COALESCE_WINDOW_MS) {
        recentByText.delete(key);
      }
    }
  }

  // E5: 投げ銭/メンバーを画面上部に固定強調表示してフェードさせる。
  function pinGift(msg) {
    if (!pinLayer) return;
    const kind = msg.kind || 'normal';
    const body = messageText(msg).trim();
    const amountText = (msg.amount && msg.amount.rawText) || '';
    const author = (msg.author && msg.author.name) || '';

    // spawn と同じ core 算出ロジックを再利用(投げ銭は金額、メンバーはラベル fallback)。
    let core = body;
    if (kind === 'superChat' || kind === 'bits') {
      core = [amountText, body].filter(Boolean).join(' ');
    } else if (kind === 'membership') {
      core = body || 'メンバー加入';
    }
    if (!core) return;
    const text = SHOW_NAME && author ? author + ': ' + core : core;

    const el = document.createElement('div');
    el.className = 'danmaku-pin';
    if (kind === 'superChat' || kind === 'bits') {
      el.classList.add('superchat');
    } else if (kind === 'membership') {
      el.classList.add('member');
    }
    el.textContent = text;

    // 最新を上に。PIN_MAX 超過は末尾(最古)から間引く。
    pinLayer.prepend(el);
    while (pinLayer.childElementCount > PIN_MAX && pinLayer.lastElementChild) {
      pinLayer.lastElementChild.remove();
    }

    // PIN_SEC 後の最後 ~0.5s でフェード→撤去。
    const fadeAt = PIN_SEC * 1000 - 500;
    if (fadeAt > 0) {
      setTimeout(function () {
        el.classList.add('leaving');
      }, fadeAt);
    }
    setTimeout(function () {
      el.remove();
    }, PIN_SEC * 1000);
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

  function normalizeArea(value) {
    return value === 'top' || value === 'bottom' ? value : 'full';
  }

  // ---- Start ----
  recomputeLanes();
  window.addEventListener('resize', recomputeLanes);
  connect();
})();
