<script lang="ts">
  import { store } from '../stores.svelte';

  interface Props {
    windowMinutes?: number;
    bucketSeconds?: number;
  }

  interface SparklineSeries {
    counts: number[];
    max: number;
    total: number;
  }

  const DEFAULT_WINDOW_MINUTES = 10;
  const DEFAULT_BUCKET_SECONDS = 60;
  const SVG_WIDTH = 120;
  const SVG_HEIGHT = 28;
  const PADDING_X = 3;
  const PADDING_Y = 4;
  const BASELINE_Y = SVG_HEIGHT - PADDING_Y;
  const PLOT_WIDTH = SVG_WIDTH - PADDING_X * 2;
  const PLOT_HEIGHT = SVG_HEIGHT - PADDING_Y * 2;

  let {
    windowMinutes = DEFAULT_WINDOW_MINUTES,
    bucketSeconds = DEFAULT_BUCKET_SECONDS,
  }: Props = $props();

  const safeWindowMinutes: number = $derived(
    positiveInteger(windowMinutes, DEFAULT_WINDOW_MINUTES)
  );
  const safeBucketSeconds: number = $derived(
    positiveInteger(bucketSeconds, DEFAULT_BUCKET_SECONDS)
  );
  const series: SparklineSeries = $derived.by(() =>
    buildSeries(safeWindowMinutes, safeBucketSeconds)
  );
  const points: string = $derived.by(() => buildPolylinePoints(series.counts, series.max));
  const ariaLabel: string = $derived(
    `直近${safeWindowMinutes}分のコメント頻度`
  );

  function positiveInteger(value: number, fallback: number): number {
    if (!Number.isFinite(value) || value <= 0) return fallback;
    return Math.max(1, Math.floor(value));
  }

  function buildSeries(windowMinutesValue: number, bucketSecondsValue: number): SparklineSeries {
    const bucketCount = Math.max(
      1,
      Math.ceil((windowMinutesValue * 60) / bucketSecondsValue)
    );
    const counts = Array<number>(bucketCount).fill(0);
    const messages = store.allMessages;
    let latestMs = Number.NEGATIVE_INFINITY;

    for (const msg of messages) {
      if (Number.isFinite(msg.timestampMs) && msg.timestampMs > latestMs) {
        latestMs = msg.timestampMs;
      }
    }

    if (!Number.isFinite(latestMs)) {
      return { counts, max: 0, total: 0 };
    }

    const windowMs = windowMinutesValue * 60_000;
    const bucketMs = bucketSecondsValue * 1000;

    for (const msg of messages) {
      const timestampMs = msg.timestampMs;
      if (!Number.isFinite(timestampMs)) continue;
      const ageMs = latestMs - timestampMs;
      if (ageMs < 0 || ageMs > windowMs) continue;

      const bucketFromRight = Math.min(
        bucketCount - 1,
        Math.floor(ageMs / bucketMs)
      );
      counts[bucketCount - 1 - bucketFromRight] += 1;
    }

    let max = 0;
    let total = 0;
    for (const count of counts) {
      if (count > max) max = count;
      total += count;
    }

    return { counts, max, total };
  }

  function buildPolylinePoints(counts: number[], maxCount: number): string {
    if (counts.length === 0) return '';

    return counts
      .map((count, index) => {
        const x = counts.length === 1
          ? SVG_WIDTH / 2
          : PADDING_X + (PLOT_WIDTH * index) / (counts.length - 1);
        const y = maxCount <= 0
          ? BASELINE_Y
          : PADDING_Y + (1 - count / maxCount) * PLOT_HEIGHT;
        return `${roundCoord(x)},${roundCoord(y)}`;
      })
      .join(' ');
  }

  function roundCoord(value: number): number {
    return Math.round(value * 10) / 10;
  }
</script>

{#if series.total > 0}
  <svg
    class="sparkline"
    viewBox={`0 0 ${SVG_WIDTH} ${SVG_HEIGHT}`}
    role="img"
    aria-label={ariaLabel}
    focusable="false"
  >
    <line
      class="sparkline-axis"
      x1={PADDING_X}
      y1={BASELINE_Y}
      x2={SVG_WIDTH - PADDING_X}
      y2={BASELINE_Y}
    />
    <polyline class="sparkline-line" points={points} />
  </svg>
{/if}

<style>
  .sparkline {
    display: block;
    width: 120px;
    height: 28px;
    flex: 0 0 auto;
    color: #7dd3fc;
    opacity: 0.9;
  }

  .sparkline-axis {
    stroke: rgba(255,255,255,0.13);
    stroke-width: 1;
  }

  .sparkline-line {
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  :global(.app[data-theme='light']) .sparkline {
    color: #2563eb;
  }

  :global(.app[data-theme='light']) .sparkline-axis {
    stroke: rgba(15,23,42,0.16);
  }

  @media (max-width: 560px) {
    .sparkline {
      display: none;
    }
  }
</style>
