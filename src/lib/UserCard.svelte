<script lang="ts">
  import { untrack } from 'svelte';
  import { appendModerationValue, deleteListener, getListener, setListenerVoice, updateListener } from './ipc';
  import { store } from './stores.svelte';
  import { ui } from './ui.svelte';
  import { isListenerDeleted, safeAvatarUrl, type ListenerRecord, type ListenerUpdate } from './types';

  type LoadState = 'loading' | 'loaded' | 'error';

  let record: ListenerRecord | null = $state(null);
  let loadState = $state<LoadState>('loading');
  let nickname = $state('');
  let label = $state('');
  let memo = $state('');
  let ttsMuted = $state(false);
  let voicevoxSpeaker = $state('');
  let bouyomiVoice = $state('');
  let webSpeechVoice = $state('');
  let voiceRate = $state('');
  let voicePitch = $state('');
  let saving = $state(false);
  let status = $state('');
  let ngAdded = $state(false);
  let panelEl: HTMLDivElement | undefined = $state();
  let failedAvatar = $state('');
  // Bumped every time the card opens/reloads; async results from older generations are dropped.
  let generation = 0;
  // Highest listener-updated seq already handled; events at or below it are never re-applied.
  let seenSeq = 0;
  // Newest event for the shown user that arrived while the record was being fetched.
  let pending: ListenerUpdate | null = null;
  // Element focused before the card opened; focus returns there on close.
  let opener: HTMLElement | null = null;

  let target = $derived(ui.userCard);
  let ready = $derived(loadState === 'loaded' && !saving);
  let comments = $derived(target ? store.sessionCommentsOf(target.platform, target.authorId, 100) : []);
  let avatar = $derived(
    target?.avatarUrl && target.avatarUrl !== failedAvatar ? (safeAvatarUrl(target.avatarUrl) ?? '') : ''
  );

  function profileOf(rec: ListenerRecord | null) {
    return { nickname: rec?.nickname ?? '', label: rec?.label ?? '', memo: rec?.memo ?? '', ttsMuted: rec?.ttsMuted ?? false };
  }

  function voiceOf(rec: ListenerRecord | null) {
    const v = rec?.ttsVoice;
    return {
      voicevoxSpeaker: v?.voicevoxSpeaker != null ? String(v.voicevoxSpeaker) : '',
      bouyomiVoice: v?.bouyomiVoice != null ? String(v.bouyomiVoice) : '',
      webSpeechVoice: v?.webSpeechVoice ?? '',
      voiceRate: v?.rate != null ? String(v.rate) : '',
      voicePitch: v?.pitch != null ? String(v.pitch) : '',
    };
  }

  // Unsaved edits per group, judged against the last record applied.
  function profileDirty(): boolean {
    const b = profileOf(record);
    return nickname !== b.nickname || label !== b.label || memo !== b.memo || ttsMuted !== b.ttsMuted;
  }

  function voiceDirty(): boolean {
    const b = voiceOf(record);
    return (
      voicevoxSpeaker !== b.voicevoxSpeaker ||
      bouyomiVoice !== b.bouyomiVoice ||
      webSpeechVoice !== b.webSpeechVoice ||
      voiceRate !== b.voiceRate ||
      voicePitch !== b.voicePitch
    );
  }

  function fillProfile(rec: ListenerRecord | null) {
    const p = profileOf(rec);
    nickname = p.nickname;
    label = p.label;
    memo = p.memo;
    ttsMuted = p.ttsMuted;
  }

  function fillVoice(rec: ListenerRecord | null) {
    const v = voiceOf(rec);
    voicevoxSpeaker = v.voicevoxSpeaker;
    bouyomiVoice = v.bouyomiVoice;
    webSpeechVoice = v.webSpeechVoice;
    voiceRate = v.voiceRate;
    voicePitch = v.voicePitch;
  }

  // Full reset (open / reload / delete): drops every draft.
  function fill(rec: ListenerRecord | null) {
    record = rec;
    fillProfile(rec);
    fillVoice(rec);
  }

  // Apply a record from outside the card, leaving a group with unsaved edits untouched.
  function applyKeepingDrafts(rec: ListenerRecord | null) {
    const keepProfile = profileDirty();
    const keepVoice = voiceDirty();
    record = rec;
    if (!keepProfile) fillProfile(rec);
    if (!keepVoice) fillVoice(rec);
  }

  // Apply a save response: refresh only the group that was saved.
  function applySaved(rec: ListenerRecord, group: 'profile' | 'voice') {
    const keepProfile = group === 'voice' && profileDirty();
    const keepVoice = group === 'profile' && voiceDirty();
    record = rec;
    if (!keepProfile) fillProfile(rec);
    if (!keepVoice) fillVoice(rec);
  }

  // Number inputs bind to number | null at runtime, so accept any and stringify.
  function optNum(text: unknown, int: boolean): number | null {
    const str = String(text ?? '').trim();
    if (str === '') return null;
    const n = Number(str);
    if (!Number.isFinite(n)) return null;
    return int ? Math.trunc(n) : n;
  }

  async function saveVoice(clear: boolean) {
    const t = target;
    if (!t || !ready) return;
    const gen = generation;
    saving = true;
    status = '';
    let msg: string;
    let rec: ListenerRecord | null = null;
    try {
      const speaker = optNum(voicevoxSpeaker, true);
      const voice = clear
        ? null
        : {
            voicevoxSpeaker: speaker != null && speaker >= 0 ? speaker : null,
            bouyomiVoice: optNum(bouyomiVoice, true),
            webSpeechVoice: webSpeechVoice.trim() || null,
            rate: optNum(voiceRate, false),
            pitch: optNum(voicePitch, false),
          };
      rec = await setListenerVoice(t.platform, t.authorId, voice);
      msg = rec ? (clear ? '声設定を解除しました' : '声設定を保存しました') : '保存できませんでした(Tauri 外、または応答なし)';
    } catch (e) {
      msg = `声設定の保存に失敗: ${String(e)}`;
    }
    if (gen !== generation) return;
    if (rec) applySaved(rec, 'voice');
    status = msg;
    saving = false;
  }

  function load() {
    const t = ui.userCard;
    if (!t) return;
    const gen = ++generation;
    loadState = 'loading';
    fill(null);
    // Only events received after this fetch starts may override its result.
    seenSeq = untrack(() => store.lastListenerUpdate?.seq ?? 0);
    pending = null;
    getListener(t.platform, t.authorId).then(
      (rec) => {
        if (gen !== generation) return;
        fill(rec);
        if (pending) fill(isListenerDeleted(pending) ? null : pending);
        pending = null;
        loadState = 'loaded';
      },
      (e) => {
        if (gen !== generation) return;
        pending = null;
        loadState = 'error';
        status = `記録の取得に失敗: ${String(e)}`;
      },
    );
  }

  // Load the record whenever a different user is opened.
  $effect(() => {
    const t = ui.userCard;
    status = ui.userCardNotice;
    ngAdded = false;
    saving = false;
    if (!t) {
      generation += 1;
      const o = opener;
      opener = null;
      if (o?.isConnected) o.focus();
      return;
    }
    if (!opener) {
      const a = document.activeElement;
      opener = a instanceof HTMLElement && !untrack(() => panelEl)?.contains(a) ? a : null;
    }
    load();
    requestAnimationFrame(() => panelEl?.focus());
  });

  // Follow listener-updated for the shown user (e.g. edits from another view).
  // Each event is handled once (by seq); events from before the current fetch are ignored.
  $effect(() => {
    const ev = store.lastListenerUpdate;
    untrack(() => {
      const t = ui.userCard;
      if (!ev || !t || ev.seq <= seenSeq) return;
      seenSeq = ev.seq;
      const u = ev.update;
      if (u.platform !== t.platform || u.authorId !== t.authorId) return;
      if (loadState === 'loaded') applyKeepingDrafts(isListenerDeleted(u) ? null : u);
      else if (loadState === 'loading') pending = u;
    });
  });

  async function save() {
    const t = target;
    if (!t || !ready) return;
    const gen = generation;
    saving = true;
    status = '';
    let msg: string;
    let rec: ListenerRecord | null = null;
    try {
      rec = await updateListener(
        t.platform,
        t.authorId,
        nickname.trim() || null,
        label.trim() || null,
        memo.trim() || null,
        ttsMuted,
      );
      msg = rec ? '保存しました' : '保存できませんでした(Tauri 外、または応答なし)';
    } catch (e) {
      msg = `保存に失敗: ${String(e)}`;
    }
    if (gen !== generation) return;
    if (rec) applySaved(rec, 'profile');
    status = msg;
    saving = false;
  }

  async function remove() {
    const t = target;
    if (!t || !ready) return;
    if (!window.confirm('このリスナーの記録を削除しますか?')) return;
    const gen = generation;
    saving = true;
    let msg: string;
    let ok = false;
    try {
      ok = await deleteListener(t.platform, t.authorId);
      msg = ok ? '記録を削除しました' : '削除できませんでした(記録なし、または Tauri 外)';
    } catch (e) {
      msg = `削除に失敗: ${String(e)}`;
    }
    if (gen !== generation) return;
    if (ok) fill(null);
    status = msg;
    saving = false;
  }

  async function addNg() {
    const t = target;
    if (!t) return;
    const gen = generation;
    try {
      await appendModerationValue('ngUsers', record?.name ?? t.name);
      if (gen === generation) ngAdded = true;
    } catch (e) {
      if (gen === generation) status = `NG追加に失敗: ${String(e)}`;
    }
  }

  function fmtDate(ms: number | undefined): string {
    if (!ms) return '-';
    return new Date(ms).toLocaleString();
  }

  function fmtTime(ms: number): string {
    const d = new Date(ms);
    return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}:${String(d.getSeconds()).padStart(2, '0')}`;
  }

  function bodyOf(frags: { type: string; text?: string; name?: string }[]): string {
    return frags.map((f) => (f.type === 'text' ? f.text : f.name) ?? '').join('');
  }

  function onPanelKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      ui.closeUserCard();
      return;
    }
    if (e.key !== 'Tab' || !panelEl) return;
    // Focus trap: keep Tab / Shift+Tab cycling inside the dialog.
    const items = Array.from(
      panelEl.querySelectorAll<HTMLElement>('button, input, textarea, select, a[href], [tabindex]:not([tabindex="-1"])'),
    ).filter((el) => !el.hasAttribute('disabled') && el.getClientRects().length > 0);
    if (items.length === 0) {
      e.preventDefault();
      panelEl.focus();
      return;
    }
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement;
    if (e.shiftKey && (active === first || active === panelEl)) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && active === last) {
      e.preventDefault();
      first.focus();
    }
  }
</script>

{#if target}
  <div class="uc-overlay">
    <button
      type="button"
      class="uc-backdrop"
      aria-label="ユーザーカードを閉じる"
      tabindex="-1"
      onclick={() => ui.closeUserCard()}
    ></button>
    <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
    <div
      bind:this={panelEl}
      class="uc-panel"
      role="dialog"
      aria-modal="true"
      aria-labelledby="user-card-title"
      tabindex="-1"
      onkeydown={onPanelKeydown}
    >
      <header class="uc-head">
        {#if avatar}
          <img
            class="uc-avatar"
            src={avatar}
            alt=""
            loading="lazy"
            decoding="async"
            referrerpolicy="no-referrer"
            onerror={() => (failedAvatar = avatar)}
          />
        {/if}
        <div class="uc-title">
          <h2 id="user-card-title">{record?.nickname || record?.name || target.name}</h2>
          <span class="uc-sub">{record?.name ?? target.name} · {target.platform}</span>
        </div>
        <button type="button" class="uc-close" aria-label="閉じる" onclick={() => ui.closeUserCard()}>×</button>
      </header>

      <div class="uc-body">
        <label class="uc-field">ニックネーム
          <input type="text" bind:value={nickname} maxlength="64" disabled={!ready} />
        </label>
        <label class="uc-field">ラベル
          <input type="text" bind:value={label} maxlength="32" disabled={!ready} />
        </label>
        <label class="uc-field">メモ
          <textarea rows="3" bind:value={memo} maxlength="500" disabled={!ready}></textarea>
        </label>
        <label class="uc-check">
          <input type="checkbox" bind:checked={ttsMuted} disabled={!ready} /> 読み上げミュート
        </label>

        <h3>読み上げの声(このリスナーのみ)</h3>
        <div class="uc-voice">
          <label class="uc-field">VOICEVOX 系 話者ID
            <input type="number" min="0" step="1" bind:value={voicevoxSpeaker} placeholder="既定" disabled={!ready} />
          </label>
          <label class="uc-field">棒読みちゃん声質
            <input type="number" step="1" bind:value={bouyomiVoice} placeholder="既定" disabled={!ready} />
          </label>
          <label class="uc-field">Web Speech 音声名
            <input type="text" bind:value={webSpeechVoice} placeholder="既定" maxlength="200" disabled={!ready} />
          </label>
          <label class="uc-field">速度(0.5〜2.0、VOICEVOX 系にも効く)
            <input type="number" min="0.5" max="2" step="0.1" bind:value={voiceRate} placeholder="1.0" disabled={!ready} />
          </label>
          <label class="uc-field">音程(0.0〜2.0、VOICEVOX 系にも効く)
            <input type="number" min="0" max="2" step="0.1" bind:value={voicePitch} placeholder="1.0" disabled={!ready} />
          </label>
        </div>
        <div class="uc-actions">
          <button type="button" class="uc-btn" onclick={() => saveVoice(false)} disabled={!ready}>声を保存</button>
          <button type="button" class="uc-btn" onclick={() => saveVoice(true)} disabled={!ready || !record?.ttsVoice}>声設定を解除</button>
        </div>

        <div class="uc-actions">
          <button type="button" class="uc-btn primary" onclick={save} disabled={!ready}>保存</button>
          <button type="button" class="uc-btn danger" onclick={addNg} disabled={ngAdded}>
            {ngAdded ? 'NGに追加済み' : 'NGユーザーに追加'}
          </button>
          {#if loadState === 'error'}
            <button type="button" class="uc-btn" onclick={load}>再試行</button>
          {/if}
          {#if record}
            <button type="button" class="uc-btn" onclick={remove} disabled={!ready}>記録を削除</button>
          {/if}
          {#if status}<span class="uc-status">{status}</span>{/if}
        </div>

        <dl class="uc-stats">
          <dt>累計コメント</dt><dd>{record?.commentCount ?? 0}</dd>
          <dt>累計ギフト</dt><dd>{record?.giftCount ?? 0}</dd>
          <dt>初回来訪</dt><dd>{fmtDate(record?.firstSeenMs)}</dd>
          <dt>最終来訪</dt><dd>{fmtDate(record?.lastSeenMs)}</dd>
        </dl>

        {#if record && record.nameHistory.length > 0}
          <h3>名前変更履歴</h3>
          <ul class="uc-list">
            {#each record.nameHistory as h}
              <li>{h.name} <span class="uc-dim">{fmtDate(h.changedAtMs)}</span></li>
            {/each}
          </ul>
        {/if}

        <h3>今回のセッションのコメント ({comments.length})</h3>
        <ul class="uc-list uc-comments">
          {#each comments as c (c.id)}
            <li><span class="uc-dim">{fmtTime(c.timestampMs)}</span> {bodyOf(c.fragments)}</li>
          {:else}
            <li class="uc-dim">なし</li>
          {/each}
        </ul>
      </div>
    </div>
  </div>
{/if}

<style>
  .uc-overlay {
    position: fixed;
    inset: 0;
    z-index: 1002;
    display: flex;
    align-items: flex-start;
    justify-content: center;
    padding: 48px 16px 24px;
  }
  .uc-backdrop {
    position: absolute;
    inset: 0;
    margin: 0;
    padding: 0;
    border: none;
    background: rgba(0, 0, 0, 0.55);
    cursor: pointer;
  }
  .uc-panel {
    position: relative;
    width: 460px;
    max-width: 100%;
    max-height: calc(100vh - 72px);
    overflow-y: auto;
    background: #1e1e1e;
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.6);
    color: #e0e0e0;
    font-size: 13px;
  }
  .uc-panel:focus {
    outline: none;
  }
  .uc-head {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px 14px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.1);
  }
  .uc-avatar {
    width: 48px;
    height: 48px;
    border-radius: 50%;
    object-fit: cover;
    flex-shrink: 0;
  }
  .uc-title {
    flex: 1;
    min-width: 0;
  }
  .uc-title h2 {
    margin: 0;
    font-size: 16px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .uc-sub,
  .uc-dim {
    opacity: 0.6;
    font-size: 11px;
  }
  .uc-close {
    background: none;
    border: none;
    color: inherit;
    font-size: 20px;
    cursor: pointer;
  }
  .uc-body {
    padding: 12px 14px 16px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .uc-field {
    display: flex;
    flex-direction: column;
    gap: 3px;
    font-size: 12px;
  }
  .uc-field input,
  .uc-field textarea {
    background: #2a2a2a;
    border: 1px solid rgba(255, 255, 255, 0.15);
    border-radius: 4px;
    color: inherit;
    padding: 4px 6px;
    font: inherit;
    resize: vertical;
  }
  .uc-voice {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 6px 10px;
  }
  .uc-check {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .uc-actions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
  }
  .uc-btn {
    padding: 4px 10px;
    border-radius: 4px;
    border: 1px solid rgba(255, 255, 255, 0.2);
    background: #2f2f2f;
    color: inherit;
    cursor: pointer;
  }
  .uc-btn.primary {
    background: #1e6fd9;
    border-color: #1e6fd9;
  }
  .uc-btn.danger {
    color: #ff8a80;
  }
  .uc-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .uc-status {
    font-size: 11px;
    opacity: 0.8;
  }
  .uc-stats {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 2px 12px;
    margin: 4px 0 0;
  }
  .uc-stats dt {
    opacity: 0.6;
  }
  .uc-stats dd {
    margin: 0;
  }
  h3 {
    margin: 8px 0 0;
    font-size: 12px;
    opacity: 0.8;
  }
  .uc-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .uc-comments {
    max-height: 200px;
    overflow-y: auto;
    word-break: break-all;
  }
</style>
