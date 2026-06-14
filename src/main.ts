import { mount } from 'svelte';

const target = document.getElementById('app')!;

// 弾幕オーバーレイ用ウィンドウ(?window=danmaku)では DanmakuOverlay だけを mount する。
// 動的 import にすることで未使用側コンポーネントの CSS を読み込まない
// (= App の :global(body){background:#121212} が弾幕ウィンドウへ混入して透明化を
//  妨げるのを防ぐ。透過オーバーレイは body 背景が透明であることが必須)。
const isDanmaku = new URLSearchParams(window.location.search).get('window') === 'danmaku';

const app = isDanmaku
  ? import('./lib/components/DanmakuOverlay.svelte').then((m) => mount(m.default, { target }))
  : import('./App.svelte').then((m) => mount(m.default, { target }));

export default app;
