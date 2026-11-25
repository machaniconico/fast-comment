export type PollDisplay = 'bar-h' | 'bar-v' | 'pie' | 'donut' | 'number';
export type PollSource = 'chat' | 'twitch' | 'youtube';
export type PollStatus = 'idle' | 'open' | 'closed';

export interface PollDisplaySettings {
  display: PollDisplay;
  showPercent: boolean;
  showCounts: boolean;
}

export interface PollPreferences extends PollDisplaySettings {
  source?: PollSource;
  question: string;
  options: string[];
}

export interface PollStart extends PollPreferences {
  durationSecs?: number;
}

export interface PollOption {
  label: string;
  votes: number;
  percent: number;
  leading: boolean;
}

export interface PollSnapshot extends PollDisplaySettings {
  id: string;
  source: PollSource;
  status: PollStatus;
  question: string;
  options: PollOption[];
  totalVoters: number;
  startedAtMs: number;
  endsAtMs?: number;
}

const COLORS = ['#58a6ff', '#f0883e', '#3fb950', '#db61a2', '#a371f7',
  '#e3b341', '#39c5cf', '#f85149', '#8ddb8c', '#bc8cff'];

export function pollRows(options: readonly PollOption[]) {
  let start = 0;
  return options.map((option, index) => {
    const row = { ...option, start, color: COLORS[index] };
    start += option.percent;
    return row;
  });
}

export function sector(start: number, percent: number): string {
  if (percent <= 0) return '';
  if (percent >= 100) return 'M 50 5 A 45 45 0 1 1 50 95 A 45 45 0 1 1 50 5 Z';
  const point = (position: number) => {
    const angle = position * Math.PI / 50;
    return `${50 + 45 * Math.sin(angle)} ${50 - 45 * Math.cos(angle)}`;
  };
  return `M 50 50 L ${point(start)} A 45 45 0 ${percent > 50 ? 1 : 0} 1 ${point(start + percent)} Z`;
}

export function roundPercent(percent: number): string {
  return `${Math.round(percent)}%`;
}

export function isWinner(status: PollStatus, leading: boolean): boolean {
  return status === 'closed' && leading;
}

export function timeLeft(endsAtMs: number, nowMs: number): string {
  const seconds = Math.max(0, Math.ceil((endsAtMs - nowMs) / 1000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
}
