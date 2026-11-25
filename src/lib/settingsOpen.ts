/** 折り畳みの対象になる節の id。開閉の記憶はこの中の id だけを受け付ける。 */
export const SETTINGS_SECTION_IDS: readonly string[] = [
  'settings-startup-launch',
  'settings-appearance',
  'settings-portability',
  'settings-backup',
  'settings-danmaku',
  'settings-test',
  'settings-youtube-low-latency',
  'settings-posting',
  'settings-x-post',
  'settings-tts',
  'settings-comment-log',
  'settings-listeners',
  'settings-integrations',
  'settings-achievements-group',
  'settings-obs',
  'settings-goals',
  'settings-timer',
  'settings-effects',
  'settings-welcome',
  'settings-participation',
  'settings-moderation',
  'settings-notify',
  'settings-app-update',
  'settings-export',
];

export const SETTINGS_OPEN_KEY = 'fc.settingsOpen';

/** 保存値(JSON 文字列)から開いている節の id を取り出す。壊れた値・配列でない値・未知の id は捨てる。 */
export function parseOpenIds(raw: string | null): string[] {
  if (raw === null) return [];
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!Array.isArray(parsed)) return [];
  const known = new Set(SETTINGS_SECTION_IDS);
  const out: string[] = [];
  for (const v of parsed) {
    if (typeof v === 'string' && known.has(v) && !out.includes(v)) out.push(v);
  }
  return out;
}

export function serializeOpenIds(ids: readonly string[]): string {
  return JSON.stringify(ids.filter((id) => SETTINGS_SECTION_IDS.includes(id)));
}
