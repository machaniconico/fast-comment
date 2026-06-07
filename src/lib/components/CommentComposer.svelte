<script lang="ts">
  /**
   * Self-post composer: type a comment and send it to the live chat as the
   * configured account. Toggled open from below the comment list (App.svelte).
   *
   * - Twitch: posts via an authenticated one-shot IRC connection (Rust side).
   * - YouTube: not yet supported — the backend returns an explicit error which
   *   is surfaced inline here.
   * Enter sends, Shift+Enter inserts a newline. IME composition is respected so
   * confirming a Japanese conversion with Enter never sends by accident.
   */
  import type { AppConfig } from '../ipc';
  import { sendChatMessage } from '../ipc';

  type SendPlatform = 'twitch' | 'youtube';

  interface Props {
    config: AppConfig | null;
  }

  let { config }: Props = $props();

  let platform: SendPlatform = $state('twitch');
  let channel: string = $state('');
  let text: string = $state('');
  let sending: boolean = $state(false);
  let errorMsg: string = $state('');
  let okMsg: string = $state('');

  // Enabled channels from config — the source of channel candidates.
  const enabledChannels = $derived(
    (config?.channels ?? []).filter((c) => c.enabled !== false),
  );

  // Candidate channel identifiers for the currently selected platform.
  const channelCandidates = $derived(
    enabledChannels.filter((c) => c.platform === platform).map((c) => c.identifier),
  );

  // Pick the initial platform from the first enabled channel (once).
  let initialized = false;
  $effect(() => {
    if (!initialized && enabledChannels.length > 0) {
      platform = enabledChannels[0].platform as SendPlatform;
      initialized = true;
    }
  });

  // Keep `channel` pointed at a valid candidate. When candidates exist and the
  // current value is not among them (platform switch, first load), snap to the
  // first candidate. When there are no candidates the field is a free text input
  // and we leave whatever the user typed untouched.
  $effect(() => {
    if (channelCandidates.length > 0 && !channelCandidates.includes(channel)) {
      channel = channelCandidates[0];
    }
  });

  async function send() {
    errorMsg = '';
    okMsg = '';
    const body = text.trim();
    if (body === '') return;
    const target = channel.trim();
    if (target === '') {
      errorMsg = '送信先チャンネルを指定してください';
      return;
    }
    sending = true;
    try {
      await sendChatMessage(platform, target, body);
      text = '';
      okMsg = '送信しました';
    } catch (e) {
      errorMsg = e instanceof Error ? e.message : String(e);
    } finally {
      sending = false;
    }
  }

  function onKeydown(e: KeyboardEvent) {
    // isComposing guards against sending mid-IME-conversion (Japanese input).
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      void send();
    }
  }

  function onInput() {
    if (errorMsg) errorMsg = '';
    if (okMsg) okMsg = '';
  }
</script>

<div class="composer">
  <div class="composer-row">
    <select
      class="composer-platform"
      bind:value={platform}
      aria-label="送信先プラットフォーム"
      disabled={sending}
    >
      <option value="twitch">Twitch</option>
      <option value="youtube">YouTube</option>
    </select>

    {#if channelCandidates.length > 0}
      <select
        class="composer-channel"
        bind:value={channel}
        aria-label="送信先チャンネル"
        disabled={sending}
      >
        {#each channelCandidates as cand}
          <option value={cand}>{cand}</option>
        {/each}
      </select>
    {:else}
      <input
        class="composer-channel"
        type="text"
        bind:value={channel}
        placeholder="チャンネル名"
        aria-label="送信先チャンネル"
        disabled={sending}
      />
    {/if}
  </div>

  <div class="composer-input-row">
    <textarea
      class="composer-text"
      bind:value={text}
      oninput={onInput}
      onkeydown={onKeydown}
      placeholder="コメントを入力（Enterで送信 / Shift+Enterで改行）"
      aria-label="投稿するコメント"
      rows="2"
      disabled={sending}
    ></textarea>
    <button
      class="composer-send"
      onclick={send}
      disabled={sending || text.trim() === ''}
      title="チャットへ送信"
    >
      {sending ? '送信中…' : '送信'}
    </button>
  </div>

  {#if errorMsg}
    <div class="composer-msg composer-msg--error" role="alert">{errorMsg}</div>
  {:else if okMsg}
    <div class="composer-msg composer-msg--ok" role="status" aria-live="polite">{okMsg}</div>
  {/if}
</div>

<style>
  .composer {
    display: flex;
    flex-direction: column;
    gap: 5px;
    padding: 6px 8px;
    background: #181818;
    border-top: 1px solid rgba(255, 255, 255, 0.08);
    flex-shrink: 0;
  }

  .composer-row {
    display: flex;
    gap: 6px;
  }

  .composer-input-row {
    display: flex;
    gap: 6px;
    align-items: stretch;
  }

  .composer-platform,
  .composer-channel {
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 4px;
    color: #e0e0e0;
    padding: 3px 8px;
    font-size: 12px;
    min-width: 0;
  }

  .composer-channel {
    flex: 1;
  }

  .composer-text {
    flex: 1;
    min-width: 0;
    resize: vertical;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 4px;
    color: #e0e0e0;
    padding: 5px 8px;
    font-size: 13px;
    font-family: inherit;
    line-height: 1.4;
  }

  .composer-text::placeholder {
    color: #555;
  }

  .composer-text:focus,
  .composer-platform:focus,
  .composer-channel:focus {
    outline: none;
    border-color: rgba(88, 166, 255, 0.55);
  }

  .composer-send {
    flex-shrink: 0;
    align-self: stretch;
    min-width: 64px;
    background: rgba(88, 166, 255, 0.22);
    border: 1px solid rgba(88, 166, 255, 0.55);
    border-radius: 4px;
    color: #cfe4ff;
    font-size: 13px;
    font-weight: 700;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
  }

  .composer-send:hover:not(:disabled) {
    background: rgba(88, 166, 255, 0.34);
    color: #fff;
  }

  .composer-send:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .composer-msg {
    font-size: 12px;
    padding: 1px 2px;
  }

  .composer-msg--error {
    color: #fca5a5;
  }

  .composer-msg--ok {
    color: #86efac;
  }

  /* Light theme */
  :global(.app[data-theme='light']) .composer {
    background: #eef2f6;
    border-top-color: rgba(15, 23, 42, 0.1);
  }

  :global(.app[data-theme='light']) .composer-platform,
  :global(.app[data-theme='light']) .composer-channel,
  :global(.app[data-theme='light']) .composer-text {
    background: #ffffff;
    border-color: rgba(15, 23, 42, 0.16);
    color: #20242a;
  }

  :global(.app[data-theme='light']) .composer-text::placeholder {
    color: #7b8794;
  }

  :global(.app[data-theme='light']) .composer-msg--error {
    color: #b91c1c;
  }

  :global(.app[data-theme='light']) .composer-msg--ok {
    color: #15803d;
  }
</style>
