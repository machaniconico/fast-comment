<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import {
    clearTtsQueue,
    getTtsQueueState,
    onTtsQueueState,
    setTtsPaused,
    skipCurrentTts,
  } from '../ipc';
  import type { TtsQueueState } from '../ipc';

  let queueState: TtsQueueState = $state({ depth: 0, paused: false, items: [] });
  let unlisten: (() => void) | null = null;
  let busy: boolean = $state(false);
  let error: string = $state('');

  const visibleItems = $derived(queueState.items.slice(0, 6));
  const hiddenCount = $derived(Math.max(0, queueState.depth - visibleItems.length));

  onMount(async () => {
    try {
      const current = await getTtsQueueState();
      if (current) queueState = normalizeQueueState(current);
      unlisten = await onTtsQueueState((next) => {
        queueState = normalizeQueueState(next);
      });
    } catch (e) {
      error = `TTSキュー状態を取得できません: ${formatError(e)}`;
    }
  });

  onDestroy(() => {
    unlisten?.();
  });

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
    <button class="control-btn" onclick={togglePaused} disabled={busy}>
      {queueState.paused ? '再開' : '一時停止'}
    </button>
    <button class="control-btn" onclick={skipCurrent} disabled={busy}>スキップ</button>
    <button class="control-btn danger" onclick={clearQueue} disabled={busy}>全消し</button>
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

  {#if error}
    <p class="error">{error}</p>
  {/if}
</section>

<style>
  .tts-queue-panel {
    display: grid;
    grid-template-columns: auto auto minmax(140px, 1fr);
    align-items: center;
    gap: 8px;
    min-height: 36px;
    padding: 4px 8px;
    background: #151515;
    border-bottom: 1px solid rgba(255,255,255,0.06);
    flex-shrink: 0;
  }

  .summary {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 96px;
  }

  .metric {
    display: flex;
    align-items: center;
    gap: 5px;
    min-width: 52px;
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
    grid-column: 1 / -1;
    margin: 0;
    color: #ff8f8f;
    font-size: 11px;
  }

  @media (max-width: 760px) {
    .tts-queue-panel {
      grid-template-columns: 1fr auto;
      align-items: start;
    }

    .items {
      grid-column: 1 / -1;
    }

    .item {
      max-width: 160px;
    }
  }
</style>
