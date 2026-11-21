import type { AchievementUnlocked } from './types';

/** Max waiting toasts; beyond this the oldest waiting ones are dropped (shown as a count). */
export const ACHIEVEMENT_QUEUE_MAX = 50;
export const ACHIEVEMENT_TOAST_MS = 5000;

/** FIFO toast queue: one shown at a time, the rest wait. Pure state, no timers. */
export class AchievementQueue {
  current: AchievementUnlocked | null = null;
  private waiting: AchievementUnlocked[] = [];
  /** Waiting items discarded because the queue overflowed. */
  dropped = 0;

  get pending(): number {
    return this.waiting.length;
  }

  /** Total not-yet-seen count (waiting + dropped). */
  get hidden(): number {
    return this.waiting.length + this.dropped;
  }

  push(items: AchievementUnlocked[]): void {
    for (const it of items) {
      if (this.current === null) {
        this.current = it;
        continue;
      }
      this.waiting.push(it);
      if (this.waiting.length > ACHIEVEMENT_QUEUE_MAX) {
        this.waiting.shift();
        this.dropped += 1;
      }
    }
  }

  /** Finish the current toast and show the next one (null when empty). */
  advance(): AchievementUnlocked | null {
    this.current = this.waiting.shift() ?? null;
    if (this.current === null) this.dropped = 0;
    return this.current;
  }
}

export function achievementViewerLabel(a: AchievementUnlocked): string {
  if (a.subject.kind === 'streamer') return '配信者';
  const n = a.subject.name?.trim();
  return n ? n : '視聴者';
}
