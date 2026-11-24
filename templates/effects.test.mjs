import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const source = readFileSync(new URL('./effects/app.js', import.meta.url), 'utf8').replace(/\r\n/g, '\n');
const style = readFileSync(new URL('./effects/style.css', import.meta.url), 'utf8');
const html = readFileSync(new URL('./effects/index.html', import.meta.url), 'utf8');
const rustConfig = readFileSync(new URL('../src-tauri/src/config.rs', import.meta.url), 'utf8');
const BEGIN = '  // ---- fc-effects:begin ----';
const END = '  // ---- fc-effects:end ----';

function block() {
  const start = source.indexOf(BEGIN);
  const end = source.indexOf(END);
  assert.ok(start >= 0 && end > start, 'effects logic block markers present');
  return source.slice(start, end + END.length);
}

function load() {
  const factory = new Function(
    `${block()}\nreturn { EFFECTS_PRESETS, EFFECTS_MAX_ACTIVE, effectsParams, effectsFrameItems, effectsPlacement, effectsEvict, effectsWsUrl };`,
  );
  return factory();
}

// 決まった列を返す乱数。尽きたら先頭へ戻る。
function seq(values) {
  let i = 0;
  return () => values[i++ % values.length];
}

test('params are rounded to the same ranges as the Rust config', () => {
  const { effectsParams } = load();
  assert.deepEqual(effectsParams({ emoji: ' 🎉 ', count: 5, animation: 'rain', durationMs: 2000 }), {
    emoji: '🎉', count: 5, animation: 'rain', durationMs: 2000,
  });
  assert.equal(effectsParams({ emoji: '🎉', count: 0 }).count, 1);
  assert.equal(effectsParams({ emoji: '🎉', count: 999 }).count, 50);
  assert.equal(effectsParams({ emoji: '🎉', count: 'x' }).count, 1);
  assert.equal(effectsParams({ emoji: '🎉', durationMs: 10 }).durationMs, 500);
  assert.equal(effectsParams({ emoji: '🎉', durationMs: 1e9 }).durationMs, 15000);
  assert.equal(effectsParams({ emoji: '🎉' }).durationMs, 3000);
  assert.equal(effectsParams({ emoji: '🎉', animation: 'nope' }).animation, 'float');
  assert.equal(effectsParams({ emoji: 'x'.repeat(40) }).emoji.length, 16);
  // 絵文字(サロゲートペア)は文字数で切り、壊さない。
  assert.equal(Array.from(effectsParams({ emoji: '🎉'.repeat(40) }).emoji).length, 16);
});

test('items without a displayable emoji are dropped', () => {
  const { effectsParams } = load();
  for (const bad of [null, undefined, 3, 'x', [], {}, { emoji: '' }, { emoji: '   ' }, { emoji: 5 }]) {
    assert.equal(effectsParams(bad), null, JSON.stringify(bad));
  }
});

test('only {"effects":[…]} frames are accepted; chat and other frames are ignored', () => {
  const { effectsFrameItems } = load();
  const frame = { effects: [{ emoji: 'a', count: 2 }, { emoji: '' }, null, { emoji: 'b' }] };
  assert.deepEqual(effectsFrameItems(frame).map((p) => p.emoji), ['a', 'b']);
  assert.deepEqual(effectsFrameItems([{ id: 'chat' }]), []);
  assert.deepEqual(effectsFrameItems({ achievements: [{ emoji: 'a' }] }), []);
  assert.deepEqual(effectsFrameItems({ effects: 'x' }), []);
  assert.deepEqual(effectsFrameItems(null), []);
  assert.deepEqual(effectsFrameItems('str'), []);
});

test('placement stays within the duration and is deterministic for a given random source', () => {
  const { EFFECTS_PRESETS, effectsPlacement } = load();
  for (const name of EFFECTS_PRESETS) {
    for (const values of [[0], [0.5], [0.999999], [0.1, 0.9, 0.3]]) {
      const p = effectsPlacement(name, 2, 5, 3000, seq(values));
      assert.ok(p.delay >= 0 && p.delay <= 900, `${name} delay ${p.delay}`);
      assert.equal(p.delay + p.dur, 3000, `${name} total`);
      assert.ok(p.dur >= 2100, name);
      for (const key of ['x', 'y', 'dx', 'dy', 'rot', 'size']) {
        assert.ok(Number.isFinite(p[key]), `${name}.${key}`);
      }
      assert.ok(p.size > 0, name);
    }
    const a = effectsPlacement(name, 0, 3, 1000, seq([0.2, 0.4, 0.6, 0.8]));
    const b = effectsPlacement(name, 0, 3, 1000, seq([0.2, 0.4, 0.6, 0.8]));
    assert.deepEqual(a, b, name);
  }
});

test('placement shapes: rain starts above and falls, float starts below and rises, big is centered and large', () => {
  const { effectsPlacement } = load();
  const rain = effectsPlacement('rain', 0, 1, 3000, seq([0.5]));
  assert.ok(rain.y < 0 && rain.dy > 100);
  const float = effectsPlacement('float', 0, 1, 3000, seq([0.5]));
  assert.ok(float.y > 100 && float.dy < 0);
  const big = effectsPlacement('big', 0, 1, 3000, seq([0, 1 - 1e-9]));
  assert.ok(big.size >= 180 && big.x >= 45 && big.x <= 55 && big.y >= 45 && big.y <= 55);
  const burst = effectsPlacement('burst', 0, 8, 3000, seq([0.5]));
  assert.equal(burst.x, 50);
  assert.equal(burst.y, 50);
  assert.ok(Math.hypot(burst.dx / 1.78, burst.dy) >= 25);
});

test('burst elements spread over all directions', () => {
  const { effectsPlacement } = load();
  const angles = [];
  for (let i = 0; i < 8; i += 1) {
    const p = effectsPlacement('burst', i, 8, 3000, seq([0, 0, 0, 0]));
    angles.push(Math.atan2(p.dy, p.dx));
  }
  assert.equal(new Set(angles.map((a) => a.toFixed(3))).size, 8);
});

test('eviction drops the oldest and caps the active elements', () => {
  const { EFFECTS_MAX_ACTIVE, effectsEvict } = load();
  assert.equal(EFFECTS_MAX_ACTIVE, 300);
  assert.deepEqual(effectsEvict(0, 10, 300), { drop: 0, keep: 10 });
  assert.deepEqual(effectsEvict(290, 10, 300), { drop: 0, keep: 10 });
  assert.deepEqual(effectsEvict(295, 10, 300), { drop: 5, keep: 10 });
  assert.deepEqual(effectsEvict(300, 50, 300), { drop: 50, keep: 50 });
  // 1 回の追加が上限を超えるなら、既存を全て消して上限まで。
  assert.deepEqual(effectsEvict(120, 400, 300), { drop: 120, keep: 300 });
  // 結果はいつでも上限以内。
  for (const [active, add] of [[0, 0], [299, 1], [300, 1], [10, 500], [300, 300]]) {
    const { drop, keep } = effectsEvict(active, add, 300);
    assert.ok(active - drop + keep <= 300, `${active}+${add}`);
    assert.ok(drop <= active && drop >= 0);
  }
});

test('ws url keeps the host, honors ?ws=, and always opts in to effectEvents', () => {
  const { effectsWsUrl } = load();
  const loc = { protocol: 'http:', host: '127.0.0.1:11180', href: 'http://127.0.0.1:11180/?template=effects' };
  assert.equal(effectsWsUrl(null, loc), 'ws://127.0.0.1:11180/ws?effectEvents=1');
  assert.equal(
    effectsWsUrl('ws://10.0.0.2:9/ws?channel=a', loc),
    'ws://10.0.0.2:9/ws?channel=a&effectEvents=1',
  );
  assert.equal(
    effectsWsUrl(null, { protocol: 'https:', host: 'h:1', href: 'https://h:1/' }),
    'wss://h:1/ws?effectEvents=1',
  );
});

test('every preset has CSS and matches the Rust preset list', () => {
  const { EFFECTS_PRESETS } = load();
  for (const name of EFFECTS_PRESETS) {
    assert.match(style, new RegExp(`\\.fx-${name}\\s*\\{`), `.fx-${name}`);
    assert.match(style, new RegExp(`@keyframes fx-${name}\\b`), `@keyframes fx-${name}`);
  }
  assert.ok(EFFECTS_PRESETS.length >= 4 && EFFECTS_PRESETS.length <= 6);
  const rust = /EFFECT_ANIMATIONS: \[&str; \d+\] = \[([^\]]*)\]/.exec(rustConfig);
  assert.ok(rust, 'Rust EFFECT_ANIMATIONS found');
  const rustNames = [...rust[1].matchAll(/"([a-z]+)"/g)].map((m) => m[1]);
  assert.deepEqual(rustNames, EFFECTS_PRESETS);
});

test('the page is transparent, loads app.js, and only opts in to effects', () => {
  assert.match(style, /background:\s*transparent/);
  assert.match(html, /<script defer src="app\.js">/);
  assert.match(source, /effectEvents/);
  assert.doesNotMatch(source, /achievementEvents|modEvents/);
});
