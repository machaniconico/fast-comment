import assert from 'node:assert/strict';
import test from 'node:test';
import { applyDeleted, applyDuplicated, groupTemplates, resolveTemplateId, suggestCopyName, templateKindLabel, templateWsPath } from './templateCatalog.ts';
import type { TemplateInfo } from './ipc.ts';

const t = (id: string, kind: TemplateInfo['kind']): TemplateInfo => ({
  id,
  name: id.replace(/^custom:/, ''),
  kind,
  editable: kind === 'custom',
});

test('kind label: builtin / custom', () => {
  assert.equal(templateKindLabel('builtin'), '内蔵');
  assert.equal(templateKindLabel('custom'), 'カスタム');
});

test('groupTemplates splits by kind and sorts by name', () => {
  const g = groupTemplates([t('simple', 'builtin'), t('custom:b', 'custom'), t('default', 'builtin'), t('custom:a', 'custom')]);
  assert.deepEqual(g.builtin.map((x) => x.id), ['default', 'simple']);
  assert.deepEqual(g.custom.map((x) => x.id), ['custom:a', 'custom:b']);
  assert.deepEqual(groupTemplates([]), { builtin: [], custom: [] });
});

test('suggestCopyName: <name>-copy, numbered on collision', () => {
  assert.equal(suggestCopyName('default', []), 'default-copy');
  assert.equal(suggestCopyName('default', ['default-copy']), 'default-copy-2');
  assert.equal(suggestCopyName('default', ['default-copy', 'default-copy-2']), 'default-copy-3');
  assert.equal(suggestCopyName('default', ['Default-Copy']), 'default-copy-2', 'collision is case-insensitive');
  assert.equal(suggestCopyName('default', ['other']), 'default-copy');
});

test('suggestCopyName: lowercase only and within the Rust name rules', () => {
  assert.equal(suggestCopyName('MyTpl', []), 'mytpl-copy');
  assert.equal(suggestCopyName('a b.c', []), 'a-b-c-copy');
  assert.equal(suggestCopyName('__x', []), 'x-copy');
  assert.equal(suggestCopyName('日本語', []), 'template-copy');
  assert.equal(suggestCopyName('', []), 'template-copy');
  const long = suggestCopyName('a'.repeat(80), []);
  assert.equal(long.length, 64);
  assert.ok(long.endsWith('-copy'));
  const longTaken = suggestCopyName('a'.repeat(80), [long]);
  assert.equal(longTaken.length, 64);
  assert.ok(longTaken.endsWith('-copy-2'));
  for (const name of [long, longTaken, suggestCopyName('A-B_C', [])]) {
    assert.match(name, /^[a-z0-9][a-z0-9_-]*$/);
  }
});

test('resolveTemplateId falls back to default when the id is gone', () => {
  const ids = ['default', 'simple', 'custom:a'];
  assert.equal(resolveTemplateId('custom:a', ids), 'custom:a');
  assert.equal(resolveTemplateId('simple', ids), 'simple');
  assert.equal(resolveTemplateId('custom:gone', ids), 'default');
  assert.equal(resolveTemplateId('', ids), 'default');
  assert.equal(resolveTemplateId('custom:a', []), 'default');
});

test('applyDuplicated adds the created template (idempotent after refetch)', () => {
  const list = [t('default', 'builtin')];
  const created = t('custom:default-copy', 'custom');
  const once = applyDuplicated(list, created);
  assert.deepEqual(once.map((x) => x.id), ['default', 'custom:default-copy']);
  assert.equal(once.find((x) => x.id === created.id)?.editable, true);
  assert.deepEqual(applyDuplicated(once, created).map((x) => x.id), ['default', 'custom:default-copy']);
  assert.deepEqual(list.map((x) => x.id), ['default'], 'input is not mutated');
});

test('applyDeleted removes the id and resets a matching selection to default', () => {
  const list = [t('default', 'builtin'), t('custom:a', 'custom'), t('custom:b', 'custom')];
  const r = applyDeleted(list, 'custom:a', 'custom:a');
  assert.deepEqual(r.list.map((x) => x.id), ['default', 'custom:b']);
  assert.equal(r.current, 'default');
  assert.equal(applyDeleted(list, 'custom:a', ' custom:a ').current, 'default', 'config value is compared trimmed');
  assert.equal(applyDeleted(list, 'custom:a', 'custom:b').current, 'custom:b');
  assert.equal(applyDeleted(list, 'custom:a', 'simple').current, 'simple');
  // 再取得後の一覧(既に消えている)に掛けても結果は同じ。
  assert.deepEqual(applyDeleted(r.list, 'custom:a', 'custom:a'), r);
});

test('templateWsPath: 集計系の内蔵は専用エンドポイント、それ以外は /ws', () => {
  assert.equal(templateWsPath('goals'), '/stats');
  assert.equal(templateWsPath('goals-neon'), '/stats');
  assert.equal(templateWsPath('goals-retro'), '/stats');
  assert.equal(templateWsPath('timer'), '/timer');
  assert.equal(templateWsPath('poll'), '/poll');
  assert.equal(templateWsPath('default'), '/ws');
  assert.equal(templateWsPath('custom:goals'), '/ws');
  assert.equal(templateWsPath('toString'), '/ws');
});
