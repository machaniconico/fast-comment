import assert from 'node:assert/strict';
import test from 'node:test';
import { createTtsAudioPlayer } from '../src/lib/ttsAudio.ts';

function harness(opts = {}) {
  const made = [];
  const create = () => {
    const a = {
      src: '', onended: null, onerror: null, paused: false, played: 0,
      play() {
        this.played++;
        if (opts.playReject) return Promise.reject(new Error('x'));
        if (opts.playPending) return new Promise((resolve, reject) => { this.resolvePlay = resolve; this.rejectPlay = reject; });
        return Promise.resolve();
      },
      pause() { this.paused = true; },
    };
    made.push(a);
    return a;
  };
  return { made, create };
}
const tick = () => new Promise((r) => setImmediate(r));

test('plays sequentially, one at a time, as data URI', () => {
  const h = harness();
  const p = createTtsAudioPlayer(h.create);
  p.enqueue('AAA'); p.enqueue('BBB');
  assert.equal(h.made.length, 1);
  assert.equal(h.made[0].src, 'data:audio/wav;base64,AAA');
  h.made[0].onended();
  assert.equal(h.made.length, 2);
  assert.equal(h.made[1].src, 'data:audio/wav;base64,BBB');
});

test('ignores empty strings', () => {
  const h = harness();
  const p = createTtsAudioPlayer(h.create);
  p.enqueue(''); p.enqueue(undefined);
  assert.equal(h.made.length, 0);
  assert.equal(p.pending, 0);
});

test('cancel stops current and clears queue', () => {
  const h = harness();
  const p = createTtsAudioPlayer(h.create);
  p.enqueue('A'); p.enqueue('B'); p.enqueue('C');
  p.cancel();
  assert.equal(h.made[0].paused, true);
  assert.equal(p.pending, 0);
  h.made[0].onended?.();
  assert.equal(h.made.length, 1);
  p.enqueue('D');
  assert.equal(h.made.length, 2);
});

test('error event advances to next', () => {
  const h = harness();
  const p = createTtsAudioPlayer(h.create);
  p.enqueue('A'); p.enqueue('B');
  h.made[0].onerror();
  assert.equal(h.made.length, 2);
});

test('play() reject advances to next', async () => {
  const h = harness({ playReject: true });
  const p = createTtsAudioPlayer(h.create);
  p.enqueue('A'); p.enqueue('B');
  await tick();
  assert.equal(h.made.length, 2);
});

test('queue cap drops oldest', () => {
  const h = harness();
  const p = createTtsAudioPlayer(h.create, 3);
  p.enqueue('0'); // playing
  for (const s of ['1', '2', '3', '4', '5']) p.enqueue(s);
  assert.equal(p.pending, 3);
  h.made[0].onended();
  assert.equal(h.made[1].src, 'data:audio/wav;base64,3');
});

test('stale ended/error handlers and late play() reject after cancel do not double-start', async () => {
  const h = harness({ playPending: true });
  const p = createTtsAudioPlayer(h.create);
  p.enqueue('A'); p.enqueue('B');
  const old = h.made[0];
  const staleEnded = old.onended;
  const staleError = old.onerror;
  const staleReject = old.rejectPlay;
  p.cancel();
  p.enqueue('C');
  assert.equal(h.made.length, 2);
  assert.equal(h.made[1].src, 'data:audio/wav;base64,C');
  p.enqueue('D');
  // Fire everything the cancelled audio could still deliver (AbortError from pause()).
  staleEnded();
  staleError(new Error('stale'));
  staleReject(new Error('AbortError'));
  await tick();
  assert.equal(h.made.length, 2, 'stale callbacks must not start the next item');
  assert.equal(p.pending, 1);
  h.made[1].onended();
  assert.equal(h.made.length, 3);
  assert.equal(h.made[2].src, 'data:audio/wav;base64,D');
});

test('invalid maxQueue falls back to default instead of looping / disabling the cap', () => {
  for (const bad of [-1, NaN, Infinity, 0]) {
    const h = harness();
    const p = createTtsAudioPlayer(h.create, bad);
    p.enqueue('0');
    for (let i = 1; i <= 60; i++) p.enqueue(String(i));
    assert.equal(p.pending, 50, `maxQueue=${bad}`);
  }
});

test('onPlayError is called on play() reject and error event, not on normal end', async () => {
  const errors = [];
  const h = harness({ playReject: true });
  const p = createTtsAudioPlayer(h.create, 50, (e) => errors.push(e));
  p.enqueue('A');
  await tick();
  assert.equal(errors.length, 1);
  const h2 = harness();
  const p2 = createTtsAudioPlayer(h2.create, 50, (e) => errors.push(e));
  p2.enqueue('A'); p2.enqueue('B');
  h2.made[0].onended();
  assert.equal(errors.length, 1);
  h2.made[1].onerror(new Error('decode'));
  assert.equal(errors.length, 2);
});
