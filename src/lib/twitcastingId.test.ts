// Rust の sources/twitcasting.rs の extract_screen_id と同じ表。両方を同時に直すこと。
import assert from 'node:assert/strict';
import test from 'node:test';
import { extractTwitcastingScreenId, isPrefixedTwitcastingId, isTwitcastingUrl } from './twitcastingId.ts';

const A32 = 'a'.repeat(32);
const TABLE: [string, string | null][] = [
  // 受理: URL
  ['twitcasting.tv/abc', 'abc'],
  ['https://twitcasting.tv/abc', 'abc'],
  ['http://twitcasting.tv/abc', 'abc'],
  ['https://twitcasting.tv/abc/', 'abc'],
  ['https://twitcasting.tv/abc?x=1', 'abc'],
  ['https://twitcasting.tv/abc#frag', 'abc'],
  ['https://twitcasting.tv/abc/movie/123', 'abc'],
  ['https://twitcasting.tv/abc/anything/else', 'abc'],
  ['https://twitcasting.tv/abc/movie/1?a=b#c', 'abc'],
  ['https://twitcasting.tv/c:abc', 'c:abc'],
  ['https://twitcasting.tv/c%3Aabc', 'c:abc'],
  ['https://twitcasting.tv/c%3aabc', 'c:abc'],
  ['https://twitcasting.tv/g:123', 'g:123'],
  ['twitcasting.tv/f:xxx', 'f:xxx'],
  ['https://twitcasting.tv/ig:abc', 'ig:abc'],
  ['https://twitcasting.tv/C:AbC', 'C:AbC'],
  ['https://TWITCASTING.TV/AbC', 'AbC'],
  ['https://www.twitcasting.tv/abc', 'abc'],
  ['https://ssl.twitcasting.tv/abc', 'abc'],
  ['https://en.twitcasting.tv/abc', 'abc'],
  ['https://ja.twitcasting.tv/abc', 'abc'],
  ['  https://twitcasting.tv/abc  ', 'abc'],
  // 受理: 裸の ID・境界
  ['abc', 'abc'],
  ['a', 'a'],
  [A32, A32],
  ['a_b9', 'a_b9'],
  ['AbC', 'AbC'],
  ['c:abc', 'c:abc'],
  ['g:123', 'g:123'],
  ['f:xxx', 'f:xxx'],
  ['ig:abc', 'ig:abc'],
  ['IG:Abc', 'IG:Abc'],
  ['  abc  ', 'abc'],
  [`https://twitcasting.tv/${A32}`, A32],
  // 拒否
  ['', null],
  ['   ', null],
  ['a'.repeat(33), null],
  [`https://twitcasting.tv/${'a'.repeat(33)}`, null],
  [':', null],
  ['c:', null],
  ['https://twitcasting.tv/:', null],
  ['https://twitcasting.tv/c:', null],
  ['x:abc', null],
  ['c::abc', null],
  ['https://twitcasting.tv/c%253Aabc', null],
  ['https://twitcasting.tv/c%3A', null],
  ['https://twitcasting.tv/%ZZ', null],
  ['https://twitcasting.tv/indexapi.php', null],
  ['https://twitcasting.tv/a.b', null],
  ['abc.def', null],
  ['example.com/x', null],
  ['https://example.com/x', null],
  ['twitcasting.tv.evil.com/x', null],
  ['https://twitcasting.tv.evil.com/x', null],
  ['https://apiv2.twitcasting.tv/abc', null],
  ['https://evil-twitcasting.tv/abc', null],
  ['https://twitcasting.tv/', null],
  ['twitcasting.tv', null],
  ['https://twitcasting.tv/?a=b', null],
  ['https://twitcasting.tv//abc', null],
];

for (const [input, expected] of TABLE) {
  test(`extractTwitcastingScreenId(${JSON.stringify(input)}) = ${JSON.stringify(expected)}`, () => {
    assert.equal(extractTwitcastingScreenId(input), expected);
  });
}

test('isPrefixedTwitcastingId は接頭辞付きの裸 ID だけを真にする', () => {
  for (const ok of ['c:abc', 'G:1', 'f:x', 'ig:a']) assert.equal(isPrefixedTwitcastingId(ok), true, ok);
  for (const ng of ['abc', 'c:', ':', 'x:abc', 'https://twitcasting.tv/c:abc']) assert.equal(isPrefixedTwitcastingId(ng), false, ng);
});

test('isTwitcastingUrl は twitcasting.tv の URL だけを真にする', () => {
  assert.equal(isTwitcastingUrl('https://apiv2.twitcasting.tv/abc'), false);
  assert.equal(isTwitcastingUrl('https://ja.twitcasting.tv/'), true);
  assert.equal(isTwitcastingUrl('https://twitcasting.tv/abc'), true);
  assert.equal(isTwitcastingUrl('twitcasting.tv/abc'), true);
  assert.equal(isTwitcastingUrl('abc'), false);
  assert.equal(isTwitcastingUrl('twitcasting.tv.evil.com/x'), false);
});
