export const configSync = $state({
  importing: null as 'all' | 'moderation' | null,
  reloading: 0,
  reloadRequired: false,
  importRevision: 0,
  goalsRevision: 0,
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
