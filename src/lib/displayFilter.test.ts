import assert from 'node:assert/strict';
import test from 'node:test';
import {
  defaultDisplayFilter, isDefaultFilter, activeFilterCount, matchesDisplayFilter,
  normalizeDisplayFilter, prependCapped, clampZoom, stepZoom, normalizeZoom, zoomActionForKey,
} from './displayFilter.ts';
import type { DisplayFilter, FilterableMessage } from './displayFilter.ts';

const noRoles = { broadcaster: false, moderator: false, member: false, subscriber: false, vip: false };
function msg(over: Partial<FilterableMessage> & { roles?: Partial<typeof noRoles> } = {}): FilterableMessage {
  const { roles, ...rest } = over;
  return { kind: 'normal', author: { roles: { ...noRoles, ...roles } }, ...rest };
}
const first = { isFirstEver: true };
const returning = { isFirstEver: false };
const f = (p: Partial<DisplayFilter>): DisplayFilter => ({ ...defaultDisplayFilter(), ...p });

test('default filter passes everything and is detected as default', () => {
  assert.equal(isDefaultFilter(defaultDisplayFilter()), true);
  assert.equal(matchesDisplayFilter(msg(), defaultDisplayFilter()), true);
  assert.equal(matchesDisplayFilter(msg({ kind: 'system' }), defaultDisplayFilter()), true);
  assert.equal(activeFilterCount(defaultDisplayFilter()), 0);
});

test('visit filter: first / returning / all x listener presence', () => {
  assert.equal(matchesDisplayFilter(msg({ listener: first }), f({ visit: 'first' })), true);
  assert.equal(matchesDisplayFilter(msg({ listener: returning }), f({ visit: 'first' })), false);
  assert.equal(matchesDisplayFilter(msg(), f({ visit: 'first' })), false);
  assert.equal(matchesDisplayFilter(msg({ listener: returning }), f({ visit: 'returning' })), true);
  assert.equal(matchesDisplayFilter(msg({ listener: first }), f({ visit: 'returning' })), false);
  assert.equal(matchesDisplayFilter(msg(), f({ visit: 'returning' })), false);
  assert.equal(matchesDisplayFilter(msg(), f({ visit: 'all' })), true);
  assert.equal(matchesDisplayFilter(msg({ listener: null }), f({ visit: 'returning' })), false);
});

test('kinds: empty = all, non-empty = OR', () => {
  assert.equal(matchesDisplayFilter(msg({ kind: 'bits' }), f({ kinds: [] })), true);
  assert.equal(matchesDisplayFilter(msg({ kind: 'bits' }), f({ kinds: ['superChat', 'bits'] })), true);
  assert.equal(matchesDisplayFilter(msg({ kind: 'normal' }), f({ kinds: ['superChat', 'bits'] })), false);
});

test('roles: empty = all, non-empty = any selected role', () => {
  assert.equal(matchesDisplayFilter(msg(), f({ roles: [] })), true);
  assert.equal(matchesDisplayFilter(msg({ roles: { vip: true } }), f({ roles: ['moderator', 'vip'] })), true);
  assert.equal(matchesDisplayFilter(msg({ roles: { member: true } }), f({ roles: ['moderator', 'vip'] })), false);
  assert.equal(matchesDisplayFilter(msg(), f({ roles: ['broadcaster'] })), false);
});

test('groups are ANDed', () => {
  const flt = f({ visit: 'first', kinds: ['normal'], roles: ['member'] });
  assert.equal(activeFilterCount(flt), 3);
  assert.equal(isDefaultFilter(flt), false);
  assert.equal(matchesDisplayFilter(msg({ listener: first, roles: { member: true } }), flt), true);
  assert.equal(matchesDisplayFilter(msg({ listener: first }), flt), false);
  assert.equal(matchesDisplayFilter(msg({ listener: returning, roles: { member: true } }), flt), false);
  assert.equal(matchesDisplayFilter(msg({ kind: 'gift', listener: first, roles: { member: true } }), flt), false);
});

test('normalizeDisplayFilter: corrupted storage falls back to defaults', () => {
  const d = defaultDisplayFilter();
  for (const bad of [null, undefined, 42, 'x', [], true]) assert.deepEqual(normalizeDisplayFilter(bad), d);
  assert.deepEqual(normalizeDisplayFilter({}), d);
  assert.deepEqual(normalizeDisplayFilter({ visit: 'nope', kinds: 'bits', roles: { a: 1 } }), d);
  assert.deepEqual(
    normalizeDisplayFilter({ visit: 'first', kinds: ['bits', 'bogus', 3, 'bits'], roles: ['vip', 'admin'] }),
    { visit: 'first', kinds: ['bits'], roles: ['vip'] },
  );
  assert.deepEqual(normalizeDisplayFilter({ visit: 'returning', kinds: [], roles: [] }), f({ visit: 'returning' }));
});

test('clampZoom: bounds, rounding, non-finite', () => {
  assert.equal(clampZoom(0.5), 0.5);
  assert.equal(clampZoom(2.0), 2.0);
  assert.equal(clampZoom(0.3), 0.5);
  assert.equal(clampZoom(2.7), 2.0);
  assert.equal(clampZoom(1.1000000001), 1.1);
  assert.equal(clampZoom(0.1 + 0.2), 0.5);
  assert.equal(clampZoom(NaN), 1);
  assert.equal(clampZoom(Infinity), 1);
});

test('stepZoom: steps without float drift and stops at limits', () => {
  let z = 1.0;
  for (let i = 0; i < 3; i++) z = stepZoom(z, 1);
  assert.equal(z, 1.3);
  assert.equal(stepZoom(2.0, 1), 2.0);
  assert.equal(stepZoom(0.5, -1), 0.5);
  assert.equal(stepZoom(1.1000000001, -1), 1.0);
});

test('normalizeZoom: stored values', () => {
  assert.equal(normalizeZoom(1.25), 1.3);
  assert.equal(normalizeZoom('1.5'), 1);
  assert.equal(normalizeZoom(null), 1);
  assert.equal(normalizeZoom(99), 2);
});

test('zoomActionForKey: JP keyboard and numpad', () => {
  assert.equal(zoomActionForKey('+', 'Semicolon'), 'in');
  assert.equal(zoomActionForKey(';', 'Semicolon'), 'in');
  assert.equal(zoomActionForKey('=', 'Equal'), 'in');
  assert.equal(zoomActionForKey('Add', 'NumpadAdd'), 'in');
  assert.equal(zoomActionForKey('-', 'Minus'), 'out');
  assert.equal(zoomActionForKey('Subtract', 'NumpadSubtract'), 'out');
  assert.equal(zoomActionForKey('0', 'Digit0'), 'reset');
  assert.equal(zoomActionForKey('0', 'Numpad0'), 'reset');
  assert.equal(zoomActionForKey('a', 'KeyA'), null);
});

test('prependCapped: newest first, capped, no-op returns same list', () => {
  const pick = (n: number) => (n % 2 === 0 ? n : null);
  const base = [10, 8];
  assert.equal(prependCapped(base, [1, 3], pick, 5), base);
  assert.deepEqual(prependCapped(base, [1, 2, 3, 4], pick, 5), [4, 2, 10, 8]);
  assert.deepEqual(prependCapped(base, [2, 4, 6, 8], pick, 3), [8, 6, 4]);
  assert.deepEqual(prependCapped([], [2, 4], pick, 50), [4, 2]);
  assert.deepEqual(prependCapped(base, [2], pick, 2), [2, 10]);
});
