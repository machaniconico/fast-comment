import assert from 'node:assert/strict';
import test from 'node:test';
import {
  BUILTIN_SOUNDS,
  createSoundPlayer,
  needsBeep,
  normalizeMinJpy,
  parseSoundRef,
  soundLabel,
  validateSoundFile,
} from './soundEffects.ts';

const ID = '0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d';

function harness(options: { load?: (id: string) => Promise<{ mime: string; bytes: Uint8Array } | null>; playFails?: boolean } = {}) {
  const log = { oscillators: [] as Array<{ freq: number; type: string }>, gains: [] as number[], audios: [] as Array<{ src: string; volume: number }>, created: 0, revoked: [] as string[], loads: [] as string[], closed: 0, contexts: 0 };
  let blobs = 0;
  const player = createSoundPlayer({
    createContext: () => {
      log.contexts += 1;
      return {
        currentTime: 10,
        state: 'suspended',
        destination: {},
        resume: async () => {},
        close: async () => { log.closed += 1; },
        createOscillator: () => {
          const rec = { freq: 0, type: '' };
          log.oscillators.push(rec);
          return {
            set type(v: string) { rec.type = v; },
            get type() { return rec.type; },
            frequency: { setValueAtTime: (v: number) => { rec.freq = v; }, exponentialRampToValueAtTime: () => {} },
            connect: () => {},
            start: () => {},
            stop: () => {},
          };
        },
        createGain: () => ({
          gain: { setValueAtTime: (v: number) => { log.gains.push(v); }, exponentialRampToValueAtTime: () => {} },
          connect: () => {},
        }),
      };
    },
    createAudio: () => {
      const rec = { src: '', volume: 1 };
      log.audios.push(rec);
      return {
        get src() { return rec.src; },
        set src(v: string) { rec.src = v; },
        get volume() { return rec.volume; },
        set volume(v: number) { rec.volume = v; },
        play: () => (options.playFails ? Promise.reject(new Error('blocked')) : Promise.resolve()),
      };
    },
    loadCustom: async (id) => {
      log.loads.push(id);
      return options.load ? options.load(id) : { mime: 'audio/wav', bytes: new Uint8Array([1, 2, 3]) };
    },
    createObjectUrl: () => { log.created += 1; blobs += 1; return `blob:${blobs}`; },
    revokeObjectUrl: (url) => { log.revoked.push(url); },
  });
  return { player, log };
}

test('parseSoundRef accepts only builtin names and custom uuids', () => {
  assert.deepEqual(parseSoundRef('builtin:chime'), { kind: 'builtin', name: 'chime' });
  assert.deepEqual(parseSoundRef(`custom:${ID}`), { kind: 'custom', id: ID });
  for (const bad of ['', 'chime', 'builtin:', 'builtin:nope', 'custom:', 'custom:abc', `custom:${ID}/..`, 'custom:../x', `custom:${ID.toUpperCase()}`, 'file:///x', null, undefined, 3]) {
    assert.equal(parseSoundRef(bad), null, String(bad));
  }
});

test('every builtin plays a synthesized tone scaled by volume', async () => {
  for (const { name } of BUILTIN_SOUNDS) {
    const { player, log } = harness();
    assert.equal(await player.play(`builtin:${name}`, 0.5), true, name);
    assert.ok(log.oscillators.length >= 1);
    assert.ok(log.gains.every((g) => g <= 0.5 && g > 0));
  }
});

test('volume is clamped and the audio context is created once', async () => {
  const { player, log } = harness();
  await player.play('builtin:pop', 9);
  await player.play('builtin:pop', -3);
  assert.equal(log.contexts, 1);
  assert.ok(log.gains.every((g) => g <= 1));
  assert.ok(Math.min(...log.gains) >= 0.0001);
});

test('custom sound is fetched once, cached as a blob url, and plays at volume', async () => {
  const { player, log } = harness();
  assert.equal(await player.play(`custom:${ID}`, 0.4), true);
  assert.equal(await player.play(`custom:${ID}`, 0.4), true);
  assert.deepEqual(log.loads, [ID]);
  assert.equal(log.created, 1);
  assert.equal(log.audios.length, 2);
  assert.equal(log.audios[0].src, 'blob:1');
  assert.equal(log.audios[0].volume, 0.4);
});

test('concurrent plays of one custom sound share a single load', async () => {
  const { player, log } = harness();
  await Promise.all([player.play(`custom:${ID}`, 1), player.play(`custom:${ID}`, 1)]);
  assert.deepEqual(log.loads, [ID]);
});

test('missing custom sound falls back to the builtin chime and is retried later', async () => {
  let available = false;
  const { player, log } = harness({ load: async () => (available ? { mime: 'audio/wav', bytes: new Uint8Array([1]) } : null) });
  assert.equal(await player.play(`custom:${ID}`, 1), true);
  assert.equal(log.audios.length, 0);
  assert.ok(log.oscillators.length > 0);
  available = true;
  await player.play(`custom:${ID}`, 1);
  assert.equal(log.audios.length, 1);
});

test('a blocked custom playback also falls back to a builtin tone', async () => {
  const { player, log } = harness({ playFails: true });
  assert.equal(await player.play(`custom:${ID}`, 1), true);
  assert.ok(log.oscillators.length > 0);
});

test('unknown sound references fall back instead of throwing', async () => {
  const { player, log } = harness();
  assert.equal(await player.play('nonsense', 1), true);
  assert.ok(log.oscillators.length > 0);
});

test('forget revokes the blob url and forces a reload', async () => {
  const { player, log } = harness();
  await player.play(`custom:${ID}`, 1);
  player.forget(ID);
  await Promise.resolve();
  await Promise.resolve();
  assert.deepEqual(log.revoked, ['blob:1']);
  await player.play(`custom:${ID}`, 1);
  assert.equal(log.loads.length, 2);
});

test('blob url cache is bounded', async () => {
  const { player, log } = harness();
  for (let i = 0; i < 12; i += 1) {
    await player.play(`custom:${ID.slice(0, -2)}${String(i).padStart(2, '0')}`, 1);
  }
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(log.revoked.length, 4);
});

test('dispose revokes urls and closes the context', async () => {
  const { player, log } = harness();
  await player.play('builtin:chime', 1);
  await player.play(`custom:${ID}`, 1);
  player.dispose();
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(log.closed, 1);
  assert.deepEqual(log.revoked, ['blob:1']);
});

test('beep is skipped only when the same message carries a sound effect', () => {
  assert.equal(needsBeep(true, undefined), true);
  assert.equal(needsBeep(true, ''), true);
  assert.equal(needsBeep(true, 'builtin:chime'), false);
  assert.equal(needsBeep(false, undefined), false);
});

test('normalizeMinJpy keeps whole non-negative yen', () => {
  assert.equal(normalizeMinJpy('500'), 500);
  assert.equal(normalizeMinJpy(99.9), 99);
  for (const bad of ['', -5, NaN, 'abc', null, undefined, Infinity]) assert.equal(normalizeMinJpy(bad), 0, String(bad));
  assert.equal(normalizeMinJpy(1e12), 10_000_000);
});

test('validateSoundFile rejects bad extension, size, emptiness and a full library', () => {
  assert.equal(validateSoundFile('a.WAV', 10, 0), null);
  assert.equal(validateSoundFile('a.mp3', 2 * 1024 * 1024, 0), null);
  assert.ok(validateSoundFile('a.mp3', 2 * 1024 * 1024 + 1, 0));
  assert.ok(validateSoundFile('a.exe', 10, 0));
  assert.ok(validateSoundFile('wav', 10, 0));
  assert.ok(validateSoundFile('a.wav', 0, 0));
  assert.ok(validateSoundFile('a.wav', 10, 32));
  assert.equal(validateSoundFile('a.wav', 10, 31), null);
});

test('soundLabel names builtins, known customs and removed customs', () => {
  assert.equal(soundLabel('builtin:bell', []), 'ベル');
  assert.equal(soundLabel(`custom:${ID}`, [{ id: ID, name: 'ピンポン.wav', ext: 'wav', size: 1 }]), 'ピンポン.wav');
  assert.equal(soundLabel(`custom:${ID}`, []), '(削除済みの音源)');
  assert.equal(soundLabel('x', []), '(不明)');
});
