<script lang="ts">
  /**
   * Virtual scrolling comment list.
   *
   * Approach: overflow-y:auto container with a tall inner div (height = itemCount * ROW_HEIGHT).
   * Only rows in the visible window [startIdx, endIdx) are rendered.
   * Auto-scroll to bottom when new messages arrive, unless the user has scrolled up.
   */
  import { onMount, untrack } from 'svelte';
  import type { ChatMessage, Platform } from '../types';
  import ContextMenu from './ContextMenu.svelte';
  import type { ContextMenuItem } from './ContextMenu.svelte';
  import CommentItem from './CommentItem.svelte';
  import { store } from '../stores.svelte';
  import { theme } from '../theme.svelte';

  interface Props {
    platformFilter?: Platform;
  }

  let { platformFilter }: Props = $props();

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
  let menu: { x: number; y: number; items: ContextMenuItem[] } | null = $state(null);
  let wrap = $derived(theme.wrapComments);
  let heights = $state(new Map<string, number>());

  let messages: ChatMessage[] = $derived.by(() => {
    if (!platformFilter) return store.visibleMessages;

    const hidden = store.hiddenIds;
    const out: ChatMessage[] = [];
    for (const msg of store.allMessages) {
      if (hidden.has(msg.id)) continue;
      if (msg.platform !== platformFilter) continue;
      // Use the store's shared matcher so regex/text mode behaves identically
      // here as in the single-column visibleMessages path.
      if (!store.matchesSearch(messageSearchText(msg))) continue;
      out.push(msg);
    }
    return out;
  });
  let totalCount = $derived(messages.length);
  let offsets = $derived.by(() => {
    const n = messages.length;
    const arr = new Array<number>(n + 1);
    arr[0] = 0;
    for (let i = 0; i < n; i++) {
      const h = wrap ? (heights.get(messages[i].id) ?? ROW_HEIGHT) : ROW_HEIGHT;
      arr[i + 1] = arr[i] + h;
    }
    return arr;
  });
  let totalHeight = $derived(wrap ? offsets[messages.length] : totalCount * ROW_HEIGHT);

  let startIdx = $derived(
    wrap
      ? Math.max(0, bisectRight(offsets, scrollTop) - OVERSCAN)
      : Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN)
  );
  // Until the container is measured (post-mount), do NOT guess a viewport
  // height — gate the visible window on `measured` so the first paint never
  // mis-sizes from a hardcoded default. The total-height spacer still renders,
  // so the scrollbar/layout is correct; only the row window waits for measure.
  let endIdx = $derived(
    !measured
      ? 0
      : wrap
        ? Math.min(messages.length, bisectLeft(offsets, scrollTop + clientHeight) + OVERSCAN)
        : Math.min(totalCount, Math.ceil((scrollTop + clientHeight) / ROW_HEIGHT) + OVERSCAN)
  );
  let visibleSlice = $derived(messages.slice(startIdx, endIdx));
  let paddingTop = $derived(wrap ? offsets[startIdx] : startIdx * ROW_HEIGHT);

  // Scroll to bottom when new messages arrive and the user is at the bottom.
  // Monitors store.receivedCount (monotonically increasing) instead of
  // visibleMessages.length so that filter/search changes do not falsely trigger
  // a scroll, and buffer saturation (maxBuffer cap) does not kill auto-scroll.
  // prevReceived is tracked via untrack() so it does not become a reactive dep
  // of this effect (reading/writing a plain reactive would be fragile in Svelte 5).
  let prevReceived = $state(0);
  // Counts new messages received while not at the bottom.
  // Reset to 0 when the user returns to the bottom (onScroll or button click).
  let unreadCount = $state(0);

  $effect(() => {
    const ids = new Set(messages.map(m => m.id));
    let changed = false;
    const next = new Map(heights);
    for (const k of next.keys()) {
      if (!ids.has(k)) {
        next.delete(k);
        changed = true;
      }
    }
    if (changed) heights = next;
  });

  $effect(() => {
    const received = store.receivedCount;
    const last = untrack(() => prevReceived);
    if (received > last) {
      untrack(() => { prevReceived = received; });
      if (isAtBottom && containerEl) {
        // schedule after DOM update
        requestAnimationFrame(() => {
          containerEl.scrollTop = containerEl.scrollHeight;
        });
      } else {
        // Not at bottom — accumulate unread count instead of auto-scrolling.
        untrack(() => { unreadCount += (received - last); });
      }
    } else if (received !== last) {
      // received < last should not occur (monotonic), but sync on mismatch
      // (e.g. store replaced at module level in tests) without scrolling.
      untrack(() => { prevReceived = received; });
    }
  });

  function onScroll() {
    if (!containerEl) return;
    menu = null;
    scrollTop = containerEl.scrollTop;
    clientHeight = containerEl.clientHeight;
    const distFromBottom = containerEl.scrollHeight - containerEl.scrollTop - containerEl.clientHeight;
    const wasAtBottom = isAtBottom;
    isAtBottom = distFromBottom < ROW_HEIGHT * 2;
    // Reset unread count when user scrolls back to the bottom.
    if (!wasAtBottom && isAtBottom) {
      unreadCount = 0;
    }
  }

  function openContextMenu(request: { x: number; y: number; items: ContextMenuItem[] }) {
    menu = request;
  }

  function closeContextMenu() {
    menu = null;
  }

  // last index where offsets[i] <= target
  function bisectRight(arr: number[], target: number): number {
    let lo = 0, hi = arr.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (arr[mid] <= target) lo = mid; else hi = mid - 1;
    }
    return lo;
  }

  // first index where offsets[i] >= target
  function bisectLeft(arr: number[], target: number): number {
    let lo = 0, hi = arr.length - 1;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (arr[mid] >= target) hi = mid; else lo = mid + 1;
    }
    return lo;
  }

  function messageSearchText(msg: ChatMessage): string {
    const body = msg.fragments.map((frag) => frag.type === 'text' ? frag.text : frag.name).join('');
    return `${msg.author.name} ${body}`.toLowerCase();
  }

  function measure(node: HTMLElement, id: string) {
    if (!wrap) return {};

    const target = node.querySelector<HTMLElement>('.comment-item') ?? node;
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const h = entry.target instanceof HTMLElement ? entry.target.offsetHeight : entry.contentRect.height;
        const prev = heights.get(id) ?? ROW_HEIGHT;
        if (Math.abs(h - prev) > 0.5) {
          heights = new Map(heights).set(id, h);
        }
      }
    });
    ro.observe(target);

    return {
      destroy() {
        ro.disconnect();
      },
    };
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
  aria-label={platformFilter ? `${platformFilter} コメント一覧` : 'コメント一覧'}
>
  <!-- Total height spacer -->
  <div class="inner" style:height="{totalHeight}px">
    <!-- Rendered window -->
    <div class="window" style:transform="translateY({paddingTop}px)">
      {#key wrap}
        {#each visibleSlice as msg (msg.id)}
          <div class="measure-row" use:measure={msg.id} role="presentation">
            <CommentItem message={msg} onOpenContextMenu={openContextMenu} />
          </div>
        {/each}
      {/key}
    </div>
  </div>

  {#if menu}
    <ContextMenu x={menu.x} y={menu.y} items={menu.items} onClose={closeContextMenu} />
  {/if}

  <!-- Scroll-to-bottom button when not at bottom -->
  {#if !isAtBottom && totalCount > 0}
    <button
      class="scroll-bottom-btn"
      onclick={() => {
        containerEl.scrollTop = containerEl.scrollHeight;
        isAtBottom = true;
        unreadCount = 0;
      }}
      aria-label={unreadCount > 0 ? `新着${unreadCount}件。最新コメントへ` : '最新コメントへ'}
    >
      {#if unreadCount > 0}
        ▼ 新着 {unreadCount}件
      {:else}
        ▼ 最新
      {/if}
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

  .measure-row {
    margin: 0;
    border: 0;
    padding: 0;
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
