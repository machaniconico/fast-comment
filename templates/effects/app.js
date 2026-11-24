/**
 * fast-comment OBS overlay - effects template (keyword effects)
 * No dependencies. Plain ES2020 JS.
 *
 * URL params:
 *   ?template=effects
 *   ?ws=ws://127.0.0.1:11180/ws   override WS endpoint (default: this page's host)
 *
 * Only `{"effects":[…]}` frames are used (the app judges keywords and cooldowns).
 * Chat batches are ignored. Recommended browser source size: 1920x1080.
 */

(function () {
  'use strict';

  // ---- fc-effects:begin ----
  // Pure helpers. templates/effects.test.mjs evaluates this block without a DOM.
  const EFFECTS_PRESETS = ['rain', 'burst', 'float', 'bounce', 'spin', 'big'];
  const EFFECTS_DEFAULT_PRESET = 'float';
  const EFFECTS_MIN_COUNT = 1;
  const EFFECTS_MAX_COUNT = 50;
  const EFFECTS_MIN_DURATION_MS = 500;
  const EFFECTS_MAX_DURATION_MS = 15000;
  const EFFECTS_DEFAULT_DURATION_MS = 3000;
  const EFFECTS_MAX_EMOJI_CHARS = 16;
  // 同時に表示する要素の上限。超えた分は古い物から消す。
  const EFFECTS_MAX_ACTIVE = 300;

  function effectsClamp(value, fallback, min, max) {
    const n = typeof value === 'number' ? value : Number(value);
    if (!Number.isFinite(n)) return fallback;
    return Math.min(max, Math.max(min, Math.trunc(n)));
  }

  // 1 件の演出を安全な値へ丸める。表示物が空なら null(描かない)。
  function effectsParams(item) {
    if (!item || typeof item !== 'object' || Array.isArray(item)) return null;
    const emoji = typeof item.emoji === 'string' ? Array.from(item.emoji.trim()).slice(0, EFFECTS_MAX_EMOJI_CHARS).join('') : '';
    if (emoji === '') return null;
    return {
      emoji: emoji,
      count: effectsClamp(item.count, 1, EFFECTS_MIN_COUNT, EFFECTS_MAX_COUNT),
      animation: EFFECTS_PRESETS.indexOf(item.animation) >= 0 ? item.animation : EFFECTS_DEFAULT_PRESET,
      durationMs: effectsClamp(item.durationMs, EFFECTS_DEFAULT_DURATION_MS, EFFECTS_MIN_DURATION_MS, EFFECTS_MAX_DURATION_MS),
    };
  }

  // Only an object with an "effects" array qualifies. Chat arrays and other frames are ignored.
  function effectsFrameItems(frame) {
    if (!frame || typeof frame !== 'object' || Array.isArray(frame)) return [];
    if (!Array.isArray(frame.effects)) return [];
    return frame.effects.map(effectsParams).filter(function (p) { return p !== null; });
  }

  // 1 個ぶんの配置。座標は画面の % (x,y)、移動量は dx[vw] / dy[vh]。
  // rand は 0 以上 1 未満を返す関数(テストでは固定列を渡す)。
  // delay + dur は常に durationMs 以内に収める。
  function effectsPlacement(animation, index, count, durationMs, rand) {
    const r = rand;
    const delay = Math.floor(r() * durationMs * 0.3);
    const dur = durationMs - delay;
    const p = { x: 50, y: 50, dx: 0, dy: 0, rot: 0, size: 40, delay: delay, dur: dur };
    switch (animation) {
      case 'rain':
        p.x = r() * 100; p.y = -10; p.dx = (r() - 0.5) * 10; p.dy = 125;
        p.rot = (r() - 0.5) * 90; p.size = 28 + r() * 24;
        break;
      case 'burst': {
        const angle = ((index + r() * 0.8) / count) * Math.PI * 2;
        const dist = 25 + r() * 20;
        // 16:9 を想定し、dx[vw] と dy[vh] が同じ見た目の距離になるよう補正する。
        p.dx = Math.cos(angle) * dist * 1.78; p.dy = Math.sin(angle) * dist;
        p.rot = (r() - 0.5) * 180; p.size = 30 + r() * 26;
        break;
      }
      case 'bounce':
        p.x = 5 + r() * 90; p.y = 100; p.dy = -(25 + r() * 20);
        p.rot = (r() - 0.5) * 30; p.size = 34 + r() * 24;
        break;
      case 'spin':
        p.x = 10 + r() * 80; p.y = 10 + r() * 80;
        p.rot = (r() < 0.5 ? -1 : 1) * (360 + r() * 360); p.size = 40 + r() * 30;
        break;
      case 'big':
        p.x = 45 + r() * 10; p.y = 45 + r() * 10; p.rot = (r() - 0.5) * 20; p.size = 180 + r() * 80;
        break;
      default: // float
        p.x = 5 + r() * 90; p.y = 105; p.dx = (r() - 0.5) * 20; p.dy = -130;
        p.rot = (r() - 0.5) * 120; p.size = 28 + r() * 24;
        break;
    }
    return p;
  }

  // 上限を超える分を古い物から消す。active は古い順の配列。
  // 返り値: drop = active の先頭から消す数、keep = 追加する数(incoming 自身が上限超なら末尾側だけ)。
  function effectsEvict(activeLength, incoming, max) {
    const keep = Math.min(incoming, max);
    const drop = Math.max(0, activeLength + keep - max);
    return { drop: Math.min(drop, activeLength), keep: keep };
  }

  // ws パラメータが無ければ、ページ自身の host(= このアプリの配信ポート)へ繋ぐ。
  function effectsWsUrl(wsParam, loc) {
    const scheme = loc.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = new URL(wsParam || `${scheme}//${loc.host}/ws`, loc.href);
    // キーワードエフェクトの {"effects":[…]} フレームを受け取る(opt-in)。
    url.searchParams.set('effectEvents', '1');
    return url.toString();
  }
  // ---- fc-effects:end ----

  const params = new URLSearchParams(location.search);
  const WS_URL = effectsWsUrl(params.get('ws'), location);
  const stage = document.getElementById('stage');
  const active = [];

  function removeEntry(entry) {
    clearTimeout(entry.timer);
    if (entry.el.parentNode) entry.el.parentNode.removeChild(entry.el);
    const i = active.indexOf(entry);
    if (i >= 0) active.splice(i, 1);
  }

  function spawn(item) {
    const plan = effectsEvict(active.length, item.count, EFFECTS_MAX_ACTIVE);
    for (let i = 0; i < plan.drop; i += 1) removeEntry(active[0]);
    for (let i = 0; i < plan.keep; i += 1) {
      const p = effectsPlacement(item.animation, i, plan.keep, item.durationMs, Math.random);
      const el = document.createElement('span');
      el.className = 'fx fx-' + item.animation;
      el.textContent = item.emoji;
      el.style.setProperty('--x', p.x + '%');
      el.style.setProperty('--y', p.y + '%');
      el.style.setProperty('--dx', p.dx + 'vw');
      el.style.setProperty('--dy', p.dy + 'vh');
      el.style.setProperty('--rot', p.rot + 'deg');
      el.style.setProperty('--size', p.size + 'px');
      el.style.setProperty('--delay', p.delay + 'ms');
      el.style.setProperty('--dur', p.dur + 'ms');
      stage.appendChild(el);
      const entry = { el: el, timer: null };
      entry.timer = setTimeout(function () { removeEntry(entry); }, p.delay + p.dur + 100);
      active.push(entry);
    }
  }

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
      effectsFrameItems(frame).forEach(spawn);
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

  // ---- Start ----
  connect();
})();
