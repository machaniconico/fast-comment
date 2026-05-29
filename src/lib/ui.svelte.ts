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

export type Tab = 'comments' | 'settings';

/** Settings sections that the command palette can jump to. */
export type SettingsAnchor = 'channels' | 'tts' | 'obs' | 'moderation';

/** Maps a SettingsAnchor to the DOM id rendered on the matching <section>. */
export const SETTINGS_ANCHOR_IDS: Record<SettingsAnchor, string> = {
  channels: 'settings-channels',
  tts: 'settings-tts',
  obs: 'settings-obs',
  moderation: 'settings-moderation',
};

class UiStore {
  activeTab: Tab = $state('comments');
  paletteOpen: boolean = $state(false);
  // One-shot scroll target. Set when navigating from the command palette;
  // Settings consumes it (scrolls, then clears) so reopening the tab later
  // does not re-scroll.
  settingsAnchor: SettingsAnchor | null = $state(null);

  setTab(tab: Tab): void {
    this.activeTab = tab;
  }

  openPalette(): void {
    this.paletteOpen = true;
  }

  closePalette(): void {
    this.paletteOpen = false;
  }

  togglePalette(): void {
    this.paletteOpen = !this.paletteOpen;
  }

  /** Switch to the settings tab and request a scroll to the given section. */
  gotoSetting(anchor: SettingsAnchor): void {
    this.activeTab = 'settings';
    this.settingsAnchor = anchor;
  }

  /** Clear the pending settings anchor (called by Settings after scrolling). */
  clearSettingsAnchor(): void {
    this.settingsAnchor = null;
  }
}

// Singleton UI store — import and use directly in components.
export const ui = new UiStore();
