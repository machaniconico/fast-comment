import assert from 'node:assert/strict';
import test from 'node:test';
import { applyModerationEvents, carryModeration, moderationMatcher, RecentModerationLog, RECENT_MODERATION_MAX, RECENT_MODERATION_TTL_MS } from './moderationEvents.ts';
import type { ModerationEvent, UiChatMessage } from './types.ts';

function msg(id: string, authorId: string, over: Partial<UiChatMessage> = {}): UiChatMessage {
  return {
    id, platform: 'twitch', channel: 'ch', kind: 'normal', fragments: [], timestampMs: 0,
    author: { id: authorId, name: authorId, badges: [], roles: { broadcaster: false, moderator: false, member: false, subscriber: false, vip: false } },
    ...over,
  } as UiChatMessage;
}

const del = (messageId: string, over: Partial<ModerationEvent> = {}): ModerationEvent =>
  ({ type: 'messageDeleted', platform: 'twitch', channel: 'ch', messageId, ...over }) as ModerationEvent;
const user = (authorId: string, durationSecs: number | null, over: Partial<ModerationEvent> = {}): ModerationEvent =>
  ({ type: 'userCleared', platform: 'twitch', channel: 'ch', authorId, durationSecs, certain: true, beforeMs: null, ...over }) as ModerationEvent;
const clear = (beforeMs: number | null = null): ModerationEvent => ({ type: 'chatCleared', platform: 'twitch', channel: 'ch', beforeMs });

test('messageDeleted marks only that id', () => {
  const entries = [msg('a', 'u1'), msg('b', 'u1')];
  const out = applyModerationEvents(entries, [del('b')]);
  assert.equal(out[0], entries[0]);
  assert.equal(out[1].removed, 'deleted');
});

test('userCleared maps duration to timeout / null to banned for all author messages', () => {
  const entries = [msg('a', 'u1'), msg('b', 'u2'), msg('c', 'u1')];
  const t = applyModerationEvents(entries, [user('u1', 600)]);
  assert.deepEqual(t.map((m) => m.removed), ['timeout', undefined, 'timeout']);
  const b = applyModerationEvents(t, [user('u1', null)]);
  assert.deepEqual(b.map((m) => m.removed), ['banned', undefined, 'banned']);
  // duration 0 (unknown-length timeout) is still a timeout, not a ban.
  assert.equal(applyModerationEvents([msg('a', 'u1')], [user('u1', 0)])[0].removed, 'timeout');
});

test('chatCleared marks the whole channel as deleted, but keeps stronger marks', () => {
  const entries = [msg('a', 'u1', { removed: 'banned' }), msg('b', 'u2'), msg('c', 'u3', { channel: 'other' })];
  const out = applyModerationEvents(entries, [clear()]);
  assert.deepEqual(out.map((m) => m.removed), ['banned', 'deleted', undefined]);
  // A later per-message delete does not downgrade an existing ban mark either.
  assert.equal(applyModerationEvents([msg('a', 'u1', { removed: 'banned' })], [del('a')])[0].removed, 'banned');
});

test('platform and channel must match (id spaces differ per platform)', () => {
  const entries = [msg('a', 'u1'), msg('a', 'u1', { platform: 'youtube' }), msg('a', 'u1', { channel: 'ch2' })];
  const out = applyModerationEvents(entries, [del('a'), user('u1', null, { platform: 'x' } as Partial<ModerationEvent>)]);
  assert.deepEqual(out.map((m) => m.removed), ['deleted', undefined, undefined]);
});

test('unknown ids, empty ids and empty events leave the array untouched', () => {
  const entries = [msg('a', 'u1')];
  assert.equal(applyModerationEvents(entries, []), entries);
  assert.equal(applyModerationEvents(entries, [del('missing'), del(''), user('', null)]), entries);
  assert.equal(moderationMatcher([]), null);
});

test('idempotent: echo of the same events changes nothing', () => {
  const entries = [msg('a', 'u1'), msg('b', 'u2')];
  const events = [del('b'), user('u1', 60)];
  const once = applyModerationEvents(entries, events);
  const twice = applyModerationEvents(once, events);
  assert.equal(twice, once);
  // App-side mark (markRemoved) followed by platform echo is also a no-op.
  const appMarked = [msg('b', 'u2', { removed: 'deleted' })];
  assert.equal(applyModerationEvents(appMarked, [del('b')]), appMarked);
});

test('matcher.hits identifies targets for pin removal', () => {
  const m = moderationMatcher([del('b')])!;
  assert.equal(m.hits(msg('b', 'u')), true);
  assert.equal(m.hits(msg('a', 'u')), false);
});

test('same-flush boundary: chatCleared / userCleared skip comments newer than beforeMs', () => {
  // old comment (ts 100) → CLEARCHAT (ts 150) → new comment (ts 200), all in one flush.
  const entries = [msg('old', 'u1', { timestampMs: 100 }), msg('same', 'u1', { timestampMs: 150 }), msg('new', 'u1', { timestampMs: 200 })];
  assert.deepEqual(applyModerationEvents(entries, [clear(150)]).map((m) => m.removed), ['deleted', 'deleted', undefined]);
  assert.deepEqual(applyModerationEvents(entries, [user('u1', 60, { beforeMs: 150 } as Partial<ModerationEvent>)]).map((m) => m.removed), ['timeout', 'timeout', undefined]);
  // messageDeleted is id-addressed, no boundary.
  assert.equal(applyModerationEvents(entries, [del('new')])[2].removed, 'deleted');
});

test('uncertain userCleared (InnerTube echo) never overwrites ban/timeout, labels banned otherwise', () => {
  const uncertain = user('u1', null, { certain: false } as Partial<ModerationEvent>);
  // App timed out the user → InnerTube author-delete echo must not turn it into a ban.
  const timedOut = [msg('a', 'u1', { removed: 'timeout' })];
  assert.equal(applyModerationEvents(timedOut, [uncertain]), timedOut);
  // Same frame: certain timeout then uncertain echo.
  assert.equal(applyModerationEvents([msg('a', 'u1')], [user('u1', 600), uncertain])[0].removed, 'timeout');
  // No mark (or only 'deleted') → 'banned'.
  assert.equal(applyModerationEvents([msg('a', 'u1')], [uncertain])[0].removed, 'banned');
  assert.equal(applyModerationEvents([msg('a', 'u1', { removed: 'deleted' })], [uncertain])[0].removed, 'banned');
  // A certain permanent ban still upgrades a timeout.
  assert.equal(applyModerationEvents(timedOut, [user('u1', null)])[0].removed, 'banned');
});

test('RecentModerationLog applies to late-arriving comments, then expires by TTL', () => {
  const log = new RecentModerationLog();
  assert.equal(log.matcher(0), null);
  log.add([del('late'), user('u9', null), clear(150)], 1000);
  const m = log.matcher(1000 + RECENT_MODERATION_TTL_MS)!;
  assert.equal(m.mark(msg('late', 'x')).removed, 'deleted');
  assert.equal(m.mark(msg('z', 'u9')).removed, 'banned');
  assert.equal(m.mark(msg('y', 'u1', { timestampMs: 100 })).removed, 'deleted');
  assert.equal(m.mark(msg('w', 'u1', { timestampMs: 200 })).removed, undefined);
  // One ms past the TTL everything is gone.
  assert.equal(log.matcher(1001 + RECENT_MODERATION_TTL_MS), null);
  assert.equal(log.size, 0);
});

test('RecentModerationLog keeps only the newest max entries (oldest dropped first)', () => {
  assert.equal(RECENT_MODERATION_MAX, 512);
  const log = new RecentModerationLog(3, 60_000);
  log.add([del('a'), del('b')], 0);
  log.add([del('c'), del('d')], 1);
  assert.equal(log.size, 3);
  const m = log.matcher(2)!;
  assert.equal(m.hits(msg('a', 'u')), false);
  for (const id of ['b', 'c', 'd']) assert.equal(m.hits(msg(id, 'u')), true);
  // Partially expired: entries recorded at t=0 drop, t=1 stays.
  const t = new RecentModerationLog(10, 5);
  t.add([del('old')], 0);
  t.add([del('fresh')], 3);
  const mt = t.matcher(6)!;
  assert.equal(mt.hits(msg('old', 'u')), false);
  assert.equal(mt.hits(msg('fresh', 'u')), true);
  assert.equal(t.size, 1);
});

test('RecentModerationLog: per-entry expiry with a monotonic clock; backwards time expires', () => {
  const log = new RecentModerationLog(10, 100);
  log.add([del('first')], 50);
  log.add([del('second')], 0); // out of order (e.g. clock went back): judged on its own age
  const m = log.matcher(140)!;
  assert.equal(m.hits(msg('first', 'u')), true); // age 90
  assert.equal(m.hits(msg('second', 'u')), false); // age 140 > ttl, dropped even though it is not first
  assert.equal(log.size, 1);
  // Clock earlier than the record → negative age → expired, never kept forever.
  assert.equal(log.matcher(10), null);
  assert.equal(log.size, 0);
});

test('carryModeration: gift update keeps the previous mark and applies recent moderation', () => {
  const prev = msg('g', 'u1', { kind: 'gift', removed: 'banned' });
  const update = msg('g', 'u1', { kind: 'gift' });
  assert.equal(carryModeration(update, prev, null).removed, 'banned');
  // No previous mark, but the event reached the UI before the update.
  const late = moderationMatcher([del('g')]);
  assert.equal(carryModeration(update, msg('g', 'u1', { kind: 'gift' }), late).removed, 'deleted');
  // Unaffected update stays the same object.
  assert.equal(carryModeration(update, msg('g', 'u1'), null), update);
});
