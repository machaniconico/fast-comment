export const meta = {
  name: 'fullralph-fastcomment-wave4-tests',
  description: 'fast-comment Wave4: 脆弱な純粋ロジック(YT parser/Twitch tags/moderation)に#[cfg(test)]テスト整備',
  phases: [{ title: 'WriteTests', detail: 'codexが各ファイルに同一モジュールテスト追加' }, { title: 'Review', detail: 'Claudeがテストの妥当性と静的整合を検証' }],
}

const CWD = '/mnt/d/workspace/fast-comment'
const HOME = '/home/macha/.codex-dualralph'

const stories = [
  {
    id: 'T1', file: 'src-tauri/src/sources/youtube/parser.rs',
    focus: 'YouTube寛容パーサ(SPEC最重要)。次を #[cfg(test)] mod tests で網羅: ' +
      '(a) dig()ヘルパのパス探索(存在/途中欠落でNone劣化); ' +
      '(b) runs[]→Fragment変換(text と emoji→Emote、url空のemojiはtextに落ちる); ' +
      '(c) addChatItemAction の liveChatTextMessageRenderer(通常)/liveChatPaidMessageRenderer(SuperChat,Amount抽出)/liveChatMembershipItemRenderer(メンバー) のkind判定; ' +
      '(d) authorBadges→Roles(member/moderator/owner)マッピング; ' +
      '(e) split_currency_value: "¥1,000"→value=1000, "1.000,50"(欧州)→正しい値, raw_text温存; ' +
      '(f) 未知/壊れたactionでpanicせずNone/スキップ。' +
      'serde_json::json!マクロで代表的なInnerTubeペイロードを構築してテストする。',
  },
  {
    id: 'T2', file: 'src-tauri/src/sources/twitch.rs',
    focus: 'Twitch IRCv3パース。次を #[cfg(test)] mod tests で網羅: ' +
      '(a) tagパース(display-name/color/badges/emotes/bits/id/tmi-sent-ts抽出); ' +
      '(b) badges→Roles(broadcaster/moderator/subscriber/vip、founder→subscriber寄せ); ' +
      '(c) emotes分割: emotesタグ(id:start-end,...)から本文をFragment分割、特に**UTF-16コードユニット境界**でサロゲートペア(😀等)を含む本文でもemote境界がずれないことを検証; ' +
      '(d) bitsタグあり→MessageKind::Bits + Amount; ' +
      '(e) PINGに対しPONG応答を返す(handle_lineの戻り値reply); ' +
      '(f) 通常PRIVMSGでemitted_privmsg=true、CAP ACK/welcome numeric(001等)でfalse。' +
      '実際のTwitch PRIVMSG生文字列を入力にする。',
  },
  {
    id: 'T3', file: 'src-tauri/src/moderation.rs',
    focus: 'モデレーション。次を #[cfg(test)] mod tests で網羅: ' +
      '(a) NGワード(正規表現含む)マッチ→非表示/グレー判定; ' +
      '(b) NGユーザー(正規表現)マッチ; ' +
      '(c) ハイライトルール(ユーザー/キーワード)→flag付与; ' +
      '(d) ローカル非表示(hidden id)判定; ' +
      '(e) 不正な正規表現を与えてもpanicせず安全に劣化(コンパイル/実行時)。' +
      'moderation.rsの実際の公開関数/構造体名を読んで使うこと。',
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
  // ---- write tests (codex inline) ----
  async (s) => {
    const prompt =
      `${s.file} に Rust の #[cfg(test)] mod tests を追加せよ(同一ファイル内なのでprivate関数にもアクセス可)。` +
      `対象: ${s.focus} ` +
      `制約: (1)まず ${s.file} を読み実在する関数/型シグネチャに正確に合わせる(存在しない関数を呼ばない)。` +
      `(2)テスト以外の既存コード(non-test)は一切変更しない。可視性変更が必要なら最小限に留め、原則テストモジュール内で完結させる。` +
      `(3)${s.file} 以外のファイルは変更しない。(4)各テストは決定的(ネットワーク/時刻/乱数依存を避ける)。` +
      `(5)WSLにcargo無しで実行検証できないため、コンパイルが通る正確なRustを書くこと(use宣言・型・借用を厳密に)。` +
      `日本語コメントで各テストの意図を簡潔に記せ。`
    return await agent(
      `あなたはcodex実行係です。担当ファイル=${s.file}。CWD=${CWD}。bashで次を実行:\n` +
      `CODEX_HOME=${HOME} codex exec --cd ${CWD} --sandbox workspace-write ${JSON.stringify(prompt)}\n` +
      `(数分かかる。完了まで待て。codexがファイルにテストを追加する)\n` +
      `実行後: git -C ${CWD} add -A -N\n` +
      `codexのexitCodeと、追加したテスト関数の概数・要点を報告せよ。codexが失敗(非0)なら ok:false。`,
      { label: `tests:${s.id}`, phase: 'WriteTests', schema: IMPL_SCHEMA }
    )
  },
  // ---- review (Claude code-reviewer) ----
  async (impl, s) => {
    if (!impl || impl.ok === false) return { file: s.file, passes: false, reason: 'codexテスト追加失敗' }
    return await agent(
      `fast-comment(${CWD})の ${s.file} に追加された #[cfg(test)] テストを検証せよ。` +
      `git -C ${CWD} diff -- ${s.file} で差分(=今回追加分のみ)を確認し、${s.file} 本体も読む。\n` +
      `判定基準: (1)テストが ${s.file} の実在関数/型を正しいシグネチャで呼んでいるか(コンパイル可能か静的確認); ` +
      `(2)テスト以外の既存コードを壊していないか(non-test改変が無い/最小); ` +
      `(3)テストが意味のある振る舞い(${s.focus})を実際に検証しているか(assert内容が妥当、トートロジーでない); ` +
      `(4)use宣言・型・借用が正しいか; (5)${s.file}以外を変更していないか。\n` +
      `WSLでcargo実行不可のため静的レビュー。重大なコンパイル不能/既存コード破壊/無意味テストがあれば passes:false。軽微なnitのみなら passes:true。` +
      `file="${s.file}", passes, reason(具体的に) を返せ。`,
      { label: `review:${s.id}`, phase: 'Review', schema: VERDICT_SCHEMA, agentType: 'oh-my-claudecode:code-reviewer', model: 'opus' }
    )
  }
)

return results.filter(Boolean)
