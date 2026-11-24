<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { addCustomSound, listCustomSounds, readCustomSound, removeCustomSound } from '../ipc';
  import {
    BUILTIN_SOUNDS,
    MAX_CUSTOM_SOUNDS,
    RULES,
    createSoundPlayer,
    normalizeMinJpy,
    replaceRemovedSound,
    soundLabel,
    validateSoundFile,
    type CustomSound,
    type SoundEffectsConfig,
  } from '../soundEffects';

  interface Props {
    value: SoundEffectsConfig;
    onVolume: (volume: number) => void;
  }

  let { value = $bindable(), onVolume }: Props = $props();

  let custom: CustomSound[] = $state([]);
  let msg = $state('');
  let busy = $state(false);

  // 試し鳴らしは UI 側だけで鳴らす(判定・間引きの経路は通さない)。
  const player = createSoundPlayer({
    createContext: () => new AudioContext() as never,
    createAudio: () => new Audio(),
    loadCustom: readCustomSound,
    createObjectUrl: (blob) => URL.createObjectURL(blob),
    revokeObjectUrl: (url) => URL.revokeObjectURL(url),
  });

  async function refresh() {
    try {
      custom = await listCustomSounds();
    } catch {
      msg = 'カスタム効果音の一覧を読み込めません';
    }
  }

  onMount(() => void refresh());
  onDestroy(() => player.dispose());

  function errorText(e: unknown): string {
    return e instanceof Error ? e.message : String(e);
  }

  function preview(sound: string) {
    void player.play(sound, value.volume, true);
  }

  async function onPick(event: Event & { currentTarget: HTMLInputElement }) {
    const input = event.currentTarget;
    const file = input.files?.[0];
    input.value = '';
    if (!file) return;
    const problem = validateSoundFile(file.name, file.size, custom.length);
    if (problem) {
      msg = problem;
      return;
    }
    busy = true;
    msg = '';
    try {
      await addCustomSound(file.name, new Uint8Array(await file.arrayBuffer()));
      await refresh();
      msg = '追加しました';
    } catch (e) {
      msg = `追加に失敗しました: ${errorText(e)}`;
    } finally {
      busy = false;
    }
  }

  async function onRemove(entry: CustomSound) {
    busy = true;
    msg = '';
    try {
      await removeCustomSound(entry.id);
      player.forget(entry.id);
      // 未保存の編集は残し、この音源を使っていた行だけ既定音にする。
      value = replaceRemovedSound(value, entry.id);
      await refresh();
      msg = '削除しました(使っていた種別は既定の音に戻しました。保存すると反映されます)';
    } catch (e) {
      msg = `削除に失敗しました: ${errorText(e)}`;
    } finally {
      busy = false;
    }
  }
</script>

<div class="sfx">
  <h4>効果音</h4>
  <div class="row">
    <label for="sfx-enabled">種別ごとの効果音を鳴らす</label>
    <input id="sfx-enabled" type="checkbox" bind:checked={value.enabled} />
  </div>
  <div class="row">
    <label for="sfx-volume">音量</label>
    <input
      id="sfx-volume"
      type="range"
      min="0"
      max="1"
      step="0.05"
      bind:value={value.volume}
      oninput={() => onVolume(value.volume)}
    />
    <span class="hint">{Math.round(value.volume * 100)}%</span>
  </div>
  <div class="row">
    <label for="sfx-interval">最小間隔(ms)</label>
    <input id="sfx-interval" type="number" min="0" max="5000" step="50" bind:value={value.minIntervalMs} />
    <span class="hint">この間隔内の音は鳴らさず捨てます(0 〜 5000)</span>
  </div>

  {#each RULES as rule (rule.key)}
    {@const r = value[rule.key]}
    <div class="row rule">
      <input id="sfx-{rule.key}-on" type="checkbox" bind:checked={r.enabled} aria-label="{rule.label}の効果音" />
      <label for="sfx-{rule.key}-on" class="rule-name">{rule.label}</label>
      <select aria-label="{rule.label}の音" bind:value={r.sound} disabled={!r.enabled}>
        <optgroup label="内蔵">
          {#each BUILTIN_SOUNDS as s (s.name)}
            <option value="builtin:{s.name}">{s.label}</option>
          {/each}
        </optgroup>
        {#if custom.length > 0 || r.sound.startsWith('custom:')}
          <optgroup label="カスタム">
            {#each custom as c (c.id)}
              <option value="custom:{c.id}">{c.name}</option>
            {/each}
            {#if r.sound.startsWith('custom:') && !custom.some((c) => `custom:${c.id}` === r.sound)}
              <option value={r.sound}>{soundLabel(r.sound, custom)}</option>
            {/if}
          </optgroup>
        {/if}
      </select>
      {#if rule.money}
        <input
          class="yen"
          type="number"
          min="0"
          step="1"
          aria-label="{rule.label}の最低金額(円)"
          value={r.minJpy}
          disabled={!r.enabled}
          onchange={(e) => (r.minJpy = normalizeMinJpy(e.currentTarget.value))}
        />
        <span class="hint">円以上</span>
      {/if}
      <button type="button" onclick={() => preview(r.sound)}>試し鳴らし</button>
    </div>
  {/each}
  <p class="hint">優先順位は 投げ銭 &gt; メンバー &gt; 初コメント。1 件に鳴る音は 1 つです。円に換算できない通貨は、最低金額が 0 の時だけ鳴ります。</p>

  <h4>カスタム効果音</h4>
  <div class="row">
    <label class="pick">
      <input type="file" accept=".wav,.mp3,.ogg,audio/wav,audio/mpeg,audio/ogg" onchange={onPick} disabled={busy || custom.length >= MAX_CUSTOM_SOUNDS} />
    </label>
    <span class="hint">wav / mp3 / ogg、2 MiB まで、{MAX_CUSTOM_SOUNDS} 件まで({custom.length} 件)</span>
  </div>
  {#if msg}<p class="hint" role="status">{msg}</p>{/if}
  <ul class="list">
    {#each custom as c (c.id)}
      <li>
        <span class="name">{c.name}</span>
        <button type="button" onclick={() => preview(`custom:${c.id}`)}>試し鳴らし</button>
        <button type="button" onclick={() => void onRemove(c)} disabled={busy}>削除</button>
      </li>
    {/each}
  </ul>
</div>

<style>
  .sfx { margin-top: 14px; border-top: 1px solid rgba(255,255,255,0.12); padding-top: 8px; color: #ccc; font-size: 13px; }
  h4 { margin: 6px 0; font-size: 13px; }
  .row { display: flex; align-items: center; gap: 8px; margin: 6px 0; flex-wrap: wrap; }
  .row label { min-width: 90px; }
  .rule-name { min-width: 120px; }
  select, input[type='number'] { background: rgba(255,255,255,0.07); border: 1px solid rgba(255,255,255,0.12); border-radius: 4px; color: inherit; padding: 4px 6px; font-size: 13px; }
  .yen { width: 90px; }
  button { border: 1px solid rgba(255,255,255,0.18); background: rgba(255,255,255,0.08); color: #e0e0e0; border-radius: 4px; cursor: pointer; font-size: 12px; padding: 4px 10px; }
  button:disabled { opacity: 0.5; cursor: not-allowed; }
  .hint { font-size: 12px; color: #aaa; }
  .list { list-style: none; margin: 6px 0 0; padding: 0; }
  .list li { display: flex; align-items: center; gap: 8px; margin: 4px 0; }
  .name { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
</style>
