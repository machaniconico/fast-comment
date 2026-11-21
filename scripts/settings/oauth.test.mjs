import assert from 'node:assert/strict';
import test, { after } from 'node:test';
import { createServer as createHttpServer } from 'node:http';
import { chromium } from 'playwright';
import { createServer } from 'vite';

const vite = await createServer({ server: { middlewareMode: true, hmr: false } });
const server = createHttpServer(vite.middlewares);
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
after(async () => {
  await vite.close();
  await new Promise(resolve => server.close(resolve));
});
const browser = await chromium.launch({ channel: process.env.PLAYWRIGHT_CHANNEL });
after(() => browser.close());
const fixture = new URL('/scripts/settings/fixture.html', 'http://127.0.0.1:' + server.address().port);

async function withSettings(t, run, mode = 'settings') {
  const page = await browser.newPage();
  page.setDefaultTimeout(5000);
  t.after(() => page.close());
  await page.goto(new URL('?' + mode, fixture).href, { waitUntil: 'commit' });
  if (!mode.endsWith('app')) await page.locator('#settings-import-json').waitFor();
  if (mode === 'settings') {
    await page.locator('details').filter({ hasText: '自分の ClientID を使う' }).locator('summary').click();
  }
  await run(page);
}

const save = page => page.getByRole('button', { name: '設定を保存', exact: true });
const importButton = page => page.locator('.portability').filter({ has: page.locator('#settings-import-json') }).getByRole('button', { name: 'テキストから読込' });
const services = [
  { name: 'ツイキャス', key: 'twitcasting', prefix: 'twitcasting', id: 'twitcastingClientId', connect: 'ログイン', cancel: '取り消す', cancelCommand: 'cancel_twitcasting_login', ready: { state: 'ready', screenId: 'current-tc' } },
  { name: 'X', key: 'xPost', prefix: 'xpost', id: 'xPostClientId', connect: 'X に接続', cancel: 'ログインを取り消す', cancelCommand: 'cancel_x_post_login', ready: { connected: true, username: 'current-x' } },
];

async function importConfig(page, bundle) {
  await page.locator('#settings-import-json').fill(JSON.stringify(bundle));
  await importButton(page).click();
}

const saved = page => page.evaluate(() => window.mock.saved);
const initial = page => page.evaluate(() => window.mock.initial);
const reloadSettings = page => page.getByRole('button', { name: '設定を再読み込み', exact: true });
const moderationImport = page => page.locator('.portability').filter({ has: page.locator('#moderation-import-json') });

async function importModeration(page, moderation) {
  await page.locator('#moderation-import-json').fill(JSON.stringify({ moderation }));
  await moderationImport(page).getByRole('button', { name: 'テキストから読込' }).click();
}


async function saveSettings(page, writeCount = 1) {
  await save(page).click();
  await page.waitForFunction(n => window.mock.writes.length === n, writeCount);
  await page.getByText('保存しました', { exact: true }).waitFor();
  return saved(page);
}

for (const service of services) {
  const port = page => page.locator('#' + service.prefix + '-port');
  const id = page => page.locator('#' + service.prefix + '-client-id');

  async function start(page, nextId = 'new-client', nextPort = '40321') {
    await id(page).fill(nextId);
    await port(page).fill(nextPort);
    await page.getByRole('button', { name: service.connect, exact: true }).click();
    await page.waitForFunction(key => !!window.mock.login[key], service.key);
  }

  const accept = page => page.evaluate(key => window.mock.login[key].accept(), service.key);
  const finish = (page, result) => page.evaluate(({ key, result }) => {
    window.mock.login[key].finish(typeof result === 'string' ? new Error(result) : result);
  }, { key: service.key, result });

  test(service.name + ' preserves the caret when editing a valid port', async t => withSettings(t, async page => {
    await port(page).fill('040123');
    await port(page).evaluate(input => input.setSelectionRange(3, 3));
    await port(page).press('5');
    assert.equal(await port(page).inputValue(), '0405123');
    // Repair the out-of-range edit at the caret, preserving the leading zeros.
    await port(page).press('Delete');
    assert.equal(await port(page).inputValue(), '040523');
    assert.equal(await port(page).evaluate(el => el.selectionStart), 4);
  }));

  test(service.name + ' prevents saving an invalid port and permits correction', async t => withSettings(t, async page => {
    await port(page).fill('65536');
    assert.equal(await save(page).isDisabled(), true);
    assert.equal(await page.evaluate(() => window.mock.writes.length), 0);
    await port(page).fill('65535');
    assert.equal((await saveSettings(page))[service.key].callbackPort, 65535);
  }));

  test(service.name + ' preserves the saved connection settings during a full import', async t => withSettings(t, async page => {
    await start(page);
    await accept(page);
    const config = await initial(page);
    config[service.key].callbackPort = 41321;
    config.credentials[service.id] = 'backup-client';
    config.ui.notifyVolume = 0.8;
    await importConfig(page, { config });
    await page.getByText(/ログイン中の.*Client ID.*待受ポート/).waitFor();
    assert.equal(await id(page).inputValue(), 'new-client');
    assert.equal(await port(page).inputValue(), '40321');
    assert.equal((await saved(page)).ui.notifyVolume, 0.8);
    await finish(page, { ...service.ready, clientId: 'new-client' });
    await page.getByText(/として(ログイン|接続)中/).waitFor();
    const next = await saveSettings(page, 2);
    assert.equal(next.credentials[service.id], 'new-client');
    assert.equal(next[service.key].callbackPort, 40321);
  }));

  for (const result of ['success', 'cancelled', 'failure before save']) {
    test(service.name + ' reads saved ClientID and port after ' + result, async t => withSettings(t, async page => {
      await start(page, 'typed-client', '040321');
      if (result !== 'failure before save') await accept(page);
      const reads = await page.evaluate(() => window.mock.reads);
      await finish(page, result === 'success' ? { ...service.ready, clientId: 'typed-client' } : result);
      await page.waitForFunction(n => window.mock.reads > n, reads);
      if (result === 'failure before save') {
        // Nothing was saved: the ClientID field shows the saved ID again (the full save does not send it),
        // and the typed port stays because the full save sends it.
        await page.waitForFunction(() => window.mock.configSync.reloading === 0);
        assert.equal(await id(page).inputValue(), (await initial(page)).credentials[service.id]);
        assert.equal(await port(page).inputValue(), '040321');
        const next = await saveSettings(page);
        assert.equal(next.credentials[service.id], (await initial(page)).credentials[service.id]);
        assert.equal(next[service.key].callbackPort, 40321);
        return;
      }
      const expected = await saved(page);
      await page.waitForFunction(({ prefix, expected }) => document.getElementById(prefix + '-port').value === String(expected), { prefix: service.prefix, expected: expected[service.key].callbackPort });
      // Inputs and the parent form must both reflect what was actually persisted.
      assert.equal(await id(page).inputValue(), expected.credentials[service.id]);
      assert.equal(await port(page).inputValue(), String(expected[service.key].callbackPort));
      const next = await saveSettings(page);
      assert.equal(next.credentials[service.id], expected.credentials[service.id]);
      assert.equal(next[service.key].callbackPort, expected[service.key].callbackPort);
    }));
  }

  test(service.name + ' ignores an older login read after a successful full import reload', async t => withSettings(t, async page => {
    await start(page);
    await accept(page);
    await page.evaluate(() => { window.mock.holdReads = true; });
    await finish(page, 'cancelled');
    await page.waitForFunction(() => window.mock.heldReads.length === 1);
    await page.evaluate(() => { window.mock.holdReads = false; });
    const config = await initial(page);
    config.credentials[service.id] = 'imported-after-login';
    config[service.key].callbackPort = 41322;
    await importConfig(page, { config });
    await page.getByText(/設定をインポートしました/).waitFor();
    await page.evaluate(() => window.mock.heldReads.shift()());
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(resolve)));
    assert.equal(await id(page).inputValue(), 'imported-after-login');
    assert.equal(await port(page).inputValue(), '41322');
    assert.equal(await page.evaluate(() => window.mock.configSync.reloadRequired), false);
    assert.equal(await save(page).isEnabled(), true);
  }));

  test(service.name + ' can cancel a pending login during a full reload', async t => withSettings(t, async page => {
    await start(page);
    await page.evaluate(() => { window.mock.readsToFail = 1; });
    await finish(page, 'cancelled');
    await reloadSettings(page).waitFor();
    await start(page);
    const attempt = await page.evaluate(key => window.mock.login[key].attempt, service.key);
    await page.evaluate(() => { window.mock.holdReads = true; });
    await reloadSettings(page).click();
    await page.waitForFunction(() => window.mock.heldReads.length === 1);
    assert.equal(await page.locator('#appearance-theme').isDisabled(), true);
    assert.equal(await port(page).isDisabled(), true);
    assert.equal(await save(page).isDisabled(), true);
    const cancel = page.getByRole('button', { name: service.cancel, exact: true });
    assert.equal(await cancel.isEnabled(), true);
    await cancel.click();
    assert.deepEqual(await page.evaluate(() => window.mock.cancels), [{ command: service.cancelCommand, attempt }]);
    await page.evaluate(() => { window.mock.holdReads = false; });
    await finish(page, 'cancelled');
    await page.evaluate(() => window.mock.heldReads.shift()());
    await page.waitForFunction(() => window.mock.configSync.reloading === 0);
  }));

  test(service.name + ' closing settings cancels login and leaves the goals toggle enabled', async t => withSettings(t, async page => {
    await page.getByRole('tab', { name: '設定', exact: true }).click();
    if (service.key === 'twitcasting') {
      await page.locator('details').filter({ hasText: '自分の ClientID を使う' }).locator('summary').click();
    }
    await start(page);
    await accept(page);
    const attempt = await page.evaluate(key => window.mock.login[key].attempt, service.key);
    await page.getByRole('tab', { name: 'コメント', exact: true }).click();
    await page.waitForFunction(() => window.mock.cancels.length === 1);
    assert.deepEqual(await page.evaluate(() => window.mock.cancels), [{ command: service.cancelCommand, attempt }]);
    await finish(page, 'cancelled');
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(resolve)));
    assert.equal(await page.evaluate(() => window.mock.configSync.reloadRequired), false);
    assert.equal(await page.getByRole('checkbox', { name: 'コメビュに表示' }).isEnabled(), true);
  }, 'app'));

  test(service.name + ' asks to reload when the post-login read fails', async t => withSettings(t, async page => {
    await start(page);
    await accept(page);
    await page.evaluate(() => { window.mock.readsToFail = 1; });
    await finish(page, { ...service.ready, clientId: 'new-client' });
    await page.getByText('設定を読み直してください', { exact: false }).first().waitFor();
    assert.equal(await save(page).isDisabled(), true);
    assert.equal(await id(page).isEnabled(), true);
    assert.equal(await port(page).isEnabled(), true);
    await page.getByRole('button', { name: '設定を再読み込み', exact: true }).click();
    await page.getByText(/として(ログイン|接続)中/).waitFor();
    assert.equal(await id(page).inputValue(), 'new-client');
    assert.equal(await save(page).isEnabled(), true);
  }));

  test(service.name + ' locks its ClientID and port through the post-login read', async t => withSettings(t, async page => {
    await start(page, 'new-client', '040321');
    await accept(page);
    assert.equal(await id(page).isDisabled(), true);
    assert.equal(await port(page).isDisabled(), true);
    await page.getByText('OAuth の接続処理中です').waitFor();

    await page.evaluate(() => { window.mock.holdReads = true; });
    await finish(page, { ...service.ready, clientId: 'new-client' });
    await page.waitForFunction(() => window.mock.heldReads.length === 1);
    assert.equal(await id(page).isDisabled(), true);
    assert.equal(await port(page).isDisabled(), true);
    assert.equal(await save(page).isDisabled(), true);
    const other = services.find(candidate => candidate.key !== service.key);
    assert.equal(await page.locator('#' + other.prefix + '-client-id').isEnabled(), true);
    assert.equal(await page.locator('#' + other.prefix + '-port').isEnabled(), true);

    await page.evaluate(() => { window.mock.holdReads = false; window.mock.heldReads.shift()(); });
    await page.getByText(/として(ログイン|接続)中/).waitFor();
    assert.equal(await id(page).isEnabled(), true);
    assert.equal(await port(page).isEnabled(), true);
    assert.equal(await save(page).isEnabled(), true);
    assert.equal(await id(page).inputValue(), 'new-client');
    assert.equal(await port(page).inputValue(), '40321');
    await id(page).fill('next-client');
    await port(page).fill('40322');
    const next = await saveSettings(page);
    assert.equal(next.credentials[service.id], 'new-client');
    assert.equal(next[service.key].callbackPort, 40322);
  }));

  test(service.name + ' times out an unanswered post-login read', async t => withSettings(t, async page => {
    await start(page);
    await accept(page);
    await page.clock.install();
    await page.evaluate(() => { window.mock.holdReads = true; });
    await finish(page, { ...service.ready, clientId: 'new-client' });
    await page.waitForFunction(() => window.mock.heldReads.length === 1);
    assert.equal(await id(page).isDisabled(), true);

    await page.clock.fastForward(10_000);
    await page.getByText('設定を読み直してください', { exact: false }).first().waitFor();
    assert.equal(await id(page).isEnabled(), true);
    assert.equal(await port(page).isEnabled(), true);
    assert.equal(await save(page).isDisabled(), true);
    assert.equal(await page.evaluate(() => window.mock.heldReads.length), 1);

    await page.evaluate(() => { window.mock.holdReads = false; });
    await page.getByRole('button', { name: '設定を再読み込み', exact: true }).click();
    const next = await saveSettings(page);
    assert.equal(next.credentials[service.id], 'new-client');
    assert.equal(next[service.key].callbackPort, 40321);
  }));

  test(service.name + ' retries a timed-out manual reload and ignores its late reply', async t => withSettings(t, async page => {
    await start(page);
    await accept(page);
    await page.evaluate(() => { window.mock.readsToFail = 1; });
    await finish(page, 'cancelled');
    const reload = page.getByRole('button', { name: '設定を再読み込み', exact: true });
    await reload.waitFor();
    await page.clock.install();
    await page.evaluate(() => { window.mock.holdReads = true; });
    await reload.click();
    await page.waitForFunction(() => window.mock.heldReads.length === 1);
    assert.equal(await page.getByRole('button', { name: '読込中...', exact: true }).isDisabled(), true);

    await page.clock.fastForward(10_000);
    await reload.waitFor();
    assert.equal(await reload.isEnabled(), true);
    assert.equal(await save(page).isDisabled(), true);
    await page.evaluate(({ key, id }) => {
      window.mock.saved.credentials[id] = 'retry-client';
      window.mock.saved[key].callbackPort = 40322;
      window.mock.revision++;
      window.mock.holdReads = false;
    }, service);
    await reload.click();
    await page.waitForFunction(prefix => document.getElementById(prefix + '-port').value === '40322', service.prefix);
    await page.evaluate(() => {
      window.mock.heldReads.shift()();
      return new Promise(resolve => requestAnimationFrame(resolve));
    });
    assert.equal(await id(page).inputValue(), 'retry-client');
    assert.equal(await port(page).inputValue(), '40322');
    assert.equal(await save(page).isEnabled(), true);
  }));

  test(service.name + ' cancels the login attempt it started', async t => withSettings(t, async page => {
    await start(page);
    const attempt = await page.evaluate(key => window.mock.login[key].attempt, service.key);
    assert.equal(typeof attempt, 'string');
    assert.ok(attempt.length > 0);
    await page.getByRole('button', { name: service.cancel, exact: true }).click();
    await page.waitForFunction(() => window.mock.cancels.length === 1);
    assert.deepEqual(await page.evaluate(() => window.mock.cancels), [{ command: service.cancelCommand, attempt }]);
    await finish(page, 'cancelled');
    await start(page, 'new-client', '40322');
    assert.notEqual(await page.evaluate(key => window.mock.login[key].attempt, service.key), attempt);
  }));

  test(service.name + ' hides login status for a different saved ClientID', async t => withSettings(t, async page => {
    await start(page);
    await accept(page);
    await finish(page, { ...service.ready, clientId: 'new-client' });
    const account = page.getByText(/として(ログイン|接続)中/);
    await account.waitFor();
    const config = await initial(page);
    config.credentials[service.id] = 'different-client';
    await importConfig(page, { config });
    await page.getByText(/設定をインポートしました/).waitFor();
    assert.equal(await account.count(), 0);
  }));
}

test('closing untouched settings before its initial config reply leaves the goals toggle enabled', async t => withSettings(t, async page => {
  const toggle = page.getByRole('checkbox', { name: 'コメビュに表示' });
  await toggle.waitFor();
  await page.evaluate(() => { window.mock.holdReads = true; });
  await page.getByRole('tab', { name: '設定', exact: true }).click();
  await page.waitForFunction(() => window.mock.heldReads.length === 1);
  await page.getByRole('tab', { name: 'コメント', exact: true }).click();
  await page.evaluate(() => window.mock.heldReads.shift()());
  await page.waitForFunction(() => window.mock.configSync.reloading === 0);
  assert.equal(await page.evaluate(() => window.mock.configSync.reloadRequired), false);
  assert.equal(await toggle.isEnabled(), true);
}, 'app'));

test('ツイキャス treats a missing connect result as a failure', async t => withSettings(t, async page => {
  await page.locator('#twitcasting-client-id').fill('new-client');
  const login = page.getByRole('button', { name: 'ログイン', exact: true });
  await login.click();
  await page.evaluate(() => window.mock.login.twitcasting.finish(null));
  await login.waitFor();
  assert.equal(await page.getByText('ツイキャスにログインしました', { exact: true }).count(), 0);
}));

test('a committed import blocks stale settings saves until reloaded', async t => withSettings(t, async page => {
  const config = await initial(page);
  config.credentials.twitcastingClientId = 'imported-client';
  config.twitcasting.callbackPort = 40222;
  await page.evaluate(() => { window.mock.readsToFail = 1; });
  await importConfig(page, { config });
  await page.getByText('設定を読み直してください', { exact: false }).first().waitFor();
  assert.equal(await save(page).isDisabled(), true);
  assert.equal((await saved(page)).credentials.twitcastingClientId, 'imported-client');
  await page.getByRole('button', { name: '設定を再読み込み', exact: true }).click();
  await page.locator('#twitcasting-client-id').evaluate(input => {
    if (input.value !== 'imported-client') throw Error('imported ClientID is missing');
  });
  assert.equal((await saveSettings(page, 2)).twitcasting.callbackPort, 40222);
}));

test('a browser import applies appearance when no persisted config exists', async t => withSettings(t, async page => {
  const appearance = { theme: 'light', fontSize: 'l', density: 'compact' };
  await importConfig(page, { config: await initial(page), appearance });
  assert.match(await page.getByRole('status').textContent(), /外観設定を反映しました/);
  assert.equal((await page.evaluate(() => window.mock.appearance())).theme, 'light');
}, 'browser'));

test('importing callbackPort "bad" shows the fixed field name without the value', async t => withSettings(t, async page => {
  await page.getByRole('button', { name: '設定JSONをエクスポート' }).click();
  const exported = page.locator('#settings-export-json');
  await page.waitForFunction(() => document.getElementById('settings-export-json').value !== '');
  const bundle = JSON.parse(await exported.inputValue());
  bundle.config.twitcasting.callbackPort = 'bad';
  await importConfig(page, bundle);
  const status = page.locator('#settings-portability').getByRole('status');
  await status.waitFor();
  assert.equal(await status.textContent(), '設定の形式が不正です (項目 twitcasting.callbackPort)');
  assert.equal(await page.evaluate(() => window.mock.writes.length), 0);
}));

test('a config import error with a raw value shows only the generic message', async t => withSettings(t, async page => {
  await page.evaluate(() => {
    window.mock.updateError = '設定の形式が不正です (項目 twitcasting.callbackPort): private-input';
  });
  await importConfig(page, { config: await initial(page) });
  const status = page.locator('#settings-portability').getByRole('status');
  await status.waitFor();
  assert.equal(await status.textContent(), '設定のインポートに失敗しました。');
}));

test('the main window preserves OAuth settings when saving the goals toggle', async t => withSettings(t, async page => {
  const toggle = page.getByRole('checkbox', { name: 'コメビュに表示' });
  await toggle.waitFor();
  await page.evaluate(() => {
    window.mock.saved.credentials.twitcastingClientId = 'main-login';
    window.mock.saved.twitcasting.callbackPort = 43421;
    window.mock.login.twitcasting = {};
  });
  await toggle.click();
  await page.waitForFunction(() => window.mock.saved.goals.showInApp === true);
  await toggle.waitFor();
  await page.evaluate(() => { delete window.mock.login.twitcasting; });
  await toggle.click();
  await page.waitForFunction(() => window.mock.saved.goals.showInApp === false);
  const config = await saved(page);
  assert.equal(config.credentials.twitcastingClientId, 'main-login');
  assert.equal(config.twitcasting.callbackPort, 43421);
}, 'app'));

async function commitOAuthBeforeWrite(page, heldWrites, holdWrites) {
  // The old path pauses after taking a snapshot. The field-specific path pauses before writing.
  await page.waitForFunction(held => window.mock.heldReads.length + window.mock[held].length === 1, heldWrites);
  await page.evaluate(async ({ heldWrites, holdWrites }) => {
    for (const [key, command] of [['xPost', 'connect_x_post'], ['twitcasting', 'connect_twitcasting']]) {
      const login = window.__TAURI_INTERNALS__.invoke(command, {
        attempt: crypto.randomUUID(), clientId: 'new-' + key, callbackPort: 43421,
      });
      window.mock.login[key].accept();
      window.mock.login[key].finish(null);
      await login;
    }
    window.mock.holdReads = false;
    window.mock[holdWrites] = false;
    (window.mock.heldReads.shift() ?? window.mock[heldWrites].shift())();
  }, { heldWrites, holdWrites });
}

async function assertOAuthPreserved(page) {
  const config = await saved(page);
  for (const { key, id } of services) {
    assert.equal(config.credentials[id], 'new-' + key);
    assert.equal(config[key].callbackPort, 43421);
  }
}

test('the goals toggle preserves both OAuth settings committed before its save', async t => withSettings(t, async page => {
  const toggle = page.getByRole('checkbox', { name: 'コメビュに表示' });
  await toggle.waitFor();
  await page.evaluate(() => {
    window.mock.holdReads = true;
    window.mock.holdGoalsWrites = true;
  });
  await toggle.click();
  await commitOAuthBeforeWrite(page, 'heldGoalsWrites', 'holdGoalsWrites');
  await page.waitForFunction(() => window.mock.saved.goals.showInApp === true);
  assert.equal(await toggle.isChecked(), true);
  await assertOAuthPreserved(page);
}, 'app'));

test('comment NG append preserves both OAuth settings committed before its save', async t => withSettings(t, async page => {
  await page.getByRole('checkbox', { name: 'コメビュに表示' }).waitFor();
  await page.evaluate(() => window.mock.pushComment({
    id: 'ng-comment', platform: 'twitch', channel: 'test-channel', kind: 'normal',
    timestampMs: Date.now(),
    author: { id: 'test-user', name: 'Test Author', badges: [], roles: {} },
    fragments: [{ type: 'text', text: 'Test comment' }],
  }));
  await page.getByRole('button', { name: 'コメント操作メニューを開く' }).click();
  await page.evaluate(() => {
    window.mock.holdReads = true;
    window.mock.holdModerationWrites = true;
  });
  await page.getByRole('menuitem', { name: 'この人をNGユーザーに追加' }).click();
  await commitOAuthBeforeWrite(page, 'heldModerationWrites', 'holdModerationWrites');
  await page.waitForFunction(() => window.mock.saved.moderation.ngUsers.includes('Test Author'));
  await assertOAuthPreserved(page);
  assert.deepEqual((await saved(page)).moderation, { ngWords: [], ngUsers: ['Test Author'], highlights: [] });
  assert.equal(await page.evaluate(() => window.mock.writes.length), 0);
}, 'app'));

test('moderation import preserves both OAuth settings committed before its save', async t => withSettings(t, async page => {
  const moderation = { ngWords: ['spam'], ngUsers: ['bot'], highlights: ['important'] };
  await page.evaluate(() => {
    window.mock.holdReads = true;
    window.mock.holdModerationWrites = true;
  });
  await importModeration(page, moderation);
  await commitOAuthBeforeWrite(page, 'heldModerationWrites', 'holdModerationWrites');
  await page.getByText('モデレーション規則をインポートしました。', { exact: true }).waitFor();
  await assertOAuthPreserved(page);
  assert.deepEqual((await saved(page)).moderation, moderation);
  assert.equal(await page.evaluate(() => window.mock.writes.length), 0);
}));


const twitcastingPort = page => page.locator('#twitcasting-port');
const spamRules = { ngWords: ['spam'], ngUsers: [], highlights: [] };

async function startTwitcasting(page, port) {
  await page.locator('#twitcasting-client-id').fill('client-' + port);
  await twitcastingPort(page).fill(port);
  await page.getByRole('button', { name: 'ログイン', exact: true }).click();
  await page.waitForFunction(() => !!window.mock.login.twitcasting);
}

async function commitTwitcasting(page, failRead = false) {
  const before = await page.evaluate(() => ({ reads: window.mock.reads, reloading: window.mock.configSync.reloading }));
  await page.evaluate(fail => {
    window.mock.login.twitcasting.accept();
    if (fail) window.mock.readsToFail = 1;
    window.mock.login.twitcasting.finish(new Error('cancelled'));
  }, failRead);
  await page.waitForFunction(
    ({ reads, reloading }) => window.mock.reads > reads && window.mock.configSync.reloading === reloading,
    before,
  );
}

test('manual reload uses 40322 when TwitCasting commits it before the 40321 reply', async t => withSettings(t, async page => {
  await startTwitcasting(page, '40321');
  await commitTwitcasting(page, true);

  // The login can already be pending when the manual reload locks the fields.
  await startTwitcasting(page, '40322');
  await page.evaluate(() => { window.mock.holdReads = true; });
  await reloadSettings(page).click();
  await page.waitForFunction(() => window.mock.heldReads.length === 1);
  assert.equal(await page.locator('#xpost-port').isDisabled(), true);
  assert.equal(await save(page).isDisabled(), true);

  await page.evaluate(() => { window.mock.holdReads = false; });
  await commitTwitcasting(page);
  assert.equal((await saved(page)).twitcasting.callbackPort, 40322);
  // The older reply is not newer than the base advanced by the 40322 notice, so it changes nothing.
  await page.evaluate(() => window.mock.heldReads.shift()());
  await page.waitForFunction(() => window.mock.configSync.reloading === 0);
  assert.equal(await twitcastingPort(page).inputValue(), '40322');
  // The earlier failed read still asks for a reload; reading the base revision clears it.
  await reloadSettings(page).click();
  assert.equal((await saveSettings(page)).twitcasting.callbackPort, 40322);
  assert.equal(await page.evaluate(() => window.mock.rejected), 0);
}));

test('moderation reload keeps OAuth 40322 when its held reply contains 39822', async t => withSettings(t, async page => {
  await page.evaluate(() => { window.mock.holdReads = true; });
  await importModeration(page, spamRules);
  await page.waitForFunction(() => window.mock.heldReads.length === 1);
  assert.equal(await page.getByRole('textbox', { name: 'NGワードを追加', exact: true }).isDisabled(), true);
  assert.equal(await save(page).isDisabled(), true);
  assert.equal(await twitcastingPort(page).isEnabled(), true);

  await page.evaluate(() => { window.mock.holdReads = false; });
  await startTwitcasting(page, '40322');
  await commitTwitcasting(page);
  assert.equal((await saved(page)).twitcasting.callbackPort, 40322);
  assert.equal(await save(page).isDisabled(), true);
  await page.evaluate(() => window.mock.heldReads.shift()());
  await page.getByText('モデレーション規則をインポートしました。', { exact: true }).waitFor();
  const config = await saveSettings(page);
  assert.equal(config.twitcasting.callbackPort, 40322);
  assert.deepEqual(config.moderation, spamRules);
}));

test('a successful full reload supersedes the pending moderation import reload', async t => withSettings(t, async page => {
  await startTwitcasting(page, '40321');
  await commitTwitcasting(page, true);
  await reloadSettings(page).waitFor();
  await page.evaluate(() => { window.mock.holdReads = true; });
  await importModeration(page, spamRules);
  await page.waitForFunction(() => window.mock.heldReads.length === 1);

  await page.evaluate(() => { window.mock.holdReads = false; });
  assert.equal(await reloadSettings(page).isEnabled(), true);
  await reloadSettings(page).click();
  await page.waitForFunction(() => !window.mock.configSync.reloadRequired);
  await page.evaluate(() => window.mock.heldReads.shift()());
  await page.getByText('モデレーション規則をインポートしました。', { exact: true }).waitFor();
  assert.equal(await page.evaluate(() => window.mock.configSync.reloadRequired), false);
  assert.deepEqual((await saveSettings(page)).moderation, spamRules);
}));

for (const outcome of ['failed', 'unanswered']) {
  test('moderation reload (' + outcome + ') releases import and blocks saving over spam', async t => withSettings(t, async page => {
    if (outcome === 'failed') {
      await page.evaluate(() => { window.mock.readsToFail = 1; });
      await importModeration(page, spamRules);
    } else {
      await page.clock.install();
      await page.evaluate(() => { window.mock.holdReads = true; });
      await importModeration(page, spamRules);
      await page.waitForFunction(() => window.mock.heldReads.length === 1);
      assert.equal(await moderationImport(page).getByRole('button', { name: '読込中...' }).isDisabled(), true);
      await page.clock.fastForward(10_000);
      await page.evaluate(() => { window.mock.holdReads = false; });
    }

    await reloadSettings(page).waitFor();
    assert.equal(await reloadSettings(page).isEnabled(), true);
    assert.equal(await moderationImport(page).getByRole('button', { name: 'テキストから読込' }).isEnabled(), true);
    assert.equal(await save(page).isDisabled(), true);
    assert.deepEqual((await saved(page)).moderation.ngWords, ['spam']);
    assert.equal(await page.evaluate(() => window.mock.writes.length), 0);
    await reloadSettings(page).click();
    assert.deepEqual((await saveSettings(page)).moderation.ngWords, ['spam']);
  }));
}

test('an unanswered full import reload releases the form for a manual retry', async t => withSettings(t, async page => {
  const config = await initial(page);
  config.twitcasting.callbackPort = 40322;
  await page.clock.install();
  await page.evaluate(() => { window.mock.holdReads = true; });
  await importConfig(page, { config });
  await page.waitForFunction(() => window.mock.heldReads.length === 1);
  assert.equal(await twitcastingPort(page).isDisabled(), true);
  assert.equal(await save(page).isDisabled(), true);

  await page.clock.fastForward(10_000);
  await page.evaluate(() => { window.mock.holdReads = false; });
  await reloadSettings(page).waitFor();
  assert.equal(await twitcastingPort(page).isEnabled(), true);
  assert.equal(await save(page).isDisabled(), true);
  await reloadSettings(page).click();
  assert.equal((await saveSettings(page, 2)).twitcasting.callbackPort, 40322);
}));

for (const [kind, action] of [
  ['ngUsers', 'この人をNGユーザーに追加'],
  ['ngWords', 'この語をNGワードに追加'],
]) {
  test(kind + ' save failure shows a toast without leaking the value or rejecting', async t => withSettings(t, async page => {
    const pageErrors = [];
    page.on('pageerror', error => pageErrors.push(error));
    await page.getByRole('checkbox', { name: 'コメビュに表示' }).waitFor();
    await page.evaluate(() => {
      window.mock.failModerationAppend = true;
      window.mock.pushComment({
        id: 'ng-save-failure', platform: 'twitch', channel: 'test-channel', kind: 'normal',
        timestampMs: Date.now(),
        author: { id: 'test-user', name: 'Private NG user', badges: [], roles: {} },
        fragments: [{ type: 'text', text: 'Private NG word' }],
      });
    });
    await page.getByRole('button', { name: 'コメント操作メニューを開く' }).click();
    await page.getByRole('menuitem', { name: action }).click();
    const toast = page.getByRole('alert').filter({ hasText: 'NG 規則を保存できませんでした。' });
    await toast.waitFor();
    assert.equal((await toast.textContent()).trim(), 'NG 規則を保存できませんでした。');
    assert.deepEqual(pageErrors, []);
    assert.deepEqual((await saved(page)).moderation[kind], []);
  }, 'app'));
}

test('a moderation import committed after reopening settings rejects the stale form save', async t => withSettings(t, async page => {
  const settingsTab = page.getByRole('tab', { name: '設定', exact: true });
  await settingsTab.click();
  await page.locator('#moderation-import-json').waitFor();
  await page.evaluate(() => { window.mock.holdModerationWrites = true; });
  await importModeration(page, spamRules);
  await page.waitForFunction(() => window.mock.heldModerationWrites.length === 1);

  // The reopened form reads the rules from before the pending import.
  await page.getByRole('tab', { name: 'コメント', exact: true }).click();
  await settingsTab.click();
  await save(page).waitFor();
  await page.waitForFunction(() => window.mock.configSync.reloading === 0);
  await page.evaluate(() => window.mock.heldModerationWrites.shift()());
  await page.waitForFunction(() => window.mock.saved.moderation.ngWords.length === 1);

  await save(page).click();
  await page.waitForFunction(() => window.mock.rejected === 1);
  await page.locator('.save-row').getByText('設定を読み直してください', { exact: true }).waitFor();
  assert.equal(await page.evaluate(() => window.mock.writes.length), 0);
  assert.deepEqual((await saved(page)).moderation, spamRules);
  assert.equal(await save(page).isDisabled(), true);

  await reloadSettings(page).click();
  assert.deepEqual((await saveSettings(page)).moderation, spamRules);
}, 'app'));

test('the goals toggle waits for a fresh config after a full import from a closed settings tab', async t => withSettings(t, async page => {
  await page.getByRole('tab', { name: '設定', exact: true }).click();
  await page.locator('#settings-import-json').waitFor();
  const config = await initial(page);
  config.goals.showInApp = true;
  await page.evaluate(() => { window.mock.holdReads = true; });
  await importConfig(page, { config });
  await page.waitForFunction(() => window.mock.saved.goals.showInApp === true && window.mock.heldReads.length > 0);

  await page.getByRole('tab', { name: 'コメント', exact: true }).click();
  const toggle = page.getByRole('checkbox', { name: 'コメビュに表示' });
  await toggle.waitFor();
  assert.equal(await toggle.isDisabled(), true);
  assert.equal(await toggle.isChecked(), false);

  await page.evaluate(() => {
    window.mock.holdReads = false;
    while (window.mock.heldReads.length) window.mock.heldReads.shift()();
  });
  await page.waitForFunction(() => {
    const input = [...document.querySelectorAll('.goals-panel-toggle input')][0];
    return input && !input.disabled && input.checked;
  });
  assert.equal((await saved(page)).goals.showInApp, true);
}, 'app'));

const rejected = page => page.evaluate(() => window.mock.rejected);
const writes = page => page.evaluate(() => window.mock.writes.length);
const saveRowReload = page => page.locator('.save-row').getByText('設定を読み直してください', { exact: true });

test('an NG append whose notice is still pending rejects the stale full save', async t => withSettings(t, async page => {
  await page.evaluate(() => {
    window.mock.holdAppendNotices = true;
    void window.__TAURI_INTERNALS__.invoke('append_moderation_value', { kind: 'ngWords', value: 'spam' });
  });
  await page.waitForFunction(() => window.mock.heldAppendNotices.length === 1);
  await save(page).click();
  await page.waitForFunction(() => window.mock.rejected === 1);
  await page.evaluate(() => window.mock.heldAppendNotices.shift()());
  await saveRowReload(page).waitFor();
  assert.equal(await writes(page), 0);
  assert.deepEqual((await saved(page)).moderation.ngWords, ['spam']);
}));

test('a held goals save then an older settings read leaves the toggle matching the saved value', async t => withSettings(t, async page => {
  const toggle = page.getByRole('checkbox', { name: 'コメビュに表示' });
  await toggle.waitFor();
  await page.evaluate(() => { window.mock.holdGoalsWrites = true; });
  await toggle.click();
  await page.waitForFunction(() => window.mock.heldGoalsWrites.length === 1);
  await page.evaluate(() => { window.mock.holdReads = true; });
  await page.getByRole('tab', { name: '設定', exact: true }).click();
  await page.waitForFunction(() => window.mock.heldReads.length === 1);
  await page.evaluate(() => window.mock.heldGoalsWrites.shift()());
  await page.waitForFunction(() => window.mock.saved.goals.showInApp === true);
  await page.evaluate(() => {
    window.mock.holdReads = false;
    window.mock.heldReads.shift()();
  });
  await page.waitForFunction(() => window.mock.configSync.reloading === 0);
  await page.getByRole('tab', { name: 'コメント', exact: true }).click();
  await toggle.waitFor();
  await page.waitForFunction(() => document.querySelector('.goals-panel-toggle input')?.disabled === false);
  assert.equal(await toggle.isChecked(), true);
  assert.equal((await saved(page)).goals.showInApp, true);
}, 'app'));

test('the main window backs off and stops retrying a failing config read', async t => withSettings(t, async page => {
  const toggle = page.getByRole('checkbox', { name: 'コメビュに表示' });
  await toggle.waitFor();
  await page.clock.install();
  const start = await page.evaluate(async () => {
    // Another path saves, and some screen learns the new revision.
    await window.__TAURI_INTERNALS__.invoke('set_moderation', { moderation: { ngWords: ['spam'], ngUsers: [], highlights: [] } });
    window.mock.readsToFail = 100;
    const reads = window.mock.reads;
    window.mock.configSync.revision = window.mock.revision;
    return reads;
  });
  const readsAfter = n => page.waitForFunction(n => window.mock.reads === n, start + n);
  await readsAfter(1);
  for (const [wait, total] of [[1000, 2], [2000, 3], [4000, 4]]) {
    await page.clock.fastForward(wait);
    await readsAfter(total);
  }
  await page.clock.fastForward(60_000);
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(resolve)));
  assert.equal(await page.evaluate(() => window.mock.reads), start + 4);
  assert.equal(await toggle.isDisabled(), true);
  await page.locator('.goals-panel').getByText('設定を読み直してください', { exact: true }).waitFor();
}, 'app'));

test('a retention normalized by Rust does not reject the next save', async t => withSettings(t, async page => {
  await page.locator('#log-retention').fill('3651');
  assert.equal((await saveSettings(page)).log.retentionDays, 3650);
  await page.waitForFunction(() => document.getElementById('log-retention').value === '3650');
  await page.locator('#log-retention').fill('30');
  assert.equal((await saveSettings(page, 2)).log.retentionDays, 30);
  assert.equal(await rejected(page), 0);
}));

test('an X disconnect from another path rejects an API-mode form save as a conflict', async t => withSettings(t, async page => {
  await page.getByRole('tab', { name: '設定', exact: true }).waitFor();
  await page.evaluate(() => {
    window.mock.saved.xPost.mode = 'api';
    window.mock.status.xPost = { ...window.mock.status.xPost, connected: true, username: 'current-x' };
  });
  await page.getByRole('tab', { name: '設定', exact: true }).click();
  await save(page).waitFor();
  await page.waitForFunction(() => window.mock.configSync.reloading === 0);
  await page.evaluate(() => window.__TAURI_INTERNALS__.invoke('disconnect_x_post', {}));
  await save(page).click();
  await page.waitForFunction(() => window.mock.rejected === 1);
  await saveRowReload(page).waitFor();
  assert.equal(await page.evaluate(() => window.mock.configSync.reloadRequired), true);
  assert.equal(await writes(page), 0);
}, 'app'));

for (const outcome of ['ok', 'fail', 'cancel']) {
  test('YouTube OAuth connect (' + outcome + ') then another edit saves without a conflict', async t => withSettings(t, async page => {
    await page.evaluate(outcome => { window.mock.youtubeConnect = outcome; }, outcome);
    await page.locator('#cred-youtube-oauth-client-id').fill('yt-client');
    await page.getByRole('button', { name: 'Googleに接続', exact: true }).click();
    await page.waitForFunction(() => window.mock.saved.credentials.youtubeOauthClientId === 'yt-client');
    await page.waitForFunction(() => !document.querySelector('.youtube-connect-btn')?.textContent.includes('接続待ち'));
    await page.locator('#log-retention').fill('10');
    const next = await saveSettings(page);
    assert.equal(next.log.retentionDays, 10);
    assert.equal(next.credentials.youtubeOauthClientId, 'yt-client');
    assert.equal(await rejected(page), 0);
  }));
}

test('disconnecting X in API mode from the form keeps the next save valid', async t => withSettings(t, async page => {
  await page.getByRole('tab', { name: '設定', exact: true }).waitFor();
  await page.evaluate(() => {
    window.mock.saved.xPost.mode = 'api';
    window.mock.status.xPost = { ...window.mock.status.xPost, connected: true, username: 'current-x' };
  });
  await page.getByRole('tab', { name: '設定', exact: true }).click();
  await save(page).waitFor();
  await page.getByRole('button', { name: '切断', exact: true }).click();
  await page.waitForFunction(() => window.mock.saved.xPost.mode === 'intent');
  await page.getByText(/X との接続を切りました/).waitFor();
  const next = await saveSettings(page);
  assert.equal(next.xPost.mode, 'intent');
  assert.equal(await rejected(page), 0);
}, 'app'));

test('the form is disabled while its full save response is pending', async t => withSettings(t, async page => {
  await page.locator('#log-retention').fill('10');
  await page.evaluate(() => { window.mock.holdUpdates = true; });
  await save(page).click();
  await page.waitForFunction(() => window.mock.heldUpdates.length === 1);
  assert.equal(await page.locator('#log-retention').isDisabled(), true);
  await page.evaluate(() => { window.mock.holdUpdates = false; window.mock.heldUpdates.shift()(); });
  await page.getByText('保存しました', { exact: true }).waitFor();
  assert.equal(await page.locator('#log-retention').isEnabled(), true);
  assert.equal(await page.locator('#log-retention').inputValue(), '10');
}));

test('a TwitCasting save notice delivered after the login settles does not overwrite a new edit', async t => withSettings(t, async page => {
  await page.evaluate(() => { window.mock.holdConfigSaved = true; });
  await startTwitcasting(page, '40322');
  await commitTwitcasting(page);
  await page.waitForFunction(() => document.getElementById('twitcasting-port').disabled === false);
  await twitcastingPort(page).fill('40323');
  await page.evaluate(() => window.mock.heldConfigSaved.shift()());
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(resolve)));
  assert.equal(await twitcastingPort(page).inputValue(), '40323');
}));

test('an older 39822 read after the 40322 notice keeps the form and base at 40322', async t => withSettings(t, async page => {
  await page.evaluate(() => { window.mock.readsToFail = 1; });
  await importModeration(page, spamRules);
  await reloadSettings(page).waitFor();
  await startTwitcasting(page, '40322');
  await page.evaluate(() => { window.mock.holdReads = true; });
  await reloadSettings(page).click();
  await page.waitForFunction(() => window.mock.heldReads.length === 1);
  await page.evaluate(() => window.mock.login.twitcasting.accept());
  await page.evaluate(() => {
    window.mock.holdReads = false;
    window.mock.heldReads.shift()();
  });
  await page.waitForFunction(() => window.mock.configSync.reloading === 0);
  assert.equal(await twitcastingPort(page).inputValue(), '40322');
  await page.evaluate(() => window.mock.login.twitcasting.finish(new Error('cancelled')));
  await page.waitForFunction(() => window.mock.configSync.reloading === 0 && !window.mock.login.twitcasting);
  await reloadSettings(page).click();
  const next = await saveSettings(page);
  assert.equal(next.twitcasting.callbackPort, 40322);
  assert.deepEqual(next.moderation, spamRules);
  assert.equal(await rejected(page), 0);
}));

test('the main window reads again after giving up once settings reload and an OAuth save', async t => withSettings(t, async page => {
  await page.getByRole('checkbox', { name: 'コメビュに表示' }).waitFor();
  await page.clock.install();
  const start = await page.evaluate(async () => {
    await window.__TAURI_INTERNALS__.invoke('set_moderation', { moderation: { ngWords: ['spam'], ngUsers: [], highlights: [] } });
    window.mock.readsToFail = 4;
    const reads = window.mock.reads;
    window.mock.configSync.revision = window.mock.revision;
    return reads;
  });
  await page.waitForFunction(n => window.mock.reads === n, start + 1);
  for (const [wait, total] of [[1000, 2], [2000, 3], [4000, 4]]) {
    await page.clock.fastForward(wait);
    await page.waitForFunction(n => window.mock.reads === n, start + total);
  }
  await page.getByRole('tab', { name: '設定', exact: true }).click();
  await page.locator('details').filter({ hasText: '自分の ClientID を使う' }).locator('summary').click();
  await page.waitForFunction(() => window.mock.configSync.reloading === 0);
  const before = await page.evaluate(() => window.mock.reads);
  await startTwitcasting(page, '40322');
  await page.evaluate(() => window.mock.login.twitcasting.accept());
  // The App learns revision 2 from the OAuth save and reads again despite the earlier failures.
  await page.waitForFunction(n => window.mock.reads > n, before);
  await page.evaluate(() => window.mock.login.twitcasting.finish(new Error('cancelled')));
  await page.getByRole('tab', { name: 'コメント', exact: true }).click();
  await page.waitForFunction(() => document.querySelector('.goals-panel-toggle input')?.disabled === false);
}, 'app'));

test('an own moderation import answered at base+2 asks for a reload', async t => withSettings(t, async page => {
  await page.evaluate(() => { window.mock.holdModerationWrites = true; });
  await importModeration(page, spamRules);
  await page.waitForFunction(() => window.mock.heldModerationWrites.length === 1);
  await page.evaluate(() => window.__TAURI_INTERNALS__.invoke('set_goals_show_in_app', { showInApp: true }));
  await page.evaluate(() => {
    window.mock.holdModerationWrites = false;
    window.mock.heldModerationWrites.shift()();
  });
  await page.waitForFunction(() => window.mock.configSync.reloadRequired === true);
  await page.waitForFunction(() => window.mock.configSync.importing === null && window.mock.configSync.reloading === 0);
  assert.equal(await save(page).isDisabled(), true);
  await reloadSettings(page).click();
  const next = await saveSettings(page);
  assert.deepEqual(next.moderation, spamRules);
  assert.equal(next.goals.showInApp, true);
}));

test('the whole app opens and switches tabs without Tauri', async t => withSettings(t, async page => {
  await page.getByRole('tab', { name: '設定', exact: true }).click();
  await page.locator('#settings-import-json').waitFor();
  await page.getByRole('tab', { name: 'コメント', exact: true }).click();
  await page.getByRole('tab', { name: '設定', exact: true }).waitFor();
  // The main thread stays responsive (no synchronous reload loop).
  await page.evaluate(() => new Promise(resolve => setTimeout(resolve, 0)));
}, 'browser-app'));

test('a late X disconnect notice newer than the base asks for a reload', async t => withSettings(t, async page => {
  await page.getByRole('tab', { name: '設定', exact: true }).waitFor();
  await page.evaluate(() => {
    window.mock.saved.xPost.mode = 'api';
    window.mock.status.xPost = { ...window.mock.status.xPost, connected: true, username: 'current-x' };
    window.mock.holdConfigSaved = true;
  });
  await page.getByRole('tab', { name: '設定', exact: true }).click();
  await save(page).waitFor();
  await page.waitForFunction(() => window.mock.configSync.reloading === 0);
  await page.getByRole('button', { name: '切断', exact: true }).click();
  await page.getByText(/X との接続を切りました/).waitFor();
  assert.equal(await page.evaluate(() => window.mock.configSync.reloadRequired), false);
  await page.evaluate(() => window.mock.heldConfigSaved.shift()());
  await page.waitForFunction(() => window.mock.configSync.reloadRequired === true);
  assert.equal(await save(page).isDisabled(), true);
}, 'app'));
