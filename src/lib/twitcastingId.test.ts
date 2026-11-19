// Rust の sources/twitcasting.rs の extract_screen_id と同じ表。両方を同時に直すこと。
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { extractTwitcastingScreenId, isPrefixedTwitcastingId, isTwitcastingUrl } from './twitcastingId.ts';

// Rust の sources/twitcasting.rs も同じ表を include_str! で全行検査する。
const TABLE = JSON.parse(
  readFileSync(new URL('./twitcastingId.cases.json', import.meta.url), 'utf8'),
) as [string, string | null][];

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
