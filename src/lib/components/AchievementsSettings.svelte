<script lang="ts">
  import { onMount } from 'svelte';
  import {
    achievementSchema, commitAchievements, duplicateAchievement, listAchievements, resetAchievementToPreset,
  } from '../ipc';
  import {
    TIER_LABELS, canDelete, canDuplicate, checkRateRows, coerceCondition, currencyOptions, findDraftError,
    formatDate, invalidFieldMessage, metricOptions, platformOptions, ratesNotice, windowOptions, withSubject,
  } from '../achievementsUi';
  import type {
    AchievementCondition, AchievementCurrencyKind, AchievementDef, AchievementList, AchievementMetric,
    AchievementRow, AchievementSchema, AchievementSubjectKind, Platform,
  } from '../types';
  import type { RateRow } from '../achievementsUi';

  const METRIC_LABELS: Record<AchievementMetric, string> = {
    comments: 'コメント数', superChatCount: 'スーパーチャット回数', donationAmount: '金額', giftCount: 'ギフト数',
    membershipEvents: 'メンバー加入数', streamsAttended: '参加した配信数', uniqueCommenters: 'コメントした人数',
    peakConcurrentViewers: '最大同時視聴者数', streams: '配信回数',
  };
  const WINDOW_LABELS = { lifetime: '通算', perStream: '1 配信ごと' } as const;
  const CURRENCY_LABELS: Record<AchievementCurrencyKind, string> = { none: 'なし', jpyConverted: '円換算', only: '通貨を指定' };
  const PLATFORM_LABELS: Record<Platform, string> = {
    twitch: 'Twitch', youtube: 'YouTube', x: 'X', niconico: 'ニコ生', twitcasting: 'ツイキャス',
  };

  let loaded = $state<AchievementList | null>(null);
  let schema = $state<AchievementSchema | null>(null);
  let drafts = $state<AchievementDef[]>([]);
  let enabled = $state(false);
  let rateRows = $state<RateRow[]>([]);
  let tab = $state<AchievementSubjectKind>('viewer');
  let editingId = $state<string | null>(null);
  let busy = $state(false);
  let message = $state('');
  const now = Date.now();

  let rows = $derived(drafts.filter((a) => a.subject === tab));
  let editing = $derived(drafts.find((a) => a.id === editingId) ?? null);
  let stats = $derived(new Map((loaded?.achievements ?? []).map((a) => [a.id, a as AchievementRow])));
  let notice = $derived(ratesNotice(loaded?.ratesUpdatedAtMs ?? null, now));

  function apply(list: AchievementList) {
    loaded = list;
    drafts = list.achievements.map(({ viewerUnlockCount: _v, streamerUnlockCount: _s, streamerLastUnlockedAtMs: _l, ...def }) => def);
    enabled = list.enabled;
    rateRows = Object.entries(list.rates).map(([code, rate]) => ({ code, text: String(rate) }));
    if (editingId && !drafts.some((a) => a.id === editingId)) editingId = null;
  }

  async function reload() {
    const [list, sch] = await Promise.all([listAchievements(), achievementSchema()]);
    schema = sch;
    if (list) apply(list);
  }
  onMount(() => { void reload().catch(() => { message = '実績を読み込めませんでした。保存先を確認してください。'; }); });

  async function run(action: () => Promise<void>) {
    if (busy) return;
    busy = true;
    try { await action(); } catch { message = '操作に失敗しました。もう一度お試しください。'; }
    finally { busy = false; }
  }

  async function save() {
    await run(async () => {
      if (!loaded) return;
      const draftError = findDraftError(drafts);
      if (draftError) { message = draftError.message; editingId = draftError.id; return; }
      const checked = checkRateRows(rateRows);
      if ('error' in checked) { message = checked.error; return; }
      const reply = await commitAchievements(loaded.revision, loaded.contentToken, enabled, $state.snapshot(drafts), checked.rates, loaded.symbolOverrides);
      if (!reply) return;
      switch (reply.result) {
        case 'ok': message = '保存しました。'; await reload(); break;
        case 'stale':
          message = '別の場所で実績が変更されたため保存できませんでした。最新の内容を読み込みました。もう一度編集してください。';
          await reload();
          break;
        case 'invalid':
          if (reply.field === 'rateCode' || reply.field === 'rateValue') {
            message = reply.field === 'rateCode'
              ? 'レート表の通貨コードが空か重複しています(「$」と「USD」のように同じ通貨になる行も重複です)。'
              : 'レート表に 0 より大きい数値でないレートがあります。';
          } else {
            message = invalidFieldMessage(drafts, reply.id, reply.field);
            editingId = reply.id;
          }
          break;
        default: message = '実績を保存できませんでした。保存先を確認してください。';
      }
    });
  }

  async function duplicate(id: string) {
    await run(async () => {
      const src = drafts.find((a) => a.id === id);
      const copy = src && canDuplicate(src) ? await duplicateAchievement($state.snapshot(src)) : null;
      if (!copy) { message = '複製できませんでした(プラチナは複製できません。しきい値も確認してください)。'; return; }
      const at = drafts.findIndex((a) => a.id === id);
      drafts.splice(at + 1, 0, copy);
      editingId = copy.id;
      message = '複製しました。保存すると反映されます。';
    });
  }

  async function resetToPreset(id: string) {
    await run(async () => {
      const preset = await resetAchievementToPreset(id);
      if (!preset) { message = 'プリセットに戻せませんでした。'; return; }
      const at = drafts.findIndex((a) => a.id === id);
      if (at >= 0) drafts[at] = preset;
      message = 'プリセットの内容に戻しました。保存すると反映されます。';
    });
  }

  function remove(id: string) {
    const at = drafts.findIndex((a) => a.id === id);
    if (at >= 0 && canDelete(drafts[at])) drafts.splice(at, 1);
    if (editingId === id) editingId = null;
    message = '削除しました。保存すると反映されます。';
  }
  function setSubject(a: AchievementDef, subject: AchievementSubjectKind) {
    if (!schema) return;
    const next = withSubject(schema, $state.snapshot(a), subject);
    Object.assign(a, next);
    tab = subject;
  }
  function setMetric(a: AchievementDef, metric: AchievementMetric) {
    if (!a.condition || !schema) return;
    a.condition = coerceCondition(schema, { ...a.condition, metric });
  }
  function setCurrencyKind(c: AchievementCondition, kind: AchievementCurrencyKind) {
    c.currency = kind === 'only' ? { mode: 'only', code: '' } : { mode: kind };
  }
  function togglePlatform(c: AchievementCondition, p: Platform, on: boolean) {
    c.platforms = on ? [...c.platforms, p] : c.platforms.filter((x) => x !== p);
  }
  function addRate() { rateRows.push({ code: '', text: '' }); }
  function progressText(a: AchievementDef): string {
    const s = stats.get(a.id);
    if (!s) return '';
    if (a.subject === 'viewer') return `${s.viewerUnlockCount} 人が解除`;
    return s.streamerUnlockCount > 0
      ? `達成済み(${s.streamerUnlockCount} 回、最新 ${formatDate(s.streamerLastUnlockedAtMs ?? 0)})`
      : '未達成';
  }
</script>

<section id="settings-achievements">
  <h3>実績</h3>
  <p>変更は「保存」を押すまで反映されません。</p>
  <label><input type="checkbox" bind:checked={enabled} disabled={busy} /> 実績機能を有効にする</label>

  <div class="tabs" role="tablist">
    <button role="tab" aria-selected={tab === 'viewer'} class:active={tab === 'viewer'} onclick={() => (tab = 'viewer')}>視聴者</button>
    <button role="tab" aria-selected={tab === 'streamer'} class:active={tab === 'streamer'} onclick={() => (tab = 'streamer')}>配信者</button>
  </div>

  {#if !loaded}
    <p>読み込み中…</p>
  {:else}
    <fieldset class="lock" disabled={busy}>
    <ul class="list">
      {#each rows as a (a.id)}
        <li class:selected={a.id === editingId}>
          <span class="tier tier-{a.tier}">{TIER_LABELS[a.tier]}</span>
          <span class="name">{a.icon} {a.name}{a.edited ? '(編集済み)' : ''}</span>
          <span class="progress">{progressText(a)}</span>
          <label class="inline"><input type="checkbox" bind:checked={a.enabled} /> 有効</label>
          <button onclick={() => (editingId = a.id === editingId ? null : a.id)}>編集</button>
        </li>
      {/each}
    </ul>

    {#if editing && schema}
      <div class="editor">
        <h4>「{editing.name}」を編集</h4>
        <label>名前 <input type="text" bind:value={editing.name} /></label>
        {#if editing.condition}
          <label>対象
            <select value={editing.subject} onchange={(e) => setSubject(editing!, e.currentTarget.value as AchievementSubjectKind)}>
              <option value="viewer">視聴者</option>
              <option value="streamer">配信者</option>
            </select>
          </label>
        {/if}
        <label>説明 <input type="text" bind:value={editing.description} /></label>
        <label>アイコン <input type="text" bind:value={editing.icon} maxlength="8" /></label>
        {#if editing.condition}
          {@const c = editing.condition}
          <label>指標
            <select value={c.metric} onchange={(e) => setMetric(editing!, e.currentTarget.value as AchievementMetric)}>
              {#each metricOptions(schema, editing.subject, c.platforms) as o}
                <option value={o.value} disabled={o.disabled}>{METRIC_LABELS[o.value]}</option>
              {/each}
            </select>
          </label>
          <label>期間
            <select bind:value={c.window}>
              {#each windowOptions(schema, c.metric) as o}
                <option value={o.value} disabled={o.disabled}>{WINDOW_LABELS[o.value]}</option>
              {/each}
            </select>
          </label>
          <label>通貨
            <select value={c.currency.mode} onchange={(e) => setCurrencyKind(c, e.currentTarget.value as AchievementCurrencyKind)}>
              {#each currencyOptions(schema, c.metric) as o}
                <option value={o.value} disabled={o.disabled}>{CURRENCY_LABELS[o.value]}</option>
              {/each}
            </select>
            {#if c.currency.mode === 'only'}
              <input type="text" bind:value={c.currency.code} placeholder="USD" size="6" />
            {/if}
          </label>
          <label>しきい値 <input type="number" min="0" step="any" bind:value={c.threshold} /></label>
          <fieldset>
            <legend>プラットフォーム(未選択はすべて)</legend>
            {#each platformOptions(schema, c.metric) as o}
              <label class="inline">
                <input type="checkbox" disabled={o.disabled} checked={c.platforms.includes(o.value)}
                  onchange={(e) => togglePlatform(c, o.value, e.currentTarget.checked)} />
                {PLATFORM_LABELS[o.value]}
              </label>
            {/each}
          </fieldset>
        {:else}
          <p>この実績は、同じ対象の他の実績をすべて解除すると獲得できます。</p>
        {/if}
        <label class="inline"><input type="checkbox" bind:checked={editing.announce.tts} /> 解除時に読み上げる</label>
        <div class="row">
          <button disabled={!canDuplicate(editing)} onclick={() => duplicate(editing!.id)}>複製</button>
          {#if canDelete(editing)}
            <button onclick={() => remove(editing!.id)}>削除</button>
          {/if}
          {#if editing.builtinId}
            <button onclick={() => resetToPreset(editing!.id)}>プリセットに戻す</button>
          {/if}
          <button onclick={() => (editingId = null)}>閉じる</button>
        </div>
      </div>
    {/if}

    <h4>通貨レート(1 単位あたりの円)</h4>
    <p class:warn={notice.warn} role="status">{notice.text}</p>
    {#each rateRows as row, i}
      <div class="row">
        <input type="text" bind:value={row.code} placeholder="USD / $" size="8" aria-label="通貨コードまたは記号" />
        <input type="text" inputmode="decimal" bind:value={row.text} size="10" aria-label="円換算" />
        <button onclick={() => rateRows.splice(i, 1)}>削除</button>
      </div>
    {/each}
    <div class="row"><button onclick={addRate}>通貨を追加</button></div>

    <div class="row">
      <button class="primary" onclick={save}>保存</button>
      <button onclick={() => run(reload)}>破棄して再読込</button>
    </div>
    </fieldset>
  {/if}
  {#if message}<p role="status" class="message">{message}</p>{/if}
</section>

<style>
  section { border-top: 1px solid #444; margin-top: 20px; padding-top: 12px; }
  h3 { margin: 0 0 10px; }
  h4 { margin: 12px 0 8px; }
  p { font-size: 12px; }
  .tabs { display: flex; gap: 4px; margin: 10px 0; }
  .tabs button.active { background: #3a5a8a; color: #fff; }
  .list { list-style: none; margin: 0; padding: 0; }
  .list li { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; padding: 5px 6px; border-bottom: 1px solid #333; }
  .list li.selected { background: #2a2f3a; }
  .name { flex: 1 1 160px; }
  .progress { font-size: 12px; opacity: 0.8; }
  .tier { font-size: 11px; padding: 1px 6px; border-radius: 8px; border: 1px solid #666; }
  .tier-bronze { color: #cd7f32; border-color: #cd7f32; }
  .tier-silver { color: #c0c0c0; border-color: #c0c0c0; }
  .tier-gold { color: #ffd700; border-color: #ffd700; }
  .tier-platinum { color: #9fe8ff; border-color: #9fe8ff; }
  .editor { border: 1px solid #444; border-radius: 4px; padding: 8px; margin: 10px 0; display: flex; flex-direction: column; gap: 6px; }
  .editor > label { display: flex; align-items: center; gap: 8px; }
  .editor > label.inline { display: inline-flex; }
  .inline { display: inline-flex; align-items: center; gap: 4px; }
  fieldset { border: 1px solid #444; display: flex; flex-wrap: wrap; gap: 8px; }
  fieldset.lock { border: 0; padding: 0; margin: 0; display: block; min-width: 0; }
  .row { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; margin: 8px 0; }
  .warn { color: #ffb347; }
  .message { color: #9fd; }
</style>
