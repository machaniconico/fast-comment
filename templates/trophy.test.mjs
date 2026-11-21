import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const source = readFileSync(new URL('./trophy/app.js', import.meta.url), 'utf8').replace(/\r\n/g, '\n');
const style = readFileSync(new URL('./trophy/style.css', import.meta.url), 'utf8');
const BEGIN = '  // ---- fc-trophy:begin ----';
const END = '  // ---- fc-trophy:end ----';

function block() {
  const start = source.indexOf(BEGIN);
  const end = source.indexOf(END);
  assert.ok(start >= 0 && end > start, 'trophy logic block markers present');
  return source.slice(start, end + END.length);
}

function load() {
  const factory = new Function(
    `${block()}\nreturn { TROPHY_DEFAULT_DURATION_MS, trophyDurationMs, trophyFrameUnlocks, trophyViewerLabel, trophyCreateQueue };`,
  );
  return factory();
}

test('duration: 5 s by default and only positive numbers override it', () => {
  const { TROPHY_DEFAULT_DURATION_MS, trophyDurationMs } = load();
  assert.equal(TROPHY_DEFAULT_DURATION_MS, 5000);

  for (const raw of [null, undefined, '', '   ', '0', '-1', '-0.1', 'abc', 'NaN', 'Infinity', '-Infinity']) {
    assert.equal(trophyDurationMs(raw), 5000, String(raw));
  }
  assert.equal(trophyDurationMs('5'), 5000);
  assert.equal(trophyDurationMs(' 7 '), 7000);
  assert.equal(trophyDurationMs('2.5'), 2500);
  assert.equal(trophyDurationMs('0.001'), 1);
});

test('only {"achievements":[…]} frames are accepted; chat is ignored', () => {
  const { trophyFrameUnlocks } = load();
  const a = { id: 'a' };
  const b = { id: 'b' };

  assert.deepEqual(trophyFrameUnlocks([{ id: 'chat' }]), []);
  assert.deepEqual(trophyFrameUnlocks({ id: 'chat' }), []);
  assert.deepEqual(trophyFrameUnlocks({ moderation: [] }), []);
  assert.deepEqual(trophyFrameUnlocks(null), []);
  assert.deepEqual(trophyFrameUnlocks({ achievements: 'nope' }), []);
  assert.deepEqual(trophyFrameUnlocks({ achievements: [a, null, 3, b] }), [a, b]);
  assert.deepEqual(trophyFrameUnlocks({ achievements: [] }), []);
});

test('queue keeps arrival order across frames and shows one item at a time', () => {
  const { trophyCreateQueue } = load();
  const queue = trophyCreateQueue();

  assert.equal(queue.shift(), null);
  queue.enqueue([{ id: '1' }, { id: '2' }], 5000);
  queue.enqueue([{ id: '3' }], 2500);
  assert.equal(queue.size(), 3);

  assert.deepEqual(queue.shift(), { unlock: { id: '1' }, durationMs: 5000 });
  assert.equal(queue.size(), 2);
  assert.deepEqual(queue.shift(), { unlock: { id: '2' }, durationMs: 5000 });
  assert.deepEqual(queue.shift(), { unlock: { id: '3' }, durationMs: 2500 });
  assert.equal(queue.shift(), null);
});

test('viewer label: streamer, named viewer, blank name', () => {
  const { trophyViewerLabel } = load();
  assert.equal(trophyViewerLabel({ kind: 'streamer', name: 'ignored' }), '配信者');
  assert.equal(trophyViewerLabel({ kind: 'viewer', name: ' taro ' }), 'taro');
  assert.equal(trophyViewerLabel({ kind: 'viewer', name: '   ' }), '視聴者');
  assert.equal(trophyViewerLabel({ kind: 'viewer' }), '視聴者');
  assert.equal(trophyViewerLabel(null), '視聴者');
});

test('opt-in, no innerHTML, and no external assets', () => {
  assert.match(source, /url\.searchParams\.set\('achievementEvents', '1'\)/);
  assert.match(source, /params\.get\('duration'\)/);
  assert.doesNotMatch(source, /innerHTML/);
  assert.doesNotMatch(source, /document\.write/);
  assert.doesNotMatch(style, /@import|url\(\s*['"]?https?:/);
});

test('duration: huge values are capped at 1 hour so setTimeout does not fire at once', () => {
  const { trophyDurationMs } = load();
  for (const raw of ['3000000', '1e10', '3601']) {
    assert.equal(trophyDurationMs(raw), 3_600_000, raw);
  }
  assert.equal(trophyDurationMs('3600'), 3_600_000);
});

test('queue keeps only the newest 50 when a burst arrives', () => {
  const { trophyCreateQueue } = load();
  const queue = trophyCreateQueue();
  queue.enqueue(Array.from({ length: 60 }, (_, i) => ({ id: `u${i}` })), 5000);
  assert.equal(queue.size(), 50);
  assert.equal(queue.shift().unlock.id, 'u10', 'the oldest 10 are dropped');
});
