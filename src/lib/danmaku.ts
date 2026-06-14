export type DanmakuSettings = {
  fontSize: number;
  durationSec: number;
  opacity: number;
  showName: boolean;
  outline: boolean;
  maxActive: number;
};

export const DANMAKU_STORAGE_KEY = 'fc.danmaku';
export const DANMAKU_SETTINGS_EVENT = 'danmaku:settings';

export const DANMAKU_DEFAULTS: DanmakuSettings = {
  fontSize: 30,
  durationSec: 7,
  opacity: 0.92,
  showName: false,
  outline: true,
  maxActive: 240,
};

function finiteNumber(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

export function clampDanmakuSettings(s: Partial<DanmakuSettings>): DanmakuSettings {
  const merged = { ...DANMAKU_DEFAULTS, ...s };

  return {
    fontSize: clamp(finiteNumber(merged.fontSize, DANMAKU_DEFAULTS.fontSize), 12, 96),
    durationSec: clamp(finiteNumber(merged.durationSec, DANMAKU_DEFAULTS.durationSec), 2, 30),
    opacity: clamp(finiteNumber(merged.opacity, DANMAKU_DEFAULTS.opacity), 0.1, 1),
    showName: merged.showName === true,
    outline: merged.outline === true,
    maxActive: Math.trunc(
      clamp(finiteNumber(merged.maxActive, DANMAKU_DEFAULTS.maxActive), 20, 1000),
    ),
  };
}

export function loadDanmakuSettings(): DanmakuSettings {
  if (typeof window === 'undefined') return { ...DANMAKU_DEFAULTS };

  try {
    const raw = localStorage.getItem(DANMAKU_STORAGE_KEY);
    if (!raw) return { ...DANMAKU_DEFAULTS };
    return clampDanmakuSettings(JSON.parse(raw) as Partial<DanmakuSettings>);
  } catch {
    return { ...DANMAKU_DEFAULTS };
  }
}

export function saveDanmakuSettings(s: DanmakuSettings): void {
  if (typeof window === 'undefined') return;

  try {
    localStorage.setItem(DANMAKU_STORAGE_KEY, JSON.stringify(clampDanmakuSettings(s)));
  } catch {
    // localStorage can fail in private mode or when storage quota is exhausted.
  }
}
