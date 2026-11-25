import { test } from 'node:test';
import assert from 'node:assert/strict';
import { matchesSettingsQuery, normalizeSettingsText } from './settingsFilter.ts';

test('全角英字は半角として一致する', () => {
  assert.equal(normalizeSettingsText('ＴＴＳ'), 'tts');
  assert.ok(matchesSettingsQuery('TTS（読み上げ）', 'ｔｔｓ'));
  assert.ok(matchesSettingsQuery('ＯＢＳオーバーレイ', 'obs'));
});

test('大文字と小文字を区別しない', () => {
  assert.ok(matchesSettingsQuery('Webhook', 'WEBHOOK'));
  assert.ok(matchesSettingsQuery('webhook', 'WebHook'));
});

test('空文字と空白だけの入力は常に一致する', () => {
  assert.ok(matchesSettingsQuery('外観', ''));
  assert.ok(matchesSettingsQuery('外観', '   '));
});

test('該当なしは一致しない', () => {
  assert.equal(matchesSettingsQuery('外観', 'タイマー'), false);
  assert.equal(matchesSettingsQuery('外観 テーマ', '外観 タイマー'), false);
});

test('空白で区切った語はすべて含まれる時に一致する', () => {
  assert.ok(matchesSettingsQuery('外観 テーマ 文字サイズ', 'テーマ　サイズ'));
});
