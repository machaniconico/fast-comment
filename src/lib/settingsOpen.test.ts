import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseOpenIds, serializeOpenIds, SETTINGS_SECTION_IDS } from './settingsOpen.ts';

test('キーが無い時は全部畳む', () => {
  assert.deepEqual(parseOpenIds(null), []);
});

test('壊れた JSON は全部畳む', () => {
  assert.deepEqual(parseOpenIds('{not json'), []);
  assert.deepEqual(parseOpenIds(''), []);
});

test('配列でない値は全部畳む', () => {
  assert.deepEqual(parseOpenIds('{"a":1}'), []);
  assert.deepEqual(parseOpenIds('"settings-tts"'), []);
  assert.deepEqual(parseOpenIds('null'), []);
});

test('未知の id・文字列でない要素・重複は捨てる', () => {
  assert.deepEqual(
    parseOpenIds('["settings-tts","nope",3,null,"settings-tts","settings-obs"]'),
    ['settings-tts', 'settings-obs'],
  );
});

test('書き出した値を読み戻せる', () => {
  const ids = ['settings-appearance', 'settings-notify'];
  assert.deepEqual(parseOpenIds(serializeOpenIds(ids)), ids);
});

test('節の id は重複しない', () => {
  assert.equal(new Set(SETTINGS_SECTION_IDS).size, SETTINGS_SECTION_IDS.length);
});
