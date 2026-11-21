import { mount } from 'svelte';
import App from '../../src/App.svelte';
import Settings from '../../src/lib/components/Settings.svelte';
import ConfigPortability from '../../src/lib/components/ConfigPortability.svelte';
import { configSync } from '../../src/lib/configSync.svelte';
import { theme } from '../../src/lib/theme.svelte';
import { store } from '../../src/lib/stores.svelte';

const initial = {
  channels: [],
  obs: { port: 11180, template: 'default', ttlMs: 12000 },
  goals: { enabled: true, showInApp: false },
  timer: {},
  effects: {},
  welcome: {},
  participation: {},
  tts: { backend: 'none', options: {} },
  moderation: { ngWords: [], ngUsers: [], highlights: [] },
  ui: { maxBuffer: 2000, notifySound: false, notifyVolume: 0.5 },
  credentials: { twitcastingClientId: 'saved-tc', xPostClientId: 'saved-x' },
  twitcasting: { callbackPort: 39822 },
  xPost: {
    callbackPort: 47821, enabled: false, mode: 'intent', intentOpen: 'button',
    combine: false, template: '{title} {url}', cooldownSec: 60, maxPostsPerDay: 5,
  },
};

const keys = { twitcasting: 'twitcastingClientId', xPost: 'xPostClientId' };

// Tauri の Channel へ 1 件送る(各 Channel の最初のメッセージは index 0)。
function sendChannel(channel, message) {
  if (!channel) return;
  const deliver = () => mock.callbacks.get(Number(String(channel.toJSON()).split(':').at(-1)))?.({ index: 0, message });
  // 通知の配送だけを遅らせられる(保存とコマンドの応答は先に進む)。
  if (mock.holdConfigSaved) mock.heldConfigSaved.push(deliver);
  else deliver();
}
const mock = window.mock = {
  initial,
  saved: structuredClone(initial),
  // Rust の config_revision と同じく、保存に成功するたびに 1 進む。
  revision: 0,
  rejected: 0,
  writes: [],
  baseRevisions: [],
  reads: 0,
  status: {
    twitcasting: { clientId: 'saved-tc', configured: true, state: 'loggedOut' },
    xPost: { clientId: 'saved-x', configured: true, connected: false },
  },
  readsToFail: 0,
  updateError: null,
  heldReads: [],
  holdReads: false,
  heldGoalsWrites: [],
  holdGoalsWrites: false,
  heldModerationWrites: [],
  holdModerationWrites: false,
  failModerationAppend: false,
  heldAppendNotices: [],
  holdAppendNotices: false,
  youtubeConnect: 'ok',
  heldUpdates: [],
  holdUpdates: false,
  heldConfigSaved: [],
  holdConfigSaved: false,
  login: {},
  cancels: [],
  callbacks: new Map(),
  configSync,
  appearance: () => theme.getSnapshot(),
  pushComment: comment => store.pushBatch([comment]),
};

window.confirm = () => true;

if (location.search === '?browser') {
  mount(ConfigPortability, { target: document.body });
} else if (location.search === '?browser-app') {
  // Tauri なしでアプリ全体を開く(get_config は null になる)。
  mount(App, { target: document.body });
} else {
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: 'main' } },
    transformCallback(callback) {
      const id = mock.callbacks.size + 1;
      mock.callbacks.set(id, callback);
      return id;
    },
    unregisterCallback(id) { mock.callbacks.delete(id); },
    async invoke(command, args) {
      switch (command) {
        case 'plugin:window|get_all_windows': return ['main'];
        case 'get_config': {
          mock.reads++;
          if (mock.readsToFail > 0) {
            mock.readsToFail--;
            throw new Error('config read failed');
          }
          const snapshot = { config: structuredClone(mock.saved), revision: mock.revision };
          return mock.holdReads
            ? new Promise(resolve => mock.heldReads.push(() => resolve(snapshot)))
            : snapshot;
        }
        case 'update_config': {
          if (mock.updateError !== null) throw mock.updateError;
          // Rust と同じく、基準 revision の照合を他の検証より先に行う。
          if (args.baseRevision != null && args.baseRevision !== mock.revision) {
            mock.rejected++;
            throw '設定が別の操作で変更されました。読み直してから保存してください';
          }
          const next = JSON.parse(JSON.stringify(args.newConfig));
          if (next.xPost?.mode === 'api' && !mock.status.xPost.connected) {
            throw 'API モードは X に接続してから選んでください';
          }
          if (next.log?.retentionDays > 3650) next.log.retentionDays = 3650;
          const protectedOAuth = [];
          for (const [service, id] of Object.entries(keys)) {
            if (typeof next[service].callbackPort !== 'number') {
              throw `設定の形式が不正です (項目 ${service}.callbackPort)`;
            }
            next[service].callbackPort = next[service].callbackPort < 1024
              ? initial[service].callbackPort : next[service].callbackPort;
            if (!mock.login[service]) continue;
            if (next[service].callbackPort !== mock.saved[service].callbackPort
                || next.credentials?.[id] !== mock.saved.credentials?.[id]) {
              next[service].callbackPort = mock.saved[service].callbackPort;
              (next.credentials ??= {})[id] = mock.saved.credentials[id];
              protectedOAuth.push(service);
            }
          }
          mock.writes.push(next);
          mock.baseRevisions.push(args.baseRevision ?? null);
          mock.saved = next;
          const outcome = { warnings: [], protectedOAuth, revision: ++mock.revision, config: structuredClone(next) };
          if (mock.holdUpdates) await new Promise(resolve => mock.heldUpdates.push(resolve));
          return outcome;
        }
        case 'set_goals_show_in_app':
          if (mock.holdGoalsWrites) {
            await new Promise(resolve => mock.heldGoalsWrites.push(resolve));
          }
          mock.saved.goals.showInApp = args.showInApp;
          return { revision: ++mock.revision, showInApp: args.showInApp };
        case 'append_moderation_value':
        case 'set_moderation': {
          if (mock.holdModerationWrites) {
            await new Promise(resolve => mock.heldModerationWrites.push(resolve));
          }
          if (command === 'set_moderation') {
            mock.saved.moderation = structuredClone(args.moderation);
            return { revision: ++mock.revision, moderation: structuredClone(mock.saved.moderation) };
          }
          if (mock.failModerationAppend) throw new Error(args.value);
          const value = args.value.trim();
          const entries = mock.saved.moderation[args.kind];
          let revision = null;
          if (value && !entries.includes(value)) {
            entries.push(value);
            revision = ++mock.revision;
          }
          // 保存は済み、完了の通知(応答)だけを止められる。
          if (mock.holdAppendNotices) await new Promise(resolve => mock.heldAppendNotices.push(resolve));
          return { value: value || null, revision, moderation: structuredClone(mock.saved.moderation) };
        }
        case 'cancel_twitcasting_login':
        case 'cancel_x_post_login':
          mock.cancels.push({ command, attempt: args.attempt });
          return null;
        case 'connect_youtube_oauth': {
          if (mock.saved.credentials.youtubeOauthClientId !== args.clientId) {
            mock.saved.credentials.youtubeOauthClientId = args.clientId;
            sendChannel(args.onConfigSaved, { revision: ++mock.revision, youtubeOauthClientId: args.clientId });
          }
          if (mock.youtubeConnect === 'fail') throw 'Google の認可に失敗しました';
          if (mock.youtubeConnect === 'cancel') throw 'ログインを取り消しました';
          return { configured: true, connected: true, channelTitle: 'test-channel' };
        }
        case 'disconnect_x_post': {
          if (mock.saved.xPost.mode === 'api') {
            mock.saved.xPost.mode = 'intent';
            sendChannel(args?.onConfigSaved, { revision: ++mock.revision, xPost: { mode: 'intent' } });
          }
          mock.status.xPost = { ...mock.status.xPost, connected: false, username: null };
          return mock.status.xPost;
        }
        case 'add_channel':
          mock.saved.channels.push(args.channel);
          return { revision: ++mock.revision, channels: structuredClone(mock.saved.channels) };
        case 'get_twitcasting_status': return mock.status.twitcasting;
        case 'get_x_post_status': return mock.status.xPost;
        case 'connect_twitcasting':
        case 'connect_x_post': {
          const service = command === 'connect_twitcasting' ? 'twitcasting' : 'xPost';
          return new Promise((resolve, reject) => {
            mock.login[service] = {
              attempt: args.attempt,
              accept() {
                mock.saved[service].callbackPort = args.callbackPort;
                mock.saved.credentials[keys[service]] = args.clientId;
                // Rust の状態は保存済みの ClientID で答える。
                mock.status[service] = { ...mock.status[service], clientId: args.clientId };
                sendChannel(args.onConfigSaved, {
                  revision: ++mock.revision, clientId: args.clientId, callbackPort: args.callbackPort,
                });
              },
              finish(result) {
                delete mock.login[service];
                if (result instanceof Error) reject(result.message);
                else {
                  if (result) mock.status[service] = result;
                  resolve(result);
                }
              },
            };
          });
        }
        default: return null;
      }
    },
  };
  mount(location.search === '?app' ? App : Settings, { target: document.body });
}
