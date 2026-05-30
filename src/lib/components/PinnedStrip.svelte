<script lang="ts">
  import type { ChatMessage } from '../types';
  import { store, unpinMessage } from '../stores.svelte';

  const PLATFORM_COLORS: Record<string, string> = {
    twitch: '#9146ff',
    youtube: '#ff0000',
  };
</script>

{#if store.pinnedMessages.length > 0}
  <div class="pinned-strip" role="list" aria-label="ピン留めコメント">
    {#each store.pinnedMessages as msg (msg.id)}
      <div class="pinned-row" role="listitem">
        <!-- Pin marker -->
        <span class="pin-icon" aria-hidden="true">📌</span>

        <!-- Platform dot -->
        <span
          class="platform-dot"
          style:background={PLATFORM_COLORS[msg.platform] ?? '#888'}
          title={msg.platform}
        ></span>

        <!-- Author name -->
        <span
          class="author-name"
          style:color={msg.author.displayColor ?? undefined}
        >
          {msg.author.name}
        </span>

        <span class="sep" aria-hidden="true">:</span>

        <!-- Fragments (text concat + emote image/name) -->
        <span class="fragments">
          {#each msg.fragments as frag}
            {#if frag.type === 'text'}
              <span>{frag.text}</span>
            {:else if frag.type === 'emote'}
              {#if frag.url}
                <img class="emote" src={frag.url} alt={frag.name} title={frag.name} />
              {:else}
                <span class="emote-name">{frag.name}</span>
              {/if}
            {/if}
          {/each}
        </span>

        <!-- Unpin button -->
        <button
          class="unpin-btn"
          onclick={() => unpinMessage(msg.id)}
          title="ピンを外す"
          aria-label="{msg.author.name} のコメントのピンを外す"
        >✕</button>
      </div>
    {/each}
  </div>
{/if}

<style>
  .pinned-strip {
    background: rgba(255, 179, 0, 0.06);
    border-bottom: 1px solid rgba(255, 179, 0, 0.18);
    padding: 2px 0;
  }

  .pinned-row {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 0 6px;
    font-size: 13px;
    line-height: 1.4;
    height: 24px;
    box-sizing: border-box;
    overflow: hidden;
  }

  .pinned-row:not(:last-child) {
    border-bottom: 1px solid rgba(255, 179, 0, 0.08);
  }

  .pin-icon {
    font-size: 10px;
    flex-shrink: 0;
    line-height: 1;
  }

  .platform-dot {
    display: inline-block;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .author-name {
    font-weight: 600;
    white-space: nowrap;
    flex-shrink: 0;
    max-width: 120px;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .sep {
    flex-shrink: 0;
    opacity: 0.5;
  }

  .fragments {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    display: flex;
    align-items: center;
    gap: 2px;
    flex-wrap: nowrap;
    min-width: 0;
  }

  .fragments span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .emote {
    height: 18px;
    width: auto;
    vertical-align: middle;
    flex-shrink: 0;
  }

  .emote-name {
    font-size: 11px;
    opacity: 0.7;
    flex-shrink: 0;
  }

  .unpin-btn {
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    padding: 0 2px;
    font-size: 10px;
    line-height: 1;
    opacity: 0;
    transition: opacity 0.15s;
    flex-shrink: 0;
    margin-left: auto;
  }

  .pinned-row:hover .unpin-btn {
    opacity: 1;
  }

  .unpin-btn:hover {
    opacity: 1 !important;
    color: #f44336;
  }
</style>
