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
  import { listen } from '@tauri-apps/api/event';
  import { getCurrentWindow } from '@tauri-apps/api/window';
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

  // ── E4: 連投まとめ(同一本文の連投を ×N に集約)──────────────────────────
  // 集約窓口(ms)。同じ本文の連投がこの時間内なら 1 つにまとめる。
  const COALESCE_WINDOW_MS = 1500;
  // key=本文 core(名前前置前の本文)。Svelte の items は id 管理なので el ではなく
  // item.id を保持し、生存判定は items.some(it=>it.id===entry.id) で行う。
  let recentByText = new Map<string, { id: number; base: string; count: number; at: number }>();

  // ── E5: 投げ銭/メンバー固定強調弾幕(流さず上部に固定表示してフェード)──
  const PIN_SEC = 8;
  const PIN_MAX = 6;
  type Pin = { id: number; text: string; kind: ChatMessage['kind']; leaving: boolean };
  let pins = $state<Pin[]>([]);
  // in-flight タイマ(onDestroy で clearTimeout し leak を防ぐ)。
  let pinTimers: ReturnType<typeof setTimeout>[] = [];

  // ── レーン(行)管理 ──────────────────────────────────────────────────────
  type LanePrev = { t: number; w: number; s: number } | null;
  let viewportW = 0;
  let laneHeight = 0;
  let laneCount = 1;
  // 表示縦帯(画面上半分/下半分/全体)。recomputeLanes で更新。
  let y0 = 0;
  // 各レーンの直近投入コメントの記録 {t,w,s} または null。
  //   t=投入時刻(performance.now() ms), w=実幅 px, s=速度 px/秒。
  // laneFreeAt(時刻配列)から置換: 相対速度を考慮した精密追突防止に用いる。
  let lanePrev: LanePrev[] = [];

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

  function recomputeLanes() {
    viewportW = window.innerWidth || 1920;
    const viewportH = window.innerHeight || 1080;
    const nextLaneHeight = Math.round(settings.fontSize * 1.45);
    // E2: settings.area で表示縦帯 [y0,y1] を決める(中央のゲーム画面を空ける)。
    let ya = 0;
    let yb = viewportH;
    if (settings.area === 'top') yb = Math.floor(viewportH / 2);
    else if (settings.area === 'bottom') ya = Math.floor(viewportH / 2);
    const bandH = yb - ya;
    const nextLaneCount = Math.max(1, Math.floor(bandH / nextLaneHeight));
    if (lanePrev.length !== nextLaneCount || laneHeight !== nextLaneHeight) {
      // レーン数/高さが変わったら lanePrev を作り直す(既存値は捨てて fill(null))。
      lanePrev = new Array(nextLaneCount).fill(null);
    }
    y0 = ya;
    laneHeight = nextLaneHeight;
    laneCount = nextLaneCount;
  }

  // E1: 精密追突防止。先行コメント(prev)と新規コメント(sNew px/秒)の相対速度を考慮し、
  // 追突しないのに十分な投入間隔(ms)を返す。prev==null なら 0(空きレーン)。
  //   base = prev.w + gap(gap=fontSize, 1文字ぶんの間隔)
  //   A = base / prev.s        ← 先行が画面端に消えるまでの時間(秒)
  //   B = (sNew>prev.s) ? (base + (sNew-prev.s)*durationSec) / sNew : A
  //                            ← 新規が速いとき、相対距離が base に開くまでの時間(秒)
  //   return max(A,B) * 1000
  function requiredGapMs(prev: LanePrev, sNew: number): number {
    if (!prev) return 0;
    const base = prev.w + settings.fontSize;
    const a = base / prev.s;
    const b = sNew > prev.s ? (base + (sNew - prev.s) * settings.durationSec) / sNew : a;
    return Math.max(a, b) * 1000;
  }

  // 各レーンの slack(今投入可能か余裕 ms)を比較し、最も空いているレーンを選ぶ。
  //   slack = (now - prev.t) - requiredGapMs(prev, sNew)
  //   prev==null は slack=+Infinity(完全に空き)。
  // 空き(slack>=0)があれば slack 最大のレーン、無ければ slack 最大に相乗り(劣化許容)。
  function pickLane(sNew: number): number {
    const now = performance.now();
    let best = 0;
    let bestSlack = -Infinity;
    for (let i = 0; i < laneCount; i += 1) {
      const prev = lanePrev[i] ?? null;
      const slack = prev ? now - prev.t - requiredGapMs(prev, sNew) : Infinity;
      if (slack > bestSlack) {
        best = i;
        bestSlack = slack;
      }
    }
    return best;
  }

  // E5: 投げ銭/メンバーを流さず画面上部に固定表示する。
  // 既存の金額/ラベルfallbackで表示文字列を作り、pins 先頭に unshift。
  // PIN_MAX 超過で末尾を pop。PIN_SEC 秒後に leaving:true → ~400ms 後に除去。
  function pinGift(msg: ChatMessage) {
    const kind = msg.kind;
    let body = msg.fragments
      .map((f) => (f.type === 'text' ? f.text : f.type === 'emote' ? f.name : ''))
      .join('')
      .trim();
    if (kind === 'superChat' || kind === 'bits') {
      const amountText = msg.amount?.rawText?.trim();
      if (amountText) body = body ? `${amountText} ${body}` : amountText;
    }
    if (kind === 'membership') body = body || 'メンバー加入';
    if (!body) return;

    const id = ++seq;
    pins.unshift({ id, text: body, kind, leaving: false });
    if (pins.length > PIN_MAX) pins.pop();

    const leaveTimer = setTimeout(() => {
      const p = pins.find((it) => it.id === id);
      if (p) p.leaving = true;
      const removeTimer = setTimeout(() => {
        const i = pins.findIndex((it) => it.id === id);
        if (i >= 0) pins.splice(i, 1);
      }, 400);
      pinTimers.push(removeTimer);
    }, PIN_SEC * 1000);
    pinTimers.push(leaveTimer);
  }

  function spawn(msg: ChatMessage) {
    const kind = msg.kind ?? 'normal';
    if (kind === 'system') return;

    // E5: 投げ銭/メンバーは固定強調弾幕へ(OFF のときは従来通り流す)。
    if (settings.pinGifts && (kind === 'superChat' || kind === 'bits' || kind === 'membership')) {
      pinGift(msg);
      return;
    }

    let body = msg.fragments
      .map((f) => (f.type === 'text' ? f.text : f.type === 'emote' ? f.name : ''))
      .join('')
      .trim();
    if (kind === 'superChat' || kind === 'bits') {
      const amountText = msg.amount?.rawText?.trim();
      if (amountText) body = body ? `${amountText} ${body}` : amountText;
    }
    if (kind === 'membership') body = body || 'メンバー加入';
    if (!body) return;

    const now = performance.now();

    // E4: 連投まとめ(非gift=通常コメントのみ)。key=core=名前前置前の本文。
    // 既存 entry があり窓口内かつその id の item がまだ items に在るなら集約して spawn 中断。
    if (settings.coalesce && kind === 'normal') {
      const entry = recentByText.get(body);
      if (
        entry &&
        now - entry.at <= COALESCE_WINDOW_MS &&
        items.some((it) => it.id === entry.id)
      ) {
        entry.count += 1;
        entry.at = now;
        const target = items.find((it) => it.id === entry.id);
        if (target) target.text = `${entry.base} ×${entry.count}`;
        return;
      }
    }

    const text = settings.showName && msg.author?.name ? `${msg.author.name}: ${body}` : body;

    const w = measureWidth(text, settings.fontSize);
    // 等速。画面幅+自分の幅を durationSec で割った速度(px/s)。
    const s = (viewportW + w) / settings.durationSec;
    const lane = pickLane(s);
    // 選択レーンの直近投入を更新(次回の requiredGapMs 計算に使う)。
    lanePrev[lane] = { t: now, w, s };

    const dc = msg.author?.displayColor;
    const color = dc && dc.trim() ? dc : '#ffffff';

    const newId = ++seq;
    items.push({
      id: newId,
      text,
      color,
      top: y0 + lane * laneHeight,
      durationSec: settings.durationSec,
      fontSize: settings.fontSize,
      kind,
    });
    // E4: 新規 spawn 後に Map 登録(次回の連投集約に使う)。
    if (settings.coalesce && kind === 'normal') {
      recentByText.set(body, { id: newId, base: text, count: 1, at: now });
    }
    // 上限超過は古いものから捨てる(アニメ未終了でも DOM 肥大を防ぐ)。
    if (items.length > settings.maxActive) items.splice(0, items.length - settings.maxActive);
  }

  function onEnd(id: number) {
    const i = items.findIndex((it) => it.id === id);
    if (i >= 0) items.splice(i, 1);
    // E4: recentByText から value.id===id のエントリを削除(肥大防止)。
    for (const [key, value] of recentByText) {
      if (value.id === id) {
        recentByText.delete(key);
        break;
      }
    }
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
    // E5: in-flight タイマを破棄(リーク防止)。
    for (const t of pinTimers) clearTimeout(t);
    pinTimers.length = 0;
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
      onanimationend={(e) => {
        if (e.animationName === 'danmaku-fly') onEnd(item.id);
      }}
    >
      {item.text}
    </div>
  {/each}
</div>

<div class="danmaku-pins">
  {#each pins as p (p.id)}
    <div
      class="danmaku-pin"
      class:superchat={p.kind === 'superChat' || p.kind === 'bits'}
      class:member={p.kind === 'membership'}
      class:leaving={p.leaving}
    >{p.text}</div>
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
    /* E3: 端フェード。fly(移動)と fade(不透明度)を2本同時再生。
       右端の出現と左端の消失を opacity でなめらかに。 */
    animation:
      danmaku-fly var(--dur) linear forwards,
      danmaku-fade var(--dur) linear forwards;
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

  /* E5: 投げ銭/メンバー固定強調弾幕。.danmaku-root の opacity とは独立。 */
  .danmaku-pins {
    position: fixed;
    top: 12px;
    left: 0;
    right: 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    pointer-events: none;
    z-index: 10;
  }

  .danmaku-pin {
    background: rgba(0, 0, 0, 0.55);
    border: 1px solid rgba(255, 255, 255, 0.25);
    border-radius: 8px;
    padding: 6px 14px;
    font-weight: 800;
    font-size: 18px;
    line-height: 1.2;
    color: #ffffff;
    white-space: nowrap;
    text-shadow:
      0 0 4px rgba(0, 0, 0, 0.8),
      -1px -1px 0 rgba(0, 0, 0, 0.85),
      1px -1px 0 rgba(0, 0, 0, 0.85),
      -1px 1px 0 rgba(0, 0, 0, 0.85),
      1px 1px 0 rgba(0, 0, 0, 0.85);
    transition: opacity 0.4s ease;
    opacity: 1;
  }

  .danmaku-pin.superchat {
    color: #ffd54f;
    border-color: rgba(255, 213, 79, 0.6);
  }

  .danmaku-pin.member {
    color: #66bb6a;
    border-color: rgba(102, 187, 106, 0.6);
  }

  .danmaku-pin.leaving {
    opacity: 0;
  }

  @keyframes danmaku-fly {
    from {
      transform: translateX(100vw);
    }
    to {
      transform: translateX(-100%);
    }
  }

  /* E3: 端フェード。冒頭6%で浮かび上がり、末尾94%→100%で消える。 */
  @keyframes danmaku-fade {
    0% {
      opacity: 0;
    }
    6% {
      opacity: 1;
    }
    94% {
      opacity: 1;
    }
    100% {
      opacity: 0;
    }
  }
</style>
