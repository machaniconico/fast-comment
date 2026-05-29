<script lang="ts">
  import { ui } from '../ui.svelte';
  import { store, clearMessages } from '../stores.svelte';

  // ── Command definitions ──────────────────────────────────────────────────

  interface Command {
    id: string;
    title: string;
    keywords: string[];
    icon: string;
    run: () => void;
  }

  const COMMANDS: Command[] = [
    {
      id: 'filter-all',
      title: 'フィルタ: ALL',
      keywords: ['all', 'filter', 'フィルタ', 'すべて'],
      icon: '🔍',
      run: () => { store.setFilterPlatform('all'); ui.setTab('comments'); },
    },
    {
      id: 'filter-twitch',
      title: 'フィルタ: Twitch',
      keywords: ['twitch', 'filter', 'フィルタ'],
      icon: '🟣',
      run: () => { store.setFilterPlatform('twitch'); ui.setTab('comments'); },
    },
    {
      id: 'filter-youtube',
      title: 'フィルタ: YouTube',
      keywords: ['youtube', 'yt', 'filter', 'フィルタ', 'ユーチューブ'],
      icon: '🔴',
      run: () => { store.setFilterPlatform('youtube'); ui.setTab('comments'); },
    },
    {
      id: 'clear',
      title: '一覧をクリア',
      keywords: ['clear', 'クリア', 'delete', '削除', 'reset'],
      icon: '🗑',
      run: () => { clearMessages(); },
    },
    {
      id: 'tab-comments',
      title: 'コメントタブを開く',
      keywords: ['comments', 'コメント', 'chat', 'tab'],
      icon: '💬',
      run: () => { ui.setTab('comments'); },
    },
    {
      id: 'tab-settings',
      title: '設定を開く',
      keywords: ['settings', '設定', 'config', 'setting'],
      icon: '⚙️',
      run: () => { ui.setTab('settings'); },
    },
    {
      id: 'goto-channels',
      title: 'チャンネル設定へ',
      keywords: ['channel', 'チャンネル', 'setting', '設定', 'channels'],
      icon: '⚙️',
      run: () => { ui.gotoSetting('channels'); },
    },
    {
      id: 'goto-tts',
      title: 'TTS(読み上げ)設定へ',
      keywords: ['tts', '読み上げ', 'voicevox', 'voice', 'speech', 'setting', '設定'],
      icon: '🔊',
      run: () => { ui.gotoSetting('tts'); },
    },
    {
      id: 'goto-obs',
      title: 'OBSオーバーレイ設定へ',
      keywords: ['obs', 'overlay', 'オーバーレイ', 'setting', '設定'],
      icon: '📺',
      run: () => { ui.gotoSetting('obs'); },
    },
    {
      id: 'goto-moderation',
      title: 'モデレーション(NG/ハイライト)設定へ',
      keywords: ['moderation', 'ng', 'highlight', 'ハイライト', 'モデレーション', 'ban', 'setting', '設定'],
      icon: '🛡',
      run: () => { ui.gotoSetting('moderation'); },
    },
  ];

  // ── Local state ──────────────────────────────────────────────────────────

  let query: string = $state('');
  let selectedIndex: number = $state(0);
  let inputEl: HTMLInputElement | null = $state(null);

  // ── Filtered commands ────────────────────────────────────────────────────

  const filteredCommands: Command[] = $derived.by(() => {
    const q = query.trim().toLowerCase();
    if (q === '') return COMMANDS;
    return COMMANDS.filter((cmd) => {
      const haystack = (cmd.title + ' ' + cmd.keywords.join(' ')).toLowerCase();
      return haystack.includes(q);
    });
  });

  // Dynamic search fallback row — shown only when query is non-empty
  const showSearchFallback: boolean = $derived(query.trim().length > 0);

  // Total navigable rows
  const totalRows: number = $derived(filteredCommands.length + (showSearchFallback ? 1 : 0));

  // Clamp selectedIndex when filtered list length changes
  $effect(() => {
    const max = totalRows - 1;
    if (selectedIndex > max) selectedIndex = 0;
  });

  // Autofocus and reset on open
  $effect(() => {
    if (ui.paletteOpen) {
      query = '';
      selectedIndex = 0;
      requestAnimationFrame(() => { inputEl?.focus(); });
    }
  });

  // ── Helpers ───────────────────────────────────────────────────────────────

  function runCommand(cmd: Command) {
    cmd.run();
    ui.closePalette();
  }

  function runSearchFallback() {
    store.setSearchQuery(query.trim());
    ui.setTab('comments');
    ui.closePalette();
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      ui.closePalette();
      return;
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      selectedIndex = (selectedIndex + 1) % totalRows;
      return;
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      selectedIndex = (selectedIndex - 1 + totalRows) % totalRows;
      return;
    }
    if (e.key === 'Enter') {
      e.preventDefault();
      const fallbackIdx = filteredCommands.length;
      if (showSearchFallback && selectedIndex === fallbackIdx) {
        runSearchFallback();
      } else if (filteredCommands[selectedIndex]) {
        runCommand(filteredCommands[selectedIndex]);
      }
    }
  }
</script>

{#if ui.paletteOpen}
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="palette-overlay"
    onclick={() => ui.closePalette()}
    aria-hidden="true"
  >
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div
      class="palette-panel"
      role="dialog"
      aria-modal="true"
      aria-label="コマンドパレット"
      onclick={(e) => e.stopPropagation()}
    >
      <input
        bind:this={inputEl}
        class="palette-input"
        type="text"
        placeholder="コマンドを検索..."
        aria-label="コマンド検索"
        autocomplete="off"
        spellcheck="false"
        bind:value={query}
        onkeydown={onKeydown}
      />

      <ul class="palette-list" role="listbox" aria-label="コマンド一覧">
        {#each filteredCommands as cmd, i (cmd.id)}
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <li
            class="palette-item"
            class:selected={selectedIndex === i}
            role="option"
            aria-selected={selectedIndex === i}
            onclick={() => runCommand(cmd)}
            onmousemove={() => { selectedIndex = i; }}
          >
            <span class="item-icon" aria-hidden="true">{cmd.icon}</span>
            <span class="item-title">{cmd.title}</span>
          </li>
        {/each}

        {#if showSearchFallback}
          {@const fallbackIdx = filteredCommands.length}
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <li
            class="palette-item palette-item--search"
            class:selected={selectedIndex === fallbackIdx}
            role="option"
            aria-selected={selectedIndex === fallbackIdx}
            onclick={runSearchFallback}
            onmousemove={() => { selectedIndex = fallbackIdx; }}
          >
            <span class="item-icon" aria-hidden="true">🔍</span>
            <span class="item-title">「{query.trim()}」でコメント検索</span>
          </li>
        {/if}

        {#if filteredCommands.length === 0 && !showSearchFallback}
          <li class="palette-empty" role="option" aria-selected="false">一致するコマンドなし</li>
        {/if}
      </ul>
    </div>
  </div>
{/if}

<style>
  .palette-overlay {
    position: fixed;
    inset: 0;
    z-index: 1000;
    background: rgba(0, 0, 0, 0.55);
    display: flex;
    justify-content: center;
    align-items: flex-start;
    padding-top: 80px;
  }

  .palette-panel {
    width: 480px;
    max-width: calc(100vw - 32px);
    background: #1e1e1e;
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.6);
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }

  .palette-input {
    width: 100%;
    background: transparent;
    border: none;
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
    color: #e0e0e0;
    font-size: 13px;
    font-family: inherit;
    padding: 12px 14px;
    outline: none;
    box-sizing: border-box;
  }

  .palette-input::placeholder {
    color: #555;
  }

  .palette-list {
    list-style: none;
    margin: 0;
    padding: 4px 0;
    max-height: 320px;
    overflow-y: auto;
  }

  .palette-item {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 7px 14px;
    cursor: pointer;
    color: #bdbdbd;
    font-size: 13px;
    transition: background 0.1s, color 0.1s;
    user-select: none;
  }

  .palette-item:hover {
    background: rgba(255, 255, 255, 0.06);
    color: #e0e0e0;
  }

  .palette-item.selected {
    background: rgba(255, 255, 255, 0.12);
    color: #fff;
  }

  .palette-item--search {
    color: #9e9e9e;
    font-style: italic;
    border-top: 1px solid rgba(255, 255, 255, 0.06);
    margin-top: 4px;
    padding-top: 9px;
  }

  .palette-item--search.selected {
    color: #fff;
    font-style: normal;
  }

  .item-icon {
    font-size: 14px;
    width: 20px;
    text-align: center;
    flex-shrink: 0;
  }

  .item-title {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .palette-empty {
    padding: 12px 14px;
    color: #555;
    font-size: 12px;
    text-align: center;
  }
</style>
