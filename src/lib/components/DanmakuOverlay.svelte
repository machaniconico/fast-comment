<script lang="ts">
  /**
   * 弾幕(ニコ生風・画面を流れるコメント)オーバーレイ。
   *
   * このコンポーネントは「弾幕ウィンドウ」(label="danmaku")でのみ mount される
   * (main.ts が ?window=danmaku を見て動的 import で切り替える)。
   * - 透明・枠なし・クリック透過・最前面の別ウィンドウとして開かれる(ipc.openDanmakuOverlay)。
   * - Rust 側は app.emit("chat", batch)(全ウィンドウ配信)なので、このウィンドウでも
   *   同じ listen('chat') でコメントを受信できる(startChatListener / onChatBatch を再利用)。
   * - 各コメントを右→左へ等速で流す。レーン(行)単位で重なりを避ける。
   */
  import { onMount, onDestroy } from 'svelte';
  import type { ChatMessage } from '../types';
  import { startChatListener, onChatBatch, offChatBatch } from '../ipc';
  import {
    clampDanmakuSettings,
    DANMAKU_SETTINGS_EVENT,
    loadDanmakuSettings,
    type DanmakuSettings,
  } from '../danmaku';

  // ── 設定(localStorage 'fc.danmaku' から読む。未設定はデフォルト)──────────
  let settings: DanmakuSettings = $state(loadDanmakuSettings());

  // ── 流れる1件 ───────────────────────────────────────────────────────────
  type Item = {
    id: number;
    text: string;
    color: string;
    top: number; // px
    durationSec: number;
    fontSize: number;
    kind: ChatMessage['kind'];
  };
  let items: Item[] = $state([]);
  let seq = 0;

  // ── レーン(行)管理 ──────────────────────────────────────────────────────
  let viewportW = 0;
  let laneHeight = 0;
  let laneCount = 1;
  let laneFreeAt: number[] = []; // 各レーンが次に空く時刻(performance.now ベース, ms)

  // 文字幅の実測(レーンが空く時刻の計算に使う)。canvas measureText で概算。
  let measureCtx: CanvasRenderingContext2D | null = null;
  function measureWidth(text: string, fontSize: number): number {
    if (!measureCtx) {
      let units = 0;
      for (const ch of text) {
        const code = ch.codePointAt(0) ?? 0;
        units += code >= 0x3000 && code <= 0x9fff ? 1 : 0.5;
      }
      return units * fontSize;
    }
    measureCtx.font = `bold ${fontSize}px sans-serif`;
    return measureCtx.measureText(text).width;
  }

  function resizeLaneFreeAt(nextLaneCount: number) {
    const next = new Array(nextLaneCount).fill(0);
    const preserveCount = Math.min(laneFreeAt.length, nextLaneCount);
    for (let i = 0; i < preserveCount; i += 1) next[i] = laneFreeAt[i] ?? 0;
    laneFreeAt = next;
  }

  function recomputeLanes() {
    viewportW = window.innerWidth || 1920;
    const viewportH = window.innerHeight || 1080;
    const nextLaneHeight = Math.round(settings.fontSize * 1.45);
    const nextLaneCount = Math.max(1, Math.floor(viewportH / nextLaneHeight));
    if (laneFreeAt.length !== nextLaneCount || laneHeight !== nextLaneHeight) {
      resizeLaneFreeAt(nextLaneCount);
    }
    laneHeight = nextLaneHeight;
    laneCount = nextLaneCount;
  }

  // 最も長く空いている(= freeAt が最小の)レーンを選ぶ。
  // 全レーンが埋まっている高負荷時は「最も早く空く」レーンに相乗りする(劣化許容)。
  function pickLane(): number {
    let best = 0;
    let bestFree = laneFreeAt[0] ?? 0;
    for (let i = 1; i < laneCount; i++) {
      const f = laneFreeAt[i] ?? 0;
      if (f < bestFree) {
        best = i;
        bestFree = f;
      }
    }
    return best;
  }

  function spawn(msg: ChatMessage) {
    if ((msg.kind ?? 'normal') === 'system') return;

    let body = msg.fragments
      .map((f) => (f.type === 'text' ? f.text : f.type === 'emote' ? f.name : ''))
      .join('')
      .trim();
    if (msg.kind === 'superChat' || msg.kind === 'bits') {
      const amountText = msg.amount?.rawText?.trim();
      if (amountText) body = body ? `${amountText} ${body}` : amountText;
    }
    if (msg.kind === 'membership') body = body || 'メンバー加入';
    if (!body) return;
    const text = settings.showName && msg.author?.name ? `${msg.author.name}: ${body}` : body;

    const now = performance.now();
    const lane = pickLane();
    const w = measureWidth(text, settings.fontSize);
    // 等速。画面幅+自分の幅を durationSec で割った速度(px/s)。
    const speed = (viewportW + w) / settings.durationSec;
    // 前のコメントが「自分の幅 + 1文字ぶんの間隔」流れ切るまで、このレーンは塞がっている扱い。
    const clearMs = ((w + settings.fontSize) / speed) * 1000;
    laneFreeAt[lane] = now + clearMs;

    const dc = msg.author?.displayColor;
    const color = dc && dc.trim() ? dc : '#ffffff';

    items.push({
      id: ++seq,
      text,
      color,
      top: lane * laneHeight,
      durationSec: settings.durationSec,
      fontSize: settings.fontSize,
      kind: msg.kind,
    });
    // 上限超過は古いものから捨てる(アニメ未終了でも DOM 肥大を防ぐ)。
    if (items.length > settings.maxActive) items.splice(0, items.length - settings.maxActive);
  }

  function onEnd(id: number) {
    const i = items.findIndex((it) => it.id === id);
    if (i >= 0) items.splice(i, 1);
  }

  function handleBatch(messages: ChatMessage[]) {
    for (const m of messages) spawn(m);
  }

  let unlisten: (() => void) | null = null;
  let unlistenSettings: (() => void) | null = null;
  let removeResize: (() => void) | null = null;

  onMount(async () => {
    // このウィンドウは透明。App の :global(body){background} は読み込まれない
    // (main.ts が動的 import で分岐するため)が、保険で明示的に透明化する。
    document.documentElement.style.background = 'transparent';
    document.body.style.background = 'transparent';

    measureCtx = document.createElement('canvas').getContext('2d');
    recomputeLanes();
    const onResize = () => recomputeLanes();
    window.addEventListener('resize', onResize);
    removeResize = () => window.removeEventListener('resize', onResize);

    // クリック透過: マウス操作は下のアプリ(ゲーム等)へ素通りさせる。
    try {
      const isTauri =
        typeof window !== 'undefined' &&
        !!(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
      if (isTauri) {
        const { getCurrentWindow } = await import('@tauri-apps/api/window');
        await getCurrentWindow().setIgnoreCursorEvents(true);
      }
    } catch (e) {
      console.warn('[danmaku] setIgnoreCursorEvents failed', e);
    }

    // コメントストリーム購読(main と同じ 'chat' を rAF バッチで受ける)。
    onChatBatch(handleBatch);
    unlisten = await startChatListener();

    // 設定画面での変更を、表示中の弾幕ウィンドウへ即時反映する。
    try {
      const isTauri =
        typeof window !== 'undefined' &&
        !!(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
      if (isTauri) {
        const { listen } = await import('@tauri-apps/api/event');
        unlistenSettings = await listen<Partial<DanmakuSettings>>(DANMAKU_SETTINGS_EVENT, (e) => {
          settings = clampDanmakuSettings({ ...settings, ...(e.payload || {}) });
          recomputeLanes();
          if (items.length > settings.maxActive) items.splice(0, items.length - settings.maxActive);
        });
      }
    } catch (e) {
      console.warn('[danmaku] settings listener failed', e);
    }
  });

  onDestroy(() => {
    unlisten?.();
    unlistenSettings?.();
    removeResize?.();
    offChatBatch();
  });
</script>

<div class="danmaku-root" style="--opacity:{settings.opacity}">
  {#each items as item (item.id)}
    <div
      class="danmaku-item"
      class:outline={settings.outline}
      class:superchat={item.kind === 'superChat' || item.kind === 'bits'}
      class:member={item.kind === 'membership'}
      style="top:{item.top}px; --dur:{item.durationSec}s; font-size:{item.fontSize}px; color:{item.color};"
      onanimationend={() => onEnd(item.id)}
    >
      {item.text}
    </div>
  {/each}
</div>

<style>
  :global(html),
  :global(body) {
    margin: 0;
    padding: 0;
    background: transparent !important;
    overflow: hidden;
  }

  .danmaku-root {
    position: fixed;
    inset: 0;
    width: 100vw;
    height: 100vh;
    overflow: hidden;
    pointer-events: none; /* 念のため(ウィンドウ側も setIgnoreCursorEvents 済み) */
    background: transparent;
    opacity: var(--opacity, 0.92);
  }

  .danmaku-item {
    position: absolute;
    left: 0;
    white-space: nowrap;
    font-weight: 700;
    line-height: 1.2;
    font-family:
      -apple-system, BlinkMacSystemFont, 'Segoe UI', 'Hiragino Kaku Gothic ProN', 'Yu Gothic',
      Meiryo, sans-serif;
    will-change: transform;
    transform: translateX(100vw);
    animation-name: danmaku-fly;
    animation-timing-function: linear;
    animation-fill-mode: forwards;
    animation-duration: var(--dur);
  }

  /* 背景が何でも読めるよう縁取り(text-shadow 4方向 + 軽いぼかし)。 */
  .danmaku-item.outline {
    text-shadow:
      -1px -1px 0 rgba(0, 0, 0, 0.85),
      1px -1px 0 rgba(0, 0, 0, 0.85),
      -1px 1px 0 rgba(0, 0, 0, 0.85),
      1px 1px 0 rgba(0, 0, 0, 0.85),
      0 0 4px rgba(0, 0, 0, 0.6);
  }

  .danmaku-item.superchat {
    color: #ffd54f !important;
  }
  .danmaku-item.member {
    color: #66bb6a !important;
  }

  @keyframes danmaku-fly {
    from {
      transform: translateX(100vw);
    }
    to {
      transform: translateX(-100%);
    }
  }
</style>
