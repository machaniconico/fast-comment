<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import {
    clearTtsQueue,
    getTtsQueueState,
    onStats,
    onTtsQueueState,
    setTtsPaused,
    skipCurrentTts,
  } from '../ipc';
  import type { ChannelStatus, TtsQueueState } from '../ipc';
  import { PLATFORM_LABELS, PLATFORM_SHORT_LABELS, platformColor } from '../platform';
  import { theme } from '../theme.svelte';

  interface ViewerEntry {
    platform: string;
    platformLabel: string;
    shortLabel: string;
    identifier: string;
    title: string;
    viewers: number | null | undefined;
    viewersKind: 'concurrent' | 'cumulative';
    kindLabel: '同接' | '来場';
    displayName: string;
    color: string;
  }

  let queueState: TtsQueueState = $state({ depth: 0, paused: false, items: [] });
  let unlisten: (() => void) | null = null;
  let unlistenStats: (() => void) | null = null;
  let destroyed = false;
  let busy: boolean = $state(false);
  let error: string = $state('');
  let viewers: number = $state(0);
  let channelStatus: ChannelStatus[] = $state([]);
  let tick: number = $state(0);
  let rotationTimer: ReturnType<typeof setInterval> | null = null;

  const viewerNumberFormatter = new Intl.NumberFormat('ja-JP');
  const viewersLabel = $derived(viewerNumberFormatter.format(Math.max(0, viewers)));
  const viewerDisplay = $derived(theme.viewerDisplay);
  const viewerEntries = $derived.by(() => buildViewerEntries(channelStatus));
  const rotatedViewer = $derived(
    viewerEntries.length === 0 ? null : viewerEntries[tick % viewerEntries.length],
  );

  const visibleItems = $derived(queueState.items.slice(0, 6));
  const hiddenCount = $derived(Math.max(0, queueState.depth - visibleItems.length));

  onMount(async () => {
    rotationTimer = setInterval(() => {
      tick += 1;
    }, 10000);

    try {
      const current = await getTtsQueueState();
      if (current) queueState = normalizeQueueState(current);
      const fn = await onTtsQueueState((next) => {
        queueState = normalizeQueueState(next);
      });
      if (destroyed) fn();
      else unlisten = fn;
    } catch (e) {
      error = `TTSキュー状態を取得できません: ${formatError(e)}`;
    }

    try {
      // 同接数は stats イベント(~1/s)で更新。初期値は0、最初のイベントで即埋まる。
      const statsFn = await onStats((s) => {
        viewers = s.viewers;
        channelStatus = s.channelStatus ?? [];
      });
      if (destroyed) statsFn();
      else unlistenStats = statsFn;
    } catch {
      // 同接数は補助表示。取得失敗時は0のままにし、TTS操作は妨げない。
    }
  });

  onDestroy(() => {
    destroyed = true;
    unlisten?.();
    unlistenStats?.();
    if (rotationTimer !== null) {
      clearInterval(rotationTimer);
      rotationTimer = null;
    }
  });

  function buildViewerEntries(statuses: ChannelStatus[]): ViewerEntry[] {
    const liveStatuses = statuses.filter((status) => status.live !== false);
    const platformCounts = new Map<string, number>();
    for (const status of liveStatuses) {
      platformCounts.set(status.platform, (platformCounts.get(status.platform) ?? 0) + 1);
    }

    return liveStatuses.map((status) => {
      const platformLabel = PLATFORM_LABELS[status.platform] ?? status.platform;
      const viewersKind = status.viewersKind === 'cumulative' ? 'cumulative' : 'concurrent';
      return {
        platform: status.platform,
        platformLabel,
        shortLabel: PLATFORM_SHORT_LABELS[status.platform] ?? platformLabel,
        identifier: status.identifier,
        title: status.title ?? status.identifier,
        viewers: status.viewers,
        viewersKind,
        kindLabel: viewersKind === 'cumulative' ? '来場' : '同接',
        displayName: platformCounts.get(status.platform)! >= 2
          ? `${platformLabel} ${truncateIdentifier(status.identifier)}`
          : platformLabel,
        color: platformColor(status.platform),
      };
    });
  }

  function truncateIdentifier(identifier: string): string {
    return identifier.length > 12 ? `${identifier.slice(0, 12)}…` : identifier;
  }

  function formatViewerCount(value: number | null | undefined): string {
    return value === null || value === undefined
      ? '—'
      : viewerNumberFormatter.format(value);
  }

  function viewerAriaLabel(entry: ViewerEntry): string {
    return `${entry.platformLabel} ${entry.kindLabel} ${formatViewerCount(entry.viewers)}`;
  }

  function normalizeQueueState(next: TtsQueueState): TtsQueueState {
    return {
      depth: Math.max(0, next.depth || 0),
      paused: next.paused === true,
      items: Array.isArray(next.items) ? next.items : [],
    };
  }

  function formatError(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
  }

  async function runControl(action: () => Promise<void>) {
    if (busy) return;
    busy = true;
    error = '';
    try {
      await action();
    } catch (e) {
      error = `TTS操作に失敗しました: ${formatError(e)}`;
    } finally {
      busy = false;
    }
  }

  function togglePaused() {
    void runControl(() => setTtsPaused(!queueState.paused));
  }

  function clearQueue() {
    void runControl(clearTtsQueue);
  }

  function skipCurrent() {
    void runControl(skipCurrentTts);
  }

  // 画面ごと再読み込みする。コメント一覧を含む UI 状態が白紙に戻る
  // (Rust 側の受信は生きたままで、重複除去も跨いで効くため過去分は再流入しない)。
  function reloadApp() {
    window.location.reload();
  }
</script>

<section class="tts-queue-panel" aria-label="読み上げキュー">
  <div class="summary">
    <div class="metric">
      <span class="metric-label">TTS</span>
      <strong>{queueState.depth}</strong>
    </div>
    <span class:paused={queueState.paused} class="status">
      {queueState.paused ? '一時停止中' : '動作中'}
    </span>
  </div>

  <div class="controls" role="group" aria-label="読み上げキュー操作">
    <button
      class="control-btn"
      onclick={reloadApp}
      title="画面を再読み込み（コメント一覧もリセット）"
    >
      リロード
    </button>
    <button class="control-btn" onclick={togglePaused} disabled={busy}>
      {queueState.paused ? '再開' : '一時停止'}
    </button>
    <button class="control-btn" onclick={skipCurrent} disabled={busy}>スキップ</button>
    <button
      class="control-btn danger"
      onclick={clearQueue}
      disabled={busy}
      title="読み上げ待ちのキューを空にする（コメント一覧は消えません）"
    >
      TTS全消し
    </button>
  </div>

  <div class="bottom-row">
    <div class="viewers-row">
      {#if viewerDisplay === 'all'}
        {#if viewerEntries.length === 0}
          <div class="metric viewers" title="同時接続数（視聴者数）">
            <span class="metric-label">同接</span>
            <strong aria-label={`同時接続数 ${viewersLabel}`}>{viewersLabel}</strong>
          </div>
        {:else}
          <div class="metric viewers viewer-list all-viewers" aria-label="プラットフォーム別の視聴者数">
            {#each viewerEntries as entry}
              <span class="viewer-entry" title={entry.title}>
                <span class="viewer-platform">{entry.shortLabel}</span>
                <strong
                  class="viewer-value"
                  style:color={entry.color}
                  aria-label={viewerAriaLabel(entry)}
                >{formatViewerCount(entry.viewers)}</strong>
                {#if entry.viewersKind === 'cumulative'}
                  <span class="viewer-kind-suffix">来場</span>
                {/if}
              </span>
            {/each}
          </div>
        {/if}
      {:else if rotatedViewer}
        <div class="metric viewers viewer-single" title={rotatedViewer.title}>
          <span class="metric-label">{rotatedViewer.kindLabel}</span>
          <strong
            class="viewer-value"
            style:color={rotatedViewer.color}
            aria-label={viewerAriaLabel(rotatedViewer)}
          >{formatViewerCount(rotatedViewer.viewers)}</strong>
          <span class="viewer-name">{rotatedViewer.displayName}</span>
        </div>
      {:else}
        <div class="metric viewers" title="同時接続数（視聴者数）">
          <span class="metric-label">同接</span>
          <strong aria-label={`同時接続数 ${viewersLabel}`}>{viewersLabel}</strong>
        </div>
      {/if}
    </div>

    <div class="items" role="list" aria-label="読み上げ待ち項目">
      {#if visibleItems.length === 0}
        <span class="empty">待ち項目なし</span>
      {:else}
        {#each visibleItems as item (item.id)}
          <div class="item" role="listitem" title={item.preview}>
            <span class="item-preview">{item.preview}</span>
          </div>
        {/each}
        {#if hiddenCount > 0}
          <span class="more">+{hiddenCount}</span>
        {/if}
      {/if}
    </div>
  </div>

  {#if error}
    <p class="error">{error}</p>
  {/if}
</section>

<style>
  /* 上段: TTS状態＋操作ボタン / 下段: 同接表示＋読み上げ待ち。
     同接は「すべて表示」でも横に伸びられるよう、ボタンと同じ行に置かない。 */
  .tts-queue-panel {
    display: grid;
    grid-template-columns: minmax(0, auto) minmax(0, 1fr);
    align-items: center;
    column-gap: 8px;
    row-gap: 2px;
    padding: 4px 8px;
    background: #151515;
    border-bottom: 1px solid rgba(255,255,255,0.06);
    flex-shrink: 0;
  }

  .summary {
    grid-row: 1;
    grid-column: 1;
    display: flex;
    align-items: center;
    gap: 6px;
    min-height: 26px;
    min-width: 0;
    max-width: 100%;
  }

  /* 配信が増えて1行に収まらないときは、items を次の行へ送って同接を優先させる */
  .bottom-row {
    grid-row: 2;
    grid-column: 1 / -1;
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    column-gap: 8px;
    row-gap: 2px;
    min-width: 0;
    min-height: 20px;
  }

  /* 同接は自分の内容幅を使い、足りなくなったら items より先に詰める */
  .viewers-row {
    display: flex;
    align-items: center;
    flex: 0 1 auto;
    min-width: 0;
  }

  .metric {
    display: flex;
    align-items: center;
    gap: 5px;
    min-width: 52px;
  }

  .metric.viewers {
    min-width: 0;
    max-width: 100%;
  }

  .metric.viewers strong {
    color: #7fc8ff;
    min-width: 0;
  }

  .viewer-single {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
  }

  .viewer-value {
    flex-shrink: 0;
  }

  .viewer-name {
    min-width: 0;
    max-width: 180px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: #bdbdbd;
    font-size: 11px;
  }

  .viewer-list {
    display: flex;
    align-items: center;
    gap: 4px;
    flex: 1 1 auto;
    min-width: 0;
  }

  /* 「すべて表示」は配信数が読めないので、切り捨てずに折り返して全件見せる */
  .all-viewers {
    flex-wrap: wrap;
    row-gap: 2px;
  }

  .viewer-entry {
    display: flex;
    align-items: center;
    gap: 3px;
    min-width: 0;
    flex: 0 1 auto;
    white-space: nowrap;
    font-size: 10px;
  }

  .viewer-platform {
    color: #bdbdbd;
    font-weight: 700;
  }

  .viewer-entry strong {
    font-size: 11px;
    text-align: left;
  }

  .viewer-kind-suffix {
    color: #a8a8a8;
    font-size: 10px;
  }

  .metric-label {
    color: #8f8f8f;
    font-size: 11px;
    font-weight: 700;
  }

  strong {
    color: #f5f5f5;
    font-size: 13px;
    line-height: 1;
    min-width: 18px;
    text-align: right;
  }

  .status {
    color: #94d3a2;
    font-size: 11px;
    font-weight: 700;
    white-space: nowrap;
  }

  .status.paused {
    color: #f3c46b;
  }

  .controls {
    grid-row: 1;
    grid-column: 2;
    justify-self: end;
    display: flex;
    align-items: center;
    gap: 4px;
    flex-shrink: 0;
  }

  .control-btn {
    min-height: 24px;
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    background: rgba(255,255,255,0.07);
    color: #e6e6e6;
    font-size: 11px;
    font-weight: 700;
    padding: 3px 8px;
    cursor: pointer;
    white-space: nowrap;
  }

  .control-btn:hover:not(:disabled) {
    background: rgba(255,255,255,0.13);
    border-color: rgba(255,255,255,0.22);
  }

  .control-btn.danger {
    color: #ffb4b4;
  }

  .control-btn:disabled {
    opacity: 0.48;
    cursor: not-allowed;
  }

  .items {
    display: flex;
    align-items: center;
    gap: 4px;
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
  }

  .item {
    max-width: 220px;
    min-width: 44px;
    padding: 3px 7px;
    border: 1px solid rgba(255,255,255,0.08);
    border-radius: 4px;
    background: rgba(255,255,255,0.045);
    color: #d8d8d8;
    font-size: 11px;
    line-height: 1.35;
    overflow: hidden;
  }

  .item-preview {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .empty {
    color: #696969;
    font-size: 11px;
  }

  .more {
    color: #9e9e9e;
    font-size: 11px;
    font-weight: 700;
    flex-shrink: 0;
  }

  .error {
    grid-row: 3;
    grid-column: 1 / -1;
    margin: 0;
    color: #ff8f8f;
    font-size: 11px;
  }

  @media (max-width: 760px) {
    .item {
      max-width: 160px;
    }
  }
</style>
