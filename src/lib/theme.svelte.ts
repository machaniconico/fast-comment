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

export interface AppearanceSnapshot {
  theme: AppearanceTheme;
  fontSize: AppearanceFontSize;
  density: AppearanceDensity;
}

const STORAGE_KEY = 'fc.appearance';
const DEFAULT_APPEARANCE: AppearanceSnapshot = {
  theme: 'dark',
  fontSize: 'm',
  density: 'comfortable',
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

class ThemeStore {
  theme: AppearanceTheme = $state(DEFAULT_APPEARANCE.theme);
  fontSize: AppearanceFontSize = $state(DEFAULT_APPEARANCE.fontSize);
  density: AppearanceDensity = $state(DEFAULT_APPEARANCE.density);
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

      const parsed = JSON.parse(raw) as unknown;
      if (
        !isRecord(parsed) ||
        !isTheme(parsed.theme) ||
        !isFontSize(parsed.fontSize) ||
        !isDensity(parsed.density)
      ) {
        this.apply(DEFAULT_APPEARANCE);
        return;
      }

      this.apply({
        theme: parsed.theme,
        fontSize: parsed.fontSize,
        density: parsed.density,
      });
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

  getSnapshot(): AppearanceSnapshot {
    return {
      theme: this.theme,
      fontSize: this.fontSize,
      density: this.density,
    };
  }

  applySnapshot(next: unknown): boolean {
    if (
      !isRecord(next) ||
      !isTheme(next.theme) ||
      !isFontSize(next.fontSize) ||
      !isDensity(next.density)
    ) {
      return false;
    }

    this.apply({
      theme: next.theme,
      fontSize: next.fontSize,
      density: next.density,
    });
    this.save();
    return true;
  }

  private apply(next: AppearanceSnapshot): void {
    this.theme = next.theme;
    this.fontSize = next.fontSize;
    this.density = next.density;
  }

  private save(): void {
    if (!canUseLocalStorage()) {
      return;
    }

    try {
      localStorage.setItem(
        STORAGE_KEY,
        JSON.stringify({
          theme: this.theme,
          fontSize: this.fontSize,
          density: this.density,
        })
      );
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
