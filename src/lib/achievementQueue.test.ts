import assert from 'node:assert/strict';
import test from 'node:test';
import { ACHIEVEMENT_QUEUE_MAX, AchievementQueue, achievementViewerLabel } from './achievementQueue.ts';
import type { AchievementUnlocked } from './types.ts';

const mk = (id: string, over: Partial<AchievementUnlocked> = {}): AchievementUnlocked => ({
  id, achievementId: 'a', name: 'n', description: '', tier: 'bronze', icon: '', subject: { kind: 'viewer', name: 'taro' },
  valueMilli: 0, thresholdMilli: 0, windowKey: 'lifetime', unlockedAtMs: 0, ...over,
});

test('first push shows immediately, rest wait in order', () => {
  const q = new AchievementQueue();
  q.push([mk('1'), mk('2'), mk('3')]);
  assert.equal(q.current?.id, '1');
  assert.equal(q.pending, 2);
  assert.equal(q.advance()?.id, '2');
  assert.equal(q.advance()?.id, '3');
  assert.equal(q.advance(), null);
  assert.equal(q.pending, 0);
});

test('push while showing appends to waiting', () => {
  const q = new AchievementQueue();
  q.push([mk('1')]);
  q.push([mk('2')]);
  assert.equal(q.current?.id, '1');
  assert.equal(q.pending, 1);
});

test('overflow drops oldest waiting and counts them', () => {
  const q = new AchievementQueue();
  q.push(Array.from({ length: ACHIEVEMENT_QUEUE_MAX + 4 }, (_, i) => mk(String(i))));
  assert.equal(q.pending, ACHIEVEMENT_QUEUE_MAX);
  assert.equal(q.dropped, 3);
  assert.equal(q.hidden, ACHIEVEMENT_QUEUE_MAX + 3);
  assert.equal(q.advance()?.id, '4');
});

test('viewer label: streamer, named, blank name', () => {
  assert.equal(achievementViewerLabel(mk('1', { subject: { kind: 'streamer' } })), '配信者');
  assert.equal(achievementViewerLabel(mk('1')), 'taro');
  assert.equal(achievementViewerLabel(mk('1', { subject: { kind: 'viewer', name: '  ' } })), '視聴者');
  assert.equal(achievementViewerLabel(mk('1', { subject: { kind: 'viewer' } })), '視聴者');
});
