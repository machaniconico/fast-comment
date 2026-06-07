<script lang="ts">
  import { buildCsv, store } from '../stores.svelte';
  import { exportCommentsCsv } from '../ipc';
  import type { UiChatMessage } from '../types';

  interface DonationEntry {
    currency: string;
    total: number;
    count: number;
  }

  interface TimelineBucket {
    key: number;
    count: number;
    startOffsetMs: number;
    label: string;
    height: number;
    isPeak: boolean;
  }

  interface TimelineData {
    buckets: TimelineBucket[];
    binSeconds: number;
    durationMs: number;
    maxCount: number;
    peak: TimelineBucket | null;
  }

  interface WordCount {
    text: string;
    count: number;
  }

  interface EmoteCount {
    name: string;
    url: string;
    count: number;
  }

  interface SpeakerRankingEntry {
    key: string;
    name: string;
    platform: string;
    count: number;
  }

  interface PlatformStat {
    platform: string;
    label: string;
    count: number;
    percent: number;
    width: number;
    className: string;
  }

  interface PlatformBreakdown {
    entries: PlatformStat[];
    total: number;
  }

  const CURRENCY_SYMBOL: Record<string, string> = {
    JPY: '¥',
    USD: '$',
    EUR: '€',
    GBP: '£',
  };

  const STOP_WORDS = new Set([
    'の', 'は', 'が', 'を', 'に', 'へ', 'と', 'も', 'で', 'です', 'ます', 'した', 'する', 'いる', 'ない',
    'a', 'an', 'the', 'to', 'of', 'and', 'is', 'it', 'you',
  ]);

  const nf = new Intl.NumberFormat('ja-JP', { maximumFractionDigits: 0 });
  const PERIOD_OPTIONS = [
    { minutes: 0, label: '全期間' },
    { minutes: 5, label: '直近5分' },
    { minutes: 15, label: '直近15分' },
    { minutes: 60, label: '直近60分' },
  ] as const;

  let periodMinutes: number = $state(0);
  let exportingCsv = $state(false);
  let csvExportMsg = $state('');
  let csvExportPath = $state('');
  let exportingReport = $state<'markdown' | 'text' | null>(null);

  const donationEntries: DonationEntry[] = $derived.by(() => (
    Object.entries(store.donationSummary.byCurrency)
      .filter(([, tally]) => tally.count > 0)
      .map(([currency, tally]) => ({ currency, total: tally.total, count: tally.count }))
      .sort((a, b) => a.currency.localeCompare(b.currency))
  ));

  // 時間帯ヒートマップ: 全期間 allMessages を 0〜23 時でバケット集計
  const hourBuckets: number[] = $derived.by(() => {
    const buckets = Array.from({ length: 24 }, () => 0);
    for (const msg of store.allMessages) {
      if (typeof msg.timestampMs !== 'number' || !Number.isFinite(msg.timestampMs)) continue;
      const hour = new Date(msg.timestampMs).getHours();
      if (hour >= 0 && hour <= 23) {
        buckets[hour] += 1;
      }
    }
    return buckets;
  });

  // ヒートマップ最大値 (0除算防止)
  const hourBucketsMax: number = $derived(Math.max(...hourBuckets, 0));

  const filteredMessages: UiChatMessage[] = $derived.by(() => filterMessagesByPeriod(store.allMessages, periodMinutes));
  const periodSummaryText: string = $derived.by(() => formatPeriodSummary(periodMinutes, filteredMessages.length));
  const timeline: TimelineData = $derived.by(() => buildTimeline(filteredMessages));
  const topWords: WordCount[] = $derived.by(() => buildTopWords(filteredMessages));
  const emoteRanking: EmoteCount[] = $derived.by(() => buildEmoteRanking(filteredMessages));
  const speakerRanking: SpeakerRankingEntry[] = $derived.by(() => buildSpeakerRanking(filteredMessages));
  const platformBreakdown: PlatformBreakdown = $derived.by(() => buildPlatformBreakdown(filteredMessages));

  async function onExportCommentsCsv() {
    if (store.totalCount === 0 || exportingCsv || exportingReport) return;
    exportingCsv = true;
    exportingReport = null;
    csvExportMsg = '';
    csvExportPath = '';
    try {
      const path = await exportCommentsCsv(buildCsv());
      if (path) {
        csvExportPath = path;
        csvExportMsg = 'CSVを出力しました';
      } else {
        csvExportMsg = 'Tauri環境でのみCSV出力できます';
      }
    } catch (e) {
      csvExportMsg = `CSV出力に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      exportingCsv = false;
    }
  }

  async function onExportReport(format: 'markdown' | 'text') {
    if (store.totalCount === 0 || exportingReport || exportingCsv) return;
    exportingReport = format;
    csvExportMsg = '';
    csvExportPath = '';

    const content = format === 'markdown' ? buildReportMarkdown() : buildReportText();
    const extension = format === 'markdown' ? 'md' : 'txt';
    const filename = `fast-comment-振り返り-${formatFileDate(new Date())}.${extension}`;

    try {
      const clipboardCopied = await copyTextToClipboard(content);
      downloadTextFile(filename, content, format === 'markdown' ? 'text/markdown;charset=utf-8' : 'text/plain;charset=utf-8');
      csvExportPath = filename;
      csvExportMsg = clipboardCopied
        ? `${format === 'markdown' ? 'Markdown' : 'テキスト'}をコピーしてダウンロードしました`
        : `${format === 'markdown' ? 'Markdown' : 'テキスト'}をダウンロードしました（コピー失敗）`;
    } catch (e) {
      csvExportMsg = `${format === 'markdown' ? 'Markdown' : 'テキスト'}出力に失敗しました: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      exportingReport = null;
    }
  }

  function buildReportMarkdown(): string {
    const lines: string[] = [
      '# 配信振り返り',
      '',
      '## 概要',
      `- 総コメント数: ${formatCount(store.totalCount)}件`,
      `- ユニーク視聴者: ${formatCount(store.uniqueViewers)}人`,
      `- ハイライト数: ${formatCount(store.highlightCount)}件`,
      '- 投げ銭:',
      ...formatDonationMarkdownLines(),
      '',
      '## コメント量タイムライン',
      ...formatTimelineMarkdownLines(),
      '',
      '## よく出た言葉',
      ...formatTopWordsMarkdownLines(),
      '',
      '## 発言者ランキング',
      ...formatSpeakerRankingMarkdownLines(),
      '',
      '## プラットフォーム内訳',
      ...formatPlatformMarkdownLines(),
    ];

    return `${lines.join('\n')}\n`;
  }

  function buildReportText(): string {
    const lines: string[] = [
      '配信振り返り',
      '',
      '概要',
      `総コメント数: ${formatCount(store.totalCount)}件`,
      `ユニーク視聴者: ${formatCount(store.uniqueViewers)}人`,
      `ハイライト数: ${formatCount(store.highlightCount)}件`,
      '投げ銭:',
      ...formatDonationTextLines(),
      '',
      'コメント量タイムライン',
      ...formatTimelineTextLines(),
      '',
      'よく出た言葉',
      ...formatTopWordsTextLines(),
      '',
      '発言者ランキング',
      ...formatSpeakerRankingTextLines(),
      '',
      'プラットフォーム内訳',
      ...formatPlatformTextLines(),
    ];

    return `${lines.join('\n')}\n`;
  }

  function filterMessagesByPeriod(messages: UiChatMessage[], minutes: number): UiChatMessage[] {
    if (minutes <= 0) {
      return messages;
    }

    if (messages.length === 0) {
      return [];
    }

    let latestTimestamp = Number.NEGATIVE_INFINITY;
    for (const msg of messages) {
      if (Number.isFinite(msg.timestampMs) && msg.timestampMs > latestTimestamp) {
        latestTimestamp = msg.timestampMs;
      }
    }

    if (!Number.isFinite(latestTimestamp)) {
      return [];
    }

    const windowMs = minutes * 60 * 1000;
    return messages.filter((msg) => (
      Number.isFinite(msg.timestampMs) && latestTimestamp - msg.timestampMs <= windowMs
    ));
  }

  function formatPeriodSummary(minutes: number, count: number): string {
    if (minutes <= 0) {
      return `全${formatCount(count)}件から集計中`;
    }

    return `直近${minutes}分の${formatCount(count)}件から集計中`;
  }

  function formatDonationMarkdownLines(): string[] {
    if (donationEntries.length === 0 && store.donationSummary.memberships === 0) {
      return ['  - データなし'];
    }

    return [
      ...donationEntries.map((entry) => (
        `  - ${safeMarkdownCode(entry.currency)}: ${formatDonationAmount(entry.currency, entry.total)} / ${formatCount(entry.count)}件`
      )),
      `  - メンバーシップ: ${formatCount(store.donationSummary.memberships)}件`,
    ];
  }

  function formatDonationTextLines(): string[] {
    if (donationEntries.length === 0 && store.donationSummary.memberships === 0) {
      return ['  データなし'];
    }

    return [
      ...donationEntries.map((entry) => (
        `  ${safeText(entry.currency)}: ${formatDonationAmount(entry.currency, entry.total)} / ${formatCount(entry.count)}件`
      )),
      `  メンバーシップ: ${formatCount(store.donationSummary.memberships)}件`,
    ];
  }

  function formatTimelineMarkdownLines(): string[] {
    if (timeline.buckets.length === 0) {
      return ['データなし'];
    }

    return [
      `集計単位: ${formatCount(timeline.binSeconds)}秒`,
      '',
      '| 時間 | 件数 |',
      '| --- | ---: |',
      ...timeline.buckets.map((bucket) => `| ${bucket.label} | ${formatCount(bucket.count)} |`),
    ];
  }

  function formatTimelineTextLines(): string[] {
    if (timeline.buckets.length === 0) {
      return ['データなし'];
    }

    return [
      `集計単位: ${formatCount(timeline.binSeconds)}秒`,
      ...timeline.buckets.map((bucket) => `${bucket.label}: ${formatCount(bucket.count)}件`),
    ];
  }

  function formatTopWordsMarkdownLines(): string[] {
    if (topWords.length === 0) {
      return ['データなし'];
    }

    return topWords.map((word) => `- ${safeMarkdownCode(word.text)}: ${formatCount(word.count)}回`);
  }

  function formatTopWordsTextLines(): string[] {
    if (topWords.length === 0) {
      return ['データなし'];
    }

    return topWords.map((word) => `${safeText(word.text)}: ${formatCount(word.count)}回`);
  }

  function formatSpeakerRankingMarkdownLines(): string[] {
    if (speakerRanking.length === 0) {
      return ['データなし'];
    }

    return speakerRanking.map((speaker, index) => (
      `${index + 1}. ${safeMarkdownCode(speaker.name)} (${safeMarkdownCode(platformLabel(speaker.platform))}) — ${formatCount(speaker.count)}件`
    ));
  }

  function formatSpeakerRankingTextLines(): string[] {
    if (speakerRanking.length === 0) {
      return ['データなし'];
    }

    return speakerRanking.map((speaker, index) => (
      `${index + 1}. ${safeText(speaker.name)} (${safeText(platformLabel(speaker.platform))}) - ${formatCount(speaker.count)}件`
    ));
  }

  function formatPlatformMarkdownLines(): string[] {
    if (platformBreakdown.entries.length === 0) {
      return ['データなし'];
    }

    return [
      '| プラットフォーム | 件数 | 割合 |',
      '| --- | ---: | ---: |',
      ...platformBreakdown.entries.map((item) => (
        `| ${safeMarkdownTableCell(item.label)} | ${formatCount(item.count)} | ${item.percent}% |`
      )),
    ];
  }

  function formatPlatformTextLines(): string[] {
    if (platformBreakdown.entries.length === 0) {
      return ['データなし'];
    }

    return platformBreakdown.entries.map((item) => (
      `${safeText(item.label)}: ${formatCount(item.count)}件 / ${item.percent}%`
    ));
  }

  function buildTimeline(messages: UiChatMessage[]): TimelineData {
    const timestamps = messages
      .map((msg) => msg.timestampMs)
      .filter((ts) => Number.isFinite(ts));

    if (timestamps.length === 0) {
      return { buckets: [], binSeconds: 60, durationMs: 0, maxCount: 0, peak: null };
    }

    const first = Math.min(...timestamps);
    const last = Math.max(...timestamps);
    const durationMs = Math.max(0, last - first);
    const binMs = chooseBinMs(durationMs);
    const bucketCount = Math.max(1, Math.floor(durationMs / binMs) + 1);
    const counts = Array.from({ length: bucketCount }, () => 0);

    for (const ts of timestamps) {
      const index = Math.min(bucketCount - 1, Math.max(0, Math.floor((ts - first) / binMs)));
      counts[index] += 1;
    }

    const maxCount = Math.max(...counts);
    let peakIndex = -1;
    if (maxCount > 0) {
      peakIndex = counts.findIndex((count) => count === maxCount);
    }

    const buckets = counts.map((count, index) => {
      const startOffsetMs = index * binMs;
      return {
        key: index,
        count,
        startOffsetMs,
        label: formatPeakTime(startOffsetMs),
        height: count === 0 || maxCount === 0 ? 0 : Math.max(3, Math.round((count / maxCount) * 100)),
        isPeak: index === peakIndex,
      };
    });

    return {
      buckets,
      binSeconds: Math.floor(binMs / 1000),
      durationMs,
      maxCount,
      peak: peakIndex >= 0 ? buckets[peakIndex] ?? null : null,
    };
  }

  function chooseBinMs(durationMs: number): number {
    if (durationMs <= 30 * 60 * 1000) return 30 * 1000;
    if (durationMs <= 3 * 60 * 60 * 1000) return 60 * 1000;
    return 300 * 1000;
  }

  function buildTopWords(messages: UiChatMessage[]): WordCount[] {
    const counts = new Map<string, number>();

    for (const msg of messages) {
      for (const token of tokenizeMessage(msg)) {
        counts.set(token, (counts.get(token) ?? 0) + 1);
      }
    }

    return Array.from(counts, ([text, count]) => ({ text, count }))
      .sort((a, b) => b.count - a.count || a.text.localeCompare(b.text, 'ja-JP'))
      .slice(0, 20);
  }

  function buildEmoteRanking(messages: UiChatMessage[]): EmoteCount[] {
    const counts = new Map<string, EmoteCount & { order: number }>();

    for (const msg of messages) {
      for (const frag of msg.fragments) {
        if (frag.type !== 'emote') {
          continue;
        }

        const url = frag.url?.trim() ?? '';
        const existing = counts.get(frag.name);
        if (existing) {
          existing.count += 1;
          if (existing.url === '' && url !== '') {
            existing.url = url;
          }
          continue;
        }

        counts.set(frag.name, {
          name: frag.name,
          url,
          count: 1,
          order: counts.size,
        });
      }
    }

    return Array.from(counts.values())
      .sort((a, b) => b.count - a.count || a.name.localeCompare(b.name, 'ja-JP') || a.order - b.order)
      .slice(0, 15)
      .map(({ order: _order, ...entry }) => entry);
  }

  function buildSpeakerRanking(messages: UiChatMessage[]): SpeakerRankingEntry[] {
    const counts = new Map<string, SpeakerRankingEntry & { order: number }>();

    for (const msg of messages) {
      const key = `${msg.platform}:${msg.author.id || msg.author.name}`;
      const existing = counts.get(key);
      if (existing) {
        existing.count += 1;
        continue;
      }

      counts.set(key, {
        key,
        name: msg.author.name.trim() === '' ? '(名無し)' : msg.author.name,
        platform: msg.platform,
        count: 1,
        order: counts.size,
      });
    }

    return Array.from(counts.values())
      .sort((a, b) => b.count - a.count || a.name.localeCompare(b.name, 'ja-JP') || a.order - b.order)
      .slice(0, 15)
      .map(({ order: _order, ...entry }) => entry);
  }

  function tokenizeMessage(msg: UiChatMessage): string[] {
    const text = msg.fragments
      .filter((fragment) => fragment.type === 'text')
      .map((fragment) => fragment.text)
      .join(' ')
      .replace(/https?:\/\/\S+|www\.\S+/gi, ' ');

    return text
      .split(/[\s\p{P}\p{S}]+/u)
      .map((token) => token.trim().toLowerCase())
      .filter((token) => token.length > 0)
      .filter((token) => Array.from(token).length > 1)
      .filter((token) => !/^[\d０-９]+(?:[.,，．][\d０-９]+)*$/u.test(token))
      .filter((token) => !STOP_WORDS.has(token));
  }

  function buildPlatformBreakdown(messages: UiChatMessage[]): PlatformBreakdown {
    const counts = new Map<string, number>();
    for (const msg of messages) {
      counts.set(msg.platform, (counts.get(msg.platform) ?? 0) + 1);
    }

    const total = messages.length;
    const entries = Array.from(counts, ([platform, count]) => {
      const percent = total === 0 ? 0 : Math.round((count * 100) / total);
      return {
        platform,
        label: platformLabel(platform),
        count,
        percent,
        width: percent,
        className: platformClass(platform),
      };
    }).sort((a, b) => platformOrder(a.platform) - platformOrder(b.platform));

    return { entries, total };
  }

  function platformOrder(platform: string): number {
    if (platform === 'youtube') return 0;
    if (platform === 'twitch') return 1;
    return 2;
  }

  function platformLabel(platform: string): string {
    if (platform === 'youtube') return 'YouTube';
    if (platform === 'twitch') return 'Twitch';
    return platform;
  }

  function platformClass(platform: string): string {
    if (platform === 'youtube') return 'youtube';
    if (platform === 'twitch') return 'twitch';
    return 'other';
  }

  function formatCount(value: number): string {
    return nf.format(value);
  }

  function formatDonationAmount(currency: string, total: number): string {
    if (currency.toLowerCase() === 'bits') {
      return `${nf.format(total)} bits`;
    }
    const symbol = CURRENCY_SYMBOL[currency] ?? `${safeText(currency)} `;
    return `${symbol}${nf.format(total)}`;
  }

  function formatAxisTime(ms: number): string {
    const totalSeconds = Math.max(0, Math.floor(ms / 1000));
    const hours = Math.floor(totalSeconds / 3600);
    const minutes = Math.floor((totalSeconds % 3600) / 60);
    const seconds = totalSeconds % 60;
    if (hours > 0) return `${hours}:${pad2(minutes)}:${pad2(seconds)}`;
    return `${minutes}:${pad2(seconds)}`;
  }

  function formatPeakTime(ms: number): string {
    const totalSeconds = Math.max(0, Math.floor(ms / 1000));
    const hours = Math.floor(totalSeconds / 3600);
    const minutes = Math.floor((totalSeconds % 3600) / 60);
    const seconds = totalSeconds % 60;
    return `${pad2(hours)}:${pad2(minutes)}:${pad2(seconds)}`;
  }

  function pad2(value: number): string {
    return String(value).padStart(2, '0');
  }

  function rankingBarWidth(count: number, maxCount: number): number {
    if (maxCount <= 0) return 0;
    return (count / maxCount) * 100;
  }

  function safeText(value: string): string {
    return value.replace(/[\r\n\t]+/g, ' ').replace(/\s+/g, ' ').trim() || '未設定';
  }

  function safeMarkdownCode(value: string): string {
    return `\`${safeText(value).replace(/`/g, "'").replace(/\|/g, '&#124;')}\``;
  }

  function safeMarkdownTableCell(value: string): string {
    return safeText(value).replace(/`/g, "'").replace(/\|/g, '&#124;');
  }

  function formatFileDate(date: Date): string {
    const year = date.getFullYear();
    const month = pad2(date.getMonth() + 1);
    const day = pad2(date.getDate());
    return `${year}-${month}-${day}`;
  }

  async function copyTextToClipboard(text: string): Promise<boolean> {
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(text);
        return true;
      }
    } catch {
      return false;
    }

    return copyTextWithTextarea(text);
  }

  function copyTextWithTextarea(text: string): boolean {
    const textarea = document.createElement('textarea');
    textarea.value = text;
    textarea.setAttribute('readonly', '');
    textarea.style.position = 'fixed';
    textarea.style.left = '-9999px';
    textarea.style.top = '0';
    document.body.appendChild(textarea);
    textarea.select();

    try {
      return document.execCommand('copy');
    } catch {
      return false;
    } finally {
      document.body.removeChild(textarea);
    }
  }

  function downloadTextFile(filename: string, content: string, type: string) {
    const blob = new Blob([content], { type });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.download = filename;
    link.rel = 'noopener';
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
    URL.revokeObjectURL(url);
  }
</script>

<section class="dashboard" aria-label="配信振り返りダッシュボード">
  <header class="dashboard-header">
    <div class="dashboard-title">
      <h2>配信振り返り</h2>
      <p>{periodSummaryText}</p>
      <p class="period-note">期間フィルタは下のコメント集計のみ適用。総数・投げ銭カードは全期間です。</p>
    </div>
    <div class="header-tools">
      <div class="period-control" aria-label="コメント集計期間">
        <span id="dashboard-period-label" class="period-label">集計期間</span>
        <div class="period-buttons" role="group" aria-labelledby="dashboard-period-label">
          {#each PERIOD_OPTIONS as option (option.minutes)}
            <button
              type="button"
              class="period-button"
              class:active={periodMinutes === option.minutes}
              aria-pressed={periodMinutes === option.minutes}
              aria-label={`${option.label}でコメント集計`}
              onclick={() => {
                periodMinutes = option.minutes;
              }}
            >
              {option.label}
            </button>
          {/each}
        </div>
      </div>
      <div class="export-area">
        <button
          class="export-btn"
          onclick={onExportCommentsCsv}
          disabled={store.totalCount === 0 || exportingCsv || exportingReport !== null}
          aria-label="コメントCSVを出力"
        >
          {exportingCsv ? '出力中...' : 'CSV出力'}
        </button>
        <button
          class="export-btn"
          onclick={() => onExportReport('markdown')}
          disabled={store.totalCount === 0 || exportingCsv || exportingReport !== null}
          aria-label="振り返りMarkdownを出力"
        >
          {exportingReport === 'markdown' ? '出力中...' : 'Markdown出力'}
        </button>
        <button
          class="export-btn"
          onclick={() => onExportReport('text')}
          disabled={store.totalCount === 0 || exportingCsv || exportingReport !== null}
          aria-label="振り返りテキストを出力"
        >
          {exportingReport === 'text' ? '出力中...' : 'テキスト出力'}
        </button>
        {#if csvExportMsg}
          <span class="export-msg" title={csvExportPath}>{csvExportMsg}</span>
        {/if}
      </div>
    </div>
  </header>

  <div class="summary-grid" aria-label="配信サマリー">
    <section class="summary-card comments">
      <span class="card-label">受信コメント</span>
      <strong>{formatCount(store.receivedCount)}</strong>
    </section>
    <section class="summary-card viewers">
      <span class="card-label">ユニーク視聴者</span>
      <strong>{formatCount(store.uniqueViewers)}</strong>
    </section>
    <section class="summary-card highlights">
      <span class="card-label">ハイライト</span>
      <strong>{formatCount(store.highlightCount)}</strong>
    </section>
    <section class="summary-card donations">
      <span class="card-label">投げ銭・メンバー</span>
      <div class="donation-lines">
        {#if donationEntries.length === 0 && store.donationSummary.memberships === 0}
          <span class="muted">まだありません</span>
        {:else}
          {#each donationEntries as entry (entry.currency)}
            <span>{entry.currency}: {formatDonationAmount(entry.currency, entry.total)} / {entry.count}件</span>
          {/each}
          <span>メンバーシップ: {formatCount(store.donationSummary.memberships)}件</span>
        {/if}
      </div>
    </section>
  </div>

  <section class="dashboard-section timeline-section">
    <div class="section-head">
      <div>
        <h3>コメント量タイムライン</h3>
        <p>{timeline.binSeconds}秒ごとの集計</p>
      </div>
      {#if timeline.peak}
        <div class="peak-label">
          最も盛り上がった時間帯 {timeline.peak.label}, {formatCount(timeline.peak.count)}件
        </div>
      {/if}
    </div>

    {#if timeline.buckets.length === 0}
      <div class="empty-state">まだコメントがありません</div>
    {:else}
      <div class="timeline-chart" aria-label="コメント量タイムライン">
        {#each timeline.buckets as bucket (bucket.key)}
          <div
            class="timeline-bar"
            class:peak={bucket.isPeak}
            title={`${bucket.label} ${bucket.count}件`}
            aria-label={`${bucket.label} ${bucket.count}件`}
          >
            <div class="timeline-fill" style={`height: ${bucket.height}%`}></div>
          </div>
        {/each}
      </div>
      <div class="timeline-axis">
        <span>0:00</span>
        <span>{formatAxisTime(timeline.durationMs)}</span>
      </div>
    {/if}
  </section>

  <div class="lower-grid">
    <section class="dashboard-section">
      <div class="section-head">
        <div>
          <h3>よく出た言葉</h3>
          <p>上位20語</p>
        </div>
      </div>

      {#if topWords.length === 0}
        <div class="empty-state compact">まだ集計できる言葉がありません</div>
      {:else}
        <div class="word-list" aria-label="上位ワード">
          {#each topWords as word (word.text)}
            <span class="word-chip">
              <span class="word-text">{word.text}</span>
              <span class="word-count">{formatCount(word.count)}</span>
            </span>
          {/each}
        </div>
      {/if}
    </section>

    <section class="dashboard-section">
      <div class="section-head">
        <div>
          <h3>プラットフォーム</h3>
          <p>{formatCount(platformBreakdown.total)}件</p>
        </div>
      </div>

      {#if platformBreakdown.entries.length === 0}
        <div class="empty-state compact">まだコメントがありません</div>
      {:else}
        <div class="platform-list" aria-label="プラットフォーム別内訳">
          {#each platformBreakdown.entries as item (item.platform)}
            <div class="platform-row">
              <div class="platform-row-head">
                <span>{item.label}</span>
                <span>{formatCount(item.count)}件 / {item.percent}%</span>
              </div>
              <div class="platform-track">
                <div class={`platform-fill ${item.className}`} style={`width: ${item.width}%`}></div>
              </div>
            </div>
          {/each}
        </div>
      {/if}
    </section>
  </div>

  <section class="dashboard-section emote-ranking-section">
    <div class="section-head">
      <div>
        <h3>エモートランキング</h3>
        <p>上位15件</p>
      </div>
    </div>

    {#if emoteRanking.length === 0}
      <div class="empty-state compact">まだエモートがありません</div>
    {:else}
      <div class="emote-ranking-list" aria-label="エモート頻度ランキング">
        {#each emoteRanking as entry, index (entry.name)}
          <div class="emote-ranking-row">
            <span class="emote-rank">{index + 1}</span>
            {#if entry.url}
              <img class="emote-thumb" src={entry.url} alt={entry.name} loading="lazy" decoding="async" />
            {/if}
            <span class="emote-name">{entry.name}</span>
            <span class="emote-count">{formatCount(entry.count)}回</span>
          </div>
        {/each}
      </div>
    {/if}
  </section>

  <section class="dashboard-section speaker-ranking-section">
    <div class="section-head">
      <div>
        <h3>発言者ランキング</h3>
        <p>上位15名</p>
      </div>
    </div>

    {#if speakerRanking.length > 0}
      <div class="speaker-ranking-list" aria-label="発言者ランキング">
        {#each speakerRanking as speaker, index (speaker.key)}
          <div class="speaker-ranking-row">
            <span class="speaker-rank">{index + 1}</span>
            <div class="speaker-main">
              <div class="speaker-row-head">
                <span class="speaker-name">{speaker.name}</span>
                <span class="speaker-count">{formatCount(speaker.count)}件</span>
              </div>
              <div class="platform-track speaker-track">
                <div
                  class="platform-fill speaker-fill"
                  style={`width: ${rankingBarWidth(speaker.count, speakerRanking[0]?.count ?? 0)}%`}
                ></div>
              </div>
            </div>
          </div>
        {/each}
      </div>
    {:else}
      <div class="empty-state compact">まだコメントがありません</div>
    {/if}
  </section>

  <!-- 時間帯アクティビティ・ヒートマップ (全期間) -->
  <section class="dashboard-section heatmap-section">
    <div class="section-head">
      <div>
        <h3>時間帯アクティビティ</h3>
        <p>全期間 / ローカル時刻 0〜23 時の分布</p>
      </div>
    </div>

    <div class="heatmap-grid" aria-label="時間帯アクティビティ・ヒートマップ">
      {#each hourBuckets as count, hour (hour)}
        {@const opacity = hourBucketsMax === 0 ? 0.08 : Math.max(0.08, count / hourBucketsMax)}
        <div
          class="heatmap-cell"
          style={`background: rgba(88,166,255,${opacity.toFixed(3)})`}
          title={`${hour}時台: ${formatCount(count)}件`}
          aria-label={`${hour}時台: ${formatCount(count)}件`}
          role="img"
        >
          <span class="heatmap-hour">
            {#if hour % 3 === 0}{hour}{/if}
          </span>
        </div>
      {/each}
    </div>
    <div class="heatmap-axis" aria-hidden="true">
      {#each [0, 3, 6, 9, 12, 15, 18, 21] as tick (tick)}
        <span>{tick}</span>
      {/each}
    </div>
  </section>
</section>

<style>
  .dashboard {
    flex: 1;
    min-height: 0;
    overflow: auto;
    background: #121212;
    color: #e0e0e0;
  }

  .dashboard-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 12px 14px;
    background: #181818;
    border-bottom: 1px solid rgba(255,255,255,0.08);
  }

  .dashboard-title {
    min-width: 220px;
  }

  .dashboard-header h2,
  .section-head h3 {
    margin: 0;
    color: #f5f5f5;
    font-size: 15px;
    line-height: 1.25;
    letter-spacing: 0;
  }

  .dashboard-header p,
  .section-head p {
    margin: 3px 0 0;
    color: #8b949e;
    font-size: 11px;
    line-height: 1.35;
  }

  .period-note {
    max-width: 440px;
    color: #6f7782;
    overflow-wrap: anywhere;
  }

  .header-tools {
    display: flex;
    align-items: flex-end;
    justify-content: flex-end;
    flex-wrap: wrap;
    gap: 8px 12px;
    min-width: 0;
  }

  .period-control {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 4px;
    min-width: 0;
  }

  .period-label {
    color: #aeb6c2;
    font-size: 11px;
    font-weight: 700;
    line-height: 1.2;
  }

  .period-buttons {
    display: flex;
    justify-content: flex-end;
    flex-wrap: wrap;
    gap: 4px;
    min-width: 0;
  }

  .period-button {
    flex-shrink: 0;
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    background: rgba(255,255,255,0.055);
    color: #c9d1d9;
    padding: 5px 8px;
    font-size: 11px;
    font-weight: 700;
    line-height: 1.2;
    cursor: pointer;
  }

  .period-button:hover {
    background: rgba(255,255,255,0.09);
    border-color: rgba(255,255,255,0.2);
  }

  .period-button.active {
    border-color: rgba(88,166,255,0.55);
    background: rgba(88,166,255,0.2);
    color: #d7ebff;
  }

  .export-area {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    flex-wrap: wrap;
    gap: 8px;
    min-width: 0;
  }

  .export-btn {
    flex-shrink: 0;
    border: 1px solid rgba(88,166,255,0.35);
    border-radius: 4px;
    background: rgba(88,166,255,0.14);
    color: #d7ebff;
    padding: 5px 10px;
    font-size: 12px;
    font-weight: 700;
    cursor: pointer;
  }

  .export-btn:hover:not(:disabled) {
    background: rgba(88,166,255,0.22);
    border-color: rgba(88,166,255,0.55);
  }

  .export-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .export-msg {
    min-width: 0;
    max-width: 260px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: #8fd19e;
    font-size: 11px;
  }

  .summary-grid {
    display: grid;
    grid-template-columns: repeat(4, minmax(140px, 1fr));
    gap: 8px;
    padding: 10px 12px;
    background: #151515;
    border-bottom: 1px solid rgba(255,255,255,0.06);
  }

  .summary-card {
    min-width: 0;
    min-height: 86px;
    padding: 10px;
    border-radius: 6px;
    background: rgba(255,255,255,0.055);
    border: 1px solid rgba(255,255,255,0.08);
    display: flex;
    flex-direction: column;
    justify-content: space-between;
    gap: 8px;
  }

  .card-label {
    color: #aeb6c2;
    font-size: 11px;
    font-weight: 700;
  }

  .summary-card strong {
    color: #fff;
    font-size: 26px;
    line-height: 1;
    font-weight: 800;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .summary-card.comments { border-top: 2px solid #58a6ff; }
  .summary-card.viewers { border-top: 2px solid #56d364; }
  .summary-card.highlights { border-top: 2px solid #f6c453; }
  .summary-card.donations { border-top: 2px solid #ff7b72; }

  .donation-lines {
    display: flex;
    flex-direction: column;
    gap: 3px;
    color: #f1f5f9;
    font-size: 12px;
    line-height: 1.35;
    min-width: 0;
  }

  .donation-lines span {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .muted {
    color: #777;
  }

  .dashboard-section {
    padding: 12px;
    border-bottom: 1px solid rgba(255,255,255,0.06);
    background: #121212;
  }

  .timeline-section {
    background: #141414;
  }

  .section-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 10px;
  }

  .peak-label {
    flex-shrink: 0;
    max-width: 52%;
    padding: 5px 8px;
    border-radius: 4px;
    background: rgba(246,196,83,0.12);
    border: 1px solid rgba(246,196,83,0.32);
    color: #ffd979;
    font-size: 12px;
    font-weight: 700;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .timeline-chart {
    height: 190px;
    display: flex;
    align-items: flex-end;
    gap: 2px;
    padding: 10px 8px 0;
    border-radius: 6px;
    background:
      linear-gradient(to top, rgba(255,255,255,0.05) 1px, transparent 1px) 0 0 / 100% 25%,
      rgba(255,255,255,0.035);
    border: 1px solid rgba(255,255,255,0.07);
    overflow-x: auto;
    overflow-y: hidden;
  }

  .timeline-bar {
    flex: 1 0 8px;
    min-width: 8px;
    height: 100%;
    display: flex;
    align-items: flex-end;
    border-radius: 3px 3px 0 0;
  }

  .timeline-fill {
    width: 100%;
    border-radius: 3px 3px 0 0;
    background: #58a6ff;
    transition: height 0.2s ease;
  }

  .timeline-bar.peak .timeline-fill {
    background: #f6c453;
    box-shadow: 0 0 0 1px rgba(246,196,83,0.35);
  }

  .timeline-axis {
    display: flex;
    justify-content: space-between;
    margin-top: 5px;
    color: #8b949e;
    font-size: 11px;
  }

  .lower-grid {
    display: grid;
    grid-template-columns: minmax(0, 1.35fr) minmax(260px, 0.65fr);
  }

  .lower-grid .dashboard-section:first-child {
    border-right: 1px solid rgba(255,255,255,0.06);
  }

  .word-list {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .word-chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    max-width: 100%;
    padding: 4px 7px;
    border-radius: 4px;
    background: rgba(255,255,255,0.07);
    border: 1px solid rgba(255,255,255,0.08);
    color: #e8e8e8;
    font-size: 12px;
  }

  .word-text {
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .word-count {
    flex-shrink: 0;
    min-width: 20px;
    padding: 1px 5px;
    border-radius: 10px;
    background: rgba(86,211,100,0.14);
    color: #9be9a8;
    font-size: 11px;
    text-align: center;
  }

  .platform-list {
    display: flex;
    flex-direction: column;
    gap: 9px;
  }

  .platform-row-head {
    display: flex;
    justify-content: space-between;
    gap: 10px;
    color: #c9d1d9;
    font-size: 12px;
    font-weight: 700;
  }

  .platform-row-head span:last-child {
    color: #8b949e;
    font-weight: 600;
    white-space: nowrap;
  }

  .platform-track {
    height: 8px;
    margin-top: 5px;
    border-radius: 999px;
    overflow: hidden;
    background: rgba(255,255,255,0.08);
  }

  .platform-fill {
    height: 100%;
    border-radius: inherit;
    transition: width 0.2s ease;
  }

  .platform-fill.youtube { background: #ff5555; }
  .platform-fill.twitch { background: #9146ff; }
  .platform-fill.other { background: #8b949e; }

  .emote-ranking-section {
    background: #121212;
  }

  .emote-ranking-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .emote-ranking-row {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }

  .emote-rank {
    flex: 0 0 26px;
    color: #8b949e;
    font-size: 12px;
    font-weight: 800;
    text-align: right;
  }

  .emote-thumb {
    flex: 0 0 22px;
    width: 22px;
    height: 22px;
    object-fit: contain;
    vertical-align: middle;
  }

  .emote-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: #c9d1d9;
    font-size: 12px;
    font-weight: 700;
  }

  .emote-count {
    flex-shrink: 0;
    min-width: 44px;
    padding: 1px 6px;
    border-radius: 10px;
    background: rgba(88,166,255,0.14);
    color: #9ecbff;
    font-size: 11px;
    font-weight: 700;
    text-align: center;
    white-space: nowrap;
  }

  .speaker-ranking-section {
    background: #121212;
  }

  .speaker-ranking-list {
    display: flex;
    flex-direction: column;
    gap: 9px;
  }

  .speaker-ranking-row {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
  }

  .speaker-rank {
    flex: 0 0 26px;
    color: #8b949e;
    font-size: 12px;
    font-weight: 800;
    text-align: right;
  }

  .speaker-main {
    flex: 1;
    min-width: 0;
  }

  .speaker-row-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    color: #c9d1d9;
    font-size: 12px;
    font-weight: 700;
  }

  .speaker-name {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .speaker-count {
    flex-shrink: 0;
    min-width: 44px;
    padding: 1px 6px;
    border-radius: 10px;
    background: rgba(86,211,100,0.14);
    color: #9be9a8;
    font-size: 11px;
    font-weight: 700;
    text-align: center;
    white-space: nowrap;
  }

  .speaker-track {
    margin-top: 5px;
  }

  .speaker-fill {
    background: #56d364;
  }

  /* 時間帯ヒートマップ */
  .heatmap-section {
    background: #121212;
  }

  .heatmap-grid {
    display: grid;
    grid-template-columns: repeat(24, 1fr);
    gap: 3px;
    border-radius: 6px;
    overflow: hidden;
  }

  .heatmap-cell {
    height: 48px;
    border-radius: 3px;
    display: flex;
    flex-direction: column;
    justify-content: flex-end;
    align-items: center;
    padding-bottom: 4px;
    transition: opacity 0.15s ease;
    cursor: default;
  }

  .heatmap-cell:hover {
    outline: 1px solid rgba(88,166,255,0.6);
    outline-offset: -1px;
  }

  .heatmap-hour {
    color: rgba(255,255,255,0.55);
    font-size: 9px;
    font-weight: 700;
    line-height: 1;
    pointer-events: none;
    min-height: 1em;
  }

  .heatmap-axis {
    display: flex;
    justify-content: space-between;
    margin-top: 5px;
    color: #8b949e;
    font-size: 11px;
    padding: 0 1px;
  }

  .empty-state {
    display: flex;
    align-items: center;
    justify-content: center;
    min-height: 150px;
    border-radius: 6px;
    border: 1px dashed rgba(255,255,255,0.13);
    background: rgba(255,255,255,0.03);
    color: #777;
    font-size: 12px;
  }

  .empty-state.compact {
    min-height: 80px;
  }

  @media (max-width: 860px) {
    .summary-grid,
    .lower-grid {
      grid-template-columns: 1fr 1fr;
    }

    .lower-grid .dashboard-section:first-child {
      border-right: none;
    }

    .dashboard-header,
    .section-head {
      align-items: stretch;
      flex-direction: column;
    }

    .peak-label,
    .export-msg {
      max-width: 100%;
    }

    .export-area {
      justify-content: flex-start;
    }

    .dashboard-title {
      min-width: 0;
    }

    .header-tools,
    .period-control,
    .period-buttons {
      align-items: flex-start;
      justify-content: flex-start;
    }
  }

  @media (max-width: 560px) {
    .summary-grid,
    .lower-grid {
      grid-template-columns: 1fr;
    }
  }
</style>
