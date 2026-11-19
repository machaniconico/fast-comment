// ツイキャスの配信者 ID(screen_id)の抽出。Rust の sources/twitcasting.rs の extract_screen_id と同じ仕様。
// URL(twitcasting.tv/<id>...)または素の ID を受ける。素の ID は他プラットフォームと衝突するため、
// 呼び出し側でプラットフォームが「ツイキャス」の時だけ使うこと。

const ID_RE = /^[A-Za-z0-9_:]+$/;
const URL_RE = /^(?:https?:\/\/)?(?:www\.|mobile\.)?twitcasting\.tv(?::\d{1,5})?\/([^/?#]*)/i;

/** 入力が twitcasting.tv の URL なら true(ID の有無は問わない)。 */
export function isTwitcastingUrl(input: string): boolean {
  return /^(?:https?:\/\/)?(?:www\.|mobile\.)?twitcasting\.tv(?::\d{1,5})?(?:[/?#]|$)/i.test(input.trim());
}

export function extractTwitcastingScreenId(input: string): string | null {
  const raw = input.trim();
  if (!raw) return null;
  const m = URL_RE.exec(raw);
  if (m) return m[1] && ID_RE.test(m[1]) ? m[1] : null;
  if (/^(?:https?:\/\/)?[^/?#]*\./i.test(raw) || /[/?#]/.test(raw)) return null;
  return ID_RE.test(raw) ? raw : null;
}
