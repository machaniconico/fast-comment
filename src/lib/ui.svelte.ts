/**
 * Svelte 5 runes-based UI view-state store (singleton).
 *
 * Holds cross-component view state that does not belong to the comment data
 * store: the active tab, the command-palette open flag, and a one-shot
 * "settings anchor" the command palette uses to scroll the Settings panel to a
 * specific section after switching tabs.
 *
 * Same pattern as stores.svelte.ts: a reactive class instance exported as a
 * singleton so any component can import `ui` and read/write reactively.
 */

export type Tab = 'comments' | 'donations' | 'participation' | 'settings';
export type ViewMode = 'unified' | 'columns';

/** Settings sections that the command palette can jump to. */
export type SettingsAnchor =
  | 'appearance'
  | 'tts'
  | 'obs'
  | 'timer'
  | 'moderation'
  | 'notify'
  | 'portability';

/** Maps a SettingsAnchor to the DOM id rendered on the matching <section>. */
export const SETTINGS_ANCHOR_IDS: Record<SettingsAnchor, string> = {
  appearance: 'settings-appearance',
  tts: 'settings-tts',
  obs: 'settings-obs',
  timer: 'settings-timer',
  moderation: 'settings-moderation',
  notify: 'settings-notify',
  portability: 'settings-portability',
};

/** localStorage key for persisting recent command ids. */
const RECENT_COMMANDS_KEY = 'fc.recentCommands';
/** localStorage key for persisting the window always-on-top preference. */
const ALWAYS_ON_TOP_KEY = 'fc.alwaysOnTop';
/** Maximum number of recent command entries to retain. */
const RECENT_COMMANDS_MAX = 5;

/**
 * Load recent command ids from localStorage.
 * Returns [] on SSR, missing key, or any parse/validation error.
 */
function loadRecentCommandIds(): string[] {
  if (typeof window === 'undefined') return [];
  try {
    const raw = localStorage.getItem(RECENT_COMMANDS_KEY);
    if (raw === null) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    const ids = parsed.filter((v): v is string => typeof v === 'string');
    return ids.slice(0, RECENT_COMMANDS_MAX);
  } catch {
    return [];
  }
}

/**
 * Load the always-on-top preference from localStorage.
 * Returns false on SSR, missing key, or any parse/validation error.
 */
function loadAlwaysOnTop(): boolean {
  if (typeof window === 'undefined') return false;
  try {
    const raw = localStorage.getItem(ALWAYS_ON_TOP_KEY);
    if (raw === null) return false;
    const parsed: unknown = JSON.parse(raw);
    return typeof parsed === 'boolean' ? parsed : false;
  } catch {
    return false;
  }
}

class UiStore {
  activeTab: Tab = $state('comments');
  viewMode: ViewMode = $state('unified');
  showDashboard: boolean = $state(false);
  showRaffle: boolean = $state(false);
  showTimer: boolean = $state(false);
  // Comment composer (self-post to chat) open flag — toggled below the comment list.
  composerOpen: boolean = $state(false);
  paletteOpen: boolean = $state(false);
  showShortcuts: boolean = $state(false);
  alwaysOnTop: boolean = $state(loadAlwaysOnTop());
  // One-shot scroll target. Set when navigating from the command palette;
  // Settings consumes it (scrolls, then clears) so reopening the tab later
  // does not re-scroll.
  settingsAnchor: SettingsAnchor | null = $state(null);
  // Recently used command ids — newest first, max RECENT_COMMANDS_MAX entries.
  recentCommandIds: string[] = $state(loadRecentCommandIds());

  setTab(tab: Tab): void {
    this.activeTab = tab;
    this.showDashboard = false;
    this.showRaffle = false;
    this.showTimer = false;
  }

  setViewMode(mode: ViewMode): void {
    this.viewMode = mode;
  }

  toggleViewMode(): void {
    this.viewMode = this.viewMode === 'unified' ? 'columns' : 'unified';
  }

  setShowDashboard(show: boolean): void {
    this.showDashboard = show;
    if (this.showDashboard) {
      this.showRaffle = false;
      this.showTimer = false;
    }
  }

  toggleDashboard(): void {
    this.showDashboard = !this.showDashboard;
    if (this.showDashboard) {
      this.showRaffle = false;
      this.showTimer = false;
    }
  }

  toggleRaffle(): void {
    this.showRaffle = !this.showRaffle;
    if (this.showRaffle) {
      this.showDashboard = false;
      this.showTimer = false;
    }
  }

  setShowTimer(show: boolean): void {
    this.showTimer = show;
    if (this.showTimer) {
      this.showDashboard = false;
      this.showRaffle = false;
    }
  }

  toggleTimer(): void {
    this.showTimer = !this.showTimer;
    if (this.showTimer) {
      this.showDashboard = false;
      this.showRaffle = false;
    }
  }

  toggleComposer(): void {
    this.composerOpen = !this.composerOpen;
  }

  openPalette(): void {
    this.paletteOpen = true;
    this.showShortcuts = false;
  }

  closePalette(): void {
    this.paletteOpen = false;
  }

  togglePalette(): void {
    this.paletteOpen = !this.paletteOpen;
    if (this.paletteOpen) {
      this.showShortcuts = false;
    }
  }

  closeShortcuts(): void {
    this.showShortcuts = false;
  }

  toggleShortcuts(): void {
    this.showShortcuts = !this.showShortcuts;
    if (this.showShortcuts) {
      this.paletteOpen = false;
    }
  }

  setAlwaysOnTop(value: boolean): void {
    this.alwaysOnTop = value;
    if (typeof window === 'undefined') return;
    try {
      localStorage.setItem(ALWAYS_ON_TOP_KEY, JSON.stringify(value));
    } catch {
      // Storage quota or private-mode errors are non-fatal.
    }
  }

  /** Switch to the settings tab and request a scroll to the given section. */
  gotoSetting(anchor: SettingsAnchor): void {
    this.activeTab = 'settings';
    this.showDashboard = false;
    this.showRaffle = false;
    this.showTimer = false;
    this.settingsAnchor = anchor;
  }

  /** Clear the pending settings anchor (called by Settings after scrolling). */
  clearSettingsAnchor(): void {
    this.settingsAnchor = null;
  }

  /**
   * Record a command execution in the recent-commands list.
   * Deduplicates, prepends the id, and caps at RECENT_COMMANDS_MAX.
   * Persists to localStorage with try/catch so storage errors are silent.
   */
  recordCommand(id: string): void {
    const next = [id, ...this.recentCommandIds.filter((v) => v !== id)].slice(
      0,
      RECENT_COMMANDS_MAX,
    );
    this.recentCommandIds = next;
    try {
      localStorage.setItem(RECENT_COMMANDS_KEY, JSON.stringify(next));
    } catch {
      // Storage quota or private-mode errors are non-fatal.
    }
  }
}

// Singleton UI store — import and use directly in components.
export const ui = new UiStore();
