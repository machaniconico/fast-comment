/**
 * Svelte 5 runes-based appearance settings store (singleton).
 *
 * Keeps UI-only appearance preferences in localStorage so they can be applied
 * immediately without involving the Tauri config file.
 */

export type AppearanceTheme = 'dark' | 'light' | 'auto';
export type ResolvedTheme = 'dark' | 'light';
export type AppearanceFontSize = 's' | 'm' | 'l';
export type AppearanceDensity = 'comfortable' | 'compact';
export type AppearanceTimeDisplay = 'seconds' | 'minutes' | 'off';
export type AppearanceViewerDisplay = 'rotate' | 'totalRotate' | 'all';

export interface AppearanceSnapshot {
  theme: AppearanceTheme;
  fontSize: AppearanceFontSize;
  density: AppearanceDensity;
  timeDisplay: AppearanceTimeDisplay;
  viewerDisplay: AppearanceViewerDisplay;
  wrapComments: boolean;
  showViewerBadges: boolean;
  showCommentMilestones: boolean;
}

const STORAGE_KEY = 'fc.appearance';
const DEFAULT_APPEARANCE: AppearanceSnapshot = {
  theme: 'dark',
  fontSize: 'm',
  density: 'comfortable',
  timeDisplay: 'seconds',
  viewerDisplay: 'rotate',
  wrapComments: false,
  showViewerBadges: false,
  showCommentMilestones: false,
};

function canUseLocalStorage(): boolean {
  return typeof window !== 'undefined' && typeof localStorage !== 'undefined';
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isTheme(value: unknown): value is AppearanceTheme {
  return value === 'dark' || value === 'light' || value === 'auto';
}

function isFontSize(value: unknown): value is AppearanceFontSize {
  return value === 's' || value === 'm' || value === 'l';
}

function isDensity(value: unknown): value is AppearanceDensity {
  return value === 'comfortable' || value === 'compact';
}

function isTimeDisplay(value: unknown): value is AppearanceTimeDisplay {
  return value === 'seconds' || value === 'minutes' || value === 'off';
}

function isViewerDisplay(value: unknown): value is AppearanceViewerDisplay {
  return value === 'rotate' || value === 'totalRotate' || value === 'all';
}

function parseAppearanceSnapshot(value: unknown): AppearanceSnapshot | null {
  if (!isRecord(value)) {
    return null;
  }

  const themeValue = value.theme;
  const fontSizeValue = value.fontSize;
  const densityValue = value.density;
  const timeDisplayValue = value.timeDisplay === undefined
    ? DEFAULT_APPEARANCE.timeDisplay
    : value.timeDisplay;
  const viewerDisplayValue = value.viewerDisplay === undefined
    ? DEFAULT_APPEARANCE.viewerDisplay
    : value.viewerDisplay;
  const wrapCommentsValue = value.wrapComments === undefined
    ? DEFAULT_APPEARANCE.wrapComments
    : value.wrapComments;
  const showViewerBadgesValue = value.showViewerBadges === undefined
    ? DEFAULT_APPEARANCE.showViewerBadges
    : value.showViewerBadges;
  const showCommentMilestonesValue = value.showCommentMilestones === undefined
    ? DEFAULT_APPEARANCE.showCommentMilestones
    : value.showCommentMilestones;

  if (
    !isTheme(themeValue) ||
    !isFontSize(fontSizeValue) ||
    !isDensity(densityValue) ||
    !isTimeDisplay(timeDisplayValue) ||
    !isViewerDisplay(viewerDisplayValue) ||
    typeof wrapCommentsValue !== 'boolean' ||
    typeof showViewerBadgesValue !== 'boolean' ||
    typeof showCommentMilestonesValue !== 'boolean'
  ) {
    return null;
  }

  return {
    theme: themeValue,
    fontSize: fontSizeValue,
    density: densityValue,
    timeDisplay: timeDisplayValue,
    viewerDisplay: viewerDisplayValue,
    wrapComments: wrapCommentsValue,
    showViewerBadges: showViewerBadgesValue,
    showCommentMilestones: showCommentMilestonesValue,
  };
}

class ThemeStore {
  theme: AppearanceTheme = $state(DEFAULT_APPEARANCE.theme);
  fontSize: AppearanceFontSize = $state(DEFAULT_APPEARANCE.fontSize);
  density: AppearanceDensity = $state(DEFAULT_APPEARANCE.density);
  timeDisplay: AppearanceTimeDisplay = $state(DEFAULT_APPEARANCE.timeDisplay);
  viewerDisplay: AppearanceViewerDisplay = $state(DEFAULT_APPEARANCE.viewerDisplay);
  wrapComments: boolean = $state(DEFAULT_APPEARANCE.wrapComments);
  showViewerBadges: boolean = $state(DEFAULT_APPEARANCE.showViewerBadges);
  showCommentMilestones: boolean = $state(DEFAULT_APPEARANCE.showCommentMilestones);
  systemTheme: ResolvedTheme = $state('dark');

  private mediaCleanup: (() => void) | null = null;

  get resolved(): ResolvedTheme {
    return this.theme === 'auto' ? this.systemTheme : this.theme;
  }

  load(): void {
    this.subscribeSystemTheme();

    if (!canUseLocalStorage()) {
      return;
    }

    try {
      const raw = localStorage.getItem(STORAGE_KEY);
      if (raw === null) {
        this.apply(DEFAULT_APPEARANCE);
        return;
      }

      const snapshot = parseAppearanceSnapshot(JSON.parse(raw) as unknown);
      if (!snapshot) {
        this.apply(DEFAULT_APPEARANCE);
        return;
      }

      this.apply(snapshot);
    } catch {
      this.apply(DEFAULT_APPEARANCE);
    }
  }

  destroy(): void {
    this.mediaCleanup?.();
    this.mediaCleanup = null;
  }

  setTheme(value: AppearanceTheme): void {
    this.theme = value;
    this.save();
  }

  setFontSize(value: AppearanceFontSize): void {
    this.fontSize = value;
    this.save();
  }

  setDensity(value: AppearanceDensity): void {
    this.density = value;
    this.save();
  }

  setTimeDisplay(value: AppearanceTimeDisplay): void {
    this.timeDisplay = value;
    this.save();
  }

  setViewerDisplay(value: AppearanceViewerDisplay): void {
    this.viewerDisplay = value;
    this.save();
  }

  setWrapComments(value: boolean): void {
    this.wrapComments = value;
    this.save();
  }

  setShowViewerBadges(value: boolean): void {
    this.showViewerBadges = value;
    this.save();
  }

  setShowCommentMilestones(value: boolean): void {
    this.showCommentMilestones = value;
    this.save();
  }

  getSnapshot(): AppearanceSnapshot {
    return {
      theme: this.theme,
      fontSize: this.fontSize,
      density: this.density,
      timeDisplay: this.timeDisplay,
      viewerDisplay: this.viewerDisplay,
      wrapComments: this.wrapComments,
      showViewerBadges: this.showViewerBadges,
      showCommentMilestones: this.showCommentMilestones,
    };
  }

  applySnapshot(next: unknown): boolean {
    const snapshot = parseAppearanceSnapshot(next);
    if (!snapshot) {
      return false;
    }

    this.apply(snapshot);
    this.save();
    return true;
  }

  private apply(next: AppearanceSnapshot): void {
    this.theme = next.theme;
    this.fontSize = next.fontSize;
    this.density = next.density;
    this.timeDisplay = next.timeDisplay;
    this.viewerDisplay = next.viewerDisplay;
    this.wrapComments = next.wrapComments;
    this.showViewerBadges = next.showViewerBadges;
    this.showCommentMilestones = next.showCommentMilestones;
  }

  private save(): void {
    if (!canUseLocalStorage()) {
      return;
    }

    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(this.getSnapshot()));
    } catch {
      // localStorage can be unavailable or full; appearance changes still apply in memory.
    }
  }

  private subscribeSystemTheme(): void {
    this.destroy();

    if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') {
      this.systemTheme = 'dark';
      return;
    }

    const media = window.matchMedia('(prefers-color-scheme: light)');
    this.systemTheme = media.matches ? 'light' : 'dark';

    const onChange = (event: MediaQueryListEvent) => {
      this.systemTheme = event.matches ? 'light' : 'dark';
    };

    media.addEventListener('change', onChange);
    this.mediaCleanup = () => {
      media.removeEventListener('change', onChange);
    };
  }
}

export const theme = new ThemeStore();
