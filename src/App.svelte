<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { TauriEvent } from '@tauri-apps/api/event';
  import type { Platform } from './lib/types';
  import CommentList from './lib/components/CommentList.svelte';
  import Dashboard from './lib/components/Dashboard.svelte';
  import Raffle from './lib/components/Raffle.svelte';
  import Timer from './lib/components/Timer.svelte';
  import MultiColumnView from './lib/components/MultiColumnView.svelte';
  import DonationPanel from './lib/components/DonationPanel.svelte';
  import Effects from './lib/components/Effects.svelte';
  import GoalsBar from './lib/components/GoalsBar.svelte';
  import Sparkline from './lib/components/Sparkline.svelte';
  import PinnedStrip from './lib/components/PinnedStrip.svelte';
  import Participation from './lib/components/Participation.svelte';
  import Milestone from './lib/components/Milestone.svelte';
  import Welcome from './lib/components/Welcome.svelte';
  import Settings from './lib/components/Settings.svelte';
  import ChannelAdd from './lib/components/ChannelAdd.svelte';
  import CommandPalette from './lib/components/CommandPalette.svelte';
  import CommentComposer from './lib/components/CommentComposer.svelte';
  import ShortcutHelp from './lib/components/ShortcutHelp.svelte';
  import Notifier from './lib/components/Notifier.svelte';
  import TtsQueuePanel from './lib/components/TtsQueuePanel.svelte';
  import { store, initStore, clearMessages } from './lib/stores.svelte';
  import { ui } from './lib/ui.svelte';
  import { theme } from './lib/theme.svelte';
  import {
    checkForUpdate,
    openReleaseUrl,
    getConfig,
    setConfig,
    getTtsQueueState,
    onTtsNotice,
    onTtsQueueState,
    setTtsPaused,
    toggleDanmakuOverlay,
    isDanmakuOverlayOpen,
    setAlwaysOnTop,
  } from './lib/ipc';
  import type { AppConfig, TtsNotice, UpdateStatus } from './lib/ipc';

  let unlisten: (() => void) | null = null;
  let unlistenTtsNotice: (() => void) | null = null;
  let unlistenTtsQueueState: (() => void) | null = null;
  let config: AppConfig | null = $state(null);
  let updateStatus = $state<UpdateStatus | null>(null);
  let updateDismissed = $state(false);
  let ttsNotice = $state<TtsNotice | null>(null);
  let ttsNoticeTimer: ReturnType<typeof setTimeout> | null = null;
  let toolsOpen = $state(false);
  let toolsMenuEl: HTMLDivElement | null = null;
  let danmakuOpen = $state(false);
  let goalsToggleSaving = $state(false);
  let goalsToggleError = $state('');
  let ttsPaused = $state(false);
  let ttsToggleBusy = $state(false);
  let unlistenDanmakuState: (() => void) | null = null;
  let destroyed = false;
  const DANMAKU_LABEL = 'danmaku';

  function isDonationPanelEnabled(cfg: AppConfig | null): boolean {
    return cfg?.ui.showDonationPanel === true;
  }

  function isGoalsBarVisible(cfg: AppConfig | null): boolean {
    return cfg?.goals?.showInApp === true;
  }

  function isGoalsFeatureEnabled(cfg: AppConfig | null): boolean {
    return cfg?.goals?.enabled === true;
  }

  function isEffectsEnabled(cfg: AppConfig | null): boolean {
    return cfg?.effects?.enabled === true;
  }

  function isWelcomeEnabled(cfg: AppConfig | null): boolean {
    return cfg?.welcome?.enabled === true;
  }

  function isTtsConfigured(cfg: AppConfig | null): boolean {
    return cfg?.tts?.backend !== undefined && cfg.tts.backend !== 'none';
  }

  const showDonationPanel = $derived(isDonationPanelEnabled(config));
  const showGoalsBar = $derived(isGoalsBarVisible(config));
  const goalsFeatureEnabled = $derived(isGoalsFeatureEnabled(config));
  const showEffects = $derived(isEffectsEnabled(config));
  const showWelcome = $derived(isWelcomeEnabled(config));
  const ttsConfigured = $derived(isTtsConfigured(config));
  const standaloneOpen = $derived(
    ui.showDashboard || ui.showRaffle || ui.showTimer || ui.toolSettingsView !== null
  );
  const showGoalsControls = $derived(
    ui.activeTab === 'comments'
      && !standaloneOpen
      && (goalsFeatureEnabled || showGoalsBar)
  );

  $effect(() => {
    if (!showDonationPanel && ui.activeTab === 'donations') ui.setTab('comments');
  });

  onMount(async () => {
    theme.load();
    void setAlwaysOnTop(ui.alwaysOnTop);
    window.addEventListener('click', onWindowClick);
    window.addEventListener('keydown', onWindowKey);
    void loadUpdateStatus();
    void loadConfig();
    {
      const fn = await initStore();
      if (destroyed) fn();
      else unlisten = fn;
    }
    {
      const fn = await onTtsNotice(showTtsNotice);
      if (destroyed) fn();
      else unlistenTtsNotice = fn;
    }
    try {
      const current = await getTtsQueueState();
      if (!destroyed && current) ttsPaused = current.paused === true;
      const fn = await onTtsQueueState((next) => {
        if (!destroyed) ttsPaused = next.paused === true;
      });
      if (destroyed) fn();
      else unlistenTtsQueueState = fn;
    } catch (e) {
      console.warn('[tts] queue state load failed', e);
    }
    const initialDanmakuOpen = await isDanmakuOverlayOpen();
    if (!destroyed) danmakuOpen = initialDanmakuOpen;
    if (initialDanmakuOpen) {
      const fn = await listenDanmakuDestroyed();
      if (fn) {
        if (destroyed) fn();
        else unlistenDanmakuState = fn;
      }
    }
  });

  onDestroy(() => {
    destroyed = true;
    theme.destroy();
    window.removeEventListener('click', onWindowClick);
    window.removeEventListener('keydown', onWindowKey);
    unlisten?.();
    unlistenTtsNotice?.();
    unlistenTtsQueueState?.();
    unlistenDanmakuState?.();
    if (searchDebounce) clearTimeout(searchDebounce);
    if (ttsNoticeTimer) clearTimeout(ttsNoticeTimer);
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

  function setSearchMode(mode: 'text' | 'regex') {
    store.setSearchMode(mode);
  }

  function onWindowKey(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      if (ui.showShortcuts) {
        ui.closeShortcuts();
        return;
      }
      if (toolsOpen) {
        closeToolsMenu();
        return;
      }
    }
    if (e.key === 'k' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      ui.togglePalette();
      return;
    }
    if (e.key === '?' && !isEditableKeyTarget(e.target)) {
      e.preventDefault();
      if (!ui.showShortcuts) closeToolsMenu();
      ui.toggleShortcuts();
    }
  }

  function isEditableKeyTarget(target: EventTarget | null): boolean {
    if (!(target instanceof Element)) return false;
    const tagName = target.tagName.toLowerCase();
    return (
      tagName === 'input' ||
      tagName === 'textarea' ||
      (target instanceof HTMLElement && target.isContentEditable) ||
      target.closest('[contenteditable="true"]') !== null
    );
  }

  function onWindowClick(e: MouseEvent) {
    if (!toolsOpen) return;
    const target = e.target;
    if (target instanceof Node && toolsMenuEl?.contains(target)) return;
    closeToolsMenu();
  }

  function toggleToolsMenu(e: MouseEvent) {
    e.stopPropagation();
    toolsOpen = !toolsOpen;
  }

  function closeToolsMenu() {
    toolsOpen = false;
  }

  function selectTimer() {
    ui.toggleTimer();
    closeToolsMenu();
  }

  function selectRaffle() {
    ui.toggleRaffle();
    closeToolsMenu();
  }

  function selectDashboard() {
    ui.toggleDashboard();
    closeToolsMenu();
  }

  async function selectDanmaku() {
    closeToolsMenu();
    try {
      const nextOpen = await toggleDanmakuOverlay();
      danmakuOpen = nextOpen;
      unlistenDanmakuState?.();
      unlistenDanmakuState = null;
      if (nextOpen) {
        const fn = await listenDanmakuDestroyed();
        if (destroyed) fn?.();
        else unlistenDanmakuState = fn;
      }
    } catch (e) {
      console.warn('[danmaku] toggle failed', e);
    }
  }

  function selectDanmakuSettings() {
    ui.openToolSettings('danmaku');
    closeToolsMenu();
  }

  function isTauri(): boolean {
    return typeof window !== 'undefined'
      && !!(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
  }

  async function listenDanmakuDestroyed(): Promise<(() => void) | null> {
    if (!isTauri()) return null;
    const { WebviewWindow } = await import('@tauri-apps/api/webviewWindow');
    const overlay = await WebviewWindow.getByLabel(DANMAKU_LABEL);
    if (!overlay) return null;
    return overlay.listen<null>(TauriEvent.WINDOW_DESTROYED, () => {
      if (destroyed) return;
      danmakuOpen = false;
      unlistenDanmakuState?.();
      unlistenDanmakuState = null;
    });
  }

  function selectParticipation() {
    ui.setTab('participation');
    closeToolsMenu();
  }

  function selectGoalsSettings() {
    ui.openToolSettings('goals');
    closeToolsMenu();
  }

  function toggleAlwaysOnTop() {
    const next = !ui.alwaysOnTop;
    ui.setAlwaysOnTop(next);
    void setAlwaysOnTop(next);
  }

  async function toggleTtsReading() {
    if (!ttsConfigured || ttsToggleBusy) return;
    const previous = ttsPaused;
    const next = !previous;
    ttsPaused = next;
    ttsToggleBusy = true;
    try {
      await setTtsPaused(next);
    } catch (e) {
      ttsPaused = previous;
      showTtsNotice({
        level: 'error',
        message: `読み上げの切り替えに失敗しました: ${e instanceof Error ? e.message : String(e)}`,
      });
    } finally {
      ttsToggleBusy = false;
    }
  }

  async function loadUpdateStatus() {
    try {
      const status = await checkForUpdate();
      if (status?.updateAvailable) {
        updateStatus = status;
        updateDismissed = false;
      }
    } catch (e) {
      console.warn('[update] update check failed', e);
    }
  }

  async function loadConfig() {
    try {
      const loaded = await getConfig();
      if (loaded) config = loaded;
    } catch (e) {
      console.warn('[config] load failed', e);
    }
  }

  function showTtsNotice(notice: TtsNotice) {
    ttsNotice = notice;
    if (ttsNoticeTimer) clearTimeout(ttsNoticeTimer);
    ttsNoticeTimer = setTimeout(() => {
      ttsNotice = null;
      ttsNoticeTimer = null;
    }, 8000);
  }

  function dismissTtsNotice() {
    ttsNotice = null;
    if (ttsNoticeTimer) {
      clearTimeout(ttsNoticeTimer);
      ttsNoticeTimer = null;
    }
  }

  function onSettingsSaved(nextConfig: AppConfig) {
    config = structuredClone($state.snapshot(nextConfig));
  }

  async function toggleGoalsInApp(event: Event) {
    if (!config || goalsToggleSaving) return;
    const previous = $state.snapshot(config);
    const next = structuredClone(previous);
    next.goals.showInApp = (event.currentTarget as HTMLInputElement).checked;
    config = next;
    goalsToggleSaving = true;
    goalsToggleError = '';
    try {
      await setConfig(next);
    } catch (e) {
      config = previous;
      goalsToggleError = e instanceof Error ? e.message : String(e);
    } finally {
      goalsToggleSaving = false;
    }
  }

  async function onUpdateDownloadClick(e: MouseEvent) {
    e.preventDefault();
    const url = updateStatus?.releaseUrl;
    if (!url) return;
    try {
      await openReleaseUrl(url);
    } catch (err) {
      console.warn('[update] failed to open release URL', err);
    }
  }
</script>

{#if showEffects && config?.effects}
  <Effects config={config.effects} />
{/if}

{#if showWelcome && config?.welcome}
  <Welcome config={config.welcome} />
{/if}

{#if theme.showCommentMilestones}
  <Milestone />
{/if}

<div
  class="app"
  data-theme={theme.resolved}
  data-font-size={theme.fontSize}
  data-density={theme.density}
  data-youtube-member-name-green={config?.ui.youtubeMemberNameGreen !== false}
  data-twitch-native-style={config?.ui.twitchNativeStyle !== false}
>
  {#if updateStatus?.updateAvailable && !updateDismissed}
    <div class="update-banner" role="status" aria-live="polite">
      <span class="update-banner__text">
        新しいバージョン v{updateStatus.latestVersion} があります（現在 v{updateStatus.currentVersion}）
      </span>
      <a class="update-download" href={updateStatus.releaseUrl || '#'} onclick={onUpdateDownloadClick}>
        ダウンロード
      </a>
      <button
        class="update-dismiss"
        onclick={() => (updateDismissed = true)}
        title="閉じる"
        aria-label="更新通知を閉じる"
      >×</button>
    </div>
  {/if}

  {#if ttsNotice}
    <div
      class={`tts-notice-banner${ttsNotice.level === 'info' ? ' tts-notice-banner--info' : ''}${ttsNotice.level === 'warn' || ttsNotice.level === 'error' ? ' tts-notice-banner--error' : ''}`}
      role="status"
      aria-live="polite"
    >
      <span class="tts-notice-banner__text">{ttsNotice.message}</span>
      <button
        class="tts-notice-dismiss"
        onclick={dismissTtsNotice}
        title="閉じる"
        aria-label="TTS通知を閉じる"
      >×</button>
    </div>
  {/if}

  <!-- ── Header ── -->
  <header class="app-header">
    <div class="header-left">
      <span class="msg-count">{store.totalCount}</span>
      {#if store.allMessages.length > 0}
        <Sparkline />
      {/if}
    </div>

    <div class="header-actions">
      <button
        class="tts-toggle-btn"
        class:enabled={ttsConfigured && !ttsPaused}
        class:muted={ttsPaused}
        disabled={!ttsConfigured || ttsToggleBusy}
        title={!ttsConfigured ? '読み上げ方式が設定されていません' : ttsPaused ? '音声読み上げをON' : '音声読み上げをOFF'}
        aria-label={!ttsConfigured ? '音声読み上げは設定で無効です' : ttsPaused ? '音声読み上げをON' : '音声読み上げをOFF'}
        aria-pressed={ttsConfigured && !ttsPaused}
        onclick={toggleTtsReading}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M4 9v6h4l5 4V5L8 9H4Z" />
          {#if ttsPaused || !ttsConfigured}
            <path class="speaker-slash" d="m17 9 5 5m0-5-5 5" />
          {:else}
            <path class="speaker-waves" d="M16 9.5a4 4 0 0 1 0 5M18.5 7a7.5 7.5 0 0 1 0 10" />
          {/if}
        </svg>
      </button>
      <button
        class="window-pin-btn"
        class:active={ui.alwaysOnTop}
        title={ui.alwaysOnTop ? '常に最前面を解除' : '常に最前面'}
        aria-label={ui.alwaysOnTop ? '常に最前面を解除' : '常に最前面'}
        aria-pressed={ui.alwaysOnTop}
        onclick={toggleAlwaysOnTop}
      >📌</button>
      <!-- Tab switcher -->
      <div class="tabs tabs-primary" role="tablist" aria-label="メイン表示">
        <button
          role="tab"
          class="tab-btn"
          class:active={ui.activeTab === 'comments' && !standaloneOpen}
          aria-selected={ui.activeTab === 'comments' && !standaloneOpen}
          onclick={() => ui.setTab('comments')}
        >コメント</button>
        {#if showDonationPanel}
          <button
            role="tab"
            class="tab-btn"
            class:active={ui.activeTab === 'donations' && !standaloneOpen}
            aria-selected={ui.activeTab === 'donations' && !standaloneOpen}
            onclick={() => ui.setTab('donations')}
          >投げ銭</button>
        {/if}
      </div>
      <div class="tools-menu" bind:this={toolsMenuEl}>
        <button
          class="tab-btn tools-trigger"
          class:active={standaloneOpen || ui.activeTab === 'participation'}
          aria-haspopup="menu"
          aria-expanded={toolsOpen}
          onclick={toggleToolsMenu}
        >ツール ▾</button>
        {#if toolsOpen}
          <div class="tools-dropdown" role="menu" aria-label="ツール">
            <button
              role="menuitem"
              class="tools-menu-item"
              class:active={ui.activeTab === 'participation' && !standaloneOpen}
              onclick={selectParticipation}
            >参加</button>
            <button
              role="menuitem"
              class="tools-menu-item"
              class:active={ui.showTimer}
              onclick={selectTimer}
            >タイマー</button>
            <button
              role="menuitem"
              class="tools-menu-item"
              class:active={ui.showRaffle}
              onclick={selectRaffle}
            >抽選</button>
            <button
              role="menuitem"
              class="tools-menu-item"
              class:active={ui.showDashboard}
              onclick={selectDashboard}
            >振り返り</button>
            <button
              role="menuitem"
              class="tools-menu-item"
              class:active={danmakuOpen}
              onclick={selectDanmaku}
            >弾幕オーバーレイ{danmakuOpen ? '（表示中）' : ''}</button>
            <button
              role="menuitem"
              class="tools-menu-item"
              class:active={ui.toolSettingsView === 'danmaku'}
              onclick={selectDanmakuSettings}
            >弾幕・OBS設定</button>
            <button
              role="menuitem"
              class="tools-menu-item"
              class:active={ui.toolSettingsView === 'goals'}
              onclick={selectGoalsSettings}
            >目標設定</button>
          </div>
        {/if}
      </div>
      <div class="tabs tabs-settings" role="tablist" aria-label="設定">
        <button
          role="tab"
          class="tab-btn"
          class:active={ui.activeTab === 'settings' && !standaloneOpen}
          aria-selected={ui.activeTab === 'settings' && !standaloneOpen}
          onclick={() => ui.setTab('settings')}
        >設定</button>
      </div>
    </div>
  </header>

  <!-- ── Channel add bar (URL paste → auto-detect) ── -->
  {#if ui.activeTab === 'comments' && !standaloneOpen}
    <div class="channel-bar">
      <ChannelAdd />
    </div>
  {/if}

  <!-- ── Comment tab toolbar ── -->
  {#if ui.activeTab === 'comments' && !standaloneOpen}
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
        <button
          class="filter-btn x"
          class:active={store.filterPlatform === 'x'}
          onclick={() => onFilterClick('x')}
        >X</button>
        <button
          class="filter-btn niconico"
          class:active={store.filterPlatform === 'niconico'}
          onclick={() => onFilterClick('niconico')}
        >ニコ生</button>
      </div>

      <!-- View mode -->
      <div class="view-mode-group" role="group" aria-label="コメント表示モード">
        <button
          class="view-mode-btn"
          class:active={ui.viewMode === 'unified'}
          aria-pressed={ui.viewMode === 'unified'}
          onclick={() => ui.setViewMode('unified')}
        >統合</button>
        <button
          class="view-mode-btn"
          class:active={ui.viewMode === 'columns'}
          aria-pressed={ui.viewMode === 'columns'}
          onclick={() => ui.setViewMode('columns')}
        >カラム</button>
      </div>

      <!-- Search -->
      <input
        type="search"
        class="search-input"
        placeholder="検索..."
        value={store.searchQuery}
        oninput={onSearchInput}
        aria-label={store.searchMode === 'regex' ? 'コメント検索（正規表現）' : 'コメント検索'}
        aria-invalid={store.searchRegexInvalid}
      />
      <div class="search-mode-group" role="group" aria-label="検索モード">
        <button
          class="search-mode-btn"
          class:active={store.searchMode === 'text'}
          aria-pressed={store.searchMode === 'text'}
          aria-label="テキスト検索モード"
          onclick={() => setSearchMode('text')}
        >text</button>
        <button
          class="search-mode-btn"
          class:active={store.searchMode === 'regex'}
          aria-pressed={store.searchMode === 'regex'}
          aria-label="正規表現検索モード"
          onclick={() => setSearchMode('regex')}
        >.*</button>
      </div>
      {#if store.searchRegexInvalid}
        <span class="search-error" aria-live="polite">無効な正規表現</span>
      {/if}
      {#if store.searchQuery.trim() !== ''}
        <span class="search-count" aria-live="polite">{store.visibleMessages.length}件</span>
      {/if}

      <!-- Clear -->
      <button class="clear-btn" onclick={clearMessages} title="一覧をクリア" aria-label="コメントをクリア">
        ✕
      </button>
    </div>
    <TtsQueuePanel />
  {/if}

  <!-- ── Main content ── -->
  <div class="main-content" role="tabpanel">
    {#if ui.toolSettingsView === 'goals'}
      <Settings focus="goals" onConfigSaved={onSettingsSaved} />
    {:else if ui.toolSettingsView === 'danmaku'}
      <Settings focus="danmaku" onConfigSaved={onSettingsSaved} />
    {:else if ui.showTimer}
      <Timer />
    {:else if ui.showDashboard}
      <Dashboard />
    {:else if ui.showRaffle}
      <Raffle />
    {:else if ui.activeTab === 'comments'}
      <PinnedStrip />
      {#if ui.viewMode === 'columns'}
        <MultiColumnView />
      {:else}
        <CommentList />
      {/if}
    {:else if ui.activeTab === 'donations' && showDonationPanel}
      <DonationPanel />
    {:else if ui.activeTab === 'participation'}
      <Participation />
    {:else}
      <Settings onConfigSaved={onSettingsSaved} />
    {/if}
  </div>

  <!-- ── Self-post composer (toggle, pinned to the bottom in the comments tab) ── -->
  {#if ui.activeTab === 'comments' && !standaloneOpen}
    <div class="composer-toggle-bar">
      <button
        class="composer-toggle"
        class:active={ui.composerOpen}
        aria-expanded={ui.composerOpen}
        onclick={() => ui.toggleComposer()}
      >{ui.composerOpen ? '▼ コメント投稿を閉じる' : '✍ 自分でコメントを投稿'}</button>
    </div>
    {#if ui.composerOpen}
      <CommentComposer {config} />
    {/if}
  {/if}

  <!-- ── In-app goals (kept at the bottom; hidden by default via showInApp=false) ── -->
  {#if showGoalsControls}
    <section class="goals-panel" aria-label="アプリ内の目標表示">
      <div class="goals-panel-controls">
        <span class="goals-panel-label">目標</span>
        <label class="goals-panel-toggle">
          <input
            type="checkbox"
            checked={showGoalsBar}
            disabled={goalsToggleSaving}
            onchange={toggleGoalsInApp}
          />
          <span>{goalsToggleSaving ? '保存中' : 'コメビュに表示'}</span>
        </label>
        {#if goalsToggleError}
          <span class="goals-toggle-error" title={goalsToggleError}>保存失敗</span>
        {/if}
      </div>
      {#if showGoalsBar}
        <GoalsBar />
      {/if}
    </section>
  {/if}

  <CommandPalette />
  <ShortcutHelp />
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
    --fc-comment-font-size: 13px;
    --fc-comment-line-height: 1.4;
    --fc-comment-padding-x: 6px;
    display: flex;
    flex-direction: column;
    height: 100vh;
    overflow: hidden;
    background: #121212;
    color: #e0e0e0;
  }

  .app[data-theme='light'] {
    background: #f5f7fa;
    color: #20242a;
  }

  .app[data-font-size='s'] {
    --fc-comment-font-size: 12px;
  }

  .app[data-font-size='m'] {
    --fc-comment-font-size: 13px;
  }

  .app[data-font-size='l'] {
    --fc-comment-font-size: 15px;
  }

  .app[data-density='comfortable'] {
    --fc-comment-line-height: 1.4;
    --fc-comment-padding-x: 6px;
  }

  .app[data-density='compact'] {
    --fc-comment-line-height: 1.2;
    --fc-comment-padding-x: 4px;
  }

  .update-banner {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 32px;
    padding: 5px 10px;
    background: #123524;
    border-bottom: 1px solid rgba(74,222,128,0.32);
    color: #d8f7e3;
    font-size: 12px;
    flex-shrink: 0;
  }

  .update-banner__text {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .update-download {
    color: #86efac;
    font-weight: 700;
    text-decoration: none;
    white-space: nowrap;
    flex-shrink: 0;
  }

  .update-download:hover {
    text-decoration: underline;
  }

  .update-dismiss {
    width: 22px;
    height: 22px;
    border: none;
    border-radius: 4px;
    background: rgba(255,255,255,0.08);
    color: #b8e7c7;
    cursor: pointer;
    flex-shrink: 0;
    line-height: 1;
  }

  .update-dismiss:hover {
    background: rgba(255,255,255,0.14);
    color: #fff;
  }

  .tts-notice-banner {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 32px;
    padding: 5px 10px;
    background: #3a2c10;
    border-bottom: 1px solid rgba(251,191,36,0.38);
    color: #fdecc8;
    font-size: 12px;
    flex-shrink: 0;
  }

  .tts-notice-banner--info {
    background: #123524;
    border-bottom-color: rgba(74,222,128,0.32);
    color: #d8f7e3;
  }

  .tts-notice-banner--error {
    background: #3f1619;
    border-bottom-color: rgba(248,113,113,0.42);
    color: #ffd7d7;
  }

  .tts-notice-banner__text {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .tts-notice-dismiss {
    width: 22px;
    height: 22px;
    border: none;
    border-radius: 4px;
    background: rgba(255,255,255,0.08);
    color: #f6d994;
    cursor: pointer;
    flex-shrink: 0;
    line-height: 1;
  }

  .tts-notice-banner--info .tts-notice-dismiss {
    color: #b8e7c7;
  }

  .tts-notice-banner--error .tts-notice-dismiss {
    color: #ffb4b4;
  }

  .tts-notice-dismiss:hover {
    background: rgba(255,255,255,0.14);
    color: #fff;
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

  .app[data-theme='light'] .app-header {
    background: #ffffff;
    border-bottom-color: rgba(15,23,42,0.12);
  }

  .header-left {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
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

  .header-actions {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-shrink: 0;
  }

  .tabs {
    display: flex;
    gap: 2px;
  }

  .tabs-primary {
    order: 1;
  }

  .tools-menu {
    position: relative;
    order: 2;
    flex-shrink: 0;
  }

  .tabs-settings {
    order: 3;
  }

  .window-pin-btn {
    width: 26px;
    height: 26px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    background: rgba(255,255,255,0.04);
    border: 1px solid rgba(255,255,255,0.08);
    color: #bdbdbd;
    padding: 0;
    font-size: 14px;
    line-height: 1;
    cursor: pointer;
    border-radius: 4px;
    filter: grayscale(1);
    transition: color 0.15s, background 0.15s, border-color 0.15s, filter 0.15s;
  }

  .tts-toggle-btn {
    width: 26px;
    height: 26px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    padding: 0;
    border: 1px solid rgba(255,255,255,0.08);
    border-radius: 4px;
    color: #8b949e;
    background: rgba(255,255,255,0.04);
    cursor: pointer;
    transition: color 0.15s, background 0.15s, border-color 0.15s;
  }

  .tts-toggle-btn svg {
    width: 16px;
    height: 16px;
    fill: currentColor;
  }

  .tts-toggle-btn .speaker-waves,
  .tts-toggle-btn .speaker-slash {
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linecap: round;
  }

  .tts-toggle-btn.enabled {
    color: #ffffff;
    background: rgba(88,166,255,0.18);
    border-color: rgba(88,166,255,0.5);
  }

  .tts-toggle-btn.muted {
    color: #fca5a5;
  }

  .tts-toggle-btn:hover:not(:disabled) {
    color: #ffffff;
    background: rgba(255,255,255,0.1);
  }

  .tts-toggle-btn:focus-visible {
    outline: 2px solid #58a6ff;
    outline-offset: 2px;
  }

  .tts-toggle-btn:disabled {
    color: #555;
    cursor: not-allowed;
    opacity: 0.55;
  }

  .window-pin-btn.active {
    color: #fff;
    background: rgba(88,166,255,0.18);
    border-color: rgba(88,166,255,0.5);
    filter: grayscale(0);
  }

  .window-pin-btn:hover:not(.active) {
    color: #ddd;
    background: rgba(255,255,255,0.08);
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

  .tools-trigger {
    white-space: nowrap;
  }

  .tools-dropdown {
    position: absolute;
    top: calc(100% + 6px);
    right: 0;
    z-index: 30;
    min-width: 112px;
    padding: 4px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    background: #222;
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    box-shadow: 0 8px 20px rgba(0,0,0,0.35);
  }

  .tools-menu-item {
    width: 100%;
    background: none;
    border: none;
    color: #bdbdbd;
    padding: 6px 10px;
    font-size: 12px;
    text-align: left;
    cursor: pointer;
    border-radius: 3px;
    transition: color 0.15s, background 0.15s;
    white-space: nowrap;
  }

  .tools-menu-item.active {
    color: #fff;
    background: rgba(88,166,255,0.22);
  }

  .tools-menu-item:hover:not(.active) {
    color: #fff;
    background: rgba(255,255,255,0.08);
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

  .app[data-theme='light'] .channel-bar,
  .app[data-theme='light'] .toolbar {
    background: #eef2f6;
    border-bottom-color: rgba(15,23,42,0.1);
  }

  .app[data-theme='light'] .tab-btn {
    color: #52606d;
  }

  .app[data-theme='light'] .tab-btn.active {
    color: #111827;
    background: rgba(15,23,42,0.08);
  }

  .app[data-theme='light'] .tab-btn:hover:not(.active) {
    color: #1f2937;
    background: rgba(15,23,42,0.05);
  }

  .app[data-theme='light'] .window-pin-btn {
    background: rgba(15,23,42,0.03);
    border-color: rgba(15,23,42,0.12);
    color: #52606d;
  }

  .app[data-theme='light'] .tts-toggle-btn {
    color: #52606d;
    background: rgba(15,23,42,0.03);
    border-color: rgba(15,23,42,0.12);
  }

  .app[data-theme='light'] .tts-toggle-btn.enabled {
    color: #0f172a;
    background: rgba(25,118,210,0.13);
    border-color: rgba(25,118,210,0.38);
  }

  .app[data-theme='light'] .tts-toggle-btn.muted {
    color: #b91c1c;
  }

  .app[data-theme='light'] .window-pin-btn.active {
    color: #0f172a;
    background: rgba(25,118,210,0.13);
    border-color: rgba(25,118,210,0.38);
  }

  .app[data-theme='light'] .window-pin-btn:hover:not(.active) {
    color: #1f2937;
    background: rgba(15,23,42,0.06);
  }

  .app[data-theme='light'] .tools-dropdown {
    background: #ffffff;
    border-color: rgba(15,23,42,0.14);
    box-shadow: 0 8px 20px rgba(15,23,42,0.16);
  }

  .app[data-theme='light'] .tools-menu-item {
    color: #52606d;
  }

  .app[data-theme='light'] .tools-menu-item.active {
    color: #111827;
    background: rgba(25,118,210,0.13);
  }

  .app[data-theme='light'] .tools-menu-item:hover:not(.active) {
    color: #111827;
    background: rgba(15,23,42,0.06);
  }

  .filter-group {
    display: flex;
    gap: 2px;
    flex-shrink: 0;
  }

  .view-mode-group {
    display: flex;
    gap: 2px;
    flex-shrink: 0;
    padding-left: 6px;
    margin-left: 2px;
    border-left: 1px solid rgba(255,255,255,0.1);
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
  .filter-btn.x.active { background: rgba(29,161,242,0.22); border-color: #1da1f2; color: #1da1f2; }
  .filter-btn.niconico.active { background: rgba(252,200,0,0.22); border-color: #fcc800; color: #fcc800; }

  .view-mode-btn {
    background: rgba(255,255,255,0.06);
    border: 1px solid rgba(255,255,255,0.1);
    color: #9e9e9e;
    padding: 3px 8px;
    font-size: 11px;
    font-weight: 600;
    border-radius: 4px;
    cursor: pointer;
    transition: all 0.15s;
    white-space: nowrap;
  }

  .view-mode-btn.active {
    color: #fff;
    background: rgba(255,255,255,0.16);
    border-color: rgba(255,255,255,0.25);
  }

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

  .search-input[aria-invalid='true'] {
    border-color: rgba(248,113,113,0.55);
  }

  .app[data-theme='light'] .search-input {
    background: #ffffff;
    border-color: rgba(15,23,42,0.16);
    color: #20242a;
  }

  .app[data-theme='light'] .search-input::placeholder {
    color: #7b8794;
  }

  .app[data-theme='light'] .search-input:focus {
    border-color: rgba(25,118,210,0.45);
  }

  .app[data-theme='light'] .search-input[aria-invalid='true'] {
    border-color: rgba(220,38,38,0.52);
  }

  .search-mode-group {
    display: flex;
    gap: 2px;
    flex-shrink: 0;
    padding-left: 4px;
  }

  .search-mode-btn {
    min-width: 28px;
    height: 22px;
    background: rgba(255,255,255,0.06);
    border: 1px solid rgba(255,255,255,0.1);
    color: #9e9e9e;
    padding: 0 6px;
    font-size: 11px;
    font-weight: 700;
    border-radius: 4px;
    cursor: pointer;
    transition: all 0.15s;
    white-space: nowrap;
  }

  .search-mode-btn.active {
    color: #fff;
    background: rgba(88,166,255,0.22);
    border-color: rgba(88,166,255,0.55);
  }

  .app[data-theme='light'] .search-mode-btn {
    background: #ffffff;
    border-color: rgba(15,23,42,0.16);
    color: #52606d;
  }

  .app[data-theme='light'] .search-mode-btn.active {
    color: #0f172a;
    background: rgba(25,118,210,0.13);
    border-color: rgba(25,118,210,0.38);
  }

  .search-error {
    color: #fca5a5;
    font-size: 11px;
    white-space: nowrap;
    flex-shrink: 0;
  }

  .app[data-theme='light'] .search-error {
    color: #b91c1c;
  }

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
  .goals-panel {
    flex-shrink: 0;
    background: #171a1f;
    border-bottom: 1px solid rgba(255,255,255,0.08);
  }

  .goals-panel :global(.goals-bar) {
    border-bottom: 0;
  }

  .goals-panel-controls {
    min-height: 27px;
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
    padding: 3px 10px;
    color: #8b949e;
    font-size: 11px;
  }

  .goals-panel-label {
    margin-right: auto;
    color: #aeb6c2;
    font-weight: 800;
  }

  .goals-panel-toggle {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    min-height: 20px;
    color: #c7cdd6;
    font-weight: 600;
    cursor: pointer;
    user-select: none;
  }

  .goals-panel-toggle input {
    width: 15px;
    height: 15px;
    margin: 0;
    accent-color: #1976d2;
    cursor: pointer;
  }

  .goals-panel-toggle input:disabled,
  .goals-panel-toggle input:disabled + span {
    cursor: wait;
    opacity: 0.6;
  }

  .goals-panel-toggle:focus-within {
    color: #ffffff;
  }

  .goals-toggle-error {
    color: #fca5a5;
    font-weight: 700;
  }

  .app[data-theme='light'] .goals-panel {
    background: #eef2f6;
    border-bottom-color: rgba(15,23,42,0.1);
  }

  .app[data-theme='light'] .goals-panel-label,
  .app[data-theme='light'] .goals-panel-toggle {
    color: #334155;
  }

  .app[data-theme='light'] .goals-toggle-error {
    color: #b91c1c;
  }

  .main-content {
    flex: 1;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }

  /* Self-post composer toggle */
  .composer-toggle-bar {
    display: flex;
    justify-content: flex-end;
    padding: 3px 8px;
    background: #161616;
    border-top: 1px solid rgba(255, 255, 255, 0.06);
    flex-shrink: 0;
  }

  .composer-toggle {
    background: none;
    border: none;
    color: #9e9e9e;
    font-size: 12px;
    padding: 2px 8px;
    border-radius: 4px;
    cursor: pointer;
    transition: color 0.15s, background 0.15s;
  }

  .composer-toggle:hover {
    color: #fff;
    background: rgba(255, 255, 255, 0.06);
  }

  .composer-toggle.active {
    color: #cfe4ff;
  }

  .app[data-theme='light'] .composer-toggle-bar {
    background: #e6ebf1;
    border-top-color: rgba(15, 23, 42, 0.1);
  }

  .app[data-theme='light'] .composer-toggle {
    color: #52606d;
  }

  .app[data-theme='light'] .composer-toggle:hover {
    color: #111827;
    background: rgba(15, 23, 42, 0.06);
  }

  .app[data-font-size] :global(.comment-item) {
    font-size: var(--fc-comment-font-size);
  }

  .app[data-density] :global(.comment-item) {
    line-height: var(--fc-comment-line-height);
    padding-left: var(--fc-comment-padding-x);
    padding-right: var(--fc-comment-padding-x);
  }

  .app[data-density='compact'] :global(.badge-img),
  .app[data-density='compact'] :global(.emote) {
    height: 18px;
  }

  .app[data-youtube-member-name-green='true'] :global(.author-name.youtube-member-name) {
    color: #2ba640 !important;
  }

  .app[data-theme='light'][data-youtube-member-name-green='true']
    :global(.author-name.youtube-member-name) {
    color: #137333 !important;
  }

  .app[data-theme='light'] :global(.comment-item) {
    border-bottom-color: rgba(15,23,42,0.08);
  }

  .app[data-theme='light'] :global(.comment-list-container) {
    scrollbar-color: rgba(15,23,42,0.22) transparent;
  }

  .app[data-theme='light'] :global(.settings h2),
  .app[data-theme='light'] :global(.settings h3),
  .app[data-theme='light'] :global(.field-row label),
  .app[data-theme='light'] :global(.dict-header),
  .app[data-theme='light'] :global(.mod-list-header),
  .app[data-theme='light'] :global(.obs-label) {
    color: #20242a;
  }

  .app[data-theme='light'] :global(.hint),
  .app[data-theme='light'] :global(.hint-inline),
  .app[data-theme='light'] :global(.mod-empty) {
    color: #64748b;
  }

  .app[data-theme='light'] :global(.platform-select),
  .app[data-theme='light'] :global(.id-input),
  .app[data-theme='light'] :global(.obs-input),
  .app[data-theme='light'] :global(.num-input),
  .app[data-theme='light'] :global(.mod-area) {
    background: #ffffff;
    border-color: rgba(15,23,42,0.16);
    color: #20242a;
  }

  .app[data-theme='light'] :global(section) {
    border-bottom-color: rgba(15,23,42,0.1);
  }
</style>
