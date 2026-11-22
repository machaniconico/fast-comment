import assert from 'node:assert/strict';
import test from 'node:test';
import {
  coerceCondition, currencyOptions, DAY_MS, metricOptions, parseRate, platformOptions, ratesNotice, ratesStale,
  trophyViews, windowOptions,
} from './achievementsUi.ts';
import type { AchievementSchema, ListenerTrophy } from './types.ts';

const ALL = ['twitch', 'youtube', 'x', 'niconico', 'twitcasting'] as const;
const schema: AchievementSchema = {
  metrics: [
    { metric: 'comments', subjects: ['viewer', 'streamer'], windows: ['lifetime', 'perStream'], currencyModes: ['none'], platforms: [...ALL] },
    { metric: 'donationAmount', subjects: ['viewer', 'streamer'], windows: ['lifetime', 'perStream'], currencyModes: ['jpyConverted', 'only'], platforms: ['youtube', 'twitch', 'niconico'] },
    { metric: 'streamsAttended', subjects: ['viewer'], windows: ['lifetime', 'perStream'], currencyModes: ['none'], platforms: [...ALL] },
    { metric: 'peakConcurrentViewers', subjects: ['streamer'], windows: ['perStream'], currencyModes: ['none'], platforms: [...ALL] },
  ],
};
const disabled = (opts: { value: string; disabled: boolean }[]) => opts.filter((o) => o.disabled).map((o) => o.value);

test('editor_disables_invalid_combinations_from_schema', () => {
  assert.deepEqual(disabled(metricOptions(schema, 'viewer', [])), ['peakConcurrentViewers']);
  assert.deepEqual(disabled(metricOptions(schema, 'streamer', [])), ['streamsAttended']);
  assert.deepEqual(disabled(windowOptions(schema, 'peakConcurrentViewers')), ['lifetime']);
  assert.deepEqual(disabled(currencyOptions(schema, 'comments')), ['jpyConverted', 'only']);
  assert.deepEqual(disabled(currencyOptions(schema, 'donationAmount')), ['none']);
  assert.deepEqual(disabled(platformOptions(schema, 'donationAmount')), ['x', 'twitcasting']);
  // No-amount platforms cannot select the amount metric.
  assert.ok(disabled(metricOptions(schema, 'viewer', ['x', 'twitcasting'])).includes('donationAmount'));
  assert.ok(!disabled(metricOptions(schema, 'viewer', ['x', 'youtube'])).includes('donationAmount'));
  assert.ok(!disabled(metricOptions(schema, 'viewer', [])).includes('donationAmount'));
});

test('editor coerces window, currency and platforms when the metric changes', () => {
  const base = { window: 'lifetime' as const, metric: 'peakConcurrentViewers' as const, threshold: 1, currency: { mode: 'jpyConverted' as const }, platforms: ['youtube' as const] };
  const c = coerceCondition(schema, base);
  assert.equal(c.window, 'perStream');
  assert.deepEqual(c.currency, { mode: 'none' });
  const d = coerceCondition(schema, { ...base, metric: 'donationAmount', currency: { mode: 'none' }, platforms: ['x', 'twitch'] });
  assert.deepEqual(d.currency, { mode: 'jpyConverted' });
  assert.deepEqual(d.platforms, ['twitch']);
});

test('rate_table_warns_after_90_days', () => {
  const now = 1_800_000_000_000;
  assert.equal(ratesStale(now - 90 * DAY_MS, now), false);
  assert.equal(ratesStale(now - 90 * DAY_MS - 1, now), true);
  assert.equal(ratesStale(null, now), true);
  assert.equal(ratesNotice(now - DAY_MS, now, 'UTC').warn, false);
  const old = ratesNotice(now - 100 * DAY_MS, now, 'UTC');
  assert.equal(old.warn, true);
  assert.match(old.text, /最終更新日: \d{4}\/\d{2}\/\d{2}/);
  assert.match(old.text, /100 日前/);
});

test('parseRate accepts only positive finite numbers', () => {
  assert.equal(parseRate('150.5'), 150.5);
  for (const bad of ['', ' ', '0', '-1', 'abc', 'Infinity', 'NaN']) assert.equal(parseRate(bad), null);
});

test('listener_detail_shows_earned_trophies', () => {
  const t = (id: string, at: number, windowKey = '*', tier: ListenerTrophy['tier'] = 'bronze'): ListenerTrophy =>
    ({ achievementId: id, name: id, icon: '🏆', tier, windowKey, unlockedAtMs: at });
  const views = trophyViews([t('a', Date.UTC(2026, 10, 1, 3, 4)), t('b', Date.UTC(2026, 10, 2, 5, 6), 'twitch:1', 'gold')], 'UTC');
  assert.deepEqual(views.map((v) => v.name), ['b', 'a']);
  assert.equal(views[0].tierLabel, 'ゴールド');
  assert.equal(views[0].scope, '配信ごと');
  assert.equal(views[1].scope, '');
  assert.match(views[1].unlockedAt, /2026\/11\/01 03:04/);
  assert.deepEqual(trophyViews([]), []);
});
