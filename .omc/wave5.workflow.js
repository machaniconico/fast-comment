export const meta = {
  name: 'fullralph-fastcomment-wave5-tests',
  description: 'fast-comment Wave5: config serde往復 / OBSテンプレ名トラバーサル防止 / YT InnerTubeマーカー抽出 のテスト整備',
  phases: [{ title: 'WriteTests', detail: 'codexが各ファイルに同一モジュールテスト追加' }, { title: 'Review', detail: 'Claudeがテスト妥当性と静的整合を検証' }],
}

const CWD = '/mnt/d/workspace/fast-comment'
const HOME = '/home/macha/.codex-dualralph'

const stories = [
  {
    id: 'C1', file: 'src-tauri/src/config.rs',
    focus: '設定永続化(SPEC §10)。次を #[cfg(test)] mod tests で網羅: ' +
      '(a) AppConfig の serde 往復(to_string→from_str)でcamelCaseキー(obs.template/ui.maxBuffer/tts.options各フィールド/youtubeOverrides)が保持される; ' +
      '(b) 各 default 関数の値(default_obs_template="default", default_max_read_len, obs.port既定11180等、実値はコード参照)が Default::default() に正しく反映; ' +
      '(c) 古い/部分的なconfig.json(obs.template欠落, options一部欠落, youtubeOverrides欠落)を from_str しても #[serde(default)] でpanicせず既定補完される(後方互換); ' +
      '(d) youtubeOverrides の paths(キー名→文字列)が空でも壊れず、指定時に保持される。' +
      'fsに触れる atomic save はテストが難しければ skip し、serde往復とdefault補完に集中する。pathパラメータ化された保存helperがあればtempfileでテストしてよい。',
  },
  {
    id: 'C2', file: 'src-tauri/src/bus.rs',
    focus: 'OBS静的配信のセキュリティ/ルーティング純粋ヘルパ。次を #[cfg(test)] mod tests で網羅: ' +
      '(a) is_valid_template_name: 正常名(default, my-template, abc_123)を許可し、パストラバーサル(.., ../foo, foo/bar, 先頭/, 絶対パス, 空文字, 長すぎ64超, 非ASCII/記号)を拒否する; ' +
      '(b) inject_template_base(もしくは相当の<base>注入関数): <head>直後に <base href="/<name>/"> を正しく挿入し、headが無い/変形HTMLでも安全に劣化する。' +
      '非同期ハンドラ(axum)は対象外。純粋関数のみをテストする。関数が private でも同一ファイル子modからアクセス可。実在する関数名/シグネチャをbus.rsを読んで確認すること。',
  },
  {
    id: 'C3', file: 'src-tauri/src/sources/youtube/innertube.rs',
    focus: 'YouTube InnerTube抽出(ホットパッチ核, SPEC §4.2)。次を #[cfg(test)] mod tests で網羅: ' +
      '(a) extract_json_string_field(もしくは相当のマーカー抽出関数): 与えたマーカー直後の文字列値を正しく取り出す/マーカー不在でNone; ' +
      '(b) API_KEY/clientVersion 抽出が既定マーカー("INNERTUBE_API_KEY":" 等)で動作し、overridesで与えた代替マーカーが優先される; ' +
      '(c) 初期continuationマーカーの優先順序: 限定マーカー(invalidationContinuationData/timedContinuationData/reloadContinuation系)が汎用"continuation":"より先に試され、汎用は最後のフォールバックになる(Wave1修正の回帰防止); ' +
      '(d) overrides が空/未指定なら既定挙動と完全一致(空paths=現行挙動)。' +
      'innertube.rsを読み実在関数/シグネチャに合わせる。HTTPには触れず文字列抽出ロジックのみテスト。代表的なHTML断片を文字列で与える。',
  },
]

const IMPL_SCHEMA = {
  type: 'object', required: ['file', 'ok', 'summary'],
  properties: { file: { type: 'string' }, ok: { type: 'boolean' }, exitCode: { type: 'number' }, summary: { type: 'string' } },
}
const VERDICT_SCHEMA = {
  type: 'object', required: ['file', 'passes', 'reason'],
  properties: { file: { type: 'string' }, passes: { type: 'boolean' }, reason: { type: 'string' } },
}

const results = await pipeline(
  stories,
  async (s) => {
    const prompt =
      `${s.file} に Rust の #[cfg(test)] mod tests を追加せよ(同一ファイル内なのでprivate関数にもアクセス可)。` +
      `対象: ${s.focus} ` +
      `制約: (1)まず ${s.file} を読み実在する関数/型シグネチャに正確に合わせる(存在しない関数を呼ばない。無ければ最も近い実在関数をテストする)。` +
      `(2)テスト以外の既存コード(non-test)は一切変更しない。可視性変更が必要なら最小限。(3)${s.file} 以外を変更しない。` +
      `(4)決定的テスト(ネットワーク/時刻/乱数非依存。fsはtempfileのみ)。(5)WSLでcargo実行不可のためコンパイルが通る正確なRustを書く(use/型/借用厳密)。` +
      `日本語コメントで各テスト意図を簡潔に。`
    return await agent(
      `あなたはcodex実行係です。担当ファイル=${s.file}。CWD=${CWD}。bashで次を実行:\n` +
      `CODEX_HOME=${HOME} codex exec --cd ${CWD} --sandbox workspace-write ${JSON.stringify(prompt)}\n` +
      `(数分かかる。完了まで待て)\n実行後: git -C ${CWD} add -A -N\n` +
      `codexのexitCodeと追加テストの概数・要点を報告。codex失敗(非0)なら ok:false。`,
      { label: `tests:${s.id}`, phase: 'WriteTests', schema: IMPL_SCHEMA }
    )
  },
  async (impl, s) => {
    if (!impl || impl.ok === false) return { file: s.file, passes: false, reason: 'codexテスト追加失敗' }
    return await agent(
      `fast-comment(${CWD})の ${s.file} に追加された #[cfg(test)] テストを検証せよ。` +
      `git -C ${CWD} diff -- ${s.file} で差分(今回追加分のみ)を確認し ${s.file} 本体も読む。\n` +
      `判定基準: (1)実在関数/型を正しいシグネチャで呼びコンパイル可能か; (2)non-test既存コードを壊していないか(改変無し/最小); ` +
      `(3)意味のある振る舞い(${s.focus})を実際に検証(assert妥当・非トートロジー)しているか; (4)use/型/借用が正しいか; (5)${s.file}以外を変更していないか。\n` +
      `WSLでcargo不可のため静的レビュー。重大なコンパイル不能/既存破壊/無意味テストがあれば passes:false、軽微nitのみなら passes:true。` +
      `file="${s.file}", passes, reason を返せ。`,
      { label: `review:${s.id}`, phase: 'Review', schema: VERDICT_SCHEMA, agentType: 'oh-my-claudecode:code-reviewer', model: 'opus' }
    )
  }
)

return results.filter(Boolean)
