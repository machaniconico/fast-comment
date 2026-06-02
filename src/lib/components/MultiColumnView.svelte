<script lang="ts">
  import type { Platform } from '../types';
  import CommentList from './CommentList.svelte';
  import { store } from '../stores.svelte';

  const columns: { platform: Platform; label: string; className: string }[] = [
    { platform: 'twitch', label: 'Twitch', className: 'twitch' },
    { platform: 'youtube', label: 'YouTube', className: 'youtube' },
  ];

  function platformCount(platform: Platform): number {
    return store.allMessages.filter((msg) => msg.platform === platform && !store.hiddenIds.has(msg.id)).length;
  }
</script>

<div class="multi-column-view" aria-label="プラットフォーム別コメント一覧">
  {#each columns as column}
    <section class="comment-column" aria-label={`${column.label} コメント`}>
      <header class="column-header">
        <span class={`platform-mark ${column.className}`}></span>
        <span class="column-title">{column.label}</span>
        <span class="column-count">{platformCount(column.platform)}</span>
      </header>
      <CommentList platformFilter={column.platform} />
    </section>
  {/each}
</div>

<style>
  .multi-column-view {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 1px;
    background: rgba(255, 255, 255, 0.08);
    overflow: hidden;
  }

  .comment-column {
    min-width: 0;
    min-height: 0;
    display: flex;
    flex-direction: column;
    background: #121212;
    overflow: hidden;
  }

  .column-header {
    height: 30px;
    display: flex;
    align-items: center;
    gap: 7px;
    padding: 0 9px;
    background: #171717;
    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
    flex-shrink: 0;
  }

  .platform-mark {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .platform-mark.twitch {
    background: #9146ff;
  }

  .platform-mark.youtube {
    background: #ff4444;
  }

  .column-title {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: #e8e8e8;
    font-size: 12px;
    font-weight: 700;
  }

  .column-count {
    margin-left: auto;
    padding: 1px 6px;
    border-radius: 10px;
    background: rgba(255, 255, 255, 0.11);
    color: #9e9e9e;
    font-size: 11px;
    line-height: 16px;
    flex-shrink: 0;
  }

  @media (max-width: 720px) {
    .multi-column-view {
      grid-template-columns: 1fr;
      grid-template-rows: repeat(2, minmax(0, 1fr));
    }
  }
</style>
