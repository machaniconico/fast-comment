/**
 * Rust `twitcasting_auth::CALLBACK_PORT`(= `TwitcastingConfig` の既定)。設定に値が無い時の代替にだけ使う。
 * Rust と一致することは twitcasting.test.ts で確かめる。
 */
export const DEFAULT_TWITCASTING_CALLBACK_PORT = 39822;

/** Rust `TwitcastingStatus.callbackUrl` と同じ形のコールバック URL。 */
export function twitcastingCallbackUrl(port: number): string {
  return `http://localhost:${port}/twitcasting/callback`;
}

/** 登録用に表示する URL。待受中は進行中のポート、それ以外は入力中の有効値(無効なら空)。 */
export function twitcastingDisplayCallbackUrl(listeningPort: number | null, inputPort: number | null): string {
  const port = listeningPort ?? inputPort;
  return port === null ? '' : twitcastingCallbackUrl(port);
}
