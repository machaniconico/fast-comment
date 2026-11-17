import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  splitKeywords,
  stateLabel,
  pauseReasonLabel,
  resultKindLabel,
  testResultLabel,
  textLength,
} from './integrations.ts';

test('splitKeywords: 空文字は空配列', () => {
  assert.deepEqual(splitKeywords(''), []);
});

test('splitKeywords: 空白だけの行(全角含む)は捨てる', () => {
  assert.deepEqual(splitKeywords('a\n   \n　　\n\t\nb'), ['a', 'b']);
});

test('splitKeywords: CRLF・CR・LF を行区切りにする', () => {
  assert.deepEqual(splitKeywords('a\r\nb\rc\nd'), ['a', 'b', 'c', 'd']);
});

test('splitKeywords: 前後の全角スペースを除き、語中の空白は残す', () => {
  assert.deepEqual(splitKeywords('　質問 です　'), ['質問 です']);
});

test('splitKeywords: 重複は除かない(Rust と同じ)', () => {
  assert.deepEqual(splitKeywords('a\na'), ['a', 'a']);
});

test('textLength: サロゲートペアを 1 文字と数える', () => {
  assert.equal(textLength('a😀'), 2);
});

test('stateLabel: 既知の状態と未知のコード', () => {
  assert.equal(stateLabel('active'), '稼働中');
  assert.equal(stateLabel('disabled'), '無効');
  assert.equal(stateLabel('paused'), '停止中');
  assert.equal(stateLabel('noUrl'), 'URL 未設定');
  assert.equal(stateLabel('somethingNew'), '不明な状態');
  assert.equal(stateLabel(''), '確認中');
  assert.equal(stateLabel(null), '確認中');
});

test('pauseReasonLabel: 既知の理由・未知のコード・空', () => {
  assert.match(pauseReasonLabel('notFound'), /404/);
  assert.match(pauseReasonLabel('unauthorized'), /401/);
  assert.match(pauseReasonLabel('redirected'), /リダイレクト/);
  assert.match(pauseReasonLabel('unreachable'), /失敗し続け/);
  assert.equal(pauseReasonLabel('weird'), '原因不明の停止');
  assert.equal(pauseReasonLabel(null), '');
});

test('resultKindLabel / testResultLabel', () => {
  assert.equal(resultKindLabel('connectFailed'), '接続できませんでした');
  assert.equal(resultKindLabel('weird'), '失敗しました');
  assert.equal(testResultLabel({ ok: true, status: 204, errorKind: null }), 'テスト送信に成功しました(HTTP 204)');
  assert.match(testResultLabel({ ok: false, status: 500, errorKind: 'httpError' }), /HTTP 500/);
  assert.match(testResultLabel({ ok: false, status: null, errorKind: 'noUrl' }), /URL が未設定/);
});
