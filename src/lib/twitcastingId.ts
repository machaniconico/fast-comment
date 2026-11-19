// ツイキャスの配信者 ID(screen_id)の抽出。Rust の sources/twitcasting.rs の extract_screen_id と同じ仕様
// (設計 §4 識別子)。URL(twitcasting.tv/<id>...)または裸の ID を受け、大小文字を保って返す。
// 接頭辞なしの裸 ID は Twitch 等と衝突するため、呼び出し側でプラットフォームを手で選んだ時だけ使うこと。

const ID_RE = /^(?:(?:c|g|f|ig):)?[A-Za-z0-9_]{1,32}$/i;
const PREFIXED_ID_RE = /^(?:c|g|f|ig):/i;
const HOST_RE = /^(?:(?:www|ssl|en|ja)\.)?twitcasting\.tv$/i;
const SCHEME_RE = /^https?:\/\//i;

// URL として扱うなら [ホスト, パス以降] を返す。裸の ID(`/ ? # .` なし)は null。
function splitUrl(raw: string): [string, string] | null {
  const hasScheme = SCHEME_RE.test(raw);
  const rest = raw.replace(SCHEME_RE, '');
  const i = rest.search(/[/?#]/);
  const host = i < 0 ? rest : rest.slice(0, i);
  if (!hasScheme && i < 0 && !host.includes('.')) return null;
  return [host, i < 0 ? '' : rest.slice(i)];
}

/** 入力が twitcasting.tv(許可サブドメイン含む)の URL なら true。ID の有無は問わない。 */
export function isTwitcastingUrl(input: string): boolean {
  const parts = splitUrl(input.trim());
  return !!parts && HOST_RE.test(parts[0]);
}

/** 接頭辞付きの裸 ID(c:xxx 等)か。UI の自動判別用。 */
export function isPrefixedTwitcastingId(input: string): boolean {
  const raw = input.trim();
  return PREFIXED_ID_RE.test(raw) && ID_RE.test(raw);
}

export function extractTwitcastingScreenId(input: string): string | null {
  const raw = input.trim();
  if (!raw) return null;
  const parts = splitUrl(raw);
  if (!parts) return ID_RE.test(raw) ? raw : null;
  if (!HOST_RE.test(parts[0])) return null;
  const segment = parts[1].replace(/^\//, '').split(/[/?#]/)[0];
  if (!segment) return null;
  // §4: デコードするのは %3A(:)だけで 1 回だけ。%253A は %3A を含まないので残り、他のエスケープ(%41 等)も下で弾かれる。
  const id = segment.replace(/%3A/gi, ':');
  return ID_RE.test(id) ? id : null;
}
