export const meta = {
  name: 'fullralph-fastcomment-wave2',
  description: 'fast-comment Wave2: Wave1でFAILした5ストーリーをfix再投入→review',
  phases: [{ title: 'Fix', detail: 'codex fix-mode 2 + claude 3 でlastFeedback反映' }, { title: 'Review', detail: 'codex criticがstory別スコープdiffで再レビュー' }],
}

const FR = '/home/macha/.claude/skills/fullralph/scripts'
const CWD = '/mnt/d/workspace/fast-comment'

const stories = [
  { id: 'US-003', engine: 'codex', files: 'src-tauri/src/tts/mod.rs src-tauri/src/tts/voicevox.rs src-tauri/src/tts/bouyomi.rs' },
  { id: 'US-004', engine: 'codex', files: 'src-tauri/src/lib.rs src-tauri/src/bus.rs src-tauri/tauri.conf.json .claude/SPEC.md SETUP.md' },
  { id: 'US-006', engine: 'claude', files: 'src/lib/stores.svelte.ts', scope: 'src/lib/stores.svelte.ts のみ編集（ipc.ts/App.svelteは既にPASS済みなので触らない）' },
  { id: 'US-007', engine: 'claude', files: 'src/lib/components/Settings.svelte src/lib/ipc.ts src/lib/types.ts src-tauri/src/config.rs', scope: 'Settings.svelte に加え、template を config 永続化するため src/lib/ipc.ts(Config型), src/lib/types.ts, src-tauri/src/config.rs(obsにtemplate:String既定"default"追加・serde camelCase) を編集してよい。stores.svelte.ts と lib.rs は触らない。Rust config構造体に追加する際はserde(default)で後方互換を保つ' },
  { id: 'US-008', engine: 'claude', files: 'src/lib/components/CommentList.svelte src/lib/components/CommentItem.svelte', scope: 'CommentList.svelte/CommentItem.svelte のみ' },
]

const IMPL_SCHEMA = {
  type: 'object',
  required: ['story', 'implemented', 'summary'],
  properties: {
    story: { type: 'string' },
    implemented: { type: 'boolean' },
    exitCode: { type: 'number' },
    summary: { type: 'string' },
    filesChanged: { type: 'array', items: { type: 'string' } },
  },
}
const VERDICT_SCHEMA = {
  type: 'object',
  required: ['story', 'passes', 'reason'],
  properties: {
    story: { type: 'string' },
    passes: { type: 'boolean' },
    reason: { type: 'string' },
  },
}

const results = await pipeline(
  stories,
  // ---- fixStage ----
  async (s) => {
    if (s.engine === 'codex') {
      return await agent(
        `あなたは fullralph の codex 修正ワーカーです。担当 story=${s.id}(Wave1でレビューFAIL、再修正)。CWD=${CWD}。\n` +
        `以下を bash で順に厳密に実行せよ:\n` +
        `1) node ${FR}/dispatcher.mjs claim-story --story=${s.id} --worker=cx-${s.id}-w2 --cwd=${CWD}\n` +
        `   "ok" が false なら implemented:false, exitCode:-1, summary:"claim失敗" を返して終了。\n` +
        `2) node ${FR}/codex-worker.mjs --story=${s.id} --mode=fix --cwd=${CWD} --sandbox=workspace-write\n` +
        `   (fixモードはPRDのlastFeedback=前回レビュー指摘を踏まえてcodexがファイルを再修正する。数分。完了まで待て)\n` +
        `3) .omc/state/fullralph-results/${s.id}.json を読み exitCode 確認:\n` +
        `   - 0 → node ${FR}/dispatcher.mjs mark-implemented --story=${s.id} --result=.omc/state/fullralph-results/${s.id}.json --cwd=${CWD}\n` +
        `   - 非0 → node ${FR}/dispatcher.mjs mark-failed --story=${s.id} --feedback="codex fix exit非0" --cwd=${CWD}\n` +
        `4) 必ず: node ${FR}/dispatcher.mjs release-story --story=${s.id} --cwd=${CWD}\n` +
        `5) git -C ${CWD} add -A -N\n` +
        `担当ファイル(${s.files})の変更概要と codex exitCode を報告せよ。`,
        { label: `fix:${s.id}`, phase: 'Fix', schema: IMPL_SCHEMA }
      )
    }
    // claude
    return await agent(
      `あなたは fullralph の Claude 修正ワーカーです。担当 story=${s.id}(Wave1でレビューFAIL、再修正)。CWD=${CWD}。\n` +
      `まず ${CWD}/.omc/fullralph-prd.json を読み、id==${s.id} の story の description / acceptance / lastFeedback を把握せよ。lastFeedback が今回の修正指示(前回FAIL理由)である。\n` +
      `ファイルスコープ: ${s.scope}\n` +
      `手順:\n` +
      `1) bash: node ${FR}/dispatcher.mjs claim-story --story=${s.id} --worker=cl-${s.id}-w2 --cwd=${CWD}\n` +
      `   "ok":false なら implemented:false, summary:"claim失敗" を返す。\n` +
      `2) lastFeedback の指摘を解消するよう、上記スコープ内のファイルのみを編集して修正せよ。acceptance を全て満たすこと。既にPASSしている他基準を壊さないこと(回帰させない)。Svelte5 runes正しく、TS/Rustとも静的に正しいコードを書く(WSLにビルドツール無し)。\n` +
      `3) bash: node -e "const fs=require('fs');fs.mkdirSync('${CWD}/.omc/state/fullralph-results',{recursive:true});fs.writeFileSync('${CWD}/.omc/state/fullralph-results/${s.id}.json',JSON.stringify({ok:true,exitCode:0,storyId:'${s.id}',engine:'claude',summary:'fixed by claude'}))"\n` +
      `4) bash: node ${FR}/dispatcher.mjs mark-implemented --story=${s.id} --result=.omc/state/fullralph-results/${s.id}.json --cwd=${CWD}\n` +
      `5) 必ず: node ${FR}/dispatcher.mjs release-story --story=${s.id} --cwd=${CWD}\n` +
      `6) bash: git -C ${CWD} add -A -N\n` +
      `変更ファイルと修正要点(lastFeedbackをどう解消したか)を報告せよ。`,
      { label: `fix:${s.id}`, phase: 'Fix', schema: IMPL_SCHEMA, agentType: 'oh-my-claudecode:executor', model: 'opus' }
    )
  },
  // ---- reviewStage ----
  async (impl, s) => {
    if (!impl || impl.implemented === false) {
      return { story: s.id, passes: false, reason: 'fix未完(claim失敗 or 修正失敗)のためレビュー省略' }
    }
    return await agent(
      `あなたは fullralph のレビュー実行係です。担当 story=${s.id}。CWD=${CWD}。bash で順に実行:\n` +
      `1) node ${FR}/reviewer-router.mjs --story=${s.id} --mode=codex --cwd=${CWD} --diff-cmd="git -C ${CWD} diff -- ${s.files}"\n` +
      `2) node ${FR}/codex-worker.mjs --story=${s.id} --mode=review --cwd=${CWD}\n` +
      `3) cat ${CWD}/.omc/state/fullralph-review-results/${s.id}.json を読み FULLRALPH-VERDICT(PASS/FAIL)と理由を抽出。\n` +
      `story="${s.id}", passes=(PASS?true:false), reason=理由(FAILなら具体的に、空diff/ビルド未検証が理由ならその旨明記) で返せ。`,
      { label: `review:${s.id}`, phase: 'Review', schema: VERDICT_SCHEMA }
    )
  }
)

return results.filter(Boolean)
