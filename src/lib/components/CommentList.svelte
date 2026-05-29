<script lang="ts">
  /**
   * Virtual scrolling comment list.
   *
   * Approach: overflow-y:auto container with a tall inner div (height = itemCount * ROW_HEIGHT).
   * Only rows in the visible window [startIdx, endIdx) are rendered.
   * Auto-scroll to bottom when new messages arrive, unless the user has scrolled up.
   */
  import { onMount, untrack } from 'svelte';
  import type { ChatMessage } from '../types';
  import CommentItem from './CommentItem.svelte';
  import { store } from '../stores.svelte';

  // Row height in px — fixed height keeps virtual math simple.
  const ROW_HEIGHT = 28;
  // Overscan rows above/below visible window to reduce pop-in.
  const OVERSCAN = 5;

  let containerEl: HTMLDivElement;
  let scrollTop = $state(0);
  // Real value is measured onMount/onScroll. We do not render the virtual
  // window until `measured` is true, so there is never a one-frame mis-size
  // from a guessed height before layout.
  let clientHeight = $state(0);
  let measured = $state(false);
  let isAtBottom = $state(true);

  let messages: ChatMessage[] = $derived(store.visibleMessages);
  let totalCount = $derived(messages.length);
  let totalHeight = $derived(totalCount * ROW_HEIGHT);

  let startIdx = $derived(
    Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN)
  );
  // Until the container is measured (post-mount), do NOT guess a viewport
  // height — gate the visible window on `measured` so the first paint never
  // mis-sizes from a hardcoded default. The total-height spacer still renders,
  // so the scrollbar/layout is correct; only the row window waits for measure.
  let endIdx = $derived(
    !measured
      ? 0
      : Math.min(totalCount, Math.ceil((scrollTop + clientHeight) / ROW_HEIGHT) + OVERSCAN)
  );
  let visibleSlice = $derived(messages.slice(startIdx, endIdx));
  let paddingTop = $derived(startIdx * ROW_HEIGHT);

  // Scroll to bottom when new messages arrive and the user is at the bottom.
  // prevCount is tracked via untrack() so it does not become a reactive dep of
  // this effect (reading/writing a plain reactive would be fragile in Svelte 5).
  let prevCount = $state(0);
  $effect(() => {
    const count = totalCount;
    const last = untrack(() => prevCount);
    if (count !== last) {
      untrack(() => { prevCount = count; });
      if (isAtBottom && containerEl) {
        // schedule after DOM update
        requestAnimationFrame(() => {
          containerEl.scrollTop = containerEl.scrollHeight;
        });
      }
    }
  });

  function onScroll() {
    if (!containerEl) return;
    scrollTop = containerEl.scrollTop;
    clientHeight = containerEl.clientHeight;
    const distFromBottom = containerEl.scrollHeight - containerEl.scrollTop - containerEl.clientHeight;
    isAtBottom = distFromBottom < ROW_HEIGHT * 2;
  }

  onMount(() => {
    clientHeight = containerEl.clientHeight;
    measured = true;
    const ro = new ResizeObserver(() => {
      clientHeight = containerEl.clientHeight;
    });
    ro.observe(containerEl);
    return () => ro.disconnect();
  });
</script>

<div
  class="comment-list-container"
  bind:this={containerEl}
  onscroll={onScroll}
  role="list"
  aria-label="コメント一覧"
>
  <!-- Total height spacer -->
  <div class="inner" style:height="{totalHeight}px">
    <!-- Rendered window -->
    <div class="window" style:transform="translateY({paddingTop}px)">
      {#each visibleSlice as msg (msg.id)}
        <CommentItem message={msg} />
      {/each}
    </div>
  </div>

  <!-- Scroll-to-bottom button when not at bottom -->
  {#if !isAtBottom && totalCount > 0}
    <button
      class="scroll-bottom-btn"
      onclick={() => { containerEl.scrollTop = containerEl.scrollHeight; isAtBottom = true; }}
      aria-label="最新コメントへ"
    >
      ▼ 最新
    </button>
  {/if}
</div>

<style>
  .comment-list-container {
    flex: 1;
    overflow-y: auto;
    overflow-x: hidden;
    position: relative;
    scrollbar-width: thin;
    scrollbar-color: rgba(255, 255, 255, 0.2) transparent;
  }

  .comment-list-container::-webkit-scrollbar {
    width: 4px;
  }
  .comment-list-container::-webkit-scrollbar-thumb {
    background: rgba(255, 255, 255, 0.2);
    border-radius: 2px;
  }

  .inner {
    position: relative;
    min-height: 100%;
  }

  .window {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
  }

  .scroll-bottom-btn {
    position: sticky;
    bottom: 8px;
    left: 50%;
    transform: translateX(-50%);
    display: block;
    margin: 0 auto;
    background: rgba(33, 33, 33, 0.92);
    color: #fff;
    border: 1px solid rgba(255, 255, 255, 0.2);
    border-radius: 16px;
    padding: 4px 14px;
    font-size: 12px;
    cursor: pointer;
    z-index: 10;
    transition: background 0.15s;
  }

  .scroll-bottom-btn:hover {
    background: rgba(60, 60, 60, 0.98);
  }
</style>
