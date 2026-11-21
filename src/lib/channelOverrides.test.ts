import assert from 'node:assert/strict';
import test from 'node:test';
import {
  applyChannelsSaved,
  chipBadge,
  replacedChannelKeys,
  fromFormState,
  toFormState,
  voiceOptions,
} from './channelOverrides.ts';

test('toFormState_maps_absent_overrides_to_follow_global', () => {
  assert.deepEqual(toFormState(undefined, 'voicevox'), { ttsDisabled: false, voice: '' });
  assert.deepEqual(toFormState({}, 'webSpeech'), { ttsDisabled: false, voice: '' });
  assert.deepEqual(toFormState({ ttsEnabled: false, ttsVoice: { voicevoxSpeaker: 3 } }, 'voicevox'), {
    ttsDisabled: true,
    voice: '3',
  });
  assert.equal(toFormState({ ttsVoice: { voicevoxSpeaker: 3 } }, 'bouyomi').voice, '');
});

test('fromFormState_omits_empty_voice_and_true_flag', () => {
  assert.deepEqual(fromFormState({ ttsDisabled: false, voice: '' }, 'voicevox'), {});
  assert.deepEqual(fromFormState({ ttsDisabled: true, voice: '' }, 'voicevox'), { ttsEnabled: false });
  assert.deepEqual(fromFormState({ ttsDisabled: false, voice: '2' }, 'voicevox'), {
    ttsVoice: { voicevoxSpeaker: 2 },
  });
  // 他 backend の声は残し、今の backend の声だけ「全体に従う」で消える
  assert.deepEqual(
    fromFormState({ ttsDisabled: false, voice: '' }, 'voicevox', {
      ttsVoice: { voicevoxSpeaker: 2, bouyomiVoice: 4 },
    }),
    { ttsVoice: { bouyomiVoice: 4 } },
  );
  assert.deepEqual(fromFormState({ ttsDisabled: false, voice: 'abc' }, 'bouyomi'), {});
});

test('voiceOptions_for_current_backend_has_follow_global_first', () => {
  const speakers = [{ id: 1, name: 'ずんだもん', style: 'ノーマル' }];
  const vv = voiceOptions('voicevox', { speakers });
  assert.deepEqual(vv[0], { value: '', label: '全体に従う' });
  assert.deepEqual(vv[1], { value: '1', label: 'ずんだもん(ノーマル)' });
  const ws = voiceOptions('webSpeech', { webVoices: ['Haruka'] });
  assert.equal(ws[0].value, '');
  assert.deepEqual(ws[1], { value: 'Haruka', label: 'Haruka' });
  assert.deepEqual(voiceOptions('bouyomi'), [{ value: '', label: '全体に従う' }]);
  // 重複は 1 件、保存済みの値は一覧に無くても残る
  assert.equal(voiceOptions('webSpeech', { webVoices: ['A', 'A'] }).length, 2);
  assert.deepEqual(voiceOptions('voicevox', { speakers: [], saved: '7' })[1].value, '7');
  assert.deepEqual(voiceOptions('webSpeech', { webVoices: [], saved: 'Gone' })[1].value, 'Gone');
  assert.deepEqual(voiceOptions('none'), [{ value: '', label: '全体に従う' }]);
});

test('chipBadge_shows_tts_off_only_when_disabled', () => {
  const base = { platform: 'twitch', identifier: 'a', enabled: true } as const;
  assert.equal(chipBadge({ ...base }), null);
  assert.equal(chipBadge({ ...base, overrides: {} }), null);
  assert.equal(chipBadge({ ...base, overrides: { ttsVoice: { bouyomiVoice: 2 } } }), null);
  assert.equal(chipBadge({ ...base, overrides: { ttsEnabled: false } }), '読まない');
});

test('applyChannelsSaved_replaces_local_channels_from_response', () => {
  const local = [{ platform: 'x', identifier: '1', enabled: true, ownStream: false }] as const;
  const saved = {
    revision: 5,
    channels: [{ platform: 'x', identifier: '1', enabled: true, ownStream: true, overrides: { ttsEnabled: false } }],
  } as const;
  const next = applyChannelsSaved([...local], { channels: [...saved.channels] });
  assert.equal(next.length, 1);
  assert.equal(next[0].ownStream, true);
  assert.deepEqual(next[0].overrides, { ttsEnabled: false });
});

test('bouyomi_voice_zero_is_distinct_from_follow_global', () => {
  assert.equal(toFormState({ ttsVoice: { bouyomiVoice: 0 } }, 'bouyomi').voice, '0');
  assert.deepEqual(fromFormState({ ttsDisabled: false, voice: '0' }, 'bouyomi'), { ttsVoice: { bouyomiVoice: 0 } });
  assert.deepEqual(fromFormState({ ttsDisabled: false, voice: '10001' }, 'bouyomi'), {
    ttsVoice: { bouyomiVoice: 10001 },
  });
  assert.deepEqual(fromFormState({ ttsDisabled: false, voice: '' }, 'bouyomi'), {});
});

test('replacedChannelKeys_lists_every_old_row_of_same_platform_for_x_and_niconico', () => {
  const channels = [
    { platform: 'niconico', identifier: 'lv1', enabled: true },
    { platform: 'twitch', identifier: 'alice', enabled: true },
    { platform: 'niconico', identifier: 'lv2', enabled: true },
    { platform: 'niconico', identifier: 'lv3', enabled: true },
  ] as const;
  assert.deepEqual(
    replacedChannelKeys([...channels], { platform: 'niconico', identifier: 'lv3', enabled: true }),
    ['niconico:lv1', 'niconico:lv2'],
  );
  assert.deepEqual(replacedChannelKeys([...channels], { platform: 'twitch', identifier: 'bob', enabled: true }), []);
});
