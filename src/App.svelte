<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import type { Platform } from './lib/types';
  import CommentList from './lib/components/CommentList.svelte';
  import Settings from './lib/components/Settings.svelte';
  import { store, initStore, clearMessages } from './lib/stores.svelte';

  type Tab = 'comments' | 'settings';
  let activeTab: Tab = $state('comments');

  let unlisten: (() => void) | null = null;

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
</script>

<div class="app">
  <!-- ── Header ── -->
  <header class="app-header">
    <div class="header-left">
      <span class="app-title">fast-comment</span>
      <span class="msg-count">{store.totalCount}</span>
    </div>

    <!-- Tab switcher -->
    <nav class="tabs" role="tablist">
      <button
        role="tab"
        class="tab-btn"
        class:active={activeTab === 'comments'}
        aria-selected={activeTab === 'comments'}
        onclick={() => activeTab = 'comments'}
      >コメント</button>
      <button
        role="tab"
        class="tab-btn"
        class:active={activeTab === 'settings'}
        aria-selected={activeTab === 'settings'}
        onclick={() => activeTab = 'settings'}
      >設定</button>
    </nav>
  </header>

  <!-- ── Comment tab toolbar ── -->
  {#if activeTab === 'comments'}
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

      <!-- Clear -->
      <button class="clear-btn" onclick={clearMessages} title="一覧をクリア" aria-label="コメントをクリア">
        ✕
      </button>
    </div>
  {/if}

  <!-- ── Main content ── -->
  <main class="main-content" role="tabpanel">
    {#if activeTab === 'comments'}
      <CommentList />
    {:else}
      <Settings />
    {/if}
  </main>
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
</style>
