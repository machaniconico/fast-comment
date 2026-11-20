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
  if (mode !== 'app') await page.locator('#settings-import-json').waitFor();
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

  test(service.name + ' ignores a reconnect read that predates the imported config', async t => withSettings(t, async page => {
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
  }));

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

test('X cancels its pending login when the settings tab closes', async t => withSettings(t, async page => {
  await page.getByRole('tab', { name: '設定', exact: true }).click();
  await page.locator('#xpost-client-id').fill('new-client');
  await page.getByRole('button', { name: 'X に接続', exact: true }).click();
  const attempt = await page.evaluate(() => window.mock.login.xPost.attempt);

  await page.getByRole('tab', { name: 'コメント', exact: true }).click();
  await page.waitForFunction(() => window.mock.cancels.length === 1);
  assert.deepEqual(await page.evaluate(() => window.mock.cancels), [
    { command: 'cancel_x_post_login', attempt },
  ]);
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
  const input = page.locator('#moderation-import-json');
  await input.fill(JSON.stringify({ moderation }));
  await page.evaluate(() => {
    window.mock.holdReads = true;
    window.mock.holdModerationWrites = true;
  });
  await page.locator('.portability').filter({ has: input }).getByRole('button', { name: 'テキストから読込' }).click();
  await commitOAuthBeforeWrite(page, 'heldModerationWrites', 'holdModerationWrites');
  await page.getByText('モデレーション規則をインポートしました。', { exact: true }).waitFor();
  await assertOAuthPreserved(page);
  assert.deepEqual((await saved(page)).moderation, moderation);
  assert.equal(await page.evaluate(() => window.mock.writes.length), 0);
}));
