<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { Platform } from './lib/types';
  import CommentList from './lib/components/CommentList.svelte';
  import PinnedStrip from './lib/components/PinnedStrip.svelte';
  import Settings from './lib/components/Settings.svelte';
  import ChannelAdd from './lib/components/ChannelAdd.svelte';
  import CommandPalette from './lib/components/CommandPalette.svelte';
  import Notifier from './lib/components/Notifier.svelte';
  import { store, initStore, clearMessages } from './lib/stores.svelte';
  import { ui } from './lib/ui.svelte';

  let unlisten: (() => void) | null = null;

  // ── Donation summary helpers ──────────────────────────────────────────────

  const CURRENCY_SYMBOL: Record<string, string> = {
    JPY: '¥', USD: '$', EUR: '€', GBP: '£',
  };

  function formatDonationAmount(currency: string, total: number): string {
    if (currency.toLowerCase() === 'bits') {
      return `${total.toLocaleString('ja-JP')} bits`;
    }
    const sym = CURRENCY_SYMBOL[currency] ?? currency + ' ';
    return `${sym}${new Intl.NumberFormat('ja-JP').format(total)}`;
  }

  /** Entries to render: only currencies with count > 0. */
  const donationEntries = $derived(
    Object.entries(store.donationSummary.byCurrency).filter(([, t]) => t.count > 0)
  );

  const hasDonations = $derived(
    donationEntries.length > 0 || store.donationSummary.memberships > 0
  );

  onMount(async () => {
    unlisten = await initStore();
  });

  onDestroy(() => {
    unlisten?.();
    if (searchDebounce) clearTimeout(searchDebounce);
  });

  function onFilterClick(p: Platform | 'all') {
    store.setFilterPlatform(p);
  }

  let searchDebounce: ReturnType<typeof setTimeout> | null = null;

  function onSearchInput(e: Event) {
    const val = (e.target as HTMLInputElement).value;
    // Update store directly — store.searchQuery is $state so this is reactive
    if (searchDebounce) clearTimeout(searchDebounce);
    searchDebounce = setTimeout(() => store.setSearchQuery(val), 120);
  }

  function onWindowKey(e: KeyboardEvent) {
    if (e.key === 'k' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      ui.togglePalette();
    }
  }
</script>

<svelte:window onkeydown={onWindowKey} />

<div class="app">
  <!-- ── Header ── -->
  <header class="app-header">
    <div class="header-left">
      <span class="app-title">fast-comment</span>
      <span class="msg-count">{store.totalCount}</span>
      {#if hasDonations}
        <div class="donation-summary">
          {#each donationEntries as [currency, tally]}
            <span class="donation-badge">
              💰 {formatDonationAmount(currency, tally.total)} ({tally.count})
            </span>
          {/each}
          {#if store.donationSummary.memberships > 0}
            <span class="donation-badge donation-badge--member">
              👑 {store.donationSummary.memberships}
            </span>
          {/if}
        </div>
      {/if}
    </div>

    <!-- Tab switcher -->
    <div class="tabs" role="tablist">
      <button
        role="tab"
        class="tab-btn"
        class:active={ui.activeTab === 'comments'}
        aria-selected={ui.activeTab === 'comments'}
        onclick={() => ui.setTab('comments')}
      >コメント</button>
      <button
        role="tab"
        class="tab-btn"
        class:active={ui.activeTab === 'settings'}
        aria-selected={ui.activeTab === 'settings'}
        onclick={() => ui.setTab('settings')}
      >設定</button>
    </div>
  </header>

  <!-- ── Channel add bar (URL paste → auto-detect) ── -->
  {#if ui.activeTab === 'comments'}
    <div class="channel-bar">
      <ChannelAdd />
    </div>
  {/if}

  <!-- ── Comment tab toolbar ── -->
  {#if ui.activeTab === 'comments'}
    <div class="toolbar">
      <!-- Platform filter -->
      <div class="filter-group" role="group" aria-label="プラットフォームフィルタ">
        <button
          class="filter-btn"
          class:active={store.filterPlatform === 'all'}
          onclick={() => onFilterClick('all')}
        >ALL</button>
        <button
          class="filter-btn twitch"
          class:active={store.filterPlatform === 'twitch'}
          onclick={() => onFilterClick('twitch')}
        >Twitch</button>
        <button
          class="filter-btn youtube"
          class:active={store.filterPlatform === 'youtube'}
          onclick={() => onFilterClick('youtube')}
        >YouTube</button>
      </div>

      <!-- Search -->
      <input
        type="search"
        class="search-input"
        placeholder="検索..."
        value={store.searchQuery}
        oninput={onSearchInput}
        aria-label="コメント検索"
      />
      {#if store.searchQuery.trim() !== ''}
        <span class="search-count" aria-live="polite">{store.visibleMessages.length}件</span>
      {/if}

      <!-- Clear -->
      <button class="clear-btn" onclick={clearMessages} title="一覧をクリア" aria-label="コメントをクリア">
        ✕
      </button>
    </div>
  {/if}

  <!-- ── Main content ── -->
  <div class="main-content" role="tabpanel">
    {#if ui.activeTab === 'comments'}
      <PinnedStrip />
      <CommentList />
    {:else}
      <Settings />
    {/if}
  </div>

  <CommandPalette />
  <Notifier />
</div>

<style>
  :global(*, *::before, *::after) {
    box-sizing: border-box;
  }

  :global(body) {
    margin: 0;
    padding: 0;
    background: #121212;
    color: #e0e0e0;
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
    font-size: 13px;
    overflow: hidden;
    height: 100vh;
  }

  :global(#app) {
    height: 100vh;
    display: flex;
    flex-direction: column;
  }

  .app {
    display: flex;
    flex-direction: column;
    height: 100vh;
    overflow: hidden;
  }

  /* Header */
  .app-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0 10px;
    height: 36px;
    background: #1a1a1a;
    border-bottom: 1px solid rgba(255,255,255,0.08);
    flex-shrink: 0;
    gap: 8px;
  }

  .header-left {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .app-title {
    font-weight: 700;
    font-size: 13px;
    color: #fff;
    letter-spacing: 0.02em;
  }

  .msg-count {
    background: rgba(255,255,255,0.12);
    color: #9e9e9e;
    font-size: 11px;
    padding: 1px 6px;
    border-radius: 10px;
    min-width: 24px;
    text-align: center;
  }

  .tabs {
    display: flex;
    gap: 2px;
  }

  .tab-btn {
    background: none;
    border: none;
    color: #757575;
    padding: 5px 10px;
    font-size: 12px;
    cursor: pointer;
    border-radius: 4px;
    transition: color 0.15s, background 0.15s;
  }

  .tab-btn.active {
    color: #fff;
    background: rgba(255,255,255,0.1);
  }

  .tab-btn:hover:not(.active) {
    color: #bbb;
    background: rgba(255,255,255,0.05);
  }

  /* Channel add bar */
  .channel-bar {
    padding: 6px 8px;
    background: #1d1d1d;
    border-bottom: 1px solid rgba(255,255,255,0.06);
    flex-shrink: 0;
  }

  /* Toolbar */
  .toolbar {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 8px;
    background: #181818;
    border-bottom: 1px solid rgba(255,255,255,0.06);
    flex-shrink: 0;
  }

  .filter-group {
    display: flex;
    gap: 2px;
    flex-shrink: 0;
  }

  .filter-btn {
    background: rgba(255,255,255,0.06);
    border: 1px solid rgba(255,255,255,0.1);
    color: #9e9e9e;
    padding: 3px 8px;
    font-size: 11px;
    font-weight: 600;
    border-radius: 12px;
    cursor: pointer;
    transition: all 0.15s;
    white-space: nowrap;
  }

  .filter-btn.active {
    color: #fff;
    background: rgba(255,255,255,0.16);
    border-color: rgba(255,255,255,0.25);
  }

  .filter-btn.twitch.active { background: rgba(145,70,255,0.3); border-color: #9146ff; color: #d4aaff; }
  .filter-btn.youtube.active { background: rgba(255,0,0,0.2); border-color: #ff4444; color: #ff9999; }

  .search-input {
    flex: 1;
    background: rgba(255,255,255,0.06);
    border: 1px solid rgba(255,255,255,0.1);
    border-radius: 4px;
    color: #e0e0e0;
    padding: 3px 8px;
    font-size: 12px;
    min-width: 0;
  }

  .search-input::placeholder { color: #555; }
  .search-input:focus { outline: none; border-color: rgba(255,255,255,0.25); }

  .search-count {
    background: rgba(255,255,255,0.12);
    color: #9e9e9e;
    font-size: 11px;
    padding: 1px 6px;
    border-radius: 10px;
    white-space: nowrap;
    flex-shrink: 0;
  }

  .clear-btn {
    background: none;
    border: none;
    color: #555;
    font-size: 13px;
    cursor: pointer;
    padding: 3px 6px;
    border-radius: 3px;
    flex-shrink: 0;
    transition: color 0.15s;
  }

  .clear-btn:hover { color: #f44336; }

  /* Main content */
  .main-content {
    flex: 1;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }

  /* Donation summary badges in header */
  .donation-summary {
    display: flex;
    align-items: center;
    gap: 4px;
    overflow: hidden;
    flex-shrink: 1;
    min-width: 0;
  }

  .donation-badge {
    background: rgba(255,255,255,0.08);
    color: #ffd600;
    font-size: 11px;
    padding: 1px 6px;
    border-radius: 10px;
    white-space: nowrap;
    flex-shrink: 0;
  }

  .donation-badge--member {
    color: #9e9e9e;
  }
</style>
