import assert from 'node:assert/strict';
import test from 'node:test';
import {
  moderationAvailability, formatTimeoutLabel, toModerationError, TIMEOUT_PRESETS, QUICK_TIMEOUT_SECS,
  DELETE_UNSUPPORTED_REASON, TWITCH_NOT_CONNECTED_REASON, TWITCH_DELETE_UNSUPPORTED_REASON, focusReturnCandidates,
} from './moderationActions.ts';
import type { ModerationTargetMessage } from './moderationActions.ts';

const on = { youtubeConnected: true };
const off = { youtubeConnected: false };
function m(over: Partial<ModerationTargetMessage> & { roles?: { broadcaster?: boolean; moderator?: boolean } } = {}): ModerationTargetMessage {
  const { roles, ...rest } = over;
  return {
    platform: 'youtube', kind: 'normal', id: 'LCC.abc', nativeId: true,
    author: { id: 'UC1', roles: { broadcaster: false, moderator: false, ...roles } },
    ...rest,
  };
}

test('x / niconico / system are hidden', () => {
  assert.equal(moderationAvailability(m({ platform: 'x' }), on).visible, false);
  assert.equal(moderationAvailability(m({ platform: 'niconico' }), on).visible, false);
  assert.equal(moderationAvailability(m({ kind: 'system' }), on).visible, false);
  assert.equal(moderationAvailability(m({ platform: 'mystery' }), on).visible, false);
});

test('platform string is case/space tolerant', () => {
  assert.equal(moderationAvailability(m({ platform: ' YouTube ' }), on).timeout.enabled, true);
});

test('not logged in disables everything with a reason', () => {
  const a = moderationAvailability(m(), off);
  assert.equal(a.visible, true);
  for (const k of ['delete', 'timeout', 'ban'] as const) {
    assert.equal(a[k].enabled, false);
    assert.ok(a[k].reason);
  }
});

test('broadcaster / moderator targets are disabled', () => {
  assert.equal(moderationAvailability(m({ roles: { broadcaster: true } }), on).ban.enabled, false);
  assert.equal(moderationAvailability(m({ roles: { moderator: true } }), on).timeout.enabled, false);
});

test('missing author id is disabled', () => {
  const a = moderationAvailability(m({ author: { id: '', roles: { broadcaster: false, moderator: false } } }), on);
  assert.equal(a.ban.enabled, false);
});

test('delete requires nativeId and LCC. prefix', () => {
  assert.equal(moderationAvailability(m(), on).delete.enabled, true);
  const noNative = moderationAvailability(m({ nativeId: false }), on);
  assert.equal(noNative.delete.enabled, false);
  assert.equal(noNative.delete.reason, DELETE_UNSUPPORTED_REASON);
  assert.equal(noNative.timeout.enabled, true);
  assert.equal(moderationAvailability(m({ nativeId: undefined }), on).delete.enabled, false);
  const inner = moderationAvailability(m({ id: 'ChwKGkNL', nativeId: true }), on);
  assert.equal(inner.delete.enabled, false);
  assert.equal(inner.ban.enabled, true);
});

const tw = { youtubeConnected: false, twitchConnected: true };
function t(over: Parameters<typeof m>[0] = {}) {
  return m({ platform: 'twitch', id: 'uuid-1', nativeId: true, ...over });
}

test('twitch not connected: everything disabled with login hint (youtube login does not help)', () => {
  for (const auth of [off, on, { youtubeConnected: true, twitchConnected: false }]) {
    const a = moderationAvailability(t(), auth);
    assert.equal(a.visible, true);
    assert.equal(a.delete.enabled || a.timeout.enabled || a.ban.enabled || a.unban.enabled, false);
    assert.equal(a.ban.reason, TWITCH_NOT_CONNECTED_REASON);
  }
});

test('twitch connected: timeout / ban / unban enabled, delete needs nativeId', () => {
  const a = moderationAvailability(t(), tw);
  assert.equal(a.delete.enabled, true);
  assert.equal(a.timeout.enabled, true);
  assert.equal(a.ban.enabled, true);
  assert.equal(a.unban.enabled, true);
  const noNative = moderationAvailability(t({ nativeId: false }), tw);
  assert.equal(noNative.delete.enabled, false);
  assert.equal(noNative.delete.reason, TWITCH_DELETE_UNSUPPORTED_REASON);
  assert.equal(noNative.unban.enabled, true);
  assert.equal(moderationAvailability(t({ nativeId: undefined }), tw).delete.enabled, false);
  assert.equal(moderationAvailability(t({ id: '' }), tw).delete.enabled, false);
});

test('twitch: broadcaster / moderator / unknown author are disabled; youtube unaffected by twitch login', () => {
  assert.equal(moderationAvailability(t({ roles: { moderator: true } }), tw).unban.enabled, false);
  assert.equal(moderationAvailability(t({ roles: { broadcaster: true } }), tw).ban.enabled, false);
  assert.equal(moderationAvailability(t({ author: { id: '', roles: { broadcaster: false, moderator: false } } }), tw).ban.enabled, false);
  assert.equal(moderationAvailability(m(), tw).timeout.enabled, false);
  assert.equal(moderationAvailability(m(), { youtubeConnected: true, twitchConnected: false }).timeout.enabled, true);
});

test('presets and labels', () => {
  assert.deepEqual([...TIMEOUT_PRESETS], [60, 600, 3600, 86400]);
  assert.equal(QUICK_TIMEOUT_SECS, 600);
  assert.deepEqual(TIMEOUT_PRESETS.map(formatTimeoutLabel), ['1分', '10分', '1時間', '24時間']);
});

test('toModerationError handles object, string and junk', () => {
  assert.deepEqual(toModerationError({ code: 'notFound', message: 'x' }), { code: 'notFound', message: 'x' });
  assert.deepEqual(toModerationError({ code: 'weird', message: 'x' }), { code: 'other', message: 'x' });
  assert.equal(toModerationError('boom').message, 'boom');
  assert.equal(toModerationError('boom').code, 'other');
  assert.equal(toModerationError(null).code, 'other');
  assert.equal(toModerationError({}).message.length > 0, true);
});

test('focus return skips missing, detached and disabled targets in priority order', () => {
  const menuBtn = { id: 'menu', isConnected: true };
  const detached = { id: 'detached', isConnected: false };
  const disabled = { id: 'disabled', isConnected: true, disabled: true };
  const list = { id: 'list', isConnected: true };
  assert.deepEqual(focusReturnCandidates(menuBtn, list), [menuBtn, list]);
  assert.deepEqual(focusReturnCandidates(detached, list), [list]);
  assert.deepEqual(focusReturnCandidates(disabled, list), [list]);
  assert.deepEqual(focusReturnCandidates(null, undefined, list), [list]);
  assert.deepEqual(focusReturnCandidates(detached, null), []);
  assert.deepEqual(focusReturnCandidates({ id: 'enabled', isConnected: true, disabled: false }), [
    { id: 'enabled', isConnected: true, disabled: false },
  ]);
});
