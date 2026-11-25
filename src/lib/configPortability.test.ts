import { test } from 'node:test';
import assert from 'node:assert/strict';
import { importMessage } from './configPortability.ts';

const disabled = 'X の自動告知は取り込み後に無効にしました。';

test('読み直しまで済んだ時は、完了の文に保存時の通知を続ける', () => {
  assert.equal(
    importMessage({ warnings: [disabled], protectedOAuth: [] }, false, true),
    `設定をインポートしました。 ${disabled}`,
  );
  assert.equal(
    importMessage({ warnings: [], protectedOAuth: [] }, true, true),
    '設定をインポートし、外観設定を反映しました。',
  );
});

test('画面の読み直しに失敗しても、保存時の通知は出す', () => {
  assert.equal(
    importMessage({ warnings: [disabled, '棒読みちゃんは自動では起動しません。'], protectedOAuth: [] }, true, false),
    `設定は保存しました。 ${disabled} 棒読みちゃんは自動では起動しません。`,
  );
});

test('読み直しに失敗し、通知も無い時は何も出さない', () => {
  assert.equal(importMessage({ warnings: [], protectedOAuth: [] }, false, false), null);
});

test('ログイン中に保持した項目は、サービス名で知らせる', () => {
  assert.equal(
    importMessage({ warnings: [], protectedOAuth: ['xPost', 'twitcasting'] }, false, false),
    '設定は保存しました。 ログイン中の X、ツイキャス の Client ID と待受ポートを保持しました。',
  );
});
