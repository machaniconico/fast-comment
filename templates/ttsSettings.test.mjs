import assert from 'node:assert/strict';
import test from 'node:test';
import {
  ENGINE_DEFAULT_URLS, urlForEngineChange, optionalClamped, buildVoicevoxParams, buildAutoSpeed,
  buildQueueLimits, kindTemplate, allTemplate, speakerLabel
} from '../src/lib/ttsSettings.ts';

test('preset URL is swapped when blank or another preset default', () => {
  assert.equal(urlForEngineChange('', 'aivisSpeech'), ENGINE_DEFAULT_URLS.aivisSpeech);
  assert.equal(urlForEngineChange('  ', 'voicevox'), ENGINE_DEFAULT_URLS.voicevox);
  assert.equal(urlForEngineChange('http://127.0.0.1:50021', 'aivisSpeech'), 'http://127.0.0.1:10101');
  assert.equal(urlForEngineChange('http://127.0.0.1:10101/', 'voicevox'), 'http://127.0.0.1:50021');
  assert.equal(urlForEngineChange(undefined, 'aivisSpeech'), 'http://127.0.0.1:10101');
});

test('custom URL is kept on engine change', () => {
  assert.equal(urlForEngineChange('http://192.168.0.5:50021', 'aivisSpeech'), 'http://192.168.0.5:50021');
});

test('blank / invalid params become null', () => {
  const p = buildVoicevoxParams({ speedScale: '', pitchScale: null, intonationScale: undefined, volumeScale: 'abc', prePhonemeLength: NaN });
  for (const v of Object.values(p)) assert.equal(v, null);
  assert.equal(optionalClamped('  ', 0, 1), null);
});

test('params are clamped to ranges', () => {
  const p = buildVoicevoxParams({ speedScale: 5, pitchScale: -1, intonationScale: -3, volumeScale: 9, prePhonemeLength: 2, postPhonemeLength: 0.3 });
  assert.deepEqual(p, { speedScale: 2, pitchScale: -0.15, intonationScale: 0, volumeScale: 2, prePhonemeLength: 1.5, postPhonemeLength: 0.3 });
  assert.equal(buildVoicevoxParams({ speedScale: '0.1' }).speedScale, 0.5);
});

test('auto speed and queue limits clamp', () => {
  const a = buildAutoSpeed({ enabled: true, queueThreshold: 9999, perQueued: 5, lengthThreshold: -4, perTenChars: NaN, maxSpeed: 9 });
  assert.deepEqual(a, { enabled: true, queueThreshold: 500, perQueued: 1, lengthThreshold: 0, perTenChars: 0.05, maxSpeed: 2 });
  assert.deepEqual(buildQueueLimits(0, -5), { maxPending: 1, maxWaitSec: 0 });
  assert.deepEqual(buildQueueLimits(9999, 999999), { maxPending: 500, maxWaitSec: 86400 });
  assert.deepEqual(buildQueueLimits('', ''), { maxPending: 64, maxWaitSec: 0 });
});

test('kind template OFF -> null', () => {
  assert.equal(kindTemplate(false, '{name}さん {text}'), null);
  assert.equal(kindTemplate(true, '{name}さん {text}'), '{name}さん {text}');
  assert.equal(kindTemplate(true, '   '), '');
  assert.equal(kindTemplate(true, ''), '');
  assert.equal(allTemplate(''), '');
  assert.equal(allTemplate('   '), '');
  assert.equal(allTemplate('{text}'), '{text}');
});

test('speaker label', () => {
  assert.equal(speakerLabel({ id: 3, name: 'ずんだもん', style: 'ノーマル' }), 'ずんだもん(ノーマル) [3]');
});
