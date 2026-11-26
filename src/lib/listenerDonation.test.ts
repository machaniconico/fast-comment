import { test } from 'node:test';
import assert from 'node:assert/strict';
import { hasDonationFrom, type ChatMessage } from './types.ts';

function gift(id: string, name: string, extra: Partial<ChatMessage> = {}): ChatMessage {
  return {
    id: 'm1',
    platformMessageId: false,
    platform: 'twitcasting',
    channel: 'c',
    author: { id, name, badges: [], roles: { broadcaster: false, moderator: false } },
    fragments: [],
    kind: 'gift',
    amount: { value: 100, currency: 'pt', rawText: '100pt' },
    timestampMs: 0,
    ...extra,
  } as ChatMessage;
}

test('著者 ID があるギフトは、その ID のリスナーの金額を変える', () => {
  assert.equal(hasDonationFrom([gift('u1', 'Alice')], 'twitcasting', 'u1'), true);
  assert.equal(hasDonationFrom([gift('u1', 'Alice')], 'twitcasting', 'Alice'), false);
  assert.equal(hasDonationFrom([gift('u1', 'Alice')], 'twitch', 'u1'), false);
});

test('著者 ID が空のギフトは、台帳と同じく名前のリスナーの金額を変える', () => {
  // listeners.rs は著者 ID が空のコメントを名前で数える。カードの authorId もその名前になる。
  assert.equal(hasDonationFrom([gift('', 'Alice')], 'twitcasting', 'Alice'), true);
  assert.equal(hasDonationFrom([gift('', 'Alice')], 'twitcasting', 'Bob'), false);
  assert.equal(hasDonationFrom([gift('', 'Alice')], 'twitcasting', ''), false);
});

test('金額の無いコメントと、ギフト以外の種別は金額を変えない', () => {
  assert.equal(hasDonationFrom([gift('u1', 'Alice', { amount: undefined })], 'twitcasting', 'u1'), false);
  assert.equal(hasDonationFrom([gift('u1', 'Alice', { kind: 'normal' })], 'twitcasting', 'u1'), false);
  assert.equal(hasDonationFrom([], 'twitcasting', 'u1'), false);
});
