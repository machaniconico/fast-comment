import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  canDelete, canDuplicate, checkRateRows, checkSymbolRows, symbolRowsOf, coerceCondition, findDraftError, invalidFieldMessage, withSubject, currencyOptions, DAY_MS, metricOptions, parseRate, platformOptions, ratesNotice, ratesStale,
  trophyViews, windowOptions,
} from './achievementsUi.ts';
import type { AchievementDef, AchievementSchema, ListenerTrophy } from './types.ts';

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

const def = (over: Partial<AchievementDef> = {}): AchievementDef => ({
  id: 'a', name: 'n', description: '', icon: '', tier: 'bronze', subject: 'viewer', enabled: true, builtinId: null, edited: false,
  condition: { window: 'lifetime', metric: 'streamsAttended', threshold: 1, currency: { mode: 'none' }, platforms: [] },
  announce: { tts: false }, ...over,
});

test('editor changes the subject and moves the metric to an allowed one', () => {
  const a = withSubject(schema, def(), 'streamer');
  assert.equal(a.subject, 'streamer');
  assert.equal(a.condition?.metric, 'comments');
  const keep = withSubject(schema, def({ condition: { ...def().condition!, metric: 'comments' } }), 'streamer');
  assert.equal(keep.condition?.metric, 'comments');
  const platinum = def({ tier: 'platinum', condition: null });
  assert.equal(withSubject(schema, platinum, 'streamer'), platinum);
});

test('duplicate and delete are gated by tier and origin', () => {
  assert.equal(canDuplicate(def()), true);
  assert.equal(canDuplicate(def({ tier: 'platinum', condition: null })), false);
  assert.equal(canDelete(def()), true);
  assert.equal(canDelete(def({ builtinId: 'viewer-first-comment' })), false);
  assert.equal(canDelete(def({ tier: 'platinum', condition: null })), false);
});

test('rate rows reject empty codes, bad values and duplicates before saving', () => {
  assert.deepEqual(checkRateRows([{ code: 'USD', text: '150.5' }, { code: ' ', text: '' }]), { rates: { USD: 150.5 } });
  assert.ok('error' in checkRateRows([{ code: '', text: '150' }]));
  assert.ok('error' in checkRateRows([{ code: 'USD', text: '' }]));
  assert.ok('error' in checkRateRows([{ code: 'USD', text: '1' }, { code: 'usd', text: '2' }]));
});

test('empty threshold and empty name are caught before saving', () => {
  assert.equal(findDraftError([def()]), null);
  const empty = def({ id: 'x', condition: { ...def().condition!, threshold: null } });
  assert.equal(findDraftError([def(), empty])?.id, 'x');
  assert.equal(findDraftError([def({ id: 'y', name: ' ' })])?.id, 'y');
});

test('rate errors name the row, never the typed code', () => {
  const secret = 'https://example.invalid/hook/TEST_ONLY';
  for (const rows of [
    [{ code: secret, text: '0' }],
    [{ code: 'USD', text: '1' }, { code: secret, text: '' }],
    [{ code: secret, text: '1' }, { code: secret.toLowerCase(), text: '2' }],
  ]) {
    const r = checkRateRows(rows);
    assert.ok('error' in r);
    assert.ok(!r.error.includes('TEST_ONLY') && !r.error.toLowerCase().includes('test_only'), r.error);
    assert.match(r.error, /\d+ 行目/);
  }
});

test('the enable checkbox is locked while saving', () => {
  const src = readFileSync(new URL('./components/AchievementsSettings.svelte', import.meta.url), 'utf8');
  const line = src.split('\n').find((l) => l.includes('bind:checked={enabled}'));
  assert.ok(line);
  assert.match(line, /disabled=\{busy\}/);
});

test('draft errors point at the position, never the typed name', () => {
  const secret = 'https://example.invalid/hook/TEST_ONLY';
  const drafts = [
    def({ id: 's', subject: 'streamer' }),
    def({ id: 'v1' }),
    def({ id: 'v2', name: secret, condition: { ...def().condition!, threshold: null } }),
  ];
  const err = findDraftError(drafts);
  assert.equal(err?.id, 'v2');
  assert.ok(!err!.message.includes('TEST_ONLY'), err!.message);
  assert.match(err!.message, /視聴者の 2 番目/);
  const invalid = invalidFieldMessage(drafts, 'v2', 'currency');
  assert.ok(!invalid.includes('TEST_ONLY'), invalid);
  assert.match(invalid, /視聴者の 2 番目の実績の通貨/);
  assert.ok(!invalidFieldMessage(drafts, secret, 'name').includes('TEST_ONLY'));
});

test('symbol override rows round trip with the map and drop fully blank rows', () => {
  assert.deepEqual(symbolRowsOf({ 'R$': 'BRL', '฿': 'THB' }), [{ symbol: 'R$', code: 'BRL' }, { symbol: '฿', code: 'THB' }]);
  assert.deepEqual(symbolRowsOf({}), []);
  const ok = checkSymbolRows([{ symbol: ' R$ ', code: ' BRL ' }, { symbol: ' ', code: '' }, { symbol: '฿', code: 'THB' }]);
  assert.deepEqual(ok, { overrides: { 'R$': 'BRL', '฿': 'THB' } });
  assert.deepEqual(checkSymbolRows([]), { overrides: {} });
});

test('symbol override rows reject empty sides and trimmed duplicates without echoing input', () => {
  const secret = 'https://example.test/?token=abc';
  const empty = checkSymbolRows([{ symbol: 'R$', code: 'BRL' }, { symbol: '', code: 'USD' }]);
  assert.ok('error' in empty && empty.error.includes('2 行目') && empty.error.includes('記号'));
  const noCode = checkSymbolRows([{ symbol: secret, code: ' ' }]);
  assert.ok('error' in noCode && noCode.error.includes('1 行目') && !noCode.error.includes(secret));
  const dup = checkSymbolRows([{ symbol: secret, code: 'USD' }, { symbol: `${secret} `, code: 'EUR' }]);
  assert.ok('error' in dup && dup.error.includes('2 行目') && !dup.error.includes(secret));
});
