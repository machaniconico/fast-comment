<script lang="ts">
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import {
    copyTextToClipboard, getConfig, getObsPollUrl, getPollState, getTwitchOauthStatus, isTauri,
    resetPoll, setPollDisplay, startPoll, stopPoll,
  } from '../ipc';
  import type { TwitchOauthStatus } from '../ipc';
  import { ui } from '../ui.svelte';
  import { isWinner, pollRows, roundPercent, sector, timeLeft } from '../pollView';
  import type { PollDisplay, PollPreferences, PollSnapshot, PollSource } from '../pollView';

  const displays: { value: PollDisplay; label: string }[] = [
    { value: 'bar-h', label: '横棒' }, { value: 'bar-v', label: '縦棒' },
    { value: 'pie', label: '円' }, { value: 'donut', label: 'ドーナツ' },
    { value: 'number', label: '数字のみ' },
  ];

  let source = $state<PollSource>('chat');
  let oauth = $state<TwitchOauthStatus | null>(null);
  let snapshot = $state<PollSnapshot | null>(null);
  let question = $state('');
  let options = $state<string[]>(Array.from({ length: 10 }, () => ''));
  let optionCount = $state(2);
  let timed = $state(false);
  let duration = $state<number | undefined>(60);
  let display = $state<PollDisplay>('bar-h');
  let showPercent = $state(true);
  let showCounts = $state(true);
  let obsUrl = $state('');
  let copied = $state(false);
  let ready = $state(false);
  let busy = $state(false);
  let error = $state('');
  let now = $state(Date.now());

  const twitch = $derived(source === 'twitch');
  const open = $derived(snapshot?.status === 'open');
  const rows = $derived(pollRows(snapshot?.options ?? []));
  const settings = $derived({ display, showPercent, showCounts });
  const endsAt = $derived(open ? snapshot?.endsAtMs : undefined);

  $effect(() => {
    if (endsAt === undefined) return;
    now = Date.now();
    const timer = setInterval(() => { now = Date.now(); }, 250);
    return () => clearInterval(timer);
  });

  $effect(() => {
    if (!ready || !twitch) return;
    let disposed = false;
    oauth = null;
    void getTwitchOauthStatus().then((status) => {
      if (!disposed) oauth = status;
    }).catch(() => {
      // 開始時にも認証状態を確認し、Rust のエラーを表示する。
    });
    return () => { disposed = true; };
  });

  function selectSource(value: string) {
    source = value === 'twitch' ? 'twitch' : 'chat';
    if (source === 'twitch') optionCount = Math.min(optionCount, 5);
  }

  function loadPreferences(saved: PollPreferences) {
    source = saved.source ?? 'chat';
    question = saved.question;
    optionCount = saved.options.length;
    options = Array.from({ length: 10 }, (_, index) => saved.options[index] ?? '');
    display = saved.display;
    showPercent = saved.showPercent;
    showCounts = saved.showCounts;
  }

  onMount(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    let updates = 0;
    let preferencesFromEvent = false;

    async function load() {
      if (isTauri()) {
        unlisten = await listen<PollSnapshot>('poll-updated', ({ payload }) => {
          if (payload.status === 'open' && payload.id !== snapshot?.id) {
            loadPreferences({ ...payload, options: payload.options.map(({ label }) => label) });
            preferencesFromEvent = true;
          }
          updates += 1;
          snapshot = payload;
        });
        if (disposed) {
          unlisten();
          return;
        }
      }
      const before = updates;
      const [initial, config, url] = await Promise.all([getPollState(), getConfig(), getObsPollUrl()]);
      if (disposed) return;
      if (before === updates) snapshot = initial;
      if (!preferencesFromEvent) {
        if (snapshot && snapshot.status !== 'idle') {
          loadPreferences({ ...snapshot, options: snapshot.options.map(({ label }) => label) });
        } else if (config?.poll) {
          loadPreferences(config.poll);
        }
      }
      obsUrl = url;
      ready = isTauri();
    }

    void load().catch((failure) => {
      if (!disposed) error = String(failure);
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  });

  async function act(action: () => Promise<unknown>) {
    busy = true;
    error = '';
    try {
      await action();
    } catch (failure) {
      error = String(failure);
    } finally {
      busy = false;
    }
  }

  function start(event: SubmitEvent) {
    event.preventDefault();
    if (!ready || busy || open) return;
    void act(() => startPoll({
      source,
      question,
      options: options.slice(0, optionCount),
      durationSecs: twitch ? duration : timed ? (duration ?? 0) : undefined,
      ...settings,
    }));
  }

  async function copyUrl() {
    await copyTextToClipboard(obsUrl);
    copied = true;
  }
</script>

<section class="poll-panel" aria-label="アンケート">
  <h2>アンケート</h2>
  <form onsubmit={start} novalidate>
    <fieldset disabled={!ready || busy || open}>
      <label>方式
        <select value={source} onchange={(event) => selectSource(event.currentTarget.value)}>
          <option value="chat">コメント</option>
          <option value="twitch">Twitch</option>
        </select>
      </label>
      {#if twitch}
        <p>質問は60文字まで、選択肢は2〜5件で各25文字までです。</p>
      {/if}
      <label>質問 <textarea rows="1" bind:value={question}></textarea></label>
      <label>選択肢の数
        <select bind:value={optionCount}>
          {#each options.slice(1, twitch ? 5 : 10) as _, index (index)}
            <option value={index + 2}>{index + 2}</option>
          {/each}
        </select>
      </label>
      {#each options.slice(0, optionCount) as _, index (index)}
        <label>選択肢 {index + 1} <textarea rows="1" bind:value={options[index]}></textarea></label>
      {/each}
      <div class="controls">
        {#if twitch}
          <label>秒数（必須、15〜1800秒）
            <input type="number" min="15" max="1800" required bind:value={duration} />
          </label>
        {:else}
          <label>終了方法
            <select bind:value={timed}>
              <option value={false}>手動</option>
              <option value={true}>時間指定</option>
            </select>
          </label>
          {#if timed}
            <label>秒数 <input type="number" bind:value={duration} /></label>
          {/if}
        {/if}
      </div>
    </fieldset>
    <fieldset class="controls" disabled={!ready || busy}>
      <label>表示
        <select bind:value={display}>
          {#each displays as choice (choice.value)}
            <option value={choice.value}>{choice.label}</option>
          {/each}
        </select>
      </label>
      <label><input type="checkbox" bind:checked={showPercent} /> 割合</label>
      <label><input type="checkbox" bind:checked={showCounts} /> 票数</label>
      {#if snapshot && snapshot.status !== 'idle'}
        <button type="button" onclick={() => act(() => setPollDisplay(settings))}>表示を反映</button>
      {/if}
    </fieldset>
    <div class="controls">
      <button type="submit" disabled={!ready || busy || open}>開始</button>
      <button type="button" disabled={!ready || busy || !open} onclick={() => act(stopPoll)}>終了</button>
      <button type="button" disabled={!ready || busy || !snapshot || snapshot.status === 'idle'} onclick={() => act(resetPoll)}>リセット</button>
    </div>
  </form>

  {#if twitch}
    <p>視聴者は Twitch の投票画面から投票できます。</p>
    {#if oauth && !oauth.pollsScopeOk}
      <p>
        {oauth.connected ? 'Twitch で再ログインしてください' : 'Twitch にログインしてください'}
        <button type="button" onclick={() => ui.gotoSetting('twitchOauth')}>Twitch ログイン設定を開く</button>
      </p>
    {/if}
  {:else}
    <p>投票するには、1〜{open ? rows.length : 'N'} の番号か選択肢の言葉そのものをコメントしてください。</p>
  {/if}
  {#if error}<p role="alert">{error}</p>{/if}

  {#if snapshot && snapshot.status !== 'idle'}
    <section class="results" aria-label="集計">
      <h3>{snapshot.question}</h3>
      <p>{open ? '投票受付中' : '結果'} · 総投票者数 {snapshot.totalVoters}人
        {#if endsAt !== undefined} · 残り {timeLeft(endsAt, now)}{/if}
      </p>
      <div class={snapshot.display}>
        {#if snapshot.display === 'pie' || snapshot.display === 'donut'}
          <svg viewBox="0 0 100 100" aria-hidden="true">
            <defs>
              <mask id="in-app-poll-donut">
                <circle cx="50" cy="50" r="46" fill="white" />
                <circle cx="50" cy="50" r="25" fill="black" />
              </mask>
            </defs>
            <g mask={snapshot.display === 'donut' ? 'url(#in-app-poll-donut)' : undefined}>
              <circle cx="50" cy="50" r="45" fill="currentColor" opacity="0.12" />
              {#each rows as row, index (index)}
                {#if row.percent > 0}<path d={sector(row.start, row.percent)} fill={row.color} />{/if}
              {/each}
            </g>
          </svg>
        {/if}
        <ol>
          {#each rows as row, index (index)}
            <li class:winner={isWinner(snapshot.status, row.leading)} style:--color={row.color} style:--percent={row.percent + '%'}>
              {#if snapshot.display === 'bar-h' || snapshot.display === 'bar-v'}
                <div class="track"><div></div></div>
              {/if}
              <span class="label">{index + 1}. {row.label}</span>
              <span class="values">
                {#if snapshot.showCounts}{row.votes}票{/if}
                {#if snapshot.showCounts && snapshot.showPercent} · {/if}
                {#if snapshot.showPercent}{roundPercent(row.percent)}{/if}
              </span>
            </li>
          {/each}
        </ol>
      </div>
    </section>
  {/if}
  <button type="button" disabled={!obsUrl || busy} onclick={() => act(copyUrl)}>
    {copied ? 'OBS URLをコピー済み' : 'OBS URLをコピー'}
  </button>
</section>

<style>
  .poll-panel { height: 100%; overflow: auto; padding: 16px; }
  form, .results { max-width: 800px; }
  h2, h3 { margin: 0 0 8px; overflow-wrap: anywhere; }
  fieldset { border: 0; padding: 0; margin: 0 0 10px; display: grid; gap: 8px; }
  label { display: inline-flex; flex-wrap: wrap; align-items: center; gap: 6px; }
  textarea { flex: 1; min-width: 0; }
  textarea { resize: vertical; }
  input[type="number"] { width: 7em; }
  .controls { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
  .results { margin: 14px 0; }
  ol { list-style: none; margin: 0; padding: 0; }
  li {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: 3px 8px;
    padding: 4px 7px;
    border-inline-start: 4px solid var(--color);
    border-radius: 4px;
  }
  .winner { box-shadow: inset 0 0 0 2px var(--color); font-weight: bold; }
  .label { overflow-wrap: anywhere; }
  .values { text-align: end; }
  .track { grid-column: 1 / -1; grid-row: 2; background: #8884; border-radius: 3px; overflow: hidden; }
  .track div { background: var(--color); }
  .bar-h .track div { width: var(--percent); height: 6px; }
  .bar-v ol { display: flex; gap: 4px; }
  .bar-v li { flex: 1; min-width: 0; display: flex; flex-direction: column; align-items: center; border-inline-start: 0; text-align: center; }
  .bar-v .track { width: min(100%, 34px); height: 110px; display: flex; align-items: end; }
  .bar-v .track div { width: 100%; height: var(--percent); }
  .bar-v .values { text-align: center; }
  .pie, .donut { display: grid; grid-template-columns: minmax(70px, 1fr) minmax(0, 2fr); gap: 10px; align-items: center; }
  svg { width: 100%; max-width: 220px; }
</style>
