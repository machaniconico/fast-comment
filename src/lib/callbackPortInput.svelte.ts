import { parseCallbackPort } from './xAutopost';

export function createCallbackPortInput(
  readPort: () => number,
  readConfigRevision: () => number,
  onChange: (port: number | null) => void,
) {
  let text = $state('');
  const port = $derived(parseCallbackPort(text));
  let syncedPort: number | undefined;
  let syncedRevision: number | undefined;

  $effect.pre(() => {
    const nextPort = readPort();
    const nextRevision = readConfigRevision();
    // 入力を親が反映しただけなら、空白や先頭の 0 を含めて編集中の文字列を保つ。
    // インポートで設定全体を置き換えた場合は、同じポートでも入力を更新する。
    if (nextPort !== syncedPort || nextRevision !== syncedRevision) {
      text = String(nextPort);
    }
    syncedPort = nextPort;
    syncedRevision = nextRevision;
  });

  return {
    get text() { return text; },
    get port() { return port; },
    onInput(event: Event & { currentTarget: HTMLInputElement }) {
      text = event.currentTarget.value;
      const nextPort = parseCallbackPort(text);
      if (nextPort !== null) syncedPort = nextPort;
      onChange(nextPort);
    },
  };
}
