export const PLATFORM_COLORS: Record<string, string> = {
  twitch: '#9146ff',
  youtube: '#ff0000',
  x: '#1da1f2',
  niconico: '#fcc800',
  twitcasting: '#1e78ff',
};

export const PLATFORM_LABELS: Record<string, string> = {
  twitch: 'Twitch',
  youtube: 'YouTube',
  x: 'X',
  niconico: 'ニコ生',
  twitcasting: 'ツイキャス',
};

export const PLATFORM_SHORT_LABELS: Record<string, string> = {
  twitch: 'TW',
  youtube: 'YT',
  x: 'X',
  niconico: 'ニコ',
  twitcasting: 'ツイキャ',
};

export function platformColor(platform: string): string {
  return PLATFORM_COLORS[platform] ?? '#888';
}
