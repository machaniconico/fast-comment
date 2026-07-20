<script lang="ts">
  import { emit } from '@tauri-apps/api/event';
  import {
    clampDanmakuSettings,
    DANMAKU_DEFAULTS,
    DANMAKU_SETTINGS_EVENT,
    loadDanmakuSettings,
    saveDanmakuSettings,
  } from '../danmaku';

  let s = $state(loadDanmakuSettings());

  function toNumber(event: Event): number {
    return Number((event.currentTarget as HTMLInputElement).value);
  }

  async function apply() {
    const next = clampDanmakuSettings(s);
    s = next;
    saveDanmakuSettings(next);

    const isTauri =
      typeof window !== 'undefined' &&
      !!(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
    if (isTauri) {
      try {
        await emit(DANMAKU_SETTINGS_EVENT, next);
      } catch (e) {
        console.warn('[danmaku] settings emit failed', e);
      }
    }
  }

  function onFontSizeInput(event: Event) {
    s.fontSize = toNumber(event);
    void apply();
  }

  function onDurationInput(event: Event) {
    s.durationSec = toNumber(event);
    void apply();
  }

  function onOpacityInput(event: Event) {
    s.opacity = toNumber(event);
    void apply();
  }

  function onMaxActiveInput(event: Event) {
    s.maxActive = toNumber(event);
    void apply();
  }

  function onShowNameChange(event: Event) {
    s.showName = (event.currentTarget as HTMLInputElement).checked;
    void apply();
  }

  function onOutlineChange(event: Event) {
    s.outline = (event.currentTarget as HTMLInputElement).checked;
    void apply();
  }

  function onAreaChange(event: Event) {
    const value = (event.currentTarget as HTMLSelectElement).value;
    s.area = value === 'top' || value === 'bottom' ? value : 'full';
    void apply();
  }

  function onCoalesceChange(event: Event) {
    s.coalesce = (event.currentTarget as HTMLInputElement).checked;
    void apply();
  }

  function onPinGiftsChange(event: Event) {
    s.pinGifts = (event.currentTarget as HTMLInputElement).checked;
    void apply();
  }

  function resetDefaults() {
    s = { ...DANMAKU_DEFAULTS };
    void apply();
  }
</script>

<div class="danmaku-settings">
  <div class="field-row">
    <label for="danmaku-font-size">文字サイズ</label>
    <input
      id="danmaku-font-size"
      type="range"
      min="12"
      max="96"
      step="1"
      value={s.fontSize}
      oninput={onFontSizeInput}
    />
    <span class="value">{s.fontSize}px</span>
  </div>

  <div class="field-row">
    <label for="danmaku-duration">流れる速さ（秒）</label>
    <input
      id="danmaku-duration"
      type="range"
      min="2"
      max="30"
      step="1"
      value={s.durationSec}
      oninput={onDurationInput}
    />
    <span class="value">{s.durationSec}秒</span>
    <span class="hint-inline">小さいほど速い</span>
  </div>

  <div class="field-row">
    <label for="danmaku-opacity">不透明度</label>
    <input
      id="danmaku-opacity"
      type="range"
      min="0.1"
      max="1"
      step="0.01"
      value={s.opacity}
      oninput={onOpacityInput}
    />
    <span class="value">{Math.round(s.opacity * 100)}%</span>
  </div>

  <div class="field-row">
    <label for="danmaku-max-active">同時表示の最大数</label>
    <input
      id="danmaku-max-active"
      type="range"
      min="20"
      max="1000"
      step="10"
      value={s.maxActive}
      oninput={onMaxActiveInput}
    />
    <span class="value">{s.maxActive}</span>
  </div>

  <div class="field-row">
    <label for="danmaku-show-name">名前を表示する</label>
    <input id="danmaku-show-name" type="checkbox" checked={s.showName} class="chk" onchange={onShowNameChange} />
  </div>

  <div class="field-row">
    <label for="danmaku-outline">縁取り（背景が明るくても読みやすく）</label>
    <input id="danmaku-outline" type="checkbox" checked={s.outline} class="chk" onchange={onOutlineChange} />
  </div>

  <div class="field-row">
    <label for="danmaku-area">表示領域</label>
    <select id="danmaku-area" value={s.area} onchange={onAreaChange}>
      <option value="full">全体</option>
      <option value="top">上半分</option>
      <option value="bottom">下半分</option>
    </select>
    <span class="hint-inline">中央のゲーム画面を空ける</span>
  </div>

  <div class="field-row">
    <label for="danmaku-coalesce">連投をまとめる（×N）</label>
    <input id="danmaku-coalesce" type="checkbox" checked={s.coalesce} class="chk" onchange={onCoalesceChange} />
  </div>

  <div class="field-row">
    <label for="danmaku-pin-gifts">投げ銭を上部に固定表示</label>
    <input id="danmaku-pin-gifts" type="checkbox" checked={s.pinGifts} class="chk" onchange={onPinGiftsChange} />
  </div>

  <div class="field-row">
    <button type="button" class="copy-btn" onclick={resetDefaults}>既定に戻す</button>
  </div>

  <p class="hint">
    「ツール▾ → 弾幕オーバーレイ」で表示をON/OFFします。表示中は変更が即座に反映されます。
  </p>
</div>

<style>
  .danmaku-settings {
    display: grid;
    gap: 6px;
  }

  .field-row {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 6px;
    flex-wrap: wrap;
  }

  .field-row label {
    font-size: 13px;
    color: #ccc;
    min-width: 210px;
  }

  input[type='range'] {
    width: min(280px, 100%);
    flex: 1 1 180px;
  }

  select {
    background: #263238;
    color: #e0e0e0;
    border: 1px solid #455a64;
    border-radius: 4px;
    font-size: 13px;
    padding: 4px 8px;
    cursor: pointer;
  }

  .value {
    color: #e0e0e0;
    font-size: 12px;
    min-width: 42px;
  }

  .hint,
  .hint-inline {
    color: #757575;
    font-size: 11px;
  }

  .hint {
    margin: 4px 0 0;
  }

  .hint-inline {
    font-weight: 400;
    text-transform: none;
    letter-spacing: 0;
  }

  .copy-btn {
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    padding: 7px 14px;
    font-weight: 600;
    transition: opacity 0.15s;
    background: #37474f;
    color: #fff;
    min-width: 60px;
  }
</style>
