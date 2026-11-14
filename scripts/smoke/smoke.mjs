#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { readFile, lstat, rm } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { verifyMinisign } from './minisign.mjs';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '..', '..');
const CONFIG_ID = 'dev.skyfade.fastcomment';

const TEMPLATE_NAMES = ['default', 'simple', 'bubble', 'ticker', 'ranking', 'goals', 'donation', 'timer', 'danmaku'];
const CUSTOM_NAME = 'smoke-copy';
const CUSTOM_ID = `custom:${CUSTOM_NAME}`;

// PR 本文案: 更新・表示の実機確認(このスクリプトはインストーラを実行しない)。
// この PC では release/fast-comment.exe と隣の templates を使って自動確認する。
// 以下は別途インストールを許可した検証環境で実施し、実行結果を PR に追記する。
// 1. default を未使用名に複製し、style.css に目視できる色変更を加えて保存する。
// 2. プレビューと OBS ブラウザソースの custom:<名前> URL で変更を確認する。
//    カスタム CSS の SHA-256 と画面を記録し、アプリを終了する(config.json は開かない)。
// 3. 同じ NSIS インストーラを同じ場所へ上書き実行し、再起動する。
// 4. カスタムが一覧に残り、CSS のハッシュ・プレビュー・OBS の表示が同じことを確認する。
// 5. default / danmaku / goals / timer の既存 URL を保存済み画面と比較する。
//    default は max=3&ttl=4000&font=150、danmaku は max=20、goals/timer は font=125 も確認する。
// 自動の HTTP/資産比較だけでは描画とインストーラ更新後の永続は判定しない。

async function exists(file) {
  try {
    await lstat(file);
    return true;
  } catch (error) {
    if (error.code === 'ENOENT') return false;
    throw error;
  }
}

async function sha256(file) {
  return createHash('sha256').update(await readFile(file)).digest('hex');
}

async function prepareTemplateChecks(exe) {
  if (!process.env.APPDATA) throw new Error('APPDATA がありません。Windows 側で実行してください');
  const copyDir = path.join(process.env.APPDATA, CONFIG_ID, 'templates', CUSTOM_NAME);
  if (await exists(copyDir)) throw new Error(`${CUSTOM_ID} が既に存在します。既存データは変更しません`);
  const builtinDir = path.join(path.dirname(exe), 'templates');
  const cssPath = path.join(builtinDir, 'default', 'style.css');
  return { copyDir, builtinDir, cssPath, beforeHash: await sha256(cssPath), owned: false, stopping: false };
}

async function expectHttp(url, expected, timeoutMs) {
  return fetchWithTimeout(url, {}, timeoutMs, async (response) => {
    if (response.status !== expected) throw new Error(`${url}: HTTP ${response.status} (期待 ${expected})`);
    return response.text();
  });
}

async function runTemplateChecks({ client, obsPort, timeoutMs, record, state }) {
  let step = 'template-list';
  const base = `http://127.0.0.1:${obsPort}`;
  const urls = [`${base}/?template=${CUSTOM_ID}`, `${base}/custom/${CUSTOM_NAME}/style.css`];
  try {
    const templates = await client.invoke('list_templates');
    const builtins = Array.isArray(templates) ? templates.filter((item) => item.kind === 'builtin') : [];
    if (builtins.length !== TEMPLATE_NAMES.length || TEMPLATE_NAMES.some((name) =>
      builtins.filter((item) => item.id === name && item.name === name && item.editable === false).length !== 1)) {
      throw new Error('内蔵 9 件の id/name/kind=builtin/editable=false が一致しません');
    }
    if (templates.some((item) => item.id === CUSTOM_ID) || await exists(state.copyDir)) {
      throw new Error(`${CUSTOM_ID} が既に存在します。既存データは変更しません`);
    }
    // 配信元が exe 隣のリソースであることも確認する。
    const servedCss = await expectHttp(`${base}/default/style.css`, 200, timeoutMs);
    if (servedCss !== await readFile(state.cssPath, 'utf8')) throw new Error('配信 CSS がビルド出力と一致しません');
    record(step, 'PASS', '内蔵 9 件 kind=builtin / editable=false');

    step = 'template-duplicate';
    if (state.stopping) throw new Error('終了処理中');
    // IPC がタイムアウトしても作成が完了しうるので、呼び出す前に後始末を予約する。
    state.owned = true;
    const created = await client.invoke('duplicate_template', { source: 'default', name: CUSTOM_NAME });
    if (created?.id !== CUSTOM_ID || created.kind !== 'custom' || created.editable !== true) {
      throw new Error('duplicate_template の TemplateInfo が不正です');
    }
    for (const url of urls) await expectHttp(url, 200, timeoutMs);
    record(step, 'PASS', `${CUSTOM_ID}: HTML / CSS 200`);

    step = 'template-builtin-readonly';
    let rejection = null;
    try {
      await client.invoke('write_template_file', {
        name: 'default', file: 'style.css', contents: `${servedCss}\n/* smoke readonly probe */\n`,
      });
    } catch (error) {
      rejection = String(error);
    }
    if (!rejection || !rejection.includes('読み取り専用') || !rejection.includes('複製')) {
      throw new Error('内蔵への書き込みが「読み取り専用」「複製」を含むエラーで拒否されませんでした');
    }
    record(step, 'PASS', '内蔵への書き込みを拒否');

    step = 'template-custom-save';
    const editedCss = `${servedCss}\n/* smoke custom save */\nbody { --smoke-copy: 1; }\n`;
    await client.invoke('write_template_file', { name: CUSTOM_ID, file: 'style.css', contents: editedCss });
    if (await client.invoke('read_template_file', { name: CUSTOM_ID, file: 'style.css' }) !== editedCss ||
      await expectHttp(urls[1], 200, timeoutMs) !== editedCss ||
      await readFile(path.join(state.copyDir, 'style.css'), 'utf8') !== editedCss) {
      throw new Error('保存 CSS の IPC / HTTP / ディスクの内容が一致しません');
    }
    record(step, 'PASS', 'カスタム CSS の保存・読み出し・配信が一致');

    step = 'template-delete';
    await client.invoke('delete_template', { id: CUSTOM_ID });
    for (const url of urls) await expectHttp(url, 404, timeoutMs);
    if (await exists(state.copyDir)) throw new Error('削除後も smoke-copy が残っています');
    if ((await client.invoke('list_templates')).some((item) => item.id === CUSTOM_ID)) {
      throw new Error('削除後も一覧に smoke-copy が残っています');
    }
    record(step, 'PASS', '削除後 HTML / CSS 404、ディスク・一覧からも削除');
  } catch (error) {
    record(step, 'FAIL', error instanceof Error ? error.message : String(error));
    const steps = ['template-list', 'template-duplicate', 'template-builtin-readonly', 'template-custom-save', 'template-delete'];
    for (const pending of steps.slice(steps.indexOf(step) + 1)) record(pending, 'FAIL', `${step} 失敗のため未実行`);
  }
}

// 呼び出し元で起動したアプリを停止してから実行する(遅れて完了する IPC との競合を防ぐ)。
async function cleanupTemplateCopy(state, record) {
  try {
    if (state.owned) await rm(state.copyDir, { recursive: true, force: true });
    if (await exists(state.copyDir)) throw new Error('既存の smoke-copy は保持しました');
    record('template-cleanup', 'PASS', 'smoke-copy の残存なし');
  } catch (error) {
    record('template-cleanup', 'FAIL', error instanceof Error ? error.message : String(error));
  }
  try {
    const afterHash = await sha256(state.cssPath);
    record('template-builtin-hash', afterHash === state.beforeHash ? 'PASS' : 'FAIL',
      `SHA-256 before=${state.beforeHash} after=${afterHash}`);
  } catch (error) {
    record('template-builtin-hash', 'FAIL', error instanceof Error ? error.message : String(error));
  }
}

async function checkLegacyTemplateUrls({ obsPort, timeoutMs, state }) {
  const queries = ['default&max=3&ttl=4000&font=150', 'danmaku&max=20', 'goals&font=125', 'timer&font=125'];
  for (const query of queries) {
    const name = query.split('&')[0];
    const base = `http://127.0.0.1:${obsPort}`;
    const html = await expectHttp(`${base}/?template=${query}`, 200, timeoutMs);
    if (!html.includes(`<base href="/${name}/">`) || html !== await expectHttp(`${base}/?template=${name}`, 200, timeoutMs)) {
      throw new Error(`${name}: パラメータ付き HTML / base が一致しません`);
    }
    for (const file of ['style.css', 'app.js']) {
      if (await expectHttp(`${base}/${name}/${file}`, 200, timeoutMs) !==
        await readFile(path.join(state.builtinDir, name, file), 'utf8')) {
        throw new Error(`${name}/${file}: 配信資産がビルド出力と一致しません`);
      }
    }
  }
  return { status: 'PASS', detail: '既存 4 URL・max/ttl/font の HTML / base / CSS / JS が一致(描画は手動確認)' };
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function parseArgs(argv) {
  const args = {
    exe: path.join(repoRoot, 'src-tauri', 'target', 'release', 'fast-comment.exe'),
    attach: false,
    cdpPort: 9333,
    obsPort: null,
    skipUpdater: false,
    downloadInstaller: false,
    timeoutMs: 8000,
  };

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new Error(`${arg} に値がありません`);
      i += 1;
      return argv[i];
    };
    switch (arg) {
      case '--exe':
        args.exe = path.resolve(value());
        break;
      case '--attach':
        args.attach = true;
        break;
      case '--cdp-port':
        args.cdpPort = Number(value());
        break;
      case '--obs-port':
        args.obsPort = Number(value());
        break;
      case '--skip-updater':
        args.skipUpdater = true;
        break;
      case '--download-installer':
        args.downloadInstaller = true;
        break;
      case '--timeout-ms':
        args.timeoutMs = Number(value());
        break;
      case '--help':
        console.log(
          'Usage: node scripts/smoke/smoke.mjs [--exe <path>] [--attach] [--cdp-port 9333] [--obs-port <n>] [--skip-updater] [--download-installer] [--timeout-ms 8000]',
        );
        console.log('OBS ポートは --obs-port または 11180。config.json は読みません。smoke-copy が既存なら停止します。');
        process.exit(0);
        break;
      default:
        throw new Error(`不明な引数: ${arg}`);
    }
  }

  if (!Number.isInteger(args.cdpPort) || args.cdpPort <= 0 || args.cdpPort > 65535) throw new Error('--cdp-port が不正です');
  if (!Number.isInteger(args.timeoutMs) || args.timeoutMs <= 0) throw new Error('--timeout-ms が不正です');
  if (args.obsPort !== null && (!Number.isInteger(args.obsPort) || args.obsPort <= 0 || args.obsPort > 65535)) {
    throw new Error('--obs-port が不正です');
  }
  return args;
}

// read を渡すと本文の読み取りまでタイムアウトの対象にする(ヘッダ到着後の停止で固まらないように)
async function fetchWithTimeout(url, options = {}, timeoutMs = 8000, read = null) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(url, { ...options, signal: controller.signal });
    return read ? await read(response) : response;
  } finally {
    clearTimeout(timer);
  }
}

async function resolveObsPort(cliPort) {
  // config.json には認証情報があるので開かない。変更済みのポートは CLI で指定する。
  return cliPort ?? 11180;
}

async function checkHealth(port, timeoutMs) {
  const deadline = Date.now() + timeoutMs * 2;
  let last = '未接続';
  while (Date.now() < deadline) {
    try {
      const response = await fetchWithTimeout(`http://127.0.0.1:${port}/?template=default`, {}, timeoutMs);
      const contentType = response.headers.get('content-type') ?? '';
      if (response.status === 200 && contentType.includes('text/html')) {
        return { status: 'PASS', detail: '200 text/html' };
      }
      last = `HTTP ${response.status} ${contentType}`;
    } catch (error) {
      last = error instanceof Error ? error.message : String(error);
    }
    await sleep(300);
  }
  return { status: 'FAIL', detail: last };
}

async function checkTemplates(port, timeoutMs) {
  const failures = [];
  for (const name of TEMPLATE_NAMES) {
    try {
      const response = await fetchWithTimeout(`http://127.0.0.1:${port}/?template=${encodeURIComponent(name)}`, {}, timeoutMs);
      if (response.status !== 200) failures.push(`${name}: HTTP ${response.status}`);
    } catch (error) {
      failures.push(`${name}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  const expectStatus = async (template, expected) => {
    try {
      const response = await fetchWithTimeout(
        `http://127.0.0.1:${port}/?template=${encodeURIComponent(template)}`,
        {},
        timeoutMs,
      );
      if (response.status !== expected) failures.push(`${template}: HTTP ${response.status} (期待 ${expected})`);
    } catch (error) {
      failures.push(`${template}: ${error instanceof Error ? error.message : String(error)}`);
    }
  };

  await expectStatus('no-such-template-xyz', 404);
  await expectStatus('../x', 400);

  return failures.length === 0
    ? { status: 'PASS', detail: `${TEMPLATE_NAMES.length + 2} 件` }
    : { status: 'FAIL', detail: failures.join(', ') };
}

function waitForOpen(ws, timeoutMs, label) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      try {
        ws.close();
      } catch {
        // ignore
      }
      reject(new Error(`${label} 接続タイムアウト`));
    }, timeoutMs);
    ws.addEventListener('open', () => {
      clearTimeout(timer);
      resolve();
    }, { once: true });
    ws.addEventListener('error', () => {
      clearTimeout(timer);
      reject(new Error(`${label} 接続失敗`));
    }, { once: true });
  });
}

async function openWebSocket(url, timeoutMs, label = url) {
  const ws = new WebSocket(url);
  await waitForOpen(ws, timeoutMs, label);
  return ws;
}

function watchWebSocket(ws) {
  const messages = [];
  const waiters = [];
  ws.addEventListener('message', (event) => {
    let value;
    try {
      value = JSON.parse(String(event.data));
    } catch {
      return;
    }
    messages.push(value);
    for (let i = waiters.length - 1; i >= 0; i -= 1) {
      const waiter = waiters[i];
      if (waiter.predicate(value)) {
        waiters.splice(i, 1);
        clearTimeout(waiter.timer);
        waiter.resolve(value);
      }
    }
  });

  // 切断されたら待機中の物を即座に失敗させる(タイムアウトまで待たない)
  let closedReason = null;
  const failAll = (reason) => {
    if (closedReason) return;
    closedReason = reason;
    for (const waiter of waiters.splice(0)) {
      clearTimeout(waiter.timer);
      waiter.reject(new Error(reason));
    }
  };
  ws.addEventListener('close', () => failAll('WS が切断されました'));
  ws.addEventListener('error', () => failAll('WS エラー'));

  return {
    messages,
    clear() {
      for (const waiter of waiters.splice(0)) clearTimeout(waiter.timer);
    },
    next(predicate, timeoutMs) {
      for (const message of messages) {
        if (predicate(message)) return Promise.resolve(message);
      }
      if (closedReason) return Promise.reject(new Error(closedReason));
      return new Promise((resolve, reject) => {
        const waiter = { predicate, resolve, reject, timer: null };
        waiter.timer = setTimeout(() => {
          const index = waiters.indexOf(waiter);
          if (index >= 0) waiters.splice(index, 1);
          reject(new Error('WS メッセージ待機タイムアウト'));
        }, timeoutMs);
        waiters.push(waiter);
      });
    },
  };
}

function closeWebSocket(ws) {
  if (!ws) return;
  try {
    ws.close();
  } catch {
    // ignore
  }
}

function pickPageTarget(targets) {
  const pages = targets.filter((target) => target.type === 'page');
  if (pages.length === 0) throw new Error('type=page の CDP ターゲットがありません');
  const mainLike = pages.filter(
    (target) =>
      /^tauri:\/\//.test(target.url ?? '') ||
      /^http:\/\/tauri\.localhost/i.test(target.url ?? ''),
  );
  const pool = mainLike.length > 0 ? mainLike : pages;
  return (
    pool.find((target) => /main/i.test(target.title ?? '') || /main/i.test(target.url ?? '')) ??
    pool[0]
  );
}

function makeCdpClient(ws, timeoutMs) {
  let nextId = 1;
  const pending = new Map();

  ws.addEventListener('message', (event) => {
    let message;
    try {
      message = JSON.parse(String(event.data));
    } catch {
      return;
    }
    if (!message.id || !pending.has(message.id)) return;
    const entry = pending.get(message.id);
    pending.delete(message.id);
    clearTimeout(entry.timer);
    if (message.error) {
      entry.reject(new Error(message.error.message ?? JSON.stringify(message.error)));
    } else {
      entry.resolve(message.result);
    }
  });

  function send(method, params = {}) {
    const id = nextId;
    nextId += 1;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`${method} タイムアウト`));
      }, timeoutMs);
      pending.set(id, { resolve, reject, timer });
      ws.send(JSON.stringify({ id, method, params }));
    });
  }

  async function evaluate(expression) {
    const result = await send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
    });
    if (result.exceptionDetails) {
      const details = result.exceptionDetails;
      // Tauri の Err(String) は CDP の description ではなく value に入る。
      throw new Error(details.exception?.description ?? details.exception?.value ?? details.text ?? 'evaluate 例外');
    }
    return result.result?.value;
  }

  return {
    evaluate,
    invoke(command, args = {}) {
      return evaluate(
        `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}, ${JSON.stringify(args)})`,
      );
    },
    close() {
      closeWebSocket(ws);
    },
  };
}

async function connectCdp(cdpPort, timeoutMs) {
  const deadline = Date.now() + timeoutMs * 2;
  let lastError = null;
  while (Date.now() < deadline) {
    let ws = null;
    try {
      const response = await fetchWithTimeout(
        `http://127.0.0.1:${cdpPort}/json/list`,
        {},
        Math.min(timeoutMs, 2000),
      );
      if (!response.ok) throw new Error(`/json/list HTTP ${response.status}`);
      const target = pickPageTarget(await response.json());
      ws = new WebSocket(target.webSocketDebuggerUrl);
      await waitForOpen(ws, timeoutMs, 'CDP WebSocket');
      const client = makeCdpClient(ws, timeoutMs);
      const available = await client.evaluate(
        'typeof window.__TAURI_INTERNALS__ !== "undefined" && typeof window.__TAURI_INTERNALS__.invoke === "function"',
      );
      if (!available) {
        client.close();
        throw new Error('window.__TAURI_INTERNALS__ が見つかりません');
      }
      return { client, target };
    } catch (error) {
      closeWebSocket(ws);
      lastError = error;
      await sleep(300);
    }
  }
  throw lastError ?? new Error('CDP 接続失敗');
}

function findChatMessage(frame, needle) {
  const list = Array.isArray(frame) ? frame : [frame];
  for (const item of list) {
    if (item && typeof item === 'object' && JSON.stringify(item).includes(needle)) return item;
  }
  return null;
}

function isModerationFrame(frame) {
  return frame && typeof frame === 'object' && !Array.isArray(frame) && Object.prototype.hasOwnProperty.call(frame, 'moderation');
}

async function runChatChecks({ client, obsPort, timeoutMs, record }) {
  let wsA = null;
  let wsB = null;
  let watchA = null;
  let watchB = null;
  try {
    wsA = await openWebSocket(`ws://127.0.0.1:${obsPort}/ws?modEvents=1`, timeoutMs, '/ws?modEvents=1');
    wsB = await openWebSocket(`ws://127.0.0.1:${obsPort}/ws`, timeoutMs, '/ws');
    watchA = watchWebSocket(wsA);
    watchB = watchWebSocket(wsB);

    const unique = `smoke-${Date.now()}`;
    try {
      await client.invoke('inject_test_comment', {
        platform: 'twitch',
        name: 'smoke',
        text: unique,
        count: 1,
      });
    } catch (error) {
      record('ws-chat', 'FAIL', `inject_test_comment 失敗: ${error instanceof Error ? error.message : String(error)}`);
      record('ws-mod', 'FAIL', 'ws-chat 失敗のため未実行');
      record('ws-mod-optin', 'FAIL', 'ws-chat 失敗のため未実行');
      return;
    }

    let messageA = null;
    let messageB = null;
    const match = (frame) => Boolean(findChatMessage(frame, unique));
    try {
      const [frameA, frameB] = await Promise.all([
        watchA.next(match, timeoutMs),
        watchB.next(match, timeoutMs),
      ]);
      messageA = findChatMessage(frameA, unique);
      messageB = findChatMessage(frameB, unique);
    } catch (error) {
      record('ws-chat', 'FAIL', error instanceof Error ? error.message : String(error));
      record('ws-mod', 'FAIL', 'ws-chat 失敗のため未実行');
      record('ws-mod-optin', 'FAIL', 'ws-chat 失敗のため未実行');
      return;
    }

    const message = messageA ?? messageB;
    const id = message?.id;
    const platform = message?.platform;
    const channel = message?.channel;
    if (!id || !platform || !channel) {
      record('ws-chat', 'FAIL', 'ChatMessage の id/platform/channel が欠落');
      record('ws-mod', 'FAIL', 'ws-chat 失敗のため未実行');
      record('ws-mod-optin', 'FAIL', 'ws-chat 失敗のため未実行');
      return;
    }
    record('ws-chat', 'PASS', `id=${id} platform=${platform} channel=${channel}`);

    let modReceived = false;
    try {
      await client.invoke('hide_message', { id, platform, channel });
      const modFrame = await watchA.next(
        (frame) =>
          Array.isArray(frame?.moderation) &&
          frame.moderation.some((event) => event?.type === 'messageDeleted' && event?.messageId === id),
        timeoutMs,
      );
      modReceived = Array.isArray(modFrame.moderation);
      record('ws-mod', 'PASS', `messageDeleted ${id}`);
    } catch (error) {
      record('ws-mod', 'FAIL', error instanceof Error ? error.message : String(error));
    }

    if (modReceived) {
      await sleep(1000);
      const leaked = watchB.messages.some(isModerationFrame);
      record(
        'ws-mod-optin',
        leaked ? 'FAIL' : 'PASS',
        leaked ? 'modEvents 未指定のクライアントへ moderation が届いた' : 'moderation は届かない',
      );
    } else {
      record('ws-mod-optin', 'FAIL', 'ws-mod 失敗のため判定不能');
    }

    try {
      await client.invoke('unhide_message', { id });
    } catch (error) {
      console.warn(`WARN unhide_message 失敗: ${error instanceof Error ? error.message : String(error)}`);
    }
  } finally {
    watchA?.clear();
    watchB?.clear();
    closeWebSocket(wsA);
    closeWebSocket(wsB);
  }
}

async function checkStats(obsPort, timeoutMs) {
  let ws = null;
  let watch = null;
  try {
    ws = await openWebSocket(`ws://127.0.0.1:${obsPort}/stats`, timeoutMs, '/stats');
    watch = watchWebSocket(ws);
    await watch.next(() => true, timeoutMs);
    return { status: 'PASS', detail: '受信あり' };
  } catch (error) {
    return { status: 'FAIL', detail: error instanceof Error ? error.message : String(error) };
  } finally {
    watch?.clear();
    closeWebSocket(ws);
  }
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function isReleaseUrl(url, version) {
  try {
    const parsed = new URL(url);
    if (parsed.protocol !== 'https:' || parsed.hostname !== 'github.com') return false;
    if (url.includes('api.github.com') || url.includes('untagged-')) return false;
    const pattern = new RegExp(
      `^https://github\\.com/[^/]+/[^/]+/releases/download/v${escapeRegExp(version)}/`,
    );
    return pattern.test(url);
  } catch {
    return false;
  }
}

async function checkUpdater({ timeoutMs, downloadInstaller }) {
  const results = [];
  const confPath = path.join(repoRoot, 'src-tauri', 'tauri.conf.json');
  let updater;
  try {
    const conf = JSON.parse(await readFile(confPath, 'utf8'));
    updater = conf?.plugins?.updater;
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    return [
      { name: 'updater-manifest', status: 'FAIL', detail: `tauri.conf.json 読み込み失敗: ${reason}` },
      { name: 'updater-assets', status: 'FAIL', detail: 'manifest 取得失敗' },
      { name: 'updater-signature', status: 'FAIL', detail: 'manifest 取得失敗' },
    ];
  }

  const endpoint = updater?.endpoints?.[0];
  const pubkey = updater?.pubkey;
  if (!endpoint || !pubkey) {
    return [
      { name: 'updater-manifest', status: 'FAIL', detail: 'endpoints[0] または pubkey がありません' },
      { name: 'updater-assets', status: 'FAIL', detail: 'manifest 取得失敗' },
      { name: 'updater-signature', status: 'FAIL', detail: 'manifest 取得失敗' },
    ];
  }

  let manifest = null;
  try {
    manifest = await fetchWithTimeout(endpoint, { redirect: 'follow' }, timeoutMs, async (response) => {
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      return response.json();
    });
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    return [
      { name: 'updater-manifest', status: 'FAIL', detail: reason },
      { name: 'updater-assets', status: 'FAIL', detail: 'manifest 取得失敗' },
      { name: 'updater-signature', status: 'FAIL', detail: 'manifest 取得失敗' },
    ];
  }

  const version = manifest?.version;
  const semverOk = typeof version === 'string' && /^\d+\.\d+\.\d+([-+][0-9A-Za-z.-]+)?$/.test(version);
  const platforms = manifest?.platforms;
  const platformEntries =
    platforms && typeof platforms === 'object' ? Object.entries(platforms) : [];
  const windows = platforms?.['windows-x86_64'];
  const urlEntries = platformEntries.map(([name, info]) => ({ name, url: info?.url, signature: info?.signature }));

  const manifestProblems = [];
  if (!semverOk) manifestProblems.push(`version が semver でない: ${String(version)}`);
  if (!windows) manifestProblems.push('platforms.windows-x86_64 がない');
  if (urlEntries.length === 0) manifestProblems.push('platforms が空');
  for (const entry of urlEntries) {
    if (!entry.url) manifestProblems.push(`${entry.name}: url がない`);
    else if (semverOk && !isReleaseUrl(entry.url, version)) manifestProblems.push(`${entry.name}: url がリリース形式でない`);
  }

  if (manifestProblems.length > 0) {
    results.push({ name: 'updater-manifest', status: 'FAIL', detail: manifestProblems.join(', ') });
    results.push({ name: 'updater-assets', status: 'FAIL', detail: 'manifest 不正のため検証不能' });
    results.push({ name: 'updater-signature', status: 'FAIL', detail: 'manifest 不正のため検証不能' });
    return results;
  }
  results.push({ name: 'updater-manifest', status: 'PASS', detail: `v${version} ${urlEntries.length} 資産` });

  const assetFailures = [];
  for (const entry of urlEntries) {
    try {
      let response = await fetchWithTimeout(entry.url, { method: 'HEAD', redirect: 'follow' }, timeoutMs);
      if (response.status !== 200 && response.status !== 206) {
        response = await fetchWithTimeout(
          entry.url,
          { method: 'GET', headers: { Range: 'bytes=0-0' }, redirect: 'follow' },
          timeoutMs,
        );
      }
      if (response.status !== 200 && response.status !== 206) {
        assetFailures.push(`${entry.name}: HTTP ${response.status}`);
      }
    } catch (error) {
      assetFailures.push(`${entry.name}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
  results.push(
    assetFailures.length === 0
      ? { name: 'updater-assets', status: 'PASS', detail: `${urlEntries.length} 件` }
      : { name: 'updater-assets', status: 'FAIL', detail: assetFailures.join(', ') },
  );

  if (!downloadInstaller) {
    results.push({ name: 'updater-signature', status: 'SKIP', detail: '--download-installer 未指定' });
    return results;
  }

  try {
    const data = await fetchWithTimeout(windows.url, { redirect: 'follow' }, timeoutMs * 20, async (response) => {
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      return Buffer.from(await response.arrayBuffer());
    });
    const verified = verifyMinisign({ pubkeyB64: pubkey, signatureB64: windows.signature, data });
    results.push(
      verified.ok
        ? { name: 'updater-signature', status: 'PASS', detail: `${data.length} bytes` }
        : { name: 'updater-signature', status: 'FAIL', detail: verified.reason },
    );
  } catch (error) {
    results.push({
      name: 'updater-signature',
      status: 'FAIL',
      detail: error instanceof Error ? error.message : String(error),
    });
  }
  return results;
}

async function isCdpReachable(cdpPort) {
  try {
    const response = await fetchWithTimeout(`http://127.0.0.1:${cdpPort}/json/version`, {}, 1000);
    return response.ok;
  } catch {
    return false;
  }
}

// 終了を待てるよう Promise を返す(最大 5 秒)。process 'exit' からは待たずに呼ぶ
async function isObsAnswering(port) {
  try {
    const response = await fetchWithTimeout(`http://127.0.0.1:${port}/?template=default`, {}, 1000);
    return response.ok;
  } catch {
    return false;
  }
}

function killChild(child) {
  if (!child || !child.pid || child.exitCode !== null || child.signalCode !== null) return Promise.resolve(true);
  const exited = new Promise((resolve) => {
    const timer = setTimeout(() => resolve(false), 5000);
    child.once('exit', () => {
      clearTimeout(timer);
      resolve(true);
    });
  });
  try {
    if (process.platform === 'win32') {
      const killer = spawn('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
      killer.on('error', (error) => {
        console.warn(`WARN プロセス終了失敗: ${error.message}`);
      });
    } else {
      child.kill();
    }
  } catch (error) {
    console.warn(`WARN プロセス終了失敗: ${error instanceof Error ? error.message : String(error)}`);
  }
  return exited;
}

async function main() {
  let args;
  try {
    args = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(`ERROR ${error instanceof Error ? error.message : String(error)}`);
    process.exit(2);
  }

  const results = [];
  const record = (name, status, detail = '') => {
    results.push({ name, status, detail });
    console.log(`${status} ${name}${detail ? ` — ${detail}` : ''}`);
  };

  const obsPort = await resolveObsPort(args.obsPort);
  let child = null;
  let cdp = null;
  let templateState = null;
  let cleanupPromise = null;
  const cleanup = () => {
    if (cleanupPromise) return cleanupPromise;
    if (templateState) templateState.stopping = true;
    cleanupPromise = (async () => {
      if (templateState?.owned && cdp) {
        try {
          await cdp.invoke('delete_template', { id: CUSTOM_ID });
        } catch {
          // 削除済み・IPC 失敗時も、停止後に今回作ったディレクトリだけを確認する。
        }
      }
      const stopped = await killChild(child);
      if (!stopped) record('process-cleanup', 'FAIL', '起動したアプリが 5 秒以内に終了しませんでした');
      if (cdp) cdp.close();
      if (templateState) await cleanupTemplateCopy(templateState, record);
    })();
    return cleanupPromise;
  };
  const onSignal = async () => {
    try {
      await cleanup();
    } finally {
      process.exit(130);
    }
  };
  const onExit = () => { void killChild(child); };
  process.on('SIGINT', onSignal);
  process.on('SIGTERM', onSignal);
  process.on('exit', onExit);

  try {
    templateState = await prepareTemplateChecks(args.exe);
    if (!args.attach) {
      if (await isCdpReachable(args.cdpPort)) {
        throw new Error(`CDP ポート ${args.cdpPort} は使用中です。既存アプリへ自動接続しません`);
      } else if (await isObsAnswering(obsPort)) {
        // CDP 無しの通常起動が残っていると、2 つ目を起動して検証対象が食い違う
        throw new Error(`OBS ポート ${obsPort} で既にアプリが動いています。終了してから再実行するか、CDP 付きで起動して --attach を使ってください`);
      } else {
        child = spawn(args.exe, [], {
          env: {
            ...process.env,
            WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${args.cdpPort}`,
          },
          stdio: 'ignore',
          windowsHide: false,
        });
        child.on('error', (error) => {
          console.warn(`WARN アプリ起動失敗: ${error.message}`);
        });
      }
    }

    try {
      const health = await checkHealth(obsPort, args.timeoutMs);
      record('health', health.status, health.detail);
    } catch (error) {
      record('health', 'FAIL', error instanceof Error ? error.message : String(error));
    }

    try {
      const templates = await checkTemplates(obsPort, args.timeoutMs);
      record('templates', templates.status, templates.detail);
    } catch (error) {
      record('templates', 'FAIL', error instanceof Error ? error.message : String(error));
    }

    let cdpClient = null;
    try {
      const connected = await connectCdp(args.cdpPort, args.timeoutMs);
      cdp = connected.client;
      cdpClient = cdp;
      record('cdp', 'PASS', connected.target.title || connected.target.url || 'page');
    } catch (error) {
      record('cdp', 'FAIL', error instanceof Error ? error.message : String(error));
    }

    if (!cdpClient) {
      for (const name of ['template-list', 'template-duplicate', 'template-builtin-readonly', 'template-custom-save', 'template-delete']) {
        record(name, 'FAIL', 'CDP 未接続');
      }
      record('ws-chat', 'FAIL', 'CDP 未接続');
      record('ws-mod', 'FAIL', 'CDP 未接続');
      record('ws-mod-optin', 'FAIL', 'CDP 未接続');
    } else {
      await runTemplateChecks({ client: cdpClient, obsPort, timeoutMs: args.timeoutMs, record, state: templateState });
      try {
        await runChatChecks({ client: cdpClient, obsPort, timeoutMs: args.timeoutMs, record });
      } catch (error) {
        const reason = error instanceof Error ? error.message : String(error);
        record('ws-chat', 'FAIL', reason);
        record('ws-mod', 'FAIL', reason);
        record('ws-mod-optin', 'FAIL', reason);
      }
    }

    try {
      const legacy = await checkLegacyTemplateUrls({ obsPort, timeoutMs: args.timeoutMs, state: templateState });
      record('template-legacy-urls', legacy.status, legacy.detail);
    } catch (error) {
      record('template-legacy-urls', 'FAIL', error instanceof Error ? error.message : String(error));
    }

    try {
      const stats = await checkStats(obsPort, args.timeoutMs);
      record('stats', stats.status, stats.detail);
    } catch (error) {
      record('stats', 'FAIL', error instanceof Error ? error.message : String(error));
    }

    if (args.skipUpdater) {
      record('updater-manifest', 'SKIP', '--skip-updater');
      record('updater-assets', 'SKIP', '--skip-updater');
      record('updater-signature', 'SKIP', '--skip-updater');
    } else {
      try {
        const updaterResults = await checkUpdater({
          timeoutMs: args.timeoutMs,
          downloadInstaller: args.downloadInstaller,
        });
        for (const item of updaterResults) record(item.name, item.status, item.detail);
      } catch (error) {
        const reason = error instanceof Error ? error.message : String(error);
        record('updater-manifest', 'FAIL', reason);
        record('updater-assets', 'FAIL', reason);
        record('updater-signature', 'FAIL', reason);
      }
    }
  } catch (error) {
    record('smoke', 'FAIL', error instanceof Error ? error.message : String(error));
  } finally {
    await cleanup();
    process.removeListener('exit', onExit);
    process.removeListener('SIGINT', onSignal);
    process.removeListener('SIGTERM', onSignal);
  }

  const counts = results.reduce(
    (acc, item) => {
      acc[item.status] = (acc[item.status] ?? 0) + 1;
      return acc;
    },
    { PASS: 0, FAIL: 0, SKIP: 0 },
  );
  console.log(`集計: PASS ${counts.PASS} / FAIL ${counts.FAIL} / SKIP ${counts.SKIP}`);
  process.exit(counts.FAIL > 0 ? 1 : 0);
}

export { prepareTemplateChecks, runTemplateChecks, cleanupTemplateCopy, checkLegacyTemplateUrls, makeCdpClient };

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(`ERROR ${error instanceof Error ? error.stack ?? error.message : String(error)}`);
    process.exit(1);
  });
}
