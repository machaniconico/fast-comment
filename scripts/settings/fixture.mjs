import { mount } from 'svelte';
import App from '../../src/App.svelte';
import Settings from '../../src/lib/components/Settings.svelte';
import ConfigPortability from '../../src/lib/components/ConfigPortability.svelte';
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
const mock = window.mock = {
  initial,
  saved: structuredClone(initial),
  writes: [],
  reads: 0,
  status: {
    twitcasting: { clientId: 'saved-tc', configured: true, state: 'loggedOut' },
    xPost: { clientId: 'saved-x', configured: true, connected: false },
  },
  readsToFail: 0,
  heldReads: [],
  holdReads: false,
  heldGoalsWrites: [],
  holdGoalsWrites: false,
  heldModerationWrites: [],
  holdModerationWrites: false,
  failModerationAppend: false,
  login: {},
  cancels: [],
  callbacks: new Map(),
  appearance: () => theme.getSnapshot(),
  pushComment: comment => store.pushBatch([comment]),
};

window.confirm = () => true;

if (location.search === '?browser') {
  mount(ConfigPortability, { target: document.body });
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
          const snapshot = structuredClone(mock.saved);
          return mock.holdReads
            ? new Promise(resolve => mock.heldReads.push(() => resolve(snapshot)))
            : snapshot;
        }
        case 'update_config': {
          const next = JSON.parse(JSON.stringify(args.newConfig));
          const protectedOAuth = [];
          for (const [service, id] of Object.entries(keys)) {
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
          mock.saved = next;
          return { warnings: [], protectedOAuth };
        }
        case 'set_goals_show_in_app':
          if (mock.holdGoalsWrites) {
            await new Promise(resolve => mock.heldGoalsWrites.push(resolve));
          }
          mock.saved.goals.showInApp = args.showInApp;
          return null;
        case 'append_moderation_value':
        case 'set_moderation': {
          if (mock.holdModerationWrites) {
            await new Promise(resolve => mock.heldModerationWrites.push(resolve));
          }
          if (command === 'set_moderation') {
            mock.saved.moderation = structuredClone(args.moderation);
            return null;
          }
          if (mock.failModerationAppend) throw new Error(args.value);
          const value = args.value.trim();
          const entries = mock.saved.moderation[args.kind];
          if (value && !entries.includes(value)) entries.push(value);
          return value || null;
        }
        case 'cancel_twitcasting_login':
        case 'cancel_x_post_login':
          mock.cancels.push({ command, attempt: args.attempt });
          return null;
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
                if (args.onConfigSaved) {
                  mock.callbacks.get(Number(String(args.onConfigSaved.toJSON()).split(':').at(-1)))?.({
                    index: 0, message: null,
                  });
                }
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
