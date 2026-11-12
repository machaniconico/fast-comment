<script lang="ts">
  import { store } from '../stores.svelte';
  import type { ChatMessage } from '../types';

  let items = $derived(store.firstCommentMessages);

  function bodyText(message: ChatMessage): string {
    return message.fragments.map((frag) => frag.type === 'text' ? frag.text : frag.name).join('');
  }
</script>

<div class="first-panel" aria-label="初見コメント一覧">
  <div class="head">
    <span class="title">初見コメント(直近50件)</span>
    <span class="total-count" aria-live="polite">{items.length}件</span>
  </div>

  {#if items.length === 0}
    <div class="empty">初見のコメントはまだありません</div>
  {:else}
    <div class="first-list" role="list">
      {#each items as msg (msg.id)}
        <article class="first-row" role="listitem">
          <div class="meta">
            <span class="author">{msg.author.nickname ?? msg.author.name}</span>
            <span class="platform">{msg.platform}</span>
          </div>
          <div class="text">{bodyText(msg)}</div>
        </article>
      {/each}
    </div>
  {/if}
</div>

<style>
  .first-panel {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    background: #121212;
    display: flex;
    flex-direction: column;
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 6px 10px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.1);
    flex-shrink: 0;
  }

  .title {
    color: #ddd;
    font-size: 12px;
    font-weight: 700;
  }

  .total-count {
    color: #888;
    font-size: 11px;
    white-space: nowrap;
  }

  .empty {
    padding: 24px 10px;
    color: #777;
    font-size: 12px;
    text-align: center;
  }

  .first-list {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }

  .first-row {
    display: flex;
    flex-direction: column;
    gap: 3px;
    padding: 8px 10px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.07);
    border-left: 3px solid #00c853;
  }

  .meta {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }

  .author {
    min-width: 0;
    color: #fff;
    font-size: 13px;
    font-weight: 700;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .platform {
    flex-shrink: 0;
    color: #888;
    font-size: 10px;
    text-transform: uppercase;
  }

  .text {
    color: #ddd;
    font-size: 13px;
    line-height: 1.4;
    overflow-wrap: anywhere;
  }
</style>
