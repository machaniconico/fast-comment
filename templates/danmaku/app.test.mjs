import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const obsSource = readFileSync(new URL('./app.js', import.meta.url), 'utf8');
const appSource = readFileSync(
  new URL('../../src/lib/components/DanmakuOverlay.svelte', import.meta.url),
  'utf8',
);

test('danmaku placement does not detect or avoid collisions', () => {
  for (const source of [obsSource, appSource]) {
    assert.match(source, /nextLane/);
    assert.doesNotMatch(source, /requiredGapMs|pickLane|lanePrev|measureWidth|measureText/);
  }
});

test('font size only controls rendering and is not used for collision decisions', () => {
  assert.match(obsSource, /fontSize = FONT_SIZE/);
  assert.match(appSource, /fontSize: settings\.fontSize/);
  assert.doesNotMatch(obsSource, /FONT_SIZE[\s\S]{0,120}(overlap|collision|追突|重複判定)/i);
});
