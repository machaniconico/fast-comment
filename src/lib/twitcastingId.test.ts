// Rust の sources/twitcasting.rs の extract_screen_id と同じ表。両方を同時に直すこと。
import assert from 'node:assert/strict';
import test from 'node:test';
import { extractTwitcastingScreenId, isTwitcastingUrl } from './twitcastingId.ts';

const TABLE: [string, string | null][] = [
  ['twitcasting.tv/abc', 'abc'],
  ['https://twitcasting.tv/abc', 'abc'],
  ['https://twitcasting.tv/abc/movie/123', 'abc'],
  ['https://twitcasting.tv/abc/', 'abc'],
  ['https://twitcasting.tv/abc?x=1', 'abc'],
  ['https://twitcasting.tv/abc/movie/1?a=b#c', 'abc'],
  ['https://twitcasting.tv/c:abc', 'c:abc'],
  ['https://twitcasting.tv/g:123', 'g:123'],
  ['twitcasting.tv/f:xxx', 'f:xxx'],
  ['abc', 'abc'],
  ['a_b9', 'a_b9'],
  ['c:abc', 'c:abc'],
  ['g:123', 'g:123'],
  ['f:xxx', 'f:xxx'],
  ['  abc  ', 'abc'],
  ['  https://twitcasting.tv/abc  ', 'abc'],
  ['', null],
  ['   ', null],
  ['example.com/x', null],
  ['https://example.com/x', null],
  ['twitcasting.tv.evil.com/x', null],
  ['https://twitcasting.tv.evil.com/x', null],
  ['https://twitcasting.tv/', null],
  ['twitcasting.tv', null],
  ['https://twitcasting.tv/?a=b', null],
];

for (const [input, expected] of TABLE) {
  test(`extractTwitcastingScreenId(${JSON.stringify(input)}) = ${JSON.stringify(expected)}`, () => {
    assert.equal(extractTwitcastingScreenId(input), expected);
  });
}

test('isTwitcastingUrl は twitcasting.tv の URL だけを真にする', () => {
  assert.equal(isTwitcastingUrl('https://twitcasting.tv/abc'), true);
  assert.equal(isTwitcastingUrl('twitcasting.tv/abc'), true);
  assert.equal(isTwitcastingUrl('abc'), false);
  assert.equal(isTwitcastingUrl('twitcasting.tv.evil.com/x'), false);
});
