import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { DEFAULT_TWITCASTING_CALLBACK_PORT, twitcastingCallbackUrl, twitcastingDisplayCallbackUrl } from './twitcasting.ts';

test('既定ポートとコールバック URL は Rust の定数と一致する', () => {
  const rust = readFileSync(new URL('../../src-tauri/src/sources/twitcasting_auth.rs', import.meta.url), 'utf8');
  const port = Number(/pub const CALLBACK_PORT: u16 = (\d+);/.exec(rust)?.[1]);
  const path = /pub const CALLBACK_PATH: &str = "([^"]+)";/.exec(rust)?.[1];
  assert.equal(DEFAULT_TWITCASTING_CALLBACK_PORT, port);
  assert.equal(twitcastingCallbackUrl(40123), `http://localhost:40123${path}`);
});

test('表示 URL は待受中なら進行中のポート、それ以外は入力中の有効値', () => {
  assert.equal(twitcastingDisplayCallbackUrl(null, 40123), 'http://localhost:40123/twitcasting/callback');
  assert.equal(twitcastingDisplayCallbackUrl(39822, 40123), 'http://localhost:39822/twitcasting/callback');
  assert.equal(twitcastingDisplayCallbackUrl(null, null), '');
});
