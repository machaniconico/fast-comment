export const configSync = $state({
  importing: null as 'all' | 'moderation' | null,
  reloading: 0,
  reloadRequired: false,
  /** この画面が知る最新の設定 revision(Rust の get_config / update_config の値)。 */
  revision: 0,
});
export const configReloadMessage = '設定を読み直してください';

type ModerationAppendHandler = (kind: 'ngUsers' | 'ngWords', value: string) => void;
const moderationAppendHandlers = new Set<ModerationAppendHandler>();

export function subscribeModerationAppend(handler: ModerationAppendHandler): () => void {
  moderationAppendHandlers.add(handler);
  return () => { moderationAppendHandlers.delete(handler); };
}

export function notifyModerationAppend(...args: Parameters<ModerationAppendHandler>): void {
  for (const handler of moderationAppendHandlers) handler(...args);
}
