import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

const panel = readFileSync(new URL('./components/BackupPanel.svelte', import.meta.url), 'utf8');
const app = readFileSync(new URL('../App.svelte', import.meta.url), 'utf8');

test('復元確認には再起動までの変更と引き継ぐ設定が表示される', () => {
  const confirmation = panel.match(/window\.confirm\([\s\S]*?\)\) return/)?.[0];
  assert.ok(confirmation);
  for (const text of [
    '事前バックアップの後、再起動までの変更は失われる',
    'X 自動告知の台帳は戻らない',
    '実行ファイルの指定や接続先の上書きは今の設定のまま',
  ]) assert.ok(confirmation.includes(text), text);
});

test('バックアップ欄に自動作成失敗の理由が表示される', () => {
  assert.match(panel, /自動バックアップに失敗しました。\{lastFailure\}/);
  assert.match(panel, /getLastAutoBackupFailure\(\)/);
});

test('復元のロールバックが未完了の間は保存禁止の通知を閉じられない', () => {
  assert.match(app, /復元の取り消しが完了していません。該当するデータの保存を停止しています。/);
  assert.match(app, /\{#if restoreFailure === 'failed'\}\s*<button/);
});

test('一覧の取得に失敗した場合はエラーを表示する', () => {
  assert.match(panel, /entries = null;\s*listError =/);
  assert.match(panel, /\{#if listError\}[\s\S]*?\{:else if entries\?\.length\}/);
});
