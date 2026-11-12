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

import { ZOOM_DEFAULT, clampZoom, normalizeZoom, stepZoom, type ZoomAction } from './displayFilter';

export type Tab = 'comments' | 'donations' | 'firstComments' | 'participation' | 'settings';
export type ViewMode = 'unified' | 'columns';
export type ToolSettingsView = 'goals' | 'danmaku';

/** Settings sections that the command palette can jump to. */
export type SettingsAnchor =
  | 'appearance'
  | 'tts'
  | 'obs'
  | 'goals'
  | 'timer'
  | 'moderation'
  | 'notify'
  | 'danmaku'
  | 'portability'
  | 'youtubeOauth'
  | 'twitchOauth';

/** Maps a SettingsAnchor to the DOM id rendered on the matching <section>. */
export const SETTINGS_ANCHOR_IDS: Record<SettingsAnchor, string> = {
  appearance: 'settings-appearance',
  tts: 'settings-tts',
  obs: 'settings-obs',
  goals: 'settings-goals',
  timer: 'settings-timer',
  moderation: 'settings-moderation',
  notify: 'settings-notify',
  danmaku: 'settings-danmaku',
  portability: 'settings-portability',
  youtubeOauth: 'settings-youtube-low-latency',
  twitchOauth: 'settings-twitch-moderation',
};

/** localStorage key for persisting recent command ids. */
const RECENT_COMMANDS_KEY = 'fc.recentCommands';
/** localStorage key for persisting the window always-on-top preference. */
const ALWAYS_ON_TOP_KEY = 'fc.alwaysOnTop';
/** localStorage key for persisting the UI zoom level. */
const ZOOM_KEY = 'fc.uiZoom';
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

/** Load the UI zoom level from localStorage; invalid or missing → 1.0. */
function loadZoom(): number {
  if (typeof window === 'undefined') return ZOOM_DEFAULT;
  try {
    const raw = localStorage.getItem(ZOOM_KEY);
    if (raw === null) return ZOOM_DEFAULT;
    return normalizeZoom(JSON.parse(raw));
  } catch {
    return ZOOM_DEFAULT;
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

/** Target of the user card (listener identity as shown on the comment). */
export interface UserCardTarget {
  platform: string;
  authorId: string;
  name: string;
  avatarUrl?: string;
  /** Stream channel (Twitch channel / YouTube videoId). Set only when opened from a comment. */
  channel?: string;
}

/** Generic toast (moderation results etc.). */
export type ToastKind = 'info' | 'success' | 'error';
export interface Toast {
  id: number;
  text: string;
  kind: ToastKind;
  action?: { label: string; run: () => void };
}

class UiStore {
  userCard: UserCardTarget | null = $state(null);
  // Initial status line for the card (e.g. a failure from a context-menu action).
  userCardNotice: string = $state('');

  openUserCard(target: UserCardTarget, notice = ''): void {
    this.userCardNotice = notice;
    this.userCard = target;
  }

  closeUserCard(): void {
    this.userCard = null;
  }

  activeTab: Tab = $state('comments');
  viewMode: ViewMode = $state('unified');
  showDashboard: boolean = $state(false);
  showRaffle: boolean = $state(false);
  showTimer: boolean = $state(false);
  toolSettingsView: ToolSettingsView | null = $state(null);
  // Comment composer (self-post to chat) open flag — toggled below the comment list.
  composerOpen: boolean = $state(false);
  paletteOpen: boolean = $state(false);
  showShortcuts: boolean = $state(false);
  alwaysOnTop: boolean = $state(loadAlwaysOnTop());
  zoom: number = $state(loadZoom());
  // Transient "NN%" toast text shown after a zoom change ('' = hidden).
  zoomToast: string = $state('');
  private _zoomToastTimer: ReturnType<typeof setTimeout> | null = null;
  // Generic toast (null = hidden). Separate from zoomToast, which keeps its own 1s style.
  toast: Toast | null = $state(null);
  private _toastTimer: ReturnType<typeof setTimeout> | null = null;
  private _toastSeq = 0;

  /** Show a toast; a newer one replaces the current. Errors stay longer. */
  showToast(text: string, kind: ToastKind = 'info', action?: Toast['action']): void {
    this._toastSeq += 1;
    this.toast = { id: this._toastSeq, text, kind, action };
    if (this._toastTimer !== null) clearTimeout(this._toastTimer);
    this._toastTimer = setTimeout(() => {
      this.toast = null;
      this._toastTimer = null;
    }, kind === 'error' ? 7000 : 3000);
  }

  dismissToast(): void {
    if (this._toastTimer !== null) clearTimeout(this._toastTimer);
    this._toastTimer = null;
    this.toast = null;
  }
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
    this.toolSettingsView = null;
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
      this.toolSettingsView = null;
    }
  }

  toggleDashboard(): void {
    this.showDashboard = !this.showDashboard;
    if (this.showDashboard) {
      this.showRaffle = false;
      this.showTimer = false;
      this.toolSettingsView = null;
    }
  }

  toggleRaffle(): void {
    this.showRaffle = !this.showRaffle;
    if (this.showRaffle) {
      this.showDashboard = false;
      this.showTimer = false;
      this.toolSettingsView = null;
    }
  }

  setShowTimer(show: boolean): void {
    this.showTimer = show;
    if (this.showTimer) {
      this.showDashboard = false;
      this.showRaffle = false;
      this.toolSettingsView = null;
    }
  }

  toggleTimer(): void {
    this.showTimer = !this.showTimer;
    if (this.showTimer) {
      this.showDashboard = false;
      this.showRaffle = false;
      this.toolSettingsView = null;
    }
  }

  toggleComposer(): void {
    this.composerOpen = !this.composerOpen;
  }

  openToolSettings(view: ToolSettingsView): void {
    this.toolSettingsView = view;
    this.showDashboard = false;
    this.showRaffle = false;
    this.showTimer = false;
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

  /** Apply the stored zoom to the webview (call once at startup). No-op outside Tauri. */
  restoreZoom(): void {
    if (this.zoom !== ZOOM_DEFAULT) void this.applyZoom(this.zoom);
  }

  private async applyZoom(level: number): Promise<void> {
    try {
      const { getCurrentWebview } = await import('@tauri-apps/api/webview');
      await getCurrentWebview().setZoom(level);
    } catch {
      // Not running inside Tauri (plain vite dev) or permission missing: ignore.
    }
  }

  /** Zoom in/out by one step, or reset to 100%; persists, applies and shows a 1s toast. */
  changeZoom(action: ZoomAction): void {
    const next = action === 'reset' ? ZOOM_DEFAULT : stepZoom(this.zoom, action === 'in' ? 1 : -1);
    this.zoom = clampZoom(next);
    void this.applyZoom(this.zoom);
    try {
      localStorage.setItem(ZOOM_KEY, JSON.stringify(this.zoom));
    } catch {
      // Storage quota or private-mode errors are non-fatal.
    }
    this.zoomToast = `${Math.round(this.zoom * 100)}%`;
    if (this._zoomToastTimer !== null) clearTimeout(this._zoomToastTimer);
    this._zoomToastTimer = setTimeout(() => {
      this.zoomToast = '';
      this._zoomToastTimer = null;
    }, 1000);
  }

  /** Switch to the settings tab and request a scroll to the given section. */
  gotoSetting(anchor: SettingsAnchor): void {
    if (anchor === 'goals' || anchor === 'danmaku') {
      this.openToolSettings(anchor);
      this.settingsAnchor = null;
      return;
    }
    this.activeTab = 'settings';
    this.showDashboard = false;
    this.showRaffle = false;
    this.showTimer = false;
    this.toolSettingsView = null;
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
