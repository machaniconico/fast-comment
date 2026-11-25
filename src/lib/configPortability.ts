// 設定の取り込み(SPEC §8.1)の後に出す文。何を残し、何を無効にしたかは Rust 側が決めて
// `warnings` で返す。ここは表示する文を組み立てるだけ。

export interface ImportOutcome {
  warnings: readonly string[];
  protectedOAuth: readonly ('xPost' | 'twitcasting')[];
}

/**
 * 取り込みが保存まで済んだ後の文。`reloaded` が false(画面の読み直しに失敗)でも保存は済んでいるので、
 * 保存時の通知(X の自動告知を無効にした、など)は落とさない。出す文が無ければ null。
 */
export function importMessage(outcome: ImportOutcome, appearanceApplied: boolean, reloaded: boolean): string | null {
  const notices = [...outcome.warnings];
  if (outcome.protectedOAuth.length > 0) {
    const services = outcome.protectedOAuth.map(service => service === 'xPost' ? 'X' : 'ツイキャス');
    notices.push(`ログイン中の ${services.join('、')} の Client ID と待受ポートを保持しました。`);
  }
  if (!reloaded) {
    return notices.length > 0 ? ['設定は保存しました。', ...notices].join(' ') : null;
  }
  const base = appearanceApplied
    ? '設定をインポートし、外観設定を反映しました。'
    : '設定をインポートしました。';
  return [base, ...notices].join(' ');
}
