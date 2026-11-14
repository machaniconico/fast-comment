import assert from 'node:assert/strict';
import test from 'node:test';
import {
  EMPTY_PROGRESS,
  applyDownloadEvent,
  formatBytes,
  progressLabel,
  progressPercent,
  statusText,
  updateErrorText,
} from './appUpdate.ts';

test('download events accumulate into progress', () => {
  let p = applyDownloadEvent(EMPTY_PROGRESS, { event: 'Started', data: { contentLength: 1000 } });
  assert.deepEqual(p, { downloaded: 0, total: 1000 });
  p = applyDownloadEvent(p, { event: 'Progress', data: { chunkLength: 250 } });
  p = applyDownloadEvent(p, { event: 'Progress', data: { chunkLength: 250 } });
  assert.deepEqual(p, { downloaded: 500, total: 1000 });
  assert.equal(progressPercent(p), 50);
  p = applyDownloadEvent(p, { event: 'Finished' });
  assert.deepEqual(p, { downloaded: 1000, total: 1000 });
  assert.equal(progressPercent(p), 100);
});

test('unknown or invalid content length leaves the percentage unknown', () => {
  for (const contentLength of [undefined, 0, -5, Number.NaN]) {
    const p = applyDownloadEvent(EMPTY_PROGRESS, { event: 'Started', data: { contentLength } });
    assert.equal(p.total, null);
    assert.equal(progressPercent(p), null);
  }
  let p = applyDownloadEvent(EMPTY_PROGRESS, { event: 'Started', data: {} });
  p = applyDownloadEvent(p, { event: 'Progress', data: { chunkLength: 2048 } });
  p = applyDownloadEvent(p, { event: 'Finished' });
  assert.deepEqual(p, { downloaded: 2048, total: null });
  assert.equal(progressLabel(p), '2 KB');
});

test('Started resets an earlier partial download', () => {
  const partial = { downloaded: 700, total: 900 };
  assert.deepEqual(applyDownloadEvent(partial, { event: 'Started', data: { contentLength: 500 } }), {
    downloaded: 0,
    total: 500,
  });
});

test('bad chunk lengths are ignored and percentage is clamped', () => {
  let p = { downloaded: 0, total: 100 };
  p = applyDownloadEvent(p, { event: 'Progress', data: { chunkLength: -10 } });
  p = applyDownloadEvent(p, { event: 'Progress', data: { chunkLength: Number.NaN } });
  assert.equal(p.downloaded, 0);
  assert.equal(progressPercent({ downloaded: 150, total: 100 }), 100);
  assert.equal(progressPercent({ downloaded: 99, total: 100 }), 99);
});

test('formatBytes and progressLabel', () => {
  assert.equal(formatBytes(-1), '0 B');
  assert.equal(formatBytes(Number.NaN), '0 B');
  assert.equal(formatBytes(512), '512 B');
  assert.equal(formatBytes(1536), '1 KB');
  assert.equal(formatBytes(5 * 1024 * 1024 + 300 * 1024), '5.3 MB');
  assert.equal(progressLabel({ downloaded: 1024 * 1024, total: 4 * 1024 * 1024 }), '25% (1.0 MB / 4.0 MB)');
});

test('updateErrorText normalizes thrown values', () => {
  assert.equal(updateErrorText(new Error('network down')), 'network down');
  assert.equal(updateErrorText('  plugin not found  '), 'plugin not found');
  assert.equal(updateErrorText(''), '不明なエラー');
  assert.equal(updateErrorText({}), '不明なエラー');
  assert.equal(updateErrorText(new Error('')), '不明なエラー');
});

test('statusText covers every phase', () => {
  assert.equal(statusText('idle', '', ''), '');
  assert.equal(statusText('upToDate', '', ''), '最新版です。');
  assert.equal(statusText('available', '0.3.0', ''), '新しいバージョン v0.3.0 があります。');
  assert.match(statusText('downloading', '0.3.0', ''), /v0\.3\.0/);
  assert.match(statusText('error', '', 'timeout'), /^更新を確認できませんでした: timeout$/);
  assert.match(statusText('installError', '0.3.0', 'sig'), /^更新をインストールできませんでした: sig$/);
  assert.notEqual(statusText('checking', '', ''), '');
  assert.notEqual(statusText('installing', '', ''), '');
});
