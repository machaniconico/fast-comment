// Pure helpers for the achievements management UI. Whether a combination is valid is decided
// by Rust (`achievement_schema` / `validate_all`); these only turn the schema into UI state.
import type {
  AchievementCondition,
  AchievementDef,
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

/** Changing the subject: move the metric to one the subject allows, then re-coerce the rest. */
export function withSubject(schema: AchievementSchema, a: AchievementDef, subject: AchievementSubjectKind): AchievementDef {
  if (!a.condition) return a;
  const options = metricOptions(schema, subject, a.condition.platforms);
  const metric = options.some((o) => o.value === a.condition!.metric && !o.disabled)
    ? a.condition.metric
    : (options.find((o) => !o.disabled)?.value ?? a.condition.metric);
  return { ...a, subject, condition: coerceCondition(schema, { ...a.condition, metric }) };
}

/** Platinum is unique per subject, so it can be neither duplicated nor deleted. */
export const canDuplicate = (a: AchievementDef): boolean => a.tier !== 'platinum';
/** Only user-made achievements (duplicates) can be deleted; presets are disabled instead. */
export const canDelete = (a: AchievementDef): boolean => a.builtinId === null && a.tier !== 'platinum';

export interface RateRow {
  code: string;
  text: string;
}

/** Pre-save check of the rate table. Rust normalizes keys and has the final say. */
export function checkRateRows(rows: RateRow[]): { rates: Record<string, number> } | { error: string } {
  const rates: Record<string, number> = {};
  const seen = new Set<string>();
  for (const row of rows) {
    const code = row.code.trim();
    if (!code && !row.text.trim()) continue;
    if (!code) return { error: 'レートの通貨コードが空の行があります。' };
    const rate = parseRate(row.text);
    if (rate === null) return { error: `レート「${code}」は 0 より大きい数値にしてください。` };
    const key = code.toUpperCase();
    if (seen.has(key)) return { error: `通貨「${code}」が重複しています。` };
    seen.add(key);
    rates[code] = rate;
  }
  return { rates };
}

/** Empty threshold / name before saving (Rust validates everything else). */
export function findDraftError(drafts: AchievementDef[]): { id: string; message: string } | null {
  for (const a of drafts) {
    if (!a.name.trim()) return { id: a.id, message: '名前が空の実績があります。' };
    if (a.condition && (a.condition.threshold === null || !Number.isFinite(a.condition.threshold))) {
      return { id: a.id, message: `「${a.name}」のしきい値を入力してください。` };
    }
  }
  return null;
}
