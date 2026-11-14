import assert from 'node:assert/strict';
import test from 'node:test';
import {
  canSelectApiMode,
  errorText,
  isYoutubeChannelIdentifier,
  ownStreamBlockedReason,
  parseCallbackPort,
  sanitizeXPostNumbers,
  testPostPreview,
} from './xAutopost.ts';

test('YouTube channel forms match the Rust eligibility rule', () => {
  // Same cases as coordinator::tests::youtube_eligibility_matches_supported_channel_forms.
  for (const id of [
    '@mine',
    'youtube.com/@mine/live',
    'https://WWW.YOUTUBE.COM/@mine',
    'UC1234567890123456789012',
    'https://youtube.com/channel/UC1234567890123456789012/live',
    '  @mine  ',
    'https://www.youtube.com/@foo',
  ]) {
    assert.equal(isYoutubeChannelIdentifier(id), true, id);
  }
  for (const id of [
    '',
    '@',
    'UCvideo12345',
    'dQw4w9WgXcQ',
    'https://youtube.com/watch?v=id',
    'https://youtube.com/live/id',
    'https://youtube.com/channel/UCinvalid',
    'https://example.com/youtube.com/@mine',
    'https://youtube.com/c/mine',
  ]) {
    assert.equal(isYoutubeChannelIdentifier(id), false, id);
  }
});

test('own-stream mark is blocked only for X rows and YouTube video rows', () => {
  assert.notEqual(ownStreamBlockedReason({ platform: 'x', identifier: 'foo' }), null);
  assert.notEqual(ownStreamBlockedReason({ platform: 'youtube', identifier: 'dQw4w9WgXcQ' }), null);
  assert.equal(ownStreamBlockedReason({ platform: 'youtube', identifier: 'https://www.youtube.com/@foo' }), null);
  assert.equal(ownStreamBlockedReason({ platform: 'twitch', identifier: 'mine' }), null);
});

test('xPost numbers are made u32-safe before save', () => {
  const base = { enabled: true, delaySecs: 2.5, staleMinutes: -1, cooldownMinutes: Number.NaN, dailyLimit: 1e12 };
  const out = sanitizeXPostNumbers(base);
  assert.deepEqual(out, { enabled: true, delaySecs: 2, staleMinutes: 0, cooldownMinutes: 60, dailyLimit: 0xffffffff });
  assert.equal(base.delaySecs, 2.5);
  assert.equal(sanitizeXPostNumbers({ ...base, delaySecs: undefined as unknown as number }).delaySecs, 60);
});

test('callback port accepts only integers in 1024..65535', () => {
  assert.equal(parseCallbackPort('47821'), 47821);
  assert.equal(parseCallbackPort(' 1024 '), 1024);
  assert.equal(parseCallbackPort(65535), 65535);
  for (const bad of ['', '80', '1023', '65536', '4782.1', '-1', 'abc', '47821a', null, undefined, 2.5]) {
    assert.equal(parseCallbackPort(bad), null, String(bad));
  }
});

test('api mode is selectable only when connected', () => {
  assert.equal(canSelectApiMode({ connected: true }), true);
  assert.equal(canSelectApiMode({ connected: false }), false);
  assert.equal(canSelectApiMode(null), false);
  assert.equal(canSelectApiMode(undefined), false);
});

test('error text reads XPostError, Error and strings', () => {
  assert.equal(errorText({ code: 'portInUse', message: 'ポート' }), 'ポート');
  assert.equal(errorText(new Error('boom')), 'boom');
  assert.equal(errorText('plain'), 'plain');
  assert.equal(errorText({ code: 'x' }), '[object Object]');
});

test('test post preview matches the Rust format', () => {
  assert.equal(testPostPreview(new Date(2026, 10, 14, 9, 5)), 'fast-comment 連携テスト 2026/11/14 09:05');
});
