<script lang="ts">
  import type { ChatMessage } from '../types';
  import { store } from '../stores.svelte';
  import { hideMessage as ipcHideMessage } from '../ipc';

  interface Props {
    message: ChatMessage;
  }

  let { message }: Props = $props();

  const PLATFORM_COLORS: Record<string, string> = {
    twitch: '#9146ff',
    youtube: '#ff0000',
  };

  const KIND_BG: Record<string, string> = {
    superChat: '#ffd600',
    membership: '#00c853',
    bits: '#9146ff',
    system: '#37474f',
  };

  const KIND_TEXT: Record<string, string> = {
    superChat: '#000',
    membership: '#fff',
    bits: '#fff',
    system: '#fff',
  };

  let platformColor = $derived(PLATFORM_COLORS[message.platform] ?? '#888');
  let isHighlighted = $derived(
    message.kind === 'superChat' ||
    message.kind === 'membership' ||
    message.kind === 'bits'
  );
  let highlightBg = $derived(KIND_BG[message.kind] ?? 'transparent');
  let highlightFg = $derived(KIND_TEXT[message.kind] ?? 'inherit');

  function onHide() {
    store.hideMessage(message.id);
    ipcHideMessage(message.id);
  }

  function formatTime(ms: number): string {
    const d = new Date(ms);
    const h = d.getHours().toString().padStart(2, '0');
    const m = d.getMinutes().toString().padStart(2, '0');
    const s = d.getSeconds().toString().padStart(2, '0');
    return `${h}:${m}:${s}`;
  }
</script>

<div
  class="comment-item"
  class:highlighted={isHighlighted}
  style:background={isHighlighted ? highlightBg : undefined}
  style:color={isHighlighted ? highlightFg : undefined}
  role="listitem"
>
  <!-- Platform indicator -->
  <span class="platform-dot" style:background={platformColor} title={message.platform}></span>

  <!-- Badges -->
  {#each message.author.badges as badge}
    {#if badge.imageUrl}
      <img class="badge-img" src={badge.imageUrl} alt={badge.label} title={badge.label} />
    {:else}
      <span class="badge-text" title={badge.label}>{badge.kind[0]?.toUpperCase()}</span>
    {/if}
  {/each}

  <!-- Author name -->
  <span
    class="author-name"
    style:color={!isHighlighted && message.author.displayColor ? message.author.displayColor : undefined}
  >
    {message.author.name}
  </span>

  <!-- Separator -->
  <span class="sep">:</span>

  <!-- Amount badge for SuperChat/Bits -->
  {#if message.amount}
    <span class="amount-badge">{message.amount.rawText}</span>
  {/if}

  <!-- Fragments -->
  <span class="fragments">
    {#each message.fragments as frag}
      {#if frag.type === 'text'}
        <span>{frag.text}</span>
      {:else if frag.type === 'emote'}
        <img class="emote" src={frag.url} alt={frag.name} title={frag.name} />
      {/if}
    {/each}
  </span>

  <!-- Time + hide button -->
  <span class="meta">
    <span class="time">{formatTime(message.timestampMs)}</span>
    <button class="hide-btn" onclick={onHide} title="非表示" aria-label="このコメントを非表示">✕</button>
  </span>
</div>

<style>
  .comment-item {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 3px 6px;
    font-size: 13px;
    line-height: 1.4;
    border-bottom: 1px solid rgba(255, 255, 255, 0.05);
    min-height: 26px;
    box-sizing: border-box;
    position: relative;
  }

  .comment-item:hover .hide-btn {
    opacity: 1;
  }

  .comment-item.highlighted {
    border-radius: 3px;
    padding: 5px 6px;
    margin: 2px 4px;
    font-weight: 500;
  }

  .platform-dot {
    display: inline-block;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .badge-img {
    width: 18px;
    height: 18px;
    vertical-align: middle;
    flex-shrink: 0;
  }

  .badge-text {
    display: inline-block;
    width: 16px;
    height: 16px;
    font-size: 10px;
    line-height: 16px;
    text-align: center;
    background: rgba(255, 255, 255, 0.2);
    border-radius: 2px;
    flex-shrink: 0;
  }

  .author-name {
    font-weight: 600;
    white-space: nowrap;
    flex-shrink: 0;
    max-width: 140px;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .sep {
    flex-shrink: 0;
    opacity: 0.5;
  }

  .amount-badge {
    display: inline-block;
    padding: 1px 6px;
    background: rgba(0, 0, 0, 0.3);
    border-radius: 10px;
    font-size: 11px;
    font-weight: 700;
    flex-shrink: 0;
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
  }

  .emote {
    height: 20px;
    width: auto;
    vertical-align: middle;
    flex-shrink: 0;
  }

  .meta {
    display: flex;
    align-items: center;
    gap: 4px;
    margin-left: auto;
    flex-shrink: 0;
  }

  .time {
    font-size: 10px;
    opacity: 0.4;
    white-space: nowrap;
  }

  .hide-btn {
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    padding: 0 2px;
    font-size: 10px;
    line-height: 1;
    opacity: 0;
    transition: opacity 0.15s;
  }

  .hide-btn:hover {
    opacity: 1 !important;
    color: #f44336;
  }
</style>
