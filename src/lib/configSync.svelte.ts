export const configSync = $state({
  importing: null as 'all' | 'moderation' | 'ttsDictionary' | null,
  reloading: 0,
  reloadRequired: false,
  /** この画面が知る最新の設定 revision(Rust の get_config / update_config の値)。 */
  revision: 0,
});
export const configReloadMessage = '設定を読み直してください';
