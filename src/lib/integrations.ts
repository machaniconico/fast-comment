// Webhook 設定画面の UI 側の純粋な整形。判定(URL の検証・きっかけの照合・停止)は
// すべて Rust 側が行い、ここは入力欄の行分割と状態コードの日本語化だけを持つ。

/** Rust `save_webhook` の上限(送り先数・キーワード数・名前とキーワードの文字数)。 */
export const WEBHOOK_MAX = 10;
export const WEBHOOK_KEYWORDS_MAX = 20;
export const WEBHOOK_TEXT_MAX = 64;

/**
 * キーワード欄を 1 行 1 件に分ける。Rust と同じく前後の空白(全角スペース含む)を
 * 除き、空行を捨てる。重複は除かない(Rust も除かない)。
 */
export function splitKeywords(text: string): string[] {
  return text
    .split(/\r\n|\r|\n/)
    .map((line) => line.trim())
    .filter((line) => line !== '');
}

/** Rust は文字数を `chars().count()`(コードポイント)で数える。 */
export function textLength(text: string): number {
  return [...text].length;
}

const STATE_LABELS: Record<string, string> = {
  active: '稼働中',
  disabled: '無効',
  paused: '停止中',
  noUrl: 'URL 未設定',
};

/** 状態コード(`WebhookStatus.state`)の表示名。未知のコードはそのまま出さず汎用名にする。 */
export function stateLabel(state: string | null | undefined): string {
  if (state == null || state === '') return '確認中';
  return STATE_LABELS[state] ?? '不明な状態';
}

const PAUSE_REASONS: Record<string, string> = {
  unauthorized: '認証エラー(401/403)。URL またはトークンが無効です',
  notFound: '送り先が見つかりません(404/410)。Webhook が削除された可能性があります',
  redirected: 'リダイレクトされました。URL を確認してください',
  unreachable: '送信に失敗し続けました。送り先の状態を確認してください',
};

/** 自動停止の理由コードを日本語にする。未知のコードは汎用文。 */
export function pauseReasonLabel(reason: string | null | undefined): string {
  if (reason == null || reason === '') return '';
  return PAUSE_REASONS[reason] ?? '原因不明の停止';
}

const ERROR_KINDS: Record<string, string> = {
  noUrl: 'URL が未設定です',
  connectFailed: '接続できませんでした',
  timedOutAfterSend: '送信後に応答がありませんでした(届いた可能性があります)',
  readFailed: '応答を読み取れませんでした(届いた可能性があります)',
  httpError: '送り先がエラーを返しました',
  delivered: '送信できました',
};

/** テスト送信・直近の結果の種別コードを日本語にする。 */
export function resultKindLabel(kind: string | null | undefined): string {
  if (kind == null || kind === '') return '';
  return ERROR_KINDS[kind] ?? '失敗しました';
}

/** テスト送信の結果 1 行。 */
export function testResultLabel(result: { ok: boolean; status: number | null; errorKind: string | null }): string {
  if (result.ok) return `テスト送信に成功しました${result.status != null ? `(HTTP ${result.status})` : ''}`;
  const detail = resultKindLabel(result.errorKind ?? 'httpError');
  return `テスト送信に失敗しました: ${detail}${result.status != null ? `(HTTP ${result.status})` : ''}`;
}
