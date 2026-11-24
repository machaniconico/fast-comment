<script lang="ts">
  import { onMount } from 'svelte';
  import type { ChannelConfig, ChannelsCommit, TtsEngineSpeaker } from '../ipc';
  import { configChangedError, getConfigSnapshot, probeTtsEngine, setChannelOverrides } from '../ipc';
  import {
    fromFormState,
    reapplyPendingForm,
    toFormState,
    voiceOptions,
    type OverridesForm,
    type TtsBackend,
  } from '../channelOverrides';

  let {
    channel,
    label,
    onclose,
    onsaved,
  }: {
    channel: ChannelConfig;
    label: string;
    onclose: () => void;
    onsaved: (commit: ChannelsCommit) => void;
  } = $props();

  const key = $derived(`${channel.platform}:${channel.identifier}`);

  let revision: number | null = $state(null);
  let backend: TtsBackend = $state('none');
  let loaded: ChannelConfig | null = $state(null);
  const current = $derived(loaded ?? channel);
  let missing = $state(false);
  let speakersFailed = $state(false);
  let savedVoice = $state('');
  let baseForm = toFormState(undefined, 'none');
  let form: OverridesForm = $state({ ttsDisabled: false, soundDisabled: false, voice: '' });
  let speakers: TtsEngineSpeaker[] = $state([]);
  let webVoices: string[] = $state([]);
  let error = $state('');
  let saving = $state(false);

  const options = $derived(voiceOptions(backend, { speakers, webVoices, saved: savedVoice }));

  // 開いた時(と Stale 後)の revision を基準にする。Stale 後は `pending`(利用者の未保存の入力)を
  // 読み直した基準の上に載せ直すので、もう一度「保存」すれば選んだ値が新しい revision へ保存される。
  async function load(pending?: OverridesForm) {
    const snap = await getConfigSnapshot();
    if (!snap) return;
    const prevBackend = backend;
    revision = snap.revision;
    backend = snap.config.tts.backend;
    loaded = snap.config.channels.find((c) => `${c.platform}:${c.identifier}` === key) ?? null;
    missing = loaded === null;
    if (missing) {
      error = '';
      return;
    }
    const base = toFormState(current.overrides, backend);
    form = pending ? reapplyPendingForm(baseForm, pending, base, prevBackend !== backend) : { ...base };
    baseForm = base;
    savedVoice = form.voice === null || form.voice === undefined ? '' : String(form.voice);
    const o = snap.config.tts.options;
    if (backend === 'voicevox') {
      speakersFailed = false;
      try {
        speakers = (await probeTtsEngine(o.voicevoxEngine ?? 'voicevox', o.voicevoxUrl ?? '')).speakers;
      } catch {
        speakers = [];
        speakersFailed = true;
      }
    } else if (backend === 'webSpeech') {
      refreshWebVoices();
    }
  }

  function refreshWebVoices() {
    if (typeof window !== 'undefined' && 'speechSynthesis' in window) {
      webVoices = window.speechSynthesis.getVoices().map((v) => v.name);
    }
  }

  onMount(() => {
    void load();
    const synth = typeof window !== 'undefined' && 'speechSynthesis' in window ? window.speechSynthesis : null;
    synth?.addEventListener('voiceschanged', refreshWebVoices);
    return () => synth?.removeEventListener('voiceschanged', refreshWebVoices);
  });

  async function save() {
    if (revision === null || saving || missing) return;
    saving = true;
    error = '';
    try {
      const commit = await setChannelOverrides(key, fromFormState(form, backend, current.overrides), revision);
      if (commit) onsaved(commit);
      onclose();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      if (msg.includes(configChangedError)) {
        error = configChangedError;
        await load({ ...form });
      } else {
        error = `保存に失敗しました: ${msg}`;
      }
    } finally {
      saving = false;
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.stopPropagation();
      onclose();
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="ov-backdrop" role="presentation" onclick={onclose}>
  <div
    class="ov-dialog"
    role="dialog"
    aria-modal="true"
    aria-label="この枠の設定"
    tabindex="-1"
    onclick={(e) => e.stopPropagation()}
    onkeydown={() => {}}
  >
    <h2 class="ov-title">この枠の設定</h2>
    <p class="ov-sub">{label}</p>

    {#if missing}
      <p class="ov-error" role="alert">この枠は削除または置き換えられました</p>
    {/if}

    <label class="ov-row">
      <span>読み上げ</span>
      <select
        aria-label="読み上げ"
        disabled={missing}
        value={form.ttsDisabled ? 'off' : 'follow'}
        onchange={(e) => (form.ttsDisabled = e.currentTarget.value === 'off')}
      >
        <option value="follow">全体に従う</option>
        <option value="off">この枠は読まない</option>
      </select>
    </label>

    <label class="ov-row">
      <span>声</span>
      {#if backend === 'bouyomi'}
        <input
          type="number"
          step="1"
          aria-label="声"
          placeholder="全体に従う"
          title="空欄は全体に従う、0 は棒読みちゃん既定"
          bind:value={form.voice}
          disabled={missing || form.ttsDisabled}
        />
      {:else}
        <select aria-label="声" bind:value={form.voice} disabled={missing || backend === 'none' || form.ttsDisabled}>
          {#each options as o (o.value)}
            <option value={o.value}>{o.label}</option>
          {/each}
        </select>
      {/if}
    </label>
    {#if backend === 'voicevox' && speakersFailed}
      <p class="ov-error">話者一覧を取得できませんでした</p>
    {/if}

    <label class="ov-row">
      <span>効果音</span>
      <span class="ov-check">
        <input type="checkbox" aria-label="この枠は効果音を鳴らさない" disabled={missing} bind:checked={form.soundDisabled} />
        この枠は効果音を鳴らさない
      </span>
    </label>

    {#if error}<p class="ov-error" role="alert">{error}</p>{/if}

    <div class="ov-actions">
      <button type="button" class="ov-btn" onclick={onclose}>キャンセル</button>
      <button type="button" class="ov-btn primary" disabled={revision === null || saving || missing} onclick={save}>保存</button>
    </div>
  </div>
</div>

<style>
  .ov-check {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .ov-row .ov-check input { flex: none; max-width: none; padding: 0; }

  .ov-backdrop {
    position: fixed;
    inset: 0;
    z-index: 200;
    display: flex;
    align-items: center;
    justify-content: center;
    background: rgba(0,0,0,0.5);
  }
  .ov-dialog {
    width: min(360px, calc(100vw - 32px));
    padding: 16px;
    background: #222;
    color: #e0e0e0;
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 6px;
    box-shadow: 0 12px 28px rgba(0,0,0,0.45);
  }
  .ov-title { margin: 0 0 4px; font-size: 14px; }
  .ov-sub { margin: 0 0 12px; font-size: 12px; color: #999; overflow-wrap: anywhere; }
  .ov-row { display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-bottom: 10px; font-size: 13px; }
  .ov-row select, .ov-row input {
    flex: 1;
    max-width: 220px;
    background: rgba(255,255,255,0.07);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: inherit;
    padding: 5px 8px;
    font-size: 13px;
  }
  .ov-error { margin: 0 0 10px; font-size: 12px; color: #ff8a80; }
  .ov-actions { display: flex; justify-content: flex-end; gap: 8px; }
  .ov-btn {
    background: rgba(255,255,255,0.07);
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    color: inherit;
    padding: 5px 12px;
    font-size: 13px;
    cursor: pointer;
  }
  .ov-btn.primary { background: #1da1f2; border-color: #1da1f2; color: #fff; }
  .ov-btn:disabled { opacity: 0.5; cursor: not-allowed; }
  :global(.app[data-theme='light']) .ov-dialog { background: #fff; color: #111827; border-color: rgba(15,23,42,0.14); }
  :global(.app[data-theme='light']) .ov-sub { color: #52606d; }
</style>
