// Pure helpers for the achievements management UI. Whether a combination is valid is decided
// by Rust (`achievement_schema` / `validate_all`); these only turn the schema into UI state.
import type {
  AchievementCondition,
  AchievementCurrencyKind,
  AchievementMetric,
  AchievementSchema,
  AchievementSubjectKind,
  AchievementTier,
  AchievementWindow,
  ListenerTrophy,
  Platform,
} from './types';

export interface Option<T extends string> {
  value: T;
  disabled: boolean;
}

export const DAY_MS = 24 * 60 * 60 * 1000;
export const RATES_STALE_DAYS = 90;

const ruleOf = (schema: AchievementSchema, metric: AchievementMetric) =>
  schema.metrics.find((rule) => rule.metric === metric);

/** Metrics selectable for a subject. With platforms chosen, a metric none of them provides is disabled. */
export function metricOptions(
  schema: AchievementSchema,
  subject: AchievementSubjectKind,
  platforms: Platform[],
): Option<AchievementMetric>[] {
  return schema.metrics.map((rule) => ({
    value: rule.metric,
    disabled:
      !rule.subjects.includes(subject) ||
      (platforms.length > 0 && !platforms.some((p) => rule.platforms.includes(p))),
  }));
}

const optionsOf = <T extends string>(all: readonly T[], allowed: readonly T[] | undefined): Option<T>[] =>
  all.map((value) => ({ value, disabled: !allowed?.includes(value) }));

export const WINDOWS: readonly AchievementWindow[] = ['lifetime', 'perStream'];
export const CURRENCY_KINDS: readonly AchievementCurrencyKind[] = ['none', 'jpyConverted', 'only'];
export const PLATFORMS: readonly Platform[] = ['twitch', 'youtube', 'x', 'niconico', 'twitcasting'];

export const windowOptions = (schema: AchievementSchema, metric: AchievementMetric) =>
  optionsOf(WINDOWS, ruleOf(schema, metric)?.windows);
export const currencyOptions = (schema: AchievementSchema, metric: AchievementMetric) =>
  optionsOf(CURRENCY_KINDS, ruleOf(schema, metric)?.currencyModes);
export const platformOptions = (schema: AchievementSchema, metric: AchievementMetric) =>
  optionsOf(PLATFORMS, ruleOf(schema, metric)?.platforms);

/** After the metric changes, pull window / currency / platforms back into what the schema allows. */
export function coerceCondition(schema: AchievementSchema, condition: AchievementCondition): AchievementCondition {
  const rule = ruleOf(schema, condition.metric);
  if (!rule) return condition;
  const window = rule.windows.includes(condition.window) ? condition.window : rule.windows[0];
  const mode = condition.currency.mode;
  const currency = rule.currencyModes.includes(mode)
    ? condition.currency
    : rule.currencyModes[0] === 'only'
      ? { mode: 'only' as const, code: '' }
      : { mode: rule.currencyModes[0] ?? ('none' as const) };
  const platforms = condition.platforms.filter((p) => rule.platforms.includes(p));
  return { ...condition, window, currency, platforms };
}

/** True when the rate table was never stamped or is older than 90 days (exactly 90 days is fine). */
export function ratesStale(updatedAtMs: number | null, nowMs: number): boolean {
  return updatedAtMs === null || nowMs - updatedAtMs > RATES_STALE_DAYS * DAY_MS;
}

export function formatDate(ms: number, timeZone?: string): string {
  return new Date(ms).toLocaleString('ja-JP', {
    timeZone, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false,
  });
}

export function ratesNotice(updatedAtMs: number | null, nowMs: number, timeZone?: string): { text: string; warn: boolean } {
  if (updatedAtMs === null) return { text: '最終更新日: 未設定(換算値を確認してください)', warn: true };
  const warn = ratesStale(updatedAtMs, nowMs);
  const days = Math.floor((nowMs - updatedAtMs) / DAY_MS);
  const base = `最終更新日: ${formatDate(updatedAtMs, timeZone).split(' ')[0]}`;
  return { text: warn ? `${base}(${days} 日前。為替が古い可能性があります)` : base, warn };
}

/** Parse a rate input: a finite number greater than zero, otherwise null. */
export function parseRate(text: string): number | null {
  const value = Number(text.trim());
  return text.trim() !== '' && Number.isFinite(value) && value > 0 ? value : null;
}

export const TIER_LABELS: Record<AchievementTier, string> = {
  bronze: 'ブロンズ', silver: 'シルバー', gold: 'ゴールド', platinum: 'プラチナ',
};

export interface TrophyView {
  key: string;
  name: string;
  icon: string;
  tier: AchievementTier;
  tierLabel: string;
  unlockedAt: string;
  scope: string;
}

/** Earned trophies for the listener detail, newest first. */
export function trophyViews(trophies: ListenerTrophy[], timeZone?: string): TrophyView[] {
  return [...trophies]
    .sort((a, b) => b.unlockedAtMs - a.unlockedAtMs)
    .map((t) => ({
      key: `${t.achievementId}|${t.windowKey}`,
      name: t.name,
      icon: t.icon,
      tier: t.tier,
      tierLabel: TIER_LABELS[t.tier],
      unlockedAt: formatDate(t.unlockedAtMs, timeZone),
      scope: t.windowKey === '*' ? '' : '配信ごと',
    }));
}
