// Pure helpers for the comment display filter and UI zoom (no Svelte/Tauri imports,
// so node --test can import them directly).
import type { MessageKind, Roles } from './types';

export type VisitFilter = 'all' | 'first' | 'returning';
export type RoleKey = keyof Roles;

export interface DisplayFilter {
  visit: VisitFilter;
  kinds: MessageKind[];
  roles: RoleKey[];
}

/** Minimal message shape the predicate needs (ChatMessage / UiChatMessage satisfy it). */
export interface FilterableMessage {
  kind: MessageKind;
  author: { roles: Roles };
  listener?: { isFirstEver: boolean } | null;
}

export const VISIT_FILTERS: readonly VisitFilter[] = ['all', 'first', 'returning'];
export const FILTER_KINDS: readonly MessageKind[] = [
  'normal',
  'superChat',
  'membership',
  'bits',
  'gift',
  'system',
];
export const FILTER_ROLES: readonly RoleKey[] = [
  'broadcaster',
  'moderator',
  'member',
  'subscriber',
  'vip',
];

export function defaultDisplayFilter(): DisplayFilter {
  return { visit: 'all', kinds: [], roles: [] };
}

export function isDefaultFilter(f: DisplayFilter): boolean {
  return f.visit === 'all' && f.kinds.length === 0 && f.roles.length === 0;
}

/** Number of active condition groups (drives the toolbar badge). */
export function activeFilterCount(f: DisplayFilter): number {
  return (f.visit !== 'all' ? 1 : 0) + (f.kinds.length > 0 ? 1 : 0) + (f.roles.length > 0 ? 1 : 0);
}

/** Groups are ANDed; values inside a group are ORed. Empty array = no restriction. */
export function matchesDisplayFilter(msg: FilterableMessage, f: DisplayFilter): boolean {
  if (f.visit !== 'all') {
    const l = msg.listener;
    if (!l) return false;
    if (f.visit === 'first' ? l.isFirstEver !== true : l.isFirstEver !== false) return false;
  }
  if (f.kinds.length > 0 && !f.kinds.includes(msg.kind)) return false;
  if (f.roles.length > 0) {
    const roles = msg.author.roles;
    if (!f.roles.some((r) => roles[r] === true)) return false;
  }
  return true;
}

function pickKnown<T extends string>(raw: unknown, allowed: readonly T[]): T[] {
  if (!Array.isArray(raw)) return [];
  const out: T[] = [];
  for (const v of raw) {
    if (typeof v === 'string' && (allowed as readonly string[]).includes(v) && !out.includes(v as T)) {
      out.push(v as T);
    }
  }
  return out;
}

/** Validate a parsed localStorage value; anything unusable falls back to the default. */
export function normalizeDisplayFilter(raw: unknown): DisplayFilter {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return defaultDisplayFilter();
  const o = raw as Record<string, unknown>;
  const visit = (VISIT_FILTERS as readonly unknown[]).includes(o.visit) ? (o.visit as VisitFilter) : 'all';
  return {
    visit,
    kinds: pickKnown(o.kinds, FILTER_KINDS),
    roles: pickKnown(o.roles, FILTER_ROLES),
  };
}

/**
 * Prepend picked items from `incoming` (oldest→newest) to `list` (newest first),
 * capped at `cap`. Returns `list` itself when nothing is picked (no allocation).
 */
export function prependCapped<E, T>(
  list: T[],
  incoming: readonly E[],
  pick: (e: E) => T | null,
  cap: number,
): T[] {
  let added: T[] | null = null;
  for (let i = incoming.length - 1; i >= 0; i -= 1) {
    const item = pick(incoming[i] as E);
    if (item === null) continue;
    (added ??= []).push(item);
    if (added.length >= cap) break;
  }
  if (added === null) return list;
  return added.concat(list).slice(0, cap);
}

// ── UI zoom ──
export const ZOOM_MIN = 0.5;
export const ZOOM_MAX = 2.0;
export const ZOOM_STEP = 0.1;
export const ZOOM_DEFAULT = 1.0;

/** Clamp to [0.5, 2.0] and round to 0.1 (kills float drift such as 1.1000000001). */
export function clampZoom(v: number): number {
  if (typeof v !== 'number' || !Number.isFinite(v)) return ZOOM_DEFAULT;
  const rounded = Math.round(v * 10) / 10;
  return Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, rounded));
}

export function stepZoom(current: number, dir: 1 | -1): number {
  return clampZoom(clampZoom(current) + dir * ZOOM_STEP);
}

/** Parse a stored zoom value (JSON text or number); invalid → 1.0. */
export function normalizeZoom(raw: unknown): number {
  return clampZoom(typeof raw === 'number' ? raw : ZOOM_DEFAULT);
}

export type ZoomAction = 'in' | 'out' | 'reset';

/** Map a Ctrl/Cmd+key event to a zoom action (JP keyboard: `+` is Shift+;). */
export function zoomActionForKey(key: string, code: string): ZoomAction | null {
  if (code === 'NumpadAdd' || key === '+' || key === '=' || key === ';') return 'in';
  if (code === 'NumpadSubtract' || key === '-') return 'out';
  if (code === 'Numpad0' || key === '0') return 'reset';
  return null;
}
